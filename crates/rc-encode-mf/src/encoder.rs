//! Media Foundation H.264 硬件编码器实装（M5a.3）。
//!
//! # 异步 MFT 事件驱动
//!
//! 硬件编码 MFT 全部是异步 MFT：需要先 `MF_TRANSFORM_ASYNC_UNLOCK`，
//! 然后按事件序驱动——`METransformNeedInput` → `ProcessInput` →
//! `METransformHaveOutput` → `ProcessOutput`。事件队列以
//! `GetEvent(NO_WAIT)` + 1ms 轮询消费，截止时间（E5）由调用方时间戳
//! 换算为轮询上限。
//!
//! # 零拷贝输入
//!
//! `MFT_MESSAGE_SET_D3D_MANAGER` 绑定共享设备；采集交来的 NV12 纹理经
//! `MFCreateDXGISurfaceBuffer` 包装为 IMFMediaBuffer，编码器直接在 GPU
//! 上取用（不回读内存）。
//!
//! # 输出
//!
//! 硬件 H.264 编码器输出 Annex-B 起始码流（`MF_MT_MPEG2`? 不——由
//! 编码器字节流自带 00 00 00 01 起始码，capabilities 声明 AnnexB）。

#![allow(unsafe_code)] // MF / D3D11 FFI 边界

use std::time::Duration;

use rc_capture::CapturedFrame;
use rc_encode::{BitstreamFormat, Codec, EncodedFrame, Encoder, EncoderCaps, EncoderConfig};
use rc_types::{Error, FrameKind, Resolution, Result, Timestamp};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::{
    eAVEncCommonRateControlMode_CBR, CODECAPI_AVEncCommonMeanBitRate,
    CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncVideoForceKeyFrame, ICodecAPI,
    IMFMediaEventGenerator, IMFTransform, METransformHaveOutput, METransformNeedInput,
    MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateSample, MFMediaType_Video,
    MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
    MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER, MF_EVENT_FLAG_NO_WAIT,
    MF_E_NO_EVENTS_AVAILABLE, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC_UNLOCK,
};

use rc_win32core::mf::MfGuard;

/// 事件轮询间隔（毫秒）。
const EVENT_POLL_MS: u32 = 1;

/// Media Foundation 硬件编码器。
pub struct MfEncoder {
    cfg: EncoderConfig,
    idr_pending: bool,
    transform: IMFTransform,
    event_gen: IMFMediaEventGenerator,
    codec: ICodecAPI,
    _mf_guard: MfGuard,
    _coinit: rc_win32core::ComApartment,
    /// 输入样本时间基（100ns 单位递增，保证单调）。
    sample_time_100ns: i64,
    /// 上一帧时长（100ns），按 fps 推导。
    frame_duration_100ns: i64,
}

impl MfEncoder {
    /// 构造编码器。违反延迟红线（E1/E2）的配置在构造期被拒绝。
    pub fn new(cfg: EncoderConfig) -> Result<Self> {
        cfg.validate()?;
        let coinit = rc_win32core::ComApartment::init(rc_win32core::ApartmentModel::MultiThreaded)
            .map_err(Error::InvalidConfig)?;
        let mf_guard = MfGuard::startup().map_err(Error::InvalidConfig)?;

        let transform = activate_encoder(&cfg)?;
        // 异步 MFT 必须显式解锁
        let attrs = unsafe {
            transform
                .GetAttributes()
                .map_err(mf_err("获取 Transform 属性失败"))?
        };
        unsafe {
            attrs
                .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                .map_err(mf_err("异步 MFT 解锁失败"))
        }
        .map_err(|e| Error::InvalidConfig(format!("异步 MFT 解锁失败：{e}")))?;

        // D3D 管理器：共享设备（与采集/解码同设备，零拷贝前提）
        let shared = rc_d3d11::SharedDevice::get().map_err(Error::InvalidConfig)?;
        let mut manager = None;
        let mut token = 0u32;
        unsafe {
            windows::Win32::Media::MediaFoundation::MFCreateDXGIDeviceManager(
                &mut token,
                &mut manager,
            )
        }
        .map_err(mf_err("创建 D3D 管理器失败"))?;
        let manager = manager.ok_or_else(|| Error::InvalidConfig("D3D 管理器缺失".into()))?;
        let device = shared.device();
        unsafe {
            manager
                .ResetDevice(&device, token)
                .map_err(mf_err("ResetDevice 失败"))?;
            let manager_unk = manager
                .cast::<windows::core::IUnknown>()
                .map_err(mf_err("D3D 管理器转接口失败"))?;
            let manager_raw = windows::core::Interface::as_raw(&manager_unk) as usize;
            transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager_raw)
                .map_err(mf_err("绑定 D3D 管理器失败"))?;
        }

        // 输出类型：H.264 + 码率（Annex-B 由编码器字节流保证）
        let out_type = unsafe { MFCreateMediaType() }.map_err(mf_err("创建输出类型失败"))?;
        unsafe {
            out_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(mf_err("设置输出主类型失败"))?;
            out_type
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
                .map_err(mf_err("设置输出子类型失败"))?;
            out_type
                .SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate_bps)
                .map_err(mf_err("设置码率失败"))?;
            out_type
                .SetUINT64(
                    &MF_MT_FRAME_SIZE,
                    ((cfg.resolution.width as u64) << 32) | cfg.resolution.height as u64,
                )
                .map_err(mf_err("设置输出尺寸失败"))?;
            out_type
                .SetUINT64(&MF_MT_FRAME_RATE, ((cfg.fps.max(1) as u64) << 32) | 1)
                .map_err(mf_err("设置输出帧率失败"))?;
            transform
                .SetOutputType(0, &out_type, 0)
                .map_err(mf_err("设置输出类型失败"))?;
        }

        // 输入类型：NV12（D3D11 零拷贝路径，尺寸/帧率随配置）
        let in_type = unsafe { MFCreateMediaType() }.map_err(mf_err("创建输入类型失败"))?;
        unsafe {
            in_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(mf_err("设置主类型失败"))?;
            in_type
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
                .map_err(mf_err("设置输入子类型失败"))?;
            in_type
                .SetUINT64(
                    &MF_MT_FRAME_SIZE,
                    ((cfg.resolution.width as u64) << 32) | cfg.resolution.height as u64,
                )
                .map_err(mf_err("设置输入尺寸失败"))?;
            in_type
                .SetUINT64(&MF_MT_FRAME_RATE, ((cfg.fps.max(1) as u64) << 32) | 1)
                .map_err(mf_err("设置输入帧率失败"))?;
            in_type
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(mf_err("设置逐行扫描失败"))?;
            transform
                .SetInputType(0, &in_type, 0)
                .map_err(mf_err("设置输入类型失败"))?;
        }

        // 编码参数：CBR + 低延迟 + 无限 GOP（不支持的键降级为默认值——
        // 部分驱动仅实现 ICodecAPI 子集，E_INVALIDARG 属常态）
        let codec: ICodecAPI = transform.cast().map_err(mf_err("获取 ICodecAPI 失败"))?;
        let _ = set_codec_u32(
            &codec,
            &CODECAPI_AVEncCommonRateControlMode,
            eAVEncCommonRateControlMode_CBR.0 as u32,
        );
        let _ = set_codec_u32(&codec, &CODECAPI_AVEncCommonMeanBitRate, cfg.bitrate_bps);
        let _ = set_codec_bool(
            &codec,
            &windows::Win32::Media::MediaFoundation::CODECAPI_AVEncCommonLowLatency,
        );
        let _ = set_codec_bool(
            &codec,
            &windows::Win32::Media::MediaFoundation::CODECAPI_AVLowLatencyMode,
        );
        if cfg.gop_len > 0 {
            let _ = set_codec_u32(
                &codec,
                &windows::Win32::Media::MediaFoundation::CODECAPI_AVEncMPVGOPSize,
                cfg.gop_len,
            );
        }

        // 流启动
        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(mf_err("通知开始流失败"))?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(mf_err("通知流启动失败"))?;
        }

        let event_gen: IMFMediaEventGenerator =
            transform.cast().map_err(mf_err("获取事件生成器失败"))?;

        let frame_duration_100ns = 10_000_000i64 / cfg.fps.max(1) as i64;
        Ok(Self {
            cfg,
            idr_pending: false,
            transform,
            event_gen,
            codec,
            _mf_guard: mf_guard,
            _coinit: coinit,
            sample_time_100ns: 0,
            frame_duration_100ns,
        })
    }

    /// 等待指定事件到达（1ms 轮询；超过截止时间返回 Timeout）。
    fn wait_event(
        &mut self,
        want: windows::Win32::Media::MediaFoundation::MF_EVENT_TYPE,
        deadline_ts: Timestamp,
    ) -> Result<()> {
        loop {
            match unsafe { self.event_gen.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => {
                    let kind = unsafe { event.GetType() }
                        .map_err(|e| Error::InvalidConfig(format!("取事件类型失败：{e}")))?;
                    if kind == want.0 as u32 {
                        return Ok(());
                    }
                    // 其它事件（DrainComplete 等）忽略继续等
                }
                Err(err) if err.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    if rc_win32core::host_time_us() >= deadline_ts.0 as u64 {
                        return Err(Error::Timeout(Duration::from_millis(5)));
                    }
                    std::thread::sleep(Duration::from_millis(EVENT_POLL_MS as u64));
                }
                Err(err) => {
                    return Err(Error::InvalidConfig(format!("事件等待失败：{err}")));
                }
            }
        }
    }
}

/// 类型化错误辅助。
fn mf_err(context: &'static str) -> impl Fn(windows::core::Error) -> Error {
    move |e| Error::InvalidConfig(format!("{context}：{e}"))
}

/// ICodecAPI 设 U32 值。
fn set_codec_u32(codec: &ICodecAPI, key: &windows::core::GUID, value: u32) -> Result<()> {
    let variant = windows::core::VARIANT::from(value);
    unsafe {
        codec
            .SetValue(key, &variant)
            .map_err(|e| Error::InvalidConfig(format!("设置编码参数失败：{e}")))
    }
}

/// ICodecAPI 设 BOOL 值。
fn set_codec_bool(codec: &ICodecAPI, key: &windows::core::GUID) -> Result<()> {
    let variant = windows::core::VARIANT::from(true);
    unsafe {
        codec
            .SetValue(key, &variant)
            .map_err(|e| Error::InvalidConfig(format!("设置编码参数失败：{e}")))
    }
}

/// 激活硬件（优先）H.264 编码 MFT。
fn activate_encoder(cfg: &EncoderConfig) -> Result<IMFTransform> {
    use windows::Win32::Media::MediaFoundation::{
        IMFActivate, MFVideoFormat_NV12, MFT_CATEGORY_VIDEO_ENCODER,
    };
    let infos = rc_win32core::mf::enumerate_video_mfts(
        MFT_CATEGORY_VIDEO_ENCODER,
        rc_win32core::mf::enum_all_sorted(),
    )
    .map_err(Error::InvalidConfig)?;
    if infos.is_empty() {
        return Err(Error::InvalidConfig(
            "未找到可用的 H.264 编码 MFT（无硬编且缺软件编码器）".into(),
        ));
    }
    let _ = cfg;
    // enumerate 只回名称/硬件标记；重新枚举激活首个可用
    unsafe {
        use windows::Win32::Media::MediaFoundation::{
            MFTEnumEx, MFT_ENUM_FLAG, MFT_REGISTER_TYPE_INFO,
        };
        let in_type = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let out_type = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        let mut act_ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG(
                windows::Win32::Media::MediaFoundation::MFT_ENUM_FLAG_ASYNCMFT.0
                    | windows::Win32::Media::MediaFoundation::MFT_ENUM_FLAG_HARDWARE.0
                    | windows::Win32::Media::MediaFoundation::MFT_ENUM_FLAG_SORTANDFILTER.0,
            ),
            Some(&in_type),
            Some(&out_type),
            &mut act_ptr,
            &mut count,
        )
        .map_err(|e| Error::InvalidConfig(format!("编码 MFT 枚举失败：{e}")))?;
        if act_ptr.is_null() || count == 0 {
            return Err(Error::InvalidConfig("无可用编码 MFT".into()));
        }
        for index in 0..count as usize {
            let slot = act_ptr.add(index);
            let Some(activate) = (*slot).clone() else {
                continue;
            };
            if let Ok(t) = activate.ActivateObject::<IMFTransform>() {
                windows::Win32::System::Com::CoTaskMemFree(Some(act_ptr.cast()));
                return Ok(t);
            }
        }
        windows::Win32::System::Com::CoTaskMemFree(Some(act_ptr.cast()));
    }
    Err(Error::InvalidConfig("编码 MFT 激活失败".into()))
}

// COM 指针仅在创建线程（会话线程）使用；Send 为满足 Encoder 契约的一次性移交
unsafe impl Send for MfEncoder {}

impl Encoder for MfEncoder {
    fn encode(
        &mut self,
        frame: &CapturedFrame,
        force_idr: bool,
        deadline_ts: Timestamp,
    ) -> Result<EncodedFrame> {
        let force_idr = force_idr || std::mem::take(&mut self.idr_pending);
        if force_idr {
            set_codec_bool(&self.codec, &CODECAPI_AVEncVideoForceKeyFrame)?;
        }
        // NeedInput → ProcessInput
        self.wait_event(METransformNeedInput, deadline_ts)?;

        let texture: ID3D11Texture2D = match &frame.texture {
            rc_capture::FrameTexture::D3D11(ptr) => unsafe {
                std::mem::transmute::<*mut core::ffi::c_void, ID3D11Texture2D>(*ptr)
            },
            _ => {
                return Err(Error::InvalidConfig(
                    "编码器仅接受 D3D11 NV12 纹理输入".into(),
                ))
            }
        };
        let buffer = unsafe {
            MFCreateDXGISurfaceBuffer(
                &ID3D11Texture2D::IID,
                &texture,
                0,
                windows::Win32::Foundation::FALSE,
            )
        }
        .map_err(mf_err("DXGI 表面包装失败"))?;
        let sample = unsafe { MFCreateSample() }.map_err(mf_err("创建样本失败"))?;
        unsafe {
            sample
                .AddBuffer(&buffer)
                .map_err(mf_err("样本加缓冲失败"))?;
            self.sample_time_100ns += self.frame_duration_100ns;
            sample
                .SetSampleTime(self.sample_time_100ns)
                .map_err(mf_err("设置样本时间失败"))?;
            sample
                .SetSampleDuration(self.frame_duration_100ns)
                .map_err(mf_err("设置样本时长失败"))?;
            self.transform
                .ProcessInput(0, &sample, 0)
                .map_err(mf_err("ProcessInput 失败"))?;
        }

        // HaveOutput → ProcessOutput
        self.wait_event(METransformHaveOutput, deadline_ts)?;
        let mut output = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: std::mem::ManuallyDrop::new(None),
            dwStatus: 0,
            pEvents: std::mem::ManuallyDrop::new(None),
        };
        let mut status = 0u32;
        unsafe {
            self.transform
                .ProcessOutput(0, std::slice::from_mut(&mut output), &mut status)
                .map_err(|e| Error::Capture {
                    recoverable: true,
                    msg: format!("ProcessOutput 失败：{e}"),
                })?;
        }
        let out_sample = (*output.pSample).clone().ok_or_else(|| Error::Capture {
            recoverable: true,
            msg: "编码器未返回样本".into(),
        })?;
        let buffer =
            unsafe { out_sample.ConvertToContiguousBuffer() }.map_err(mf_err("样本拼接失败"))?;
        let mut data_ptr = std::ptr::null_mut::<u8>();
        let mut data_len = 0u32;
        unsafe {
            buffer
                .Lock(&mut data_ptr, None, Some(&mut data_len))
                .map_err(mf_err("样本锁定失败"))?;
        }
        let data = if data_ptr.is_null() || data_len == 0 {
            Vec::new()
        } else {
            let slice = unsafe { std::slice::from_raw_parts(data_ptr, data_len as usize) };
            slice.to_vec()
        };
        unsafe {
            buffer.Unlock().map_err(mf_err("样本解锁失败"))?;
        }

        let is_keyframe = force_idr;
        Ok(EncodedFrame {
            data,
            kind: if is_keyframe {
                FrameKind::Idr
            } else {
                FrameKind::P
            },
            is_keyframe,
            encode_done_ts: Timestamp(rc_win32core::host_time_us() as u32),
        })
    }

    fn capabilities(&self) -> EncoderCaps {
        EncoderCaps {
            codec: self.cfg.codec,
            hardware: true,
            max_resolution: Resolution::new(4096, 4096),
            bitstream_format: BitstreamFormat::AnnexB,
            profiles: match self.cfg.codec {
                Codec::H264 => &[66, 77, 100],
                _ => &[],
            },
        }
    }

    fn set_bitrate(&mut self, bps: u32) -> Result<()> {
        if bps == 0 {
            return Err(Error::InvalidConfig("bitrate must be > 0".into()));
        }
        if set_codec_u32(&self.codec, &CODECAPI_AVEncCommonMeanBitRate, bps).is_err() {
            eprintln!(
                "[rc-encode-mf] 码率调整被驱动拒绝（保持旧值 {}）",
                self.cfg.bitrate_bps
            );
            return Ok(());
        }
        self.cfg.bitrate_bps = bps;
        Ok(())
    }

    fn request_idr(&mut self) {
        self.idr_pending = true;
    }

    fn flush(&mut self) -> Result<Vec<EncodedFrame>> {
        // 零输出延迟（E3）；复位编码器输出状态由 STREAMING 消息承担
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
        }
        Ok(Vec::new())
    }
}
