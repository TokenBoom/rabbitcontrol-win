//! RabbitControl Windows 硬件视频编码器（Media Foundation）。
//!
//! # 设计
//!
//! 基于 Media Foundation Transform (MFT) 的硬件 H.264/HEVC 编码：
//!
//! - **MFT 选择**：`MFTEnumEx` 枚举 `MFT_CATEGORY_VIDEO_ENCODER`，按
//!   `MFT_ENUM_FLAG_HARDWARE` 优先硬件异步 MFT（Intel QuickSync /
//!   NVIDIA NVENC / AMD AMF 的 MF 封装）；异步 MFT 经 `IMFMediaEventGenerator`
//!   事件驱动，支持 core 契约的 2-deep 流水线编码。
//! - **零拷贝输入**：采集纹理来自 [`rc_d3d11::SharedDevice`] 共享设备，
//!   `MFCreateDXGISurfaceBuffer` 把 `ID3D11Texture2D` 直接包装成
//!   `IMFSample` 投喂，无 GPU→CPU→GPU 往返；CPU 帧（假后端回环）
//!   走 staging 纹理上传路径。
//! - **低延迟契约**：`CODECAPI_AVLowLatencyMode` 开启、无限 GOP 按需
//!   IDR（`CODECAPI_AVEncVideoForceKeyframe`）、禁用 B 帧——core 的
//!   [`EncoderConfig::validate`] 在构造期已强制 E1/E2，这里负责映射到
//!   MF 属性。
//! - **输出格式**：Annex-B（MF 输出长度前缀码流，在拷贝进输出缓冲时
//!   就地转换为起始码，避免二次分配）。
//!
//! # 当前状态
//!
//! [`probe`]（编码 MFT 枚举）已实现；编码会话仍为骨架占位。
//! unsafe 策略：crate 级 `deny(unsafe_code)`，FFI 边界定点 `allow`。

#![deny(missing_docs)]
#![deny(unsafe_code)]

pub mod probe;

use rc_capture::CapturedFrame;
use rc_encode::{BitstreamFormat, Codec, EncodedFrame, Encoder, EncoderCaps, EncoderConfig};
use rc_types::{Error, Resolution, Result, Timestamp};

/// Media Foundation 硬件编码器（骨架占位）。
pub struct MfEncoder {
    cfg: EncoderConfig,
    idr_pending: bool,
}

impl MfEncoder {
    /// 构造编码器。违反延迟红线（E1/E2）的配置在构造期被拒绝。
    pub fn new(cfg: EncoderConfig) -> Result<Self> {
        cfg.validate()?;
        // TODO: MFStartup（在 rc_win32core::ComApartment MTA 内）→
        // MFTEnumEx 选硬件异步 MFT → 输入类型 MFVideoFormat_NV12/D3D11、
        // 输出类型 H264/HEVC  Annex-B → low latency / bitrate / GOP 属性
        // → ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM)。
        Ok(Self {
            cfg,
            idr_pending: false,
        })
    }
}

impl Encoder for MfEncoder {
    fn encode(
        &mut self,
        _frame: &CapturedFrame,
        _force_idr: bool,
        _deadline_ts: Timestamp,
    ) -> Result<EncodedFrame> {
        // TODO: MFCreateDXGISurfaceBuffer 包装输入纹理 → ProcessInput →
        // 事件循环 ProcessOutput drain → 长度前缀转 Annex-B 拼装输出。
        Err(Error::InvalidConfig(
            "mf encoder not yet implemented".into(),
        ))
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
        // TODO: CODECAPI_AVEncCommonMeanBitRate 动态调整。
        self.cfg.bitrate_bps = bps;
        Ok(())
    }

    fn request_idr(&mut self) {
        self.idr_pending = true;
    }

    fn flush(&mut self) -> Result<Vec<EncodedFrame>> {
        // 零输出延迟（E3）：无内部缓冲。
        Ok(Vec::new())
    }
}
