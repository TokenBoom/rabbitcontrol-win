#![windows_subsystem = "windows"]

mod app;
mod auth;
mod desktop;
mod graphics;
mod settings;
mod shell;

use std::error::Error;
use windows::Win32::{
    System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_SINGLETHREADED},
    UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR},
};

fn run() -> Result<(), Box<dyn Error>> {
    rc_platform_win::register_all();
    let shell = shell::Shell::new()?;
    let mut app = app::App::new(&shell)?;
    shell.show_and_run(|| app.dispatch())?;
    Ok(())
}

fn main() {
    let result = unsafe { RoInitialize(RO_INIT_SINGLETHREADED) }
        .map_err(Box::<dyn Error>::from)
        .and_then(|()| {
            let result = run();
            unsafe { RoUninitialize() };
            result
        });
    if let Err(error) = result {
        let message = format!("Could not start RabbitControl.\n\n{error}");
        eprintln!("{message}");
        unsafe { MessageBoxW(None, message.as_str(), "RabbitControl", MB_ICONERROR) };
        std::process::exit(1);
    }
}
