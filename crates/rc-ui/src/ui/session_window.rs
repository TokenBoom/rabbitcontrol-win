//! 独立会话窗口（M3.6）：第二 HWND + 第二 XAML Island，同 UI 线程宿主。
//!
//! - 唯一允许自由缩放的窗口（`WS_OVERLAPPEDWINDOW`，min 640×400 逻辑尺寸）；
//! - 标题显示对端设备名；WM_CLOSE → 确认框 → 经主窗口 UI_EVENT 唤醒 App
//!   释放输入并断开会话；
//! - 全屏：切换 `WS_OVERLAPPEDWINDOW` ↔ 无边框铺满当前显示器；Esc 退出由
//!   会话键盘处理器经 Action 触发；
//! - 消息泵复用主窗口循环（GetMessageW 全线程分发，本窗口消息走默认处理）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use windows::{
    core::{Error, Interface, Result, HSTRING, PCWSTR},
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::{
            GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::{HiDpi::GetDpiForWindow, WindowsAndMessaging::*},
    },
    UI::Xaml::{FrameworkElement, Hosting::DesktopWindowXamlSource, Markup::XamlReader},
};

/// 主窗口句柄（Shell::new 时登记；跨窗口通知路由用）。
static MAIN_HWND: OnceLock<isize> = OnceLock::new();
/// 关闭请求标志：session wndproc WM_CLOSE 确认后置位，App dispatch 消费。
static CLOSE_REQUESTED: AtomicBool = AtomicBool::new(false);
/// 全屏前的还原几何。
#[derive(Clone, Copy)]
struct RestoreRect {
    left: i32,
    top: i32,
    width: i32,
    height: i32,
}
static RESTORE_RECT: OnceLock<RestoreRect> = OnceLock::new();

use crate::ui::shell::UI_EVENT;

/// 最小逻辑尺寸（WM_GETMINMAXINFO 按 DPI 换算物理值）。
const MIN_LOGICAL_W: i32 = 640;
const MIN_LOGICAL_H: i32 = 400;

/// 登记主窗口句柄（App 通知路由）。
pub fn set_main_hwnd(hwnd: HWND) {
    let _ = MAIN_HWND.set(hwnd.0);
}

/// 取出并清除关闭请求（App dispatch 每轮调用）。
pub fn take_close_request() -> bool {
    CLOSE_REQUESTED.swap(false, Ordering::SeqCst)
}

/// 会话窗口宿主。
pub struct SessionWindow {
    hwnd: HWND,
    _source: DesktopWindowXamlSource,
    /// 会话 UI 根（FindName 的第二查找根）。
    pub root: FrameworkElement,
}

impl SessionWindow {
    /// 创建并装载会话 XAML（不显示）。
    pub fn new() -> Result<Self> {
        unsafe {
            use crate::app::WithContext;
            let instance = GetModuleHandleW(None).ctx(|| "会话窗口：取模块实例".to_string())?;
            let name = HSTRING::from("RabbitControlSessionWindow");
            let class = WNDCLASSW {
                hInstance: instance,
                lpszClassName: PCWSTR(name.as_wide().as_ptr()),
                lpfnWndProc: Some(session_wndproc),
                hCursor: LoadCursorW(None, IDC_ARROW).ctx(|| "会话窗口：加载光标".to_string())?,
                hIcon: LoadIconW(instance, PCWSTR(1 as _)).unwrap_or_default(),
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 {
                // 同一进程里重复注册会失败（类已存在）；这里只会创建一次，失败即是真问题
                return Err(Error::from_win32()).ctx(|| "会话窗口：注册窗口类".to_string());
            }
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class.lpszClassName,
                "RabbitControl 远程",
                WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                1024,
                640,
                None,
                None,
                instance,
                std::ptr::null(),
            );
            if hwnd.0 == 0 {
                return Err(Error::from_win32()).ctx(|| "会话窗口：CreateWindowExW".to_string());
            }
            let source =
                DesktopWindowXamlSource::new().ctx(|| "会话窗口：创建 XAML Island".to_string())?;
            let native: windows::Win32::System::WinRT::Xaml::IDesktopWindowXamlSourceNative2 =
                source.cast()?;
            native
                .AttachToWindow(hwnd)
                .ctx(|| "会话窗口：XAML Island 挂到窗口".to_string())?;
            let island = native
                .WindowHandle()
                .ctx(|| "会话窗口：取 Island 子窗口".to_string())?;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, island.0);
            fit_island(hwnd, island);
            let content = XamlReader::Load(include_str!("assets/session_window.xaml"))
                .ctx(|| "会话窗口：解析 XAML".to_string())?;
            let root: FrameworkElement = content
                .cast()
                .ctx(|| "会话窗口：XAML 根节点类型".to_string())?;
            source
                .SetContent(&root)
                .ctx(|| "会话窗口：装载 XAML".to_string())?;
            Ok(Self {
                hwnd,
                _source: source,
                root,
            })
        }
    }

    /// 窗口句柄。
    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// 客户区宽度（逻辑像素，即 XAML 的单位）。
    pub fn client_width(&self) -> f64 {
        unsafe {
            let mut rect = RECT::default();
            if !GetClientRect(self.hwnd, &mut rect).as_bool() {
                return 1024.0;
            }
            f64::from(rect.right - rect.left) * 96.0 / f64::from(GetDpiForWindow(self.hwnd).max(96))
        }
    }

    /// 设置标题（对端设备名）。
    pub fn set_title(&self, title: &str) {
        unsafe {
            let wide = HSTRING::from(title);
            let _ = SetWindowTextW(self.hwnd, PCWSTR(wide.as_wide().as_ptr()));
        }
    }

    /// 显示并前置。
    pub fn show(&self) {
        unsafe {
            ShowWindow(self.hwnd, SW_SHOWNORMAL);
            let _ = SetForegroundWindow(self.hwnd);
        }
    }

    /// 销毁窗口（会话状态清理由调用方负责）。
    pub fn close(&self) {
        unsafe {
            DestroyWindow(self.hwnd);
        }
    }

    /// 切换全屏（无边框铺满当前显示器 ↔ 还原）。返回切换后是否处于全屏。
    pub fn toggle_fullscreen(&self) -> bool {
        unsafe {
            let style = GetWindowLongW(self.hwnd, GWL_STYLE);
            let caption_bits = (WS_CAPTION.0 | WS_THICKFRAME.0) as i32;
            if (style & caption_bits) != 0 {
                // 进入全屏：记住还原几何，无边框铺满最近显示器
                let mut rect = RECT::default();
                if GetWindowRect(self.hwnd, &mut rect).as_bool() {
                    let _ = RESTORE_RECT.set(RestoreRect {
                        left: rect.left,
                        top: rect.top,
                        width: rect.right - rect.left,
                        height: rect.bottom - rect.top,
                    });
                }
                SetWindowLongW(self.hwnd, GWL_STYLE, style & !caption_bits);
                let monitor = MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST);
                let mut info = MONITORINFO {
                    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                if GetMonitorInfoW(monitor, &mut info).as_bool() {
                    SetWindowPos(
                        self.hwnd,
                        None,
                        info.rcMonitor.left,
                        info.rcMonitor.top,
                        info.rcMonitor.right - info.rcMonitor.left,
                        info.rcMonitor.bottom - info.rcMonitor.top,
                        SWP_NOZORDER | SWP_FRAMECHANGED,
                    );
                }
                true
            } else {
                // 退出全屏
                SetWindowLongW(self.hwnd, GWL_STYLE, style | WS_OVERLAPPEDWINDOW.0 as i32);
                if let Some(r) = RESTORE_RECT.get() {
                    SetWindowPos(
                        self.hwnd,
                        None,
                        r.left,
                        r.top,
                        r.width,
                        r.height,
                        SWP_NOZORDER | SWP_FRAMECHANGED,
                    );
                }
                false
            }
        }
    }
}

/// 最小尺寸约束 + Island 跟随 + 关闭确认。
unsafe fn fit_island(parent: HWND, island: HWND) {
    let mut rect = RECT::default();
    if GetClientRect(parent, &mut rect).as_bool() {
        let _ = SetWindowPos(
            island,
            None,
            0,
            0,
            rect.right,
            rect.bottom,
            SWP_NOACTIVATE | SWP_NOZORDER | SWP_SHOWWINDOW,
        );
    }
}

unsafe extern "system" fn session_wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_GETMINMAXINFO => {
            let result = DefWindowProcW(hwnd, msg, w, l);
            let dpi = GetDpiForWindow(hwnd).max(96);
            let limits = &mut *(l.0 as *mut MINMAXINFO);
            limits.ptMinTrackSize = POINT {
                x: MIN_LOGICAL_W * dpi as i32 / 96,
                y: MIN_LOGICAL_H * dpi as i32 / 96,
            };
            result
        }
        WM_SIZE => {
            let island = HWND(GetWindowLongPtrW(hwnd, GWLP_USERDATA));
            if island.0 != 0 {
                fit_island(hwnd, island);
            }
            DefWindowProcW(hwnd, msg, w, l)
        }
        WM_DPICHANGED => {
            // 系统建议矩形（l 指向 RECT）
            let rect = l.0 as *const RECT;
            if !rect.is_null() {
                SetWindowPos(
                    hwnd,
                    None,
                    (*rect).left,
                    (*rect).top,
                    (*rect).right - (*rect).left,
                    (*rect).bottom - (*rect).top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            DefWindowProcW(hwnd, msg, w, l)
        }
        WM_CLOSE => {
            // 确认框（模态自跑消息循环）；确认后经主窗口 UI_EVENT 唤醒 App 收尾
            let text = HSTRING::from("断开连接并关闭远程窗口？");
            let caption = HSTRING::from("RabbitControl");
            let ok = MessageBoxW(
                hwnd,
                PCWSTR(text.as_wide().as_ptr()),
                PCWSTR(caption.as_wide().as_ptr()),
                MB_YESNO | MB_ICONQUESTION,
            );
            if ok == IDYES {
                CLOSE_REQUESTED.store(true, Ordering::SeqCst);
                if let Some(main) = MAIN_HWND.get() {
                    let _ = PostMessageW(HWND(*main as _), UI_EVENT, WPARAM(2), None);
                }
            }
            LRESULT(0) // App 决定何时 DestroyWindow
        }
        WM_DESTROY => {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}
