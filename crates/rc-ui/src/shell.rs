use std::{
    ptr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use windows::{
    core::{Error, Interface, Result, HSTRING, PCWSTR},
    Foundation::{Point, PropertyValue},
    Win32::{
        Foundation::{BOOL, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::{
            GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, MONITORINFO,
            MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
        },
        System::{LibraryLoader::GetModuleHandleW, WinRT::Xaml::IDesktopWindowXamlSourceNative2},
        UI::{Input::KeyboardAndMouse::GetDoubleClickTime, WindowsAndMessaging::*},
    },
    UI::Xaml::{
        Automation::AutomationProperties,
        Controls::{ContentControl, Primitives::ButtonBase},
        FrameworkElement,
        Hosting::{DesktopWindowXamlSource, WindowsXamlManager},
        Input::PointerEventHandler,
        RoutedEventHandler, SizeChangedEventHandler, UIElement,
    },
};

pub const UI_EVENT: u32 = WM_APP + 1;

const DEFAULT_WIDTH: i32 = 960;
const DEFAULT_HEIGHT: i32 = 620;

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
            fit_island(window.0, island, true)?;
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

#[derive(Clone, Copy)]
struct DragState {
    start_cursor: POINT,
    start_window: (i32, i32),
    last_pos: (i32, i32),
    was_maximized: bool,
    restore_ratio_x: f64,
}

// 使用指针捕获与屏幕坐标直移替代系统模态移动循环，避免与 XAML Island 消息泵冲突导致卡顿或失效。
fn attach_caption(root: &FrameworkElement, hwnd: HWND) -> Result<()> {
    let drag: UIElement = root.FindName("DragRegion")?.cast()?;
    let drag_state: Arc<Mutex<Option<DragState>>> = Arc::new(Mutex::new(None));
    let last_click: Arc<Mutex<Option<(Instant, Point)>>> = Arc::new(Mutex::new(None));

    let press_drag = drag.clone();
    let press_state = drag_state.clone();
    let press_click = last_click.clone();
    drag.PointerPressed(PointerEventHandler::new(move |_, args| {
        if let Some(args) = args {
            let point = args.GetCurrentPoint(None)?;
            if point.Properties()?.IsLeftButtonPressed()? {
                let now = Instant::now();
                let pos = point.Position()?;
                let dbl_window = Duration::from_millis(u64::from(unsafe { GetDoubleClickTime() }));
                let mut click_guard = press_click.lock().unwrap();
                let is_double_click = matches!(
                    *click_guard,
                    Some((prev_time, prev_pos))
                        if now.duration_since(prev_time) <= dbl_window
                            && (pos.X - prev_pos.X).abs().max((pos.Y - prev_pos.Y).abs()) <= 4.0
                );
                args.SetHandled(true)?;
                if is_double_click {
                    *click_guard = None;
                    *press_state.lock().unwrap() = None;
                    window_command(hwnd, SC_MAXIMIZE);
                } else {
                    *click_guard = Some((now, pos));
                    let mut start_cursor = POINT::default();
                    let mut rect = RECT::default();
                    let ok = unsafe {
                        GetCursorPos(&mut start_cursor).as_bool()
                            && GetWindowRect(hwnd, &mut rect).as_bool()
                    };
                    if ok {
                        let win_w = (rect.right - rect.left).max(1) as f64;
                        let ratio = ((start_cursor.x - rect.left) as f64 / win_w).clamp(0.1, 0.9);
                        let pointer = args.Pointer()?;
                        let _ = press_drag.CapturePointer(&pointer);
                        *press_state.lock().unwrap() = Some(DragState {
                            start_cursor,
                            start_window: (rect.left, rect.top),
                            last_pos: (rect.left, rect.top),
                            was_maximized: unsafe { IsZoomed(hwnd).as_bool() },
                            restore_ratio_x: ratio,
                        });
                    }
                }
            }
        }
        Ok(())
    }))?;

    let move_drag = drag.clone();
    let move_state = drag_state.clone();
    let move_click = last_click;
    drag.PointerMoved(PointerEventHandler::new(move |_, args| {
        let mut guard = move_state.lock().unwrap();
        let Some(state) = *guard else {
            return Ok(());
        };
        if let Some(args) = args {
            let point = args.GetCurrentPoint(None)?;
            if !point.Properties()?.IsLeftButtonPressed()? {
                *guard = None;
                let _ = move_drag.ReleasePointerCaptures();
                return Ok(());
            }
            let mut cur = POINT::default();
            if !unsafe { GetCursorPos(&mut cur).as_bool() } {
                return Ok(());
            }
            let dx = cur.x - state.start_cursor.x;
            let dy = cur.y - state.start_cursor.y;
            if state.was_maximized {
                if dx.abs().max(dy.abs()) < 4 {
                    return Ok(());
                }
                *move_click.lock().unwrap() = None;
                unsafe { ShowWindow(hwnd, SW_RESTORE) };
                let mut restored = RECT::default();
                if unsafe { GetWindowRect(hwnd, &mut restored).as_bool() } {
                    let restored_w = (restored.right - restored.left).max(1) as f64;
                    let new_x = cur.x - (restored_w * state.restore_ratio_x) as i32;
                    let new_y = cur.y - 18;
                    unsafe {
                        SetWindowPos(
                            hwnd,
                            None,
                            new_x,
                            new_y,
                            0,
                            0,
                            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                        );
                    }
                    *guard = Some(DragState {
                        start_cursor: cur,
                        start_window: (new_x, new_y),
                        last_pos: (new_x, new_y),
                        was_maximized: false,
                        restore_ratio_x: state.restore_ratio_x,
                    });
                }
            } else {
                let new_x = state.start_window.0 + dx;
                let new_y = state.start_window.1 + dy;
                if (new_x, new_y) != state.last_pos {
                    *move_click.lock().unwrap() = None;
                    unsafe {
                        SetWindowPos(
                            hwnd,
                            None,
                            new_x,
                            new_y,
                            0,
                            0,
                            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                        );
                    }
                    *guard = Some(DragState {
                        last_pos: (new_x, new_y),
                        ..state
                    });
                }
            }
            args.SetHandled(true)?;
        }
        Ok(())
    }))?;

    let release_drag = drag.clone();
    let release_state = drag_state.clone();
    drag.PointerReleased(PointerEventHandler::new(move |_, _| {
        *release_state.lock().unwrap() = None;
        let _ = release_drag.ReleasePointerCaptures();
        Ok(())
    }))?;

    let lost_state = drag_state;
    drag.PointerCaptureLost(PointerEventHandler::new(move |_, _| {
        *lost_state.lock().unwrap() = None;
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
                hIcon: LoadIconW(instance, PCWSTR(1 as *const u16)).unwrap_or_default(),
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 {
                return Err(Error::from_win32());
            }
            let (x, y) = centered_origin(DEFAULT_WIDTH, DEFAULT_HEIGHT);
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
                x,
                y,
                DEFAULT_WIDTH,
                DEFAULT_HEIGHT,
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

unsafe fn centered_origin(width: i32, height: i32) -> (i32, i32) {
    let mut cursor = POINT::default();
    let _ = GetCursorPos(&mut cursor);
    let monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTOPRIMARY);
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(monitor, &mut info).as_bool() {
        let work_w = info.rcWork.right - info.rcWork.left;
        let work_h = info.rcWork.bottom - info.rcWork.top;
        return (
            info.rcWork.left + ((work_w - width) / 2).max(0),
            info.rcWork.top + ((work_h - height) / 2).max(0),
        );
    }
    (CW_USEDEFAULT, CW_USEDEFAULT)
}

impl Drop for Window {
    fn drop(&mut self) {
        unsafe { DestroyWindow(self.0) };
    }
}

unsafe fn fit_island(parent: HWND, island: HWND, show: bool) -> Result<()> {
    let mut rect = RECT::default();
    GetClientRect(parent, &mut rect).ok()?;
    let mut flags = SWP_NOACTIVATE | SWP_NOZORDER;
    if show {
        flags |= SWP_SHOWWINDOW;
    }
    SetWindowPos(island, None, 0, 0, rect.right, rect.bottom, flags).ok()
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => return LRESULT(1),
        WM_GETMINMAXINFO => {
            let result = DefWindowProcW(hwnd, msg, w, l);
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
            let params = l.0 as *mut NCCALCSIZE_PARAMS;
            if IsZoomed(hwnd).as_bool() {
                // 最大化时将客户区严格限定在工作区，避免边框溢出屏幕。
                let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
                let mut info = MONITORINFO {
                    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                if GetMonitorInfoW(monitor, &mut info).as_bool() {
                    (*params).rgrc[0] = info.rcWork;
                }
                return LRESULT(0);
            }
            // 保留左右下原生缩放边框，仅取消顶部标题栏留白。
            let top = (*params).rgrc[0].top;
            DefWindowProcW(hwnd, msg, w, l);
            (*params).rgrc[0].top = top + 1;
            return LRESULT(0);
        }
        WM_SIZE => {
            let island = HWND(GetWindowLongPtrW(hwnd, GWLP_USERDATA));
            if island.0 != 0 {
                let _ = fit_island(hwnd, island, false);
            }
        }
        WM_DPICHANGED => {
            // 跨显示器拖动时采用系统建议窗口矩形，保持物理尺寸一致。
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
