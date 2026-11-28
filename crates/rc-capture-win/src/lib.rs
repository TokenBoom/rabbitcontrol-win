//! RabbitControl Windows 屏幕采集与显示器控制。
//!
//! # 采集路径
//!
//! - **DXGI Desktop Duplication**（主路径）：`IDXGIOutputDuplication` 的
//!   `AcquireNextFrame` 在屏幕无变化时阻塞到超时返回空——天然满足 core
//!   契约 C1（静止不出帧、CPU 占用趋零）；`LastPresentTime` 给出内容变化
//!   时刻（契约 C2）；`DirtyRects` 上报脏区；`DXGI_ERROR_ACCESS_LOST`
//!   （锁屏/分辨率切换/会话切换）映射为可恢复失效，重建后继续（契约
//!   C4/C5）。
//! - **Windows Graphics Capture**（备选路径）：`Windows.Graphics.Capture`
//!   WinRT API，支持单窗口采集、HDR 与免管理员侵入式捕获；帧池回调驱动，
//!   在 [`rc_win32core`] 的 COM/消息设施上运行。
//!
//! 两条路径产出的纹理都创建在 [`rc_d3d11::SharedDevice`] 共享设备上，
//! 以 `FrameTexture::D3D11` 不透明句柄交给编码器，全链路零拷贝。
//!
//! # 显示器控制
//!
//! 本 crate 同时提供被控会话需要的显示原语：主屏逻辑/物理分辨率查询
//! （每显示器 DPI 感知）、显示模式枚举与切换（[`DisplayModeGuard`] 会话
//! 守卫，结束恢复原分辨率）、锁屏、壁纸截取。
//!
//! # 当前状态
//!
//! [`probe`]（DXGI 适配器/输出枚举探测）已实现；采集/显示控制仍为骨架。
//! unsafe 策略：crate 级 `deny(unsafe_code)`，FFI 边界模块定点 `allow`。

#![deny(missing_docs)]
#![deny(unsafe_code)]

pub mod dxgi;
pub mod probe;

pub use dxgi::{
    conversion_gpu_ms, list_main_display_modes, main_display_backing_resolution,
    main_display_logical_resolution, relay_self_check, DisplayModePreset, DxgiBackend,
};

use rc_capture::CaptureBackend;
pub use rc_capture::DisplayInfo;
use rc_types::{Resolution, Result};

/// DXGI 采集配置。
#[derive(Debug, Clone, Copy)]
pub struct DxgiConfig {
    /// 目标显示器索引（0 = 主屏）。
    pub display: u32,
    /// 目标帧率。
    pub fps: u32,
    /// 期望采集分辨率（`None` = 显示器原生分辨率）。
    pub resolution: Option<Resolution>,
    /// 平台单调时钟原点（微秒），帧时间戳以此为零偏移基准。
    pub origin_us: Option<u64>,
}

/// 构造 DXGI 采集后端（duplication 立即建立，失败上抛由平台层降级）。
pub fn new_backend(cfg: DxgiConfig) -> Result<DxgiBackend> {
    let result = DxgiBackend::new(cfg);
    if let Err(err) = &result {
        eprintln!("[capture] DXGI 采集后端初始化失败：{err}");
    }
    result
}

/// 包装为 trait 对象 + 后端标签（平台 provider 使用）。
pub fn new_backend_trait(cfg: DxgiConfig) -> Result<(Box<dyn CaptureBackend>, &'static str)> {
    let backend = new_backend(cfg)?;
    Ok((Box::new(backend), "dxgi"))
}

/// 主屏逻辑分辨率（DPI 缩放后）。
///
/// TODO: `GetDpiForSystem` + `GetSystemMetrics` 换算。
/// 主屏物理分辨率。
/// 枚举主屏可用显示模式。
///
/// TODO: `EnumDisplaySettingsW`（`ENUM_CURRENT_SETTINGS`/`ENUM_REGISTRY_SETTINGS` 遍历）。
/// 显示模式会话守卫：切换到目标分辨率，[`DisplayModeGuard::restore`] 恢复。
///
/// TODO: `ChangeDisplaySettingsExW`（`CDS_FULLSCREEN` 避免写入注册表）。
pub struct DisplayModeGuard {
    original: Resolution,
}

impl DisplayModeGuard {
    /// 捕获当前主屏显示模式。
    pub fn capture_current() -> Option<Self> {
        None
    }

    /// 原始分辨率。
    pub fn original_resolution(&self) -> Resolution {
        self.original
    }

    /// 切换到目标分辨率，返回实际生效的分辨率。
    pub fn switch_to(
        &mut self,
        _target_w: u32,
        _target_h: u32,
    ) -> std::result::Result<Resolution, String> {
        Err("display mode switch not yet implemented".into())
    }

    /// 恢复原分辨率。
    pub fn restore(&mut self) {}
}

/// 主屏壁纸 JPEG 截图（缩放到 `max_width`×`max_height` 内）。
///
/// TODO: 读取壁纸源文件（`SystemParametersInfoW(SPI_GETDESKWALLPAPER)`）
/// 经 WIC 缩放转码 JPEG；纯色/幻灯片目录回退为桌面截图。
/// 立即锁定屏幕。
///
/// TODO: `LockWorkStation`。
pub fn lock_screen_now() -> bool {
    false
}

/// 主屏 [`DisplayInfo`] 快照（采集前协商用）。
pub fn main_display_info() -> Option<DisplayInfo> {
    let backing = main_display_backing_resolution()?;
    let logical = main_display_logical_resolution()?;
    let scale = if logical.width > 0 {
        backing.width as f32 / logical.width as f32
    } else {
        1.0
    };
    Some(DisplayInfo {
        id: rc_types::DisplayId(0),
        resolution: backing,
        scale,
        hdr: false,
    })
}

/// 主屏壁纸 JPEG 快照（M5b.4 被控状态页用；当前未实装返回 None）。
pub fn main_display_wallpaper_jpeg(_max_width: u32, _max_height: u32) -> Option<Vec<u8>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_helpers_do_not_panic() {
        // 真实显示器环境返回 Some；CI 无头环境允许 None
        let _ = main_display_backing_resolution();
        let _ = main_display_logical_resolution();
        let _ = main_display_info();
        let modes = list_main_display_modes();
        let _ = modes.len();
    }
}
