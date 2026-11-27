//! 后台被控引擎：信令连接与被控服务的生命周期，以及「让位 / 接管」状态机。
//!
//! 不含任何界面与 Win32 代码，可单测。主界面与后台进程不能同时持有信令
//! （服务端以新连接顶掉旧连接），所以：
//!
//! - 主界面在运行 → 后台**让位**（不连信令、不提供被控、不刷新登录令牌）；
//! - 主界面退出 → 后台**接管**（恢复登录态、连信令、按开关提供被控）；
//! - 后台被信令以 `replaced_by_new_connection` 踢掉 → 视为主界面抢先一步
//!   连上了，同样让位。
//!
//! 让位前若正有远端会话，会以 `ByeReason::SessionSwitching` 告别，控制端
//! 有界退避自动重连，续到主界面刚接管的服务上。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use rc_appcore::agent_ipc::{AgentSnapshot, PendingApproval};
use rc_appcore::agent_state::AgentState;
use rc_appcore::auth_text::format_auth_error;
use rc_appcore::host::{bridge_relay_incoming, HostRuntime};
use rc_appcore::signal_runtime::SignalRuntime;
use rc_client_core::app_prefs::{self, AppPrefs};
use rc_client_core::auth_client::{self, AuthMaintainerHandle, AuthSessionManager};
use rc_client_core::cert_manager::CertManager;
use rc_client_core::host_bridge::{HostApprovalPrompt, HostUpdate};
use rc_client_core::server_config::{self, ServerConnectionState};
use rc_session::proto::ByeReason;

use crate::diag_log;

/// 后台维护登录令牌 / 设备证书的轮询间隔。
const MAINTAINER_INTERVAL: Duration = Duration::from_secs(30);
/// 未登录或登录态失效时，检查磁盘登录态是否变化的间隔。
const AUTH_RECHECK_INTERVAL: Duration = Duration::from_secs(5);
/// 被控服务意外停止后的重启延迟。
const HOST_RESTART_DELAY: Duration = Duration::from_secs(5);
/// 登录态被服务端终止后的最短重试间隔（用户可能很久才重新登录）。
const BLOCKED_RETRY: Duration = Duration::from_secs(5 * 60);

/// 「主界面此刻是否在运行」的探测函数。
pub type GuiProbe = Box<dyn FnMut() -> bool + Send>;

/// 引擎推给界面层的事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineEvent {
    /// 状态变化（托盘提示需要刷新）。
    StateChanged,
    /// 有连接请求等待审批；`deadline` 之后 core 自动拒绝。
    ApprovalRequested {
        /// 审批代次。
        generation: u64,
        /// 请求方地址。
        remote_addr: String,
        /// 请求方是否已通过密码验证。
        password_verified: bool,
        /// 自动拒绝时刻。
        deadline: Instant,
    },
    /// 审批请求已了结（放行、拒绝、取消或超时）。
    ApprovalResolved,
    /// 远端会话开始。
    SessionStarted {
        /// 对端描述。
        peer: String,
    },
    /// 远端会话结束。
    SessionEnded,
}

/// 托盘展示状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayView {
    /// 未登录 RabbitControl 账号。
    NotLoggedIn,
    /// 已登录，但「允许被控」关闭。
    Idle,
    /// 在线，可被控。
    Online,
    /// 有连接请求等待审批。
    PendingApproval,
    /// 被控会话进行中。
    InSession,
    /// 主界面正在托管，后台不提供服务。
    Suspended,
    /// 登录态已失效，等待用户重新登录。
    Blocked,
}

impl TrayView {
    /// 托盘提示文本。
    pub fn tooltip(self) -> &'static str {
        match self {
            TrayView::NotLoggedIn => "RabbitControl · 未登录",
            TrayView::Idle => "RabbitControl · 被控已关闭",
            TrayView::Online => "RabbitControl · 在线，可被控",
            TrayView::PendingApproval => "RabbitControl · 有连接待审批",
            TrayView::InSession => "RabbitControl · 被控中",
            TrayView::Suspended => "RabbitControl · 后台待命",
            TrayView::Blocked => "RabbitControl · 登录已失效",
        }
    }
}

/// 信令被服务端踢掉后的处置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KickAction {
    /// 同设备的新连接顶掉了本连接：让位。
    Yield,
    /// 登录态被终止：停手，等用户重新登录。
    Block,
    /// 服务端重启等瞬态原因：core 会自行重连，不必处理。
    Transient,
}

/// 判定踢线原因对应的处置（原因字符串见 core `classify_kicked_reason`）。
fn classify_kick(reason: &str) -> KickAction {
    match reason.trim().to_ascii_lowercase().as_str() {
        "replaced_by_new_connection" => KickAction::Yield,
        "user_logout" | "password_changed" | "account_updated" | "token_revoked"
        | "account_disabled" | "account_deleted" | "user_deleted" | "device_unbound"
        | "cert_revoked" | "disabled_by_admin" => KickAction::Block,
        _ => KickAction::Transient,
    }
}

/// 运行状态。
enum RunState {
    /// 主界面在运行：不持有信令与被控；探测函数返回 `false` 时接管。
    Suspended(GuiProbe),
    /// 不持有信令：未登录，或登录态已失效（等待磁盘登录态变化）。
    Idle {
        /// 登录态失效的原因（未登录时为 `None`）。
        reason: Option<String>,
        /// 进入该状态时 `auth.json` 的修改时间。
        auth_stamp: Option<SystemTime>,
        /// 进入时刻（失效后的重试计时）。
        since: Instant,
        /// 上次检查磁盘登录态的时刻。
        last_check: Instant,
    },
    /// 持有信令连接与（按开关的）被控服务。
    Active,
}

/// 本轮 `tick` 的决定。
enum Tick {
    Nothing,
    Activate,
    Poll,
}

/// 后台被控引擎。
pub struct Engine {
    state_path: PathBuf,
    prefs_path: PathBuf,
    server_path: PathBuf,
    manager: Arc<AuthSessionManager>,
    relogin: Arc<Mutex<Option<String>>>,
    maintainer: Option<AuthMaintainerHandle>,
    runtime: SignalRuntime,
    host: HostRuntime,
    prefs: AppPrefs,
    host_enabled: bool,
    state: RunState,
    /// 主界面是否在运行的通用探测（被信令「顶掉」时据此决定让位还是报失效）。
    gui_probe: GuiProbe,
    signal_online: bool,
    device_id: Option<String>,
    temp_code: Option<String>,
    connected_peer: Option<String>,
    pending_approval: Option<HostApprovalPrompt>,
    last_error: Option<String>,
    /// 被控服务意外停止后的重启时刻。
    host_restart_at: Option<Instant>,
}

impl Engine {
    /// 用默认路径（与主界面共用同一份配置）创建。
    pub fn new() -> Result<Self, String> {
        Self::with_paths(
            &server_config::default_config_path(),
            &auth_client::default_auth_path(),
            None,
            &app_prefs::default_prefs_path(),
            &AgentState::path(),
            Box::new(|| false),
        )
    }

    /// 用指定路径创建（单测用临时目录，保证不触碰真实用户配置）。
    ///
    /// `certs_dir` 为 `None` 时使用默认证书目录。创建过程不联网。
    pub fn with_paths(
        server_path: &Path,
        auth_path: &Path,
        certs_dir: Option<&Path>,
        prefs_path: &Path,
        state_path: &Path,
        gui_probe: GuiProbe,
    ) -> Result<Self, String> {
        let cert = match certs_dir {
            Some(dir) => CertManager::load_or_init(dir.to_path_buf())
                .map_err(|e| format!("证书初始化失败：{e}"))?,
            None => CertManager::default_dir().map_err(|e| format!("证书初始化失败：{e}"))?,
        };
        let cfg = server_config::load_from_path(server_path);
        let manager = Arc::new(AuthSessionManager::new(
            cfg,
            auth_path.to_path_buf(),
            Arc::new(cert),
        ));
        let prefs = app_prefs::load_from_path(prefs_path);
        // 已保存的出站代理对后台进程同样生效（必须早于任何网络连接）
        rc_client_core::proxy::set_global_proxy_config(prefs.proxy.clone());
        let now = Instant::now();
        Ok(Self {
            state_path: state_path.to_path_buf(),
            prefs_path: prefs_path.to_path_buf(),
            server_path: server_path.to_path_buf(),
            manager: Arc::clone(&manager),
            relogin: Arc::new(Mutex::new(None)),
            maintainer: None,
            // 信令运行时必须与后台维护线程共用同一个认证管理器
            runtime: SignalRuntime::new(manager),
            host: HostRuntime::new(),
            prefs,
            host_enabled: AgentState::load_from(state_path).host_enabled,
            state: RunState::Idle {
                reason: None,
                auth_stamp: None,
                since: now,
                last_check: now,
            },
            gui_probe,
            signal_online: false,
            device_id: None,
            temp_code: None,
            connected_peer: None,
            pending_approval: None,
            last_error: None,
            host_restart_at: None,
        })
    }

    /// 替换主界面探测函数。
    pub fn set_gui_probe(&mut self, probe: GuiProbe) {
        self.gui_probe = probe;
    }

    /// 「允许被控」开关当前值。
    pub fn host_enabled(&self) -> bool {
        self.host_enabled
    }

    /// 是否正持有信令与被控（即处于 `Active`）。
    pub fn is_active(&self) -> bool {
        matches!(self.state, RunState::Active)
    }

    /// 接管：恢复登录态、连信令、按开关启动被控服务。
    pub fn activate(&mut self) -> Vec<EngineEvent> {
        self.stop_services();
        self.last_error = None;
        self.host_restart_at = None;
        // 主界面运行期间用户可能改过服务器、偏好与开关
        let cfg = server_config::load_from_path(&self.server_path);
        self.manager.update_server_config(cfg);
        self.prefs = app_prefs::load_from_path(&self.prefs_path);
        self.host_enabled = AgentState::load_from(&self.state_path).host_enabled;

        let session = match self.manager.reload_from_disk() {
            Ok(session) => session,
            Err(err) => {
                return self.go_idle(Some(format!(
                    "登录状态无法恢复：{}",
                    format_auth_error(err)
                )))
            }
        };
        if session.is_none() || !self.manager.server_config().is_configured() {
            return self.go_idle(None);
        }
        if self.maintainer.is_none() {
            let manager = Arc::clone(&self.manager);
            let relogin = Arc::clone(&self.relogin);
            self.maintainer = Some(self.manager.spawn_background_maintainer_with_callbacks(
                MAINTAINER_INTERVAL,
                move || manager.server_config().cached_device_id,
                || {},
                move |reason| {
                    *relogin.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason);
                },
            ));
        }
        if let Err(err) = self.runtime.sync() {
            return self.go_idle(Some(format!("信令启动失败：{err}")));
        }
        let _ = self
            .runtime
            .set_allow_remote_assist(self.prefs.allow_remote_assist);
        self.state = RunState::Active;
        diag_log!(
            "已接管：信令已启动，被控服务{}",
            if self.host_enabled {
                "开启"
            } else {
                "关闭"
            }
        );
        if self.host_enabled {
            self.start_host();
        }
        vec![EngineEvent::StateChanged]
    }

    /// 让位：主界面要接管信令与被控。返回让位前「允许被控」是否开启。
    ///
    /// `gui_probe` 在主界面进程存活期间返回 `true`。
    pub fn suspend_for_gui(&mut self, gui_probe: GuiProbe) -> (bool, Vec<EngineEvent>) {
        let mut events = Vec::new();
        if self.is_active() && self.host.session_active {
            // 通知控制端稍后自动重连
            self.host.stop_with_reason(ByeReason::SessionSwitching);
            events.push(EngineEvent::SessionEnded);
        }
        self.stop_services();
        if self.pending_approval.take().is_some() {
            events.push(EngineEvent::ApprovalResolved);
        }
        self.state = RunState::Suspended(gui_probe);
        diag_log!("已让位：主界面接管信令与被控");
        events.push(EngineEvent::StateChanged);
        (self.host_enabled, events)
    }

    /// 周期驱动（引擎线程每 ~100ms 调一次）：轮询信令与被控事件、处理状态迁移。
    pub fn tick(&mut self) -> Vec<EngineEvent> {
        let mut events = Vec::new();
        // 先在不借用 `self.state` 的上下文里做出决定，再执行（避免跨调用的可变借用）
        let mut become_active = Tick::Nothing;
        match &mut self.state {
            RunState::Suspended(probe) => {
                if !probe() {
                    // 主界面已退出：把探测函数收回来备用，然后接管
                    self.gui_probe = std::mem::replace(probe, Box::new(|| false));
                    become_active = Tick::Activate;
                }
            }
            RunState::Idle {
                reason,
                auth_stamp,
                since,
                last_check,
            } => {
                let flagged = self
                    .relogin
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take();
                if let Some(flag) = flagged {
                    *reason = Some(flag);
                    *since = Instant::now();
                    return vec![EngineEvent::StateChanged];
                }
                let due = last_check.elapsed() >= AUTH_RECHECK_INTERVAL;
                if due {
                    *last_check = Instant::now();
                }
                let reason_is_some = reason.is_some();
                if due
                    && (auth_file_stamp(&self.manager.auth_path()) != *auth_stamp
                        || (reason_is_some && since.elapsed() >= BLOCKED_RETRY))
                {
                    become_active = Tick::Activate;
                }
            }
            RunState::Active => become_active = Tick::Poll,
        }
        match become_active {
            Tick::Nothing => events,
            Tick::Activate => self.activate(),
            Tick::Poll => {
                self.tick_active(&mut events);
                events
            }
        }
    }

    fn tick_active(&mut self, events: &mut Vec<EngineEvent>) {
        // 先把标志取出来再调用别的方法：`if let` 的临时锁守卫会活到整个分支结束
        let flagged = self
            .relogin
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(reason) = flagged {
            events.append(&mut self.go_idle(Some(reason)));
            return;
        }
        let rt = self.runtime.poll();
        for incoming in rt.host_incoming {
            bridge_relay_incoming(self.host.handle.as_ref(), incoming, &self.manager);
        }
        if let Some(reason) = rt.kicked_reason {
            self.handle_kick(reason, events);
            if !self.is_active() {
                return;
            }
        }
        if let Some(status) = rt.status {
            self.signal_online = status.state == ServerConnectionState::Connected;
            self.device_id = status.device_id;
            if status.state == ServerConnectionState::Error {
                self.last_error = status.error_message;
            }
        }
        let mut host_stopped = false;
        for update in self.host.poll() {
            match update {
                HostUpdate::Listening { .. } => {}
                HostUpdate::PasswordRotated { password } => self.temp_code = Some(password),
                HostUpdate::ApprovalRequested(prompt) => {
                    let deadline = Instant::now()
                        + Duration::from_micros(rc_session::auth::DEFAULT_AUTH_TIMEOUT_US);
                    self.pending_approval = Some(prompt);
                    // 只留最新一条：排队竞态时以最后一次请求为准
                    let prompt = self.pending_approval.as_ref().expect("刚放入待审批请求");
                    events.push(EngineEvent::ApprovalRequested {
                        generation: prompt.generation,
                        remote_addr: prompt.remote_addr.clone(),
                        password_verified: prompt.password_verified,
                        deadline,
                    });
                }
                HostUpdate::ApprovalCancelled { generation } => {
                    if self.clear_approval(generation) {
                        events.push(EngineEvent::ApprovalResolved);
                    }
                }
                HostUpdate::ClientConnected { remote_addr }
                | HostUpdate::OwnerConnected { remote_addr, .. } => {
                    self.pending_approval = None;
                    self.connected_peer = Some(remote_addr.clone());
                    events.push(EngineEvent::ApprovalResolved);
                    events.push(EngineEvent::SessionStarted { peer: remote_addr });
                }
                HostUpdate::ClientDisconnected { .. } => {
                    self.connected_peer = None;
                    events.push(EngineEvent::SessionEnded);
                }
                HostUpdate::Stopped => host_stopped = true,
                HostUpdate::Error { message, .. } => self.last_error = Some(message),
            }
        }
        if host_stopped {
            self.connected_peer = None;
            self.temp_code = None;
            if self.host_enabled {
                // 不是我们主动停的：稍后重启
                self.host_restart_at = Some(Instant::now() + HOST_RESTART_DELAY);
            }
        }
        if let Some(at) = self.host_restart_at {
            if Instant::now() >= at {
                self.host_restart_at = None;
                if self.host_enabled && self.host.handle.is_none() {
                    self.start_host();
                }
            }
        }
    }

    /// 处理信令踢线：让位 / 停手等待重新登录 / 仅记录。
    fn handle_kick(&mut self, reason: String, events: &mut Vec<EngineEvent>) {
        diag_log!("信令连接被服务端断开：{reason}");
        match classify_kick(&reason) {
            KickAction::Yield => {
                // 同设备的新连接顶掉了本连接：主界面在就让它接管；不在则说明
                // 同一身份在别处连了信令，按登录态失效处理，避免来回互踢。
                let mut probe = std::mem::replace(&mut self.gui_probe, Box::new(|| false));
                let gui_running = probe();
                self.gui_probe = probe;
                if gui_running {
                    let probe = std::mem::replace(&mut self.gui_probe, Box::new(|| false));
                    let (_, mut ev) = self.suspend_for_gui(probe);
                    events.append(&mut ev);
                } else {
                    self.last_error = Some(reason);
                    events.append(
                        &mut self
                            .go_idle(Some("同一设备在别处连上了信令，已暂停后台服务".to_string())),
                    );
                }
            }
            KickAction::Block => {
                self.last_error = Some(reason.clone());
                events.append(&mut self.go_idle(Some(reason)));
            }
            // 服务端重启等瞬态原因：core 会自行重连
            KickAction::Transient => self.last_error = Some(reason),
        }
    }

    /// 开关「允许被控」（持久化；运行中立即生效）。
    pub fn set_host_enabled(&mut self, on: bool) -> Result<Vec<EngineEvent>, String> {
        let state = AgentState { host_enabled: on };
        AgentState::save_to(&self.state_path, &state)?;
        self.host_enabled = on;
        let mut events = vec![EngineEvent::StateChanged];
        if self.is_active() {
            if on {
                self.start_host();
            } else {
                self.host.stop();
                self.connected_peer = None;
                self.pending_approval = None;
                self.temp_code = None;
                events.push(EngineEvent::SessionEnded);
                events.push(EngineEvent::ApprovalResolved);
            }
        }
        Ok(events)
    }

    /// 应答待审批的连接请求（代次不符则忽略）。
    pub fn respond_approval(&mut self, generation: u64, accept: bool) -> Vec<EngineEvent> {
        let mut events = Vec::new();
        if let Some(handle) = self.host.handle.as_ref() {
            let _ = handle.respond_approval(generation, accept);
        }
        if self.clear_approval(generation) {
            events.push(EngineEvent::ApprovalResolved);
        }
        events
    }

    /// 断开当前正在控制本机的远端。
    pub fn disconnect_client(&mut self) {
        if let Some(handle) = self.host.handle.as_ref() {
            handle.disconnect_client();
        }
    }

    /// 停止全部后台服务（退出前调用）。
    pub fn shutdown(&mut self) {
        self.stop_services();
        let now = Instant::now();
        self.state = RunState::Idle {
            reason: None,
            auth_stamp: None,
            since: now,
            last_check: now,
        };
    }

    /// 托盘展示状态。
    pub fn tray_view(&self) -> TrayView {
        match &self.state {
            RunState::Suspended(_) => TrayView::Suspended,
            RunState::Idle { reason, .. } => match reason {
                Some(_) => TrayView::Blocked,
                None => TrayView::NotLoggedIn,
            },
            RunState::Active => {
                if self.host.session_active {
                    TrayView::InSession
                } else if self.pending_approval.is_some() {
                    TrayView::PendingApproval
                } else if self.host_enabled {
                    TrayView::Online
                } else {
                    TrayView::Idle
                }
            }
        }
    }

    /// 当前状态快照（供主界面 / 诊断读取）。
    pub fn snapshot(&self) -> AgentSnapshot {
        AgentSnapshot {
            logged_in: self.manager.current_session().is_some(),
            signal_online: self.signal_online,
            device_id: self.device_id.clone(),
            host_enabled: self.host_enabled,
            temp_code: self.temp_code.clone(),
            session_active: self.host.session_active,
            connected_peer: self.connected_peer.clone(),
            pending_approval: self.pending_approval.as_ref().map(|p| PendingApproval {
                generation: p.generation,
                remote_addr: p.remote_addr.clone(),
                password_verified: p.password_verified,
            }),
            suspended: matches!(self.state, RunState::Suspended(_)),
            last_error: self.last_error.clone(),
        }
    }

    fn start_host(&mut self) {
        let password = self
            .prefs
            .custom_host_password
            .clone()
            .filter(|p| !p.is_empty());
        match HostRuntime::start(
            &self.manager,
            password,
            self.prefs.allow_assist_file_transfer,
        ) {
            Ok(started) => {
                // 首个访问密码（自定义密码或刚生成的临时码）
                self.temp_code = Some(started.password.clone());
                self.host.install(started);
                self.host_restart_at = None;
            }
            Err(err) => {
                diag_log!("被控服务启动失败：{err}");
                self.last_error = Some(format!("被控服务启动失败：{err}"));
            }
        }
    }

    fn clear_approval(&mut self, generation: u64) -> bool {
        match self.pending_approval.as_ref() {
            Some(p) if p.generation == generation => {
                self.pending_approval = None;
                true
            }
            _ => false,
        }
    }

    /// 停掉被控、信令与后台维护线程，清空会话态。
    fn stop_services(&mut self) {
        self.host.stop();
        self.runtime.stop();
        if let Some(mut handle) = self.maintainer.take() {
            handle.stop();
        }
        self.signal_online = false;
        self.device_id = None;
        self.temp_code = None;
        self.connected_peer = None;
        self.pending_approval = None;
    }

    /// 进入等待态：`reason` 为 `None` 表示未登录，否则是登录态失效的原因。
    fn go_idle(&mut self, reason: Option<String>) -> Vec<EngineEvent> {
        match &reason {
            Some(reason) => diag_log!("进入等待：{reason}"),
            None => diag_log!("进入等待：未登录或未配置服务器"),
        }
        self.stop_services();
        let now = Instant::now();
        self.state = RunState::Idle {
            reason,
            auth_stamp: auth_file_stamp(&self.manager.auth_path()),
            since: now,
            last_check: now,
        };
        vec![EngineEvent::StateChanged]
    }
}

/// `auth.json` 的修改时间（取不到时为 `None`）。
fn auth_file_stamp(auth_path: &Path) -> Option<SystemTime> {
    std::fs::metadata(auth_path).and_then(|m| m.modified()).ok()
}

#[cfg(test)]
impl Engine {
    /// 把等待态里的「上次检查时刻」拨到过去，让下一次 `tick` 立即复查磁盘登录态。
    fn last_auth_check_was(&mut self, at: Instant) {
        if let RunState::Idle { last_check, .. } = &mut self.state {
            *last_check = at;
        } else {
            panic!("引擎不在等待态");
        }
    }

    /// 注入一条信令踢线原因（正常由 `tick` 从信令运行时取出）。
    fn inject_kick(&mut self, reason: &str) -> Vec<EngineEvent> {
        let mut events = Vec::new();
        self.handle_kick(reason.to_string(), &mut events);
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_engine(tag: &str) -> (Engine, PathBuf) {
        let dir = std::env::temp_dir().join(format!("rc-host-agent-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 服务器未配置：保证引擎绝不会去连信令
        std::fs::write(dir.join("server.json"), b"{}").unwrap();
        let engine = engine_at(&dir, Box::new(|| false)).expect("引擎创建（不联网）");
        (engine, dir)
    }

    fn engine_at(dir: &Path, probe: GuiProbe) -> Result<Engine, String> {
        Engine::with_paths(
            &dir.join("server.json"),
            &dir.join("auth.json"),
            Some(&dir.join("certs")),
            &dir.join("prefs.json"),
            &dir.join("agent.json"),
            probe,
        )
    }

    fn cleanup(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn not_logged_in_means_idle_and_nothing_runs() {
        let (mut engine, dir) = temp_engine("idle");
        assert_eq!(engine.activate(), vec![EngineEvent::StateChanged]);
        assert!(!engine.is_active());
        assert_eq!(engine.tray_view(), TrayView::NotLoggedIn);
        assert!(engine.host.handle.is_none());
        assert!(engine.tick().is_empty());
        cleanup(&dir);
    }

    #[test]
    fn suspend_then_takeover_when_gui_exits() {
        let (mut engine, dir) = temp_engine("suspend");
        engine.activate();
        let gui_running = Arc::new(Mutex::new(true));
        let flag = Arc::clone(&gui_running);
        let (_, events) = engine.suspend_for_gui(Box::new(move || *flag.lock().unwrap()));
        assert!(events.contains(&EngineEvent::StateChanged));
        assert_eq!(engine.tray_view(), TrayView::Suspended);
        assert!(engine.snapshot().suspended);
        assert!(engine.tick().is_empty(), "主界面还在：保持让位");
        *gui_running.lock().unwrap() = false;
        assert_eq!(engine.tick(), vec![EngineEvent::StateChanged]);
        assert!(!engine.snapshot().suspended);
        cleanup(&dir);
    }

    #[test]
    fn host_toggle_persists_and_survives_restart() {
        let (mut engine, dir) = temp_engine("toggle");
        assert!(!engine.host_enabled(), "默认关闭：被控需显式开启");
        engine.set_host_enabled(true).expect("保存开关");
        assert!(engine.host_enabled());
        // 新进程读到同样的开关
        assert!(engine_at(&dir, Box::new(|| false)).unwrap().host_enabled());
        engine.set_host_enabled(false).expect("保存开关");
        assert!(!engine_at(&dir, Box::new(|| false)).unwrap().host_enabled());
        cleanup(&dir);
    }

    #[test]
    fn kick_reasons_map_to_the_right_action() {
        assert_eq!(
            classify_kick("replaced_by_new_connection"),
            KickAction::Yield
        );
        assert_eq!(
            classify_kick(" Replaced_By_New_Connection "),
            KickAction::Yield
        );
        for reason in [
            "user_logout",
            "password_changed",
            "token_revoked",
            "device_unbound",
            "cert_revoked",
            "account_disabled",
            "disabled_by_admin",
        ] {
            assert_eq!(classify_kick(reason), KickAction::Block, "{reason}");
        }
        assert_eq!(classify_kick("server_shutdown"), KickAction::Transient);
        assert_eq!(classify_kick(""), KickAction::Transient);
    }

    #[test]
    fn stale_approval_generation_is_ignored() {
        let (mut engine, dir) = temp_engine("approval");
        engine.pending_approval = Some(HostApprovalPrompt {
            generation: 7,
            session_id: 1,
            remote_addr: "1.2.3.4:5".into(),
            requested_res: "1920x1080".into(),
            requested_fps: 60,
            password_verified: false,
        });
        assert!(engine.respond_approval(6, true).is_empty());
        assert!(engine.pending_approval.is_some());
        assert_eq!(
            engine.respond_approval(7, false),
            vec![EngineEvent::ApprovalResolved]
        );
        assert!(engine.pending_approval.is_none());
        cleanup(&dir);
    }

    #[test]
    fn auth_file_change_triggers_a_new_attempt() {
        let (mut engine, dir) = temp_engine("wakeup");
        engine.activate();
        assert_eq!(engine.tray_view(), TrayView::NotLoggedIn);
        engine.last_auth_check_was(Instant::now());
        // 用户在主界面登录后 auth.json 出现：磁盘登录态变化要触发重新尝试
        // （内容是否有效由 activate 判定；这里只验证「会去试」）
        std::fs::write(dir.join("auth.json"), b"{}").unwrap();
        assert!(engine.tick().is_empty(), "检查间隔内不重复尝试");
        engine.last_auth_check_was(Instant::now() - AUTH_RECHECK_INTERVAL - Duration::from_secs(1));
        assert_eq!(engine.tick(), vec![EngineEvent::StateChanged]);
        // 文件内容不是有效登录态：仍是未登录
        assert_eq!(engine.tray_view(), TrayView::NotLoggedIn);
        // 磁盘没再变：不重复尝试
        engine.last_auth_check_was(Instant::now() - AUTH_RECHECK_INTERVAL - Duration::from_secs(1));
        assert!(engine.tick().is_empty());
        cleanup(&dir);
    }

    #[test]
    fn replaced_kick_yields_only_while_the_gui_is_running() {
        let gui_running = Arc::new(Mutex::new(false));
        let (mut engine, dir) = {
            let dir =
                std::env::temp_dir().join(format!("rc-host-agent-yield-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("server.json"), b"{}").unwrap();
            let flag = Arc::clone(&gui_running);
            let engine = engine_at(&dir, Box::new(move || *flag.lock().unwrap())).unwrap();
            (engine, dir)
        };
        // 主界面不在：不能让位成「永远等一个不存在的进程」，也不能来回互踢
        engine.inject_kick("replaced_by_new_connection");
        assert_eq!(engine.tray_view(), TrayView::Blocked);
        assert_eq!(
            engine.last_error.as_deref(),
            Some("replaced_by_new_connection")
        );

        // 主界面在运行：让位，等它退出后接管
        *gui_running.lock().unwrap() = true;
        let mut idle = {
            let dir =
                std::env::temp_dir().join(format!("rc-host-agent-yield2-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("server.json"), b"{}").unwrap();
            let flag = Arc::clone(&gui_running);
            let mut engine = engine_at(&dir, Box::new(move || *flag.lock().unwrap())).unwrap();
            let events = engine.inject_kick("replaced_by_new_connection");
            assert!(events.contains(&EngineEvent::StateChanged));
            assert_eq!(engine.tray_view(), TrayView::Suspended);
            let _ = std::fs::remove_dir_all(&dir);
            engine
        };
        *gui_running.lock().unwrap() = false;
        assert_eq!(idle.tick(), vec![EngineEvent::StateChanged]);
        assert!(!idle.snapshot().suspended);
        cleanup(&dir);
    }

    #[test]
    fn tray_tooltips_are_distinct() {
        let views = [
            TrayView::NotLoggedIn,
            TrayView::Idle,
            TrayView::Online,
            TrayView::PendingApproval,
            TrayView::InSession,
            TrayView::Suspended,
            TrayView::Blocked,
        ];
        for (i, a) in views.iter().enumerate() {
            for b in &views[i + 1..] {
                assert_ne!(a.tooltip(), b.tooltip());
            }
        }
    }
}
