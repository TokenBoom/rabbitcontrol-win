//! `loopback` 子命令：进程内全链路回环（采集 → 编码 → 打包 → 解包 → 解码 → 呈现）。
//!
//! core 自带的 `Loopback::run_real` 只认 macOS 的后端标签，在 Windows 上永远回落到假后端，
//! 验证不到真实的硬件链路。本命令用 core 同一套管线（`HostPipeline` / `ControllerPipeline`）
//! 直接驱动 Windows 的真实后端，会话时钟、时间戳、解码执行模式都和真实会话一致：
//!
//! - `--source dxgi`：真实桌面采集（桌面要解锁、显示器要亮着，否则采集挂起不出帧）；
//! - `--source synthetic`：合成的动画帧（锁屏、熄屏时也能验证管线与编解码 / 呈现）；
//! - `--source auto`（默认）：采集没挂起就用 dxgi，否则退到合成帧；`--strict` 时禁止退回。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use rc_capture::{CaptureBackend, CapturedFrame, DisplayInfo, FrameTexture, InvalidationReason};
use rc_pipeline::{ControllerPipeline, DiffMode, HostPipeline, PipelineConfig, MAX_PAYLOAD};
use rc_types::{DisplayId, Resolution, Timestamp};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{ID3D11Resource, ID3D11Texture2D};

/// 采集源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Source {
    /// 采集没挂起就用 dxgi，否则退到合成帧。
    Auto,
    /// 真实桌面采集。
    Dxgi,
    /// 合成的动画帧。
    Synthetic,
}

/// 呈现帧数达到采集帧数的这个比例才算通过（编码慢于帧率时调度器会丢帧，属正常）。
const PASS_RATIO: f64 = 0.8;

/// 运行全链路回环。全部后端就绪、且呈现帧数达标才算通过。
pub fn run(
    duration_s: u64,
    width: u32,
    height: u32,
    fps: u32,
    source: Source,
    strict: bool,
) -> Result<(), String> {
    let _com = rc_win32core::ComApartment::init(rc_win32core::ApartmentModel::MultiThreaded)?;
    let duration = Duration::from_secs(duration_s);
    let trace_cap = (duration_s as usize + 4) * fps as usize * 2;
    // 会话时钟与真实会话同一套：主机时钟减去建管线前记下的零点，采集时间戳也在这个域里
    let origin_us = rc_win32core::host_time_us();
    let pcfg = PipelineConfig {
        resolution: Resolution::new(width, height),
        fps,
        bitrate_bps: 8_000_000,
        prefer_hardware: true,
        // 真实硬件会话里控制端管线用的就是 async_decode = prefer_hardware（流水线解码）
        async_decode: true,
        target_latency_us: 22_000,
        trace_capacity: trace_cap,
        diff: DiffMode::TrustPlatform,
        clock_origin_us: Some(origin_us),
        max_packet_payload: MAX_PAYLOAD,
        ..PipelineConfig::default()
    };
    pcfg.validate()
        .map_err(|err| format!("管线配置非法：{err}"))?;
    println!(
        "== 全链路回环（Windows 真实后端）==\n{width}x{height} @ {fps} fps · 8 Mbps · 时长 {duration_s} s · 采集源 {source:?}"
    );

    let (capture, capture_label) = select_capture(&pcfg, source, strict, origin_us)?;
    let (encoder, encoder_label) = rc_pipeline::select_encoder_labeled(&pcfg)
        .map_err(|err| format!("编码器创建失败：{err}"))?;
    let (decoder, decoder_label) = rc_pipeline::select_decoder_labeled(&pcfg)
        .map_err(|err| format!("解码器创建失败：{err}"))?;
    let (renderer, renderer_label) = rc_pipeline::select_renderer_labeled(&pcfg)
        .map_err(|err| format!("渲染器创建失败：{err}"))?;
    let paced = capture_label == "synthetic";
    println!(
        "后端：采集 {capture_label} · 编码 {encoder_label} · 解码 {decoder_label} · 呈现 {renderer_label}"
    );

    let mut host = HostPipeline::with_backends_paced(pcfg.clone(), capture, encoder, paced)
        .map_err(|err| format!("被控管线创建失败：{err}"))?;
    let mut controller = ControllerPipeline::with_backends(pcfg, decoder, renderer)
        .map_err(|err| format!("控制端管线创建失败：{err}"))?;
    let clock: Arc<dyn Fn() -> u32 + Send + Sync> = Arc::new(move || {
        u32::try_from(rc_win32core::host_time_us().saturating_sub(origin_us)).unwrap_or(u32::MAX)
    });
    host.set_clock(Arc::clone(&clock));
    controller.set_clock(Arc::clone(&clock));

    // 控制端管线放独立线程：数据报经容量 2 的通道过去，等价于一跳网络
    let (datagram_tx, datagram_rx) = mpsc::sync_channel::<Vec<Vec<u8>>>(2);
    let need_idr = Arc::new(AtomicBool::new(false));
    let controller_need_idr = Arc::clone(&need_idr);
    let controller_clock = Arc::clone(&clock);
    let controller_thread = std::thread::Builder::new()
        .name("rc-loop-controller".into())
        .spawn(move || -> std::result::Result<ControllerPipeline, String> {
            while let Ok(datagrams) = datagram_rx.recv() {
                for datagram in &datagrams {
                    controller
                        .push_datagram(datagram, controller_clock())
                        .map_err(|err| format!("控制端收包失败：{err}"))?;
                }
                if controller.take_keyframe_request() {
                    controller_need_idr.store(true, Ordering::Release);
                }
            }
            Ok(controller)
        })
        .map_err(|err| format!("启动控制端线程失败：{err}"))?;

    let started = Instant::now();
    while started.elapsed() < duration {
        if need_idr.swap(false, Ordering::AcqRel) {
            host.request_idr();
        }
        let out = host
            .tick(clock())
            .map_err(|err| format!("被控管线 tick 失败：{err}"))?;
        let busy = out.is_some();
        if let Some(datagrams) = out {
            if !datagrams.is_empty() {
                datagram_tx
                    .send(datagrams)
                    .map_err(|_| "控制端通道已关闭".to_string())?;
            }
        }
        if !busy {
            let remaining = duration.saturating_sub(started.elapsed());
            if !remaining.is_zero() {
                host.wait_event(remaining.min(Duration::from_millis(10)));
            }
        }
    }

    // 收尾：停采集，把在途的帧排空
    host.stop_capture();
    let drain_deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() <= drain_deadline {
        match host
            .tick(clock())
            .map_err(|err| format!("被控管线排空失败：{err}"))?
        {
            Some(datagrams) => {
                if !datagrams.is_empty() {
                    let _ = datagram_tx.send(datagrams);
                }
            }
            None if !host.has_pending_encodes() => break,
            None => host.wait_event(Duration::from_millis(5)),
        }
    }
    host.shutdown();
    drop(datagram_tx);
    let mut controller = controller_thread
        .join()
        .map_err(|_| "控制端线程崩溃".to_string())?
        .map_err(|err| format!("控制端管线失败：{err}"))?;
    controller.finish_in_flight();
    controller.shutdown();

    let stats = host.sched_stats();
    let displayed = controller.displayed();
    let summary = rc_pipeline::merge_traces(host.trace(), controller.trace(), trace_cap).summary();
    let seconds = duration.as_secs_f64();
    println!(
        "采集 {} 帧 · 编码 {} 帧 · 丢弃（重复 {} / 过期 {} / 预算 {} / 被取代 {}）· 关键帧请求 {}",
        stats.captured,
        stats.encoded,
        stats.dropped_repeat,
        stats.dropped_stale,
        stats.dropped_budget,
        stats.dropped_superseded,
        stats.keyframe_requests
    );
    println!(
        "呈现 {displayed} 帧（{:.1} fps）· 未完成 {} · 端到端延迟 p50 {} ms · p95 {} ms · p99 {} ms · 丢包 {} · 编解码错误 {}",
        displayed as f64 / seconds,
        summary.unfinished,
        millis(summary.e2e_p50_us),
        millis(summary.e2e_p95_us),
        millis(summary.e2e_p99_us),
        summary.lost_in_network,
        summary.decode_errors
    );
    if stats.captured == 0 {
        return Err(
            "没有采到任何帧（桌面锁屏或显示器熄屏时采集会挂起，可改用 --source synthetic）"
                .to_string(),
        );
    }
    if displayed == 0 {
        return Err(format!(
            "采了 {} 帧、编了 {} 帧，但一帧都没有呈现出来",
            stats.captured, stats.encoded
        ));
    }
    if (displayed as f64) < (stats.captured as f64) * PASS_RATIO {
        return Err(format!(
            "只呈现了 {displayed}/{} 帧（低于 {:.0}%）：编码跟不上目标帧率",
            stats.captured,
            PASS_RATIO * 100.0
        ));
    }
    println!("通过：全链路正常");
    Ok(())
}

fn millis(micros: Option<u32>) -> String {
    micros.map_or("—".to_string(), |value| {
        format!("{:.1}", f64::from(value) / 1000.0)
    })
}

/// 选采集后端。
fn select_capture(
    pcfg: &PipelineConfig,
    source: Source,
    strict: bool,
    origin_us: u64,
) -> std::result::Result<(Box<dyn CaptureBackend>, &'static str), String> {
    let synthetic = || {
        SyntheticCapture::new(
            pcfg.resolution.width,
            pcfg.resolution.height,
            pcfg.fps,
            origin_us,
        )
        .map(|capture| (Box::new(capture) as Box<dyn CaptureBackend>, "synthetic"))
    };
    if source == Source::Synthetic {
        return synthetic();
    }
    let (mut capture, label) = rc_pipeline::select_capture_labeled(pcfg)
        .map_err(|err| format!("采集后端创建失败：{err}"))?;
    if strict || source == Source::Dxgi {
        return Ok((capture, label));
    }
    match capture.poll_invalidation() {
        None => Ok((capture, label)),
        Some(_) => {
            eprintln!("[loopback] 桌面采集处于挂起态（锁屏 / 熄屏），改用合成帧");
            synthetic()
        }
    }
}

/// 合成动画帧的采集后端：每帧在 CPU 侧画一块左右移动的亮条 + 逐行渐变，整帧上传到一张 NV12 纹理。
struct SyntheticCapture {
    shared: Arc<rc_d3d11::SharedDevice>,
    texture: ID3D11Texture2D,
    resource: ID3D11Resource,
    plane: Vec<u8>,
    width: u32,
    height: u32,
    interval: Duration,
    next_frame_at: Option<Instant>,
    origin_us: u64,
    seq: u64,
    index: u32,
}

// COM 指针只在采集线程上使用；Send 为满足 CaptureBackend 契约的一次性移交。
unsafe impl Send for SyntheticCapture {}

/// 帧纹理的保活守卫（纹理实际由 SyntheticCapture 持有，这里只是多留一份引用）。
struct TextureGuard {
    _texture: ID3D11Texture2D,
}
unsafe impl Send for TextureGuard {}
unsafe impl Sync for TextureGuard {}

impl SyntheticCapture {
    fn new(width: u32, height: u32, fps: u32, origin_us: u64) -> std::result::Result<Self, String> {
        let shared =
            rc_d3d11::SharedDevice::get().map_err(|err| format!("共享 D3D11 设备不可用：{err}"))?;
        let texture = shared
            .create_nv12_texture(width, height)
            .map_err(|err| format!("创建合成帧纹理失败：{err}"))?;
        let resource = texture
            .cast()
            .map_err(|err| format!("合成帧纹理转资源失败：{err}"))?;
        Ok(Self {
            shared,
            texture,
            resource,
            plane: vec![0; (width * height * 3 / 2) as usize],
            width,
            height,
            interval: Duration::from_micros(1_000_000 / u64::from(fps.max(1))),
            next_frame_at: None,
            origin_us,
            seq: 0,
            index: 0,
        })
    }

    /// 画第 `index` 帧：Y 平面逐行渐变 + 一块左右往返的亮条，UV 平面中性。
    fn paint(&mut self) {
        let (width, height) = (self.width as usize, self.height as usize);
        let bar_width = (width / 8).max(1);
        let travel = width.saturating_sub(bar_width);
        let phase = (self.index as usize * 9) % (travel * 2).max(1);
        let bar_x = if phase < travel {
            phase
        } else {
            travel * 2 - phase
        };
        let luma = &mut self.plane[..width * height];
        for (row, line) in luma.chunks_exact_mut(width).enumerate() {
            line.fill(48 + (row * 96 / height.max(1)) as u8);
            let bar_end = (bar_x + bar_width).min(width);
            line[bar_x..bar_end].fill(235);
        }
        self.plane[width * height..].fill(128);
        self.index = self.index.wrapping_add(1);
    }
}

impl CaptureBackend for SyntheticCapture {
    fn next_frame(&mut self, _timeout: Duration) -> rc_types::Result<Option<CapturedFrame>> {
        // 自带节拍：睡到下一帧的时刻；落后超过两帧时重新对齐，避免追帧
        let now = Instant::now();
        let due = self.next_frame_at.map_or(now, |at| {
            if at + self.interval * 2 < now {
                now
            } else {
                at
            }
        });
        if due > now {
            std::thread::sleep(due - now);
        }
        self.next_frame_at = Some(due + self.interval);

        self.paint();
        let context = self.shared.context();
        unsafe {
            context.UpdateSubresource(
                &self.resource,
                0,
                None,
                self.plane.as_ptr().cast(),
                self.width,
                0,
            )
        };
        self.seq += 1;
        let now_us = rc_win32core::host_time_us();
        Ok(Some(CapturedFrame {
            texture: FrameTexture::D3D11(Interface::as_raw(&self.texture)),
            // 与真实采集一样：内容变化时刻，会话时钟域
            timestamp: Timestamp(
                u32::try_from(now_us.saturating_sub(self.origin_us)).unwrap_or(u32::MAX),
            ),
            dirty: None,
            seq: rc_types::CaptureSeq(self.seq),
            is_repeat: false,
            resource_guard: Some(Arc::new(TextureGuard {
                _texture: self.texture.clone(),
            })),
        }))
    }

    fn poll_invalidation(&mut self) -> Option<InvalidationReason> {
        None
    }

    fn rebuild(&mut self) -> rc_types::Result<()> {
        Ok(())
    }

    fn display_info(&self) -> DisplayInfo {
        DisplayInfo {
            id: DisplayId(0),
            resolution: Resolution::new(self.width, self.height),
            scale: 1.0,
            hdr: false,
        }
    }
}
