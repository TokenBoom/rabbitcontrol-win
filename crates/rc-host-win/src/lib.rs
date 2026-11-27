//! RabbitControl Windows 被控端用户会话代理。
//!
//! # 为什么需要它
//!
//! Windows 服务运行在 Session 0，看不到也摸不到用户桌面：采集
//! （DXGI duplication 作用于用户会话的显示器）、输入注入（UIPI）、
//! 托盘交互都必须发生在登录用户的会话内。因此被控端是双进程：
//!
//! - **本 crate（agent）**：登录会话内驻留，托盘图标
//!   （`Shell_NotifyIconW` + [`rc_win32core::MessageWindow`] 消息泵）、
//!   连接审批对话框、实际的采集/注入/渲染执行者；
//! - **rc-daemon（服务）**：LocalSystem 权限，处理 UAC 安全桌面/
//!   锁屏场景的输入注入、显示模式切换提权操作、agent 进程监管
//!   （崩溃重启、登录会话变化时在新会话拉起 agent）。
//!
//! 两者经命名管道 `\.\pipe\rabbitcontrol-win` 通信。
//!
//! # 当前状态
//!
//! 骨架占位。

#![deny(missing_docs)]
#![allow(unsafe_code)] // Shell_NotifyIconW FFI（托盘）

pub mod state;
pub mod tray;

/// 托盘应用（骨架占位）。
#[derive(Default)]
pub struct TrayApp;

impl TrayApp {
    /// 创建托盘应用。
    pub fn new() -> Self {
        TrayApp
    }

    /// 进入消息循环（阻塞）。
    pub fn run(&mut self) {
        // TODO: 注册托盘图标 + Win32 消息循环。
    }
}
