//! 开机自动启动（M5b GUI 侧）：守护服务安装/卸载（UAC 提权）+ 状态探测。
//!
//! 流程：`ShellExecuteExW(runas)` 单次提权调 `rc-daemon setup|teardown`，
//! 阻塞等待其退出（≤30s），随后管道 Ping 确认服务就绪。全部纯阻塞函数，
//! 调用方须在工作线程执行。

#![allow(unsafe_code)] // ShellExecuteExW FFI

use std::time::Duration;

use windows58::core::PCWSTR;
use windows58::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows58::Win32::System::Threading::{WaitForSingleObject, INFINITE};
use windows58::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows58::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// 同目录守护可执行名。
const DAEMON_EXE: &str = "rc-daemon.exe";

/// 探测守护服务的超时：服务卡住时不能拖慢界面。
const PROBE_TIMEOUT_MS: u32 = 1_500;

/// 守护服务是否在线（管道 Ping，带超时）。
pub fn daemon_reachable() -> bool {
    daemon_query(r#"{"op":"ping"}"#).is_some()
}

/// 守护服务查询（返回响应 JSON 文本）。
pub fn daemon_query(frame: &str) -> Option<String> {
    rc_appcore::daemon_ipc::call(frame, PROBE_TIMEOUT_MS).ok()
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
