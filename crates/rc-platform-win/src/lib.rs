//! RabbitControl Windows 平台能力总成（对应 macOS `rc-platform-macos`）。
//!
//! 统一将 Windows 原生硬件后端（DXGI/WGC 采集、Media Foundation 编解码、
//! D3D11 渲染、SendInput 输入、WASAPI 音频、Win32 剪贴板与显示器控制）
//! 注册至 [`rabbitcontrol-core`](rc_pipeline) 对应的抽象提供者槽位。
//!
//! 主程序 / Host 驻留端 / CLI 入口最早阶段调用 [`register_all`] 即可。

#![deny(missing_docs)]
#![forbid(unsafe_code)]

use std::any::Any;
use std::sync::Arc;
use std::time::Instant;

use rc_capture::CaptureBackend;
use rc_client_core::platform::{
    ClientPlatformHooks, DisplayModeSessionGuard, HardwareRendererOutput, PlatformDisplayMode,
    PreallocatedRenderSurface,
};
use rc_decode::Decoder;
use rc_encode::Encoder;
use rc_input::InputInjector;
use rc_pipeline::{PipelineConfig, PlatformMediaProvider};
use rc_render::Renderer;
use rc_types::{Resolution, Result};

struct WinMediaProvider;

impl PlatformMediaProvider for WinMediaProvider {
    fn host_clock_us(&self) -> Option<u64> {
        Some(rc_capture_win::host_time_us())
    }

    fn promote_current_thread_qos(&self) {
        rc_capture_win::promote_current_thread_qos();
    }

    fn create_capture(
        &self,
        cfg: &PipelineConfig,
    ) -> Result<(Box<dyn CaptureBackend>, &'static str)> {
        let dxgi = rc_capture_win::new_backend(rc_capture_win::DxgiConfig {
            display: 0,
            fps: cfg.fps,
            resolution: Some(cfg.resolution.to_even()),
            origin_us: cfg.clock_origin_us,
        })?;
        Ok((Box::new(dxgi), "dxgi"))
    }

    fn create_encoder(&self, cfg: &PipelineConfig) -> Result<(Box<dyn Encoder>, &'static str)> {
        let enc = rc_encode_mf::MfEncoder::new(rc_pipeline::default_encoder_config(cfg))?;
        Ok((Box::new(enc), "mediafoundation"))
    }

    fn create_decoder(&self, cfg: &PipelineConfig) -> Result<(Box<dyn Decoder>, &'static str)> {
        let dec = rc_decode_mf::new_decoder(rc_decode_mf::MfDecoderConfig {
            codec: cfg.codec,
            max_resolution: Resolution::new(
                cfg.resolution.width.max(4096),
                cfg.resolution.height.max(4096),
            ),
        })?;
        Ok((Box::new(dec), "mediafoundation"))
    }

    fn supports_hevc(&self) -> bool {
        // TODO: MFT 枚举探测 HEVC 硬编/硬解能力后再上报。
        false
    }

    fn create_renderer(&self, cfg: &PipelineConfig) -> Result<(Box<dyn Renderer>, &'static str)> {
        let renderer = rc_render_win::D3d11Renderer::new(rc_render_win::D3d11RendererConfig {
            size: (cfg.resolution.width, cfg.resolution.height),
            vsync: false,
            session_start: None,
            cursor_compositing: true,
        })?;
        Ok((Box::new(renderer), "d3d11"))
    }
}

struct WinDisplayModeGuardWrapper(rc_capture_win::DisplayModeGuard);

impl DisplayModeSessionGuard for WinDisplayModeGuardWrapper {
    fn original_resolution(&self) -> Resolution {
        self.0.original_resolution()
    }

    fn switch_to(
        &mut self,
        target_w: u32,
        target_h: u32,
    ) -> std::result::Result<Resolution, String> {
        self.0.switch_to(target_w, target_h)
    }

    fn restore(&mut self) {
        self.0.restore();
    }
}

struct WinClientPlatformHooks;

impl ClientPlatformHooks for WinClientPlatformHooks {
    fn promote_current_thread_qos(&self) {
        rc_capture_win::promote_current_thread_qos();
    }

    fn begin_latency_critical_activity(
        &self,
        reason: &str,
        interactive: bool,
    ) -> Option<Box<dyn Any + Send>> {
        Some(Box::new(rc_capture_win::LatencyCriticalActivity::begin(
            reason,
            interactive,
        )))
    }

    fn main_display_logical_resolution(&self) -> Option<Resolution> {
        rc_capture_win::main_display_logical_resolution()
    }

    fn main_display_backing_resolution(&self) -> Option<Resolution> {
        rc_capture_win::main_display_backing_resolution()
    }

    fn list_main_display_modes(&self) -> Vec<PlatformDisplayMode> {
        rc_capture_win::list_main_display_modes()
            .into_iter()
            .map(|m| PlatformDisplayMode {
                width: m.width,
                height: m.height,
                is_hidpi: m.is_hidpi,
            })
            .collect()
    }

    fn capture_display_mode_guard(&self) -> Option<Box<dyn DisplayModeSessionGuard>> {
        rc_capture_win::DisplayModeGuard::capture_current()
            .map(|g| Box::new(WinDisplayModeGuardWrapper(g)) as Box<dyn DisplayModeSessionGuard>)
    }

    fn capture_wallpaper_jpeg(&self, max_width: u32, max_height: u32) -> Option<Vec<u8>> {
        rc_capture_win::main_display_wallpaper_jpeg(max_width, max_height)
    }

    fn lock_screen_now(&self) -> bool {
        rc_capture_win::lock_screen_now()
    }

    fn create_main_display_injector(&self) -> Option<Box<dyn InputInjector>> {
        rc_input_win::InputBridge::for_main_display()
            .ok()
            .map(|b| Box::new(b) as Box<dyn InputInjector>)
    }

    fn create_hardware_renderer(
        &self,
        width: u32,
        height: u32,
        session_start: Instant,
        preallocated: Option<&PreallocatedRenderSurface>,
    ) -> std::result::Result<HardwareRendererOutput, String> {
        let cfg = rc_render_win::D3d11RendererConfig {
            size: (width, height),
            vsync: false,
            session_start: Some(session_start),
            cursor_compositing: true,
        };
        let renderer = match preallocated {
            Some(surface) => rc_render_win::D3d11Renderer::from_raw_panel(surface.raw_ptr(), cfg)
                .map_err(|e| e.to_string())?,
            None => rc_render_win::D3d11Renderer::new(cfg).map_err(|e| e.to_string())?,
        };
        let layer_addr = renderer.surface_ptr();
        Ok(HardwareRendererOutput {
            renderer: Box::new(renderer),
            layer_addr,
            label: "d3d11",
        })
    }
}

/// 幂等注册全部 Windows 硬件提供者与原生钩子（应在主程序/Agent/CLI 入口最早阶段调用）。
pub fn register_all() {
    let _ = rc_pipeline::register_platform_provider(Arc::new(WinMediaProvider));
    let _ = rc_audio_win::register();
    let _ = rc_clipboard_win::register();
    let _ = rc_client_core::platform::register_platform_hooks(Arc::new(WinClientPlatformHooks));
}
