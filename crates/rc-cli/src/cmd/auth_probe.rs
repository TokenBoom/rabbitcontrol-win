//! `auth-probe` 联调回归（L1s）：服务器探测 → 登录 → 设备列表。
//!
//! 纯内存路径：不读取、不写入任何本地配置（`server.json` / `auth.json`
//! 不受影响），凭据仅驻留命令行参数与进程内存，供里程碑验收冒烟。

use rc_client_core::auth_client::AuthHttpClient;
use rc_client_core::server_config::{self, ServerConfig};

use std::net::IpAddr;

/// 执行完整回归链路，逐段打印结果。
pub fn run(origin: &str, email: &str, password: &str) -> Result<(), String> {
    let cfg = server_config_from(origin)?;

    println!("[1/3] 探测 {origin} …");
    let client = AuthHttpClient::new(
        cfg.server_host.clone(),
        cfg.api_port,
        cfg.use_https,
        cfg.ca_cert_pem.clone(),
    )
    .map_err(|err| format!("构造客户端失败：{err:?}"))?;
    let probe = client
        .probe_server(None)
        .map_err(|err| format!("服务器探测失败：{err:?}"))?;
    println!(
        "      健康：{}（版本 {}）",
        probe.health.status, probe.health.version
    );
    let ca_pem = probe
        .ca_bundle
        .as_ref()
        .map(|ca| ca.ca_cert_pem.clone())
        .ok_or_else(|| "服务器未返回 CA bundle".to_string())?;
    println!(
        "      Root CA 指纹：{}",
        probe
            .ca_bundle
            .as_ref()
            .map(|ca| ca.ca_fingerprint_sha256.as_str())
            .unwrap_or("-")
    );

    // TOFU：用探测锚定的 Root CA 重建客户端，后续请求据此校验 TLS。
    let client = AuthHttpClient::new(
        cfg.server_host.clone(),
        cfg.api_port,
        cfg.use_https,
        Some(ca_pem),
    )
    .map_err(|err| format!("锚定 CA 后构造客户端失败：{err:?}"))?;

    println!("[2/3] 登录 {email} …");
    let request = rc_client_core::auth_client::LoginRequest {
        email: email.to_string(),
        password: password.to_string(),
        device_key: None,
        hostname: Some(local_hostname()),
        os: Some(std::env::consts::OS.to_string()),
        app_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        arch: Some(std::env::consts::ARCH.to_string()),
        device_pubkey: None,
    };
    let session = client
        .login(&request)
        .map_err(|err| format!("登录失败：{err:?}"))?;
    println!("      成功：{} s 后令牌过期", session.expires_in_sec);

    println!("[3/3] 拉取设备列表 …");
    let devices = client
        .list_my_devices(&session.access_token)
        .map_err(|err| format!("设备列表失败：{err:?}"))?;
    println!("      共 {} 台设备：", devices.len());
    for device in &devices {
        println!(
            "      {} {}（{}，{}）",
            server_config::format_device_id(&device.device_id),
            if device.alias.trim().is_empty() {
                device.hostname.as_str()
            } else {
                device.alias.as_str()
            },
            device.os,
            if device.online { "在线" } else { "离线" },
        );
    }

    println!("L1s 回归通过：探测 / 登录 / 设备列表 全部成功。");
    Ok(())
}

/// 把 origin 参数规范化为内存中的 ServerConfig（仅 API 段，不落盘）。
fn server_config_from(origin: &str) -> Result<ServerConfig, String> {
    let trimmed = origin.trim();
    if trimmed.is_empty() {
        return Err("origin 不能为空".into());
    }
    let (use_https, default_port, remainder) = if let Some(rest) = trimmed.strip_prefix("https://")
    {
        (true, 443, rest)
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        (false, 80, rest)
    } else {
        (true, server_config::DEFAULT_API_PORT, trimmed)
    };
    let (host, api_port) =
        server_config::parse_server_input(remainder.trim_end_matches('/'), default_port)?;
    if !use_https {
        let loopback = host.eq_ignore_ascii_case("localhost")
            || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
        if !loopback {
            return Err("远程服务器需使用 HTTPS（本机调试可用 http://loopback）".into());
        }
    }
    Ok(ServerConfig {
        enabled: true,
        server_host: host,
        signal_port: server_config::DEFAULT_SIGNAL_PORT,
        relay_port: server_config::DEFAULT_RELAY_PORT,
        api_port,
        use_https,
        ca_fingerprint_sha256: None,
        ca_cert_pem: None,
        server_auth_pubkey_hex: None,
        device_key: String::new(),
        cached_device_id: None,
    })
}

/// 本机主机名（诊断输出用）。
fn local_hostname() -> String {
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
