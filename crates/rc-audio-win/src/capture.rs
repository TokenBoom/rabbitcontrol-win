//! WASAPI 环回采集（M5a.5）：默认渲染端点 LOOPBACK，独立泵线程。
//!
//! - 格式：设备 mix 格式（float32 交错）就地转 16-bit 立体声，按 20ms 块
//!   打包为 [`PcmFrame`] 推入有界队列（drop-oldest，容量 20 帧）；
//! - 泵线程：10ms 轮询 `GetNextPacketSize`（LOOPBACK + EVENTCALLBACK 有
//!   静音不触发的已知问题，故不用事件回调）；线程内自防御 `CoInitializeEx(MTA)`，
//!   WASAPI 对象全部在线程内创建（COM 接口不跨线程传递）；
//! - [`LoopbackCapture::stop`] 置停止标志，泵线程退出前 Stop/Reset 客户端。

#![allow(unsafe_code)] // WASAPI FFI 边界

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rc_audio::{AudioCapture, PcmFrame};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
    WAVEFORMATEX,
};
use windows::Win32::Media::Multimedia::WAVE_FORMAT_IEEE_FLOAT;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};

/// 泵线程轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(10);
/// 出队块大小（帧）：20ms @48k。
const CHUNK_FRAMES: usize = 960;
/// 队列容量（帧数）。
const QUEUE_CAP: usize = 20;
/// 泵线程就绪等待上限。
const READY_TIMEOUT: Duration = Duration::from_secs(5);

/// 读取 mix 格式三元组 `(采样率, 声道, 是否 float32)`。
///
/// # Safety
/// `fmt` 必须指向有效的 `WAVEFORMATEX`。
unsafe fn probe_mix_format(fmt: *const WAVEFORMATEX) -> (u32, u16, bool) {
    let f = unsafe { &*fmt };
    let float = f.wFormatTag == WAVE_FORMAT_IEEE_FLOAT as u16
        // WAVE_FORMAT_EXTENSIBLE(0xFFFE)：常量在 KernelStreaming 模块，内联使用
        || (f.wFormatTag == 0xFFFE && f.wBitsPerSample == 32);
    (f.nSamplesPerSec, f.nChannels, float)
}

/// 环回采集运行时（泵线程持全部 WASAPI 对象，结构体本身无 COM 引用）。
pub struct LoopbackCapture {
    queue: Arc<Mutex<VecDeque<PcmFrame>>>,
    stop_flag: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    sample_rate: u32,
    channels: u8,
}

impl LoopbackCapture {
    /// 在默认渲染端点上创建 LOOPBACK 采集并启动泵线程。
    ///
    /// 阻塞等待泵线程就绪（≤5s）；无音频端点的环境优雅失败。
    pub fn new() -> std::result::Result<Self, String> {
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let stop_flag = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(u32, u8), String>>();

        let thread = {
            let queue = Arc::clone(&queue);
            let stop_flag = Arc::clone(&stop_flag);
            std::thread::Builder::new()
                .name("rc-audio-loopback".into())
                .spawn(move || {
                    // Start 成功后立即发就绪；退出码经 before 通道尾部（无人接收，忽略）
                    pump_and_run(&queue, &stop_flag, &ready_tx);
                })
                .map_err(|e| format!("泵线程启动失败：{e}"))?
        };

        let (rate, channels) = match ready_rx.recv_timeout(READY_TIMEOUT) {
            Ok(Ok(pair)) => pair,
            Ok(Err(err)) => {
                stop_flag.store(true, Ordering::SeqCst);
                let _ = thread.join();
                return Err(err);
            }
            Err(_) => {
                stop_flag.store(true, Ordering::SeqCst);
                let _ = thread.join();
                return Err("泵线程就绪超时".into());
            }
        };

        Ok(Self {
            queue,
            stop_flag,
            thread: Some(thread),
            sample_rate: rate,
            channels,
        })
    }
}

/// 泵线程主体：COM → 激活 → Start → 轮询循环；停止时 Stop/Reset。
/// 向调用方（经 ready 通道，见 [`LoopbackCapture::new`]）上报 `(采样率, 声道)` 或错误。
fn pump_and_run(
    queue: &Mutex<VecDeque<PcmFrame>>,
    stop: &AtomicBool,
    ready: &mpsc::Sender<Result<(u32, u8), String>>,
) {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        let owns = hr.is_ok();
        let outcome = pump_inner(queue, stop, ready);
        if owns {
            CoUninitialize();
        }
        let _ = outcome;
    }
}

unsafe fn pump_inner(
    queue: &Mutex<VecDeque<PcmFrame>>,
    stop: &AtomicBool,
    ready: &mpsc::Sender<Result<(u32, u8), String>>,
) -> Result<(u32, u8), String> {
    let enumerator: IMMDeviceEnumerator =
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| e.to_string())?;
    let device = enumerator
        .GetDefaultAudioEndpoint(eRender, eConsole)
        .map_err(|e| format!("默认渲染端点缺失：{e}"))?;
    let client: IAudioClient = device
        .Activate(CLSCTX_ALL, None)
        .map_err(|e| format!("激活 AudioClient 失败：{e}"))?;
    let mix = client
        .GetMixFormat()
        .map_err(|e| format!("GetMixFormat 失败：{e}"))?;
    let (rate, channels, is_float) = probe_mix_format(mix);
    let channels = channels.clamp(1, 2);

    client
        .Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK,
            1_000_000,
            0,
            mix,
            None,
        )
        .map_err(|e| format!("LOOPBACK 初始化失败：{e}"))?;
    let capture: IAudioCaptureClient = client
        .GetService()
        .map_err(|e| format!("获取 CaptureClient 失败：{e}"))?;
    client.Start().map_err(|e| format!("Start 失败：{e}"))?;

    // Start 成功即发就绪（pump 循环随后持续运行到 stop）
    let _ = ready.send(Ok((rate, channels as u8)));

    pump_loop(&capture, queue, stop, rate, channels, is_float);
    let _ = client.Stop();
    let _ = client.Reset();
    Ok((rate, channels as u8))
}

/// 泵主循环：轮询数据包 → float32 → i16 立体声 → 20ms 块入队。
fn pump_loop(
    capture: &IAudioCaptureClient,
    queue: &Mutex<VecDeque<PcmFrame>>,
    stop: &AtomicBool,
    rate: u32,
    channels: u16,
    _is_float: bool,
) {
    let mut staging: Vec<i16> = Vec::with_capacity(CHUNK_FRAMES * 2);
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let mut packet = match unsafe { capture.GetNextPacketSize() } {
            Ok(p) => p,
            Err(_) => return,
        };
        if packet == 0 {
            std::thread::sleep(POLL_INTERVAL);
            continue;
        }
        while packet > 0 {
            let mut data = std::ptr::null_mut::<u8>();
            let mut frames = 0u32;
            let mut flags = 0u32;
            if unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }.is_err()
            {
                return;
            }
            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
            let ch = channels as usize;
            if silent || data.is_null() {
                staging.resize(staging.len() + frames as usize * 2, 0);
            } else {
                let total = frames as usize * ch;
                let f32_slice = unsafe { std::slice::from_raw_parts(data.cast::<f32>(), total) };
                for i in 0..frames as usize {
                    let base = i * ch;
                    let l = f32_slice.get(base).copied().unwrap_or(0.0);
                    let r = if ch >= 2 {
                        f32_slice.get(base + 1).copied().unwrap_or(l)
                    } else {
                        l
                    };
                    staging.push((l.clamp(-1.0, 1.0) * 32767.0) as i16);
                    staging.push((r.clamp(-1.0, 1.0) * 32767.0) as i16);
                }
            }
            let _ = unsafe { capture.ReleaseBuffer(frames) };
            packet = match unsafe { capture.GetNextPacketSize() } {
                Ok(p) => p,
                Err(_) => return,
            };
        }
        // 20ms 块出队（立体声 2 采样/帧）
        let chunk_samples = CHUNK_FRAMES * 2;
        while staging.len() >= chunk_samples {
            let chunk: Vec<i16> = staging.drain(..chunk_samples).collect();
            let mut guard = queue.lock().expect("audio queue 锁");
            if guard.len() >= QUEUE_CAP {
                guard.pop_front();
            }
            guard.push_back(PcmFrame {
                sample_rate: rate,
                channels: 2,
                timestamp_us: rc_win32core::host_time_us() as u32,
                samples: chunk,
            });
        }
        staging.clear();
    }
}

impl AudioCapture for LoopbackCapture {
    fn try_recv_frame(&mut self) -> Option<PcmFrame> {
        self.queue.lock().expect("audio queue 锁").pop_front()
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u8 {
        self.channels
    }

    fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creation_is_graceful_without_device() {
        // 无音频端点的环境（CI/远程会话）应优雅失败而非 panic
        let _ = LoopbackCapture::new();
    }
}
