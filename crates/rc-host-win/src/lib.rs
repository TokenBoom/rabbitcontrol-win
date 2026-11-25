//! RabbitControl Windows 被控驻留端（系统托盘 + 连接审批，对应 macOS `rc-host-macos` / AppKit 状态栏）。
//!
//! # 设计
//!
//! - 系统托盘图标（`Shell_NotifyIconW` + 消息窗口），菜单项：
//!   会话状态、断开连接、退出；
//! - 接入审批窗：新控制会话请求时弹出原生批准/拒绝对话框
//!   （对应 macOS 审批窗）；
//! - 与 `rc-daemon`（Windows 服务）经 IPC 协同：服务负责会话与
//!   输入注入高权限上下文，托盘负责用户交互；
//! - 状态机 `state` 模块对应 macOS 同名实现。
//!
//! # 当前状态
//!
//! 骨架占位。

#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod state;

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
