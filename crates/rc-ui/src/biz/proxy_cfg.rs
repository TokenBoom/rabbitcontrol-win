//! 代理设置（#5）：设置页输入 ↔ core [`ProxyConfig`]。
//!
//! 规则：
//! - 留空 = 不使用代理；
//! - 地址里的账号密码会保留，但界面只显示 `协议://[用户名@]主机:端口`，
//!   **密码永不回显**；
//! - 输入没带密码、且用户名 / 主机 / 端口与已保存的一致时，沿用已保存的密码
//!   （重复点「应用」或只改别处不会把密码清掉）。
//!
//! 应用与持久化由调用方（`rc-ui` App）完成，这里只有纯函数。

#![deny(missing_docs)]

use rc_client_core::proxy::{ProxyConfig, ProxyMode, ProxyProtocol, ResolvedProxy};

/// 由输入的代理地址与已保存的配置得到新的代理配置。
///
/// 输入支持 `http://`、`socks5://`、`socks5h://`（可带 `用户名:密码@`）。
pub fn build(input: &str, saved: &ProxyConfig) -> Result<ProxyConfig, String> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(ProxyConfig::default());
    }
    let ResolvedProxy {
        protocol,
        host,
        port,
        username,
        password,
    } = rc_client_core::proxy::parse_proxy_url(input)
        .map_err(|err| format!("代理地址无效：{err}"))?;
    // 不带密码的地址解析出来是 Some("")：按「没有密码」处理
    let input_password = password.filter(|p| !p.is_empty());
    // 同一端点且同一用户：沿用已保存的密码
    let keep_saved = input_password.is_none()
        && matches!(saved.mode, ProxyMode::Manual)
        && saved.host == host
        && saved.port == port
        && saved.username == username;
    let saved_password = saved.password.clone().filter(|p| !p.is_empty());
    Ok(ProxyConfig {
        mode: ProxyMode::Manual,
        protocol: Some(protocol),
        host,
        port,
        username,
        password: input_password.or_else(|| keep_saved.then_some(saved_password).flatten()),
    })
}

/// 文本框显示用的地址（不含密码）；未启用手动代理时为空串。
pub fn display(cfg: &ProxyConfig) -> String {
    if !matches!(cfg.mode, ProxyMode::Manual) {
        return String::new();
    }
    let scheme = match cfg.protocol {
        Some(ProxyProtocol::Socks5) => "socks5",
        _ => "http",
    };
    match cfg.username.as_deref().filter(|u| !u.is_empty()) {
        Some(user) => format!("{scheme}://{user}@{}:{}", cfg.host, cfg.port),
        None => format!("{scheme}://{}:{}", cfg.host, cfg.port),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manual(url: &str, saved: &ProxyConfig) -> ProxyConfig {
        build(url, saved).expect("应解析成功")
    }

    #[test]
    fn empty_input_disables_proxy() {
        let cfg = manual("   ", &ProxyConfig::default());
        assert!(!matches!(cfg.mode, ProxyMode::Manual));
        assert_eq!(display(&cfg), "");
    }

    #[test]
    fn credentials_from_url_are_kept_and_password_never_shown() {
        let cfg = manual(
            "socks5://alice:s3cret@10.0.0.2:1080",
            &ProxyConfig::default(),
        );
        assert_eq!(cfg.protocol, Some(ProxyProtocol::Socks5));
        assert_eq!(cfg.host, "10.0.0.2");
        assert_eq!(cfg.port, 1080);
        assert_eq!(cfg.username.as_deref(), Some("alice"));
        assert_eq!(cfg.password.as_deref(), Some("s3cret"));
        let shown = display(&cfg);
        assert_eq!(shown, "socks5://alice@10.0.0.2:1080");
        assert!(!shown.contains("s3cret"), "密码不得回显");
    }

    #[test]
    fn saved_password_survives_when_endpoint_unchanged() {
        let saved = manual("http://bob:pw@proxy:8080", &ProxyConfig::default());
        // 用户看到的是不含密码的地址，原样再点「应用」
        let again = manual(&display(&saved), &saved);
        assert_eq!(again.password.as_deref(), Some("pw"));
        assert_eq!(again.username, saved.username);
        // 换了主机或用户：不再沿用旧密码
        let other_host = manual("http://bob@other:8080", &saved);
        assert_eq!(other_host.password, None);
        let other_user = manual("http://carol@proxy:8080", &saved);
        assert_eq!(other_user.password, None);
        // 显式输入新密码：以输入为准
        let rotated = manual("http://bob:new@proxy:8080", &saved);
        assert_eq!(rotated.password.as_deref(), Some("new"));
    }

    #[test]
    fn http_without_credentials() {
        let cfg = manual("http://127.0.0.1:7890", &ProxyConfig::default());
        assert_eq!(cfg.protocol, Some(ProxyProtocol::Http));
        assert_eq!(cfg.username, None);
        assert_eq!(display(&cfg), "http://127.0.0.1:7890");
    }

    #[test]
    fn invalid_input_is_rejected() {
        let saved = ProxyConfig::default();
        assert!(build("not a url", &saved).is_err());
        assert!(
            build("https://proxy:443", &saved).is_err(),
            "core 明确拒绝 https 代理"
        );
        assert!(build("ftp://proxy:21", &saved).is_err());
        assert!(build("socks5://proxy:99999", &saved).is_err());
    }
}
