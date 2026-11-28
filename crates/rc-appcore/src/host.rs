//! M5 被控端编排（GUI-free）：[`HostHandle`] 生命周期 + 事件归纳。
//!
//! v1 范围：LAN 直连 + 中继直入（relay incoming 经 `offer_relay_incoming`
//! 桥接）、临时 9 位码/自定义密码、机主审批（含同账号 OwnerTicket 免密）。
//! 信令侧主动推送连接预告的桥接（`SignalRuntime` 扩展）为下一批。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rc_client_core::auth_client::AuthSessionManager;
use rc_client_core::host_bridge::{generate_temp_code, HostHandle, HostUpdate, UiHostConfig};
use rc_client_core::relay_transport::SessionMtlsBundle;
use rc_client_core::signal_client::SignalClientHandle;
use rc_session::auth::DEFAULT_AUTH_TIMEOUT_US;

/// 被控默认监听地址（端口被占自动回退临时端口）。
pub const DEFAULT_HOST_LISTEN: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 44350);

/// 启动成功的被控服务：句柄 + 当前访问密码（自定义密码，或刚生成的临时码）。
pub struct StartedHost {
    /// 被控服务句柄。
    pub handle: HostHandle,
    /// 当前访问密码（交给被控服务的那个；临时码在每次会话结束后由 core 轮换）。
    pub password: String,
}

/// 被控运行时状态（持有方为 `None` 时表示未启用）。
#[derive(Default)]
pub struct HostRuntime {
    /// 被控服务句柄（未启用时为 `None`）。
    pub handle: Option<HostHandle>,
    /// 临时码（Listening 后上报）。
    pub temp_code: String,
    /// 被控会话进行中（Banner 计时展示）。
    pub session_active: bool,
    /// 正在控制本机的远端描述。
    pub connected_peer: Option<String>,
}

/// 启动被控服务的可调项（都来自用户设置；主界面与后台进程共用同一份偏好）。
#[derive(Debug, Clone, Default)]
pub struct HostOptions {
    /// 自定义访问密码；`None` = 用临时验证码（每次远控结束自动刷新）。
    pub password: Option<String>,
    /// 他人远程协助本机时是否允许文件互传。
    pub allow_file_transfer: bool,
    /// 是否允许非同账号的他人远程协助（`false` = 只允许同账号本人自控）。
    pub allow_remote_assist: bool,
    /// 局域网直连监听是否强制 mTLS（与「默认开启加密连接」同一个开关）。
    pub accept_mtls_lan: bool,
    /// 信令客户端句柄（core v0.2.6+：中继 → P2P 复探用）。
    pub signal_client: Option<Arc<SignalClientHandle>>,
}

impl HostRuntime {
    /// 未启用状态下的被控运行时。
    pub fn new() -> Self {
        Self {
            handle: None,
            temp_code: String::new(),
            session_active: false,
            connected_peer: None,
        }
    }

    /// 启动被控服务（OwnerTicket 免密自控常开）。
    ///
    /// 没设自定义密码时先发一个临时码：core 只在一次会话结束后轮换临时码，
    /// 第一个要由应用生成并交给被控服务（与 macOS agent 的做法一致）。
    pub fn start(
        manager: &AuthSessionManager,
        options: HostOptions,
    ) -> Result<StartedHost, String> {
        let HostOptions {
            password,
            allow_file_transfer,
            allow_remote_assist,
            accept_mtls_lan,
            signal_client,
        } = options;
        let mtls_config = build_mtls(manager)?;
        let owner_verifier = build_owner_verifier(manager);
        let custom = password.is_some();
        let password = password.unwrap_or_else(generate_temp_code);
        let handle = HostHandle::spawn(UiHostConfig {
            listen_addr: DEFAULT_HOST_LISTEN,
            password: Some(password.clone()),
            approval_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
            prefer_hardware: true,
            require_approval: true,
            auto_accept_with_password: true,
            // 临时码每次远控结束换一个；自定义密码固定不变
            auto_rotate_password: !custom,
            clipboard_enabled: true,
            audio_enabled: true,
            allow_owner_control: true,
            allow_remote_assist,
            allow_assist_file_transfer: allow_file_transfer,
            owner_verifier,
            mtls_config: Some(mtls_config),
            accept_mtls_lan,
            forbid_passwordless_auto_approve: false,
            file_transfer_dir: None,
            signal_client,
        })?;
        Ok(StartedHost { handle, password })
    }

    /// 信令客户端换了一个（重建 / 停止）后同步给被控服务（core v0.2.6+）。
    ///
    /// 被控服务留着它的句柄是为了会话运行期「中继 → P2P」复探时刷新 STUN 映射并复用
    /// UDP 端口；句柄过期不复探，所以信令一换就要跟着换，停了就交回 `None`。
    pub fn set_signal_client(&mut self, signal_client: Option<Arc<SignalClientHandle>>) {
        if let Some(handle) = self.handle.as_ref() {
            handle.set_signal_client(signal_client);
        }
    }

    /// 切换访问密码模式：`password` 为 `None` 时换一个新的临时码（每次远控结束再换），
    /// 否则用固定的自定义密码。对下一次新握手生效；core 不会为手动切换发事件，
    /// 所以当前访问码在这里就地更新。
    pub fn set_password_mode(&mut self, password: Option<String>) {
        let Some(handle) = self.handle.as_ref() else {
            return;
        };
        let custom = password.is_some();
        let password = password.unwrap_or_else(generate_temp_code);
        handle.set_password_mode(Some(password.clone()), !custom);
        self.temp_code = password;
    }

    /// 实时切换「允许他人远程协助」门控（被控服务未运行时什么都不做）。
    pub fn set_allow_remote_assist(&mut self, allow: bool) {
        if let Some(handle) = self.handle.as_ref() {
            handle.set_allow_remote_assist(allow);
        }
    }

    /// 设备证书续签后把新的 mTLS 束交给被控服务（会话中途换证书，不必重启被控服务）。
    pub fn refresh_mtls(
        &mut self,
        manager: &AuthSessionManager,
        accept_mtls_lan: bool,
    ) -> Result<(), String> {
        let Some(handle) = self.handle.as_ref() else {
            return Ok(());
        };
        let mtls_config = build_mtls(manager)?;
        handle.set_mtls_config(Some(mtls_config), accept_mtls_lan);
        Ok(())
    }

    /// 安放刚启动的被控服务（句柄 + 当前访问密码）。
    pub fn install(&mut self, started: StartedHost) {
        self.temp_code = started.password;
        self.handle = Some(started.handle);
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
        self.reset();
    }

    /// 停止被控服务，并向在途控制端发送指定的告别原因
    /// （`ByeReason::SessionSwitching` 可让控制端自动重连到接管方）。
    pub fn stop_with_reason(&mut self, reason: rc_session::proto::ByeReason) {
        if let Some(mut handle) = self.handle.take() {
            handle.stop_with_reason(reason);
        }
        self.reset();
    }

    /// 退出登录时停掉被控服务：没有账号就没有 mTLS 身份，不该再对外服务；
    /// 在途控制端收到「正常结束」的告别。
    pub fn stop_for_logout(&mut self) {
        self.stop_with_reason(rc_session::proto::ByeReason::Normal);
    }

    fn reset(&mut self) {
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
