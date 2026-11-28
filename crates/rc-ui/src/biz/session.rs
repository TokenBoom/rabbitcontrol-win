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
use rc_client_core::signal_client::{ConnectGrant, SignalClientHandle};
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
    /// 信令客户端句柄（core v0.2.6+：P2P → 中继降级时用它重新申请中继）。
    pub signal_client: Option<std::sync::Arc<SignalClientHandle>>,
    /// 是否开启端到端 mTLS / E2EE 加密（设置页「默认开启加密连接」）。
    pub encrypt_connection: bool,
}

/// `request_connect` 阻塞上限。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// 组装 `UiSessionConfig`：凭据 + mTLS 束 + Owner 免密签名器。
///
/// 语义对齐 iOS `SessionService.open`：
/// - `encrypt_connection` 跟随设置页「默认开启加密连接」（服务端中继本身强制客户端证书，
///   所以无论开关如何都要有设备证书）；
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

    let target_device_id = grant.target_id.clone();
    Ok(UiSessionConfig {
        signal_client: params.signal_client.clone(),
        target_device_id: Some(target_device_id),
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
        // M3.4：rc-clipboard-win 已实装，默认开启（运行时经 SetClipboardEnabled 开关）
        clipboard_enabled: true,
        connection_mode: Default::default(),
        tools_only: false,
        audio_enabled: false, // M3.5 WASAPI 播放器落地后开启
        encrypt_connection: params.encrypt_connection,
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
        SessionUpdate::Reconnecting {
            attempt,
            max_attempts,
            reason,
        } => SessionView::Connecting(format!("重连中 {attempt}/{max_attempts}：{reason}")),
        SessionUpdate::Ended {
            reason, ui_error, ..
        } => {
            // v0.2.2 UiError 集成：优先用结构化错误标题+建议
            match ui_error {
                Some(err) => {
                    let mut text = err.title.clone();
                    if !err.suggestion.is_empty() {
                        text.push_str(&format!("（{}）", err.suggestion));
                    }
                    SessionView::Ended(text)
                }
                None => SessionView::Ended(reason.clone()),
            }
        }
        SessionUpdate::MediaState { .. } | SessionUpdate::MediaResumed { .. } => {
            // 音频/视频暂停恢复状态：HUD/状态条可选展示，v1 不改变视图状态
            current.clone()
        }
        SessionUpdate::ToolPermissionDenied { .. } => {
            // 工具权限被拒：不改变连接状态（工具面板自行提示）
            current.clone()
        }
        SessionUpdate::ToolPermissionsUpdated { .. } => current.clone(),
        other => {
            let _ = other;
            current.clone()
        }
    }
}

/// 会话画质状态（由 `SessionUpdate::DisplayConfig` 归纳；被控端权威数据）。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct QualityState {
    /// 被控端预设列表 `(宽, 高, 标签)`，严格原生宽高比、由高到低。
    pub presets: Vec<(u32, u32, String)>,
    /// 当前生效流宽（像素）。
    pub width: u32,
    /// 当前生效流高（像素）。
    pub height: u32,
    /// 当前生效帧率。
    pub fps: u32,
    /// 被控端允许的最高帧率。
    pub max_fps: u32,
    /// 当前编码器（`"h264"` / `"hevc"`；旧版被控端未上报时 `None`）。
    pub codec: Option<String>,
    /// 被控端支持编码器列表（如 `["hevc", "h264"]`）。
    pub supported_codecs: Vec<String>,
    /// 控制端硬件解码路径是否支持 HEVC。
    pub client_supports_hevc: bool,
    /// 当前生效鼠标模式（`"smart"` / `"remote"` / `"local"`；未上报时 `None`）。
    pub mouse_mode: Option<String>,
    /// 可用显示器列表 `(display_id, 展示名)`（M3.8）。
    pub displays: Vec<(u32, String)>,
    /// 当前采集的显示器 ID。
    pub active_display_id: u32,
}

/// 鼠标模式候选（固定顺序，与下拉框索引一一对应）。
pub const MOUSE_MODES: [(&str, &str); 3] = [
    ("smart", "智能跟踪"),
    ("remote", "远程光标"),
    ("local", "本地光标"),
];

/// 预钳制流参数请求（ios 踩坑：越界请求被控端静默无流）。
///
/// 规则：`0` = 保持当前；尺寸必须等于当前值或命中 presets；fps ≤ max_fps。
/// 返回 `None` 表示请求非法，调用方不得下发。
pub fn clamp_stream_request(
    width: u32,
    height: u32,
    fps: u32,
    state: &QualityState,
) -> Option<(u32, u32, u32)> {
    let size_ok = (width == 0 && height == 0)
        || (width == state.width && height == state.height)
        || state
            .presets
            .iter()
            .any(|(w, h, _)| *w == width && *h == height);
    if !size_ok {
        return None;
    }
    if fps != 0 && (state.max_fps != 0 && fps > state.max_fps) {
        return None;
    }
    Some((width, height, fps))
}

/// 按被控端 max_fps 生成帧率候选（30/60/120/144/240 过滤）。
pub fn fps_candidates(max_fps: u32) -> Vec<u32> {
    [30u32, 60, 120, 144, 240]
        .into_iter()
        .filter(|v| max_fps == 0 || *v <= max_fps)
        .collect()
}

/// 码率档位（与设置页的「画质偏好」共用）：自适应 / 8M / 14M / 30M。
pub const BITRATE_PRESETS: [&str; 4] = ["auto", "8m", "14m", "30m"];
/// 各码率档位下发的比特率；`0` = 自适应，不下发。
pub const BITRATE_STEPS_BPS: [u32; 4] = [0, 8_000_000, 14_000_000, 30_000_000];

/// 会话窗口「显示与画质」菜单里的哪一组。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamSetting {
    /// 分辨率。
    Resolution,
    /// 帧率。
    Fps,
    /// 码率档位。
    Bitrate,
    /// 编解码器。
    Codec,
    /// 鼠标模式。
    Mouse,
    /// 显示器。
    Display,
}

impl StreamSetting {
    /// 菜单里的标题。
    pub fn title(self) -> &'static str {
        match self {
            Self::Resolution => "分辨率",
            Self::Fps => "帧率",
            Self::Bitrate => "码率",
            Self::Codec => "编解码",
            Self::Mouse => "鼠标",
            Self::Display => "显示器",
        }
    }

    /// 菜单里的图标（Segoe MDL2 Assets 码点）。
    pub fn icon(self) -> &'static str {
        match self {
            Self::Resolution => "\u{E772}",
            Self::Fps => "\u{E916}",
            Self::Bitrate => "\u{E945}",
            Self::Codec => "\u{E714}",
            Self::Mouse => "\u{E962}",
            Self::Display => "\u{E7F4}",
        }
    }
}

/// 「显示与画质」菜单里的一组选项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingGroup {
    /// 哪一组。
    pub setting: StreamSetting,
    /// 选项文案；可调的组第 0 项一律是「跟随当前」（保持被控端现在的值）。
    pub options: Vec<String>,
    /// 当前生效项的下标。
    pub selected: usize,
    /// 为假时整组不可选（对端没上报这项能力）。
    pub enabled: bool,
}

/// 由被控端能力与当前生效值，算出菜单的各组选项（纯函数，菜单与勾都从它来）。
pub fn setting_groups(
    quality: &QualityState,
    fps_options: &[u32],
    bitrate_preset: &str,
) -> Vec<SettingGroup> {
    let mut resolutions = vec!["跟随当前".to_string()];
    resolutions.extend(quality.presets.iter().map(|(_, _, label)| label.clone()));
    let current_resolution = quality
        .presets
        .iter()
        .position(|(w, h, _)| *w == quality.width && *h == quality.height)
        .map_or(0, |index| index + 1);

    let mut frame_rates = vec!["跟随当前".to_string()];
    frame_rates.extend(fps_options.iter().map(|fps| format!("{fps} fps")));
    let current_fps = fps_options
        .iter()
        .position(|fps| *fps == quality.fps)
        .map_or(0, |index| index + 1);

    let codec = if quality.supported_codecs.is_empty() {
        SettingGroup {
            setting: StreamSetting::Codec,
            options: vec!["自动".to_string()],
            selected: 0,
            enabled: false,
        }
    } else {
        let mut options = vec!["跟随当前".to_string()];
        options.extend(
            quality
                .supported_codecs
                .iter()
                .map(|codec| codec_label(codec)),
        );
        SettingGroup {
            setting: StreamSetting::Codec,
            selected: quality
                .codec
                .as_deref()
                .and_then(|codec| quality.supported_codecs.iter().position(|s| s == codec))
                .map_or(0, |index| index + 1),
            options,
            enabled: true,
        }
    };

    vec![
        SettingGroup {
            setting: StreamSetting::Resolution,
            selected: current_resolution,
            options: resolutions,
            enabled: true,
        },
        SettingGroup {
            setting: StreamSetting::Fps,
            selected: current_fps,
            options: frame_rates,
            enabled: true,
        },
        SettingGroup {
            setting: StreamSetting::Bitrate,
            selected: BITRATE_PRESETS
                .iter()
                .position(|preset| *preset == bitrate_preset)
                .unwrap_or(0),
            options: ["自适应", "8 Mbps", "14 Mbps", "30 Mbps"]
                .iter()
                .map(|label| label.to_string())
                .collect(),
            enabled: true,
        },
        codec,
        SettingGroup {
            setting: StreamSetting::Mouse,
            selected: MOUSE_MODES
                .iter()
                .position(|(mode, _)| Some(*mode) == quality.mouse_mode.as_deref())
                .unwrap_or(0),
            options: MOUSE_MODES
                .iter()
                .map(|(_, label)| label.to_string())
                .collect(),
            enabled: true,
        },
        SettingGroup {
            setting: StreamSetting::Display,
            selected: quality
                .displays
                .iter()
                .position(|(id, _)| *id == quality.active_display_id)
                .unwrap_or(0),
            options: quality
                .displays
                .iter()
                .map(|(_, name)| name.clone())
                .collect(),
            enabled: !quality.displays.is_empty(),
        },
    ]
}

fn codec_label(codec: &str) -> String {
    match codec {
        "hevc" => "HEVC".to_string(),
        "h264" => "H.264".to_string(),
        other => other.to_uppercase(),
    }
}

/// 分辨率、帧率两组的选中下标 → 流参数请求（下标 0 = 保持当前）。
pub fn stream_request(
    resolution: usize,
    fps: usize,
    quality: &QualityState,
    fps_options: &[u32],
) -> (u32, u32, u32) {
    let (width, height) = resolution
        .checked_sub(1)
        .and_then(|index| quality.presets.get(index))
        .map_or((0, 0), |(width, height, _)| (*width, *height));
    let fps = fps
        .checked_sub(1)
        .and_then(|index| fps_options.get(index).copied())
        .unwrap_or(0);
    (width, height, fps)
}

/// 请求与当前生效值一致：不用下发（菜单回填触发的、或用户选了同一项）。
pub fn stream_request_unchanged(request: (u32, u32, u32), quality: &QualityState) -> bool {
    let (width, height, fps) = request;
    let size_same = width == 0 || (width == quality.width && height == quality.height);
    let fps_same = fps == 0 || fps == quality.fps;
    size_same && fps_same
}

/// 分辨率的简写（主流写法）：`4K` / `2K` / `1080p` / `720p`；对不上就用 `宽×高`。
pub fn short_size(width: u32, height: u32) -> String {
    match (width, height) {
        (3840, 2160) => "4K".to_string(),
        (2560, 1440) => "2K".to_string(),
        (_, 1080) => "1080p".to_string(),
        (_, 720) => "720p".to_string(),
        _ => format!("{width}×{height}"),
    }
}

/// 状态条上的连接信息：`1080p · 60 fps · P2P 直连`。
pub fn session_summary(
    width: u32,
    height: u32,
    fps: u32,
    transport: &str,
    watch_only: bool,
) -> String {
    let mut parts = vec![short_size(width, height), format!("{fps} fps")];
    if !transport.is_empty() {
        parts.push(transport.to_string());
    }
    if watch_only {
        parts.push("仅观看".to_string());
    }
    parts.join(" · ")
}

/// HUD 快照 → 两行等宽文本（纯函数可单测；core 契约：UI 只格式化）。
///
/// 形如：
/// `00:11:05 · UDP LAN · 1920×1080`
/// `60.0 fps · 14.2 Mbps · RTT 4.5 ms · 帧 12.3 ms · 丢包 0.1%`
pub fn format_hud(s: &rc_client_core::hud::HudSnapshot) -> String {
    let rate = if s.bitrate_kbps >= 1000.0 {
        format!("{:.1} Mbps", s.bitrate_kbps / 1000.0)
    } else {
        format!("{:.0} kbps", s.bitrate_kbps)
    };
    let rtt = s
        .rtt_ms
        .map_or_else(|| "—".into(), |v| format!("{v:.1} ms"));
    let frame = s
        .frame_ms
        .map_or_else(|| "—".into(), |v| format!("{v:.1} ms"));
    format!(
        "{} · {} · {}\n{:.1} fps · {} · RTT {} · 帧 {} · 丢包 {:.1}%",
        s.elapsed, s.transport, s.resolution, s.fps, rate, rtt, frame, s.loss_pct
    )
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

/// 被控端画面挂起（没有画面可收）时给用户的提示；画面正常时为 `None`。
///
/// 只在采集挂起时提示：Windows 被控端锁屏后采不到安全桌面，画面真的停了；macOS 被控端
/// 锁屏期间仍会送锁屏画面、可以直接键鼠解锁，不该拿提示把画面盖住。
pub fn host_screen_hint(locked: bool, capture_suspended: bool) -> Option<&'static str> {
    if !capture_suspended {
        return None;
    }
    Some(if locked {
        "远端电脑已锁屏，画面已暂停，解锁后自动恢复"
    } else {
        "远端屏幕已熄灭，正在唤醒…"
    })
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
    fn host_screen_hint_only_when_capture_is_suspended() {
        assert_eq!(
            host_screen_hint(true, true),
            Some("远端电脑已锁屏，画面已暂停，解锁后自动恢复")
        );
        assert_eq!(
            host_screen_hint(false, true),
            Some("远端屏幕已熄灭，正在唤醒…")
        );
        // 锁屏但画面还在送（macOS 被控端）：不盖住画面
        assert_eq!(host_screen_hint(true, false), None);
        assert_eq!(host_screen_hint(false, false), None);
    }

    #[test]
    fn stream_request_clamping() {
        let state = QualityState {
            presets: vec![
                (1920, 1080, "1920×1080".into()),
                (1280, 720, "1280×720".into()),
            ],
            width: 1920,
            height: 1080,
            fps: 60,
            max_fps: 120,
            ..Default::default()
        };
        // 命中预设 + 合法帧率
        assert_eq!(
            clamp_stream_request(1280, 720, 60, &state),
            Some((1280, 720, 60))
        );
        // 0 = 保持当前
        assert_eq!(clamp_stream_request(0, 0, 0, &state), Some((0, 0, 0)));
        // 当前尺寸本身合法
        assert_eq!(
            clamp_stream_request(1920, 1080, 120, &state),
            Some((1920, 1080, 120))
        );
        // 越界尺寸（ios 踩坑：不得下发）
        assert_eq!(clamp_stream_request(2560, 1440, 60, &state), None);
        // 帧率超 max_fps
        assert_eq!(clamp_stream_request(0, 0, 240, &state), None);
        // 长宽不匹配（512×1080 不在预设）
        assert_eq!(clamp_stream_request(512, 1080, 60, &state), None);
    }

    #[test]
    fn fps_candidates_filter_by_max() {
        assert_eq!(fps_candidates(240), vec![30, 60, 120, 144, 240]);
        assert_eq!(fps_candidates(60), vec![30, 60]);
        assert_eq!(fps_candidates(0), vec![30, 60, 120, 144, 240]); // 未上报 → 全量
    }

    #[test]
    fn bitrate_presets_map() {
        assert_eq!(bitrate_preset_to_bps("8m"), 8_000_000);
        assert_eq!(bitrate_preset_to_bps("14m"), 14_000_000);
        assert_eq!(bitrate_preset_to_bps("30m"), 30_000_000);
        assert_eq!(bitrate_preset_to_bps("auto"), 14_000_000);
        assert_eq!(bitrate_preset_to_bps("unknown"), 14_000_000);
    }

    fn sample_quality() -> QualityState {
        QualityState {
            presets: vec![
                (3840, 2160, "4K".to_string()),
                (1920, 1080, "1080P".to_string()),
            ],
            width: 1920,
            height: 1080,
            fps: 60,
            max_fps: 120,
            codec: Some("h264".to_string()),
            supported_codecs: vec!["hevc".to_string(), "h264".to_string()],
            mouse_mode: Some("remote".to_string()),
            displays: vec![(1, "显示器 1".to_string()), (2, "显示器 2".to_string())],
            active_display_id: 2,
            ..Default::default()
        }
    }

    #[test]
    fn setting_groups_follow_the_effective_values() {
        let groups = setting_groups(&sample_quality(), &[30, 60, 120], "14m");
        let group = |setting| {
            groups
                .iter()
                .find(|g| g.setting == setting)
                .expect("组存在")
        };
        assert_eq!(
            group(StreamSetting::Resolution).options,
            ["跟随当前", "4K", "1080P"]
        );
        assert_eq!(group(StreamSetting::Resolution).selected, 2);
        assert_eq!(
            group(StreamSetting::Fps).options,
            ["跟随当前", "30 fps", "60 fps", "120 fps"]
        );
        assert_eq!(group(StreamSetting::Fps).selected, 2);
        assert_eq!(
            group(StreamSetting::Bitrate).options,
            ["自适应", "8 Mbps", "14 Mbps", "30 Mbps"]
        );
        assert_eq!(group(StreamSetting::Bitrate).selected, 2);
        assert_eq!(
            group(StreamSetting::Codec).options,
            ["跟随当前", "HEVC", "H.264"]
        );
        assert_eq!(group(StreamSetting::Codec).selected, 2);
        assert_eq!(
            group(StreamSetting::Mouse).options,
            ["智能跟踪", "远程光标", "本地光标"]
        );
        assert_eq!(group(StreamSetting::Mouse).selected, 1);
        assert_eq!(
            group(StreamSetting::Display).options,
            ["显示器 1", "显示器 2"]
        );
        assert_eq!(group(StreamSetting::Display).selected, 1);
        // 对端没上报编解码：只剩「自动」且不可选
        let state = QualityState {
            supported_codecs: Vec::new(),
            ..sample_quality()
        };
        let codec = setting_groups(&state, &[60], "auto")
            .into_iter()
            .find(|g| g.setting == StreamSetting::Codec)
            .expect("编解码组");
        assert_eq!(codec.options, ["自动"]);
        assert!(!codec.enabled);
    }

    #[test]
    fn stream_requests_follow_menu_indexes() {
        let state = sample_quality();
        let options = [30u32, 60, 120];
        assert_eq!(stream_request(1, 3, &state, &options), (3840, 2160, 120));
        assert_eq!(stream_request(2, 1, &state, &options), (1920, 1080, 30));
        assert_eq!(stream_request(0, 0, &state, &options), (0, 0, 0));
        // 与生效值一致 → 不下发
        assert!(stream_request_unchanged((0, 0, 0), &state));
        assert!(stream_request_unchanged((1920, 1080, 0), &state));
        assert!(stream_request_unchanged((1920, 1080, 60), &state));
        assert!(!stream_request_unchanged((3840, 2160, 0), &state));
        assert!(!stream_request_unchanged((1920, 1080, 120), &state));
    }

    #[test]
    fn session_summary_uses_short_names() {
        assert_eq!(
            session_summary(1920, 1080, 60, "P2P 直连", false),
            "1080p · 60 fps · P2P 直连"
        );
        assert_eq!(
            session_summary(3840, 2160, 30, "", true),
            "4K · 30 fps · 仅观看"
        );
        assert_eq!(
            session_summary(1920, 1200, 60, "", false),
            "1920×1200 · 60 fps"
        );
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
            recoverable: false,
            ui_error: None,
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
    fn hud_formats_two_lines() {
        use rc_client_core::hud::HudSnapshot;
        let s = HudSnapshot {
            elapsed: "00:11:05".into(),
            transport: "UDP LAN",
            fps: 60.0,
            bitrate_kbps: 14_200.0,
            rtt_ms: Some(4.5),
            frame_ms: Some(12.34),
            loss_pct: 0.12,
            resolution: "1920x1080".into(),
        };
        let text = format_hud(&s);
        let (l1, l2) = text.split_once('\n').expect("两行");
        assert_eq!(l1, "00:11:05 · UDP LAN · 1920x1080");
        assert_eq!(
            l2,
            "60.0 fps · 14.2 Mbps · RTT 4.5 ms · 帧 12.3 ms · 丢包 0.1%"
        );
        // 低码率走 kbps；无样本字段显示占位符
        let empty = HudSnapshot {
            bitrate_kbps: 850.0,
            rtt_ms: None,
            frame_ms: None,
            ..s
        };
        let text2 = format_hud(&empty);
        assert!(text2.contains("850 kbps"), "{text2}");
        assert!(text2.contains("RTT —"), "{text2}");
        assert!(text2.contains("帧 —"), "{text2}");
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
