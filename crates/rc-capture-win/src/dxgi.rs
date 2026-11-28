//! DXGI Desktop Duplication 采集后端实装（M5a.2）。
//!
//! # 管线
//!
//! ```text
//! [采集专用设备] IDXGIOutput1::DuplicateOutput
//!   → AcquireNextFrame(≤16ms)                     （无变化 → Ok(None)，铁律 2）
//!   → CopyResource 到跨设备中转纹理 → ReleaseFrame → 等本设备 GPU 写完
//! [共享设备]     CopyResource 到自有 BGRA 纹理
//!   → VideoProcessorBlt BGRA → NV12               （BT.709 有限范围，缩放到协商分辨率；
//!                                                   每帧一张纹理环，编码器零拷贝输入）
//!   → CapturedFrame { texture: FrameTexture::D3D11 }
//! ```
//!
//! # 两个设备
//!
//! 桌面复制放在**独立的 D3D11 设备**上（[`SharedDevice::open_isolated_device`]），帧经共享
//! 纹理中转到全进程共享设备。原因：`AcquireNextFrame` 是阻塞调用，等在共享设备上时会把
//! 同设备上其它线程的 D3D11 调用一并卡住——硬件编码器 MFT 的线程只能趁两次取帧的间隙
//! 推进，单帧编码从 ~3 ms 变成按显示器垂直同步周期（16.7 ms）跳（本机实测）。
//!
//! # 出帧的前提
//!
//! DWM 只在桌面有变化时合成，桌面复制也只在 DWM 合成之后才给帧。两种状态下连首帧
//! 都没有，后端统一按「挂起待重建」处理：
//!
//! - **显示器熄屏**：DWM 停止合成。上层（`host_bridge`）负责点亮显示器；
//! - **锁屏 / UAC 安全桌面**：用户桌面不再上屏。解锁后重建，管线强制 IDR 恢复。
//!
//! 挂起期间 `poll_invalidation` 持续上报、`rebuild` 持续返回可恢复错误，管线因此
//! 把采集线程停靠在 500ms 退避的重试循环里，而不是空转轮询。
//!
//! # 已知限制（v1）
//!
//! - 仅主适配器（SharedDevice 未绑定多适配器，多 GPU 多显示器场景未覆盖）；
//! - 屏幕旋转未做坐标补偿；
//! - 光标不并入帧内容（鼠标-only 更新按无内容帧丢弃，由控制端叠加）；
//! - 脏区上报为全帧（`dirty: None`），上层按全帧处理。

#![allow(unsafe_code)] // DXGI / D3D11 FFI 边界

use std::sync::Arc;
use std::time::Duration;

use rc_capture::{CaptureBackend, CapturedFrame, DisplayInfo, FrameTexture, InvalidationReason};
use rc_d3d11::SharedDevice;
use rc_types::{DisplayId, Error, Resolution, Result};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Query, ID3D11Resource, ID3D11Texture2D,
    ID3D11VideoContext, ID3D11VideoContext1, ID3D11VideoDevice, ID3D11VideoProcessor,
    ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_QUERY_DESC, D3D11_QUERY_EVENT,
    D3D11_RESOURCE_MISC_SHARED, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO,
};

use crate::DxgiConfig;

/// 重复帧判定阈值：AccumulatedFrames == 0 视为无内容更新。
const NO_CONTENT_FRAMES: u32 = 0;
/// 锁屏 / 熄屏状态的查询间隔（两种查询都是系统调用，不必每帧都查）。
const STATE_POLL_INTERVAL_US: u64 = 250_000;
/// NV12 帧纹理环长度。同时活着的帧：采集交接 1 + 调度在途 2 + 正在编码 1；再留余量给
/// 「输出迟到、MFT 还攥着输入没放」的帧。每帧必须有自己的纹理——复用同一张时，下一帧的
/// 色彩转换要写入编码器正在读的纹理，驱动只能把两者串行化。
const NV12_RING: usize = 6;

/// 可恢复的采集错误（资源问题按「重建可解决」上报，不拆会话）。
fn recoverable(msg: impl Into<String>) -> Error {
    Error::Capture {
        recoverable: true,
        msg: msg.into(),
    }
}

/// 纹理尺寸。
fn texture_size(texture: &ID3D11Texture2D) -> (u32, u32) {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };
    (desc.Width, desc.Height)
}

/// 当前是否处于「桌面复制不出帧」的状态（锁屏 / UAC 安全桌面 / 显示器熄屏）。
fn desktop_is_unavailable() -> bool {
    rc_win32core::screen_is_locked() || rc_win32core::display_is_asleep()
}

/// NV12 输出尺寸：协商分辨率不超过桌面尺寸时按协商值缩放，否则输出桌面原尺寸
/// （宽高各取偶数，NV12 的半分辨率色度平面要求）。
fn output_size(negotiated: Option<Resolution>, width: u32, height: u32) -> (u32, u32) {
    match negotiated {
        Some(resolution) => (
            resolution.width.min(width) & !1,
            resolution.height.min(height) & !1,
        ),
        None => (width, height),
    }
}

/// 跨设备中转纹理：采集专用设备写、共享设备读。
struct Relay {
    /// 采集专用设备上的写端。
    writer: ID3D11Texture2D,
    /// 共享设备上的读端。
    reader: ID3D11Texture2D,
}

impl Relay {
    /// 在 `device` 上建一张可共享的 BGRA 纹理，并在共享设备上打开它。
    fn new(device: &ID3D11Device, width: u32, height: u32) -> Result<Self> {
        let mut writer = None;
        unsafe {
            device.CreateTexture2D(
                &D3D11_TEXTURE2D_DESC {
                    Width: width,
                    Height: height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET).0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: D3D11_RESOURCE_MISC_SHARED.0 as u32,
                },
                None,
                Some(&mut writer),
            )
        }
        .map_err(|e| Error::InvalidConfig(format!("创建中转纹理失败：{e}")))?;
        let writer = writer.ok_or_else(|| Error::InvalidConfig("中转纹理未返回".into()))?;
        let resource: IDXGIResource = writer
            .cast()
            .map_err(|e| Error::InvalidConfig(format!("中转纹理取共享接口失败：{e}")))?;
        let handle = unsafe { resource.GetSharedHandle() }
            .map_err(|e| Error::InvalidConfig(format!("取中转纹理共享句柄失败：{e}")))?;
        let reader = SharedDevice::get()
            .map_err(Error::InvalidConfig)?
            .open_shared_texture(handle)
            .map_err(Error::InvalidConfig)?;
        Ok(Self { writer, reader })
    }
}

/// 桌面复制源：独立的采集专用设备 + duplication + 中转纹理。
struct DuplicationSource {
    display: u32,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output: IDXGIOutput1,
    duplication: IDXGIOutputDuplication,
    desc: DXGI_OUTDUPL_DESC,
    /// 中转纹理（随桌面尺寸重建）。
    relay: Option<Relay>,
    /// 用来等本设备 GPU 工作做完的事件查询。
    done_query: ID3D11Query,
}

impl DuplicationSource {
    /// 在独立设备上枚举目标输出并创建 duplication。
    fn new(display: u32) -> Result<Self> {
        let shared = SharedDevice::get().map_err(Error::InvalidConfig)?;
        let device = shared
            .open_isolated_device()
            .map_err(Error::InvalidConfig)?;
        let context = unsafe { device.GetImmediateContext() }
            .map_err(|e| Error::InvalidConfig(format!("取采集设备上下文失败：{e}")))?;
        let output = unsafe { open_output(display) }?;
        let duplication = unsafe { output.DuplicateOutput(&device) }
            .map_err(|e| Error::InvalidConfig(format!("DuplicateOutput 失败：{e}")))?;
        let desc = unsafe { duplication.GetDesc() };
        eprintln!(
            "[capture] DXGI duplication 已建立：{}x{}（显示器 {}）",
            desc.ModeDesc.Width, desc.ModeDesc.Height, display
        );
        let mut query = None;
        unsafe {
            device.CreateQuery(
                &D3D11_QUERY_DESC {
                    Query: D3D11_QUERY_EVENT,
                    MiscFlags: 0,
                },
                Some(&mut query),
            )
        }
        .map_err(|e| Error::InvalidConfig(format!("创建采集完成查询失败：{e}")))?;
        let done_query = query.ok_or_else(|| Error::InvalidConfig("采集完成查询未返回".into()))?;
        Ok(Self {
            display,
            device,
            context,
            output,
            duplication,
            desc,
            relay: None,
            done_query,
        })
    }

    /// 当前桌面图像描述（尺寸等）。
    fn desc(&self) -> DXGI_OUTDUPL_DESC {
        self.desc
    }

    /// 确保中转纹理与桌面尺寸一致。
    fn ensure_relay(&mut self, width: u32, height: u32) -> Result<()> {
        if self
            .relay
            .as_ref()
            .is_some_and(|relay| texture_size(&relay.writer) == (width, height))
        {
            return Ok(());
        }
        self.relay = Some(Relay::new(&self.device, width, height)?);
        Ok(())
    }

    /// 中转纹理在共享设备上的读端（调用前须已成功 [`Self::acquire_into_relay`]）。
    fn relay_reader(&self) -> Result<ID3D11Texture2D> {
        self.relay
            .as_ref()
            .map(|relay| relay.reader.clone())
            .ok_or_else(|| Error::InvalidConfig("中转纹理尚未创建".into()))
    }

    /// 等到下一帧桌面更新并搬进中转纹理。返回桌面尺寸。
    ///
    /// `Ok(None)`：超时、无内容更新（鼠标-only），或访问丢失（此时置 `invalidation`）。
    fn acquire_into_relay(
        &mut self,
        timeout: Duration,
        invalidation: &mut Option<InvalidationReason>,
    ) -> Result<Option<(u32, u32)>> {
        let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        let acquired = unsafe {
            self.duplication
                .AcquireNextFrame(timeout_ms, &mut info, &mut resource)
        };
        if let Err(err) = acquired {
            let code = err.code();
            if code == DXGI_ERROR_WAIT_TIMEOUT {
                return Ok(None);
            }
            if code == DXGI_ERROR_ACCESS_LOST {
                eprintln!(
                    "[capture] duplication 访问丢失（锁屏 / 切换桌面 / 显示模式变化），等待重建"
                );
                *invalidation = Some(InvalidationReason::AccessLost);
                return Ok(None);
            }
            return Err(recoverable(format!("AcquireNextFrame 失败：{err}")));
        }

        // 铁律 2：无内容更新（鼠标-only）不产帧
        let copied = self.copy_to_relay(resource.as_ref(), &info);
        // duplication 帧必须立即释放（契约：句柄仅到下一次采集）
        let _ = unsafe { self.duplication.ReleaseFrame() };
        let Some(size) = copied? else {
            return Ok(None);
        };
        self.wait_gpu();
        Ok(Some(size))
    }

    /// 把 duplication 交出的桌面帧拷进中转纹理，返回桌面尺寸；无内容更新返回 `Ok(None)`。
    fn copy_to_relay(
        &mut self,
        resource: Option<&IDXGIResource>,
        info: &DXGI_OUTDUPL_FRAME_INFO,
    ) -> Result<Option<(u32, u32)>> {
        if info.AccumulatedFrames == NO_CONTENT_FRAMES {
            return Ok(None);
        }
        let desktop: ID3D11Texture2D = resource
            .ok_or_else(|| recoverable("AcquireNextFrame 未返回桌面帧"))?
            .cast()
            .map_err(|e| recoverable(format!("桌面帧非纹理：{e}")))?;
        let (width, height) = texture_size(&desktop);
        self.ensure_relay(width, height)?;
        let relay = self
            .relay
            .as_ref()
            .ok_or_else(|| Error::InvalidConfig("中转纹理尚未创建".into()))?;
        let src: ID3D11Resource = desktop
            .cast()
            .map_err(|e| recoverable(format!("桌面帧转资源失败：{e}")))?;
        let dst: ID3D11Resource = relay
            .writer
            .cast()
            .map_err(|e| recoverable(format!("中转纹理转资源失败：{e}")))?;
        unsafe { self.context.CopyResource(&dst, &src) };
        Ok(Some((width, height)))
    }

    /// 等本设备的 GPU 工作做完：中转纹理写完，共享设备那侧才能读。
    fn wait_gpu(&self) {
        unsafe {
            self.context.End(&self.done_query);
            self.context.Flush();
            let mut spins = 0u32;
            while self.context.GetData(&self.done_query, None, 0, 0).is_err() {
                if spins < 50 {
                    spins += 1;
                    std::thread::yield_now();
                } else {
                    std::thread::sleep(Duration::from_micros(250));
                }
            }
        }
    }

    /// 丢弃旧 duplication 重新建立；独立设备不可用时整个源一起重建（幂等，契约 C5）。
    fn rebuild(&mut self) -> Result<()> {
        match unsafe { self.output.DuplicateOutput(&self.device) } {
            Ok(duplication) => {
                // 赋值时旧的 duplication 随之释放
                self.desc = unsafe { duplication.GetDesc() };
                self.duplication = duplication;
                Ok(())
            }
            Err(err) => {
                eprintln!("[capture] 重建 duplication 失败，重建采集设备：{err}");
                *self = Self::new(self.display)?;
                Ok(())
            }
        }
    }
}

/// BGRA → NV12 视频处理器（输入为桌面尺寸，输出为协商分辨率；任一变化即重建）。
struct VideoConverter {
    context: ID3D11VideoContext,
    processor: ID3D11VideoProcessor,
    input_view: ID3D11VideoProcessorInputView,
    /// 与 NV12 帧纹理环一一对应的输出视图。
    output_views: Vec<ID3D11VideoProcessorOutputView>,
}

/// 帧纹理保活守卫（Arc 存入 CapturedFrame.resource_guard）。
struct Nv12Guard {
    _texture: ID3D11Texture2D,
}
// COM 引用自身线程安全（仅存活到编码器读取完本帧）
unsafe impl Send for Nv12Guard {}
unsafe impl Sync for Nv12Guard {}

/// DXGI Desktop Duplication 采集后端。
pub struct DxgiBackend {
    cfg: DxgiConfig,
    display: DisplayInfo,
    seq: u64,
    /// 桌面复制源（在独立的采集专用设备上）。
    source: DuplicationSource,
    bgra_copy: Option<ID3D11Texture2D>,
    /// NV12 帧纹理环（每帧一张，尺寸变化时重建）。
    nv12_ring: Vec<ID3D11Texture2D>,
    ring_next: usize,
    converter: Option<VideoConverter>,
    /// 待重建的失效原因。`Some` 期间不出帧，直到 `rebuild` 成功才清零。
    invalidation: Option<InvalidationReason>,
    /// 上次查询锁屏 / 熄屏状态的主机时钟（微秒），用来节流查询。
    state_checked_at_us: Option<u64>,
    /// 自上次（重）建 duplication 以来是否还没出过帧（首帧日志用）。
    awaiting_first_frame: bool,
}

impl DxgiBackend {
    /// 枚举目标输出并创建 duplication。
    ///
    /// 锁屏 / 熄屏状态下 duplication 照样建得出来，只是不出帧：此时不报错，
    /// 直接进入「挂起待重建」态（返回 `Ok`），避免上层降级到假后端。
    pub fn new(cfg: DxgiConfig) -> Result<Self> {
        let source = DuplicationSource::new(cfg.display)?;
        let desc = source.desc();
        let display = DisplayInfo {
            id: DisplayId(cfg.display),
            resolution: Resolution::new(desc.ModeDesc.Width, desc.ModeDesc.Height),
            scale: 1.0,
            hdr: false,
        };
        let mut backend = Self {
            cfg,
            display,
            seq: 0,
            source,
            bgra_copy: None,
            nv12_ring: Vec::new(),
            ring_next: 0,
            converter: None,
            invalidation: None,
            state_checked_at_us: None,
            awaiting_first_frame: true,
        };
        backend.refresh_suspension();
        Ok(backend)
    }

    /// 丢弃全部帧相关的 GPU 资源（duplication 重建 / 尺寸变化时调用）。
    fn drop_frame_resources(&mut self) {
        self.converter = None;
        self.nv12_ring.clear();
        self.ring_next = 0;
        self.bgra_copy = None;
    }

    /// 确保 BGRA 拷贝纹理 / NV12 帧纹理环 / 视频处理器与当前桌面尺寸一致。
    fn ensure_pools(&mut self, width: u32, height: u32) -> Result<()> {
        let size_ok = self
            .bgra_copy
            .as_ref()
            .is_some_and(|texture| texture_size(texture) == (width, height));
        if size_ok {
            return Ok(());
        }
        self.drop_frame_resources();
        let shared = SharedDevice::get().map_err(Error::InvalidConfig)?;
        let device = shared.device();
        let mut bgra = None;
        unsafe {
            device.CreateTexture2D(
                &D3D11_TEXTURE2D_DESC {
                    Width: width,
                    Height: height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET).0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                },
                None,
                Some(&mut bgra),
            )
        }
        .map_err(|e| Error::InvalidConfig(format!("创建 BGRA 拷贝纹理失败：{e}")))?;
        let bgra = bgra.ok_or_else(|| Error::InvalidConfig("BGRA 拷贝纹理未返回".into()))?;

        let (out_width, out_height) = output_size(self.cfg.resolution, width, height);
        let nv12_ring = (0..NV12_RING)
            .map(|_| shared.create_nv12_texture(out_width, out_height))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Error::InvalidConfig)?;
        let fps = self.cfg.fps.max(1);
        let frame_rate = DXGI_RATIONAL {
            Numerator: fps,
            Denominator: 1,
        };
        let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputWidth: width,
            InputHeight: height,
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: frame_rate,
            OutputFrameRate: frame_rate,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            OutputWidth: out_width,
            OutputHeight: out_height,
        };
        let context: ID3D11VideoContext1 = shared
            .context()
            .cast()
            .map_err(|e| Error::InvalidConfig(format!("上下文无视频接口：{e}")))?;
        let converter =
            unsafe { create_converter(&device, &context, &bgra, &nv12_ring, &content_desc)? };
        self.bgra_copy = Some(bgra);
        self.nv12_ring = nv12_ring;
        self.ring_next = 0;
        self.converter = Some(converter);
        Ok(())
    }

    /// 共享设备上：中转纹理 → 自有 BGRA 纹理 → NV12 帧纹理环的下一张（硬件转换 + 缩放）。
    fn convert_to_nv12(
        &mut self,
        relay: ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> Result<ID3D11Texture2D> {
        self.ensure_pools(width, height)?;
        let (Some(converter), Some(bgra)) = (self.converter.as_ref(), self.bgra_copy.as_ref())
        else {
            return Err(Error::InvalidConfig("视频处理器缺失".into()));
        };
        let index = self.ring_next;
        self.ring_next = (index + 1) % self.nv12_ring.len().max(1);
        let Some(nv12) = self.nv12_ring.get(index) else {
            return Err(Error::InvalidConfig("NV12 帧纹理环缺失".into()));
        };
        copy_resource(&relay, bgra)?;
        blt_to(converter, index)?;
        Ok(nv12.clone())
    }

    /// 节流查询锁屏 / 熄屏状态，进入挂起。
    ///
    /// 只置位不清零：清零统一发生在 `rebuild` 成功之后，这样失效在重建前持续上报
    /// （采集契约），管线也知道该等谁来恢复。
    fn refresh_suspension(&mut self) {
        if self.invalidation.is_some() {
            return;
        }
        let now = rc_win32core::host_time_us();
        if self
            .state_checked_at_us
            .is_some_and(|at| now.saturating_sub(at) < STATE_POLL_INTERVAL_US)
        {
            return;
        }
        self.state_checked_at_us = Some(now);
        if desktop_is_unavailable() {
            eprintln!("[capture] 锁屏或显示器熄屏：采集挂起（解锁 / 亮屏后自动恢复）");
            self.invalidation = Some(InvalidationReason::SessionLocked);
        }
    }
}

/// 创建视频处理器与输入 / 输出视图。
///
/// 色彩空间显式指定：输入是全范围 sRGB 桌面（BGRA），输出是 BT.709 有限范围 NV12——
/// 与 macOS 被控端（`420v` + BT.709）及各平台渲染端着色器严格对齐；不指定的话驱动
/// 默认按 BT.601 转换，会有色偏。
unsafe fn create_converter(
    device: &ID3D11Device,
    context: &ID3D11VideoContext1,
    input: &ID3D11Texture2D,
    outputs: &[ID3D11Texture2D],
    content_desc: &D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
) -> Result<VideoConverter> {
    let video_device: ID3D11VideoDevice = device
        .cast()
        .map_err(|e| Error::InvalidConfig(format!("设备无视频接口：{e}")))?;
    let enumerator: ID3D11VideoProcessorEnumerator = video_device
        .CreateVideoProcessorEnumerator(content_desc)
        .map_err(|e| Error::InvalidConfig(format!("创建视频处理器枚举失败：{e}")))?;
    let processor = video_device
        .CreateVideoProcessor(&enumerator, 0)
        .map_err(|e| Error::InvalidConfig(format!("创建视频处理器失败：{e}")))?;
    let mut input_view = None;
    video_device
        .CreateVideoProcessorInputView(
            input,
            &enumerator,
            &D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
            },
            Some(&mut input_view),
        )
        .map_err(|e| Error::InvalidConfig(format!("输入视图创建失败：{e}")))?;
    let Some(input_view) = input_view else {
        return Err(Error::InvalidConfig("视频处理器视图未返回".into()));
    };
    let output_views = outputs
        .iter()
        .map(|output| {
            let mut view = None;
            video_device
                .CreateVideoProcessorOutputView(
                    output,
                    &enumerator,
                    &D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                        ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                        Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                            Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                        },
                    },
                    Some(&mut view),
                )
                .map_err(|e| Error::InvalidConfig(format!("输出视图创建失败：{e}")))?;
            view.ok_or_else(|| Error::InvalidConfig("视频处理器视图未返回".into()))
        })
        .collect::<Result<Vec<_>>>()?;
    context.VideoProcessorSetStreamColorSpace1(
        &processor,
        0,
        DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709,
    );
    context.VideoProcessorSetOutputColorSpace1(
        &processor,
        DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    );
    Ok(VideoConverter {
        context: context
            .cast()
            .map_err(|e| Error::InvalidConfig(format!("视频上下文降级失败：{e}")))?,
        processor,
        input_view,
        output_views,
    })
}

/// 打开第 `index` 个显示输出（IDXGIOutput1）。
unsafe fn open_output(index: u32) -> Result<IDXGIOutput1> {
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, DXGI_ERROR_NOT_FOUND,
    };
    let factory: IDXGIFactory1 = CreateDXGIFactory1()
        .map_err(|e| Error::InvalidConfig(format!("CreateDXGIFactory1 失败：{e}")))?;
    let mut adapter_index = 0u32;
    loop {
        let adapter: IDXGIAdapter1 = factory
            .EnumAdapters1(adapter_index)
            .map_err(|_| Error::InvalidConfig("未找到显示适配器".into()))?;
        let mut output_index = 0u32;
        loop {
            match adapter.EnumOutputs(output_index) {
                Ok(output) => {
                    if output_index == index {
                        return output.cast::<IDXGIOutput1>().map_err(|e| {
                            Error::InvalidConfig(format!("输出不支持 duplication：{e}"))
                        });
                    }
                    output_index += 1;
                }
                Err(err) if err.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(err) => return Err(Error::InvalidConfig(format!("枚举输出失败：{err}"))),
            }
        }
        adapter_index += 1;
    }
}

impl CaptureBackend for DxgiBackend {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>> {
        self.refresh_suspension();
        if self.invalidation.is_some() {
            return Ok(None);
        }
        let mut access_lost = None;
        let acquired = self.source.acquire_into_relay(timeout, &mut access_lost);
        if let Some(reason) = access_lost {
            self.invalidation = Some(reason);
        }
        let Some((width, height)) = acquired? else {
            return Ok(None);
        };
        let relay = self.source.relay_reader()?;

        // 分辨率变化：这一帧照常交出去，下一次采集前上层 rebuild
        let desktop = self.source.desc();
        if width != desktop.ModeDesc.Width || height != desktop.ModeDesc.Height {
            eprintln!("[capture] 显示分辨率变化 → {width}x{height}，等待重建");
            self.invalidation = Some(InvalidationReason::ResolutionChanged);
        }

        let nv12 = self.convert_to_nv12(relay, width, height)?;
        self.seq += 1;
        if self.awaiting_first_frame {
            self.awaiting_first_frame = false;
            let (out_width, out_height) = output_size(self.cfg.resolution, width, height);
            eprintln!("[capture] 首帧 {width}x{height} → 编码输入 {out_width}x{out_height}");
        }
        let now_us = rc_win32core::host_time_us();
        let origin = self.cfg.origin_us.unwrap_or(0);
        let ts = now_us.saturating_sub(origin.min(now_us)) as u32;
        Ok(Some(CapturedFrame {
            texture: FrameTexture::D3D11(Interface::as_raw(&nv12)),
            timestamp: rc_types::Timestamp(ts),
            dirty: None, // v1：全帧处理
            seq: rc_types::CaptureSeq(self.seq),
            is_repeat: false,
            resource_guard: Some(Arc::new(Nv12Guard { _texture: nv12 })),
        }))
    }

    fn poll_invalidation(&mut self) -> Option<InvalidationReason> {
        self.invalidation
    }

    fn rebuild(&mut self) -> Result<()> {
        // 锁屏 / 熄屏还没解除：重建了也不出帧。返回可恢复错误，管线退避约 500ms 后重试。
        if desktop_is_unavailable() {
            self.invalidation = Some(InvalidationReason::SessionLocked);
            return Err(InvalidationReason::SessionLocked
                .to_capture_error("锁屏或显示器熄屏，采集挂起待恢复"));
        }
        let resuming = self.invalidation == Some(InvalidationReason::SessionLocked);
        self.drop_frame_resources();
        self.invalidation = None;
        if let Err(err) = self.source.rebuild() {
            // 重建失败：失效持续上报，管线按退避重试
            self.invalidation = Some(InvalidationReason::AccessLost);
            return Err(err);
        }
        let desc = self.source.desc();
        self.display.resolution = Resolution::new(desc.ModeDesc.Width, desc.ModeDesc.Height);
        self.awaiting_first_frame = true;
        if resuming {
            eprintln!("[capture] 采集恢复：duplication 已重建");
        }
        Ok(())
    }

    fn display_info(&self) -> DisplayInfo {
        self.display
    }
}

/// 诊断：实测一次 BGRA → NV12 视频处理器转换（含缩放）的 GPU 耗时，单位毫秒。
///
/// 连做 `iterations` 次后用事件查询等 GPU 做完，返回（连做时每次耗时，逐次等待时每次耗时，
/// 空载基线）。被控端帧率上不去时用它判断是不是固定功能视频处理器成了瓶颈。
pub fn conversion_gpu_ms(
    input: (u32, u32),
    output: (u32, u32),
    iterations: u32,
) -> Result<(f32, f32, f32)> {
    let shared = SharedDevice::get().map_err(Error::InvalidConfig)?;
    let device = shared.device();
    let context = shared.context();
    let video_context: ID3D11VideoContext1 = shared
        .context()
        .cast()
        .map_err(|e| Error::InvalidConfig(format!("上下文无视频接口：{e}")))?;
    let mut bgra = None;
    unsafe {
        device.CreateTexture2D(
            &D3D11_TEXTURE2D_DESC {
                Width: input.0,
                Height: input.1,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET).0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            },
            None,
            Some(&mut bgra),
        )
    }
    .map_err(|e| Error::InvalidConfig(format!("创建输入纹理失败：{e}")))?;
    let bgra = bgra.ok_or_else(|| Error::InvalidConfig("输入纹理未返回".into()))?;
    let nv12_ring = (0..NV12_RING)
        .map(|_| shared.create_nv12_texture(output.0, output.1))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Error::InvalidConfig)?;
    let frame_rate = DXGI_RATIONAL {
        Numerator: 60,
        Denominator: 1,
    };
    let converter = unsafe {
        create_converter(
            &device,
            &video_context,
            &bgra,
            &nv12_ring,
            &D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputWidth: input.0,
                InputHeight: input.1,
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: frame_rate,
                OutputFrameRate: frame_rate,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
                OutputWidth: output.0,
                OutputHeight: output.1,
            },
        )?
    };
    let mut query = None;
    unsafe {
        device.CreateQuery(
            &D3D11_QUERY_DESC {
                Query: D3D11_QUERY_EVENT,
                MiscFlags: 0,
            },
            Some(&mut query),
        )
    }
    .map_err(|e| Error::InvalidConfig(format!("创建计时查询失败：{e}")))?;
    let query: ID3D11Query = query.ok_or_else(|| Error::InvalidConfig("计时查询未返回".into()))?;
    // 事件查询在它之前的全部 GPU 工作做完之后才变为可用
    let wait_gpu = || unsafe {
        context.End(&query);
        context.Flush();
        while context.GetData(&query, None, 0, 0).is_err() {
            std::thread::sleep(Duration::from_millis(1));
        }
    };
    // 与采集后端一样轮转使用帧纹理环
    let mut ring_next = 0;
    let mut convert = || {
        let index = ring_next;
        ring_next = (index + 1) % converter.output_views.len();
        blt_to(&converter, index)
    };
    convert()?;
    wait_gpu();
    let started = std::time::Instant::now();
    for _ in 0..iterations.max(1) {
        convert()?;
    }
    wait_gpu();
    let batched_ms = started.elapsed().as_secs_f32() * 1000.0 / iterations.max(1) as f32;
    // 逐次等待：每次转换后都等 GPU 做完再开始下一次（看单次的真实时延）
    let singles = iterations.div_ceil(4).clamp(1, 30);
    let started = std::time::Instant::now();
    for _ in 0..singles {
        convert()?;
        wait_gpu();
    }
    let single_ms = started.elapsed().as_secs_f32() * 1000.0 / singles as f32;
    let started = std::time::Instant::now();
    wait_gpu();
    Ok((
        batched_ms,
        single_ms,
        started.elapsed().as_secs_f32() * 1000.0,
    ))
}

/// 在设备之间 / 设备内整张拷贝 BGRA 纹理。
fn copy_resource(src: &ID3D11Texture2D, dst: &ID3D11Texture2D) -> Result<()> {
    let src: ID3D11Resource = src
        .cast()
        .map_err(|e| Error::InvalidConfig(format!("源纹理转资源失败：{e}")))?;
    let dst: ID3D11Resource = dst
        .cast()
        .map_err(|e| Error::InvalidConfig(format!("目标纹理转资源失败：{e}")))?;
    // 拷贝走共享设备的上下文：两个纹理都在共享设备上（中转纹理的读端也是）
    unsafe {
        SharedDevice::get()
            .map_err(Error::InvalidConfig)?
            .context()
            .CopyResource(&dst, &src)
    };
    Ok(())
}

/// 把视频处理器输入视图的内容转换到输出纹理环第 `index` 张（硬件转换 + 缩放）。
fn blt_to(converter: &VideoConverter, index: usize) -> Result<()> {
    let Some(output_view) = converter.output_views.get(index) else {
        return Err(Error::InvalidConfig("NV12 帧纹理环缺失".into()));
    };
    let streams = [D3D11_VIDEO_PROCESSOR_STREAM {
        Enable: windows::Win32::Foundation::TRUE,
        OutputIndex: 0,
        InputFrameOrField: 0,
        PastFrames: 0,
        FutureFrames: 0,
        ppPastSurfaces: std::ptr::null_mut(),
        pInputSurface: std::mem::ManuallyDrop::new(Some(converter.input_view.clone())),
        ppFutureSurfaces: std::ptr::null_mut(),
        ppPastSurfacesRight: std::ptr::null_mut(),
        pInputSurfaceRight: std::mem::ManuallyDrop::new(None),
        ppFutureSurfacesRight: std::ptr::null_mut(),
    }];
    unsafe {
        converter
            .context
            .VideoProcessorBlt(&converter.processor, output_view, 0, &streams)
    }
    .map_err(|e| Error::InvalidConfig(format!("色彩转换失败：{e}")))
}

/// 在 `device` 上建一张 BGRA 纹理（`misc_flags`：跨设备共享时传 `D3D11_RESOURCE_MISC_SHARED`）。
fn create_bgra_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    misc_flags: u32,
) -> Result<ID3D11Texture2D> {
    let mut texture = None;
    unsafe {
        device.CreateTexture2D(
            &D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET).0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: misc_flags,
            },
            None,
            Some(&mut texture),
        )
    }
    .map_err(|e| Error::InvalidConfig(format!("创建 BGRA 纹理失败：{e}")))?;
    texture.ok_or_else(|| Error::InvalidConfig("BGRA 纹理未返回".into()))
}

/// 诊断：验证跨设备中转链路（采集专用设备 → 共享纹理 → 共享设备 → NV12）。
///
/// 在采集专用设备上写一张纯灰 BGRA 纹理，走与真实帧完全相同的路径搬到共享设备并转换成
/// NV12，读回中心像素，返回（亮度，U）。不依赖桌面内容，锁屏 / 熄屏时也能跑。
pub fn relay_self_check() -> std::result::Result<(u8, u8), String> {
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_USAGE_STAGING,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;

    const WIDTH: u32 = 320;
    const HEIGHT: u32 = 180;
    const GRAY: u8 = 128;

    let mut source = DuplicationSource::new(0).map_err(|e| e.to_string())?;
    let src = create_bgra_texture(&source.device, WIDTH, HEIGHT, 0).map_err(|e| e.to_string())?;
    let pixels: Vec<u8> = [GRAY, GRAY, GRAY, 255u8]
        .iter()
        .copied()
        .cycle()
        .take((WIDTH * HEIGHT * 4) as usize)
        .collect();
    let src_resource: ID3D11Resource = src.cast().map_err(|e| e.to_string())?;
    unsafe {
        source.context.UpdateSubresource(
            &src_resource,
            0,
            None,
            pixels.as_ptr().cast(),
            WIDTH * 4,
            0,
        )
    };
    source
        .ensure_relay(WIDTH, HEIGHT)
        .map_err(|e| e.to_string())?;
    let writer = source
        .relay
        .as_ref()
        .ok_or("中转纹理尚未创建")?
        .writer
        .clone();
    // 与真实帧一样：在采集专用设备上拷进中转纹理，等它写完，再交给共享设备
    let writer_resource: ID3D11Resource = writer.cast().map_err(|e| e.to_string())?;
    unsafe { source.context.CopyResource(&writer_resource, &src_resource) };
    source.wait_gpu();
    let reader = source.relay_reader().map_err(|e| e.to_string())?;

    let shared = SharedDevice::get().map_err(|e| e.to_string())?;
    let device = shared.device();
    let context = shared.context();
    let bgra = create_bgra_texture(&device, WIDTH, HEIGHT, 0).map_err(|e| e.to_string())?;
    let reader_resource: ID3D11Resource = reader.cast().map_err(|e| e.to_string())?;
    let bgra_resource: ID3D11Resource = bgra.cast().map_err(|e| e.to_string())?;
    unsafe { context.CopyResource(&bgra_resource, &reader_resource) };
    let nv12 = shared
        .create_nv12_texture(WIDTH, HEIGHT)
        .map_err(|e| e.to_string())?;
    let video_context: ID3D11VideoContext1 = shared
        .context()
        .cast()
        .map_err(|e| format!("上下文无视频接口：{e}"))?;
    let frame_rate = DXGI_RATIONAL {
        Numerator: 60,
        Denominator: 1,
    };
    let converter = unsafe {
        create_converter(
            &device,
            &video_context,
            &bgra,
            std::slice::from_ref(&nv12),
            &D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputWidth: WIDTH,
                InputHeight: HEIGHT,
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: frame_rate,
                OutputFrameRate: frame_rate,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
                OutputWidth: WIDTH,
                OutputHeight: HEIGHT,
            },
        )
    }
    .map_err(|e| e.to_string())?;
    blt_to(&converter, 0).map_err(|e| e.to_string())?;

    // 读回：NV12 → 暂存纹理 → CPU
    let mut staging = None;
    unsafe {
        device.CreateTexture2D(
            &D3D11_TEXTURE2D_DESC {
                Width: WIDTH,
                Height: HEIGHT,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            },
            None,
            Some(&mut staging),
        )
    }
    .map_err(|e| format!("创建读回暂存纹理失败：{e}"))?;
    let staging = staging.ok_or("读回暂存纹理未返回")?;
    let nv12_resource: ID3D11Resource = nv12.cast().map_err(|e| e.to_string())?;
    let staging_resource: ID3D11Resource = staging.cast().map_err(|e| e.to_string())?;
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    let sample = unsafe {
        context.CopyResource(&staging_resource, &nv12_resource);
        context
            .Map(&staging_resource, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .map_err(|e| format!("读回映射失败：{e}"))?;
        let base = mapped.pData as *const u8;
        let pitch = mapped.RowPitch as usize;
        let luma = *base.add((HEIGHT / 2) as usize * pitch + (WIDTH / 2) as usize);
        let chroma = *base
            .add(HEIGHT as usize * pitch + (HEIGHT / 4) as usize * pitch + (WIDTH / 2) as usize);
        context.Unmap(&staging_resource, 0);
        (luma, chroma)
    };
    Ok(sample)
}

/// 主屏逻辑分辨率（当前实现直接返回物理分辨率；HiDPI feature 按需启用）。
pub fn main_display_logical_resolution() -> Option<Resolution> {
    // HiDPI feature 未启用：直接取物理分辨率（scale 由调用方按需换算）
    main_display_backing_resolution()
}

/// 主屏物理分辨率。
pub fn main_display_backing_resolution() -> Option<Resolution> {
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
    let w = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let h = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    (w > 0 && h > 0).then(|| Resolution::new(w as u32, h as u32))
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

/// 枚举主屏可用显示模式（`EnumDisplaySettingsW`）。
pub fn list_main_display_modes() -> Vec<DisplayModePreset> {
    use windows::core::PCWSTR;
    use windows::Win32::Graphics::Gdi::{EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS};
    let mut modes = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut dm = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    let device_name: Vec<u16> = "\\\\.\\DISPLAY1"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        for mode in [
            ENUM_CURRENT_SETTINGS,
            windows::Win32::Graphics::Gdi::ENUM_REGISTRY_SETTINGS,
        ] {
            if EnumDisplaySettingsW(PCWSTR(device_name.as_ptr()), mode, &mut dm).as_bool() {
                let key = (dm.dmPelsWidth, dm.dmPelsHeight, dm.dmBitsPerPel);
                if seen.insert(key) {
                    modes.push(DisplayModePreset {
                        width: dm.dmPelsWidth,
                        height: dm.dmPelsHeight,
                        is_hidpi: false,
                    });
                }
            }
        }
    }
    modes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiated_resolution_is_clamped_to_the_desktop_and_even() {
        let desktop = (3840, 2160);
        assert_eq!(output_size(None, 3840, 2160), desktop);
        assert_eq!(
            output_size(Some(Resolution::new(1920, 1080)), 3840, 2160),
            (1920, 1080)
        );
        // 协商分辨率比桌面还大：不放大，输出桌面原尺寸
        assert_eq!(
            output_size(Some(Resolution::new(4096, 2160)), 3840, 2160),
            desktop
        );
        // 奇数尺寸取偶
        assert_eq!(
            output_size(Some(Resolution::new(1919, 1079)), 3840, 2160),
            (1918, 1078)
        );
    }
}
