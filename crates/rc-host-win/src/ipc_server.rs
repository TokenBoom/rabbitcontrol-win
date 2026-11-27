//! 响应主界面 / 诊断工具经命名管道发来的请求（协议见 `rc_appcore::agent_ipc`）。
//!
//! 单线程循环，一次只处理一个客户端。需要同步拿到结果的握手（`gui-online`）
//! 直接在引擎上加锁执行——引擎线程一次 `tick` 只占锁几十毫秒，主界面等得起。
//! 本模块不碰任何窗口：状态变化经 [`EventSink`] 推给界面线程，进程级动作
//! （打开主界面、退出）由 [`Hooks`] 注入。

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use rc_appcore::agent_ipc::{self, AgentSnapshot, Request, Response};
use rc_appcore::named_pipe::PipeServer;
use serde_json::json;

use crate::diag_log;
use crate::engine::{Engine, EngineEvent, TrayView};

/// 引擎状态出口：界面线程据此刷新托盘与审批窗。
pub trait EventSink: Send + Sync {
    /// 发布最新状态（不触发界面重绘）。
    fn publish(&self, view: TrayView, snapshot: AgentSnapshot);
    /// 推送需要界面处理的事件（会唤醒界面线程）。
    fn push(&self, events: Vec<EngineEvent>);
}

/// 进程级动作（由 `app` 注入）。
pub struct Hooks {
    /// 打开或激活主界面。
    pub open_gui: Box<dyn Fn() + Send + Sync>,
    /// 整体退出。
    pub quit: Box<dyn Fn() + Send + Sync>,
}

/// 在当前线程运行管道服务，直到 `stop` 置位。
pub fn serve(
    engine: Arc<Mutex<Engine>>,
    hooks: Arc<Hooks>,
    sink: Arc<dyn EventSink>,
    stop: Arc<AtomicBool>,
) {
    serve_named(&agent_ipc::pipe_name(), engine, hooks, sink, stop);
}

/// [`serve`] 的底层形式：指定管道名（测试用唯一的名字，互不干扰）。
pub fn serve_named(
    pipe: &str,
    engine: Arc<Mutex<Engine>>,
    hooks: Arc<Hooks>,
    sink: Arc<dyn EventSink>,
    stop: Arc<AtomicBool>,
) {
    let server = match PipeServer::create(pipe, agent_ipc::MAX_FRAME_BYTES, None) {
        Ok(server) => server,
        Err(err) => {
            diag_log!("管道服务无法创建 {pipe}：{err}");
            return;
        }
    };
    diag_log!("管道服务已就绪：{pipe}");
    while server.wait_client(&stop) {
        let frame = match server.read_frame() {
            Ok(frame) => frame,
            Err(err) => {
                diag_log!("读取请求失败：{err}");
                server.finish_session();
                continue;
            }
        };
        let response = match serde_json::from_str::<Request>(frame.trim_end_matches(['\n', '\r'])) {
            Ok(request) => {
                diag_log!("请求：{request:?}");
                handle(&engine, &hooks, sink.as_ref(), request)
            }
            Err(err) => Response::err(format!("无法识别的请求：{err}")),
        };
        let wire = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
        if let Err(err) = server.write_frame(&wire) {
            diag_log!("写入响应失败：{err}");
        }
        server.finish_session();
    }
}

fn handle(
    engine: &Mutex<Engine>,
    hooks: &Hooks,
    sink: &dyn EventSink,
    request: Request,
) -> Response {
    let mut engine = engine.lock().unwrap_or_else(|e| e.into_inner());
    let publish = |engine: &Engine, events: Vec<EngineEvent>| {
        sink.publish(engine.tray_view(), engine.snapshot());
        sink.push(events);
    };
    match request {
        Request::Ping => Response::ok(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "pid": std::process::id(),
        })),
        Request::Snapshot => {
            let snapshot = engine.snapshot();
            Response::ok(serde_json::to_value(snapshot).unwrap_or(serde_json::Value::Null))
        }
        Request::GuiOnline { pid } => {
            // 同步让位：主界面拿到响应时，信令与被控已完全交给它
            let (host_enabled, events) =
                engine.suspend_for_gui(crate::gui_probe::process_probe(pid));
            publish(&engine, events);
            Response::ok(json!({ "host_enabled": host_enabled }))
        }
        Request::SetHostEnabled { enabled } => match engine.set_host_enabled(enabled) {
            Ok(events) => {
                publish(&engine, events);
                Response::ok(json!({ "host_enabled": engine.host_enabled() }))
            }
            Err(err) => Response::err(err),
        },
        Request::RespondApproval { generation, accept } => {
            let events = engine.respond_approval(generation, accept);
            publish(&engine, events);
            Response::ok(serde_json::Value::Null)
        }
        Request::DisconnectClient => {
            engine.disconnect_client();
            Response::ok(serde_json::Value::Null)
        }
        Request::OpenGui => {
            (hooks.open_gui)();
            Response::ok(serde_json::Value::Null)
        }
        Request::Quit => {
            (hooks.quit)();
            Response::ok(serde_json::Value::Null)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    /// 起一个真实的管道服务（独立线程），用一个未被登录的临时引擎应答。
    struct TestServer {
        pipe: String,
        stop: Arc<AtomicBool>,
    }

    impl TestServer {
        fn start(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("rc-host-ipc-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("server.json"), b"{}").unwrap();
            let engine = Arc::new(Mutex::new(
                Engine::with_paths(
                    &dir.join("server.json"),
                    &dir.join("auth.json"),
                    Some(&dir.join("certs")),
                    &dir.join("prefs.json"),
                    &dir.join("agent.json"),
                    Box::new(|| false),
                )
                .unwrap(),
            ));
            struct Recorder;
            impl EventSink for Recorder {
                fn publish(&self, _view: TrayView, _snapshot: AgentSnapshot) {}
                fn push(&self, _events: Vec<EngineEvent>) {}
            }
            let stop = Arc::new(AtomicBool::new(false));
            let hooks = Arc::new(Hooks {
                open_gui: Box::new(|| {}),
                quit: Box::new(|| {}),
            });
            let pipe = format!(
                r"\\.\pipe\rabbitcontrol-agent-test-{tag}-{}",
                std::process::id()
            );
            let client_pipe = pipe.clone();
            let stop_for_thread = Arc::clone(&stop);
            std::thread::spawn(move || {
                serve_named(&pipe, engine, hooks, Arc::new(Recorder), stop_for_thread);
                let _ = std::fs::remove_dir_all(&dir);
            });
            // 服务线程刚被拉起：等它把管道建好再让测试发请求
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                match agent_ipc::call_pipe(&client_pipe, &Request::Ping, 1_000) {
                    Ok(_) => break,
                    Err(err)
                        if err == "后台被控进程未运行" && std::time::Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(err) => panic!("测试管道服务未就绪：{err}"),
                }
            }
            Self {
                pipe: client_pipe,
                stop,
            }
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
        }
    }

    fn call(pipe: &str, request: &Request) -> Response {
        agent_ipc::call_pipe(pipe, request, 5_000).expect("管道调用应成功")
    }

    #[test]
    fn ping_reports_version_and_pid() {
        let server = TestServer::start("ping");
        // 服务线程刚起：管道可能还没建好，稍等
        let response = agent_ipc::call_pipe(&server.pipe, &Request::Ping, 5_000).expect("ping");
        assert!(response.ok);
        assert_eq!(
            response.get::<String>("version").as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(response.get::<u32>("pid"), Some(std::process::id()));
    }

    #[test]
    fn snapshot_and_host_toggle_round_trip() {
        let server = TestServer::start("toggle");
        let snapshot = call(&server.pipe, &Request::Snapshot);
        assert!(snapshot.ok);
        assert_eq!(snapshot.get::<bool>("logged_in"), Some(false));
        assert_eq!(snapshot.get::<bool>("host_enabled"), Some(false));

        let toggled = call(&server.pipe, &Request::SetHostEnabled { enabled: true });
        assert!(toggled.ok);
        assert_eq!(toggled.get::<bool>("host_enabled"), Some(true));
        let again = call(&server.pipe, &Request::Snapshot);
        assert_eq!(again.get::<bool>("host_enabled"), Some(true));
        assert_eq!(
            again.get::<serde_json::Value>("pending_approval"),
            Some(serde_json::Value::Null)
        );
    }

    #[test]
    fn gui_online_handshake_reports_host_toggle_and_suspends() {
        let server = TestServer::start("handshake");
        call(&server.pipe, &Request::SetHostEnabled { enabled: true });
        // 用本测试进程的 PID 冒充「主界面」：探测函数会盯着它（它一直活着）
        let response = call(
            &server.pipe,
            &Request::GuiOnline {
                pid: std::process::id(),
            },
        );
        assert!(response.ok);
        assert_eq!(response.get::<bool>("host_enabled"), Some(true));
        let snapshot = call(&server.pipe, &Request::Snapshot);
        assert_eq!(snapshot.get::<bool>("suspended"), Some(true));
        assert_eq!(
            snapshot.get::<serde_json::Value>("last_error"),
            Some(serde_json::Value::Null)
        );
    }

    #[test]
    fn unknown_requests_get_a_structured_error() {
        let server = TestServer::start("unknown");
        let raw =
            rc_appcore::named_pipe::round_trip(&server.pipe, "{\"op\":\"nope\"}", 4096, 5_000)
                .expect("管道应答");
        let response: Response = serde_json::from_str(&raw).unwrap();
        assert!(!response.ok);
        assert!(response.error.unwrap().contains("无法识别"));
    }

    #[test]
    fn missing_pipe_is_reported_as_not_running() {
        let missing = format!(r"\\.\pipe\rabbitcontrol-agent-gone-{}", std::process::id());
        let started = std::time::Instant::now();
        let err = agent_ipc::call_pipe(&missing, &Request::Ping, 5_000).unwrap_err();
        assert_eq!(err, "后台被控进程未运行");
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
