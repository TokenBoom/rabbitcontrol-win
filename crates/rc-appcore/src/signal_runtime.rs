//! 信令运行时：登录后拉起/重建信令长连接，代计数器防异步竞态。
//!
//! 主界面与后台被控进程共用同一份实现：同一设备同一时刻只能有一条信令
//! 连接（服务端以新连接顶掉旧连接），所以两个进程必须让位 / 接管，而不是各连一条。
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
    ConnectGrant, DeviceStatusEvent, RelayIncoming, SignalClientHandle, SignalClientOptions,
    TokenProvider, TokenRefresher,
};

use crate::auth_text;

/// 单次 `poll` 排空得到的事件集合。
#[derive(Debug, Default)]
pub struct RuntimeEvents {
    /// 最新信令状态快照（若有变化）。
    pub status: Option<ServerStatusSnapshot>,
    /// 账号设备在线状态变化。
    pub device_events: Vec<DeviceStatusEvent>,
    /// 被服务端踢下线的原因（改密/解绑/吊销等），触发回登录页。
    pub kicked_reason: Option<String>,
    /// M5：入站中继会话请求（本端作为被控端），App 决定 offer 给 host 或快速拒绝。
    pub host_incoming: Vec<RelayIncoming>,
}

struct Inner {
    signal: Option<Arc<SignalClientHandle>>,
    lan: Option<LanDiscoveryHandle>,
}

/// 信令运行时句柄。
pub struct SignalRuntime {
    manager: Arc<AuthSessionManager>,
    inner: Mutex<Inner>,
    generation: AtomicU64,
}

/// 停掉一个交出去过的信令句柄。
///
/// 句柄可能还被在途的会话 / 被控配置引用着：先请求后台线程立刻收尾（共享引用也管用，
/// 否则旧连接会多活一会儿，触发服务端的重复登录顶号），本运行时独占时再原地等它退出；
/// 仍有别人引用时，由最后一个引用释放时的 `Drop` 完成清理。
fn retire_signal(signal: Option<Arc<SignalClientHandle>>) {
    let Some(signal) = signal else {
        return;
    };
    signal.request_stop();
    if let Ok(mut sole) = Arc::try_unwrap(signal) {
        sole.stop();
    }
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
    /// 返回新代数；服务器未配置时同样成功（信令保持 Disabled 态、不启动
    /// 局域网发现，`snapshot` 可探测到）。
    pub fn sync(&self) -> Result<u64, String> {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let cfg = self.manager.server_config();
        // 与 core 信令客户端判定「启用」的条件一致：不满足时信令本就不建连。
        let server_configured =
            cfg.enabled && !cfg.server_host.trim().is_empty() && cfg.signal_port != 0;
        let manager = Arc::clone(&self.manager);

        let token_provider: TokenProvider = Arc::new(move || {
            manager
                .ensure_valid_access_token()
                .map_err(auth_text::format_auth_error)
        });
        let manager_refresh = Arc::clone(&self.manager);
        let token_refresher: TokenRefresher = Arc::new(move |stale| {
            manager_refresh
                .refresh_after_unauthorized(stale)
                .map_err(auth_text::format_auth_error)
        });
        let device_pubkey = Some(self.manager.cert_manager().device_pubkey_hex());

        let mut inner = self.inner.lock().expect("信令运行时锁");
        retire_signal(inner.signal.take());
        inner.lan = None;

        let signal = SignalClientHandle::spawn_with_options(
            cfg,
            auth_text::local_hostname(),
            Vec::new(),
            SignalClientOptions {
                token_provider: Some(token_provider),
                token_refresher: Some(token_refresher),
                device_pubkey,
                allow_remote_assist: true,
                // v0.1.23 双权限模型：同账号控制与远程协助分开开关。
                allow_remote_control: Some(true),
                dynamic_lan_discovery: true,
                upnp_enabled: true,
            },
            |_| {},
        );
        inner.signal = Some(Arc::new(signal));
        // 局域网发现只在服务器已配置时启动。发现线程一起来就绑定 UDP
        // `0.0.0.0:19999`——服务器未配置时它没有任何用处，却会让 Windows
        // 防火墙为每个新编译出的程序（包括 `cargo test` 的测试程序）弹一次
        // 「允许入站连接」确认。
        if server_configured {
            // 随机实例 ID 每进程唯一即可（对端按实例 ID 去重展示）。
            let instance_id: u64 = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            inner.lan = Some(LanDiscoveryHandle::spawn(
                instance_id,
                auth_text::local_hostname(),
            ));
        }
        Ok(generation)
    }

    /// 登出：递增代数并停止信令与 LAN 发现。
    pub fn stop(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        let mut inner = self.inner.lock().expect("信令运行时锁");
        retire_signal(inner.signal.take());
        inner.lan = None;
    }

    /// 当前信令客户端句柄（未启动时返回 None）。
    ///
    /// 交给会话 / 被控配置（core v0.2.6+ 的 `signal_client`）：会话运行期 P2P → 中继降级时用它
    /// 重新申请中继，被控端中继 → P2P 复探时用它刷新 STUN 映射并复用 UDP 端口。
    pub fn signal_client(&self) -> Option<Arc<SignalClientHandle>> {
        self.inner.lock().expect("信令运行时锁").signal.clone()
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
        // 阻塞型请求（连接 / 远程开机）在后台线程持有同一把锁。此时跳过本轮
        // 轮询，而不是让 UI 线程等锁——事件都在有界队列里，下一轮再取。
        let Ok(inner) = self.inner.try_lock() else {
            return RuntimeEvents::default();
        };
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
            // M5：入站中继连入排空（被控端桥接）
            while let Some(incoming) = signal.try_recv_incoming() {
                events.host_incoming.push(incoming);
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

    /// 向服务端发起远程开机请求（阻塞 ≤ `timeout`，调用方须在工作线程执行）。
    ///
    /// 本机没有目标设备的 MAC 地址：唤醒包由同账号在线设备或服务端转发，
    /// 这里只负责拿回结果。成功返回提示文案，失败返回原因。
    pub fn request_wol(&self, target_id: &str, timeout: Duration) -> Result<String, String> {
        let inner = self.inner.lock().expect("信令运行时锁");
        let Some(signal) = inner.signal.as_ref() else {
            return Err("信令未运行，请先登录".into());
        };
        let ack = signal
            .request_wol(target_id, timeout)
            .map_err(|e| auth_text::ui_error_text(&e))?;
        if !ack.ok {
            let from_catalog = ack.ui_error.as_ref().map(auth_text::ui_error_text);
            let from_server = (!ack.message.trim().is_empty()).then(|| ack.message.clone());
            return Err(from_catalog
                .or(from_server)
                .unwrap_or_else(|| "服务端拒绝了唤醒请求".to_string()));
        }
        Ok(wol_success_text(&ack.method))
    }

    /// 把出站代理应用到正在运行的信令客户端并立即重连以切换通道。
    ///
    /// 信令未运行时返回 `Err`（此时只需进程级配置，登录时自然生效）。
    pub fn set_proxy(&self, proxy: rc_client_core::proxy::ProxyConfig) -> Result<(), String> {
        let inner = self.inner.lock().expect("信令运行时锁");
        let Some(signal) = inner.signal.as_ref() else {
            return Err("信令未运行".into());
        };
        signal.set_proxy_config(Some(proxy));
        signal.trigger_reconnect("proxy config changed");
        Ok(())
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

    // 约定：这里的单测不得打开任何监听套接字。`cargo test` 的测试程序放在
    // 带哈希的路径下，依赖一变路径就变，而 Windows 防火墙对每个新路径的程序
    // 第一次监听端口都会弹一次「允许入站连接」确认——一个绑定 UDP 的单测
    // 等于让每次改动都弹一次窗。

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
        // 服务器未配置：信令已建但保持 Disabled，且不得启动局域网发现
        // （它会绑定 UDP 监听，见 `sync` 的说明）
        {
            let inner = runtime.inner.lock().expect("信令运行时锁");
            assert!(inner.signal.is_some());
            assert!(inner.lan.is_none(), "服务器未配置时不应启动局域网发现");
        }
        assert!(runtime.lan_peers().is_empty());
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

/// 远程开机请求等待服务端确认的上限。
pub const WOL_TIMEOUT: Duration = Duration::from_secs(15);

/// 远程开机成功文案（按服务端报告的唤醒方式）。
pub fn wol_success_text(method: &str) -> String {
    let how = match method {
        "already_online" => return "设备已在线，无需唤醒".to_string(),
        "lan_peer" => "已通过局域网内的同账号设备发送唤醒包",
        "server" => "服务端已转发唤醒请求",
        _ => "唤醒请求已发送",
    };
    format!("{how}，等待设备上线…")
}

#[cfg(test)]
mod wol_text_tests {
    use super::wol_success_text;

    #[test]
    fn success_text_follows_method() {
        assert_eq!(wol_success_text("already_online"), "设备已在线，无需唤醒");
        assert!(wol_success_text("lan_peer").starts_with("已通过局域网"));
        assert!(wol_success_text("server").contains("服务端"));
        assert_eq!(wol_success_text(""), "唤醒请求已发送，等待设备上线…");
    }
}
