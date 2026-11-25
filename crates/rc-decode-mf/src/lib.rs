//! RabbitControl Windows 硬件视频解码器（Media Foundation，对应 `rabbitcontrol-apple-media` 的 VT 解码）。
//!
//! # 设计
//!
//! - `MFTEnumEx` 枚举 `MFT_CATEGORY_VIDEO_DECODER`（H.264/HEVC 硬解 MFT，
//!   走 D3D11VA 输出 `ID3D11Texture2D`）；
//! - 输入 Annex-B 码流（与 `rc-encode-mf` 输出对接）；
//! - 低延迟：`CODECAPI_AVDecVideoMaxNumRefFrame` 压低参考帧数、
//!   `MF_LOW_LATENCY` 属性；
//! - 输出 [`FrameTexture::D3D11`] 交 `rc-render-win` 零拷贝上屏。
//!
//! # 当前状态
//!
//! 骨架占位：解码调用返回未实现错误。

#![deny(missing_docs)]
#![forbid(unsafe_code)]

use rc_decode::{DecodeOutput, Decoder};
use rc_encode::Codec;
use rc_types::{Error, Resolution, Result, Timestamp};

/// MF 解码器配置（对应 `VtDecoderConfig`）。
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

/// Media Foundation 硬件解码器（骨架占位）。
// 字段在 MF 实现落地后投入使用。
#[allow(dead_code)]
pub struct MfDecoder {
    cfg: MfDecoderConfig,
    waiting_keyframe: bool,
}

impl MfDecoder {
    /// 构造解码器。
    pub fn new(cfg: MfDecoderConfig) -> Result<Self> {
        // TODO: MFStartup → MFTEnumEx 选硬解 MFT → 设置输入（ Annex-B ）/
        // 输出（NV12/D3D11）媒体类型 → 启用 D3D11 感知。
        Ok(Self {
            cfg,
            waiting_keyframe: true,
        })
    }
}

/// 构造解码器（对应 macOS `new_decoder` 自由函数）。
pub fn new_decoder(cfg: MfDecoderConfig) -> Result<MfDecoder> {
    MfDecoder::new(cfg)
}

impl Decoder for MfDecoder {
    fn decode(
        &mut self,
        _data: &[u8],
        _kind: rc_types::FrameKind,
        _deadline_ts: Timestamp,
    ) -> Result<DecodeOutput> {
        // TODO: ProcessInput → ProcessOutput 循环，输出 D3D11 纹理帧。
        Err(Error::InvalidConfig(
            "mf decoder not yet implemented".into(),
        ))
    }

    fn needs_keyframe(&self) -> bool {
        self.waiting_keyframe
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}
