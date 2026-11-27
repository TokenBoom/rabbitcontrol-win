//! 托盘图标（`Shell_NotifyIconW`）：被控端状态展示与快捷菜单。
//!
//! - 图标：状态着色（空闲灰 / 待审批黄 / 会话中绿），运行时加载内嵌资源；
//! - 菜单：打开主界面 / 被控开关 / 退出；
//! - 事件经 [`TrayEvents`] 回调给宿主（agent 主循环）。

#![allow(unsafe_code)] // Shell_NotifyIconW FFI

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use windows::core::PCWSTR;
use windows::Win32::Foundation::HINSTANCE;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION,
    NOTIFYICONDATAW, NOTIFY_ICON_DATA_FLAGS,
};
use windows::Win32::UI::WindowsAndMessaging::LoadIconW;

/// 托盘回调消息 ID（WM_APP + 2，避开 Island 的 UI_EVENT）。
pub const TRAY_MSG: u32 = 5;

/// 托盘图标状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayState {
    /// 空闲（被控服务未开）。
    Idle,
    /// 待审批。
    Pending,
    /// 被控会话进行中。
    Active,
}

impl TrayState {
    /// 提示文本。
    pub fn tooltip(self) -> &'static str {
        match self {
            TrayState::Idle => "RabbitControl 被控端（空闲）",
            TrayState::Pending => "RabbitControl 被控端（有连接待审批）",
            TrayState::Active => "RabbitControl 被控端（被控中）",
        }
    }

    /// 系统图标资源 ID（1 = exe 嵌入的应用图标）。
    pub fn icon_id(self) -> PCWSTR {
        let _ = self;
        PCWSTR(1 as _)
    }
}

/// 托盘句柄封装（Drop 时自动移除图标）。
pub struct TrayIcon {
    hwnd: HWND,
    data: NOTIFYICONDATAW,
    visible: Arc<AtomicBool>,
}

impl TrayIcon {
    /// 注册托盘图标（挂到 `hwnd` 的消息窗口，TRAY_MSG 回调）。
    pub fn new(hwnd: HWND, callback_msg: u32) -> windows::core::Result<Self> {
        let mut data = NOTIFYICONDATAW {
            uID: TRAY_MSG,
            hWnd: hwnd,
            uFlags: NOTIFY_ICON_DATA_FLAGS(NIF_MESSAGE.0 | NIF_TIP.0),
            uCallbackMessage: callback_msg,
            ..Default::default()
        };
        // NIM_ADD 后设置版本为 4（支持 NOTIFYICON_VERSION_4 消息语义）
        unsafe {
            if !Shell_NotifyIconW(NIM_ADD, &data).as_bool() {
                return Err(windows::core::Error::from_win32());
            }
            // NOTIFYICON_VERSION_4：uVersion 存于 uTimeout union 字段
            data.Anonymous.uTimeout = 4;
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &data);
        }
        Ok(Self {
            hwnd,
            data,
            visible: Arc::new(AtomicBool::new(true)),
        })
    }

    /// 更新状态（图标提示文案随状态变化）。
    pub fn set_state(&mut self, state: TrayState) {
        let tip = state.tooltip();
        let len = tip.encode_utf16().count().min(127);
        self.data.uFlags = NOTIFY_ICON_DATA_FLAGS(NIF_TIP.0);
        self.data.szTip[..len].copy_from_slice(&tip.encode_utf16().collect::<Vec<_>>()[..len]);
        self.data.szTip[len] = 0;
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &self.data);
        }
    }

    /// 是否可见。
    pub fn is_visible(&self) -> bool {
        self.visible.load(Ordering::SeqCst)
    }

    /// 隐藏托盘图标（登录主界面时不需要被控托盘）。
    pub fn hide(&self) {
        if self.visible.swap(false, Ordering::SeqCst) {
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &self.data);
            }
        }
    }

    /// 重新显示。
    pub fn show(&self) {
        if !self.visible.swap(true, Ordering::SeqCst) {
            unsafe {
                let _ = Shell_NotifyIconW(NIM_ADD, &self.data);
                let mut d = NOTIFYICONDATAW {
                    uID: TRAY_MSG,
                    hWnd: self.hwnd,
                    ..Default::default()
                };
                d.Anonymous.uTimeout = 4;
                let _ = Shell_NotifyIconW(NIM_SETVERSION, &d);
            }
        }
    }

    /// 状态图标句柄（供消息处理绘制）。
    pub fn load_state_icon(state: TrayState) -> windows::core::Result<isize> {
        let h = unsafe { LoadIconW(exe_instance(), state.icon_id())? };
        Ok(h.0 as isize)
    }
}

impl Drop for TrayIcon {
    fn drop(&mut self) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &self.data);
        }
    }
}

/// exe 内嵌图标句柄（模块默认实例）。
fn exe_instance() -> HINSTANCE {
    HINSTANCE(unsafe { GetModuleHandleW(None).unwrap_or_default() }.0)
}
