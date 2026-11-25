use std::{
    ptr,
    sync::{Arc, Mutex},
};
use windows::{
    core::{Error, Interface, Result, HSTRING, PCWSTR},
    Foundation::PropertyValue,
    Win32::{
        Foundation::{BOOL, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::{
            GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
        },
        System::{LibraryLoader::GetModuleHandleW, WinRT::Xaml::IDesktopWindowXamlSourceNative2},
        UI::{Input::KeyboardAndMouse::ReleaseCapture, WindowsAndMessaging::*},
    },
    UI::Xaml::{
        Automation::AutomationProperties,
        Controls::{ContentControl, Primitives::ButtonBase},
        FrameworkElement,
        Hosting::{DesktopWindowXamlSource, WindowsXamlManager},
        Input::{DoubleTappedEventHandler, PointerEventHandler},
        RoutedEventHandler, SizeChangedEventHandler, UIElement,
    },
};

pub const UI_EVENT: u32 = WM_APP + 1;

pub struct Shell {
    native: IDesktopWindowXamlSourceNative2,
    source: DesktopWindowXamlSource,
    manager: WindowsXamlManager,
    window: Window,
}

impl Shell {
    pub fn new() -> Result<Self> {
        let window = Window::new()?;
        let manager = WindowsXamlManager::InitializeForCurrentThread()?;
        let source = DesktopWindowXamlSource::new()?;
        let native: IDesktopWindowXamlSourceNative2 = source.cast()?;
        unsafe {
            native.AttachToWindow(window.0)?;
            let island = native.WindowHandle()?;
            SetWindowLongPtrW(window.0, GWLP_USERDATA, island.0);
            fit_island(window.0, island)?;
        }
        Ok(Self {
            native,
            source,
            manager,
            window,
        })
    }

    pub fn set_content(&self, root: &FrameworkElement) -> Result<()> {
        attach_caption(root, self.window.0)?;
        self.source.SetContent(root)
    }

    pub fn hwnd(&self) -> HWND {
        self.window.0
    }

    pub fn show_and_run(&self, mut on_event: impl FnMut() -> Result<()>) -> Result<()> {
        unsafe {
            ShowWindow(self.window.0, SW_SHOWNORMAL);
            let mut msg = MSG::default();
            loop {
                match GetMessageW(&mut msg, None, 0, 0).0 {
                    -1 => return Err(Error::from_win32()),
                    0 => return Ok(()),
                    _ if msg.hwnd == self.window.0 && msg.message == UI_EVENT => on_event()?,
                    _ => {
                        let mut handled = BOOL::default();
                        self.native.PreTranslateMessage(&msg, &mut handled)?;
                        if !handled.as_bool() {
                            TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                        }
                    }
                }
            }
        }
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        unsafe { SetWindowLongPtrW(self.window.0, GWLP_USERDATA, 0) };
        let _ = self.source.Close();
        let _ = self.manager.Close();
    }
}

// The XAML Island receives client input. Hand dragging back to Win32 so the
// window follows the native move loop (including snapping across monitors).
fn attach_caption(root: &FrameworkElement, hwnd: HWND) -> Result<()> {
    let drag: UIElement = root.FindName("DragRegion")?.cast()?;
    let origin = Arc::new(Mutex::new(None));
    let pressed = origin.clone();
    drag.PointerPressed(PointerEventHandler::new(move |_, args| {
        if let Some(args) = args {
            let point = args.GetCurrentPoint(None)?;
            *pressed.lock().unwrap() = if point.Properties()?.IsLeftButtonPressed()? {
                Some(point.Position()?)
            } else {
                None
            };
        }
        Ok(())
    }))?;
    let released = origin.clone();
    drag.PointerReleased(PointerEventHandler::new(move |_, _| {
        *released.lock().unwrap() = None;
        Ok(())
    }))?;
    drag.PointerMoved(PointerEventHandler::new(move |_, args| {
        let start = *origin.lock().unwrap();
        if let (Some(start), Some(args)) = (start, args) {
            let point = args.GetCurrentPoint(None)?;
            let pos = point.Position()?;
            if !point.Properties()?.IsLeftButtonPressed()? {
                *origin.lock().unwrap() = None;
            } else if (pos.X - start.X).abs().max((pos.Y - start.Y).abs()) >= 4.0 {
                *origin.lock().unwrap() = None;
                args.SetHandled(true)?;
                unsafe {
                    ReleaseCapture();
                    PostMessageW(
                        hwnd,
                        WM_SYSCOMMAND,
                        WPARAM((SC_MOVE | HTCAPTION) as usize),
                        LPARAM(GetMessagePos() as isize),
                    );
                }
            }
        }
        Ok(())
    }))?;
    drag.DoubleTapped(DoubleTappedEventHandler::new(move |_, _| {
        window_command(hwnd, SC_MAXIMIZE);
        Ok(())
    }))?;
    for (name, command) in [
        ("Minimize", SC_MINIMIZE),
        ("Maximize", SC_MAXIMIZE),
        ("CloseWindow", SC_CLOSE),
    ] {
        let button: ButtonBase = root.FindName(name)?.cast()?;
        button.Click(RoutedEventHandler::new(move |_, _| {
            window_command(hwnd, command);
            Ok(())
        }))?;
    }
    let maximize: ContentControl = root.FindName("Maximize")?.cast()?;
    root.SizeChanged(SizeChangedEventHandler::new(move |_, _| {
        let (glyph, name) = if unsafe { IsZoomed(hwnd).as_bool() } {
            ("\u{E923}", "还原")
        } else {
            ("\u{E922}", "最大化")
        };
        maximize.SetContent(PropertyValue::CreateString(glyph)?)?;
        AutomationProperties::SetName(&maximize, name)
    }))?;
    Ok(())
}

fn window_command(hwnd: HWND, command: u32) {
    unsafe {
        let command = if command == SC_MAXIMIZE && IsZoomed(hwnd).as_bool() {
            SC_RESTORE
        } else {
            command
        };
        PostMessageW(hwnd, WM_SYSCOMMAND, WPARAM(command as usize), None);
    }
}

struct Window(HWND);

impl Window {
    fn new() -> Result<Self> {
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let name = HSTRING::from("RabbitControlWindow");
            let class = WNDCLASSW {
                hInstance: instance,
                lpszClassName: PCWSTR(name.as_wide().as_ptr()),
                lpfnWndProc: Some(window_proc),
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 {
                return Err(Error::from_win32());
            }
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class.lpszClassName,
                "RabbitControl",
                WS_POPUP
                    | WS_THICKFRAME
                    | WS_SYSMENU
                    | WS_MINIMIZEBOX
                    | WS_MAXIMIZEBOX
                    | WS_CLIPCHILDREN,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                960,
                620,
                None,
                None,
                instance,
                ptr::null(),
            );
            if hwnd.0 == 0 {
                return Err(Error::from_win32());
            }
            Ok(Self(hwnd))
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        unsafe { DestroyWindow(self.0) };
    }
}

unsafe fn fit_island(parent: HWND, island: HWND) -> Result<()> {
    let mut rect = RECT::default();
    GetClientRect(parent, &mut rect).ok()?;
    SetWindowPos(
        island,
        None,
        0,
        0,
        rect.right,
        rect.bottom,
        SWP_NOACTIVATE | SWP_NOZORDER | SWP_SHOWWINDOW,
    )
    .ok()
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_GETMINMAXINFO => {
            let result = DefWindowProcW(hwnd, msg, w, l);
            // Popup-style windows normally maximize over the taskbar.
            let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(monitor, &mut info).as_bool() {
                let limits = &mut *(l.0 as *mut MINMAXINFO);
                limits.ptMaxPosition = POINT {
                    x: info.rcWork.left - info.rcMonitor.left,
                    y: info.rcWork.top - info.rcMonitor.top,
                };
                limits.ptMaxSize = POINT {
                    x: info.rcWork.right - info.rcWork.left,
                    y: info.rcWork.bottom - info.rcWork.top,
                };
            }
            return result;
        }
        WM_NCCALCSIZE if w.0 != 0 => {
            if IsZoomed(hwnd).as_bool() {
                return LRESULT(0);
            }
            // Keep the native resize frame at the sides and bottom. The
            // default top inset would leave an unpainted band above the Island.
            let params = l.0 as *mut NCCALCSIZE_PARAMS;
            let top = (*params).rgrc[0].top;
            DefWindowProcW(hwnd, msg, w, l);
            (*params).rgrc[0].top = top + 1;
            return LRESULT(0);
        }
        WM_SIZE => {
            let island = HWND(GetWindowLongPtrW(hwnd, GWLP_USERDATA));
            if island.0 != 0 {
                let _ = fit_island(hwnd, island);
            }
        }
        WM_DPICHANGED => {
            // Windows supplies a suggested window rectangle for the new monitor DPI.
            let rect = &*(l.0 as *const RECT);
            SetWindowPos(
                hwnd,
                None,
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                SWP_NOACTIVATE | SWP_NOZORDER,
            );
        }
        WM_CLOSE => PostQuitMessage(0),
        _ => return DefWindowProcW(hwnd, msg, w, l),
    }
    LRESULT(0)
}
