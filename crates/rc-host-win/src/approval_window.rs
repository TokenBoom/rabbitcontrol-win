//! 独立的连接审批小窗。
//!
//! 有人请求控制本机、且既不是机主自控也没通过密码时，由后台进程（而不是主界面）
//! 在屏幕右下角弹出一个置顶小窗：谁在请求、是否已验证密码、倒计时后自动拒绝。
//! 主界面没开也能审批；用户点了「允许」或「拒绝」后把结果交回引擎。
//!
//! 窗口整体自绘（背景、文字、两颗按钮），配色沿用主界面的设计令牌：白卡片、
//! 品牌蓝主按钮、描边次按钮。键盘 `Enter` = 允许，`Esc` = 拒绝。

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use windows::core::{Error, Result, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect,
    FrameRect, InvalidateRect, SelectObject, SetBkMode, SetTextColor, CLEARTYPE_QUALITY,
    DEFAULT_CHARSET, DRAW_TEXT_FORMAT, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX,
    DT_SINGLELINE, DT_VCENTER, FW_NORMAL, FW_SEMIBOLD, HBRUSH, HDC, HFONT, HGDIOBJ, PAINTSTRUCT,
    TRANSPARENT,
};
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_ESCAPE, VK_RETURN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, FlashWindowEx, IsWindowVisible, LoadCursorW, PostMessageW,
    RegisterClassW, SetWindowPos, ShowWindow, SystemParametersInfoW, FLASHWINFO, FLASHW_ALL,
    FLASHW_TIMERNOFG, HWND_TOPMOST, IDC_ARROW, SPI_GETWORKAREA, SWP_NOACTIVATE, SWP_SHOWWINDOW,
    SW_HIDE, SW_SHOWNA, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WM_APP, WM_CLOSE, WM_KEYDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_PAINT, WNDCLASSW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

/// `WM_MOUSELEAVE`（`Win32::UI::Controls` 里的常量，为它单开一个 feature 不值得）。
const WM_MOUSELEAVE: u32 = 0x02A3;

use crate::win::{exe_instance, x_param, y_param};

/// 审批结果消息（发往后台进程主窗口）：`wParam` 1 = 允许，0 = 拒绝。
pub const RESULT_MSG: u32 = WM_APP + 2;
/// 窗口类名。
const CLASS: &str = "RabbitControlApprovalWindow";

// 设计令牌（与 rc-ui styles.xaml 一致；COLORREF 布局为 0x00BBGGRR）
const COLOR_CARD: u32 = 0xFF_FF_FF; // #FFFFFF
const COLOR_BORDER: u32 = 0xF6_EE_E8; // #E8EEF6
const COLOR_TEXT: u32 = 0x31_1E_15; // #151E31
const COLOR_TEXT_SECONDARY: u32 = 0x78_62_51; // #516278
const COLOR_PRIMARY: u32 = 0xFF_6C_34; // #346CFF
const COLOR_PRIMARY_HOVER: u32 = 0xE6_5B_2A; // #2A5BE6
const COLOR_TEXT_ON_PRIMARY: u32 = 0xFF_FF_FF; // #FFFFFF
const COLOR_SECONDARY_BORDER: u32 = 0xD9_C8_BB; // #BBC8D9
const COLOR_SECONDARY_HOVER: u32 = 0xFB_F8_F6; // #F6F8FB

// 逻辑尺寸（96 DPI 下的像素）
const WIN_W: i32 = 372;
const WIN_H: i32 = 198;
const PAD: i32 = 20;
const BTN_W: i32 = 96;
const BTN_H: i32 = 34;
const BTN_GAP: i32 = 12;
const EDGE: i32 = 16;

/// 两颗按钮的标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Button {
    Deny,
    Allow,
}

/// 审批小窗的显示内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    /// 请求方地址。
    pub peer: String,
    /// 请求方是否已通过密码验证。
    pub password_verified: bool,
    /// 自动拒绝时刻。
    pub deadline: Instant,
    /// 审批代次（结果交回时带回）。
    pub generation: u64,
}

struct UiState {
    prompt: Option<Prompt>,
    hover: Option<Button>,
}

static UI: Mutex<UiState> = Mutex::new(UiState {
    prompt: None,
    hover: None,
});
static WINDOW: AtomicIsize = AtomicIsize::new(0);
static AGENT_WINDOW: AtomicIsize = AtomicIsize::new(0);
static CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);

/// GDI 字体缓存。句柄不是 `Send`，以裸值存放；DPI 变化时重建。
struct FontCache {
    dpi: u32,
    title: isize,
    body: isize,
    button: isize,
}

static FONTS: Mutex<FontCache> = Mutex::new(FontCache {
    dpi: 0,
    title: 0,
    body: 0,
    button: 0,
});

/// 记下后台进程主窗口（审批结果发往这里）。
pub fn set_agent_window(hwnd: HWND) {
    AGENT_WINDOW.store(hwnd.0 as isize, Ordering::SeqCst);
}

/// 弹出审批小窗（须在界面线程调用）。
pub fn show(prompt: Prompt) -> Result<()> {
    register_class()?;
    let hwnd = match current_window() {
        hwnd if hwnd.0.is_null() => create_window()?,
        hwnd => hwnd,
    };
    {
        let mut ui = UI.lock().unwrap_or_else(|e| e.into_inner());
        ui.prompt = Some(prompt);
        ui.hover = None;
    }
    layout(hwnd)?;
    // Windows 11：让系统给窗口加圆角（旧系统忽略这个属性）
    let rounded: i32 = 2; // DWMWCP_ROUND
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &rounded as *const i32 as *const core::ffi::c_void,
            std::mem::size_of::<i32>() as u32,
        );
        let _ = ShowWindow(hwnd, SW_SHOWNA);
    }
    flash(hwnd);
    Ok(())
}

/// 收起审批小窗（须在界面线程调用）。
pub fn hide() {
    let hwnd = current_window();
    if !hwnd.0.is_null() {
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
    UI.lock().unwrap_or_else(|e| e.into_inner()).prompt = None;
}

/// 取走待审批的代次（界面线程点击按钮时用）。
pub fn pending_generation() -> Option<u64> {
    UI.lock()
        .unwrap_or_else(|e| e.into_inner())
        .prompt
        .as_ref()
        .map(|p| p.generation)
}

/// 每秒刷新一次倒计时；返回 `true` 表示已过自动拒绝时刻。
pub fn tick_countdown() -> bool {
    let hwnd = current_window();
    if hwnd.0.is_null() || !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return false;
    }
    let expired = {
        let ui = UI.lock().unwrap_or_else(|e| e.into_inner());
        match ui.prompt.as_ref() {
            Some(prompt) => Instant::now() >= prompt.deadline,
            None => true,
        }
    };
    unsafe {
        let _ = InvalidateRect(hwnd, None, false);
    }
    expired
}

fn register_class() -> Result<()> {
    if CLASS_REGISTERED.load(Ordering::SeqCst) {
        return Ok(());
    }
    let class_name = crate::win::wide(CLASS);
    let wndclass = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: exe_instance(),
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW)? },
        lpszClassName: PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };
    if unsafe { RegisterClassW(&wndclass) } == 0 {
        return Err(Error::from_win32());
    }
    CLASS_REGISTERED.store(true, Ordering::SeqCst);
    Ok(())
}

fn create_window() -> Result<HWND> {
    let class_name = crate::win::wide(CLASS);
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            PCWSTR(class_name.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            exe_instance(),
            None,
        )?
    };
    WINDOW.store(hwnd.0 as isize, Ordering::SeqCst);
    Ok(hwnd)
}

fn current_window() -> HWND {
    HWND(WINDOW.load(Ordering::SeqCst) as *mut core::ffi::c_void)
}

fn flash(hwnd: HWND) {
    let info = FLASHWINFO {
        cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
        hwnd,
        dwFlags: FLASHW_ALL | FLASHW_TIMERNOFG,
        uCount: 3,
        dwTimeout: 0,
    };
    unsafe {
        let _ = FlashWindowEx(&info);
    }
}

/// 定位到主显示器工作区右下角，并按主显示器 DPI 设定尺寸。
fn layout(hwnd: HWND) -> Result<()> {
    let dpi = unsafe { GetDpiForSystem() };
    let width = scale(WIN_W, dpi);
    let height = scale(WIN_H, dpi);
    let mut work = RECT::default();
    unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut work as *mut RECT as *mut core::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )?;
    }
    let x = work.right - width - scale(EDGE, dpi);
    let y = work.bottom - height - scale(EDGE, dpi);
    unsafe {
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            x,
            y,
            width,
            height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )?;
    }
    Ok(())
}

/// 逻辑像素 → 物理像素（四舍五入）。
fn scale(px: i32, dpi: u32) -> i32 {
    ((i64::from(px) * i64::from(dpi) + 48) / 96) as i32
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let hovered = hit_test(hwnd, x_param(lparam), y_param(lparam));
            let changed = {
                let mut ui = UI.lock().unwrap_or_else(|e| e.into_inner());
                std::mem::replace(&mut ui.hover, hovered) != hovered
            };
            let mut track = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut track);
            if changed {
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            let had_hover = UI
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .hover
                .take()
                .is_some();
            if had_hover {
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if let Some(button) = hit_test(hwnd, x_param(lparam), y_param(lparam)) {
                submit(button);
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let key = wparam.0 & 0xffff;
            if key == VK_RETURN.0 as usize {
                submit(Button::Allow);
            } else if key == VK_ESCAPE.0 as usize {
                submit(Button::Deny);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            // 关掉窗口等同于拒绝：不能留下一个「没人管」的请求
            submit(Button::Deny);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn hit_test(hwnd: HWND, x: i32, y: i32) -> Option<Button> {
    let (deny, allow) = button_rects(hwnd);
    if contains(allow, x, y) {
        Some(Button::Allow)
    } else if contains(deny, x, y) {
        Some(Button::Deny)
    } else {
        None
    }
}

fn submit(button: Button) {
    let Some(generation) = pending_generation() else {
        return;
    };
    let agent = HWND(AGENT_WINDOW.load(Ordering::SeqCst) as *mut core::ffi::c_void);
    if agent.0.is_null() {
        return;
    }
    unsafe {
        let _ = PostMessageW(
            agent,
            RESULT_MSG,
            WPARAM(usize::from(button == Button::Allow)),
            LPARAM(generation as isize),
        );
    }
    // 立刻收起；引擎确认后还会发「已了结」事件
    hide();
}

fn contains(rect: RECT, x: i32, y: i32) -> bool {
    x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
}

/// 两颗按钮的客户区矩形（物理像素）：拒绝在左，允许在右。
fn button_rects(hwnd: HWND) -> (RECT, RECT) {
    layout_buttons(unsafe { GetDpiForWindow(hwnd) })
}

/// 按给定 DPI 计算两颗按钮的矩形（纯函数，便于测试）。
fn layout_buttons(dpi: u32) -> (RECT, RECT) {
    let (width, height) = (scale(WIN_W, dpi), scale(WIN_H, dpi));
    let (button_w, button_h) = (scale(BTN_W, dpi), scale(BTN_H, dpi));
    let (pad, gap) = (scale(PAD, dpi), scale(BTN_GAP, dpi));
    let allow = RECT {
        left: width - pad - button_w,
        top: height - pad - button_h,
        right: width - pad,
        bottom: height - pad,
    };
    let deny = RECT {
        left: allow.left - gap - button_w,
        top: allow.top,
        right: allow.left - gap,
        bottom: allow.bottom,
    };
    (deny, allow)
}

fn paint(hwnd: HWND) {
    let mut paint_struct = PAINTSTRUCT::default();
    let hdc = unsafe { BeginPaint(hwnd, &mut paint_struct) };
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    let (peer, detail, hover) = {
        let ui = UI.lock().unwrap_or_else(|e| e.into_inner());
        let peer = ui
            .prompt
            .as_ref()
            .map(|p| p.peer.clone())
            .unwrap_or_default();
        let detail = ui.prompt.as_ref().map_or_else(String::new, |p| {
            let how = if p.password_verified {
                "密码已验证"
            } else {
                "未验证密码"
            };
            let remaining = p
                .deadline
                .saturating_duration_since(Instant::now())
                .as_secs();
            format!("{how} · {remaining} 秒后自动拒绝")
        });
        (peer, detail, ui.hover)
    };
    let client = RECT {
        left: 0,
        top: 0,
        right: scale(WIN_W, dpi),
        bottom: scale(WIN_H, dpi),
    };
    with_brush(COLOR_CARD, |brush| unsafe {
        let _ = FillRect(hdc, &client, brush);
    });
    with_brush(COLOR_BORDER, |brush| unsafe {
        let _ = FrameRect(hdc, &client, brush);
    });
    let pad = scale(PAD, dpi);
    let (title_font, body_font, button_font) = fonts(dpi);
    // 三行：标题 / 请求方 / 倒计时——请求方地址单独一行，再长也不会把标题挤掉
    text(
        hdc,
        "有设备请求控制这台电脑",
        RECT {
            left: pad,
            top: scale(16, dpi),
            right: client.right - pad,
            bottom: scale(40, dpi),
        },
        title_font,
        COLOR_TEXT,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
    );
    text(
        hdc,
        &peer,
        RECT {
            left: pad,
            top: scale(48, dpi),
            right: client.right - pad,
            bottom: scale(72, dpi),
        },
        button_font,
        COLOR_TEXT,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
    );
    text(
        hdc,
        &detail,
        RECT {
            left: pad,
            top: scale(80, dpi),
            right: client.right - pad,
            bottom: scale(102, dpi),
        },
        body_font,
        COLOR_TEXT_SECONDARY,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
    );
    let (deny, allow) = button_rects(hwnd);
    let (fill, border, label_color) = if hover == Some(Button::Deny) {
        (
            COLOR_SECONDARY_HOVER,
            Some(COLOR_SECONDARY_BORDER),
            COLOR_TEXT,
        )
    } else {
        (COLOR_CARD, Some(COLOR_SECONDARY_BORDER), COLOR_TEXT)
    };
    paint_button(hdc, deny, "拒绝", fill, border, label_color, button_font);
    let fill = if hover == Some(Button::Allow) {
        COLOR_PRIMARY_HOVER
    } else {
        COLOR_PRIMARY
    };
    paint_button(
        hdc,
        allow,
        "允许",
        fill,
        None,
        COLOR_TEXT_ON_PRIMARY,
        button_font,
    );
    unsafe {
        let _ = EndPaint(hwnd, &paint_struct);
    }
}

fn paint_button(
    hdc: HDC,
    rect: RECT,
    label: &str,
    fill: u32,
    border: Option<u32>,
    label_color: u32,
    font: HFONT,
) {
    with_brush(fill, |brush| unsafe {
        let _ = FillRect(hdc, &rect, brush);
    });
    if let Some(border) = border {
        with_brush(border, |brush| unsafe {
            let _ = FrameRect(hdc, &rect, brush);
        });
    }
    text(
        hdc,
        label,
        rect,
        font,
        label_color,
        DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
    );
}

fn text(hdc: HDC, value: &str, mut rect: RECT, font: HFONT, color: u32, format: DRAW_TEXT_FORMAT) {
    let mut units: Vec<u16> = value.encode_utf16().collect();
    unsafe {
        let _ = SetBkMode(hdc, TRANSPARENT);
        let _ = SetTextColor(hdc, COLORREF(color));
        let previous = SelectObject(hdc, HGDIOBJ(font.0));
        let _ = DrawTextW(hdc, &mut units, &mut rect, format);
        let _ = SelectObject(hdc, previous);
    }
}

fn with_brush<R>(color: u32, body: impl FnOnce(HBRUSH) -> R) -> R {
    let brush = unsafe { CreateSolidBrush(COLORREF(color)) };
    let result = body(brush);
    unsafe {
        let _ = DeleteObject(HGDIOBJ(brush.0));
    }
    result
}

/// 取当前 DPI 的三套字体（标题 / 说明 / 按钮）。
fn fonts(dpi: u32) -> (HFONT, HFONT, HFONT) {
    let mut cache = FONTS.lock().unwrap_or_else(|e| e.into_inner());
    if cache.dpi != dpi {
        for old in [cache.title, cache.body, cache.button] {
            if old != 0 {
                unsafe {
                    let _ = DeleteObject(HGDIOBJ(old as *mut core::ffi::c_void));
                }
            }
        }
        cache.dpi = dpi;
        cache.title = make_font(16, FW_SEMIBOLD, dpi).0 as isize;
        cache.body = make_font(13, FW_NORMAL, dpi).0 as isize;
        cache.button = make_font(13, FW_SEMIBOLD, dpi).0 as isize;
    }
    (
        HFONT(cache.title as *mut core::ffi::c_void),
        HFONT(cache.body as *mut core::ffi::c_void),
        HFONT(cache.button as *mut core::ffi::c_void),
    )
}

fn make_font(size_px: i32, weight: windows::Win32::Graphics::Gdi::FONT_WEIGHT, dpi: u32) -> HFONT {
    let face = crate::win::wide("Microsoft YaHei UI");
    unsafe {
        CreateFontW(
            -scale(size_px, dpi),
            0,
            0,
            0,
            weight.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            0,
            0,
            CLEARTYPE_QUALITY.0 as u32,
            0,
            PCWSTR(face.as_ptr()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_pixels_scale_with_rounding() {
        assert_eq!(scale(100, 96), 100);
        assert_eq!(scale(100, 120), 125);
        assert_eq!(scale(100, 144), 150);
        assert_eq!(scale(100, 192), 200);
        assert_eq!(scale(1, 144), 2, "半像素向上取整");
    }

    #[test]
    fn buttons_are_placed_bottom_right_without_overlap() {
        for dpi in [96u32, 120, 144, 192, 288] {
            let (deny, allow) = layout_buttons(dpi);
            assert!(deny.right < allow.left, "两颗按钮不能重叠");
            assert!(deny.left >= 0);
            assert!(allow.right <= scale(WIN_W, dpi));
            assert!(allow.bottom <= scale(WIN_H, dpi));
            assert!(deny.top > scale(102, dpi), "按钮不能压住说明文字");
        }
    }
}
