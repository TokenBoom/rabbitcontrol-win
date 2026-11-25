//! RabbitControl Windows 输入注入与捕获。
//!
//! # 注入（被控侧）
//!
//! `SendInput` 批量注入键鼠事件。坐标经 core 的归一化协议还原后按
//! 虚拟屏映射（`MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK` 覆盖
//! 多显示器）；对 UAC 安全桌面/锁屏等高完整性上下文，普通用户进程
//! 的 SendInput 会被 UIPI 拦截——这些场景由 rc-daemon（LocalSystem
//! 服务）代理注入，用户会话代理经命名管道转发事件。
//!
//! # 捕获（主控侧）
//!
//! 全屏沉浸会话中需要把本地键鼠（含系统快捷键）重定向到远端：
//! `SetWindowsHookExW` 低级钩子（`WH_KEYBOARD_LL`/`WH_MOUSE_LL`）
//! 拦截后在消息泵线程分发，本地放行规则（如 Ctrl+Alt+Del 不可拦截、
//! Win 键组合可选放行）由会话配置决定。钩子依赖
//! [`rc_win32core::MessageWindow`] 的消息泵。

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
