#![windows_subsystem = "windows"]

mod desktop;
mod graphics;
mod shell;

use std::error::Error;
use windows::{
    core::Interface,
    Win32::{
        System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_SINGLETHREADED},
        UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR},
    },
    UI::Xaml::{
        Controls::{SwapChainPanel, TextBlock},
        FrameworkElement,
        Markup::XamlReader,
    },
};

fn run() -> Result<(), Box<dyn Error>> {
    let desktop = desktop::initial_monitor()?;
    let (width, height) = desktop.dimensions();
    let shell = shell::Shell::new()?;
    let root: FrameworkElement = XamlReader::Load(include_str!("view.xaml"))?.cast()?;
    let panel: SwapChainPanel = root.FindName("Framebuffer")?.cast()?;
    let bounds: FrameworkElement = panel.cast()?;
    bounds.SetWidth(width.into())?;
    bounds.SetHeight(height.into())?;
    let size: TextBlock = root.FindName("DesktopSize")?.cast()?;
    size.SetText(format!("{width} × {height}").as_str())?;
    shell.set_content(&root)?;
    let surface = graphics::Surface::new(&panel, width, height)?;
    surface.present_idle()?;
    shell.show_and_run()?;
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
