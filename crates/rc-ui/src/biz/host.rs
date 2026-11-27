//! M5 被控端编排（GUI-free）：[`HostHandle`] 生命周期 + 事件归纳。
//!
//! v1 范围：LAN 直连 + 中继直入（relay incoming 经 `offer_relay_incoming`
//! 桥接）、临时 9 位码/自定义密码、机主审批（含同账号 OwnerTicket 免密）。
//! 信令侧主动推送连接预告的桥接（`SignalRuntime` 扩展）为下一批。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rc_client_core::auth_client::AuthSessionManager;
use rc_client_core::host_bridge::{HostHandle, HostUpdate, UiHostConfig};
use rc_client_core::relay_transport::SessionMtlsBundle;
use rc_session::auth::DEFAULT_AUTH_TIMEOUT_US;

/// 被控默认监听地址（端口被占自动回退临时端口）。
pub const DEFAULT_HOST_LISTEN: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 44350);

/// 被控运行时状态（App 持有；handle 可为 None = 未启用）。
pub struct HostRuntime {
    pub handle: Option<HostHandle>,
    /// 临时码（Listening 后上报）。
    pub temp_code: String,
    /// 被控会话进行中（Banner 计时展示）。
    pub session_active: bool,
    pub connected_peer: Option<String>,
}

impl HostRuntime {
    pub fn new() -> Self {
        Self {
            handle: None,
            temp_code: String::new(),
            session_active: false,
            connected_peer: None,
        }
    }

    /// 启动被控服务（密码可选；OwnerTicket 免密自控常开）。
    pub fn start(
        manager: &AuthSessionManager,
        password: Option<String>,
        allow_file_transfer: bool,
    ) -> Result<HostHandle, String> {
        let mtls_config = build_mtls(manager)?;
        let owner_verifier = build_owner_verifier(manager);
        let handle = HostHandle::spawn(UiHostConfig {
            listen_addr: DEFAULT_HOST_LISTEN,
            password,
            approval_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
            prefer_hardware: true,
            require_approval: true,
            auto_accept_with_password: true,
            auto_rotate_password: true,
            clipboard_enabled: true,
            audio_enabled: true,
            allow_owner_control: true,
            allow_remote_assist: true,
            allow_assist_file_transfer: allow_file_transfer,
            owner_verifier,
            mtls_config: Some(mtls_config),
            accept_mtls_lan: true,
            forbid_passwordless_auto_approve: false,
            file_transfer_dir: None,
        })?;
        Ok(handle)
    }

    /// 排空事件并归纳状态；返回审批请求（0 或 1 条，排队竞态时取最新）。
    pub fn poll(&mut self) -> Vec<HostUpdate> {
        let Some(handle) = self.handle.as_ref() else {
            return Vec::new();
        };
        let mut updates = Vec::new();
        while let Some(update) = handle.try_recv_update() {
            match &update {
                HostUpdate::Listening { .. } => {}
                HostUpdate::PasswordRotated { password } => {
                    self.temp_code = password.clone();
                }
                HostUpdate::ClientConnected { .. } => {
                    self.session_active = true;
                }
                HostUpdate::OwnerConnected { .. } => {
                    self.session_active = true;
                }
                HostUpdate::ClientDisconnected { .. } => {
                    self.session_active = false;
                    self.connected_peer = None;
                }
                HostUpdate::Stopped => {
                    self.session_active = false;
                }
                _ => {}
            }
            updates.push(update);
        }
        updates
    }

    /// 停止被控服务。
    pub fn stop(&mut self) {
        if let Some(mut handle) = self.handle.take() {
            handle.stop();
        }
        self.session_active = false;
        self.temp_code.clear();
        self.connected_peer = None;
    }
}

/// mTLS 束（与主控连接共用一套设备证书链）。
pub fn build_mtls(manager: &AuthSessionManager) -> Result<SessionMtlsBundle, String> {
    let cfg = manager.server_config();
    let session = manager.current_session();
    let ca_cert_pem = cfg
        .ca_cert_pem
        .clone()
        .ok_or_else(|| "服务器 CA 尚未锚定".to_string())?;
    let cert_manager = manager.cert_manager();
    let (cert_pem, _) = cert_manager
        .load_device_cert()
        .ok_or_else(|| "本机设备证书缺失".to_string())?;
    let key_pem = cert_manager.tls_key_pem()?;
    let local_user_id = session
        .as_ref()
        .map(|s| s.user_id.clone())
        .ok_or_else(|| "登录会话缺失".to_string())?;
    Ok(SessionMtlsBundle {
        ca_cert_pem,
        cert_pem,
        key_pem,
        local_user_id,
        expected_peer_device_id: None,
        expected_peer_user_id: None,
    })
}

/// OwnerTicket 免密自控校验器（服务端公钥就绪时构造；否则 None 走密码/审批）。
fn build_owner_verifier(
    manager: &AuthSessionManager,
) -> Option<Arc<dyn rc_session::auth::OwnerTicketVerifier>> {
    let cfg = manager.server_config();
    let pubkey_hex = cfg.server_auth_pubkey_hex.clone()?;
    let session = manager.current_session()?;
    // 绑定设备 ID 从 server 配置缓存取（登录会话未存 device_id）
    let device_id = manager
        .server_config()
        .cached_device_id
        .clone()
        .filter(|d| !d.is_empty())?;
    let verifier = rc_client_core::cert_manager::RcOwnerTicketVerifier::new(
        &pubkey_hex,
        session.user_id.clone(),
        device_id,
    )
    .ok()?;
    Some(Arc::new(verifier))
}

/// 中继连入桥接：host 在运行时移交，否则用本端证书快速拒绝（H10：避免对端空等超时）。
pub fn bridge_relay_incoming(
    handle: Option<&HostHandle>,
    incoming: rc_client_core::signal_client::RelayIncoming,
    manager: &AuthSessionManager,
) {
    if let Some(handle) = handle {
        handle.offer_relay_incoming(incoming);
        return;
    }
    let mtls = build_mtls(manager)
        .ok()
        .map(|b| rc_client_core::relay_transport::RelayMtlsConfig {
            ca_cert_pem: b.ca_cert_pem,
            client_cert_pem: b.cert_pem,
            client_key_pem: b.key_pem,
            server_name: None,
        });
    rc_client_core::relay_transport::reject_relay_incoming_fast(incoming, mtls);
}

/// 审批应答超时占位（对齐 core 常量语义）。
#[allow(dead_code)]
pub const APPROVAL_WAIT: Duration = Duration::from_secs(30);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_runtime_starts_idle() {
        let mut rt = HostRuntime::new();
        assert!(rt.handle.is_none());
        assert!(rt.poll().is_empty());
        rt.stop(); // 幂等
        assert!(!rt.session_active);
        assert!(rt.temp_code.is_empty());
    }
}
