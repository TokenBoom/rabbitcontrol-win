#![windows_subsystem = "windows"]

mod app;
mod auth;
#[allow(dead_code)] // M2 会话管线接线后移除
mod desktop;
#[allow(dead_code)] // M2 会话管线接线后移除
mod graphics;
mod history;
mod runtime;
mod session;
mod settings;
mod shell;
mod widgets;

use std::error::Error;

use windows::Win32::{
    Foundation::{GetLastError, ERROR_ALREADY_EXISTS},
    System::Threading::CreateMutexW,
    UI::WindowsAndMessaging::{
        FindWindowW, MessageBoxW, SetForegroundWindow, ShowWindow, MB_ICONERROR, SW_RESTORE,
    },
};

use windows::core::{HSTRING, PCWSTR};

/// 单实例互斥量名（会话级全局）。
const MUTEX_NAME: &str = "Global\\rabbitcontrol-win";
/// 主窗口类名（二次启动时用于唤起已有实例）。
const WINDOW_CLASS: &str = "RabbitControlWindow";

/// 进程级单实例守卫：已存在实例时唤起其窗口并返回 false。
fn acquire_single_instance() -> bool {
    let name = HSTRING::from(MUTEX_NAME);
    // 句柄有意不关闭：互斥量随进程存活到退出。
    let created = unsafe { CreateMutexW(std::ptr::null(), false, PCWSTR(name.as_wide().as_ptr())) };
    let _handle = match created {
        Ok(handle) => handle,
        Err(_) => return true, // 创建失败时放行（宁可双开也不拒绝启动）。
    };
    // HANDLE 为 Copy；内核句柄保持打开，互斥量随进程存活。
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe {
            let class = HSTRING::from(WINDOW_CLASS);
            let existing = FindWindowW(PCWSTR(class.as_wide().as_ptr()), None);
            if existing.0 != 0 {
                let _ = ShowWindow(existing, SW_RESTORE);
                let _ = SetForegroundWindow(existing);
            }
        }
        return false;
    }
    true
}

fn run() -> Result<(), Box<dyn Error>> {
    if !acquire_single_instance() {
        return Ok(());
    }
    rc_platform_win::register_all();
    let shell = shell::Shell::new()?;
    let mut app = app::App::new(&shell)?;
    shell.show_and_run(|| app.dispatch())?;
    Ok(())
}

fn main() {
    let result = unsafe {
        windows::Win32::System::WinRT::RoInitialize(
            windows::Win32::System::WinRT::RO_INIT_SINGLETHREADED,
        )
    }
    .map_err(Box::<dyn Error>::from)
    .and_then(|()| {
        let result = run();
        unsafe { windows::Win32::System::WinRT::RoUninitialize() };
        result
    });
    if let Err(error) = result {
        let message = format!("Could not start RabbitControl.\n\n{error}");
        eprintln!("{message}");
        unsafe { MessageBoxW(None, message.as_str(), "RabbitControl", MB_ICONERROR) };
        std::process::exit(1);
    }
}
