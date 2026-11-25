use std::ptr;
use windows::{
    core::{Error, Interface, Result, HSTRING, PCWSTR},
    Win32::{
        Foundation::{BOOL, HWND, LPARAM, LRESULT, RECT, WPARAM},
        System::{LibraryLoader::GetModuleHandleW, WinRT::Xaml::IDesktopWindowXamlSourceNative2},
        UI::WindowsAndMessaging::*,
    },
    UI::Xaml::{
        FrameworkElement,
        Hosting::{DesktopWindowXamlSource, WindowsXamlManager},
    },
};

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
        self.source.SetContent(root)
    }

    pub fn show_and_run(&self) -> Result<()> {
        unsafe {
            ShowWindow(self.window.0, SW_SHOWNORMAL);
            let mut msg = MSG::default();
            loop {
                match GetMessageW(&mut msg, None, 0, 0).0 {
                    -1 => return Err(Error::from_win32()),
                    0 => return Ok(()),
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
                WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
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
