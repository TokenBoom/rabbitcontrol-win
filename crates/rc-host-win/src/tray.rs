//! 托盘图标与菜单（`Shell_NotifyIconW`）。
//!
//! 主界面关掉以后，托盘图标是「RabbitControl 仍在后台运行」的唯一可见提示：
//! 提示文案反映当前状态，会话开始 / 结束弹气泡通知，菜单提供打开主界面、
//! 开关被控、断开连接、复制设备码与整体退出。
//!
//! 使用 `NOTIFYICON_VERSION_4` 消息语义：回调以 `WM_COMMAND` 形式到达
//! （`wParam` 低 16 位为图标 ID，高 16 位为事件）。资源管理器重启后会广播
//! `TaskbarCreated`，收到时需要重新挂图标。

use windows::core::{Error, Result, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NIM_SETVERSION, NOTIFYICONDATAW, NOTIFYICON_VERSION_4, NOTIFY_ICON_DATA_FLAGS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, LoadIconW, RegisterWindowMessageW,
    SetForegroundWindow, TrackPopupMenu, MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING,
    TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_CONTEXTMENU,
};

use crate::win::{exe_instance, wide, x_param, y_param};

/// 托盘回调消息 ID（`WM_COMMAND` 形式，见模块说明）。
pub const TRAY_CALLBACK_MSG: u32 = WM_APP + 1;
/// 左键单击 / 键盘选中（`NIN_SELECT`）。
const NIN_SELECT: u32 = 1024;
/// 托盘图标在 exe 资源里的 ID（与主界面同一个图标）。
const ICON_RESOURCE_ID: u16 = 1;
/// 常规图标标志（不含只在弹气泡那一次使用的 `NIF_INFO`）。
fn base_flags() -> NOTIFY_ICON_DATA_FLAGS {
    NIF_MESSAGE | NIF_ICON | NIF_TIP
}

/// 托盘菜单命令。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    /// 打开或激活主界面。
    OpenGui,
    /// 开关「允许被控」。
    ToggleHost,
    /// 断开当前正在控制本机的远端。
    Disconnect,
    /// 整体退出 RabbitControl（后台进程 + 请求守护服务暂不拉起）。
    Quit,
}

/// 托盘菜单的可选项（随状态变化）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuState {
    /// 「允许被控」是否开启（菜单打勾）。
    pub host_enabled: bool,
    /// 有远端会话进行中（「断开连接」才可用）。
    pub session_active: bool,
}

/// 托盘图标（离开作用域自动移除）。
pub struct TrayIcon {
    data: NOTIFYICONDATAW,
}

impl TrayIcon {
    /// 挂上托盘图标；回调发往 `hwnd` 的消息循环。
    pub fn add(hwnd: HWND, tooltip: &str) -> Result<Self> {
        let icon = unsafe { LoadIconW(exe_instance(), PCWSTR(ICON_RESOURCE_ID as *const u16))? };
        let mut data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: base_flags(),
            uCallbackMessage: TRAY_CALLBACK_MSG,
            hIcon: icon,
            ..Default::default()
        };
        write_fixed(&mut data.szTip, tooltip);
        if !unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool() {
            return Err(Error::from_win32());
        }
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        unsafe {
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &data);
        }
        Ok(Self { data })
    }

    /// 更新提示文案。
    pub fn set_tooltip(&mut self, tooltip: &str) {
        write_fixed(&mut self.data.szTip, tooltip);
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &self.data);
        }
    }

    /// 弹一条气泡通知。
    pub fn show_balloon(&mut self, title: &str, text: &str) {
        write_fixed(&mut self.data.szInfoTitle, title);
        write_fixed(&mut self.data.szInfo, text);
        self.data.uFlags = base_flags() | NIF_INFO;
        self.data.dwInfoFlags = NIIF_INFO;
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &self.data);
        }
        // 之后改提示文案时不再重复弹这条气泡
        self.data.uFlags = base_flags();
    }

    /// 资源管理器重启后重新挂上图标。
    pub fn re_add(&mut self) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_ADD, &self.data);
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &self.data);
        }
    }
}

impl Drop for TrayIcon {
    fn drop(&mut self) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &self.data);
        }
    }
}

/// `TaskbarCreated` 广播消息的 ID（系统注册的全局消息，每进程取一次）。
pub fn taskbar_created_msg() -> u32 {
    static MSG: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *MSG.get_or_init(|| {
        let name = wide("TaskbarCreated");
        unsafe { RegisterWindowMessageW(PCWSTR(name.as_ptr())) }
    })
}

/// 托盘回调是否要求弹出菜单；是则返回菜单应出现的屏幕坐标。
pub fn menu_anchor(wparam: WPARAM, lparam: LPARAM) -> Option<(i32, i32)> {
    let w = wparam.0 as u32;
    let (id, event) = (w & 0xffff, w >> 16);
    if id != 1 {
        return None;
    }
    match event {
        WM_CONTEXTMENU => Some((x_param(lparam), y_param(lparam))),
        NIN_SELECT => {
            let mut point = POINT::default();
            unsafe { GetCursorPos(&mut point) }.ok()?;
            Some((point.x, point.y))
        }
        _ => None,
    }
}

/// 把文本写进定长 NUL 结尾的缓冲（超长截断）。
fn write_fixed(buf: &mut [u16], text: &str) {
    let units: Vec<u16> = text.encode_utf16().take(buf.len() - 1).collect();
    buf.fill(0);
    buf[..units.len()].copy_from_slice(&units);
}

fn append(
    menu: windows::Win32::UI::WindowsAndMessaging::HMENU,
    flags: windows::Win32::UI::WindowsAndMessaging::MENU_ITEM_FLAGS,
    id: usize,
    label: &str,
) {
    // 宽字符串必须活到 AppendMenuW 返回之后
    let label = wide(label);
    unsafe {
        let _ = AppendMenuW(menu, flags, id, PCWSTR(label.as_ptr()));
    }
}

/// 弹出托盘菜单并返回所选命令（直接关闭菜单返回 `None`）。
pub fn show_menu(hwnd: HWND, at: (i32, i32), state: MenuState) -> Option<TrayCommand> {
    const CMD_OPEN_GUI: usize = 1;
    const CMD_TOGGLE_HOST: usize = 2;
    const CMD_DISCONNECT: usize = 3;
    const CMD_QUIT: usize = 5;
    let menu = unsafe { CreatePopupMenu() }.ok()?;
    append(menu, MF_STRING, CMD_OPEN_GUI, "打开主界面");
    append(menu, MF_SEPARATOR, 0, "");
    append(
        menu,
        MF_STRING
            | if state.host_enabled {
                MF_CHECKED
            } else {
                MF_STRING
            },
        CMD_TOGGLE_HOST,
        "允许被控",
    );
    append(
        menu,
        MF_STRING
            | if state.session_active {
                MF_STRING
            } else {
                MF_GRAYED
            },
        CMD_DISCONNECT,
        "断开连接",
    );
    append(menu, MF_SEPARATOR, 0, "");
    append(menu, MF_STRING, CMD_QUIT, "退出 RabbitControl");
    // 菜单需要一个前台窗口配合，点击别处时才会自动关闭
    unsafe {
        let _ = SetForegroundWindow(hwnd);
        let chosen = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
            at.0,
            at.1,
            0,
            hwnd,
            None,
        )
        .0 as usize;
        let _ = DestroyMenu(menu);
        match chosen {
            CMD_OPEN_GUI => Some(TrayCommand::OpenGui),
            CMD_TOGGLE_HOST => Some(TrayCommand::ToggleHost),
            CMD_DISCONNECT if state.session_active => Some(TrayCommand::Disconnect),
            CMD_QUIT => Some(TrayCommand::Quit),
            _ => None,
        }
    }
}
