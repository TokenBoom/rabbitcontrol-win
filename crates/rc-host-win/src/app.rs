//! 把引擎、托盘、审批窗、管道服务接到一个 Win32 消息循环里。
//!
//! # 线程模型
//!
//! - 界面线程（调用 [`run`] 的线程）：只跑消息循环，持有托盘图标与审批窗，
//!   从不等待引擎；
//! - 引擎线程：每 100ms 驱动一次引擎，并处理界面线程发来的命令；
//! - 管道线程：应答主界面 / 诊断工具的请求。
//!
//! 任何线程都不在持有引擎锁时去碰窗口；窗口里发生的事一律变成 [`Command`]
//! 交给引擎线程执行。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use rc_appcore::agent_ipc::AgentSnapshot;
use rc_win32core::{ApartmentModel, ComApartment};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, DispatchMessageW, GetMessageW, KillTimer, PostMessageW, PostQuitMessage,
    SetTimer, TranslateMessage, MSG, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLOSE, WM_DESTROY,
    WM_TIMER,
};

use crate::approval_window::{self, Prompt};
use crate::diag_log;
use crate::engine::{Engine, EngineEvent, TrayView};
use crate::gui_probe;
use crate::ipc_server::{self, EventSink, Hooks as IpcHooks};
use crate::tray::{self, MenuState, TrayCommand, TrayIcon};
use crate::win;

/// 后台进程主窗口类名（单实例判定也用它）。
pub const AGENT_WINDOW_CLASS: &str = "RabbitControlAgentWindow";
/// 引擎有新事件：界面线程排空队列。
const WM_ENGINE_EVENTS: u32 = WM_APP + 0x10;
/// 引擎线程已停妥：界面线程收尾退出。
const WM_ENGINE_STOPPED: u32 = WM_APP + 0x11;
/// 审批窗倒计时的定时器 ID。
const TIMER_APPROVAL: usize = 1;
/// 引擎驱动周期。
const TICK: Duration = Duration::from_millis(100);

/// 界面线程发给引擎线程的命令。
#[derive(Debug)]
enum Command {
    /// 开关「允许被控」。
    SetHostEnabled(bool),
    /// 应答审批请求。
    RespondApproval { generation: u64, accept: bool },
    /// 断开当前远端。
    Disconnect,
    /// 整体退出。
    Quit,
}

/// 进程级动作（由 `main` 注入）。
pub struct Hooks {
    /// 打开或激活主界面。
    pub open_gui: Box<dyn Fn() -> Result<(), String> + Send + Sync>,
    /// 用户主动退出后的收尾（例如请守护服务暂不拉起）。会话结束导致的退出不调用。
    pub after_user_quit: Box<dyn Fn() + Send + Sync>,
    /// 启动后弹一个演示审批窗（仅调试构建配合 `--demo-approval`）。
    pub demo_prompt: bool,
}

/// 界面线程侧的事件队列：其他线程往里放，再唤醒界面线程。
struct UiQueue {
    hwnd: AtomicIsize,
    items: Mutex<VecDeque<EngineEvent>>,
    latest: Mutex<(TrayView, AgentSnapshot)>,
}

impl UiQueue {
    fn new(hwnd: HWND, view: TrayView, snapshot: AgentSnapshot) -> Self {
        Self {
            hwnd: AtomicIsize::new(hwnd.0 as isize),
            items: Mutex::new(VecDeque::new()),
            latest: Mutex::new((view, snapshot)),
        }
    }

    fn take(&self) -> Option<EngineEvent> {
        self.items
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
    }

    fn menu_state(&self) -> (TrayView, AgentSnapshot) {
        self.latest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl EventSink for UiQueue {
    fn publish(&self, view: TrayView, snapshot: AgentSnapshot) {
        *self.latest.lock().unwrap_or_else(|e| e.into_inner()) = (view, snapshot);
    }

    fn push(&self, events: Vec<EngineEvent>) {
        if events.is_empty() {
            return;
        }
        self.items
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(events);
        let hwnd = HWND(self.hwnd.load(Ordering::SeqCst) as *mut core::ffi::c_void);
        unsafe {
            let _ = PostMessageW(hwnd, WM_ENGINE_EVENTS, WPARAM(0), LPARAM(0));
        }
    }
}

/// 托盘图标只在界面线程上使用；`TrayIcon` 内含裸句柄不是 `Send`。
struct UiOnly<T>(T);

// SAFETY: `UiOnly` 只存放在界面线程独占的静态里，其他线程从不读写。
unsafe impl<T> Send for UiOnly<T> {}

static TRAY: Mutex<UiOnly<Option<TrayIcon>>> = Mutex::new(UiOnly(None));

/// 界面线程共享状态（窗口过程通过它取队列与命令通道）。
struct UiState {
    queue: Arc<UiQueue>,
    commands: Sender<Command>,
    open_gui: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
}

static UI: OnceLock<UiState> = OnceLock::new();
/// 退出是否由用户主动发起（决定要不要通知守护服务）。
static USER_QUIT: AtomicBool = AtomicBool::new(false);

/// 后台进程主消息循环需要的 COM 套间（Shell / 剪贴板要求 STA）。
pub fn init_com() -> Result<ComApartment, String> {
    ComApartment::init(ApartmentModel::SingleThreaded)
}

/// 运行后台进程，直到用户从托盘退出或会话结束。阻塞当前线程。
pub fn run(engine: Engine, hooks: Hooks) -> Result<(), String> {
    // 单实例：同会话已有后台进程就不再起一个（它的管道也已经占着）
    if gui_probe::find_window(AGENT_WINDOW_CLASS).is_some() {
        return Err("后台被控进程已在运行".to_string());
    }
    let hwnd = win::create_hidden_window(
        AGENT_WINDOW_CLASS,
        "RabbitControl",
        WINDOW_STYLE(0),
        WINDOW_EX_STYLE(0),
        Some(agent_wndproc),
    )
    .map_err(|err| format!("创建后台主窗口失败：{err}"))?;
    approval_window::set_agent_window(hwnd);

    let engine = Arc::new(Mutex::new(engine));
    let (view, snapshot) = {
        let engine = engine.lock().unwrap_or_else(|e| e.into_inner());
        (engine.tray_view(), engine.snapshot())
    };
    let queue = Arc::new(UiQueue::new(hwnd, view, snapshot));
    let (sender, receiver) = mpsc::channel::<Command>();
    let open_gui: Arc<dyn Fn() -> Result<(), String> + Send + Sync> = Arc::new(move || {
        let result = (hooks.open_gui)();
        if let Err(ref err) = result {
            diag_log!("打开主界面：{err}");
        }
        result
    });
    UI.set(UiState {
        queue: Arc::clone(&queue),
        commands: sender.clone(),
        open_gui: Arc::clone(&open_gui),
    })
    .map_err(|_| "界面状态重复初始化".to_string())?;

    let stop = Arc::new(AtomicBool::new(false));
    spawn_ipc(
        Arc::clone(&engine),
        open_gui,
        sender,
        Arc::clone(&queue),
        Arc::clone(&stop),
    );
    spawn_engine(
        Arc::clone(&engine),
        receiver,
        hwnd.0 as isize,
        Arc::clone(&queue),
    );

    *TRAY.lock().unwrap_or_else(|e| e.into_inner()) = UiOnly(Some(
        TrayIcon::add(hwnd, view.tooltip()).map_err(|err| format!("创建托盘图标失败：{err}"))?,
    ));
    unsafe {
        SetTimer(hwnd, TIMER_APPROVAL, 1000, None);
    }
    if hooks.demo_prompt {
        show_demo_prompt();
    }

    let mut message = MSG::default();
    loop {
        // -1 = 出错，0 = WM_QUIT，其余 > 0
        if unsafe { GetMessageW(&mut message, None, 0, 0) }.0 <= 0 {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }

    // 收尾：摘托盘、收审批窗、停引擎
    unsafe {
        let _ = KillTimer(hwnd, TIMER_APPROVAL);
    }
    approval_window::hide();
    *TRAY.lock().unwrap_or_else(|e| e.into_inner()) = UiOnly(None);
    stop.store(true, Ordering::SeqCst);
    engine.lock().unwrap_or_else(|e| e.into_inner()).shutdown();
    if USER_QUIT.load(Ordering::SeqCst) {
        (hooks.after_user_quit)();
    }
    Ok(())
}

fn spawn_ipc(
    engine: Arc<Mutex<Engine>>,
    open_gui: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
    commands: Sender<Command>,
    queue: Arc<UiQueue>,
    stop: Arc<AtomicBool>,
) {
    let hooks = Arc::new(IpcHooks {
        open_gui: Box::new(move || {
            let _ = open_gui();
        }),
        quit: Box::new(move || {
            let _ = commands.send(Command::Quit);
        }),
    });
    let spawned = std::thread::Builder::new()
        .name("rc-agent-ipc".to_string())
        .spawn(move || ipc_server::serve(engine, hooks, queue, stop));
    if let Err(err) = spawned {
        diag_log!("管道线程启动失败：{err}");
    }
}

fn spawn_engine(
    engine: Arc<Mutex<Engine>>,
    receiver: mpsc::Receiver<Command>,
    hwnd_raw: isize,
    queue: Arc<UiQueue>,
) {
    // HWND 不是 Send：以裸值传进线程，线程里再还原
    let spawned = std::thread::Builder::new()
        .name("rc-agent-engine".to_string())
        .spawn(move || {
            let hwnd = HWND(hwnd_raw as *mut core::ffi::c_void);
            // 没有让位就先接管（登录恢复 / 连信令可能要几秒，放在引擎线程做）
            {
                let mut engine = engine.lock().unwrap_or_else(|e| e.into_inner());
                if engine.snapshot().suspended {
                    queue.publish(engine.tray_view(), engine.snapshot());
                } else {
                    let events = engine.activate();
                    queue.publish(engine.tray_view(), engine.snapshot());
                    queue.push(events);
                }
            }
            loop {
                let quit = match receiver.recv_timeout(TICK) {
                    Ok(Command::Quit) => true,
                    Ok(command) => {
                        let mut engine = engine.lock().unwrap_or_else(|e| e.into_inner());
                        let events = apply(&mut engine, command);
                        queue.publish(engine.tray_view(), engine.snapshot());
                        queue.push(events);
                        false
                    }
                    Err(RecvTimeoutError::Timeout) => false,
                    Err(RecvTimeoutError::Disconnected) => true,
                };
                if quit {
                    let mut engine = engine.lock().unwrap_or_else(|e| e.into_inner());
                    engine.shutdown();
                    queue.publish(engine.tray_view(), engine.snapshot());
                    unsafe {
                        let _ = PostMessageW(hwnd, WM_ENGINE_STOPPED, WPARAM(0), LPARAM(0));
                    }
                    diag_log!("引擎已停止");
                    return;
                }
                let mut engine = engine.lock().unwrap_or_else(|e| e.into_inner());
                let events = engine.tick();
                queue.publish(engine.tray_view(), engine.snapshot());
                queue.push(events);
            }
        });
    if let Err(err) = spawned {
        diag_log!("引擎线程启动失败：{err}");
    }
}

fn apply(engine: &mut Engine, command: Command) -> Vec<EngineEvent> {
    match command {
        Command::SetHostEnabled(on) => engine.set_host_enabled(on).unwrap_or_else(|err| {
            diag_log!("切换被控开关失败：{err}");
            Vec::new()
        }),
        Command::RespondApproval { generation, accept } => {
            engine.respond_approval(generation, accept)
        }
        Command::Disconnect => {
            engine.disconnect_client();
            Vec::new()
        }
        Command::Quit => Vec::new(),
    }
}

/// 调试用：不经过引擎，直接弹一个 30 秒后自动消失的审批窗。
fn show_demo_prompt() {
    let prompt = Prompt {
        peer: "演示设备 203.0.113.7:51820".to_string(),
        password_verified: false,
        deadline: std::time::Instant::now() + Duration::from_secs(30),
        generation: u64::MAX,
    };
    if let Err(err) = approval_window::show(prompt) {
        diag_log!("演示审批窗弹出失败：{err}");
    }
}

unsafe extern "system" fn agent_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let Some(state) = UI.get() else {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    };
    match msg {
        WM_ENGINE_EVENTS => {
            drain(state);
            LRESULT(0)
        }
        WM_ENGINE_STOPPED => {
            // 只有用户主动退出才会走到这里（会话结束走 WM_CLOSE，不置这个标志）
            USER_QUIT.store(true, Ordering::SeqCst);
            unsafe {
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        tray::TRAY_CALLBACK_MSG => {
            if let Some(at) = tray::menu_anchor(wparam, lparam) {
                on_tray_menu(state, hwnd, at);
            }
            LRESULT(0)
        }
        approval_window::RESULT_MSG => {
            // 代次随消息传递：审批窗点击后立刻收起，事后去查「当前待审批」已查不到
            let (generation, accept) = approval_window::unpack_result(wparam.0, lparam.0);
            diag_log!(
                "审批结果：{}（代次 {generation}）",
                if accept { "允许" } else { "拒绝" }
            );
            let _ = state
                .commands
                .send(Command::RespondApproval { generation, accept });
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == TIMER_APPROVAL && approval_window::tick_countdown() {
                approval_window::hide();
            }
            LRESULT(0)
        }
        _ if msg == tray::taskbar_created_msg() => {
            // 资源管理器重启：图标得重新挂
            if let Some(tray) = TRAY.lock().unwrap_or_else(|e| e.into_inner()).0.as_mut() {
                tray.re_add();
            }
            LRESULT(0)
        }
        WM_CLOSE | WM_DESTROY => {
            unsafe {
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn drain(state: &UiState) {
    while let Some(event) = state.queue.take() {
        let (view, _) = state.queue.menu_state();
        if let Some(tray) = TRAY.lock().unwrap_or_else(|e| e.into_inner()).0.as_mut() {
            tray.set_tooltip(view.tooltip());
        }
        match event {
            EngineEvent::StateChanged => {}
            EngineEvent::ApprovalRequested {
                generation,
                remote_addr,
                password_verified,
                deadline,
            } => {
                let prompt = Prompt {
                    peer: remote_addr,
                    password_verified,
                    deadline,
                    generation,
                };
                if let Err(err) = approval_window::show(prompt) {
                    diag_log!("审批窗弹出失败：{err}");
                }
            }
            EngineEvent::ApprovalResolved => approval_window::hide(),
            EngineEvent::SessionStarted { peer } => {
                balloon("RabbitControl", &format!("远端已连接：{peer}"))
            }
            EngineEvent::SessionEnded => balloon("RabbitControl", "远端已断开"),
        }
    }
}

fn balloon(title: &str, text: &str) {
    if let Some(tray) = TRAY.lock().unwrap_or_else(|e| e.into_inner()).0.as_mut() {
        tray.show_balloon(title, text);
    }
}

fn on_tray_menu(state: &UiState, hwnd: HWND, at: (i32, i32)) {
    let (view, snapshot) = state.queue.menu_state();
    let command = tray::show_menu(
        hwnd,
        at,
        &MenuState {
            host_enabled: snapshot.host_enabled,
            session_active: snapshot.session_active,
            device_code: snapshot.device_id.clone(),
            temp_code: snapshot.temp_code.clone(),
            status: tray::status_text(view, snapshot.connected_peer.as_deref()),
        },
    );
    let Some(command) = command else {
        return;
    };
    match command {
        TrayCommand::OpenGui => {
            let _ = (state.open_gui)();
        }
        TrayCommand::CopyDeviceCode => {
            copy_and_notify("设备码", snapshot.device_id.as_deref().unwrap_or_default());
        }
        TrayCommand::CopyTempCode => {
            copy_and_notify(
                "临时验证码",
                snapshot.temp_code.as_deref().unwrap_or_default(),
            );
        }
        TrayCommand::CopyShareText => copy_and_notify(
            "设备 ID 与验证码",
            &tray::share_text(
                snapshot.device_id.as_deref().unwrap_or_default(),
                snapshot.temp_code.as_deref().unwrap_or_default(),
            ),
        ),
        TrayCommand::ToggleHost => {
            let _ = state
                .commands
                .send(Command::SetHostEnabled(!snapshot.host_enabled));
        }
        TrayCommand::Disconnect => {
            let _ = state.commands.send(Command::Disconnect);
        }
        TrayCommand::Quit => {
            let _ = state.commands.send(Command::Quit);
        }
    }
}

/// 把文本放进剪贴板，并用托盘气泡告知结果（菜单已经关了，这是唯一的反馈渠道）。
fn copy_and_notify(what: &str, text: &str) {
    let copied = rc_clipboard_win::WinClipboard::new().and_then(|mut clipboard| {
        use rc_session::ClipboardBackend;
        clipboard.write_text(text)
    });
    match copied {
        Ok(()) => balloon("RabbitControl", &format!("已复制{what}")),
        Err(err) => balloon("RabbitControl", &format!("复制{what}失败：{err}")),
    }
}
