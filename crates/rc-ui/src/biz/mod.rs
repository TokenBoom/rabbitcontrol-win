//! 业务编排层（GUI-free）：认证、信令运行时、会话编排、历史与设置。
//!
//! 本层不引用任何 windows 类型，全部可单测；对 UI 层暴露纯值接口。

pub mod auth;
pub mod autostart;
pub mod history;
pub mod host;
pub mod proxy_cfg;
pub mod runtime;
pub mod session;
pub mod settings;
pub mod update_check;
pub mod vt;

/// core 结构化错误 → 界面文案：标题 +（恢复建议）。
///
/// 只在错误码确实存在于 core 错误目录时调用——目录里没有的码会落到
/// 「发生未知错误」，那种情况应沿用本地文案（例如 HTTP 429 限流）。
pub(crate) fn ui_error_text(ui: &rc_client_core::ui_error::UiError) -> String {
    let mut text = ui.title.clone();
    if !ui.suggestion.is_empty() {
        text.push_str(&format!("（{}）", ui.suggestion));
    }
    text
}
