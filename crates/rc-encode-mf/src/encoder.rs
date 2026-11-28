//! Media Foundation H.264 硬件编码器实装（M5a.3）。
//!
//! # 异步 MFT 事件驱动
//!
//! 硬件编码 MFT 全部是异步 MFT：需要先 `MF_TRANSFORM_ASYNC_UNLOCK`，
//! 然后按事件序驱动——`METransformNeedInput` → `ProcessInput` →
//! `METransformHaveOutput` → `ProcessOutput`。事件队列以 `GetEvent(NO_WAIT)` + 1ms
//! 轮询消费；等待某类事件时顺手取到的其它事件入队留存（提前到达的下一个 NeedInput
//! 丢了的话，下一帧就再也等不到输入机会）。
//!
//! # 截止时间（E5，契约模型）
//!
//! `deadline_ts` 与帧时间戳同属注入的会话时间域：预算 = `deadline_ts − 帧时间戳`，
//! 从进入 [`Encoder::encode`] 起算。等到输出之前超时，输入已被 MFT 吃掉但输出被弃，
//! 解码端的参考链断了，所以请求下一帧强制 IDR，并在下一帧之前先把那份迟到的输出
//! 取走丢弃（不取走 MFT 不再要输入）。
//!
//! # 零拷贝输入
//!
//! `MFT_MESSAGE_SET_D3D_MANAGER` 绑定共享设备；采集交来的 NV12 纹理经
//! `MFCreateDXGISurfaceBuffer` 包装为 IMFMediaBuffer，编码器直接在 GPU
//! 上取用（不回读内存）。
//!
//! # 输出
//!
//! 硬件 H.264 编码器输出 Annex-B 码流（自带 `00 00 00 01` 起始码）；是否关键帧
//! 按码流里的 NAL 类型判定，不依赖「本帧请求过 IDR」。
//!
//! COM：Media Foundation 要求调用线程初始化过 COM 套间。编码器可能被移交给别的线程
//! 使用，所以按线程各做一次配对的套间初始化（线程退出时释放）。

#![allow(unsafe_code)] // MF / D3D11 FFI 边界

use std::collections::VecDeque;
use std::time::{Duration, Instant};

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
    MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER, MF_EVENT_FLAG_NO_WAIT, MF_EVENT_TYPE,
    MF_E_NO_EVENTS_AVAILABLE, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC_UNLOCK,
};

use rc_win32core::mf::MfGuard;

/// 事件轮询间隔。
const EVENT_POLL: Duration = Duration::from_millis(1);
/// 取走迟到输出的等待上限（它本来就已经产生了）。
const LATE_OUTPUT_WAIT: Duration = Duration::from_millis(200);

thread_local! {
    /// 本线程的 COM 套间：首次使用时初始化，线程退出时配对释放。
    static THREAD_COM: rc_win32core::ComApartment = rc_win32core::ComApartment::init(
        rc_win32core::ApartmentModel::MultiThreaded,
    )
    .expect("编码线程初始化 COM 套间失败");
}

/// 保证当前线程已初始化 COM 套间。
fn ensure_com() {
    THREAD_COM.with(|_| ());
}

/// Media Foundation 硬件编码器。
pub struct MfEncoder {
    cfg: EncoderConfig,
    idr_pending: bool,
    transform: IMFTransform,
    event_gen: IMFMediaEventGenerator,
    codec: ICodecAPI,
    /// 等待某类事件时顺手取到的其它事件，按原顺序留待后续使用。
    queued_events: VecDeque<MF_EVENT_TYPE>,
    /// 上一次等输出超时：那份迟到的输出还没取走，下一帧编码前先丢弃。
    late_output_pending: bool,
    _mf_guard: MfGuard,
    /// 输入样本时间基（100ns 单位递增，保证单调）。
    sample_time_100ns: i64,
    /// 帧时长（100ns），按 fps 推导。
    frame_duration_100ns: i64,
}

impl MfEncoder {
    /// 构造编码器。违反延迟红线（E1/E2）的配置在构造期被拒绝。
    pub fn new(cfg: EncoderConfig) -> Result<Self> {
        cfg.validate()?;
        ensure_com();
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
        let encoder = Self {
            cfg,
            idr_pending: false,
            transform,
            event_gen,
            codec,
            queued_events: VecDeque::new(),
            late_output_pending: false,
            _mf_guard: mf_guard,
            sample_time_100ns: 0,
            frame_duration_100ns,
        };
        eprintln!(
            "[encoder] 已创建：{:?} @ {} fps · {} bps",
            encoder.cfg.resolution, encoder.cfg.fps, encoder.cfg.bitrate_bps
        );
        Ok(encoder)
    }

    /// 等到指定事件（1ms 轮询）；超过截止时间返回 `Error::Timeout`。
    fn wait_event(&mut self, want: MF_EVENT_TYPE, deadline: Instant) -> Result<()> {
        loop {
            if let Some(position) = self.queued_events.iter().position(|kind| *kind == want) {
                self.queued_events.remove(position);
                return Ok(());
            }
            match unsafe { self.event_gen.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => {
                    let kind = unsafe { event.GetType() }.map_err(mf_err("取事件类型失败"))?;
                    let kind = MF_EVENT_TYPE(kind as i32);
                    if kind == want {
                        return Ok(());
                    }
                    // 其它事件（提前到达的 NeedInput、DrainComplete 等）留待后续
                    self.queued_events.push_back(kind);
                }
                Err(err) if err.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    if Instant::now() >= deadline {
                        return Err(Error::Timeout(EVENT_POLL));
                    }
                    std::thread::sleep(EVENT_POLL);
                }
                Err(err) => {
                    return Err(mf_err("事件等待失败")(err));
                }
            }
        }
    }

    /// 向 MFT 要一帧输出，取出码流字节。
    fn take_output(&mut self) -> Result<Vec<u8>> {
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
        }
        .map_err(|e| Error::Capture {
            recoverable: true,
            msg: format!("ProcessOutput 失败：{e}"),
        })?;
        // pSample 是 ManuallyDrop：取出后由 out_sample 独占，避免双重释放
        let out_sample: Option<windows::Win32::Media::MediaFoundation::IMFSample> =
            unsafe { std::mem::ManuallyDrop::take(&mut output.pSample) };
        let out_sample = out_sample.ok_or_else(|| Error::Capture {
            recoverable: true,
            msg: "编码器未返回样本".into(),
        })?;
        let buffer =
            unsafe { out_sample.ConvertToContiguousBuffer() }.map_err(mf_err("样本拼接失败"))?;
        let mut data_ptr = std::ptr::null_mut::<u8>();
        // `pcbCurrentLength` 是已填充长度（编码器写好的字节数）
        let mut data_len = 0u32;
        unsafe {
            buffer
                .Lock(&mut data_ptr, None, Some(&mut data_len))
                .map_err(mf_err("样本锁定失败"))?;
        }
        let data = if data_ptr.is_null() || data_len == 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(data_ptr, data_len as usize) }.to_vec()
        };
        unsafe { buffer.Unlock() }.map_err(mf_err("样本解锁失败"))?;
        Ok(data)
    }
}

/// 类型化错误辅助。
fn mf_err(context: &'static str) -> impl Fn(windows::core::Error) -> Error {
    move |e| {
        eprintln!("[encoder] {context}：{e}");
        Error::InvalidConfig(format!("{context}：{e}"))
    }
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

/// 按 NAL 单元类型判断 Annex-B 码流的首个片是不是 IDR（类型 5）。
///
/// 返回 `None` 表示没找到片（只有 SPS/PPS/SEI 等），由调用方回退到「是否请求过 IDR」。
fn first_slice_is_idr(data: &[u8]) -> Option<bool> {
    let mut index = 0;
    while index + 3 < data.len() {
        if data[index] == 0 && data[index + 1] == 0 && data[index + 2] == 1 {
            match data[index + 3] & 0x1F {
                5 => return Some(true),
                1 => return Some(false),
                _ => index += 3,
            }
        } else {
            index += 1;
        }
    }
    None
}

/// 激活硬件（优先）H.264 编码 MFT。
fn activate_encoder(cfg: &EncoderConfig) -> Result<IMFTransform> {
    use windows::Win32::Media::MediaFoundation::{
        IMFActivate, MFTEnumEx, MFVideoFormat_NV12, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG,
        MFT_REGISTER_TYPE_INFO,
    };
    let infos = rc_win32core::mf::enumerate_video_mfts(
        MFT_CATEGORY_VIDEO_ENCODER,
        rc_win32core::mf::enum_all_sorted(),
    )
    .map_err(Error::InvalidConfig)?;
    eprintln!(
        "[encoder] 候选编码 MFT：{}",
        infos
            .iter()
            .map(|info| info.friendly_name.as_str())
            .collect::<Vec<_>>()
            .join(" / ")
    );
    if infos.is_empty() {
        return Err(Error::InvalidConfig(
            "未找到可用的 H.264 编码 MFT（无硬编且缺软件编码器）".into(),
        ));
    }
    let _ = cfg;
    // enumerate 只回名称/硬件标记；重新枚举激活首个可用
    unsafe {
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
            eprintln!("[encoder] MFTEnumEx 没有找到异步硬件 H.264 编码 MFT");
            return Err(Error::InvalidConfig("无可用编码 MFT".into()));
        }
        for index in 0..count as usize {
            let slot = act_ptr.add(index);
            let Some(activate) = (*slot).clone() else {
                continue;
            };
            let name = rc_win32core::mf::activate_info(&activate).friendly_name;
            match activate.ActivateObject::<IMFTransform>() {
                Ok(transform) => {
                    eprintln!("[encoder] 选用编码 MFT：{name}");
                    windows::Win32::System::Com::CoTaskMemFree(Some(act_ptr.cast()));
                    return Ok(transform);
                }
                Err(err) => eprintln!("[encoder] {name} 激活失败，尝试下一个：{err}"),
            }
        }
        windows::Win32::System::Com::CoTaskMemFree(Some(act_ptr.cast()));
    }
    Err(Error::InvalidConfig("编码 MFT 激活失败".into()))
}

// COM 指针只在一次调用的线程上使用；Send 为满足 Encoder 契约的一次性移交
unsafe impl Send for MfEncoder {}

impl Encoder for MfEncoder {
    fn encode(
        &mut self,
        frame: &CapturedFrame,
        force_idr: bool,
        deadline_ts: Timestamp,
    ) -> Result<EncodedFrame> {
        // E5：预算 = deadline_ts - 帧时间戳（同一注入时间域），从进入本函数起算
        let budget_us = deadline_ts.0.saturating_sub(frame.timestamp.0);
        if budget_us == 0 {
            return Err(Error::Timeout(Duration::ZERO));
        }
        let started = Instant::now();
        let deadline = started + Duration::from_micros(u64::from(budget_us));
        ensure_com();
        let force_idr = force_idr || std::mem::take(&mut self.idr_pending);

        // 上一次迟到的输出先取走丢弃：不取走，MFT 不会再要输入
        if std::mem::take(&mut self.late_output_pending) {
            self.wait_event(METransformHaveOutput, Instant::now() + LATE_OUTPUT_WAIT)?;
            self.take_output()?;
        }

        if force_idr {
            set_codec_bool(&self.codec, &CODECAPI_AVEncVideoForceKeyFrame)?;
        }
        // NeedInput → ProcessInput。等不到输入机会说明 MFT 还没消化完：这帧没被吃掉，
        // 参考链无损，直接超时返回即可。
        self.wait_event(METransformNeedInput, deadline)
            .map_err(|_| Error::Timeout(started.elapsed()))?;

        // 只借用、不接管引用计数：纹理归采集端（`resource_guard`）持有。之前这里把指针
        // transmute 成 COM 对象再随作用域 Drop，等于每帧多 Release 一次，采集端的纹理
        // 被提前销毁，第二帧起就是释放后使用。
        let texture: &ID3D11Texture2D = match &frame.texture {
            rc_capture::FrameTexture::D3D11(ptr) => {
                unsafe { ID3D11Texture2D::from_raw_borrowed(ptr) }
                    .ok_or_else(|| Error::InvalidConfig("采集帧纹理指针为空".into()))?
            }
            _ => {
                return Err(Error::InvalidConfig(
                    "编码器仅接受 D3D11 NV12 纹理输入".into(),
                ))
            }
        };
        let buffer = unsafe {
            MFCreateDXGISurfaceBuffer(
                &ID3D11Texture2D::IID,
                texture,
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

        // HaveOutput → ProcessOutput。输入已经被吃掉：等不到输出就超时返回，那份迟到
        // 的输出下一帧丢弃，并请求 IDR 重建参考链。
        if self.wait_event(METransformHaveOutput, deadline).is_err() {
            self.late_output_pending = true;
            self.idr_pending = true;
            return Err(Error::Timeout(started.elapsed()));
        }
        let data = self.take_output()?;

        let elapsed_us = u32::try_from(started.elapsed().as_micros()).unwrap_or(u32::MAX);
        let is_keyframe = first_slice_is_idr(&data).unwrap_or(force_idr);
        Ok(EncodedFrame {
            data,
            kind: if is_keyframe {
                FrameKind::Idr
            } else {
                FrameKind::P
            },
            is_keyframe,
            // 完成时刻按注入时间域估算（契约允许的模型，见 rc-encode 文档）
            encode_done_ts: Timestamp(frame.timestamp.0.saturating_add(elapsed_us)),
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
        self.queued_events.clear();
        self.late_output_pending = false;
        self.idr_pending = true;
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::first_slice_is_idr;

    #[test]
    fn idr_is_detected_from_the_first_slice_nal_type() {
        // SPS(7) + PPS(8) + IDR 片(5)
        let idr = [
            0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0, 0, 0, 1, 0x65, 0x88,
        ];
        // SPS + PPS + 非 IDR 片(1)
        let p = [
            0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0, 0, 0, 1, 0x41, 0x9a,
        ];
        assert_eq!(first_slice_is_idr(&idr), Some(true));
        assert_eq!(first_slice_is_idr(&p), Some(false));
        assert_eq!(first_slice_is_idr(&[0, 0, 0, 1, 0x67, 0x42]), None);
    }
}
