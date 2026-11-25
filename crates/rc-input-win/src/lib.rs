//! RabbitControl Windows 输入注入与捕获（对应 macOS `rc-input-macos` / CGEventTap）。
//!
//! # 设计
//!
//! - **注入**（被控侧）：`SendInput` 批量注入键鼠事件；坐标用
//!   [`rc_input::denormalize_coord`] 反归一化后按虚拟屏（多显示器）映射，
//!   `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`；需要 UIAccess/完整性级别
//!   处理 UAC 安全桌面场景（与服务端 `rc-daemon` 高权限会话协同）。
//! - **捕获**（主控侧）：`SetWindowsHookExW` 低级键鼠钩子
//!   （`WH_KEYBOARD_LL` / `WH_MOUSE_LL`），用于会话内全屏沉浸式捕获
//!   与本地快捷键放行。
//! - 键位映射 `keymap` 模块：core `KeyCode` ↔ Windows 扫描码/虚拟键码。
//!
//! # 当前状态
//!
//! 骨架占位：注入/捕获均返回未实现错误。

#![deny(missing_docs)]
#![forbid(unsafe_code)]

use std::time::Duration;

use rc_input::{InputCapture, InputEvent, InputInjector};
use rc_types::{Error, Result};

/// Windows 输入注入桥（对应 macOS `InputBridge`，骨架占位）。
pub struct InputBridge {
    screen_w: u32,
    screen_h: u32,
}

impl InputBridge {
    /// 创建面向主显示器的注入桥。
    pub fn for_main_display() -> Result<Self> {
        // TODO: 读取主屏分辨率与虚拟屏边界。
        Ok(Self {
            screen_w: 1920,
            screen_h: 1080,
        })
    }
}

impl InputInjector for InputBridge {
    fn inject(&mut self, _events: &[InputEvent]) -> Result<()> {
        // TODO: 批量 SendInput。
        Err(Error::InvalidConfig(
            "sendinput injection not yet implemented".into(),
        ))
    }

    fn set_screen_size(&mut self, w: u32, h: u32) {
        self.screen_w = w;
        self.screen_h = h;
    }
}

/// Windows 输入捕获（低级钩子，骨架占位）。
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
