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

pub mod probe;

use std::time::Duration;

use rc_capture::{CaptureBackend, CapturedFrame, DisplayInfo, InvalidationReason};
use rc_types::{DisplayId, Error, Resolution, Result};

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

/// DXGI Desktop Duplication 采集后端（骨架占位）。
// 字段在 DXGI 实现落地后投入使用。
#[allow(dead_code)]
pub struct DxgiBackend {
    cfg: DxgiConfig,
    display: DisplayInfo,
    seq: u64,
}

/// 构造 DXGI 采集后端。
pub fn new_backend(cfg: DxgiConfig) -> Result<DxgiBackend> {
    // TODO: rc_d3d11::SharedDevice::get() → 按 cfg.display 枚举 IDXGIOutput
    // → DuplicateOutput；帧到达后拷贝到共享设备上的自有纹理再交付
    // （duplication 帧在下次 Acquire 前必须释放）。
    Ok(DxgiBackend {
        cfg,
        display: DisplayInfo {
            id: DisplayId(cfg.display),
            resolution: cfg.resolution.unwrap_or(Resolution::new(1920, 1080)),
            scale: 1.0,
            hdr: false,
        },
        seq: 0,
    })
}

impl CaptureBackend for DxgiBackend {
    fn next_frame(&mut self, _timeout: Duration) -> Result<Option<CapturedFrame>> {
        // TODO: AcquireNextFrame(timeout) → 超时/无变化 Ok(None)；
        // DirtyRects 合并裁剪；LastPresentTime 按 origin_us 换算为
        // host_time_us 时间轴上的内容变化时刻。
        Err(Error::InvalidConfig(
            "dxgi capture not yet implemented".into(),
        ))
    }

    fn poll_invalidation(&mut self) -> Option<InvalidationReason> {
        // TODO: 订阅 WM_DISPLAYCHANGE / WTS_SESSION_LOCK 上报失效。
        None
    }

    fn rebuild(&mut self) -> Result<()> {
        // TODO: 幂等重建 duplication：释放旧 IDXGIOutputDuplication 后
        // 重新 DuplicateOutput；连续调用不报错（契约 C5）。
        Ok(())
    }

    fn display_info(&self) -> DisplayInfo {
        self.display
    }
}

/// 主屏逻辑分辨率（DPI 缩放后）。
///
/// TODO: `GetDpiForSystem` + `GetSystemMetrics` 换算。
pub fn main_display_logical_resolution() -> Option<Resolution> {
    None
}

/// 主屏物理分辨率。
pub fn main_display_backing_resolution() -> Option<Resolution> {
    None
}

/// 显示模式预设。
#[derive(Debug, Clone, Copy)]
pub struct DisplayModePreset {
    /// 宽（像素）。
    pub width: u32,
    /// 高（像素）。
    pub height: u32,
    /// 是否 HiDPI 模式。
    pub is_hidpi: bool,
}

/// 枚举主屏可用显示模式。
///
/// TODO: `EnumDisplaySettingsW`（`ENUM_CURRENT_SETTINGS`/`ENUM_REGISTRY_SETTINGS` 遍历）。
pub fn list_main_display_modes() -> Vec<DisplayModePreset> {
    Vec::new()
}

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
pub fn main_display_wallpaper_jpeg(_max_width: u32, _max_height: u32) -> Option<Vec<u8>> {
    None
}

/// 立即锁定屏幕。
///
/// TODO: `LockWorkStation`。
pub fn lock_screen_now() -> bool {
    false
}
