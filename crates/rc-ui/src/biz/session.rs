//! 会话编排（GUI-free）：信令连接请求 → [`UiSessionConfig`] 组装 →
//! [`SessionHandle`] 启动，以及给 UI 消费的输入归一化辅助。
//!
//! 阻塞式 `request_connect` 必须由调用方放在后台线程执行（见
//! `app.rs` 的线程模型）；本模块只提供纯组装与轮询语义，不引用
//! windows 类型，可单测。

use std::time::Duration;

use rc_client_core::auth_client::AuthSessionManager;
use rc_client_core::input_bridge;
use rc_client_core::relay_transport::SessionMtlsBundle;
use rc_client_core::session_bridge::{SessionHandle, SessionUpdate, UiSessionConfig};
use rc_client_core::signal_client::ConnectGrant;
use rc_session::auth::OwnerTicketSigner;

/// 连接参数（UI → 后台线程的纯值包）。
#[derive(Debug, Clone)]
pub struct ConnectParams {
    /// 可选会话密码。
    pub password: Option<String>,
    /// 仅观看。
    pub watch_only: bool,
    /// 请求码率（bps）。
    pub bitrate_bps: u32,
    /// 请求帧率。
    pub fps: u32,
    /// LAN 直连候选地址（可选加速）。
    pub lan_direct: Option<String>,
}

/// `request_connect` 阻塞上限。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// 组装 `UiSessionConfig`：凭据 + mTLS 束 + Owner 免密签名器。
///
/// 语义对齐 iOS `SessionService.open`：
/// - `encrypt_connection` 恒开（服务端中继强制客户端证书）；
/// - `mtls_config` 由设备证书 + 锚定 CA + 本账号 user_id 组装，
///   `expected_peer_device_id` 留空由会话桥按 `grant.target_id` 填充；
/// - `grant.owner_grant` 存在时构造 OwnerTicket 签名器（同账号免密）。
pub fn build_session_config(
    manager: &AuthSessionManager,
    grant: ConnectGrant,
    params: &ConnectParams,
) -> Result<UiSessionConfig, String> {
    let cfg = manager.server_config();
    let session = manager.current_session();
    let ca_cert_pem = cfg
        .ca_cert_pem
        .clone()
        .ok_or_else(|| "服务器 CA 尚未锚定，请先在设置里重新探测保存".to_string())?;
    let cert_manager = manager.cert_manager();
    let (cert_pem, _) = cert_manager
        .load_device_cert()
        .ok_or_else(|| "本机设备证书缺失，请重新登录以重新签发".to_string())?;
    let key_pem = cert_manager.tls_key_pem()?;
    let local_user_id = session
        .as_ref()
        .map(|s| s.user_id.clone())
        .ok_or_else(|| "登录会话缺失，请重新登录".to_string())?;

    let mtls_config = SessionMtlsBundle {
        ca_cert_pem,
        cert_pem,
        key_pem,
        local_user_id,
        expected_peer_device_id: None,
        expected_peer_user_id: None,
    };

    let owner_signer = grant
        .owner_grant
        .clone()
        .map(|grant| cert_manager.create_owner_ticket_signer(grant))
        .transpose()?
        .map(|signer| std::sync::Arc::new(signer) as std::sync::Arc<dyn OwnerTicketSigner>);

    Ok(UiSessionConfig {
        connect_addr: grant
            .peer_public_addr
            .clone()
            .and_then(|addr| addr.parse().ok())
            .unwrap_or_else(|| std::net::SocketAddr::from(([127, 0, 0, 1], 9901))),
        relay_grant: Some(grant),
        password: params.password.clone(),
        watch_only: params.watch_only,
        width: 0,
        height: 0,
        fps: params.fps,
        bitrate_bps: params.bitrate_bps,
        prefer_hardware: true,
        clipboard_enabled: false, // M3.4 剪贴板后端落地后开启
        audio_enabled: false,     // M3.5 WASAPI 播放器落地后开启
        encrypt_connection: true,
        mtls_config: Some(mtls_config),
        owner_grant: None,
        owner_signer,
        lan_direct_addr: params
            .lan_direct
            .as_deref()
            .and_then(|addr| addr.parse().ok()),
        force_relay: false,
    })
}

/// 比特率档位字符串 → bps（与设置页 QualityBox 一致）。
pub fn bitrate_preset_to_bps(preset: &str) -> u32 {
    match preset {
        "8m" => 8_000_000,
        "14m" => 14_000_000,
        "30m" => 30_000_000,
        // auto：以均衡档起步，会话内码控自适应。
        _ => 14_000_000,
    }
}

/// UI 侧会话状态快照（由 [`SessionUpdate`] 归纳，纯值便于跨线程）。
#[derive(Debug, Clone, PartialEq)]
pub enum SessionView {
    /// 未在会话中。
    Idle,
    /// 连接中（阶段文案）。
    Connecting(String),
    /// 已协商（后端标签 / 传输方式 / 分辨率）。
    Negotiated {
        width: u32,
        height: u32,
        fps: u32,
        backend: String,
        transport: String,
        mtls_active: bool,
        watch_only: bool,
    },
    /// 已结束（原因）。
    Ended(String),
}

/// 由 [`SessionUpdate`] 归纳 UI 状态（纯函数，便于单测）。
pub fn view_state_of(update: &SessionUpdate, current: &SessionView) -> SessionView {
    match update {
        SessionUpdate::Connecting { phase, watch_only } => {
            let _ = watch_only;
            SessionView::Connecting(connect_phase_text(phase))
        }
        SessionUpdate::Negotiated {
            width,
            height,
            fps,
            backend,
            transport,
            mtls_active,
            watch_only,
            ..
        } => SessionView::Negotiated {
            width: *width,
            height: *height,
            fps: *fps,
            backend: backend.clone(),
            transport: transport.clone(),
            mtls_active: *mtls_active,
            watch_only: *watch_only,
        },
        SessionUpdate::Ended { reason } => SessionView::Ended(reason.clone()),
        other => {
            let _ = other;
            current.clone()
        }
    }
}

/// 连接阶段中文文案。
pub fn connect_phase_text(phase: &rc_client_core::session_bridge::ConnectPhase) -> String {
    use rc_client_core::session_bridge::ConnectPhase as P;
    match phase {
        P::EndpointCreating => "正在创建连接端点…".to_string(),
        P::Connecting => "正在握手…".to_string(),
        P::AwaitingWelcome => "等待对端裁定参数…".to_string(),
        P::FirstFramePending => "等待首帧…".to_string(),
    }
}

/// 输入几何：窗口（stage）坐标 → 归一化 0–65535（黑边补偿）。
pub fn normalize_in_video(
    x: f64,
    y: f64,
    stage_w: f64,
    stage_h: f64,
    video: Option<(u32, u32)>,
) -> (u16, u16) {
    input_bridge::normalize_pointer_in_video(x, y, stage_w, stage_h, video)
}

/// 滚轮：Windows `WHEEL_DELTA`（120）→ 行数（I4 约定）。
pub fn wheel_to_lines(delta: i16) -> i16 {
    (delta as f64 / 120.0).round() as i16
}

/// 复用检查：会话桥 spawn 必须拿到 `ConnectGrant`，此处确保字段语义
/// 未漂移（编译期断言级别轻量检查）。
#[allow(dead_code)]
fn grant_is_relay_capable(grant: &ConnectGrant) -> bool {
    !grant.relay_addr.is_empty()
}

/// 会话句柄装饰：统一断开 / 重连语义（App 直接持有 [`SessionHandle`]）。
pub fn disconnect(handle: &SessionHandle) {
    handle.send(rc_client_core::session_bridge::SessionCommand::Disconnect);
}

/// 等待会话线程退出（阻塞；调用方在 UI 线程应使用短超时轮询替代）。
#[allow(dead_code)]
pub fn join_session(handle: SessionHandle) {
    handle.join();
}

/// 会话桥连通性冒烟：构造 → 默认配置检查（纯逻辑，不触网）。
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitrate_presets_map() {
        assert_eq!(bitrate_preset_to_bps("8m"), 8_000_000);
        assert_eq!(bitrate_preset_to_bps("14m"), 14_000_000);
        assert_eq!(bitrate_preset_to_bps("30m"), 30_000_000);
        assert_eq!(bitrate_preset_to_bps("auto"), 14_000_000);
        assert_eq!(bitrate_preset_to_bps("unknown"), 14_000_000);
    }

    #[test]
    fn view_state_transitions() {
        let idle = SessionView::Idle;
        let connecting = SessionUpdate::Connecting {
            phase: rc_client_core::session_bridge::ConnectPhase::Connecting,
            watch_only: false,
        };
        assert_eq!(
            view_state_of(&connecting, &idle),
            SessionView::Connecting("正在握手…".to_string())
        );
        let ended = SessionUpdate::Ended {
            reason: "对端断开".to_string(),
        };
        assert_eq!(
            view_state_of(&ended, &SessionView::Connecting("x".into())),
            SessionView::Ended("对端断开".to_string())
        );
        // HUD 等未纳入视图的事件不改变状态。
        let current = SessionView::Negotiated {
            width: 1920,
            height: 1080,
            fps: 60,
            backend: "fake".into(),
            transport: "TCP Relay".into(),
            mtls_active: true,
            watch_only: false,
        };
        assert_eq!(
            view_state_of(&ended, &current),
            SessionView::Ended("对端断开".to_string())
        );
    }

    #[test]
    fn wheel_delta_maps_to_lines() {
        assert_eq!(wheel_to_lines(120), 1);
        assert_eq!(wheel_to_lines(-120), -1);
        assert_eq!(wheel_to_lines(240), 2);
    }

    #[test]
    fn normalize_maps_center_to_mid() {
        // 视口满铺 stage：中心 → 32767（65535/2 截断）。
        let (x, y) = normalize_in_video(500.0, 250.0, 1000.0, 500.0, None);
        assert!((32766..=32769).contains(&x), "x={x}");
        assert!((32766..=32769).contains(&y), "y={y}");
    }
}
