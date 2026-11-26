use std::{
    ptr,
    sync::{mpsc, Arc},
};
use windows::{
    core::{Error, Interface, Result, HSTRING, PCWSTR},
    Win32::{
        Foundation::{BOOL, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::{
            GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, MONITORINFO,
            MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
        },
        System::{LibraryLoader::GetModuleHandleW, WinRT::Xaml::IDesktopWindowXamlSourceNative2},
        UI::{HiDpi::GetDpiForWindow, WindowsAndMessaging::*},
    },
    UI::Xaml::{
        Controls::Primitives::ButtonBase,
        FrameworkElement,
        Hosting::{DesktopWindowXamlSource, WindowsXamlManager},
        Input::PointerEventHandler,
        UIElement,
    },
};

pub const UI_EVENT: u32 = WM_APP + 1;

/// 设计基准逻辑尺寸（含标题栏）。布局按此坐标书写；窗口物理尺寸 =
/// 逻辑尺寸 × DPI × 小屏适配系数（由 App 在 DPI 变化时重设）。
pub const DESIGN_WIDTH: f64 = 1000.0;
pub const DESIGN_HEIGHT: f64 = 660.0;
const TIMER_ID: usize = 1;
/// UI 轮询周期（信令事件/在线状态/动效驱动）。
pub const TIMER_PERIOD_MS: u32 = 50;

pub struct Shell {
    native: IDesktopWindowXamlSourceNative2,
    source: DesktopWindowXamlSource,
    manager: WindowsXamlManager,
    window: Window,
    hwnd: HWND,
    events_tx: mpsc::Sender<()>,
}

impl Shell {
    pub fn new() -> Result<Self> {
        let window = Window::new()?;
        let manager = WindowsXamlManager::InitializeForCurrentThread()?;
        let source = DesktopWindowXamlSource::new()?;
        let native: IDesktopWindowXamlSourceNative2 = source.cast()?;
        let (events_tx, _) = mpsc::channel();
        unsafe {
            native.AttachToWindow(window.0)?;
            let island = native.WindowHandle()?;
            SetWindowLongPtrW(window.0, GWLP_USERDATA, island.0);
            fit_island(window.0, island, true)?;
            resize_to_design(window.0, GetDpiForWindow(window.0));
            SetTimer(window.0, TIMER_ID, TIMER_PERIOD_MS, None);
        }
        let hwnd = window.0;
        Ok(Self {
            native,
            source,
            manager,
            window,
            hwnd,
            events_tx,
        })
    }

    pub fn set_content(&self, root: &FrameworkElement) -> Result<()> {
        attach_caption(root, self.hwnd)?;
        self.source.SetContent(root)
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// 跨线程 UI 通知句柄（Clone 便宜；XAML 对象不可经此传递）。
    pub fn notifier(&self) -> ShellNotifier {
        ShellNotifier {
            tx: self.events_tx.clone(),
            hwnd: self.hwnd,
        }
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
                    _ if msg.hwnd == self.window.0
                        && msg.message == WM_TIMER
                        && msg.wParam.0 == TIMER_ID =>
                    {
                        on_event()?
                    }
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
        unsafe {
            KillTimer(self.window.0, TIMER_ID);
            SetWindowLongPtrW(self.window.0, GWLP_USERDATA, 0);
            let _ = self.source.Close();
            let _ = self.manager.Close();
        }
    }
}

/// 跨线程 UI 通知句柄。
#[derive(Clone)]
pub struct ShellNotifier {
    tx: mpsc::Sender<()>,
    hwnd: HWND,
}

impl ShellNotifier {
    /// 唤醒 UI 线程排空事件队列。
    pub fn notify(&self) {
        let _ = self.tx.send(());
        unsafe {
            let _ = PostMessageW(self.hwnd, UI_EVENT, None, None);
        }
    }
}

#[derive(Clone, Copy)]
struct DragState {
    start_cursor: POINT,
    start_window: (i32, i32),
    last_pos: (i32, i32),
}

// 使用指针捕获与屏幕坐标直移替代系统模态移动循环，避免与 XAML Island 消息泵冲突导致卡顿或失效。
fn attach_caption(root: &FrameworkElement, hwnd: HWND) -> Result<()> {
    let drag: UIElement = root.FindName("DragRegion")?.cast()?;
    let drag_state: Arc<std::sync::Mutex<Option<DragState>>> =
        Arc::new(std::sync::Mutex::new(None));

    let press_drag = drag.clone();
    let press_state = drag_state.clone();
    drag.PointerPressed(PointerEventHandler::new(move |_, args| {
        if let Some(args) = args {
            let point = args.GetCurrentPoint(None)?;
            if point.Properties()?.IsLeftButtonPressed()? {
                args.SetHandled(true)?;
                let mut start_cursor = POINT::default();
                let mut rect = RECT::default();
                let ok = unsafe {
                    GetCursorPos(&mut start_cursor).as_bool()
                        && GetWindowRect(hwnd, &mut rect).as_bool()
                };
                if ok {
                    let pointer = args.Pointer()?;
                    let _ = press_drag.CapturePointer(&pointer);
                    *press_state.lock().unwrap() = Some(DragState {
                        start_cursor,
                        start_window: (rect.left, rect.top),
                        last_pos: (rect.left, rect.top),
                    });
                }
            }
        }
        Ok(())
    }))?;

    let move_drag = drag.clone();
    let move_state = drag_state.clone();
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
            let new_x = state.start_window.0 + (cur.x - state.start_cursor.x);
            let new_y = state.start_window.1 + (cur.y - state.start_cursor.y);
            if (new_x, new_y) != state.last_pos {
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

    for (name, command) in [("Minimize", SC_MINIMIZE), ("CloseWindow", SC_CLOSE)] {
        let button: ButtonBase = root.FindName(name)?.cast()?;
        button.Click(windows::UI::Xaml::RoutedEventHandler::new(move |_, _| {
            unsafe {
                PostMessageW(hwnd, WM_SYSCOMMAND, WPARAM(command as usize), None);
            }
            Ok(())
        }))?;
    }
    Ok(())
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
            // 物理尺寸先按 96DPI 占位，attach 后按真实 DPI 重设。
            let (x, y) = centered_origin(DESIGN_WIDTH as i32, DESIGN_HEIGHT as i32);
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class.lpszClassName,
                "RabbitControl",
                // 固定窗口：无 THICKFRAME / MAXIMIZEBOX，仅拖动 + 最小化 + 关闭。
                WS_POPUP | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN,
                x,
                y,
                DESIGN_WIDTH as i32,
                DESIGN_HEIGHT as i32,
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

/// 按 设计基准 × DPI 设置窗口物理尺寸并居中到当前工作区。
unsafe fn resize_to_design(hwnd: HWND, dpi: u32) {
    let scale = dpi as f64 / 96.0;
    let width = (DESIGN_WIDTH * scale).round() as i32;
    let height = (DESIGN_HEIGHT * scale).round() as i32;
    let (x, y) = centered_origin(width, height);
    SetWindowPos(
        hwnd,
        None,
        x,
        y,
        width,
        height,
        SWP_NOACTIVATE | SWP_NOZORDER,
    );
}

/// 当前显示器工作区（物理像素）。
pub fn work_area(hwnd: HWND) -> Option<(i32, i32, i32, i32)> {
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let ok = unsafe {
        GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut info).as_bool()
    };
    ok.then_some((
        info.rcWork.left,
        info.rcWork.top,
        info.rcWork.right,
        info.rcWork.bottom,
    ))
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
        unsafe {
            DestroyWindow(self.0);
        }
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
            // 无边框：客户区即整个窗口（保留 1px 顶边避免残影）。
            let params = l.0 as *mut NCCALCSIZE_PARAMS;
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
        // DPI 变化：按新 DPI 重设窗口物理尺寸，并通知 App 重算根缩放。
        WM_DPICHANGED => {
            resize_to_design(hwnd, GetDpiForWindow(hwnd));
            PostMessageW(hwnd, UI_EVENT, None, None);
        }
        WM_CLOSE => PostQuitMessage(0),
        _ => return DefWindowProcW(hwnd, msg, w, l),
    }
    LRESULT(0)
}
