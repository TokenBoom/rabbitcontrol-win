//! RabbitControl Windows 音频后端（WASAPI）。
//!
//! # 采集（被控侧）
//!
//! 默认渲染端点的环回采集：`IMMDeviceEnumerator` 取 eRender/eConsole
//! 默认设备，`AUDCLNT_STREAMFLAGS_LOOPBACK` + 事件驱动共享模式
//! 初始化 `IAudioClient`，专用线程泵 `IAudioCaptureClient` 数据包；
//! 系统混音格式（float32 交错）就地转换为 core 要求的 48 kHz /
//! 16-bit PCM（必要时先经 `IAudioResampler` MFT 重采样）。
//!
//! # 播放（主控侧）
//!
//! 共享模式 push 驱动播放：`AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` 让音频
//! 引擎完成采样率/声道/位深转换，`push_frame` 顺带按 `GetCurrentPadding`
//! 补空位写入，端点缓冲即有界队列（溢出丢帧控延迟），无独立泵线程。
//!
//! COM 套间初始化统一走 [`rc_win32core::com::ComApartment`]。

#![deny(missing_docs)]
#![allow(unsafe_code)] // 本 crate 整体为 WASAPI FFI 边界

use std::sync::Arc;

use rc_audio::{AudioBackendProvider, AudioCapture, AudioPlayer, PcmFrame};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX, WAVE_FORMAT_PCM,
};
use windows::Win32::Media::Multimedia::WAVE_FORMAT_IEEE_FLOAT;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

/// WASAPI 环回采集（骨架占位；M5 被控端实装）。
pub struct WasapiLoopbackCapture {
    sample_rate: u32,
    channels: u8,
}

impl AudioCapture for WasapiLoopbackCapture {
    fn try_recv_frame(&mut self) -> Option<PcmFrame> {
        None
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u8 {
        self.channels
    }

    fn stop(&mut self) {}
}

/// WASAPI 共享模式播放器（push 驱动泵送，无独立泵线程）。
///
/// - 格式：i16 PCM + `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | SRC_DEFAULT_QUALITY`，
///   采样率/声道/位深转换交由音频引擎（Win8+；本仓基线 Win10 1903+），免自研重采样；
/// - 泵送：[`AudioPlayer::push_frame`] 顺带按 `GetCurrentPadding` 补空位写入，
///   端点缓冲（引擎默认 ~10ms×2）即有界队列，溢出丢帧控延迟（契约允许）；
/// - 静音：直接丢帧不写入（push 驱动模型下端点自然输出静音）；
/// - COM：会话线程自防御初始化（MTA），守卫与播放器同生命周期。
pub struct WasapiPlayer {
    inner: Option<PlayerInner>,
    muted: bool,
}

struct PlayerInner {
    client: IAudioClient,
    render: IAudioRenderClient,
    /// 端点缓冲总帧数。
    buffer_frames: u32,
    /// 写格式声道数（写入按此交错）。
    channels: u16,
    /// 写格式是否 float32（mix 格式回退路径）。
    is_float: bool,
    /// 写格式采样率（回退路径最近邻重采样）。
    sample_rate: u32,
    /// COM 套间守卫（同线程同生命周期）。
    _com: rc_win32core::ComApartment,
}

impl WasapiPlayer {
    /// 在默认渲染端点上创建共享模式播放器。
    pub fn new(sample_rate: u32, channels: u8) -> std::result::Result<Self, String> {
        let com = rc_win32core::ComApartment::init(rc_win32core::ApartmentModel::MultiThreaded)?;
        unsafe { Self::activate(sample_rate, channels, com) }
    }

    unsafe fn activate(
        sample_rate: u32,
        channels: u8,
        com: rc_win32core::ComApartment,
    ) -> std::result::Result<Self, String> {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| e.to_string())?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(|e| format!("默认渲染端点缺失：{e}"))?;
        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| format!("激活 AudioClient 失败：{e}"))?;

        let channels = channels.max(1);
        let write_format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: u16::from(channels),
            nSamplesPerSec: sample_rate,
            wBitsPerSample: 16,
            nBlockAlign: u16::from(channels) * 2,
            nAvgBytesPerSec: sample_rate * u32::from(channels) * 2,
            cbSize: 0,
        };
        let autoconv = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        let init = client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            autoconv,
            0,
            0,
            &write_format,
            None,
        );
        let (is_float, device_rate, device_channels) = match init {
            Ok(()) => (false, sample_rate, u16::from(channels)),
            Err(_) => {
                // 回退：设备 mix 格式（float32）+ 手动 i16→f32 与最近邻重采样
                let mix = client
                    .GetMixFormat()
                    .map_err(|e| format!("GetMixFormat 失败：{e}"))?;
                let (rate, ch, float) = probe_mix_format(mix);
                client
                    .Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 0, 0, mix, None)
                    .map_err(|e| format!("mix 格式初始化失败：{e}"))?;
                (float, rate, ch)
            }
        };

        let render: IAudioRenderClient = client
            .GetService()
            .map_err(|e| format!("获取 RenderClient 失败：{e}"))?;
        let buffer_frames = client
            .GetBufferSize()
            .map_err(|e| format!("GetBufferSize 失败：{e}"))?;
        client.Start().map_err(|e| format!("Start 失败：{e}"))?;

        Ok(Self {
            inner: Some(PlayerInner {
                client,
                render,
                buffer_frames,
                channels: device_channels,
                is_float,
                sample_rate: device_rate,
                _com: com,
            }),
            muted: false,
        })
    }
}

/// 读取 mix 格式三元组 `(采样率, 声道, 是否 float32)`。
unsafe fn probe_mix_format(fmt: *const WAVEFORMATEX) -> (u32, u16, bool) {
    let f = unsafe { &*fmt };
    let float = f.wFormatTag == WAVE_FORMAT_IEEE_FLOAT as u16
        || (f.wFormatTag == (WAVE_FORMAT_EXTENSIBLE_VAL as u16) && f.wBitsPerSample == 32);
    (f.nSamplesPerSec, f.nChannels, float)
}

/// `WAVE_FORMAT_EXTENSIBLE`（Multimedia 模块，u32 常量）。
const WAVE_FORMAT_EXTENSIBLE_VAL: u32 = 0xFFFE;

// COM 指针仅在创建线程（会话线程）使用；Send 为满足 AudioPlayer 契约的一次性移交
unsafe impl Send for WasapiPlayer {}

impl Drop for WasapiPlayer {
    fn drop(&mut self) {
        self.stop();
    }
}

impl AudioPlayer for WasapiPlayer {
    fn push_frame(&mut self, frame: &PcmFrame) -> std::result::Result<(), String> {
        let Some(inner) = self.inner.as_mut() else {
            return Err("播放器已停止".into());
        };
        if self.muted {
            return Ok(()); // 静音：直接丢帧
        }
        unsafe {
            let padding = inner
                .client
                .GetCurrentPadding()
                .map_err(|e| format!("GetCurrentPadding 失败：{e}"))?;
            let free = inner.buffer_frames.saturating_sub(padding);
            if free == 0 {
                return Ok(()); // 端点缓冲满：丢帧控延迟（契约允许）
            }
            let src_ch = u16::from(frame.channels.max(1));
            let src_rate = frame.sample_rate.max(1);
            let want = (frame.samples.len() / usize::from(src_ch)) as u32;
            let to_write = free.min(want);
            if to_write == 0 {
                return Ok(());
            }
            let data = inner
                .render
                .GetBuffer(to_write)
                .map_err(|e| format!("GetBuffer 失败：{e}"))?;

            // 回退路径（mix 格式采样率 ≠ 帧率）最近邻重采样（引擎转换不可用时的降级）
            let step = f64::from(src_rate) / f64::from(inner.sample_rate.max(1));
            for i in 0..to_write as usize {
                let idx = ((i as f64 * step) as usize).min(want as usize - 1);
                let base = idx * usize::from(src_ch);
                let l = frame.samples.get(base).copied().unwrap_or(0);
                let r = if src_ch >= 2 {
                    frame.samples.get(base + 1).copied().unwrap_or(l)
                } else {
                    l
                };
                write_sample(inner, data, i, l, r);
            }
            inner
                .render
                .ReleaseBuffer(to_write, 0)
                .map_err(|e| format!("ReleaseBuffer 失败：{e}"))?;
        }
        Ok(())
    }

    fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
    }

    fn is_muted(&self) -> bool {
        self.muted
    }

    fn stop(&mut self) {
        if let Some(inner) = self.inner.take() {
            unsafe {
                let _ = inner.client.Stop();
                let _ = inner.client.Reset();
            }
            drop(inner);
        }
    }
}

/// 写入第 `i` 帧的双声道样本（按写格式做位深/声道适配）。
unsafe fn write_sample(inner: &PlayerInner, data: *mut u8, i: usize, l: i16, r: i16) {
    match (inner.is_float, inner.channels) {
        (true, 1) => {
            let s = (i32::from(l) + i32::from(r)) / 2;
            let v = s as f32 / 32768.0;
            unsafe { std::ptr::copy_nonoverlapping(v.to_le_bytes().as_ptr(), data.add(i * 4), 4) };
        }
        (true, _) => {
            let vl = l as f32 / 32768.0;
            let vr = r as f32 / 32768.0;
            let mut bytes = [0u8; 8];
            bytes[..4].copy_from_slice(&vl.to_le_bytes());
            bytes[4..].copy_from_slice(&vr.to_le_bytes());
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), data.add(i * 8), 8) };
        }
        (false, 1) => {
            let s = ((i32::from(l) + i32::from(r)) / 2) as i16;
            unsafe { std::ptr::copy_nonoverlapping(s.to_le_bytes().as_ptr(), data.add(i * 2), 2) };
        }
        (false, _) => {
            let mut bytes = [0u8; 4];
            bytes[..2].copy_from_slice(&l.to_le_bytes());
            bytes[2..].copy_from_slice(&r.to_le_bytes());
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), data.add(i * 4), 4) };
        }
    }
}

struct WasapiProvider;

impl AudioBackendProvider for WasapiProvider {
    fn create_system_audio_capture(&self) -> std::result::Result<Box<dyn AudioCapture>, String> {
        // M5 被控端：IMMDeviceEnumerator → 默认渲染端点 → loopback 初始化。
        Err("wasapi loopback capture not yet implemented".into())
    }

    fn create_audio_player(
        &self,
        sample_rate: u32,
        channels: u8,
    ) -> std::result::Result<Box<dyn AudioPlayer>, String> {
        WasapiPlayer::new(sample_rate, channels).map(|p| Box::new(p) as Box<dyn AudioPlayer>)
    }
}

/// 注册 WASAPI 音频提供者（幂等）。
pub fn register() -> bool {
    rc_audio::register_audio_provider(Arc::new(WasapiProvider))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 播放器创建：有音频端点的会话应成功；无端点（CI/远程会话）优雅失败。
    #[test]
    fn player_create_is_graceful() {
        match WasapiPlayer::new(48_000, 2) {
            Ok(mut player) => {
                assert!(!player.is_muted());
                // 静音态推帧：直接丢弃不触端点
                player.set_muted(true);
                let frame = PcmFrame {
                    sample_rate: 48_000,
                    channels: 2,
                    timestamp_us: 0,
                    samples: vec![0i16; 960],
                };
                player.push_frame(&frame).expect("静音推帧不报错");
                player.stop();
                // stop 后再推帧应报错（幂等不 panic）
                assert!(player.push_frame(&frame).is_err());
            }
            Err(reason) => {
                eprintln!("无音频端点（CI/远程会话），跳过播放器测试：{reason}");
            }
        }
    }
}
