use std::net::IpAddr;
use std::path::Path;

use rc_client_core::auth_client::AuthHttpClient;
use rc_client_core::server_config::{self, ServerConfig};

use crate::auth;

/// 首次配置时的默认服务器输入地址。
pub const DEFAULT_ORIGIN_INPUT: &str = "https://127.0.0.1:21118";

/// 解析后的服务器 API 接入端点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerOrigin {
    pub host: String,
    pub api_port: u16,
    pub use_https: bool,
}

/// 解析用户输入的服务器地址，规范化协议、主机名与端口。
pub fn parse_origin(input: &str) -> Result<ServerOrigin, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("请输入有效的服务器地址，例如 https://example.com:21118。".to_string());
    }
    if trimmed.chars().any(char::is_whitespace) || trimmed.chars().any(char::is_control) {
        return Err("服务器地址不能包含空白或控制字符。".to_string());
    }

    let (use_https, default_port, remainder) =
        if let Some(rest) = strip_prefix_ignore_case(trimmed, "https://") {
            (true, 443, rest)
        } else if let Some(rest) = strip_prefix_ignore_case(trimmed, "http://") {
            (false, 80, rest)
        } else if trimmed.contains("://") {
            return Err("仅支持 HTTPS 或本机 HTTP 调试地址。".to_string());
        } else {
            (true, server_config::DEFAULT_API_PORT, trimmed)
        };

    let host_port = remainder.trim_end_matches('/');
    if host_port.is_empty() {
        return Err("请输入有效的服务器地址，例如 https://example.com:21118。".to_string());
    }
    if host_port.contains(['/', '?', '#', '@']) {
        return Err("仅填写协议、主机和端口，请移除路径、账号或查询参数。".to_string());
    }

    let (host, api_port) = server_config::parse_server_input(host_port, default_port)?;
    if !use_https {
        let is_loopback = host.eq_ignore_ascii_case("localhost")
            || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
        if !is_loopback {
            return Err("远程服务器需使用 HTTPS。本机调试可使用 HTTP。".to_string());
        }
    }

    Ok(ServerOrigin {
        host,
        api_port,
        use_https,
    })
}

fn strip_prefix_ignore_case<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    value
        .get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &value[prefix.len()..])
}

/// 格式化用于状态栏显示的服务器地址。
pub fn format_origin(cfg: &ServerConfig) -> String {
    if !cfg.is_configured() {
        return "未配置服务器".to_string();
    }
    cfg.api_base_url()
        .unwrap_or_else(|| "未配置服务器".to_string())
}

/// 生成设置页输入框的初始可编辑服务器地址。
pub fn editable_origin(cfg: &ServerConfig) -> String {
    cfg.api_base_url()
        .unwrap_or_else(|| DEFAULT_ORIGIN_INPUT.to_string())
}

/// 从指定路径加载服务器配置，失败或缺失时回退默认配置。
pub fn load_config_from(path: &Path) -> ServerConfig {
    server_config::load_from_path(path)
}

/// 探测目标服务器健康状态与 Root CA，并原子写入配置文件；返回新配置及端点是否发生变化。
pub fn probe_and_save_server(
    current: &ServerConfig,
    input: &str,
    path: &Path,
) -> Result<(ServerConfig, bool), String> {
    let origin = parse_origin(input)?;
    let client = AuthHttpClient::new(origin.host.clone(), origin.api_port, origin.use_https, None)
        .map_err(auth::format_auth_error)?;
    let probe = client.probe_server(None).map_err(auth::format_auth_error)?;

    let ca_fp = probe
        .ca_bundle
        .as_ref()
        .map(|b| b.ca_fingerprint_sha256.clone())
        .or_else(|| probe.health.ca_fingerprint_sha256.clone());

    let endpoint_changed = !current.enabled
        || current.server_host != origin.host
        || current.api_port != origin.api_port
        || current.use_https != origin.use_https
        || current.ca_fingerprint_sha256 != ca_fp;

    let next = ServerConfig {
        enabled: true,
        server_host: origin.host,
        signal_port: if current.signal_port == 0 {
            server_config::DEFAULT_SIGNAL_PORT
        } else {
            current.signal_port
        },
        relay_port: if current.relay_port == 0 {
            server_config::DEFAULT_RELAY_PORT
        } else {
            current.relay_port
        },
        api_port: origin.api_port,
        use_https: origin.use_https,
        ca_fingerprint_sha256: ca_fp,
        ca_cert_pem: probe.ca_bundle.as_ref().map(|b| b.ca_cert_pem.clone()),
        server_auth_pubkey_hex: probe.ca_bundle.as_ref().map(|b| b.auth_pubkey_hex.clone()),
        device_key: current.device_key.clone(),
        cached_device_id: if endpoint_changed {
            None
        } else {
            current.cached_device_id.clone()
        },
    };

    server_config::save_to_path(path, &next)?;
    Ok((next, endpoint_changed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn resolves_server_origins() {
        assert_eq!(
            parse_origin("host.local").unwrap(),
            ServerOrigin {
                host: "host.local".to_string(),
                api_port: 21118,
                use_https: true,
            }
        );
        assert_eq!(
            parse_origin("host.local:443").unwrap(),
            ServerOrigin {
                host: "host.local".to_string(),
                api_port: 443,
                use_https: true,
            }
        );
        assert_eq!(
            parse_origin("https://host.local").unwrap(),
            ServerOrigin {
                host: "host.local".to_string(),
                api_port: 443,
                use_https: true,
            }
        );
        assert_eq!(
            parse_origin("https://host.local:8443/").unwrap(),
            ServerOrigin {
                host: "host.local".to_string(),
                api_port: 8443,
                use_https: true,
            }
        );
        assert_eq!(
            parse_origin("http://[::1]:32100").unwrap(),
            ServerOrigin {
                host: "::1".to_string(),
                api_port: 32100,
                use_https: false,
            }
        );
        assert_eq!(
            parse_origin("http://localhost").unwrap(),
            ServerOrigin {
                host: "localhost".to_string(),
                api_port: 80,
                use_https: false,
            }
        );
        for invalid in [
            "",
            "   ",
            "https://one.test/api",
            "http://one.test",
            "https://user:pass@one.test",
            "https://one.test/#f",
            "https://one.test?a=1",
            "tcp://one.test:21118",
        ] {
            assert!(parse_origin(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn load_and_save_server_config_roundtrip() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("rc-win-settings-test-{nanos}"));
        let path = dir.join("server.json");

        let default_cfg = load_config_from(&path);
        assert!(!default_cfg.is_configured());
        assert_eq!(format_origin(&default_cfg), "未配置服务器");
        assert_eq!(editable_origin(&default_cfg), DEFAULT_ORIGIN_INPUT);

        let saved = ServerConfig {
            enabled: true,
            server_host: "127.0.0.1".to_string(),
            signal_port: 21116,
            relay_port: 21117,
            api_port: 21118,
            use_https: false,
            ca_fingerprint_sha256: None,
            ca_cert_pem: None,
            server_auth_pubkey_hex: None,
            device_key: default_cfg.device_key.clone(),
            cached_device_id: Some("604928235".to_string()),
        };
        server_config::save_to_path(&path, &saved).unwrap();

        let loaded = load_config_from(&path);
        assert_eq!(loaded, saved);
        assert_eq!(format_origin(&loaded), "http://127.0.0.1:21118");
        assert_eq!(editable_origin(&loaded), "http://127.0.0.1:21118");

        let _ = fs::remove_dir_all(&dir);
    }
}
