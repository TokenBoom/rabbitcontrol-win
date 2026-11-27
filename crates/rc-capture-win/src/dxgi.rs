//! DXGI Desktop Duplication 采集后端实装（M5a.2）。
//!
//! # 管线
//!
//! ```text
//! IDXGIOutput1::DuplicateOutput(共享设备)
//!   → AcquireNextFrame(≤16ms)                     （无变化 → Ok(None)，铁律 2）
//!   → CopyResource 到自有 BGRA 纹理               （duplication 帧必须立即释放）
//!   → ReleaseFrame
//!   → VideoProcessorBlt BGRA → NV12               （共享设备，编码器零拷贝输入）
//!   → CapturedFrame { texture: FrameTexture::D3D11 }
//! ```
//!
//! # 已知限制（v1）
//!
//! - 仅主适配器（SharedDevice 未绑定多适配器，多 GPU 多显示器场景未覆盖）；
//! - 屏幕旋转未做坐标补偿；
//! - 光标不并入帧内容（鼠标-only 更新按无内容帧丢弃）；
//! - 脏区上报为全帧（`dirty: None`），上层按全帧处理。

#![allow(unsafe_code)] // DXGI / D3D11 FFI 边界

use std::sync::Arc;
use std::time::Duration;

use rc_capture::{CaptureBackend, CapturedFrame, DisplayInfo, FrameTexture, InvalidationReason};
use rc_d3d11::SharedDevice;
use rc_types::{DisplayId, Error, Resolution, Result};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor,
    ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::DXGI_ERROR_ACCESS_LOST;
use windows::Win32::Graphics::Dxgi::{IDXGIOutput1, DXGI_OUTDUPL_DESC};

use crate::DxgiConfig;

/// 重复帧判定阈值：AccumulatedFrames == 0 视为无内容更新。
const NO_CONTENT_FRAMES: u32 = 0;

/// BGRA → NV12 视频处理器组（按输入尺寸缓存，尺寸变化重建）。
struct VideoConverter {
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enum_: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
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
    device: ID3D11Device,
    output: IDXGIOutput1,
    duplication_desc: DXGI_OUTDUPL_DESC,
    duplication: Option<windows::Win32::Graphics::Dxgi::IDXGIOutputDuplication>,
    bgra_copy: Option<ID3D11Texture2D>,
    nv12: Option<ID3D11Texture2D>,
    converter: Option<VideoConverter>,
    stale: Option<InvalidationReason>,
}

impl DxgiBackend {
    /// 枚举目标输出并创建 duplication。
    pub fn new(cfg: DxgiConfig) -> Result<Self> {
        let shared = SharedDevice::get().map_err(Error::InvalidConfig)?;
        let device = shared.device();
        let output = unsafe { open_output(cfg.display) }?;
        let duplicated = unsafe { output.DuplicateOutput(&device) }
            .map_err(|e| Error::InvalidConfig(format!("DuplicateOutput 失败：{e}")))?;
        let desc = unsafe { duplicated.GetDesc() };
        let display = DisplayInfo {
            id: DisplayId(cfg.display),
            resolution: Resolution::new(desc.ModeDesc.Width, desc.ModeDesc.Height),
            scale: 1.0,
            hdr: false,
        };
        Ok(Self {
            cfg,
            display,
            seq: 0,
            device,
            output,
            duplication_desc: desc,
            duplication: Some(duplicated),
            bgra_copy: None,
            nv12: None,
            converter: None,
            stale: None,
        })
    }

    /// 确保 BGRA 拷贝纹理 / NV12 输出纹理 / 转换器与当前帧尺寸一致。
    fn ensure_pools(&mut self, width: u32, height: u32) -> Result<()> {
        let size_ok = self
            .bgra_copy
            .as_ref()
            .map(|t| {
                let mut desc =
                    windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC::default();
                unsafe { t.GetDesc(&mut desc) };
                (desc.Width, desc.Height)
            })
            .is_some_and(|(w, h)| w == width && h == height);
        if size_ok {
            return Ok(());
        }
        let shared = SharedDevice::get().map_err(Error::InvalidConfig)?;
        let bgra_desc = windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: windows::Win32::Graphics::Direct3D11::D3D11_USAGE_DEFAULT,
            BindFlags: (windows::Win32::Graphics::Direct3D11::D3D11_BIND_SHADER_RESOURCE
                | windows::Win32::Graphics::Direct3D11::D3D11_BIND_RENDER_TARGET)
                .0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut bgra = None;
        unsafe {
            self.device
                .CreateTexture2D(&bgra_desc, None, Some(&mut bgra))
        }
        .map_err(|e| Error::InvalidConfig(format!("创建 BGRA 拷贝纹理失败：{e}")))?;
        self.bgra_copy = bgra;

        let nv12 = shared
            .create_nv12_texture(width, height)
            .map_err(Error::InvalidConfig)?;
        self.nv12 = Some(nv12);

        let video_device: ID3D11VideoDevice = self
            .device
            .cast()
            .map_err(|e| Error::InvalidConfig(format!("设备无视频接口：{e}")))?;
        let video_context: ID3D11VideoContext = shared
            .context()
            .cast()
            .map_err(|e| Error::InvalidConfig(format!("上下文无视频接口：{e}")))?;
        let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputWidth: width,
            InputHeight: height,
            InputFrameFormat:
                windows::Win32::Graphics::Direct3D11::D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: windows::Win32::Graphics::Dxgi::Common::DXGI_RATIONAL {
                Numerator: self.cfg.fps.max(1),
                Denominator: 1,
            },
            OutputFrameRate: windows::Win32::Graphics::Dxgi::Common::DXGI_RATIONAL {
                Numerator: self.cfg.fps.max(1),
                Denominator: 1,
            },
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            OutputWidth: width,
            OutputHeight: height,
        };
        let enum_: ID3D11VideoProcessorEnumerator =
            unsafe { video_device.CreateVideoProcessorEnumerator(&content_desc) }
                .map_err(|e| Error::InvalidConfig(format!("创建视频处理器枚举失败：{e}")))?;
        let processor = unsafe { video_device.CreateVideoProcessor(&enum_, 0) }
            .map_err(|e| Error::InvalidConfig(format!("创建视频处理器失败：{e}")))?;
        self.converter = Some(VideoConverter {
            video_device,
            video_context,
            enum_,
            processor,
        });
        Ok(())
    }

    /// BGRA 拷贝纹理 → NV12（VideoProcessorBlt 硬件转换）。
    fn convert_to_nv12(&mut self, width: u32, height: u32) -> Result<ID3D11Texture2D> {
        self.ensure_pools(width, height)?;
        let bgra = self
            .bgra_copy
            .as_ref()
            .ok_or_else(|| Error::InvalidConfig("BGRA 拷贝纹理缺失".into()))?;
        let nv12 = self
            .nv12
            .as_ref()
            .ok_or_else(|| Error::InvalidConfig("NV12 纹理缺失".into()))?
            .clone();
        let Some(converter) = self.converter.as_ref() else {
            return Err(Error::InvalidConfig("视频处理器缺失".into()));
        };
        unsafe {
            let mut input_view_slot: Option<ID3D11VideoProcessorInputView> = None;
            converter
                .video_device
                .CreateVideoProcessorInputView(
                    bgra,
                    &converter.enum_,
                    &D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                        FourCC: 0,
                        ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                        Anonymous: windows::Win32::Graphics::Direct3D11::D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                            Texture2D: windows::Win32::Graphics::Direct3D11::D3D11_TEX2D_VPIV {
                                MipSlice: 0,
                                ArraySlice: 0,
                            },
                        },
                    },
                    Some(&mut input_view_slot),
                )
                .map_err(|e| Error::InvalidConfig(format!("输入视图创建失败：{e}")))?;
            let Some(input_view) = input_view_slot else {
                return Err(Error::InvalidConfig("输入视图未返回".into()));
            };
            let mut output_view_slot: Option<ID3D11VideoProcessorOutputView> = None;
            converter
                .video_device
                .CreateVideoProcessorOutputView(
                    &nv12,
                    &converter.enum_,
                    &D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                        ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                        Anonymous: windows::Win32::Graphics::Direct3D11::D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                            Texture2D: windows::Win32::Graphics::Direct3D11::D3D11_TEX2D_VPOV { MipSlice: 0 },
                        },
                    },
                    Some(&mut output_view_slot),
                )
                .map_err(|e| Error::InvalidConfig(format!("输出视图创建失败：{e}")))?;
            let Some(output_view) = output_view_slot else {
                return Err(Error::InvalidConfig("输出视图未返回".into()));
            };
            let streams = [D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: windows::Win32::Foundation::TRUE,
                OutputIndex: 0,
                InputFrameOrField: 0,
                PastFrames: 0,
                FutureFrames: 0,
                ppPastSurfaces: std::ptr::null_mut(),
                pInputSurface: std::mem::ManuallyDrop::new(Some(input_view)),
                ppFutureSurfaces: std::ptr::null_mut(),
                ppPastSurfacesRight: std::ptr::null_mut(),
                pInputSurfaceRight: std::mem::ManuallyDrop::new(None),
                ppFutureSurfacesRight: std::ptr::null_mut(),
            }];
            converter
                .video_context
                .VideoProcessorBlt(&converter.processor, &output_view, 0, &streams)
                .map_err(|e| Error::InvalidConfig(format!("色彩转换失败：{e}")))?;
        }
        Ok(nv12)
    }
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
        if self.stale.is_some() {
            return Ok(None);
        }
        // clone COM 引用，避免持借用跨 &mut self 方法调用
        let Some(duplication) = self.duplication.clone() else {
            return Ok(None);
        };
        let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        let mut info = windows::Win32::Graphics::Dxgi::DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<windows::Win32::Graphics::Dxgi::IDXGIResource> = None;
        let acquired =
            unsafe { duplication.AcquireNextFrame(timeout_ms, &mut info, &mut resource) };
        if let Err(err) = &acquired {
            let code = err.code();
            let timeout_hit = code == windows::Win32::Graphics::Dxgi::DXGI_ERROR_WAIT_TIMEOUT;
            let lost = code == DXGI_ERROR_ACCESS_LOST;
            if timeout_hit {
                return Ok(None);
            }
            if lost {
                self.stale = Some(InvalidationReason::AccessLost);
                return Ok(None);
            }
            return Err(Error::Capture {
                recoverable: true,
                msg: format!("AcquireNextFrame 失败：{err}"),
            });
        }
        let _ = acquired;

        // 铁律 2：无内容更新（鼠标-only）不产帧
        if info.AccumulatedFrames == NO_CONTENT_FRAMES {
            let _ = unsafe { duplication.ReleaseFrame() };
            return Ok(None);
        }

        let Some(resource) = resource else {
            let _ = unsafe { duplication.ReleaseFrame() };
            return Ok(None);
        };
        let desktop: ID3D11Texture2D = resource.cast().map_err(|e| Error::Capture {
            recoverable: true,
            msg: format!("桌面帧非纹理：{e}"),
        })?;
        let mut desktop_desc =
            windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC::default();
        unsafe { desktop.GetDesc(&mut desktop_desc) };
        let width = desktop_desc.Width;
        let height = desktop_desc.Height;

        let context = SharedDevice::get().map_err(Error::InvalidConfig)?.context();
        let copy = self.bgra_copy.as_ref();
        let need_pool = copy.is_none();
        if need_pool {
            let _ = self.ensure_pools(width, height);
        }
        let Some(bgra) = self.bgra_copy.as_ref() else {
            let _ = unsafe { duplication.ReleaseFrame() };
            return Err(Error::Capture {
                recoverable: true,
                msg: "BGRA 拷贝纹理不可用".into(),
            });
        };
        unsafe {
            let src: windows::Win32::Graphics::Direct3D11::ID3D11Resource =
                desktop.cast().map_err(|e| Error::Capture {
                    recoverable: true,
                    msg: format!("桌面帧转资源失败：{e}"),
                })?;
            let dst: windows::Win32::Graphics::Direct3D11::ID3D11Resource =
                bgra.cast().map_err(|e| Error::Capture {
                    recoverable: true,
                    msg: format!("拷贝纹理转资源失败：{e}"),
                })?;
            context.CopyResource(&dst, &src);
        }
        // duplication 帧必须立即释放（契约：句柄仅到下一次采集）
        let _ = unsafe { duplication.ReleaseFrame() };

        // 分辨率变化：标记失效（下一帧前上层 rebuild）
        if width != self.duplication_desc.ModeDesc.Width
            || height != self.duplication_desc.ModeDesc.Height
        {
            self.stale = Some(InvalidationReason::ResolutionChanged);
        }

        let nv12 = self.convert_to_nv12(width, height)?;
        let raw = windows::core::Interface::as_raw(&nv12);
        self.seq += 1;
        let now_us = rc_win32core::host_time_us();
        let origin = self.cfg.origin_us.unwrap_or(0);
        let ts = now_us.saturating_sub(origin.min(now_us)) as u32;
        Ok(Some(CapturedFrame {
            texture: FrameTexture::D3D11(raw),
            timestamp: rc_types::Timestamp(ts),
            dirty: None, // v1：全帧处理
            seq: rc_types::CaptureSeq(self.seq),
            is_repeat: false,
            resource_guard: Some(Arc::new(Nv12Guard { _texture: nv12 })),
        }))
    }

    fn poll_invalidation(&mut self) -> Option<InvalidationReason> {
        self.stale.take()
    }

    fn rebuild(&mut self) -> Result<()> {
        // 幂等（契约 C5）：丢弃旧资源后重新 DuplicateOutput
        self.duplication = None;
        self.bgra_copy = None;
        self.nv12 = None;
        self.converter = None;
        self.stale = None;
        let duplicated = unsafe { self.output.DuplicateOutput(&self.device) }
            .map_err(|e| Error::InvalidConfig(format!("重建 duplication 失败：{e}")))?;
        let desc = unsafe { duplicated.GetDesc() };
        self.display.resolution = Resolution::new(desc.ModeDesc.Width, desc.ModeDesc.Height);
        self.duplication_desc = desc;
        self.duplication = Some(duplicated);
        Ok(())
    }

    fn display_info(&self) -> DisplayInfo {
        self.display
    }
}

/// 主屏逻辑分辨率（当前实现直接返回物理分辨率；HiDpi feature 按需启用）。
pub fn main_display_logical_resolution() -> Option<Resolution> {
    // HiDpi feature 未启用：直接取物理分辨率（scale 由调用方按需换算）
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
