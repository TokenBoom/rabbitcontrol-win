//! Win32 小工具：宽字符串、进程实例、不可见的顶层窗口。
//!
//! 后台进程的主窗口是一个不可见的普通顶层窗口（不是 message-only 窗口）：
//! 托盘图标需要收到 `TaskbarCreated` 广播（资源管理器重启后重新挂图标），
//! 而 message-only 窗口收不到广播消息。

use windows::core::{Error, Result, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, LoadCursorW, RegisterClassW, IDC_ARROW, WINDOW_EX_STYLE, WINDOW_STYLE,
    WNDCLASSW, WNDPROC,
};

/// UTF-16 编码（含 NUL 终止）。
pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// exe 自身的模块实例。
pub fn exe_instance() -> HINSTANCE {
    HINSTANCE(
        unsafe { GetModuleHandleW(None) }
            .map(|m| m.0)
            .unwrap_or_default(),
    )
}

/// 注册窗口类并创建一个不可见的顶层窗口。
pub fn create_hidden_window(
    class: &str,
    title: &str,
    style: WINDOW_STYLE,
    ex_style: WINDOW_EX_STYLE,
    proc: WNDPROC,
) -> Result<HWND> {
    let class_name = wide(class);
    let instance = exe_instance();
    let wndclass = WNDCLASSW {
        lpfnWndProc: proc,
        hInstance: instance,
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW)? },
        lpszClassName: PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };
    if unsafe { RegisterClassW(&wndclass) } == 0 {
        return Err(Error::from_win32());
    }
    let title = wide(title);
    unsafe {
        CreateWindowExW(
            ex_style,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            style,
            0,
            0,
            0,
            0,
            None,
            None,
            instance,
            None,
        )
    }
}

/// `lparam` 里以有符号 16 位打包的 x 坐标（`GET_X_LPARAM` 语义）。
pub fn x_param(lparam: LPARAM) -> i32 {
    lparam.0 as i16 as i32
}

/// `lparam` 里以有符号 16 位打包的 y 坐标（`GET_Y_LPARAM` 语义）。
pub fn y_param(lparam: LPARAM) -> i32 {
    (lparam.0 >> 16) as i16 as i32
}
