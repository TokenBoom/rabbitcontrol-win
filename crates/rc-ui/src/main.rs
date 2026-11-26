// #![windows_subsystem = "windows"] // 调试：启用控制台

mod app;
mod biz;
#[allow(dead_code)] // M5a 接线后移除
mod media;
mod ui;

use std::error::Error;

use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, MessageBoxW, SetForegroundWindow, ShowWindow, MB_ICONERROR, SW_RESTORE,
};

use windows::core::{HSTRING, PCWSTR};

/// 主窗口类名（二次启动时用于唤起已有实例）。
const WINDOW_CLASS: &str = "RabbitControlWindow";

/// 进程级单实例守卫：已存在实例时唤起其窗口并返回 false。
fn acquire_single_instance() -> bool {
    let class = HSTRING::from(WINDOW_CLASS);
    let existing = unsafe { FindWindowW(PCWSTR(class.as_wide().as_ptr()), None) };
    if existing.0 != 0 {
        // 已有实例：激活窗口并退出。
        unsafe {
            let _ = ShowWindow(existing, SW_RESTORE);
            let _ = SetForegroundWindow(existing);
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
    let shell = ui::shell::Shell::new()?;
    let mut app = app::App::new(&shell)?;
    let _ = shell.show_and_run(|| app.dispatch());
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
