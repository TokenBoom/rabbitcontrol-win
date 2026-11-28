//! RabbitControl Windows 跨进程共享的应用层逻辑（GUI-free）。
//!
//! 主界面（`rc-ui`）与后台被控进程（`rc-host-win` 的 `rc-host-agent`）共用：
//!
//! - [`auth_text`]：认证错误文案、本机标识；
//! - [`signal_runtime`]：信令长连接与局域网发现的生命周期（同一设备同一时刻
//!   只能有一条信令连接——服务端会以新连接顶掉旧连接）；
//! - [`host`]：被控服务（[`rc_client_core::host_bridge::HostHandle`]）编排；
//! - [`agent_state`]：后台被控进程的持久化开关；
//! - [`agent_ipc`]：主界面 ↔ 后台被控进程的命名管道协议；
//! - [`named_pipe`]：命名管道服务端的通用骨架（守护服务与后台进程共用）；
//! - [`trace_log`]：内部事件日志开关（环境变量 `RC_TRACE`，诊断用）。
//! - [`share_code`]：「复制并分享」的文案与设备码的 3-3-3 显示格式；
//!
//! 本 crate 不引用任何 UI 框架；可单测。

#![deny(missing_docs)]
#![deny(unsafe_code)]

pub mod agent_ipc;
pub mod agent_state;
pub mod auth_text;
pub mod daemon_ipc;
pub mod host;
pub mod named_pipe;
pub mod p2p_port_lease;
pub mod share_code;
pub mod signal_runtime;
pub mod trace_log;
