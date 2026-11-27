//! 认证错误文案与本机标识（主界面与后台被控进程共用，GUI-free）。

use rc_client_core::auth_client::AuthClientError;

/// core 结构化错误 → 界面文案：标题 +（恢复建议）。
///
/// 只在错误码确实存在于 core 错误目录时调用——目录里没有的码会落到
/// 「发生未知错误」，那种情况应沿用本地文案（例如 HTTP 429 限流）。
pub fn ui_error_text(ui: &rc_client_core::ui_error::UiError) -> String {
    let mut text = ui.title.clone();
    if !ui.suggestion.is_empty() {
        text.push_str(&format!("（{}）", ui.suggestion));
    }
    text
}

/// 获取本机主机名，避免在 Windows 上回退为 macOS 默认标签。
pub fn local_hostname() -> String {
    for key in ["COMPUTERNAME", "HOSTNAME"] {
        if let Ok(value) = std::env::var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    "Windows".to_string()
}

/// 将认证客户端错误映射为适合界面展示的中文提示。
///
/// 能对上 core 错误目录（`ui_error`）的走目录文案（标题 + 恢复建议，全平台
/// 一致）；目录没有覆盖的保留本地的中文映射——限流（429）目录里就没有条目，
/// 不能硬套一个不存在的码（会落到「发生未知错误」）。
pub fn format_auth_error(err: AuthClientError) -> String {
    use rc_client_core::ui_error;
    let platform = ui_error::current_platform();
    // 目录文案：标题 +（恢复建议）
    let catalog = |code: &str, status: Option<u16>| {
        ui_error_text(&ui_error::resolve_ui_error(
            code,
            platform,
            &[],
            status,
            None,
        ))
    };
    match &err {
        AuthClientError::Api {
            status,
            code,
            message,
        } => {
            let trimmed = message.trim();
            match (status, code.as_str()) {
                (401, _) => catalog("auth.invalid_credentials", Some(*status)),
                (403, "password_change_required") => {
                    catalog("auth.password_change_required", Some(*status))
                }
                (403, _) => {
                    // 账号停用等具体原因由服务端 message 给出，保留在文案里
                    let mut text = catalog("auth.forbidden", Some(*status));
                    if !trimmed.is_empty() {
                        text.push_str(&format!(" 服务端说明：{trimmed}"));
                    }
                    text
                }
                (429, _) => "登录尝试过于频繁，请稍后再试。".to_string(),
                _ if !trimmed.is_empty() => trimmed.to_string(),
                _ => format!("请求失败（{code}）。"),
            }
        }
        AuthClientError::ReloginRequired(msg) => {
            let lower = msg.to_ascii_lowercase();
            let code = if lower.contains("reuse") || msg.contains("重放") {
                "auth.token_reuse_detected"
            } else {
                "auth.relogin_required"
            };
            catalog(code, None)
        }
        // 网络 / TLS / 配置 / 协议等本地错误：沿用错误自身的描述
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_auth_errors_to_chinese_messages() {
        let api = |status: u16, code: &str, message: &str| AuthClientError::Api {
            status,
            code: code.to_string(),
            message: message.to_string(),
        };
        // core 错误目录文案（标题 + 恢复建议）
        assert_eq!(
            format_auth_error(api(401, "unauthorized", "")),
            "账号或密码错误（请核对邮箱地址与密码（注意大小写）后重试；若忘记密码请联系管理员重置。）"
        );
        assert_eq!(
            format_auth_error(api(403, "password_change_required", "must change")),
            "请先修改初始密码（请立即设置新密码（8–128 字节），修改完成后将自动解锁全部功能。）"
        );
        // 403 其他原因：目录标题 + 服务端说明
        assert_eq!(
            format_auth_error(api(403, "device_disabled", "设备已被管理员停用")),
            "无权执行此操作（请确认当前账号角色，或联系服务器管理员授予相应权限。） 服务端说明：设备已被管理员停用"
        );
        // 目录没有限流条目：保留本地文案，不能落到「未知错误」
        assert_eq!(
            format_auth_error(api(429, "rate_limited", "")),
            "登录尝试过于频繁，请稍后再试。"
        );
        // 登录态失效：目录文案（不能被误判成网络不可达）
        assert_eq!(
            format_auth_error(AuthClientError::ReloginRequired(String::new())),
            "需要重新登录（请点击重新登录以恢复远控与设备同步服务。）"
        );
        assert_eq!(
            format_auth_error(AuthClientError::ReloginRequired("token_reuse".into())),
            "检测到异常令牌重放（请立即重新登录账号，如非本人操作建议尽快修改登录密码。）"
        );
        // 其余 API 错误保留服务端 message / 错误码
        assert_eq!(
            format_auth_error(api(500, "internal", "服务器内部错误")),
            "服务器内部错误"
        );
        assert_eq!(
            format_auth_error(api(502, "bad_gateway", "")),
            "请求失败（bad_gateway）。"
        );
        // 本地错误沿用自身描述
        assert_eq!(
            format_auth_error(AuthClientError::Network("timeout".into())),
            AuthClientError::Network("timeout".into()).to_string()
        );
    }

    #[test]
    fn resolves_non_empty_local_hostname() {
        assert!(!local_hostname().trim().is_empty());
    }
}
