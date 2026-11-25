//! RabbitControl Windows 屏幕采集后端。
//!
//! # 设计（对应 macOS `rc-capture-macos` / ScreenCaptureKit）
//!
//! 计划实现两条采集路径：
//! - **DXGI Desktop Duplication**：主路径。`IDXGIOutputDuplication::AcquireNextFrame`
//!   天然满足契约 C1（无变化阻塞超时返回 `Ok(None)`）、C2（`LastPresentTime`
//!   即内容变化时刻）与脏区上报（`DirtyRects`）；`DXGI_ERROR_ACCESS_LOST`
//!   映射为 [`InvalidationReason::AccessLost`]（契约 C4 可恢复重建）。
//! - **Windows Graphics Capture (WGC)**：备选路径，支持单窗口采集与 HDR，
//!   通过 `Windows.Graphics.Capture` WinRT API + 帧池回调驱动。
//!
//! 产出的 [`FrameTexture::D3D11`] 句柄为 `ID3D11Texture2D` COM 指针，
//! 交给 `rc-encode-mf` 零拷贝编码。
//!
//! 本 crate 同时提供显示器控制原语（对应 macOS `ffi` 层）：
//! 主屏逻辑/物理分辨率、显示模式枚举与切换（`DisplayModeGuard` 会话守卫）、
//! 锁屏、壁纸截取、主机单调时钟与线程 QoS 提升。
//!
//! # 当前状态
//!
//! 骨架占位：全部返回未实现错误，接口形状已与 core 契约对齐。

#![deny(missing_docs)]
#![forbid(unsafe_code)]

use std::time::Duration;

use rc_capture::{CaptureBackend, CapturedFrame, DisplayInfo, InvalidationReason};
use rc_types::{DisplayId, Error, Resolution, Result};

/// DXGI 采集配置（对应 macOS `SckConfig`）。
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

/// 构造 DXGI 采集后端（对应 macOS `new_backend`）。
pub fn new_backend(cfg: DxgiConfig) -> Result<DxgiBackend> {
    // TODO: 创建 D3D11 设备 → 枚举 IDXGIOutput → DuplicateOutput。
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
        // TODO: AcquireNextFrame + 脏区裁剪 + LastPresentTime 换算时间戳。
        Err(Error::InvalidConfig(
            "dxgi capture not yet implemented".into(),
        ))
    }

    fn poll_invalidation(&mut self) -> Option<InvalidationReason> {
        None
    }

    fn rebuild(&mut self) -> Result<()> {
        // TODO: 幂等重建 duplication（分辨率变化 / ACCESS_LOST / 设备移除）。
        Ok(())
    }

    fn display_info(&self) -> DisplayInfo {
        self.display
    }
}

/// 主机单调时钟（微秒）。对应 macOS `host_time_us`。
///
/// TODO: 用 `QueryPerformanceCounter` 实现；骨架期先以进程内单调时钟代替。
pub fn host_time_us() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    ORIGIN.get_or_init(Instant::now).elapsed().as_micros() as u64
}

/// 提升当前线程调度优先级（对应 macOS `promote_current_thread_qos`）。
///
/// TODO: `SetThreadPriority` / MMCSS `AvSetMmThreadCharacteristics`（"Pro Audio"）。
pub fn promote_current_thread_qos() {}

/// 延迟关键活动守卫（对应 macOS `LatencyCriticalActivity`）。
///
/// TODO: Windows 上用 `PowerSetRequest`（`ES_SYSTEM_REQUIRED`/`ES_DISPLAY_REQUIRED`）
/// 阻止采集会话期间系统休眠。
pub struct LatencyCriticalActivity;

impl LatencyCriticalActivity {
    /// 开始一个延迟关键活动。
    pub fn begin(_reason: &str, _interactive: bool) -> Self {
        LatencyCriticalActivity
    }
}

/// 主屏逻辑分辨率（DPI 缩放后）。TODO: `GetSystemMetrics` + 每显示器 DPI。
pub fn main_display_logical_resolution() -> Option<Resolution> {
    None
}

/// 主屏物理（backing）分辨率。
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

/// 枚举主屏可用显示模式。TODO: `EnumDisplaySettingsW`。
pub fn list_main_display_modes() -> Vec<DisplayModePreset> {
    Vec::new()
}

/// 显示模式会话守卫：切换到目标分辨率，Drop/restore 时恢复原分辨率。
/// TODO: `ChangeDisplaySettingsExW`（`CDS_FULLSCREEN`）。
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

/// 主屏壁纸 JPEG 截图（缩放到 `max_width`×`max_height` 内）。TODO: 读壁纸文件 + WIC 转码。
pub fn main_display_wallpaper_jpeg(_max_width: u32, _max_height: u32) -> Option<Vec<u8>> {
    None
}

/// 立即锁定屏幕。TODO: `LockWorkStation`。
pub fn lock_screen_now() -> bool {
    false
}
