//! RabbitControl Windows 剪贴板后端（Win32 剪贴板直读直写）。
//!
//! # 设计说明（相对方案 v2 的简化）
//!
//! core 的 [`ClipboardBackend`] 是**轮询契约**（`change_count` 由会话线程按
//! 100ms 节流调用），而 `GetClipboardSequenceNumber` / `OpenClipboard` /
//! `GetClipboardData` 是纯 User32 API——**无 COM 套间要求**、可安全在会话线程
//! 直接调用。方案中的「STA 代理线程 + message-only 窗口」是为事件驱动
//! （`WM_CLIPBOARDUPDATE`）设计准备的；轮询契约下属过度设计，故不做代理线程。
//!
//! - 变更检测：`GetClipboardSequenceNumber`（零成本，无需打开剪贴板）；
//! - 文本读写：`OpenClipboard` + `CF_UNICODETEXT`；`OpenClipboard` 有界重试
//!   （剪贴板被其它进程短时占用属常态），重试耗尽按「无文本」处理不阻塞会话线程；
//! - 写入：`EmptyClipboard` + `SetClipboardData`（`GMEM_MOVEABLE` 句柄所有权
//!   移交系统；失败时 `GlobalFree` 自回收）；
//! - 回环抑制、65535 字节上限与 100ms 节流由 core 剪贴板通道负责（铁律 4）。

#![deny(missing_docs)]
#![allow(unsafe_code)] // 本 crate 整体为 Win32 剪贴板 FFI 边界

use std::sync::Arc;
use std::time::Duration;

use rc_session::ClipboardBackend;
use rc_types::{Error, Result};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber, OpenClipboard,
    SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::System::Ole::CF_UNICODETEXT;

/// `OpenClipboard` 有界重试上界（被占用属常态，不无限等待）。
const OPEN_RETRIES: u32 = 8;
/// 每次重试间隔。
const OPEN_RETRY_INTERVAL: Duration = Duration::from_millis(2);

/// Win32 剪贴板后端（无状态：变更计数来自系统全局序号）。
#[derive(Debug, Default)]
pub struct WinClipboard;

impl WinClipboard {
    /// 创建后端实例。
    pub fn new() -> Result<Self> {
        Ok(Self)
    }
}

/// 有界重试打开剪贴板；耗尽返回 `false`（调用方按无文本/忙碌降级）。
fn open_clipboard_bounded() -> bool {
    for _ in 0..OPEN_RETRIES {
        if unsafe { OpenClipboard(None) }.is_ok() {
            return true;
        }
        std::thread::sleep(OPEN_RETRY_INTERVAL);
    }
    false
}

impl ClipboardBackend for WinClipboard {
    fn change_count(&mut self) -> Result<u64> {
        Ok(unsafe { GetClipboardSequenceNumber() } as u64)
    }

    fn read_text(&mut self) -> Result<Option<String>> {
        if !open_clipboard_bounded() {
            // 板被其它进程长时间占用：按「当前无文本」降级，节流后 core 会再轮询
            return Ok(None);
        }
        let outcome = unsafe { read_locked_text() };
        let _ = unsafe { CloseClipboard() };
        outcome
    }

    fn write_text(&mut self, text: &str) -> Result<()> {
        if !open_clipboard_bounded() {
            return Err(Error::Io(std::io::Error::other(
                "OpenClipboard: 有界重试耗尽",
            )));
        }
        let outcome = unsafe { write_locked_text(text) };
        let _ = unsafe { CloseClipboard() };
        outcome
    }
}

/// 前置条件：剪贴板已打开。读取 `CF_UNICODETEXT`（无该 flavor / 非 UTF-16 → `None`）。
unsafe fn read_locked_text() -> Result<Option<String>> {
    // 无该 flavor（图片/文件/空板，CB7）时 GetClipboardData 返回 Err
    let Ok(handle) = (unsafe { GetClipboardData(CF_UNICODETEXT.0 as u32) }) else {
        return Ok(None);
    };
    let hglb = HGLOBAL(handle.0);
    let units = unsafe { GlobalSize(hglb) } / 2; // UTF-16 单元上界（含 NUL）
    if units == 0 {
        return Ok(None);
    }
    let ptr = unsafe { GlobalLock(hglb) };
    if ptr.is_null() {
        return Err(Error::Io(std::io::Error::other("GlobalLock 失败")));
    }
    let slice = std::slice::from_raw_parts(ptr.cast::<u16>(), units);
    let len = slice.iter().position(|&c| c == 0).unwrap_or(units);
    let text = String::from_utf16_lossy(&slice[..len]);
    let _ = unsafe { GlobalUnlock(hglb) };
    Ok(Some(text))
}

/// 前置条件：剪贴板已打开。覆盖写入 UTF-16 文本（句柄所有权移交系统）。
unsafe fn write_locked_text(text: &str) -> Result<()> {
    let mut units: Vec<u16> = text.encode_utf16().collect();
    units.push(0); // NUL 终止
    let byte_len = units.len() * 2;

    unsafe { EmptyClipboard() }
        .map_err(|_| Error::Io(std::io::Error::other("EmptyClipboard 失败")))?;
    let Ok(hglb) = (unsafe { GlobalAlloc(GMEM_MOVEABLE, byte_len) }) else {
        return Err(Error::Io(std::io::Error::other("GlobalAlloc 失败")));
    };
    let ptr = unsafe { GlobalLock(hglb) };
    if ptr.is_null() {
        let _ = unsafe { GlobalFree(hglb) };
        return Err(Error::Io(std::io::Error::other("GlobalLock 失败")));
    }
    unsafe {
        std::ptr::copy_nonoverlapping(units.as_ptr().cast::<u8>(), ptr.cast::<u8>(), byte_len)
    };
    let _ = unsafe { GlobalUnlock(hglb) };
    // SetClipboardData 成功后句柄所有权移交系统，不得 GlobalFree
    if unsafe { SetClipboardData(CF_UNICODETEXT.0 as u32, HANDLE(hglb.0)) }.is_err() {
        let _ = unsafe { GlobalFree(hglb) };
        return Err(Error::Io(std::io::Error::other("SetClipboardData 失败")));
    }
    Ok(())
}

/// 注册 Win32 剪贴板提供者（幂等）。
pub fn register() -> bool {
    rc_session::register_clipboard_provider(Arc::new(|| {
        WinClipboard::new().map(|c| Box::new(c) as Box<dyn ClipboardBackend>)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 交互会话可用性探测：锁屏 / 非交互窗口站下剪贴板不可用，测试降级跳过。
    fn clipboard_available() -> bool {
        let mut backend = WinClipboard::new().expect("构造后端");
        backend.change_count().is_ok()
            && open_clipboard_bounded()
            && unsafe { CloseClipboard() }.is_ok()
    }

    #[test]
    fn change_count_is_readable_and_monotonic() {
        let mut backend = WinClipboard::new().expect("构造后端");
        let a = backend.change_count().expect("读取变更计数");
        let b = backend.change_count().expect("再次读取变更计数");
        assert!(b >= a);
    }

    #[test]
    fn text_roundtrip_restores_clipboard() {
        if !clipboard_available() {
            eprintln!("剪贴板不可用（锁屏/非交互会话），跳过往返测试");
            return;
        }
        let mut backend = WinClipboard::new().expect("构造后端");
        // 保存现场 → 写入已知文本 → 读回校验 → 恢复现场
        let saved = backend.read_text().expect("读取现场");
        backend
            .write_text("RabbitControl 剪贴板往返测试 ✅")
            .expect("写入");
        let read = backend.read_text().expect("读回");
        assert_eq!(read.as_deref(), Some("RabbitControl 剪贴板往返测试 ✅"));
        match saved {
            Some(prev) => backend.write_text(&prev).expect("恢复现场"),
            // 原本无文本：写回空串（clear-only 语义允许）
            None => backend.write_text("").expect("清空恢复"),
        }
    }
}
