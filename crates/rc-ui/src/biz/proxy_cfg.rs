//! 代理设置集成（#5）：设置页网络 tab ↔ core ProxyConfig。
use rc_client_core::proxy::ProxyConfig;

/// 从设置页输入构建 ProxyConfig 并应用到全局。
#[allow(dead_code)] // UI 接线待下批
pub fn apply(proxy_url: &str) -> Result<(), String> {
    let cfg = if proxy_url.trim().is_empty() {
        ProxyConfig::default() // None = 直连
    } else {
        let resolved = rc_client_core::proxy::parse_proxy_url(proxy_url.trim())
            .map_err(|e| format!("代理地址无效：{e}"))?;
        ProxyConfig {
            mode: rc_client_core::proxy::ProxyMode::Manual,
            protocol: Some(resolved.protocol),
            host: resolved.host.clone(),
            port: resolved.port,
            username: None,
            password: None,
        }
    };
    rc_client_core::proxy::set_global_proxy_config(cfg);
    Ok(())
}
