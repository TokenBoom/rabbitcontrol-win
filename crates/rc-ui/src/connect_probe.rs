//! 无界面连接诊断：`rabbitcontrol-win --connect-probe <设备码>`。
//!
//! 当「第二台设备」去连一个被控端，把协商结果和画面统计打印出来。用途是在**一台机器上**
//! 做端到端验证：被控端是后台进程 `rc-host-agent`（或另一个主界面实例），本进程用
//! `RC_CONFIG_DIR` 指向另一套配置目录，以同一账号登录成另一台设备，再去连它。
//!
//! ```text
//! set RC_CONFIG_DIR=D:\rc-probe-profile      # 另一套配置目录（第二台设备的身份）
//! set RC_PROBE_PASSWORD=<账号密码>            # 首次登录用；之后走会话恢复
//! rabbitcontrol-win --connect-probe 419450905 --origin https://host:21118 --email a@b.c --seconds 20
//! ```
//!
//! 不渲染到窗口（解码、呈现走平台默认的离屏表面），所以锁屏时也能跑。退出码：0 = 协商成功
//! 且收到了画面；1 = 失败（原因在 stderr）；2 = 参数错误。

use std::sync::Arc;
use std::time::{Duration, Instant};

use rc_appcore::signal_runtime::SignalRuntime;
use rc_client_core::auth_client::AuthSessionManager;
use rc_client_core::server_config::{self, ServerConnectionState};
use rc_client_core::session_bridge::{SessionHandle, SessionUpdate};

use crate::biz::{auth, session as sess};

/// 等信令上线、等被控端回应的上限。
const SIGNAL_ONLINE_TIMEOUT: Duration = Duration::from_secs(20);

fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let index = args.iter().position(|arg| arg == flag)?;
    args.get(index + 1).map(String::as_str)
}

/// 诊断入口。返回进程退出码。
pub fn run(args: &[String]) -> i32 {
    let Some(code) = flag_value(args, "--connect-probe") else {
        eprintln!(
            "用法：rabbitcontrol-win --connect-probe <设备码> [--seconds 20] \
             [--origin https://主机:21118 --email 邮箱]（密码用环境变量 RC_PROBE_PASSWORD）"
        );
        return 2;
    };
    let seconds: u64 = flag_value(args, "--seconds")
        .and_then(|value| value.parse().ok())
        .unwrap_or(20)
        .clamp(3, 600);
    // 第二台设备的身份必须放在另一套配置目录里：同一设备同一时刻只能有一条信令连接，
    // 用默认配置目录会把自己正在运行的主界面 / 后台进程顶下线。
    let profile = rc_client_core::config_paths::config_dir();
    if profile == rc_client_core::config_paths::user_config_dir()
        && !args.iter().any(|arg| arg == "--allow-default-profile")
    {
        eprintln!(
            "拒绝运行：当前配置目录就是默认的那一套（{}）。\n\
             请用环境变量 RC_CONFIG_DIR 指向另一个目录，让本进程成为「第二台设备」；\
             确实要占用本机身份请加 --allow-default-profile。",
            profile.display()
        );
        return 2;
    }
    match probe(code, seconds, args) {
        Ok(summary) => {
            eprintln!("[probe] 通过：{summary}");
            0
        }
        Err(err) => {
            eprintln!("[probe] 失败：{err}");
            1
        }
    }
}

/// 登录（恢复会话，或用 `--email` + `RC_PROBE_PASSWORD` 重新登录）。
fn sign_in(
    manager: &Arc<AuthSessionManager>,
    server_path: &std::path::Path,
    args: &[String],
) -> Result<auth::AccountSession, String> {
    if !manager.server_config().is_configured() {
        let origin = flag_value(args, "--origin")
            .ok_or("这套配置目录还没保存服务器，请加 --origin https://主机:21118")?;
        auth::probe_and_save_server(manager, server_path, origin)?;
    }
    if let Some(account) = auth::restore_session(manager, server_path)? {
        eprintln!("[probe] 恢复登录：{}", account.email);
        return Ok(account);
    }
    let email = flag_value(args, "--email")
        .ok_or("没有已保存的登录，请加 --email 并设置环境变量 RC_PROBE_PASSWORD")?;
    let password = std::env::var("RC_PROBE_PASSWORD")
        .ok()
        .filter(|password| !password.is_empty())
        .ok_or("没有已保存的登录：请设置环境变量 RC_PROBE_PASSWORD")?;
    let account = auth::login(manager, server_path, email, &password)?;
    eprintln!(
        "[probe] 登录成功：{}，本机设备码 {}",
        account.email,
        account
            .device_id
            .as_deref()
            .map(server_config::format_device_id)
            .unwrap_or_default()
    );
    Ok(account)
}

/// 等到信令上线（拿到本机设备码）。
fn wait_signal_online(runtime: &SignalRuntime) -> Result<(), String> {
    let deadline = Instant::now() + SIGNAL_ONLINE_TIMEOUT;
    loop {
        let events = runtime.poll();
        if let Some(status) = events.status {
            if let Some(err) = status.error_message {
                return Err(format!("信令：{err}"));
            }
            if status.state == ServerConnectionState::Connected && status.device_id.is_some() {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            return Err("信令 20 秒内没有上线".to_string());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn probe(code: &str, seconds: u64, args: &[String]) -> Result<String, String> {
    rc_platform_win::register_all();
    let auth_runtime = auth::AuthRuntime::new()?;
    let account = sign_in(&auth_runtime.manager(), &auth_runtime.server_path(), args)?;

    let runtime = SignalRuntime::new(auth_runtime.manager());
    runtime.sync()?;
    wait_signal_online(&runtime)?;
    eprintln!("[probe] 请求连接 {code} …");
    let grant = runtime.request_connect(code, sess::CONNECT_TIMEOUT)?;
    let params = sess::ConnectParams {
        password: None,
        watch_only: false,
        bitrate_bps: sess::bitrate_preset_to_bps("14m"),
        fps: 60,
        lan_direct: None,
    };
    let cfg = sess::build_session_config(&auth_runtime.manager(), grant, &params)?;
    let handle = SessionHandle::spawn(cfg);

    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut last_hud = Instant::now() - Duration::from_secs(1);
    let mut negotiated: Option<String> = None;
    let mut frames_seen = 0u32;
    let mut ended: Option<String> = None;
    // 被控端最近一次上报的（锁屏，采集挂起）
    let mut host_screen: Option<(bool, bool)> = None;
    while Instant::now() < deadline && ended.is_none() {
        while let Some(update) = handle.try_recv_update() {
            match update {
                SessionUpdate::Connecting { phase, .. } => {
                    eprintln!("[probe] 连接中：{}", sess::connect_phase_text(&phase))
                }
                SessionUpdate::Negotiated {
                    width,
                    height,
                    fps,
                    bitrate_bps,
                    backend,
                    transport,
                    mtls_active,
                    ..
                } => {
                    let text = format!(
                        "{width}x{height} @ {fps} fps · {} kbps · 解码/呈现 {backend} · 传输 {transport} · mTLS {mtls_active}",
                        bitrate_bps / 1000
                    );
                    eprintln!("[probe] 已协商：{text}");
                    negotiated = Some(text);
                }
                SessionUpdate::Hud(hud) => {
                    if Instant::now().duration_since(last_hud) < Duration::from_millis(900) {
                        continue;
                    }
                    last_hud = Instant::now();
                    if hud.fps > 0.0 {
                        frames_seen += 1;
                    }
                    eprintln!(
                        "[probe] HUD {}：{:.1} fps · {:.0} kbps · 往返 {:?} ms · 丢包 {:.2}% · {}",
                        hud.elapsed,
                        hud.fps,
                        hud.bitrate_kbps,
                        hud.rtt_ms,
                        hud.loss_pct,
                        hud.resolution
                    );
                }
                SessionUpdate::HostScreenState {
                    locked,
                    capture_suspended,
                } => {
                    if host_screen != Some((locked, capture_suspended)) {
                        host_screen = Some((locked, capture_suspended));
                        eprintln!(
                            "[probe] 远端状态：锁屏 {locked} · 采集挂起 {capture_suspended}{}",
                            sess::host_screen_hint(locked, capture_suspended)
                                .map(|hint| format!("（{hint}）"))
                                .unwrap_or_default()
                        );
                    }
                }
                SessionUpdate::Ended { reason, .. } => ended = Some(reason),
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    sess::disconnect(&handle);
    sess::join_session(handle);
    runtime.stop();
    drop(account);

    if let Some(reason) = ended {
        return Err(format!("会话提前结束：{reason}"));
    }
    let negotiated = negotiated.ok_or("没有完成协商（被控端没有回应，或握手失败）")?;
    if frames_seen == 0 {
        let suspended = host_screen
            .and_then(|(locked, suspended)| sess::host_screen_hint(locked, suspended))
            .map(|hint| format!("；{hint}"))
            .unwrap_or_default();
        return Err(format!(
            "已协商（{negotiated}），但 {seconds} 秒内没有收到任何画面（帧率始终为 0）{suspended}"
        ));
    }
    Ok(format!("{negotiated}；{} 次统计都收到了画面", frames_seen))
}
