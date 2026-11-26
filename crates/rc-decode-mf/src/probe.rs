//! 解码器能力探测：枚举 `MFT_CATEGORY_VIDEO_DECODER`（硬件在前）。

use rc_win32core::mf::{enum_all_sorted, enumerate_video_mfts, MftInfo};
use windows::Win32::Media::MediaFoundation::MFT_CATEGORY_VIDEO_DECODER;

/// 枚举本机全部视频解码 MFT（按 T-rating 排序，硬件优先）。
pub fn probe_decoders() -> std::result::Result<Vec<MftInfo>, String> {
    enumerate_video_mfts(MFT_CATEGORY_VIDEO_DECODER, enum_all_sorted())
}
