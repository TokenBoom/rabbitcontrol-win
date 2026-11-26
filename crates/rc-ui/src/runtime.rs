//! 信令运行时：登录后拉起/重建信令长连接，代计数器防异步竞态。
//!
//! 语义对齐 iOS `RuntimeService`：`sync` 收口「停旧 → 重建」，每次调用
//! 递增代计数器；后台结果落地前必须校验代数一致，避免旧信令的事件
//! 窜入新会话。GUI-free：不引用任何 windows 类型，可单测。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rc_client_core::auth_client::AuthSessionManager;
use rc_client_core::lan_discovery::LanDiscoveryHandle;
use rc_client_core::server_config::ServerStatusSnapshot;
use rc_client_core::signal_client::{
    ConnectGrant, DeviceStatusEvent, SignalClientHandle, SignalClientOptions, TokenProvider,
    TokenRefresher,
};

use crate::auth;

/// 单次 `poll` 排空得到的事件集合。
#[derive(Debug, Default)]
pub struct RuntimeEvents {
    /// 最新信令状态快照（若有变化）。
    pub status: Option<ServerStatusSnapshot>,
    /// 账号设备在线状态变化。
    pub device_events: Vec<DeviceStatusEvent>,
    /// 被服务端踢下线的原因（改密/解绑/吊销等），触发回登录页。
    pub kicked_reason: Option<String>,
}

struct Inner {
    signal: Option<SignalClientHandle>,
    lan: Option<LanDiscoveryHandle>,
}

/// 信令运行时句柄。
pub struct SignalRuntime {
    manager: Arc<AuthSessionManager>,
    inner: Mutex<Inner>,
    generation: AtomicU64,
}

impl SignalRuntime {
    /// 基于认证运行时构建信令运行时。
    pub fn new(manager: Arc<AuthSessionManager>) -> Self {
        Self {
            manager,
            inner: Mutex::new(Inner {
                signal: None,
                lan: None,
            }),
            generation: AtomicU64::new(0),
        }
    }

    /// 当前代数（后台回投事件时据此校验是否仍为最新）。
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// 登录/恢复会话后同步：停掉旧信令与 LAN 发现，按当前服务器配置重建。
    ///
    /// 返回新代数；服务器未配置时同样成功（信令保持 Disabled 态，
    /// `snapshot` 可探测到）。
    pub fn sync(&self) -> Result<u64, String> {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let cfg = self.manager.server_config();
        let manager = Arc::clone(&self.manager);

        let token_provider: TokenProvider = Arc::new(move || {
            manager
                .ensure_valid_access_token()
                .map_err(auth::format_auth_error)
        });
        let manager_refresh = Arc::clone(&self.manager);
        let token_refresher: TokenRefresher = Arc::new(move |stale| {
            manager_refresh
                .refresh_after_unauthorized(stale)
                .map_err(auth::format_auth_error)
        });
        let device_pubkey = Some(self.manager.cert_manager().device_pubkey_hex());

        let mut inner = self.inner.lock().expect("信令运行时锁");
        if let Some(mut old) = inner.signal.take() {
            old.stop();
        }
        inner.lan = None;

        let signal = SignalClientHandle::spawn_with_options(
            cfg,
            auth::local_hostname(),
            Vec::new(),
            SignalClientOptions {
                token_provider: Some(token_provider),
                token_refresher: Some(token_refresher),
                device_pubkey,
                allow_remote_assist: true,
                dynamic_lan_discovery: true,
            },
            |_| {},
        );
        inner.signal = Some(signal);
        // LAN 发现：随机实例 ID 每进程唯一即可（对端按实例 ID 去重展示）。
        let instance_id: u64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        inner.lan = Some(LanDiscoveryHandle::spawn(
            instance_id,
            auth::local_hostname(),
        ));
        Ok(generation)
    }

    /// 登出：递增代数并停止信令与 LAN 发现。
    pub fn stop(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        let mut inner = self.inner.lock().expect("信令运行时锁");
        if let Some(mut signal) = inner.signal.take() {
            signal.stop();
        }
        inner.lan = None;
    }

    /// 当前信令状态快照（未启动时返回 None）。
    // M2 状态条升级时消费；当前仅供单测。
    #[allow(dead_code)]
    pub fn snapshot(&self) -> Option<ServerStatusSnapshot> {
        self.inner
            .lock()
            .expect("信令运行时锁")
            .signal
            .as_ref()
            .map(|signal| signal.status())
    }

    /// 排空全部待处理事件。
    pub fn poll(&self) -> RuntimeEvents {
        let inner = self.inner.lock().expect("信令运行时锁");
        let mut events = RuntimeEvents::default();
        if let Some(signal) = inner.signal.as_ref() {
            while let Some(status) = signal.try_recv_status() {
                events.status = Some(status);
            }
            while let Some(event) = signal.try_recv_device_status() {
                events.device_events.push(event);
            }
            if let Some(reason) = signal.try_recv_kicked_reason() {
                events.kicked_reason = Some(reason);
            }
        }
        events
    }

    /// 实时切换「允许他人远程协助」门控。
    pub fn set_allow_remote_assist(&self, allow: bool) -> Result<(), String> {
        let inner = self.inner.lock().expect("信令运行时锁");
        match inner.signal.as_ref() {
            Some(signal) => signal.set_allow_remote_assist(allow),
            None => Err("信令未运行，请先登录".into()),
        }
    }

    /// 发起 9 位码连接请求（阻塞，调用方须在工作线程执行）。
    // M2 会话接线时移除该 allow。
    #[allow(dead_code)]
    pub fn request_connect(
        &self,
        target_id: &str,
        timeout: Duration,
    ) -> Result<ConnectGrant, String> {
        let inner = self.inner.lock().expect("信令运行时锁");
        match inner.signal.as_ref() {
            Some(signal) => signal.request_connect(target_id, timeout),
            None => Err("信令未运行，请先登录".into()),
        }
    }

    /// LAN 发现的对端快照（直连加速候选）。
    pub fn lan_peers(&self) -> Vec<rc_client_core::lan_discovery::LanPeer> {
        self.inner
            .lock()
            .expect("信令运行时锁")
            .lan
            .as_ref()
            .map(|lan| lan.peers())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_client_core::cert_manager::CertManager;
    use rc_client_core::server_config::ServerConfig;

    /// 在临时目录构造认证运行时（服务器未配置），保证单测不触碰真实用户配置。
    fn test_runtime(tag: &str) -> (SignalRuntime, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("rc-win-runtime-{tag}-{}", std::process::id()));
        let cert = CertManager::load_or_init(dir.clone()).expect("证书初始化失败");
        let cfg = ServerConfig {
            enabled: false,
            server_host: String::new(),
            signal_port: 21116,
            relay_port: 21117,
            api_port: 21118,
            use_https: false,
            ca_fingerprint_sha256: None,
            ca_cert_pem: None,
            server_auth_pubkey_hex: None,
            device_key: String::new(),
            cached_device_id: None,
        };
        let manager = Arc::new(AuthSessionManager::new(
            cfg,
            dir.join("auth.json"),
            Arc::new(cert),
        ));
        (SignalRuntime::new(manager), dir)
    }

    #[test]
    fn sync_and_stop_advance_generation() {
        let (runtime, dir) = test_runtime("gen");
        let gen0 = runtime.generation();
        let gen1 = runtime.sync().expect("sync 失败");
        assert_eq!(gen1, gen0 + 1);
        assert!(runtime.snapshot().is_some());
        runtime.stop();
        assert_eq!(runtime.generation(), gen1 + 1);
        assert!(runtime.snapshot().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn poll_returns_empty_before_signal_run() {
        let (runtime, dir) = test_runtime("poll");
        let events = runtime.poll();
        assert!(events.status.is_none());
        assert!(events.device_events.is_empty());
        assert!(events.kicked_reason.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn assist_gate_requires_running_signal() {
        let (runtime, dir) = test_runtime("assist");
        assert!(runtime.set_allow_remote_assist(false).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lan_peers_empty_when_stopped() {
        let (runtime, dir) = test_runtime("lan");
        assert!(runtime.lan_peers().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
