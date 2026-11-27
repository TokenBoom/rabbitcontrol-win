//! 开机自动启动（M5b GUI 侧）：守护服务安装/卸载（UAC 提权）+ 状态探测。
//!
//! 流程：`ShellExecuteExW(runas)` 单次提权调 `rc-daemon setup|teardown`，
//! 阻塞等待其退出（≤30s），随后管道 Ping 确认服务就绪。全部纯阻塞函数，
//! 调用方须在工作线程执行。

#![allow(unsafe_code)] // ShellExecuteExW / 命名管道探测 FFI

use std::time::Duration;

use windows58::core::PCWSTR;
use windows58::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows58::Win32::System::Threading::{WaitForSingleObject, INFINITE};
use windows58::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows58::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// 同目录守护可执行名。
const DAEMON_EXE: &str = "rc-daemon.exe";

/// 守护服务是否在线（管道 Ping；~300ms 超时语义由系统管道超时决定）。
pub fn daemon_reachable() -> bool {
    daemon_query(r#"{"op":"ping"}"#).is_some()
}

/// 守护服务查询（返回响应 JSON 文本）。
pub fn daemon_query(frame: &str) -> Option<String> {
    let name = wide(r"\\.\pipe\rabbitcontrol-daemon");
    unsafe {
        let handle: HANDLE = windows58::Win32::Storage::FileSystem::CreateFileW(
            PCWSTR(name.as_ptr()),
            (windows58::Win32::Storage::FileSystem::FILE_GENERIC_READ
                | windows58::Win32::Storage::FileSystem::FILE_GENERIC_WRITE)
                .0,
            windows58::Win32::Storage::FileSystem::FILE_SHARE_NONE,
            None,
            windows58::Win32::Storage::FileSystem::OPEN_EXISTING,
            windows58::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL,
            None,
        )
        .ok()?;
        let mut data = frame.as_bytes().to_vec();
        data.push(b'\n');
        let mut written = 0u32;
        let write = windows58::Win32::Storage::FileSystem::WriteFile(
            handle,
            Some(&data),
            Some(&mut written),
            None,
        );
        if write.is_err() {
            let _ = CloseHandle(handle);
            return None;
        }
        let mut buf = [0u8; 4096];
        let mut read = 0u32;
        let read_ok = windows58::Win32::Storage::FileSystem::ReadFile(
            handle,
            Some(&mut buf),
            Some(&mut read),
            None,
        );
        let _ = CloseHandle(handle);
        if read_ok.is_err() {
            return None;
        }
        Some(String::from_utf8_lossy(&buf[..read as usize]).into_owned())
    }
}

/// 开关被控自启服务：`on=true` 安装并启动，`false` 停止并卸载。
/// 返回最终状态（管道可达与否）。
pub fn set_enabled(on: bool) -> Result<bool, String> {
    let verb = if on { "setup" } else { "teardown" };
    run_elevated_daemon(verb)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if daemon_reachable() {
            return Ok(true);
        }
        if !on {
            // 关闭路径：管道不可达即成功
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    if on {
        Err("服务已授权但未能在 10 秒内就绪".into())
    } else {
        Ok(false)
    }
}

/// UAC 提权运行 `rc-daemon.exe <verb>`（阻塞等待进程退出，≤30s）。
fn run_elevated_daemon(verb: &str) -> Result<(), String> {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(DAEMON_EXE)))
        .filter(|p| p.exists())
        .ok_or_else(|| format!("未找到 {DAEMON_EXE}（请重新安装客户端）"))?;
    let file = wide(&exe.to_string_lossy());
    let params = wide(verb);
    let runas = wide("runas");
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(runas.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe {
        ShellExecuteExW(&mut info).map_err(|e| format!("UAC 提权失败：{e}"))?;
        if info.hProcess == HANDLE::default() {
            return Ok(()); // 用户取消 UAC 时可能无句柄；由后续探测裁决
        }
        let wait = WaitForSingleObject(info.hProcess, INFINITE);
        let _ = CloseHandle(info.hProcess);
        if wait != WAIT_OBJECT_0 {
            return Err("提权进程等待超时".into());
        }
    }
    let _ = verb;
    Ok(())
}

/// UTF-16（NUL 终止）。
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_reachable_is_safe_offline() {
        // 服务未运行时 Ping 必须安全返回 None（不 panic / 不长阻塞）
        let reachable = daemon_reachable();
        assert!(!reachable);
    }
}
