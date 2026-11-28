//! RabbitControl Windows 硬件视频解码器（Media Foundation / D3D11VA）。
//!
//! - MFT 选择：`MFTEnumEx` 枚举 `MFT_CATEGORY_VIDEO_DECODER`（H.264 入 NV12 出），硬件
//!   解码 MFT 优先，逐个尝试直到有一个能完成初始化，全部失败才报错。
//! - 零拷贝：`IMFDXGIDeviceManager` 绑定 [`rc_d3d11::SharedDevice`]。解出的帧拷进本解码器
//!   自有的 NV12 纹理环（4 张轮流用）——交给上层的纹理指针由解码器持有，调用方只读、
//!   不负责释放；一张纹理最早在 4 帧之后才会被改写。
//! - 低延迟：`MF_LOW_LATENCY` + `CODECAPI_AVLowLatencyMode`；异步 MFT（硬件解码器都是）
//!   按 `METransformNeedInput` / `METransformHaveOutput` 事件驱动。
//! - 2 帧深流水线（[`Decoder::submit_decode`] / [`Decoder::recv_decoded`]）：解码耗时和
//!   下一帧的到达重叠，高分辨率下单帧解码超过管线的同步预算也不会被判成解码失败。
//!   管线自己把在途上限压在 2 帧、队列满了按过期丢帧，所以解码器不按截止时间拒收：
//!   延迟由管线的有界队列兜底，连续性交给解码器。
//!
//! COM：Media Foundation 要求调用线程初始化过 COM 套间。解码器可能被移交给别的线程
//! 使用，所以按线程各做一次配对的套间初始化（线程退出时释放）。

#![deny(missing_docs)]
#![allow(unsafe_code)] // FFI 边界

pub mod probe;

use std::collections::VecDeque;
use std::ptr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rc_d3d11::SharedDevice;
use rc_decode::{DecodeOutput, DecodeSubmitStatus, Decoder};
use rc_encode::Codec;
use rc_types::{Error, FrameKind, Resolution, Result, Timestamp};
use windows::core::{Interface, VARIANT};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Resource, ID3D11Texture2D, D3D11_BOX, D3D11_TEXTURE2D_DESC,
};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVLowLatencyMode, ICodecAPI, IMFDXGIBuffer, IMFMediaEventGenerator, IMFMediaType,
    IMFSample, IMFTransform, METransformHaveOutput, METransformNeedInput,
    MFCreateDXGIDeviceManager, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFMediaType_Video, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFT_CATEGORY_VIDEO_DECODER, MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_REGISTER_TYPE_INFO, MF_EVENT_FLAG_NO_WAIT, MF_EVENT_TYPE, MF_E_NOTACCEPTING,
    MF_E_NO_EVENTS_AVAILABLE, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
    MF_LOW_LATENCY, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK,
};

/// 交给上层后仍然有效的解码帧数量（纹理环长度）：第 N 帧的纹理在第 N+4 帧到来时才会改写。
const TEXTURE_RING: usize = 4;
/// 事件 / 输出轮询间隔。
const EVENT_POLL: Duration = Duration::from_millis(1);
/// 等 MFT 要输入的最长时间（它消化完上一帧就会要）。
const INPUT_WAIT: Duration = Duration::from_millis(50);
/// 同步解码（一进一出）时等输出的最长时间：关键帧首当其冲要建参考链，给得更宽。
const OUTPUT_WAIT: Duration = Duration::from_millis(50);
const OUTPUT_WAIT_KEYFRAME: Duration = Duration::from_millis(100);

thread_local! {
    /// 本线程的 COM 套间：首次使用时初始化，线程退出时配对释放。
    static THREAD_COM: rc_win32core::ComApartment = rc_win32core::ComApartment::init(
        rc_win32core::ApartmentModel::MultiThreaded,
    )
    .expect("解码线程初始化 COM 套间失败");
}

/// 保证当前线程已初始化 COM 套间。
fn ensure_com() {
    THREAD_COM.with(|_| ());
}

/// MF 解码器配置。
#[derive(Debug, Clone, Copy)]
pub struct MfDecoderConfig {
    /// 码流编解码器。
    pub codec: Codec,
    /// 支持的最大分辨率（保留给后续的分辨率校验；当前不限制）。
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
    /// 异步 MFT 的事件队列；同步 MFT 为 `None`。
    events: Option<IMFMediaEventGenerator>,
    /// 等待某类事件时顺手取到的其它事件，按原顺序留待后续使用。
    queued_events: VecDeque<MF_EVENT_TYPE>,
    shared: Arc<SharedDevice>,
    /// 解码帧落地用的 NV12 纹理环（尺寸变化时重建）。
    ring: Vec<ID3D11Texture2D>,
    ring_next: usize,
    waiting_keyframe: bool,
    /// 流水线解码：已提交、尚未被领走的帧 token（FIFO，与解码顺序一致）。
    in_flight: VecDeque<u64>,
    /// 已解出、尚未被领走的画面，与 `in_flight` 队首一一对应。
    ready: VecDeque<DecodeOutput>,
    /// Media Foundation 运行时引用（全进程配对 `MFStartup` / `MFShutdown`）。
    mf_guard: Option<rc_win32core::mf::MfGuard>,
}

// COM 指针只在一次调用的线程上使用；Send 为满足 Decoder 契约的一次性移交。
unsafe impl Send for MfDecoder {}

fn mfw(e: windows::core::Error) -> Error {
    Error::InvalidConfig(format!("{e:?}"))
}

fn invalid(context: &str) -> impl Fn(windows::core::Error) -> Error + '_ {
    move |e| {
        eprintln!("[decoder] {context}：{e}");
        Error::InvalidConfig(format!("{context}：{e}"))
    }
}

/// 纹理尺寸。
fn texture_size(texture: &ID3D11Texture2D) -> (u32, u32) {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };
    (desc.Width, desc.Height)
}

impl MfDecoder {
    /// 构造解码器。
    pub fn new(cfg: MfDecoderConfig) -> Result<Self> {
        if cfg.codec != Codec::H264 {
            return Err(Error::InvalidConfig("mf decoder: HEVC 待 D6 后开放".into()));
        }
        ensure_com();
        let mf_guard = rc_win32core::mf::MfGuard::startup().map_err(Error::InvalidConfig)?;
        let shared = SharedDevice::get().map_err(Error::InvalidConfig)?;
        let _ = cfg.max_resolution;

        let candidates = unsafe { activate_h264_decoders() }
            .map_err(|e| Error::InvalidConfig(format!("解码 MFT 枚举失败：{e}")))?;
        eprintln!(
            "[decoder] 候选解码 MFT：{}",
            candidates
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>()
                .join(" / ")
        );
        let mut last_error = Error::InvalidConfig("未找到 H.264 解码 MFT".into());
        for (name, transform) in candidates {
            let mut decoder = Self {
                transform,
                events: None,
                queued_events: VecDeque::new(),
                shared: Arc::clone(&shared),
                ring: Vec::new(),
                ring_next: 0,
                waiting_keyframe: true,
                in_flight: VecDeque::new(),
                ready: VecDeque::new(),
                mf_guard: None,
            };
            match decoder.setup() {
                Ok(()) => {
                    eprintln!(
                        "[decoder] 选用解码 MFT：{name}（{}）",
                        if decoder.events.is_some() {
                            "异步事件驱动"
                        } else {
                            "同步"
                        }
                    );
                    decoder.mf_guard = Some(mf_guard);
                    return Ok(decoder);
                }
                Err(err) => {
                    eprintln!("[decoder] {name} 不可用：{err}");
                    last_error = err;
                }
            }
        }
        Err(last_error)
    }

    /// 把枚举出的 MFT 配置到可以开始解码：异步解锁、低延迟、D3D 设备、输入输出类型。
    fn setup(&mut self) -> Result<()> {
        unsafe {
            let attributes = self
                .transform
                .GetAttributes()
                .map_err(invalid("读取 MFT 属性失败"))?;
            let asynchronous = attributes
                .GetUINT32(&MF_TRANSFORM_ASYNC)
                .map(|value| value != 0)
                .unwrap_or(false);
            if asynchronous {
                // 硬件解码 MFT 是异步 MFT，必须解锁后才能设置类型 / 处理消息
                attributes
                    .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                    .map_err(invalid("异步 MFT 解锁失败"))?;
                self.events = Some(
                    self.transform
                        .cast::<IMFMediaEventGenerator>()
                        .map_err(invalid("异步 MFT 没有事件队列"))?,
                );
            }
            let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
            if let Ok(codec) = self.transform.cast::<ICodecAPI>() {
                let _ = codec.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
            }

            let mut manager = None;
            let mut token = 0u32;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)
                .map_err(invalid("DXGIDeviceManager 失败"))?;
            let manager = manager.ok_or_else(|| Error::InvalidConfig("未返回设备管理器".into()))?;
            manager
                .ResetDevice(&self.shared.device(), token)
                .map_err(invalid("ResetDevice 失败"))?;
            self.transform
                .ProcessMessage(
                    MFT_MESSAGE_SET_D3D_MANAGER,
                    Interface::as_raw(&manager) as usize,
                )
                .map_err(invalid("SET_D3D_MANAGER 失败"))?;

            // 输入类型：H.264（解码器：输入先于输出协商）
            let in_type: IMFMediaType = MFCreateMediaType().map_err(invalid("创建输入类型失败"))?;
            in_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(invalid("设置输入主类型失败"))?;
            in_type
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
                .map_err(invalid("设置输入子类型失败"))?;
            self.transform
                .SetInputType(0, &in_type, 0)
                .map_err(invalid("SetInputType 失败"))?;
            self.set_nv12_output_type()?;

            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(invalid("通告流开始失败"))?;
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(invalid("通告开始推流失败"))?;
        }
        Ok(())
    }

    /// 从 MFT 自报的可用输出类型里选 NV12 并设置（盲设新类型会被拒绝）。
    fn set_nv12_output_type(&mut self) -> Result<()> {
        unsafe {
            for index in 0..16u32 {
                match self.transform.GetOutputAvailableType(0, index) {
                    Ok(available) => {
                        let is_nv12 = available
                            .GetGUID(&MF_MT_SUBTYPE)
                            .map(|subtype| subtype == MFVideoFormat_NV12)
                            .unwrap_or(false);
                        if is_nv12 {
                            self.transform
                                .SetOutputType(0, &available, 0)
                                .map_err(invalid("SetOutputType 失败"))?;
                            return Ok(());
                        }
                    }
                    Err(_) => break, // 枚举结束
                }
            }
        }
        Err(Error::InvalidConfig("解码器未提供 NV12 输出类型".into()))
    }

    /// 等到指定类型的 MFT 事件；其它事件按序留存。超时返回 `Error::Timeout`。
    fn wait_event(&mut self, want: MF_EVENT_TYPE, limit: Instant) -> Result<()> {
        let Some(events) = self.events.clone() else {
            return Ok(());
        };
        loop {
            if let Some(position) = self.queued_events.iter().position(|kind| *kind == want) {
                self.queued_events.remove(position);
                return Ok(());
            }
            match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => {
                    let kind = unsafe { event.GetType() }.map_err(invalid("取事件类型失败"))?;
                    let kind = MF_EVENT_TYPE(kind as i32);
                    if kind == want {
                        return Ok(());
                    }
                    // 不能丢：等待输出期间到达的 NeedInput 等事件后面还要用
                    self.queued_events.push_back(kind);
                }
                Err(err) if err.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    if Instant::now() >= limit {
                        return Err(Error::Timeout(INPUT_WAIT));
                    }
                    std::thread::sleep(EVENT_POLL);
                }
                Err(err) => {
                    return Err(invalid("事件等待失败")(err));
                }
            }
        }
    }

    /// 没等到输出时的结果：还没解出过关键帧就继续等关键帧，否则就是「这一帧没画面」。
    fn no_output(&self) -> DecodeOutput {
        if self.waiting_keyframe {
            DecodeOutput::NeedKeyframe
        } else {
            DecodeOutput::Nothing
        }
    }

    /// 喂一帧码流给 MFT。等关键帧期间拒收 P 帧，返回 `Ok(false)`。
    fn feed(&mut self, data: &[u8], kind: FrameKind, limit: Instant) -> Result<bool> {
        if data.is_empty() {
            return Ok(false);
        }
        if kind == FrameKind::Idr {
            self.waiting_keyframe = false;
        } else if self.waiting_keyframe {
            return Ok(false);
        }
        ensure_com();
        unsafe {
            let buffer = MFCreateMemoryBuffer(data.len() as u32).map_err(mfw)?;
            let mut destination: *mut u8 = ptr::null_mut();
            // Lock 的两个长度都是出参：`pcbMaxLength` 是缓冲区容量，`pcbCurrentLength`
            // 是已填充长度（新建缓冲为 0）。取错成后者会让第一帧就误报「缓冲不足」。
            let mut capacity = 0u32;
            buffer
                .Lock(&mut destination, Some(&mut capacity), None)
                .map_err(mfw)?;
            if (data.len() as u32) > capacity || destination.is_null() {
                let _ = buffer.Unlock();
                return Err(Error::InvalidConfig("输入样本缓冲不足".into()));
            }
            ptr::copy_nonoverlapping(data.as_ptr(), destination, data.len());
            buffer.SetCurrentLength(data.len() as u32).map_err(mfw)?;
            buffer.Unlock().map_err(mfw)?;
            let sample: IMFSample = MFCreateSample().map_err(mfw)?;
            sample.AddBuffer(&buffer).map_err(mfw)?;

            // 异步 MFT 必须等到 NeedInput 才能喂，否则被拒收
            if self.events.is_some() {
                self.wait_event(METransformNeedInput, Instant::now() + INPUT_WAIT)?;
            }
            match self.transform.ProcessInput(0, &sample, 0) {
                Ok(()) => {}
                Err(err) if err.code() == MF_E_NOTACCEPTING => {
                    // 同步解码器在上一帧输出没取走时会拒收：先把在途的输出收进 `ready` 再试一次
                    let drained = !self.in_flight.is_empty() && self.collect_output(limit)?;
                    if !drained {
                        return Err(invalid("ProcessInput 失败")(err));
                    }
                    self.transform
                        .ProcessInput(0, &sample, 0)
                        .map_err(invalid("ProcessInput 重试失败"))?;
                }
                Err(err) => return Err(invalid("ProcessInput 失败")(err)),
            }
        }
        Ok(true)
    }

    /// 等到并取出一帧输出放进 `ready`；到 `limit` 仍没有返回 `Ok(false)`。
    fn collect_output(&mut self, limit: Instant) -> Result<bool> {
        loop {
            if self.events.is_some() {
                match self.wait_event(METransformHaveOutput, limit) {
                    Ok(()) => {}
                    Err(err) if !err.is_fatal() => return Ok(false),
                    Err(err) => return Err(err),
                }
            }
            match self.take_output()? {
                Some(output) => {
                    self.ready.push_back(output);
                    return Ok(true);
                }
                None if Instant::now() < limit => std::thread::sleep(EVENT_POLL),
                None => return Ok(false),
            }
        }
    }

    /// 领走最早的一帧输出及其 token。
    fn take_ready(&mut self) -> Result<(u64, DecodeOutput)> {
        let token = self
            .in_flight
            .pop_front()
            .ok_or_else(|| Error::InvalidConfig("解码输出没有对应的在途帧".into()))?;
        let output = self
            .ready
            .pop_front()
            .ok_or_else(|| Error::InvalidConfig("在途帧没有解码输出".into()))?;
        Ok((token, output))
    }

    /// 向 MFT 要一帧输出并拷进纹理环；`Ok(None)` 表示暂时没有可取的输出。
    fn take_output(&mut self) -> Result<Option<DecodeOutput>> {
        loop {
            let mut out = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: std::mem::ManuallyDrop::new(None),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            };
            let mut status = 0u32;
            let result = unsafe {
                self.transform
                    .ProcessOutput(0, std::slice::from_mut(&mut out), &mut status)
            };
            match result {
                Ok(()) => {
                    // pSample 是 ManuallyDrop：取出后由 sample 独占，避免双重释放
                    let sample: Option<IMFSample> =
                        unsafe { std::mem::ManuallyDrop::take(&mut out.pSample) };
                    let Some(sample) = sample else {
                        return Err(Error::InvalidConfig("解码 MFT 未返回样本".into()));
                    };
                    let (texture, width, height) = unsafe { self.copy_to_ring(&sample)? };
                    if width == 0 || height == 0 {
                        continue;
                    }
                    self.waiting_keyframe = false;
                    return Ok(Some(DecodeOutput::Frame(rc_decode::DecodedFrame {
                        // 纹理由纹理环持有：只交出借用指针，不转移引用计数
                        texture: rc_capture::FrameTexture::D3D11(Interface::as_raw(&texture)),
                        // 管线记延迟用的是自己的会话时钟，这里只是解码器本地的完成时刻
                        decode_done_ts: Timestamp(rc_win32core::host_time_us() as u32),
                        width,
                        height,
                    })));
                }
                Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                Err(err) if err.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // 首帧之后 MFT 才知道真实尺寸，要求重新协商输出类型
                    self.set_nv12_output_type()?;
                }
                Err(err) => {
                    return Err(invalid("ProcessOutput 失败")(err));
                }
            }
        }
    }

    /// 把解码样本的 D3D11 纹理拷进纹理环的下一张（子资源下标取自样本），返回这张纹理。
    ///
    /// 安全性：样本缓冲经 `IMFDXGIBuffer` 取出的纹理引用在拷贝期间保持存活。
    unsafe fn copy_to_ring(&mut self, sample: &IMFSample) -> Result<(ID3D11Texture2D, u32, u32)> {
        let buffer = sample
            .GetBufferByIndex(0)
            .map_err(invalid("取解码样本缓冲失败"))?;
        let dxgi_buffer: IMFDXGIBuffer = buffer.cast().map_err(invalid("解码样本缓冲非 D3D11"))?;
        let mut source: Option<ID3D11Texture2D> = None;
        dxgi_buffer
            .GetResource(
                &ID3D11Texture2D::IID,
                &mut source as *mut _ as *mut *mut core::ffi::c_void,
            )
            .map_err(invalid("GetResource 失败"))?;
        let source = source.ok_or_else(|| Error::InvalidConfig("GetResource 未返回".into()))?;
        let subresource = dxgi_buffer
            .GetSubresourceIndex()
            .map_err(invalid("GetSubresourceIndex 失败"))?;

        let (width, height) = texture_size(&source);
        let size_changed = self
            .ring
            .first()
            .map_or(true, |texture| texture_size(texture) != (width, height));
        if size_changed {
            self.ring = (0..TEXTURE_RING)
                .map(|_| self.shared.create_nv12_texture(width, height))
                .collect::<std::result::Result<_, _>>()
                .map_err(Error::InvalidConfig)?;
            self.ring_next = 0;
        }
        let target = self.ring[self.ring_next].clone();
        self.ring_next = (self.ring_next + 1) % TEXTURE_RING;
        let source_resource: ID3D11Resource =
            source.cast().map_err(invalid("解码源纹理转资源失败"))?;
        let target_resource: ID3D11Resource = target.cast().map_err(invalid("纹理环转资源失败"))?;
        self.shared.context().CopySubresourceRegion(
            Some(&target_resource),
            0,
            0,
            0,
            0,
            Some(&source_resource),
            subresource,
            Some(&D3D11_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: width,
                bottom: height,
                back: 1,
            }),
        );
        Ok((target, width, height))
    }
}

impl Decoder for MfDecoder {
    fn decode(
        &mut self,
        data: &[u8],
        kind: FrameKind,
        _deadline_ts: Timestamp,
    ) -> Result<DecodeOutput> {
        let limit = Instant::now()
            + if kind == FrameKind::Idr {
                OUTPUT_WAIT_KEYFRAME
            } else {
                OUTPUT_WAIT
            };
        if !self.feed(data, kind, limit)? {
            return Ok(self.no_output());
        }
        self.in_flight.push_back(0);
        // 同步路径没有流水线缓冲：这一帧没在预算内解出、队列里又没有更早解好的画面，
        // 才算「没画面」；token 留在队列里，随下一帧一起按先进先出领走。
        if !self.collect_output(limit)? && self.ready.is_empty() {
            return Ok(self.no_output());
        }
        self.take_ready().map(|(_, output)| output)
    }

    fn needs_keyframe(&self) -> bool {
        self.waiting_keyframe
    }

    fn flush(&mut self) -> Result<()> {
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)
                .map_err(invalid("冲刷解码器失败"))?;
            // 异步 MFT 冲刷后停在「流未开始」状态，要重新通告才开始发 NeedInput
            if self.events.is_some() {
                self.transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                    .map_err(invalid("重新通告流开始失败"))?;
            }
        }
        self.queued_events.clear();
        self.in_flight.clear();
        self.ready.clear();
        self.waiting_keyframe = true;
        Ok(())
    }

    fn supports_pipelined_decode(&self) -> bool {
        true
    }

    fn submit_decode(
        &mut self,
        token: u64,
        data: &[u8],
        kind: FrameKind,
        _deadline_ts: Timestamp,
    ) -> Result<DecodeSubmitStatus> {
        if !self.feed(data, kind, Instant::now() + INPUT_WAIT)? {
            return Ok(DecodeSubmitStatus::NeedKeyframe);
        }
        self.in_flight.push_back(token);
        Ok(DecodeSubmitStatus::Submitted)
    }

    fn recv_decoded(&mut self, timeout: Duration) -> Result<Option<(u64, Result<DecodeOutput>)>> {
        if self.in_flight.is_empty() {
            return Ok(None);
        }
        // 队首还没解出就等它；已经解好的立刻领走，不看 timeout
        if self.ready.is_empty() && !self.collect_output(Instant::now() + timeout)? {
            return Ok(None);
        }
        let (token, output) = self.take_ready()?;
        Ok(Some((token, Ok(output))))
    }

    fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }
}

/// 枚举并激活全部 H.264 → NV12 解码 MFT（硬件在前），返回（友好名称, MFT）。
unsafe fn activate_h264_decoders() -> windows::core::Result<Vec<(String, IMFTransform)>> {
    use windows::Win32::Media::MediaFoundation::IMFActivate;
    let in_type = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let out_type = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let mut activates: *mut Option<IMFActivate> = ptr::null_mut();
    let mut count = 0u32;
    MFTEnumEx(
        MFT_CATEGORY_VIDEO_DECODER,
        rc_win32core::mf::enum_all_sorted(),
        Some(&in_type),
        Some(&out_type),
        &mut activates,
        &mut count,
    )?;
    let mut result = Vec::new();
    if !activates.is_null() {
        for index in 0..count as usize {
            let Some(activate) = (*activates.add(index)).clone() else {
                continue;
            };
            let info = rc_win32core::mf::activate_info(&activate);
            if let Ok(transform) = activate.ActivateObject::<IMFTransform>() {
                result.push((info.friendly_name, transform));
            }
        }
        windows::Win32::System::Com::CoTaskMemFree(Some(activates.cast()));
    }
    Ok(result)
}

/// 构造解码器。
pub fn new_decoder(cfg: MfDecoderConfig) -> Result<MfDecoder> {
    MfDecoder::new(cfg)
}
