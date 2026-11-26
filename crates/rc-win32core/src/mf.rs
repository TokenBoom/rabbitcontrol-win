//! Media Foundation 轻量设施：MF 运行时守卫与视频 MFT 枚举。
//!
//! 仅覆盖探测/能力上报所需的最小面（`MFStartup` / `MFTEnumEx`）；
//! 编码器/解码器会话生命周期由 `rc-encode-mf` / `rc-decode-mf` 自持。
//!
//! 安全性：`unsafe` 仅限 windows-rs 投影调用点；`MFTEnumEx` 返回的
//! activate 数组与 `GetAllocatedString` 分配的字符串缓冲都以
//! `CoTaskMemFree` 释放，无泄漏路径。

#![allow(unsafe_code)]

use windows::core::{GUID, PWSTR};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, MFShutdown, MFStartup, MFTEnumEx, MFT_ENUM_HARDWARE_URL_Attribute,
    MFT_FRIENDLY_NAME_Attribute, MFSTARTUP_LITE, MFT_ENUM_FLAG, MFT_ENUM_FLAG_ASYNCMFT,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT, MF_VERSION,
};
use windows::Win32::System::Com::CoTaskMemFree;

/// 全量枚举标志（同步 + 异步 + 硬件 MFT，按 T-rating 优先排序，硬件在前）。
///
/// `MFT_ENUM_FLAG` 的 `BitOr` 非 const，故以函数提供。
pub fn enum_all_sorted() -> MFT_ENUM_FLAG {
    MFT_ENUM_FLAG_SYNCMFT
        | MFT_ENUM_FLAG_ASYNCMFT
        | MFT_ENUM_FLAG_HARDWARE
        | MFT_ENUM_FLAG_SORTANDFILTER
}

/// Media Foundation 运行时守卫：构造时 `MFStartup`，Drop 时 `MFShutdown`。
pub struct MfGuard;

impl MfGuard {
    /// 初始化 Media Foundation（`MFSTARTUP_LITE`，不访问网络注册表）。
    pub fn startup() -> std::result::Result<Self, String> {
        let hr = unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) };
        if hr.is_err() {
            return Err(format!("MFStartup 失败：{hr:?}"));
        }
        Ok(Self)
    }
}

impl Drop for MfGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
        }
    }
}

/// 单个视频 MFT 的探测信息。
#[derive(Debug, Clone)]
pub struct MftInfo {
    /// 友好名称（如 "NVIDIA H.264/HEVC Encoder MFT"）。
    pub friendly_name: String,
    /// 是否硬件（驱动内）MFT。
    pub hardware: bool,
}

/// 枚举指定类别的视频 MFT。
///
/// `category` 传 `MFT_CATEGORY_VIDEO_ENCODER` / `MFT_CATEGORY_VIDEO_DECODER`。
pub fn enumerate_video_mfts(
    category: GUID,
    flags: MFT_ENUM_FLAG,
) -> std::result::Result<Vec<MftInfo>, String> {
    let mut act_ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    let hr = unsafe { MFTEnumEx(category, flags, None, None, &mut act_ptr, &mut count) };
    if let Err(e) = hr {
        return Err(format!("MFTEnumEx 失败：{e:?}"));
    }
    if act_ptr.is_null() || count == 0 {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(count as usize);
    for index in 0..count as usize {
        // 数组槽位是 COM 指针，Clone 会 AddRef 出独立所有权，
        // 原数组随后整体 CoTaskMemFree，不会双重释放。
        let slot = unsafe { act_ptr.add(index) };
        let Some(activate) = (unsafe { (*slot).clone() }) else {
            continue;
        };
        out.push(read_activate_info(&activate));
    }
    unsafe { CoTaskMemFree(Some(act_ptr.cast())) };
    Ok(out)
}

fn read_activate_info(activate: &IMFActivate) -> MftInfo {
    let friendly_name = allocated_string(activate, &MFT_FRIENDLY_NAME_Attribute)
        .unwrap_or_else(|| "<未命名 MFT>".into());
    let hardware = allocated_string(activate, &MFT_ENUM_HARDWARE_URL_Attribute).is_some();
    MftInfo {
        friendly_name,
        hardware,
    }
}

fn allocated_string(activate: &IMFActivate, key: &GUID) -> Option<String> {
    let mut buffer = PWSTR::null();
    let mut length = 0u32;
    let hr = unsafe { activate.GetAllocatedString(key, &mut buffer, &mut length) };
    if hr.is_err() || buffer.is_null() {
        return None;
    }
    let text = unsafe { buffer.to_string().unwrap_or_default() };
    unsafe { CoTaskMemFree(Some(buffer.as_ptr().cast())) };
    Some(text)
}
