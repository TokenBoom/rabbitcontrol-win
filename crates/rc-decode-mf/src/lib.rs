//! RabbitControl Windows 硬件视频解码器（Media Foundation / D3D11VA）。
//!
//! # 设计
//!
//! - **MFT 选择**：`MFTEnumEx` 枚举 `MFT_CATEGORY_VIDEO_DECODER`，优先
//!   支持 D3D11VA 的硬件解码 MFT；`MF_SA_D3D11_AWARE` + 共享设备
//!   [`rc_d3d11::SharedDevice`] 的 `IMFDXGIDeviceManager` 传入，解码输出
//!   直接落在共享设备的 `ID3D11Texture2D` 上，渲染零拷贝。
//! - **低延迟**：`MF_LOW_LATENCY` + `CODECAPI_AVDecVideoMaxNumRefFrame`
//!   压低参考帧；Annex-B 输入与 `rc-encode-mf` 输出对接。
//! - **错误恢复**：码流损坏/参考缺失时上报 `needs_keyframe`，由会话层
//!   回源请求 IDR。
//!
//! # 当前状态
//!
//! 骨架占位：解码调用返回未实现错误。

#![deny(missing_docs)]
#![forbid(unsafe_code)]

use rc_decode::{DecodeOutput, Decoder};
use rc_encode::Codec;
use rc_types::{Error, Resolution, Result, Timestamp};

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
        // TODO: MFStartup → MFTEnumEx 选 D3D11VA 硬解 MFT →
        // IMFDXGIDeviceManager 绑定 rc_d3d11::SharedDevice →
        // 输入 Annex-B / 输出 NV12-on-D3D11 媒体类型。
        Ok(Self {
            cfg,
            waiting_keyframe: true,
        })
    }
}

/// 构造解码器。
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
        // TODO: ProcessInput → ProcessOutput 循环，输出共享设备纹理帧。
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
