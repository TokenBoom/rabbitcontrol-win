use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rc_client_core::auth_client::{
    self, AuthClientError, AuthHttpClient, AuthMaintainerHandle, AuthSessionManager,
    BindDeviceRequest, MyDeviceInfo,
};
use rc_client_core::cert_manager::CertManager;
use rc_client_core::server_config::{self, ServerConfig};

use crate::biz::settings;

const MAINTAINER_INTERVAL: Duration = Duration::from_secs(30);

/// 登录或恢复会话后的账号与本机设备信息。
#[derive(Debug, Clone)]
pub struct AccountSession {
    pub email: String,
    pub device_id: Option<String>,
    pub devices: Vec<MyDeviceInfo>,
}

/// 进程内认证运行时，统一托管会话、设备证书与后台自动续期线程。
pub struct AuthRuntime {
    server_path: PathBuf,
    manager: Arc<AuthSessionManager>,
    maintainer: Option<AuthMaintainerHandle>,
}

impl AuthRuntime {
    /// 初始化认证运行时与本地设备密钥存储。
    pub fn new() -> Result<Self, String> {
        let server_path = server_config::default_config_path();
        let cfg = settings::load_config_from(&server_path);
        let cert_mgr = CertManager::default_dir()?;
        let manager = Arc::new(AuthSessionManager::new(
            cfg,
            auth_client::default_auth_path(),
            Arc::new(cert_mgr),
        ));
        Ok(Self {
            server_path,
            manager,
            maintainer: None,
        })
    }

    pub fn server_config(&self) -> ServerConfig {
        self.manager.server_config()
    }

    /// 检查本地是否同时存在已配置服务器与持久化登录凭据。
    pub fn has_saved_session(&self) -> bool {
        self.manager.server_config().is_configured()
            && auth_client::load_auth_session(&self.manager.auth_path()).is_some()
    }

    pub fn is_logged_in(&self) -> bool {
        self.manager.current_session().is_some()
    }

    pub fn manager(&self) -> Arc<AuthSessionManager> {
        Arc::clone(&self.manager)
    }

    pub fn server_path(&self) -> PathBuf {
        self.server_path.clone()
    }

    /// 启动后台 Token 轮换与设备证书自动续签线程。
    pub fn start_maintainer(&mut self, on_relogin: impl Fn(String) + Send + Sync + 'static) {
        self.stop_maintainer();
        let mgr_for_id = Arc::clone(&self.manager);
        let handle = self.manager.spawn_background_maintainer_with_callbacks(
            MAINTAINER_INTERVAL,
            move || mgr_for_id.server_config().cached_device_id,
            || {},
            on_relogin,
        );
        self.maintainer = Some(handle);
    }

    /// 停止并回收后台维护线程。
    pub fn stop_maintainer(&mut self) {
        if let Some(mut handle) = self.maintainer.take() {
            handle.stop();
        }
    }
}

impl Drop for AuthRuntime {
    fn drop(&mut self) {
        self.stop_maintainer();
    }
}

// 本机标识与认证错误文案下沉到共享 crate（后台被控进程同样需要）
pub use rc_appcore::auth_text::{format_auth_error, local_hostname};

/// 探测并保存新服务器配置；若端点变更则清理旧凭据并在后台通知旧服务器登出。
pub fn probe_and_save_server(
    manager: &AuthSessionManager,
    server_path: &Path,
    input: &str,
) -> Result<ServerConfig, String> {
    let current = manager.server_config();
    let (next, endpoint_changed) = settings::probe_and_save_server(&current, input, server_path)?;
    if endpoint_changed {
        let (old_cfg, prev_sess) = manager.take_logout_context_and_wipe();
        if prev_sess.is_some() {
            let old_did = old_cfg.cached_device_id.clone();
            let _ = thread::Builder::new()
                .name("rc-win-old-logout".to_string())
                .spawn(move || {
                    let _ = AuthSessionManager::notify_remote_logout(
                        &old_cfg,
                        prev_sess.as_ref(),
                        old_did.as_deref(),
                    );
                });
        }
    }
    manager.update_server_config(next.clone());
    Ok(next)
}

/// 启动时静默恢复磁盘会话，按需刷新 Token 并补齐设备绑定与 mTLS 证书。
pub fn restore_session(
    manager: &AuthSessionManager,
    server_path: &Path,
) -> Result<Option<AccountSession>, String> {
    if !manager.server_config().is_configured() {
        return Ok(None);
    }
    let restore_result = manager.restore_on_startup().map_err(format_auth_error);
    let sess = match restore_result {
        Ok(Some(s)) => s,
        Err(err) if err.contains("completed successfully") => {
            // v0.1.24 restore_on_startup 在某些场景下返回 HRESULT 0 作为错误，
            // 视为未保存会话，回退到手动登录。
            eprintln!("[auth] restore_on_startup returned success-as-error, ignoring");
            return Ok(None);
        }
        Ok(None) => return Ok(None),
        Err(err) => return Err(err),
    };

    if sess.must_change_password {
        manager.wipe_local_credentials();
        return Err("该账号需要先在 RabbitControl 管理网页修改初始密码。".to_string());
    }

    let device_id = match manager.server_config().cached_device_id {
        Some(did) => did,
        None => match bind_device_and_ensure_cert(manager, server_path) {
            Ok(did) => did,
            Err(err) => {
                manager.wipe_local_credentials();
                return Err(err);
            }
        },
    };
    let devices = list_my_devices(manager).unwrap_or_default();
    Ok(Some(AccountSession {
        email: sess.email,
        device_id: Some(device_id),
        devices,
    }))
}

/// 使用邮箱与密码登录，并在成功后绑定本机设备及签发 mTLS 证书。
pub fn login(
    manager: &AuthSessionManager,
    server_path: &Path,
    email: &str,
    password: &str,
) -> Result<AccountSession, String> {
    if !manager.server_config().is_configured() {
        return Err("请先配置并校验 RabbitControl 服务器地址。".to_string());
    }

    let hostname = local_hostname();
    let sess = manager
        .login(email, password, Some(&hostname), Some(std::env::consts::OS))
        .map_err(format_auth_error)?;

    if sess.must_change_password {
        let _ = manager.logout(None);
        return Err("该账号需要先在 RabbitControl 管理网页修改初始密码。".to_string());
    }

    let device_id = match bind_device_and_ensure_cert(manager, server_path) {
        Ok(did) => did,
        Err(err) => {
            let _ = manager.logout(None);
            return Err(err);
        }
    };
    let devices = list_my_devices(manager).unwrap_or_default();
    Ok(AccountSession {
        email: sess.email,
        device_id: Some(device_id),
        devices,
    })
}

/// 清空本地会话与设备证书，并尽量通知服务端吊销当前会话。
pub fn logout(manager: &AuthSessionManager) -> Result<(), String> {
    let (old_cfg, prev_sess) = manager.take_logout_context_and_wipe();
    let device_id = old_cfg.cached_device_id.clone();
    if prev_sess.is_some() {
        AuthSessionManager::notify_remote_logout(
            &old_cfg,
            prev_sess.as_ref(),
            device_id.as_deref(),
        )
        .map_err(|_| "本地已退出。服务器未确认会话注销。".to_string())?;
    }
    Ok(())
}

/// 查询当前账号绑定的所有设备列表，遇未授权时自动轮换 Token 重试一次。
pub fn list_my_devices(manager: &AuthSessionManager) -> Result<Vec<MyDeviceInfo>, String> {
    with_valid_token(manager, |client, token| client.list_my_devices(token))
        .map_err(format_auth_error)
}

/// 重命名设备（对齐 iOS `renameDevice`）。
pub fn rename_device(
    manager: &AuthSessionManager,
    device_id: &str,
    alias: &str,
) -> Result<(), String> {
    with_valid_token(manager, |client, token| {
        client.rename_device(token, device_id, alias)
    })
    .map(|_| ())
    .map_err(format_auth_error)
}

/// 解绑设备（对齐 iOS `removeDevice`）。
pub fn remove_device(manager: &AuthSessionManager, device_id: &str) -> Result<(), String> {
    with_valid_token(manager, |client, token| {
        client.remove_device(token, device_id)
    })
    .map_err(format_auth_error)
}

fn bind_device_and_ensure_cert(
    manager: &AuthSessionManager,
    server_path: &Path,
) -> Result<String, String> {
    let cfg = manager.server_config();
    let pubkey_hex = manager.cert_manager().device_pubkey_hex();
    let hostname = local_hostname();
    let os_name = std::env::consts::OS.to_string();

    let bound = with_valid_token(manager, |client, token| {
        client.bind_device(
            token,
            &BindDeviceRequest {
                device_key: cfg.device_key.clone(),
                hostname: Some(hostname.clone()),
                os: Some(os_name.clone()),
                app_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                arch: Some(std::env::consts::ARCH.to_string()),
                alias: None,
                device_pubkey: Some(pubkey_hex.clone()),
                allow_remote_assist: Some(true),
            },
        )
    })
    .map_err(format_auth_error)?;

    let mut updated_cfg = manager.server_config();
    updated_cfg.cached_device_id = Some(bound.device_id.clone());
    server_config::save_to_path(server_path, &updated_cfg)?;
    manager.update_server_config(updated_cfg);

    if let Some(sess) = manager.current_session() {
        manager
            .cert_manager()
            .set_expected_binding(&sess.user_id, &bound.device_id);
    }
    manager
        .ensure_device_cert(&bound.device_id)
        .map_err(format_auth_error)?;
    Ok(bound.device_id)
}

fn with_valid_token<T>(
    manager: &AuthSessionManager,
    op: impl Fn(&AuthHttpClient, &str) -> Result<T, AuthClientError>,
) -> Result<T, AuthClientError> {
    let cfg = manager.server_config();
    let client = AuthHttpClient::from_server_config(&cfg)?;
    let token = manager.ensure_valid_access_token()?;
    match op(&client, &token) {
        Ok(res) => Ok(res),
        Err(err) if err.is_unauthorized() => {
            let fresh = manager.refresh_after_unauthorized(&token)?;
            op(&client, &fresh)
        }
        Err(err) => Err(err),
    }
}
