//! 业务编排层（GUI-free）：认证、信令运行时、会话编排、历史与设置。
//!
//! 本层不引用任何 windows 类型，全部可单测；对 UI 层暴露纯值接口。

pub mod auth;
pub mod autostart;
pub mod history;
pub mod local_prefs;
pub mod proxy_cfg;
pub mod session;
pub mod settings;
pub mod terminal;
#[cfg(debug_assertions)]
pub mod terminal_demo;
pub mod terminal_keys;
pub mod update_check;
pub mod vt;
