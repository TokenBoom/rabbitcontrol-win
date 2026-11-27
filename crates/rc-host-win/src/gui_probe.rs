//! 主界面是否在运行的探测，以及从托盘打开主界面。
//!
//! 「主界面在运行」以它的顶层窗口为准（类名 [`GUI_WINDOW_CLASS`]，与 `rc-ui`
//! 共用同一个常量）。显式拿到主界面 PID 时再叠加进程句柄等待，启动早期窗口
//! 还没建出来也能正确判定。

use std::os::windows::process::CommandExt;
use std::time::{Duration, Instant};

use rc_appcore::agent_ipc::GUI_WINDOW_CLASS;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HANDLE, HWND, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetWindowThreadProcessId, SetForegroundWindow, ShowWindow, SW_RESTORE,
};

use crate::engine::GuiProbe;
use crate::win::wide;

/// 主界面可执行文件名（与本进程同目录）。
pub const GUI_EXE: &str = "rabbitcontrol-win.exe";

/// 主界面进程刚启动时窗口可能还没建出来：这段时间只看进程是否存活。
const WINDOW_GRACE: Duration = Duration::from_secs(15);

/// 按类名找一个顶层窗口（本会话的桌面）。
pub fn find_window(class: &str) -> Option<HWND> {
    let class = wide(class);
    let hwnd = unsafe { FindWindowW(PCWSTR(class.as_ptr()), PCWSTR::null()) }.ok()?;
    (!hwnd.0.is_null()).then_some(hwnd)
}

/// 找主界面的顶层窗口。
pub fn find_gui_window() -> Option<HWND> {
    find_window(GUI_WINDOW_CLASS)
}

/// 主界面进程 PID（以窗口为准；没开则为 `None`）。
pub fn gui_pid() -> Option<u32> {
    let hwnd = find_gui_window()?;
    let mut pid = 0u32;
    let thread = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    (thread != 0 && pid != 0).then_some(pid)
}

/// 通用探测：每次调用按窗口类名找一遍（主界面可能随时启动或退出）。
pub fn window_probe() -> GuiProbe {
    Box::new(|| find_gui_window().is_some())
}

/// 盯住给定进程：它退出后返回 `false`。
///
/// 启动后的 [`WINDOW_GRACE`] 内只看进程是否存活（窗口可能还没建出来）；之后
/// 还要求窗口存在——万一 PID 被别的进程复用，也不会误以为主界面还在。
pub fn process_probe(pid: u32) -> GuiProbe {
    // HANDLE 不是 Send：以裸值持有，进程存续期间有效
    let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }
        .ok()
        .map(|h| h.0 as isize);
    let started = Instant::now();
    Box::new(move || {
        let Some(raw) = raw else {
            return false;
        };
        let alive = unsafe { WaitForSingleObject(HANDLE(raw as *mut core::ffi::c_void), 0) }
            != WAIT_OBJECT_0;
        alive && (started.elapsed() < WINDOW_GRACE || find_gui_window().is_some())
    })
}

/// 打开或激活主界面。
pub fn open_main_gui() -> Result<(), String> {
    if let Some(hwnd) = find_gui_window() {
        unsafe {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
        }
        return Ok(());
    }
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(GUI_EXE)))
        .ok_or_else(|| "定位主界面程序失败".to_string())?;
    if !exe.exists() {
        return Err(format!("主界面程序不存在：{}", exe.display()));
    }
    // 后台进程没有控制台：让主界面独立于本进程的生存期与标准句柄
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    let mut command = std::process::Command::new(&exe);
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    command
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("启动主界面失败：{err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probes_never_panic_when_no_gui_is_running() {
        // 测试环境下不假设主界面在或不在：只验证探测函数可反复调用且返回 bool
        let mut probe = window_probe();
        let _ = probe();
        let _ = probe();
    }

    #[test]
    fn a_dead_process_pid_makes_the_probe_false() {
        // PID 0（System Idle Process）打不开同步句柄：视为「不在运行」
        let mut probe = process_probe(0);
        assert!(!probe());
    }
}
