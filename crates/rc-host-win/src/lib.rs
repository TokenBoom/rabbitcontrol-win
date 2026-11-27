//! RabbitControl Windows 后台被控进程（`rc-host-agent`）。
//!
//! # 职责
//!
//! 登录用户的会话内常驻、没有主窗口的后台被控服务：信令连接、被控服务、
//! 托盘菜单、独立的审批小窗，以及与主界面 / 守护服务的本机进程通信。
//! 主界面关掉以后，这台电脑仍然可以被发现和控制。
//!
//! # 三个进程的分工（对齐 macOS 的 `rc-ui` / `rc-host-agent` / `rc-daemon`）
//!
//! - `rabbitcontrol-win.exe`（主界面）：控制端；主界面打开期间由它托管信令与被控；
//! - `rc-host-agent.exe`（本 crate）：主界面没开时接管信令与被控。同一设备只能有
//!   一条信令连接，所以两者是「让位 / 接管」关系，见 [`engine`]；
//! - `rc-daemon.exe`（守护服务，LocalSystem）：登录时把本进程拉进用户会话，
//!   以及高权限操作（UAC / 锁屏注入代理）。
//!
//! # 模块
//!
//! - [`engine`]：状态机与后台服务编排，不含界面，可单测；
//! - [`tray`]：托盘图标与菜单；
//! - [`approval_window`]：独立的连接审批小窗；
//! - [`ipc_server`]：响应主界面 / 诊断工具的本机管道请求；
//! - [`gui_probe`]：主界面是否在运行的探测、从托盘打开主界面；
//! - [`app`]：把以上接到一个 Win32 消息循环里；
//! - [`diag`]：诊断日志；[`win`]：Win32 小工具。

#![deny(missing_docs)]
#![allow(unsafe_code)] // 托盘 / 窗口 / 进程 FFI 边界

pub mod app;
pub mod approval_window;
pub mod diag;
pub mod engine;
pub mod gui_probe;
pub mod ipc_server;
pub mod tray;
pub mod win;
