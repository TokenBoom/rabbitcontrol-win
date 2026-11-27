//! RabbitControl Windows 输入注入与捕获。
//!
//! # 注入（被控侧）
//!
//! `SendInput` 注入键鼠事件。坐标经 core 的归一化协议还原后按
//! 虚拟屏映射（`MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK` 覆盖
//! 多显示器）；对 UAC 安全桌面/锁屏等高完整性上下文，普通用户进程
//! 的 SendInput 会被 UIPI 拦截——这些场景由 rc-daemon（LocalSystem
//! 服务）代理注入，用户会话代理经命名管道转发事件。
//!
//! # 捕获（主控侧）
//!
//! 低级钩子捕获（`WH_KEYBOARD_LL`/`WH_MOUSE_LL`）暂未启用：win 客户端
//! 的会话输入走 XAML Island 指针/键盘事件（见 rc-ui session_view），
//! 全局重定向属后续体验项。

#![deny(missing_docs)]
#![allow(unsafe_code)] // SendInput / GetSystemMetrics FFI 边界

use std::time::Duration;

use rc_input::{InputCapture, InputEvent, InputInjector, MouseButton};
use rc_types::{Error, Result};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

/// 滚轮一格的 Windows 单位（WHEEL_DELTA）。
const WHEEL_DELTA: i32 = 120;

/// Windows 输入注入桥（对应 macOS `InputBridge`）。
pub struct InputBridge {
    origin_x: f64,
    origin_y: f64,
    screen_w: u32,
    screen_h: u32,
}

impl InputBridge {
    /// 创建面向主显示器的注入桥。
    pub fn for_main_display() -> Result<Self> {
        Ok(Self {
            origin_x: 0.0,
            origin_y: 0.0,
            screen_w: 1920,
            screen_h: 1080,
        })
    }

    /// 归一化坐标（0..=65535 相对目标屏）→ 虚拟屏绝对坐标（0..=65535）。
    fn to_virtual(&self, x: u16, y: u16) -> (i32, i32) {
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        let vw = vw.max(1) as f64;
        let vh = vh.max(1) as f64;
        // 目标屏内物理坐标
        let px = self.origin_x + (x as f64 / 65535.0) * self.screen_w as f64;
        let py = self.origin_y + (y as f64 / 65535.0) * self.screen_h as f64;
        // 映射到虚拟屏归一化
        let nx = ((px - vx as f64) * 65535.0 / vw).round() as i32;
        let ny = ((py - vy as f64) * 65535.0 / vh).round() as i32;
        (nx.clamp(0, 65535), ny.clamp(0, 65535))
    }
}

/// 构造鼠标 INPUT。
fn mouse_input(flags: u32, mouse_data: i32, dx: i32, dy: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: windows::Win32::UI::Input::KeyboardAndMouse::MOUSEINPUT {
                dx,
                dy,
                mouseData: mouse_data as u32,
                dwFlags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS(flags),
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// 构造键盘 INPUT（Set-1 扫描码注入；E0 前缀键加 EXTENDEDKEY 标志）。
fn key_input(scancode: u16, pressed: bool) -> INPUT {
    let extended = (scancode & 0xE000) == 0xE000;
    let code = scancode & 0xFF;
    let mut flags = KEYEVENTF_SCANCODE.0;
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY.0;
    }
    if !pressed {
        flags |= KEYEVENTF_KEYUP.0;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY(0),
                wScan: code,
                dwFlags: windows::Win32::UI::Input::KeyboardAndMouse::KEYBD_EVENT_FLAGS(flags),
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

impl InputInjector for InputBridge {
    fn inject(&mut self, events: &[InputEvent]) -> Result<()> {
        for event in events {
            let inputs: Vec<INPUT> = match event {
                InputEvent::MouseMove { x, y } => {
                    let (nx, ny) = self.to_virtual(*x, *y);
                    vec![mouse_input(
                        (MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK).0,
                        0,
                        nx,
                        ny,
                    )]
                }
                InputEvent::MouseButton { button, pressed } => {
                    let (down, up, data) = match button {
                        MouseButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, 0),
                        MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, 0),
                        MouseButton::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 0),
                        MouseButton::X1 => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, 1),
                        MouseButton::X2 => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, 2),
                    };
                    let flag = if *pressed { down.0 } else { up.0 };
                    vec![mouse_input(flag, data, 0, 0)]
                }
                InputEvent::MouseWheel { delta, horizontal } => {
                    let data = (*delta as i32) * WHEEL_DELTA;
                    let flag = if *horizontal {
                        MOUSEEVENTF_HWHEEL.0
                    } else {
                        MOUSEEVENTF_WHEEL.0
                    };
                    vec![mouse_input(flag, data, 0, 0)]
                }
                InputEvent::Key {
                    scancode,
                    pressed,
                    modifiers: _,
                } => vec![key_input(*scancode, *pressed)],
                InputEvent::Sync => Vec::new(),
            };
            if inputs.is_empty() {
                continue;
            }
            let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
            if sent as usize != inputs.len() {
                return Err(Error::InvalidConfig(format!(
                    "SendInput 只注入 {sent}/{} 条（目标上下文可能拒注入）",
                    inputs.len()
                )));
            }
        }
        Ok(())
    }

    fn set_screen_size(&mut self, w: u32, h: u32) {
        self.screen_w = w;
        self.screen_h = h;
    }

    fn set_screen_bounds(&mut self, origin_x: f64, origin_y: f64, w: u32, h: u32) {
        self.origin_x = origin_x;
        self.origin_y = origin_y;
        self.screen_w = w;
        self.screen_h = h;
    }
}

/// Windows 输入捕获（低级钩子，暂未启用——win 主控走 XAML 事件）。
pub struct HookInputCapture;

impl HookInputCapture {
    /// 安装低级键鼠钩子。
    pub fn new() -> Result<Self> {
        Err(Error::InvalidConfig(
            "low-level hook capture not yet implemented".into(),
        ))
    }
}

impl InputCapture for HookInputCapture {
    fn next_events(&mut self, _timeout: Duration) -> Result<Vec<InputEvent>> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_input_encodes_extended_and_up_flags() {
        // union 不可模式匹配，直接字段访问
        let down = key_input(0x1D, true);
        let ki = unsafe { &down.Anonymous.ki };
        assert_eq!(ki.wScan, 0x1D);
        assert_eq!(ki.dwFlags.0, KEYEVENTF_SCANCODE.0);

        let ext_down = key_input(0xE01D, true);
        let ki = unsafe { &ext_down.Anonymous.ki };
        assert_eq!(ki.wScan, 0x1D);
        assert_eq!(ki.dwFlags.0, KEYEVENTF_SCANCODE.0 | KEYEVENTF_EXTENDEDKEY.0);

        let up = key_input(0x2A, false);
        let ki = unsafe { &up.Anonymous.ki };
        assert_eq!(ki.dwFlags.0, KEYEVENTF_SCANCODE.0 | KEYEVENTF_KEYUP.0);
    }

    #[test]
    fn wheel_multiplies_wheel_delta() {
        let inputs = wheel_inputs(2, false);
        let mi = unsafe { &inputs[0].Anonymous.mi };
        assert_eq!(mi.mouseData, 2 * WHEEL_DELTA as u32);
        assert_eq!(mi.dwFlags.0, MOUSEEVENTF_WHEEL.0);
    }

    fn wheel_inputs(delta: i16, horizontal: bool) -> Vec<INPUT> {
        let data = delta as i32 * WHEEL_DELTA;
        let flag = if horizontal {
            MOUSEEVENTF_HWHEEL.0
        } else {
            MOUSEEVENTF_WHEEL.0
        };
        vec![mouse_input(flag, data, 0, 0)]
    }
}
