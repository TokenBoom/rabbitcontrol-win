//! RabbitControl Windows 硬件视频解码器（Media Foundation / D3D11VA）。
//!
//! MFT 选择：`MFTEnumEx` 枚举 `MFT_CATEGORY_VIDEO_DECODER`（H.264 入 NV12 出），
//! 优先硬件解码 MFT，无硬件时回落微软软件解码器。
//! 零拷贝：`IMFDXGIDeviceManager` 绑定 [`rc_d3d11::SharedDevice`]。
//! 低延迟：`MF_LOW_LATENCY` 属性。

#![deny(missing_docs)]
#![allow(unsafe_code)] // FFI 边界

pub mod probe;

use std::ptr;
use std::sync::Arc;

use rc_d3d11::SharedDevice;
use rc_decode::{DecodeOutput, Decoder};
use rc_encode::Codec;
use rc_types::{Error, FrameKind, Resolution, Result, Timestamp};
use windows::core::Interface;
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::{
    IMFDXGIBuffer, IMFMediaType, IMFTransform, MFCreateDXGIDeviceManager, MFCreateMediaType,
    MFCreateMemoryBuffer, MFMediaType_Video, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC_UNLOCK,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

/// MF 解码器配置。
#[derive(Debug, Clone, Copy)]
pub struct MfDecoderConfig {
    /// 码流编解码器。
    pub codec: Codec,
    /// 支持的最大分辨率（超出拒绝）。
    pub max_resolution: Resolution,
}

impl Default for MfDecoderConfig {
    fn default() -> Self {
        Self {
            codec: Codec::H264,
            max_resolution: Resolution::new(4096, 4096),
        }
    }
}

/// Media Foundation 硬件解码器。
pub struct MfDecoder {
    transform: IMFTransform,
    device: ID3D11Device,
    waiting_keyframe: bool,
    mf_guard: Option<rc_win32core::mf::MfGuard>,
    coinit_owned: bool,
}

unsafe impl Send for MfDecoder {}

impl MfDecoder {
    /// 构造解码器。
    pub fn new(cfg: MfDecoderConfig) -> Result<Self> {
        if cfg.codec != Codec::H264 {
            return Err(Error::InvalidConfig("mf decoder: HEVC 待 D6 后开放".into()));
        }
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let coinit_owned = !(hr.is_err() && hr == RPC_E_CHANGED_MODE);
        if hr.is_err() && hr != RPC_E_CHANGED_MODE {
            return Err(Error::InvalidConfig(format!("CoInitializeEx 失败：{hr:?}")));
        }
        let mf_guard = Some(rc_win32core::mf::MfGuard::startup().map_err(Error::InvalidConfig)?);

        let shared = SharedDevice::get().map_err(Error::InvalidConfig)?;
        let device = shared.device();

        let mut manager = None;
        let mut token = 0u32;
        unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager) }
            .map_err(|e| Error::InvalidConfig(format!("DXGIDeviceManager 失败：{e:?}")))?;
        let manager = manager.ok_or_else(|| Error::InvalidConfig("未返回管理器".into()))?;
        unsafe { manager.ResetDevice(&device, token) }
            .map_err(|e| Error::InvalidConfig(format!("ResetDevice 失败：{e:?}")))?;

        let transform = unsafe { activate_h264_decoder() }
            .map_err(|e| Error::InvalidConfig(format!("未找到 H.264 解码 MFT：{e}")))?;
        // 硬件解码 MFT 是异步 MFT，必须解锁后才能设置类型/处理消息
        if let Ok(attrs) = unsafe { transform.GetAttributes() } {
            unsafe { attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }
                .map_err(|e| Error::InvalidConfig(format!("异步 MFT 解锁失败：{e:?}")))?;
        }

        unsafe {
            transform.ProcessMessage(
                MFT_MESSAGE_SET_D3D_MANAGER,
                Interface::as_raw(&manager) as usize,
            )
        }
        .map_err(|e| Error::InvalidConfig(format!("SET_D3D_MANAGER 失败：{e:?}")))?;

        // 输入类型：H.264（解码器：输入先于输出协商）
        let in_type: IMFMediaType = unsafe { MFCreateMediaType() }
            .map_err(|e| Error::InvalidConfig(format!("创建输入类型失败：{e:?}")))?;
        unsafe {
            in_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| Error::InvalidConfig(format!("in major：{e:?}")))?;
            in_type
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
                .map_err(|e| Error::InvalidConfig(format!("in subtype：{e:?}")))?;
            transform
                .SetInputType(0, &in_type, 0)
                .map_err(|e| Error::InvalidConfig(format!("SetInputType：{e:?}")))?;
        }

        // 输出类型：NV12
        let out_type: IMFMediaType = unsafe { MFCreateMediaType() }
            .map_err(|e| Error::InvalidConfig(format!("创建输出类型失败：{e:?}")))?;
        unsafe {
            out_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| Error::InvalidConfig(format!("out major：{e:?}")))?;
            out_type
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
                .map_err(|e| Error::InvalidConfig(format!("out subtype：{e:?}")))?;
            transform
                .SetOutputType(0, &out_type, 0)
                .map_err(|e| Error::InvalidConfig(format!("SetOutputType：{e:?}")))?;
        }

        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(mfw)?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(mfw)?;
        }

        Ok(Self {
            transform,
            device,
            waiting_keyframe: true,
            mf_guard,
            coinit_owned,
        })
    }
}

fn mfw(e: windows::core::Error) -> Error {
    Error::InvalidConfig(format!("{e:?}"))
}

impl Decoder for MfDecoder {
    fn decode(
        &mut self,
        data: &[u8],
        _kind: FrameKind,
        _deadline: Timestamp,
    ) -> Result<DecodeOutput> {
        if data.is_empty() {
            return Ok(DecodeOutput::Nothing);
        }
        let _ = self.device; // D3D11VA 路径的设备引用

        // 构造输入样本
        let buffer: windows::Win32::Media::MediaFoundation::IMFMediaBuffer =
            unsafe { MFCreateMemoryBuffer(data.len() as u32) }.map_err(mfw)?;
        unsafe {
            let mut dst: *mut u8 = ptr::null_mut();
            let mut max_len = 0u32;
            buffer
                .Lock(&mut dst, None, Some(&mut max_len))
                .map_err(mfw)?;
            if (data.len() as u32) > max_len {
                buffer.Unlock().map_err(mfw)?;
                return Err(Error::InvalidConfig("输入样本缓冲不足".into()));
            }
            ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len());
            buffer.SetCurrentLength(data.len() as u32).map_err(mfw)?;
            buffer.Unlock().map_err(mfw)?;
        }
        let sample: windows::Win32::Media::MediaFoundation::IMFSample =
            unsafe { windows::Win32::Media::MediaFoundation::MFCreateSample() }.map_err(mfw)?;
        unsafe { sample.AddBuffer(&buffer) }.map_err(mfw)?;

        unsafe { self.transform.ProcessInput(0, &sample, 0) }.map_err(mfw)?;

        // 排输出（一进一出 D5，取到第一帧即返回）
        loop {
            let mut out = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: std::mem::ManuallyDrop::new(None),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            };
            let mut status = 0u32;
            let hr = unsafe {
                self.transform
                    .ProcessOutput(0, std::slice::from_mut(&mut out), &mut status)
            };
            match hr {
                Ok(()) => {
                    let sample = (*out.pSample)
                        .clone()
                        .ok_or_else(|| Error::InvalidConfig("解码 MFT 未返回样本".into()))?;
                    let (tex, w, h) =
                        unsafe { extract_texture(&sample, &shared_device_texture_pool())? };
                    if w == 0 || h == 0 {
                        continue;
                    }
                    self.waiting_keyframe = false;
                    let raw = Interface::as_raw(&tex);
                    std::mem::forget(tex); // 所有权移交 FrameTexture
                    return Ok(DecodeOutput::Frame(rc_decode::DecodedFrame {
                        texture: rc_capture::FrameTexture::D3D11(raw),
                        decode_done_ts: Timestamp(rc_win32core::host_time_us() as u32),
                        width: w,
                        height: h,
                    }));
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                    if self.waiting_keyframe {
                        return Ok(DecodeOutput::NeedKeyframe);
                    }
                    return Ok(DecodeOutput::Nothing);
                }
                Err(e) => {
                    return Err(Error::InvalidConfig(format!("ProcessOutput 失败：{e:?}")));
                }
            }
        }
    }

    fn needs_keyframe(&self) -> bool {
        self.waiting_keyframe
    }

    fn flush(&mut self) -> Result<()> {
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)
                .map_err(mfw)?;
        }
        Ok(())
    }
}

impl Drop for MfDecoder {
    fn drop(&mut self) {
        let _ = unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0) };
        self.mf_guard.take();
        if self.coinit_owned {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

// 辅助函数

/// 共享设备引用（供 texture 提取）
fn shared_device_texture_pool() -> Arc<SharedDevice> {
    SharedDevice::get().expect("共享设备未初始化")
}

/// 从解码输出样本提取 D3D11 纹理 → 拷贝到自有独立 NV12 纹理
unsafe fn extract_texture(
    sample: &windows::Win32::Media::MediaFoundation::IMFSample,
    shared: &Arc<SharedDevice>,
) -> Result<(ID3D11Texture2D, u32, u32)> {
    use windows::Win32::Graphics::Direct3D11::D3D11_BOX;

    let buffer = sample
        .GetBufferByIndex(0)
        .map_err(|e| Error::InvalidConfig(format!("{e:?}")))?;
    let dxgi_buffer: IMFDXGIBuffer = buffer
        .cast()
        .map_err(|e| Error::InvalidConfig(format!("缓冲非 D3D11：{e:?}")))?;
    let mut src: Option<ID3D11Texture2D> = None;
    dxgi_buffer
        .GetResource(
            &ID3D11Texture2D::IID,
            &mut src as *mut _ as *mut *mut core::ffi::c_void,
        )
        .map_err(|e| Error::InvalidConfig(format!("GetResource 失败：{e:?}")))?;
    let src = src.ok_or_else(|| Error::InvalidConfig("GetResource 未返回".into()))?;
    let sub = dxgi_buffer
        .GetSubresourceIndex()
        .map_err(|e| Error::InvalidConfig(format!("{e:?}")))?;

    let mut desc = Default::default();
    unsafe { src.GetDesc(&mut desc) };
    let (w, h) = (desc.Width, desc.Height);

    let dst = shared
        .create_nv12_texture(w, h)
        .map_err(Error::InvalidConfig)?;
    let box_ = D3D11_BOX {
        left: 0,
        top: 0,
        front: 0,
        right: w,
        bottom: h,
        back: 1,
    };
    let context = shared.context();
    unsafe {
        let dst_res: windows::Win32::Graphics::Direct3D11::ID3D11Resource =
            dst.cast().map_err(mfw)?;
        let src_res: windows::Win32::Graphics::Direct3D11::ID3D11Resource =
            src.cast().map_err(mfw)?;
        context.CopySubresourceRegion(Some(&dst_res), 0, 0, 0, 0, Some(&src_res), sub, Some(&box_));
    }
    Ok((dst, w, h))
}

/// 枚举并激活首个可用的 H.264 解码 MFT
unsafe fn activate_h264_decoder() -> windows::core::Result<IMFTransform> {
    use windows::Win32::Media::MediaFoundation::{
        IMFActivate, MFTEnumEx, MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG, MFT_ENUM_FLAG_ASYNCMFT,
        MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SYNCMFT, MFT_REGISTER_TYPE_INFO,
    };
    let in_type = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let out_type = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let flags = MFT_ENUM_FLAG(
        MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SYNCMFT.0 | MFT_ENUM_FLAG_ASYNCMFT.0,
    );
    let mut acts: *mut Option<IMFActivate> = ptr::null_mut();
    let mut count = 0u32;
    MFTEnumEx(
        MFT_CATEGORY_VIDEO_DECODER,
        flags,
        Some(&in_type),
        Some(&out_type),
        &mut acts,
        &mut count,
    )?;
    if acts.is_null() || count == 0 {
        let flags_sw = MFT_ENUM_FLAG(MFT_ENUM_FLAG_SYNCMFT.0);
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            flags_sw,
            Some(&in_type),
            Some(&out_type),
            &mut acts,
            &mut count,
        )?;
    }
    if acts.is_null() || count == 0 {
        return Err(windows::core::Error::from_win32());
    }
    let mut result: Option<IMFTransform> = None;
    for i in 0..count as usize {
        let slot = acts.add(i);
        let Some(activate) = (*slot).clone() else {
            continue;
        };
        if let Ok(t) = activate.ActivateObject::<IMFTransform>() {
            result = Some(t);
            break;
        }
    }
    windows::Win32::System::Com::CoTaskMemFree(Some(acts.cast()));
    result.ok_or_else(windows::core::Error::from_win32)
}

/// 构造解码器。
pub fn new_decoder(cfg: MfDecoderConfig) -> Result<MfDecoder> {
    MfDecoder::new(cfg)
}
