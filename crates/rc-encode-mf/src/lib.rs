//! RabbitControl Windows 硬件视频编码器（Media Foundation，对应 macOS `rc-encode-vt`）。
//!
//! # 设计
//!
//! 基于 Media Foundation Transform (MFT) 的硬件 H.264/HEVC 编码：
//! - `MFTEnumEx` 枚举 `MFT_CATEGORY_VIDEO_ENCODER`，优先硬件异步 MFT
//!   （Intel QuickSync / NVIDIA NVENC 的 MF 封装）；
//! - 输入直接吃 [`FrameTexture::D3D11`] 纹理（`IMFDXGIBuffer` → `IMFSample`），
//!   零拷贝；CPU 帧走 `MFCreateMemoryBuffer` 上传路径；
//! - 低延迟配置满足契约 E1/E2：`CODECAPI_AVLowLatencyMode`、
//!   `b_frames = 0`、无限 GOP 按需 IDR（`CODECAPI_AVEncVideoForceKeyframe`）；
//! - 输出 Annex-B（长度前缀 → 起始码转换见 `annexb` 模块，对应 macOS 同名实现）。
//!
//! # 当前状态
//!
//! 骨架占位：配置校验对齐 core 契约，编码调用返回未实现错误。

#![deny(missing_docs)]
#![forbid(unsafe_code)]

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
        // TODO: MFStartup → MFTEnumEx 选硬件 MFT → 设置输入/输出媒体类型
        // （MF_MT_FRAME_SIZE / low latency / bitrate）→ ProcessMessage 启动。
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
        // TODO: ProcessInput（D3D11 纹理零拷贝）→ ProcessOutput  drain → Annex-B 拼装。
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
