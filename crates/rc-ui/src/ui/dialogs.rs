//! Win32 通用对话框（文件选择；模态自跑消息循环，UI 线程直调安全）。

#![allow(unsafe_code)] // Win32 对话框 FFI

use windows::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY, OPENFILENAMEW,
};

/// 单选文件对话框；取消返回 `None`。
pub fn pick_file() -> Option<String> {
    let mut buf = [0u16; 1024];
    // 过滤器为 NUL 分隔的双 NUL 结尾串，手工构造（HSTRING 不支持内嵌 NUL）
    let mut filter: Vec<u16> = "所有文件 (*.*)\0*.*\0\0".encode_utf16().collect();
    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: windows::Win32::Foundation::HWND::default(),
        lpstrFilter: windows::core::PCWSTR(filter.as_mut_ptr()),
        lpstrFile: windows::core::PWSTR(buf.as_mut_ptr()),
        nMaxFile: buf.len() as u32,
        Flags: OFN_FILEMUSTEXIST | OFN_HIDEREADONLY,
        ..Default::default()
    };
    unsafe {
        if GetOpenFileNameW(&mut ofn).as_bool() {
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            Some(String::from_utf16_lossy(&buf[..end]))
        } else {
            None
        }
    }
}
