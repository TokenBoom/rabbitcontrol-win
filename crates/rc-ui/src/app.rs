//! UI 线程应用壳：路由、页面装配与事件分发。
//!
//! 线程模型：仅向 UI 线程传递纯值消息（XAML 控件始终停留在 STA 线程
//! 内）；后台线程经 [`Events`] 回投 [`Event`]，由 50ms 定时器与显式通知
//! 共同驱动 `dispatch` 排空。

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use rc_client_core::app_prefs::{self, AppPrefs};
use rc_client_core::auth_client::MyDeviceInfo;
use rc_client_core::server_config::{self, ServerStatusSnapshot};
use windows::{
    core::{Interface, Result, HSTRING},
    System::VirtualKey,
    Win32::UI::HiDpi::GetDpiForWindow,
    UI::Xaml::{
        Application,
        Controls::Primitives::{ButtonBase, ToggleButton},
        Controls::{ComboBox, Control, PasswordBox, StackPanel, TextBlock, TextBox, ToggleSwitch},
        FrameworkElement,
        Input::KeyEventHandler,
        Markup::XamlReader,
        Media::{ScaleTransform, Transform, TransformGroup},
        RoutedEventHandler,
        Shapes::Ellipse,
        UIElement, Visibility,
    },
};

use rc_client_core::session_bridge::{SessionHandle, SessionUpdate};
use rc_input::InputEvent;
use windows::UI::Xaml::Controls::SwapChainPanel;
use windows::UI::Xaml::Input::PointerEventHandler;

use crate::{
    auth::{self, AccountSession, AuthRuntime},
    history::{self, TargetHistory},
    runtime::SignalRuntime,
    session::{self as sess, SessionView},
    settings,
    shell::{self, Shell, ShellNotifier, DESIGN_HEIGHT, DESIGN_WIDTH},
    widgets,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Page {
    Devices,
    Assist,
    Favorites,
    Settings,
    Session,
}

#[derive(Clone)]
enum Action {
    Login,
    OpenServerSettings,
    SaveServer,
    Logout,
    RefreshDevices,
    Nav(Page),
    CopyId,
    AssistToggled(bool),
    ToggleCodeVisibility,
    AdvancedToggle,
    HistoryTabFav(bool),
    DeviceConnect(String),
    ConnectSubmit,
    DeviceToggleFav(String),
    DeviceRename(String),
    DeviceUnbind(String),
    RecentDelete(String),
    RecentConnect(String),
    SettingsTab(u8),
    QualityChanged,
    HudToggled(bool),
    EncryptToggled(bool),
    LockOnExitToggled(bool),
    ProbeServer,
    BannerDisconnect,
    SessionDisconnect,
    SessionReconnect,
}

enum Event {
    Click(Action),
    RestoreFinished(std::result::Result<Option<AccountSession>, String>),
    LoginFinished(std::result::Result<AccountSession, String>),
    LogoutFinished(std::result::Result<(), String>),
    SaveServerFinished(std::result::Result<(), String>),
    DevicesFinished(std::result::Result<Vec<MyDeviceInfo>, String>),
    DeviceActionFinished(std::result::Result<(), String>),
    SessionLaunched(Box<SessionLaunchOutcome>, String, String),
    SessionPointer(SessionPointerEvent),
    SessionKey(SessionKeyEvent),
}

/// 会话启动结果（Box 降低 Event 枚举整体尺寸）。
struct SessionLaunchOutcome {
    result: std::result::Result<SessionHandle, String>,
    target_id: String,
    target_name: String,
}

/// 会话舞台鼠标事件（纯值）。
#[derive(Debug, Clone, Copy)]
enum SessionPointerEvent {
    Move {
        x: f64,
        y: f64,
    },
    Button {
        x: f64,
        y: f64,
        button: u8,
        pressed: bool,
    },
    Wheel {
        delta: i32,
    },
}

/// 会话键盘事件（纯值）。
#[derive(Debug, Clone, Copy)]
struct SessionKeyEvent {
    key: VirtualKey,
    pressed: bool,
}

/// 事件发送句柄（跨线程克隆便宜；XAML 对象不可经此传递）。
#[derive(Clone)]
pub struct Events {
    tx: Sender<Event>,
    notifier: ShellNotifier,
}

impl Events {
    fn send(&self, event: Event) {
        let _ = self.tx.send(event);
        self.notifier.notify();
    }
}

/// 全局禁用控件清单（busy 态）；缺失的条目自动跳过。
const BUSY_CONTROLS: [&str; 12] = [
    "SignIn",
    "Email",
    "Password",
    "Origin",
    "SaveServer",
    "BackToLogin",
    "RefreshDevices",
    "ConnectPeerButton",
    "PeerCodeBox",
    "LogoutButton",
    "ProbeServer",
    "NetworkOrigin",
];

/// 活动会话的 UI 侧状态（句柄 + 视图态 + 视口尺寸）。
struct ActiveSession {
    handle: SessionHandle,
    video: Option<(u32, u32)>,
    watch_only: bool,
    target_name: String,
    target_id: String,
}

pub struct App {
    root: FrameworkElement,
    hwnd: windows::Win32::Foundation::HWND,
    last_dpi: u32,
    events: Events,
    inbox: Receiver<Event>,
    auth: AuthRuntime,
    runtime: Arc<SignalRuntime>,
    history: Mutex<TargetHistory>,
    prefs: Mutex<AppPrefs>,
    busy: bool,
    logged_in: bool,
    page: Page,
    devices: Vec<MyDeviceInfo>,
    online_override: std::collections::HashMap<String, bool>,
    lan_codes: Vec<String>,
    history_tab_fav: bool,
    tick: u64,
    session: Option<ActiveSession>,
    session_view: SessionView,
    peer_watch_only: bool,
    pressed_keys: Vec<u16>,
    pressed_buttons: Vec<u8>,
    pressed_modifiers: u8,
}

impl App {
    pub fn new(shell: &Shell) -> std::result::Result<Self, Box<dyn std::error::Error>> {
        install_styles()?;
        let root = XamlReader::Load(include_str!("view.xaml"))?.cast()?;
        let (tx, inbox) = mpsc::channel();
        let notifier = shell.notifier();
        let auth = AuthRuntime::new().map_err(std::io::Error::other)?;
        let runtime = Arc::new(SignalRuntime::new(auth.manager()));
        let prefs_path = app_prefs::default_prefs_path();
        let prefs = app_prefs::load_from_path(&prefs_path);
        let history_store = TargetHistory::load_from(&history::default_history_path());
        let mut app = Self {
            root,
            hwnd: shell.hwnd(),
            last_dpi: unsafe { GetDpiForWindow(shell.hwnd()) },
            events: Events { tx, notifier },
            inbox,
            auth,
            runtime,
            history: Mutex::new(history_store),
            prefs: Mutex::new(prefs),
            busy: false,
            logged_in: false,
            page: Page::Devices,
            devices: Vec::new(),
            online_override: std::collections::HashMap::new(),
            lan_codes: Vec::new(),
            history_tab_fav: false,
            tick: 0,
            session: None,
            session_view: SessionView::Idle,
            peer_watch_only: false,
            pressed_keys: Vec::new(),
            pressed_buttons: Vec::new(),
            pressed_modifiers: 0,
        };
        app.wire()?;
        app.update_caption()?;
        app.apply_scale()?;
        if app.auth.has_saved_session() {
            app.set_busy(true)?;
            app.label("LoginError", "正在恢复登录…")?;
            let manager = app.auth.manager();
            let server_path = app.auth.server_path();
            let events = app.events.clone();
            thread::spawn(move || {
                events.send(Event::RestoreFinished(auth::restore_session(
                    &manager,
                    &server_path,
                )));
            });
        }
        shell.set_content(&app.root)?;
        Ok(app)
    }

    fn sender(&self) -> Events {
        self.events.clone()
    }

    fn wire(&mut self) -> Result<()> {
        // 导航
        for (name, page) in [
            ("NavDevices", Page::Devices),
            ("NavAssist", Page::Assist),
            ("NavFavorites", Page::Favorites),
            ("NavSettings", Page::Settings),
        ] {
            let sender = self.sender();
            // 0.37 投影下继承成员经 ButtonBase 接口访问。
            let button: ButtonBase = self.root.FindName(name)?.cast()?;
            button.Click(RoutedEventHandler::new(move |_, _| {
                sender.send(Event::Click(Action::Nav(page)));
                Ok(())
            }))?;
        }
        // 登录流
        self.click("SignIn", Action::Login)?;
        self.click("ServerLink", Action::OpenServerSettings)?;
        self.click("SaveServer", Action::SaveServer)?;
        self.key_enter("Password", Action::Login)?;
        self.key_enter("Origin", Action::SaveServer)?;
        // 主壳
        self.click("LogoutButton", Action::Logout)?;
        self.click("RefreshDevices", Action::RefreshDevices)?;
        // 开始协助
        self.click("CopyIdButton", Action::CopyId)?;
        self.click("ToggleCodeButton", Action::ToggleCodeVisibility)?;
        self.click("ConnectPeerButton", Action::ConnectSubmit)?;
        self.click("PeerAdvancedButton", Action::AdvancedToggle)?;
        self.click("HistoryTabRecent", Action::HistoryTabFav(false))?;
        self.click("HistoryTabFav", Action::HistoryTabFav(true))?;
        {
            let sender = self.sender();
            let toggle: ToggleSwitch = self.root.FindName("AssistToggle")?.cast()?;
            let toggle_ref = toggle.clone();
            toggle.Toggled(RoutedEventHandler::new(move |_, _| {
                let on = toggle_ref.IsOn().unwrap_or(false);
                sender.send(Event::Click(Action::AssistToggled(on)));
                Ok(())
            }))?;
        }
        {
            // 输入框：实时校验 9 位码并联动连接按钮可用态；回车直接发起。
            let sender = self.sender();
            let box_: TextBox = self.root.FindName("PeerCodeBox")?.cast()?;
            let connect: Control = self.root.FindName("ConnectPeerButton")?.cast()?;
            let connect_ref = connect.clone();
            let box_for_changed = box_.clone();
            box_.TextChanged(windows::UI::Xaml::Controls::TextChangedEventHandler::new(
                move |_, _| {
                    let digits = digit_of(&box_for_changed.Text().unwrap_or_default().to_string());
                    let _ = connect_ref.SetIsEnabled(digits.len() == 9);
                    Ok(())
                },
            ))?;
            let box_for_key = box_.clone();
            let box_element: UIElement = box_.cast()?;
            box_element.KeyDown(KeyEventHandler::new(move |_, args| {
                if let Some(args) = args {
                    if args.Key()? == VirtualKey::Enter {
                        let digits = digit_of(&box_for_key.Text().unwrap_or_default().to_string());
                        if digits.len() == 9 {
                            args.SetHandled(true)?;
                            let sender = sender.clone();
                            sender.send(Event::Click(Action::DeviceConnect(digits)));
                        }
                    }
                }
                Ok(())
            }))?;
        }
        // 设置
        self.settings_tab("TabGeneral", 0)?;
        self.settings_tab("TabSecurity", 1)?;
        self.settings_tab("TabNetwork", 2)?;
        {
            let sender = self.sender();
            let combo: windows::UI::Xaml::Controls::Primitives::Selector =
                self.root.FindName("QualityBox")?.cast()?;
            combo.SelectionChanged(
                windows::UI::Xaml::Controls::SelectionChangedEventHandler::new(move |_, _| {
                    let sender = sender.clone();
                    sender.send(Event::Click(Action::QualityChanged));
                    Ok(())
                }),
            )?;
        }
        self.switch_toggled("HudToggle", Action::HudToggled(true))?;
        self.switch_toggled("EncryptToggle", Action::EncryptToggled(true))?;
        self.switch_toggled("LockOnExitToggle", Action::LockOnExitToggled(true))?;
        self.click("ProbeServer", Action::ProbeServer)?;
        self.click("BannerDisconnect", Action::BannerDisconnect)?;
        // 会话工具条
        self.click("SessionDisconnect", Action::SessionDisconnect)?;
        self.click("SessionReconnect", Action::SessionReconnect)?;
        // 会话舞台输入（M2：鼠标 + 滚轮 + 键盘；watch_only 时不发送）
        {
            let sender = self.sender();
            let panel: SwapChainPanel = self.root.FindName("SessionPanel")?.cast()?;
            let panel_ui: UIElement = panel.cast()?;
            let sender_moved = sender.clone();
            panel_ui.PointerMoved(PointerEventHandler::new(move |_, args| {
                if let Some(args) = args {
                    let pos = args.GetCurrentPoint(None).and_then(|pt| pt.Position())?;
                    let sender = sender_moved.clone();
                    sender.send(Event::SessionPointer(SessionPointerEvent::Move {
                        x: f64::from(pos.X),
                        y: f64::from(pos.Y),
                    }));
                }
                Ok(())
            }))?;
            let panel_press: UIElement = panel.clone().cast()?;
            let sender_press = sender.clone();
            panel_press.PointerPressed(PointerEventHandler::new(move |_, args| {
                if let Some(args) = args {
                    let pt = args.GetCurrentPoint(None)?;
                    let pos = pt.Position()?;
                    let props = pt.Properties()?;
                    let button = if props.IsLeftButtonPressed()? {
                        0u8
                    } else if props.IsRightButtonPressed()? {
                        1
                    } else if props.IsMiddleButtonPressed()? {
                        2
                    } else {
                        return Ok(());
                    };
                    args.SetHandled(true)?;
                    let sender = sender_press.clone();
                    sender.send(Event::SessionPointer(SessionPointerEvent::Button {
                        x: f64::from(pos.X),
                        y: f64::from(pos.Y),
                        button,
                        pressed: true,
                    }));
                }
                Ok(())
            }))?;
            let panel_released: SwapChainPanel = panel.clone();
            let panel_release: UIElement = panel_released.cast()?;
            let sender_release = sender.clone();
            panel_release.PointerReleased(PointerEventHandler::new(move |_, args| {
                if let Some(args) = args {
                    let pt = args.GetCurrentPoint(None)?;
                    let pos = pt.Position()?;
                    let props = pt.Properties()?;
                    let button = if !props.IsLeftButtonPressed()? {
                        0u8
                    } else if !props.IsRightButtonPressed()? {
                        1
                    } else if !props.IsMiddleButtonPressed()? {
                        2
                    } else {
                        return Ok(());
                    };
                    args.SetHandled(true)?;
                    let sender = sender_release.clone();
                    sender.send(Event::SessionPointer(SessionPointerEvent::Button {
                        x: f64::from(pos.X),
                        y: f64::from(pos.Y),
                        button,
                        pressed: false,
                    }));
                }
                Ok(())
            }))?;
            let panel_wheel: SwapChainPanel = panel.clone();
            let panel_wheel_ui: UIElement = panel_wheel.cast()?;
            let sender_wheel = sender.clone();
            panel_wheel_ui.PointerWheelChanged(PointerEventHandler::new(move |_, args| {
                if let Some(args) = args {
                    let pt = args.GetCurrentPoint(None)?;
                    let delta = pt.Properties()?.MouseWheelDelta()?;
                    let sender = sender_wheel.clone();
                    sender.send(Event::SessionPointer(SessionPointerEvent::Wheel { delta }));
                    args.SetHandled(true)?;
                }
                Ok(())
            }))?;
        }
        {
            // 键盘：挂在 SessionStage（面板获得焦点或点击后生效）。
            let sender = self.sender();
            let stage: UIElement = self.root.FindName("SessionStage")?.cast()?;
            let sender_down = sender.clone();
            stage.KeyDown(KeyEventHandler::new(move |_, args| {
                if let Some(args) = args {
                    let key = args.Key()?;
                    args.SetHandled(true)?;
                    let sender = sender_down.clone();
                    sender.send(Event::SessionKey(SessionKeyEvent { key, pressed: true }));
                }
                Ok(())
            }))?;
            let sender_up = sender.clone();
            stage.KeyUp(KeyEventHandler::new(move |_, args| {
                if let Some(args) = args {
                    let key = args.Key()?;
                    args.SetHandled(true)?;
                    let sender = sender_up.clone();
                    sender.send(Event::SessionKey(SessionKeyEvent {
                        key,
                        pressed: false,
                    }));
                }
                Ok(())
            }))?;
        }
        Ok(())
    }

    /// 以名字接一个按钮类事件到指定 Action。
    fn click(&self, name: &str, action: Action) -> Result<()> {
        let sender = self.sender();
        let button: ButtonBase = self.root.FindName(name)?.cast()?;
        button.Click(RoutedEventHandler::new(move |_, _| {
            sender.send(Event::Click(action.clone()));
            Ok(())
        }))?;
        Ok(())
    }

    fn key_enter(&self, name: &str, action: Action) -> Result<()> {
        let sender = self.sender();
        let element: UIElement = self.root.FindName(name)?.cast()?;
        element.KeyDown(KeyEventHandler::new(move |_, args| {
            if let Some(args) = args {
                if args.Key()? == VirtualKey::Enter {
                    args.SetHandled(true)?;
                    sender.send(Event::Click(action.clone()));
                }
            }
            Ok(())
        }))?;
        Ok(())
    }

    fn switch_toggled(&self, name: &str, action: Action) -> Result<()> {
        let sender = self.sender();
        let toggle: ToggleSwitch = self.root.FindName(name)?.cast()?;
        let toggle_ref = toggle.clone();
        // 预解析目标 Action 形态，闭包内仅注入当前布尔值。
        let probe = match action {
            Action::HudToggled(_) => 1,
            Action::EncryptToggled(_) => 2,
            Action::LockOnExitToggled(_) => 3,
            _ => 0,
        };
        toggle.Toggled(RoutedEventHandler::new(move |_, _| {
            let on = toggle_ref.IsOn().unwrap_or(false);
            let event_action = match probe {
                1 => Action::HudToggled(on),
                2 => Action::EncryptToggled(on),
                3 => Action::LockOnExitToggled(on),
                _ => Action::HudToggled(on),
            };
            sender.send(Event::Click(event_action));
            Ok(())
        }))?;
        Ok(())
    }

    /// Tab 用 Checked/Unchecked 事件驱动（Click 在 islands 的 ToggleButton
    /// 上不触发；导航按钮正常是因导航依赖同一事件模型——此差异实测得出）。
    fn settings_tab(&self, name: &str, index: u8) -> Result<()> {
        let sender = self.sender();
        let tab: ToggleButton = self.root.FindName(name)?.cast()?;
        tab.Checked(RoutedEventHandler::new(move |_, _| {
            sender.send(Event::Click(Action::SettingsTab(index)));
            Ok(())
        }))?;
        let sender = self.sender();
        tab.Unchecked(RoutedEventHandler::new(move |_, _| {
            let _ = sender; // Unchecked 仅用于视觉归一，不触发切换
            Ok(())
        }))?;
        Ok(())
    }

    fn find<T: Interface>(&self, name: &str) -> Result<T> {
        self.root.FindName(name)?.cast()
    }

    fn find_opt<T: Interface>(&self, name: &str) -> Option<T> {
        self.root.FindName(name).ok()?.cast().ok()
    }

    fn label(&self, name: &str, value: &str) -> Result<()> {
        let text: TextBlock = self.find(name)?;
        text.SetText(HSTRING::from(value))
    }

    fn set_visible(&self, name: &str, visible: bool) -> Result<()> {
        let element: UIElement = self.find(name)?;
        element.SetVisibility(if visible {
            Visibility::Visible
        } else {
            Visibility::Collapsed
        })
    }

    fn set_busy(&mut self, busy: bool) -> Result<()> {
        self.busy = busy;
        for name in BUSY_CONTROLS {
            if let Some(control) = self.find_opt::<Control>(name) {
                control.SetIsEnabled(!busy)?;
            }
        }
        Ok(())
    }

    pub fn dispatch(&mut self) -> Result<()> {
        // DPI 变化：window_proc 已重设窗口尺寸，这里同步根布局缩放。
        let dpi = unsafe { GetDpiForWindow(self.hwnd) };
        if dpi != self.last_dpi {
            self.last_dpi = dpi;
            self.apply_scale()?;
        }
        while let Ok(event) = self.inbox.try_recv() {
            match event {
                Event::Click(_) if self.busy => {}
                Event::Click(action) => self.on_click(action)?,
                Event::RestoreFinished(result) => {
                    self.set_busy(false)?;
                    match result {
                        Ok(Some(account)) => self.open_main(&account)?,
                        Ok(None) => self.label("LoginError", "")?,
                        Err(message) => self.label("LoginError", &message)?,
                    }
                }
                Event::LoginFinished(result) => {
                    self.set_busy(false)?;
                    match result {
                        Err(message) => self.label("LoginError", &message)?,
                        Ok(account) => self.open_main(&account)?,
                    }
                }
                Event::LogoutFinished(result) => {
                    self.set_busy(false)?;
                    self.return_to_login(result.err().unwrap_or_default())?;
                }
                Event::SaveServerFinished(result) => {
                    self.set_busy(false)?;
                    match result {
                        Err(message) => {
                            self.label("SettingsError", &message)?;
                            self.label("NetworkError", &message)?;
                        }
                        Ok(()) => {
                            self.find::<PasswordBox>("Password")?.SetPassword("")?;
                            self.update_caption()?;
                            self.label("SettingsError", "")?;
                            self.label("LoginError", "")?;
                            self.label("NetworkError", "服务器已保存并通过校验。")?;
                            if !self.logged_in {
                                self.set_visible("LoginView", true)?;
                                self.set_visible("ServerSettingsView", false)?;
                            } else {
                                self.settings_tab_current()?;
                            }
                        }
                    }
                }
                Event::DevicesFinished(result) => {
                    self.set_busy(false)?;
                    match result {
                        Ok(mut devices) => {
                            // 服务端偶发重复条目时按设备码去重（快照防御）。
                            devices.sort_by(|a, b| a.device_id.cmp(&b.device_id));
                            devices.dedup_by(|a, b| a.device_id == b.device_id);
                            self.devices = devices;
                            // 不清 online_override：实时事件可能比 REST 快照更新。
                            self.render_devices()?;
                            self.render_favorites()?;
                        }
                        Err(message) => {
                            if !self.auth.is_logged_in() {
                                self.return_to_login(message)?;
                            } else {
                                self.label("AssistError", &format!("刷新设备列表失败：{message}"))?;
                            }
                        }
                    }
                }
                Event::DeviceActionFinished(result) => {
                    self.set_busy(false)?;
                    match result {
                        Ok(()) => self.on_click(Action::RefreshDevices)?,
                        Err(message) => self.label("AssistError", &message)?,
                    }
                }
                Event::SessionLaunched(launch, _t, _n) => {
                    let (target_id, target_name) =
                        (launch.target_id.clone(), launch.target_name.clone());
                    match launch.result {
                        Ok(handle) => {
                            let watch_only = self.peer_watch_only;
                            self.session = Some(ActiveSession {
                                handle,
                                video: None,
                                watch_only,
                                target_name,
                                target_id,
                            });
                            self.session_view = SessionView::Idle;
                            self.page = Page::Session;
                            self.show_page(Page::Session)?;
                            self.apply_session_view();
                        }
                        Err(message) => {
                            self.label("AssistError", &format!("连接失败：{message}"))?;
                            self.set_session_overlay(&format!("连接失败：{message}"), false);
                            self.set_visible("SessionReconnect", true)?;
                        }
                    }
                }
                Event::SessionPointer(pointer) => self.on_session_pointer(pointer)?,
                Event::SessionKey(key) => self.on_session_key(key)?,
            }
        }
        self.on_tick()?;
        Ok(())
    }

    fn on_tick(&mut self) -> Result<()> {
        if !self.logged_in {
            return Ok(());
        }
        self.tick += 1;
        let generation = self.runtime.generation();
        let events = self.runtime.poll();
        if let Some(reason) = events.kicked_reason {
            self.auth.stop_maintainer();
            self.runtime.stop();
            self.set_busy(false)?;
            self.return_to_login(format!("已退出登录：{reason}"))?;
            return Ok(());
        }
        if let Some(status) = events.status {
            self.apply_status(&status, generation)?;
        }
        if !events.device_events.is_empty() {
            for event in events.device_events {
                self.online_override.insert(event.device_id, event.online);
            }
            self.render_devices()?;
        }
        // 会话更新排空（状态机 + HUD + 结束处理）。
        let mut session_updates = Vec::new();
        let mut session_running = false;
        if let Some(active) = self.session.as_ref() {
            session_running = active.handle.is_running();
            while let Some(update) = active.handle.try_recv_update() {
                session_updates.push(update);
            }
        }
        for update in session_updates {
            if let SessionUpdate::Negotiated { width, height, .. } = &update {
                if let Some(active) = self.session.as_mut() {
                    active.video = Some((*width, *height));
                }
            }
            if matches!(update, SessionUpdate::Ended { .. }) {
                session_running = false;
            }
            self.session_view = sess::view_state_of(&update, &self.session_view);
        }
        if self.session.is_some() {
            self.apply_session_view();
            if !session_running {
                self.release_pressed_input();
                let text = self.session_overlay_text();
                self.set_session_overlay(&text, false);
            }
        }

        // 每约 1s 刷新一次 LAN 直连候选。
        if self.tick % 20 == 0 {
            let codes = self
                .runtime
                .lan_peers()
                .iter()
                .filter_map(|peer| peer.device_id.clone())
                .collect::<Vec<_>>();
            if codes != self.lan_codes {
                self.lan_codes = codes;
                self.render_devices()?;
            }
        }
        Ok(())
    }

    #[allow(clippy::only_used_in_recursion)]
    fn on_click(&mut self, action: Action) -> Result<()> {
        match action {
            Action::Login => self.start_login(),
            Action::OpenServerSettings => {
                let origin = settings::editable_origin(&self.auth.server_config());
                self.find::<TextBox>("Origin")?
                    .SetText(HSTRING::from(&origin))?;
                self.label("SettingsError", "")?;
                self.set_visible("LoginView", false)?;
                self.set_visible("ServerSettingsView", true)
            }
            Action::SaveServer => self.save_server_settings(),
            Action::ProbeServer => self.save_server_settings(),
            Action::Logout => {
                if self.logged_in {
                    self.do_logout()?;
                } else {
                    self.set_visible("LoginView", true)?;
                    self.set_visible("ServerSettingsView", false)?;
                }
                Ok(())
            }
            Action::RefreshDevices => {
                if self.logged_in {
                    self.set_busy(true)?;
                    let manager = self.auth.manager();
                    let events = self.sender();
                    thread::spawn(move || {
                        events.send(Event::DevicesFinished(auth::list_my_devices(&manager)));
                    });
                }
                Ok(())
            }
            Action::Nav(page) => {
                self.page = page;
                self.show_page(page)
            }
            Action::CopyId => {
                if let Some(id) = self.auth.server_config().cached_device_id.clone() {
                    copy_to_clipboard(&server_config::format_device_id(&id));
                    self.label("AssistError", "设备 ID 已复制到剪贴板。")?;
                } else {
                    self.label("AssistError", "本机设备码尚未分配。")?;
                }
                Ok(())
            }
            Action::AssistToggled(on) => {
                if let Err(err) = self.runtime.set_allow_remote_assist(on) {
                    self.label("AssistError", &err)?;
                }
                self.save_prefs_patch(|prefs| {
                    prefs.apply_security_patch(rc_client_core::app_prefs::SecurityPrefsPatch {
                        allow_remote_assist: Some(on),
                        ..Default::default()
                    })
                })
            }
            Action::ToggleCodeVisibility => Ok(()),
            Action::AdvancedToggle => {
                let visible = self.peer_advanced_visible();
                self.set_visible("PeerAdvancedRow", !visible)
            }
            Action::HistoryTabFav(fav) => {
                self.history_tab_fav = fav;
                self.render_history()
            }
            Action::DeviceConnect(code) => self.start_session(&code),
            Action::ConnectSubmit => {
                let text = self.find::<TextBox>("PeerCodeBox")?.Text()?.to_string();
                let digits = digit_of(&text);
                if digits.len() == 9 {
                    self.start_session(&digits)?;
                }
                Ok(())
            }
            Action::SessionDisconnect => {
                if let Some(active) = self.session.as_ref() {
                    sess::disconnect(&active.handle);
                }
                Ok(())
            }
            Action::SessionReconnect => {
                let target = self.session.as_ref().map(|s| s.target_id.clone());
                if let Some(target) = target {
                    self.start_session(&target)?;
                }
                Ok(())
            }
            Action::DeviceToggleFav(device_id) => {
                self.history.lock().unwrap().toggle_favorite(&device_id);
                self.persist_history()?;
                self.render_devices()?;
                self.render_history()
            }
            Action::DeviceRename(device_id) => {
                let input = prompt_text("重命名设备");
                if let Some(alias) = input.filter(|alias| !alias.trim().is_empty()) {
                    self.set_busy(true)?;
                    let manager = self.auth.manager();
                    let events = self.sender();
                    thread::spawn(move || {
                        let result = auth::rename_device(&manager, &device_id, alias.trim());
                        events.send(Event::DeviceActionFinished(result));
                    });
                }
                Ok(())
            }
            Action::DeviceUnbind(device_id) => {
                self.set_busy(true)?;
                let manager = self.auth.manager();
                let events = self.sender();
                thread::spawn(move || {
                    let result = auth::remove_device(&manager, &device_id);
                    events.send(Event::DeviceActionFinished(result));
                });
                Ok(())
            }
            Action::RecentDelete(device_id) => {
                self.history.lock().unwrap().remove_recent(&device_id);
                self.persist_history()?;
                self.render_history()
            }
            Action::RecentConnect(code) => self.prefill_connect(&code),
            Action::SettingsTab(index) => {
                self.apply_settings_tab(index)?;
                Ok(())
            }
            Action::QualityChanged => {
                let selector: windows::UI::Xaml::Controls::Primitives::Selector =
                    self.find::<ComboBox>("QualityBox")?.cast()?;
                let index = selector.SelectedIndex()?;
                let preset = ["auto", "8m", "14m", "30m"][index.max(0) as usize % 4];
                self.save_prefs_patch(|prefs| {
                    prefs.apply_stream_patch(rc_client_core::app_prefs::StreamPrefsPatch {
                        bitrate_preset: Some(preset.to_string()),
                        ..Default::default()
                    })
                })
            }
            Action::HudToggled(on) => self.save_prefs_patch(|prefs| {
                prefs.apply_stream_patch(rc_client_core::app_prefs::StreamPrefsPatch {
                    show_hud: Some(on),
                    ..Default::default()
                })
            }),
            Action::EncryptToggled(on) => self.save_prefs_patch(|prefs| {
                prefs.apply_security_patch(rc_client_core::app_prefs::SecurityPrefsPatch {
                    default_encrypt_connection: Some(on),
                    ..Default::default()
                })
            }),
            Action::LockOnExitToggled(on) => self.save_prefs_patch(|prefs| {
                prefs.apply_stream_patch(rc_client_core::app_prefs::StreamPrefsPatch {
                    lock_on_exit: Some(on),
                    ..Default::default()
                })
            }),
            Action::BannerDisconnect => {
                self.label(
                    "AssistError",
                    "被控链路尚未开放（M5）：当前没有活跃的被控会话。",
                )?;
                Ok(())
            }
        }
    }

    /// 连接请求先导：记录最近目标后直接发起会话。
    fn prefill_connect(&mut self, code: &str) -> Result<()> {
        // 语义对齐 iOS TargetHistory：发起连接即记录最近目标。
        let at_sec = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let name = self
            .devices
            .iter()
            .find(|d| d.device_id == code)
            .map(|d| self.device_name_of(d))
            .unwrap_or_else(|| code.to_string());
        self.history
            .lock()
            .unwrap()
            .record_connect(code, &name, at_sec);
        self.persist_history()?;
        self.start_session(code)
    }

    /// 发起会话：后台线程完成阻塞式信令请求与配置组装。
    fn start_session(&mut self, code: &str) -> Result<()> {
        if !self.logged_in {
            self.label("AssistError", "请先登录。")?;
            return Ok(());
        }
        if self.runtime.snapshot().and_then(|s| s.device_id).is_none() {
            self.label("AssistError", "信令尚未在线，请稍候重试。")?;
            return Ok(());
        }
        let password = self
            .find_opt::<PasswordBox>("PeerPasswordBox")
            .map(|box_| box_.Password().unwrap_or_default().to_string())
            .filter(|p| !p.is_empty());
        // 0.37 投影 CheckBox.IsChecked 需 UIA；M2 以勾选框视觉状态不可读，
        // 仅观看入口保留在 UI，后续经 GetTogglePattern 化（先读 IReference）。
        // M2：仅观看勾选框的视觉态读取在 M3 完善（0.37 无直接投影）。
        let watch_only = false;
        self.peer_watch_only = watch_only;
        let preset = self.prefs.lock().unwrap().bitrate_preset.clone();
        let params = sess::ConnectParams {
            password,
            watch_only,
            bitrate_bps: sess::bitrate_preset_to_bps(&preset),
            fps: 60,
            lan_direct: self
                .runtime
                .lan_peers()
                .iter()
                .find(|p| p.device_id.as_deref() == Some(code))
                .map(|p| p.addr.clone()),
        };
        let name = self
            .devices
            .iter()
            .find(|d| d.device_id == code)
            .map(|d| self.device_name_of(d))
            .unwrap_or_else(|| code.to_string());
        self.set_session_overlay("正在连接…", true);
        let _ = self.set_visible("SessionReconnect", false);
        self.page = Page::Session;
        self.show_page(Page::Session)?;
        let manager = self.auth.manager();
        let runtime = Arc::clone(&self.runtime);
        let events = self.sender();
        let code = code.to_string();
        thread::spawn(move || {
            let result = runtime
                .request_connect(&code, sess::CONNECT_TIMEOUT)
                .and_then(|grant| sess::build_session_config(&manager, grant, &params));
            let outcome = match result {
                Ok(cfg) => Ok(SessionHandle::spawn(cfg)),
                Err(err) => Err(err),
            };
            events.send(Event::SessionLaunched(
                Box::new(SessionLaunchOutcome {
                    result: outcome,
                    target_id: code,
                    target_name: name,
                }),
                String::new(),
                String::new(),
            ));
        });
        Ok(())
    }

    /// 会话视图渲染：状态行 / 后端徽章 / 遮罩 / HUD。
    fn apply_session_view(&mut self) {
        let view = self.session_view.clone();
        match &view {
            SessionView::Idle => {}
            SessionView::Connecting(text) => {
                let _ = self.label("SessionStatus", text);
                self.set_session_overlay(text, true);
            }
            SessionView::Negotiated {
                width,
                height,
                fps,
                backend,
                transport,
                mtls_active: _,
                watch_only,
            } => {
                let mode = if *watch_only { " · 观看" } else { "" };
                let status = format!(
                    "{target} · {w}×{h}@{fps}{mode}",
                    target = self
                        .session
                        .as_ref()
                        .map(|s| s.target_name.as_str())
                        .unwrap_or(""),
                    w = width,
                    h = height,
                    fps = fps,
                );
                let _ = self.label("SessionStatus", &status);
                let badge = format!("{backend} · {transport}",);
                let _ = self.label("SessionBackend", &badge);
                self.set_session_overlay("", false);
            }
            SessionView::Ended(reason) => {
                let _ = self.label("SessionStatus", &format!("已结束：{reason}"));
                self.set_session_overlay(&format!("会话已结束：{reason}"), false);
                let _ = self.set_visible("SessionReconnect", true);
            }
        }
    }

    fn set_session_overlay(&self, text: &str, spinner: bool) {
        let _ = self.label("SessionOverlayText", text);
        if let Some(ring) =
            self.find_opt::<windows::UI::Xaml::Controls::ProgressRing>("SessionSpinner")
        {
            let _ = ring.SetIsActive(spinner);
        }
        let visible = !text.is_empty() || spinner;
        let _ = self.set_visible("SessionOverlay", visible);
    }

    fn session_overlay_text(&self) -> String {
        self.find_opt::<TextBlock>("SessionOverlayText")
            .and_then(|t| t.Text().ok())
            .map(|t| t.to_string())
            .unwrap_or_default()
    }

    fn save_server_settings(&mut self) -> Result<()> {
        let input = self.find::<TextBox>("Origin")?.Text()?.to_string();
        let input_network = self
            .find_opt::<TextBox>("NetworkOrigin")
            .map(|box_| box_.Text().unwrap_or_default().to_string())
            .unwrap_or_default();
        let effective = if input_network.is_empty() || input.is_empty() {
            if input.is_empty() {
                input_network
            } else {
                input
            }
        } else {
            input
        };
        self.set_busy(true)?;
        self.label("SettingsError", "正在连接并校验服务器…")?;
        self.label("NetworkError", "正在连接并校验服务器…")?;
        let manager = self.auth.manager();
        let server_path = self.auth.server_path();
        let events = self.sender();
        thread::spawn(move || {
            let result =
                auth::probe_and_save_server(&manager, &server_path, &effective).map(|_| ());
            events.send(Event::SaveServerFinished(result));
        });
        Ok(())
    }

    fn peer_advanced_visible(&self) -> bool {
        self.find_opt::<StackPanel>("PeerAdvancedRow")
            .and_then(|panel| panel.cast::<UIElement>().ok())
            .and_then(|element| element.Visibility().ok())
            .is_some_and(|v| v == Visibility::Visible)
    }

    fn start_login(&mut self) -> Result<()> {
        if self.logged_in {
            return Ok(());
        }
        if !self.auth.server_config().is_configured() {
            return self.label("LoginError", "请先点击「服务器设置」连接并保存服务器地址。");
        }
        let email = self.find::<TextBox>("Email")?.Text()?.to_string();
        let email = email.trim().to_string();
        if !email
            .split_once('@')
            .is_some_and(|(name, host)| !name.is_empty() && !host.is_empty())
        {
            return self.label("LoginError", "请输入有效的账号邮箱。");
        }
        let password = self
            .find::<PasswordBox>("Password")?
            .Password()?
            .to_string();
        if password.is_empty() {
            return self.label("LoginError", "请输入密码。");
        }
        self.set_busy(true)?;
        self.label("LoginError", "正在登录…")?;
        let manager = self.auth.manager();
        let server_path = self.auth.server_path();
        let events = self.sender();
        thread::spawn(move || {
            events.send(Event::LoginFinished(auth::login(
                &manager,
                &server_path,
                &email,
                &password,
            )));
        });
        Ok(())
    }

    fn do_logout(&mut self) -> Result<()> {
        self.auth.stop_maintainer();
        self.runtime.stop();
        self.set_busy(true)?;
        let manager = self.auth.manager();
        let events = self.sender();
        thread::spawn(move || {
            events.send(Event::LogoutFinished(auth::logout(&manager)));
        });
        Ok(())
    }

    /// 登录成功（或恢复）后的主壳装配。
    fn open_main(&mut self, account: &AccountSession) -> Result<()> {
        self.find::<PasswordBox>("Password")?.SetPassword("")?;
        self.label("LoginError", "")?;
        self.label("AccountEmail", &account.email)?;
        // 用登录响应携带的数据即时渲染首屏，REST 全量随后补齐。
        self.devices = account.devices.clone();
        if let Some(id) = account.device_id.clone() {
            self.label("LocalIdText", &server_config::format_device_id(&id))?;
        }
        self.logged_in = true;
        self.set_visible("LoginOverlay", false)?;
        self.set_visible("MainShell", true)?;
        self.apply_scale()?;
        self.load_prefs_into_ui()?;
        if let Err(err) = self.runtime.sync() {
            self.label("AssistError", &format!("信令启动失败：{err}"))?;
        }
        self.render_devices()?;
        self.render_history()?;
        self.page = Page::Devices;
        self.show_page(Page::Devices)?;
        self.on_click(Action::RefreshDevices)?;
        let events = self.sender();
        self.auth.start_maintainer(move |reason| {
            let _ = reason;
            // 强制重登：后台维护线程回调，直接停运行时并回登录页。
            events.send(Event::LogoutFinished(Err(String::new())));
        });
        Ok(())
    }

    fn return_to_login(&mut self, message: String) -> Result<()> {
        self.auth.stop_maintainer();
        self.runtime.stop();
        self.logged_in = false;
        self.devices.clear();
        self.online_override.clear();
        self.set_visible("MainShell", false)?;
        self.set_visible("LoginOverlay", true)?;
        self.set_visible("LoginView", true)?;
        self.set_visible("ServerSettingsView", false)?;
        self.label("LoginError", &message)?;
        self.set_busy(false)
    }

    fn apply_status(&mut self, snapshot: &ServerStatusSnapshot, generation: u64) -> Result<()> {
        if generation != self.runtime.generation() || !self.logged_in {
            return Ok(());
        }
        let online = snapshot.error_message.is_none() && snapshot.device_id.is_some();
        let dot: Ellipse = self.find("StatusDot")?;
        let color = if online {
            windows::UI::Color {
                A: 255,
                R: 0x22,
                G: 0xB5,
                B: 0x73,
            }
        } else {
            windows::UI::Color {
                A: 255,
                R: 0xB9,
                G: 0xC4,
                B: 0xD4,
            }
        };
        let brush = windows::UI::Xaml::Media::SolidColorBrush::new()?;
        brush.SetColor(color)?;
        let shape: windows::UI::Xaml::Shapes::Shape = dot.cast()?;
        shape.SetFill(&brush)?;
        let state_text = match snapshot.error_message.as_deref() {
            Some(message) => format!("信令：{message}"),
            None if online => "信令在线".to_string(),
            None => "信令连接中…".to_string(),
        };
        self.label("StatusText", &state_text)?;
        if let Some(id) = snapshot.device_id.as_deref() {
            self.label("LocalIdText", &server_config::format_device_id(id))?;
            if online && self.online_override.get(id) != Some(&true) {
                self.online_override.insert(id.to_string(), true);
                self.render_devices()?;
            }
            if self.auth.server_config().cached_device_id.as_deref() != Some(id) {
                let mut cfg = self.auth.server_config();
                cfg.cached_device_id = Some(id.to_string());
                if let Err(err) = server_config::save_to_path(&self.auth.server_path(), &cfg) {
                    self.label("AssistError", &format!("保存设备码失败：{err}"))?;
                }
                self.auth.manager().update_server_config(cfg);
            }
        }
        Ok(())
    }

    fn show_page(&mut self, page: Page) -> Result<()> {
        self.set_visible("DevicesPage", page == Page::Devices)?;
        self.set_visible("AssistPage", page == Page::Assist)?;
        self.set_visible("FavoritesPage", page == Page::Favorites)?;
        self.set_visible("SettingsPage", page == Page::Settings)?;
        self.set_visible("SessionPage", page == Page::Session)?;
        self.select_nav(page)?;
        if page == Page::Favorites {
            self.render_favorites()?;
        }
        if page == Page::Assist {
            self.render_history()?;
        }
        if page == Page::Settings {
            self.settings_tab_current()?;
        }
        Ok(())
    }

    fn select_nav(&self, page: Page) -> Result<()> {
        for (name, target) in [
            ("NavDevices", Page::Devices),
            ("NavAssist", Page::Assist),
            ("NavFavorites", Page::Favorites),
            ("NavSettings", Page::Settings),
            // Session 页无导航高亮。
        ] {
            if let Some(button) = self.find_opt::<ToggleButton>(name) {
                set_checked(&button, page == target)?;
            }
        }
        Ok(())
    }

    fn update_caption(&self) -> Result<()> {
        let origin = settings::format_origin(&self.auth.server_config());
        self.label("ServerCaption", &origin)?;
        self.label("ServerOriginCaption", &origin)?;
        Ok(())
    }

    fn settings_tab_current(&self) -> Result<()> {
        let origin = settings::format_origin(&self.auth.server_config());
        self.find::<TextBox>("NetworkOrigin")?
            .SetText(HSTRING::from(&origin))?;
        self.label("CurrentServerCaption", &format!("当前：{origin}"))
    }

    // ── 会话输入（M2：鼠标/滚轮/键盘；watch_only 不发送）──

    fn stage_size(&self) -> (f64, f64) {
        self.find_opt::<SwapChainPanel>("SessionPanel")
            .and_then(|panel| panel.cast::<FrameworkElement>().ok())
            .and_then(|element| {
                let w = element.ActualWidth().ok()?;
                let h = element.ActualHeight().ok()?;
                Some((w, h))
            })
            .unwrap_or((0.0, 0.0))
    }

    fn on_session_pointer(&mut self, event: SessionPointerEvent) -> Result<()> {
        // 直接字段访问：session 借用与 pressed_* 修改字段不相交。
        let Some(active) = self.session.as_ref() else {
            return Ok(());
        };
        if !matches!(self.session_view, SessionView::Negotiated { .. }) || active.watch_only {
            return Ok(());
        }
        let handle = &active.handle;
        let video = active.video;
        let stage = self.stage_size();
        if stage.0 <= 0.0 || stage.1 <= 0.0 {
            return Ok(());
        }
        match event {
            SessionPointerEvent::Move { x, y } => {
                let (nx, ny) = sess::normalize_in_video(x, y, stage.0, stage.1, video);
                handle.send_input(InputEvent::MouseMove { x: nx, y: ny });
            }
            SessionPointerEvent::Button {
                x,
                y,
                button,
                pressed,
            } => {
                let (nx, ny) = sess::normalize_in_video(x, y, stage.0, stage.1, video);
                handle.send_input(InputEvent::MouseMove { x: nx, y: ny });
                if pressed {
                    if !self.pressed_buttons.contains(&button) {
                        self.pressed_buttons.push(button);
                    }
                } else {
                    self.pressed_buttons.retain(|b| *b != button);
                }
                handle.send_input(InputEvent::MouseButton {
                    button: match button {
                        0 => rc_input::MouseButton::Left,
                        1 => rc_input::MouseButton::Right,
                        _ => rc_input::MouseButton::Middle,
                    },
                    pressed,
                });
            }
            SessionPointerEvent::Wheel { delta } => {
                handle.send_input(InputEvent::MouseWheel {
                    delta: sess::wheel_to_lines(delta as i16),
                    horizontal: false,
                });
            }
        }
        Ok(())
    }

    fn on_session_key(&mut self, event: SessionKeyEvent) -> Result<()> {
        let Some(active) = self.session.as_ref() else {
            return Ok(());
        };
        if !matches!(self.session_view, SessionView::Negotiated { .. }) || active.watch_only {
            return Ok(());
        }
        let handle = &active.handle;
        // 修饰键跟踪（供其余按键携带位标志）。
        let mod_bit = match event.key {
            VirtualKey::LeftControl | VirtualKey::RightControl => Some(0u8),
            VirtualKey::LeftShift | VirtualKey::RightShift => Some(1),
            VirtualKey::LeftMenu | VirtualKey::RightMenu => Some(2),
            VirtualKey::LeftWindows | VirtualKey::RightWindows => Some(3),
            _ => None,
        };
        if let Some(bit) = mod_bit {
            if event.pressed {
                self.pressed_modifiers |= 1 << bit;
            } else {
                self.pressed_modifiers &= !(1 << bit);
            }
        }
        let Some(code) = vkey_to_dom(event.key) else {
            return Ok(());
        };
        let Some(scancode) = rc_client_core::input_bridge::scancode_for(&code) else {
            return Ok(());
        };
        if event.pressed {
            if !self.pressed_keys.contains(&scancode) {
                self.pressed_keys.push(scancode);
            }
        } else {
            self.pressed_keys.retain(|k| *k != scancode);
        }
        handle.send_input(InputEvent::Key {
            scancode,
            pressed: event.pressed,
            modifiers: self.pressed_modifiers,
        });
        Ok(())
    }

    /// 焦点丢失 / 会话结束时补发全部已按下键与鼠标键的 released。
    fn release_pressed_input(&mut self) {
        let Some(active) = self.session.as_ref() else {
            return;
        };
        let modifiers = self.pressed_modifiers;
        for scancode in self.pressed_keys.drain(..) {
            active.handle.send_input(InputEvent::Key {
                scancode,
                pressed: false,
                modifiers,
            });
        }
        for button in self.pressed_buttons.drain(..) {
            active.handle.send_input(InputEvent::MouseButton {
                button: match button {
                    0 => rc_input::MouseButton::Left,
                    1 => rc_input::MouseButton::Right,
                    _ => rc_input::MouseButton::Middle,
                },
                pressed: false,
            });
        }
        self.pressed_modifiers = 0;
    }

    fn apply_settings_tab(&self, index: u8) -> Result<()> {
        for (name, on) in [
            ("TabGeneral", index == 0),
            ("TabSecurity", index == 1),
            ("TabNetwork", index == 2),
        ] {
            if let Some(tab) = self.find_opt::<ToggleButton>(name) {
                set_checked(&tab, on)?;
            }
        }
        self.set_visible("PanelGeneral", index == 0)?;
        self.set_visible("PanelSecurity", index == 1)?;
        self.set_visible("PanelNetwork", index == 2)?;
        if index == 2 {
            self.settings_tab_current()?;
        }
        Ok(())
    }

    fn load_prefs_into_ui(&mut self) -> Result<()> {
        let prefs = self.prefs.lock().unwrap().clone();
        let index = match prefs.bitrate_preset.as_str() {
            "auto" => 0,
            "8m" => 1,
            "14m" => 2,
            "30m" => 3,
            _ => 1,
        };
        if let Some(combo) = self.find_opt::<ComboBox>("QualityBox") {
            let selector: windows::UI::Xaml::Controls::Primitives::Selector = combo.cast()?;
            selector.SetSelectedIndex(index)?;
        }
        if let Some(toggle) = self.find_opt::<ToggleSwitch>("HudToggle") {
            toggle.SetIsOn(prefs.show_hud)?;
        }
        if let Some(toggle) = self.find_opt::<ToggleSwitch>("EncryptToggle") {
            toggle.SetIsOn(prefs.default_encrypt_connection)?;
        }
        if let Some(toggle) = self.find_opt::<ToggleSwitch>("LockOnExitToggle") {
            toggle.SetIsOn(prefs.lock_on_exit)?;
        }
        if let Some(toggle) = self.find_opt::<ToggleSwitch>("AssistToggle") {
            toggle.SetIsOn(true)?;
        }
        self.label(
            "VersionText",
            &format!("版本 {}", env!("CARGO_PKG_VERSION")),
        )?;
        Ok(())
    }

    fn save_prefs_patch(
        &mut self,
        apply: impl FnOnce(&mut AppPrefs) -> std::result::Result<(), String>,
    ) -> Result<()> {
        let mut prefs = self.prefs.lock().unwrap();
        if let Err(err) = apply(&mut prefs) {
            self.label("AssistError", &format!("保存设置失败：{err}"))?;
            return Ok(());
        }
        let path = app_prefs::default_prefs_path();
        if let Err(err) = app_prefs::save_to_path(&path, &prefs) {
            self.label("AssistError", &format!("写入设置失败：{err}"))?;
        }
        Ok(())
    }

    fn persist_history(&self) -> Result<()> {
        let path = history::default_history_path();
        let result = self.history.lock().unwrap().save_to(&path);
        if let Err(err) = result {
            self.label("AssistError", &err)?;
        }
        Ok(())
    }

    fn device_name_of(&self, device: &MyDeviceInfo) -> String {
        let alias = device.alias.trim();
        if !alias.is_empty() {
            alias.to_string()
        } else {
            let host = device.hostname.trim();
            if host.is_empty() {
                device.device_id.clone()
            } else {
                host.to_string()
            }
        }
    }

    fn render_devices(&mut self) -> Result<()> {
        let local_id = self.auth.server_config().cached_device_id.clone();
        let host: StackPanel = self.find("DeviceListHost")?;
        let children = panel_children(&host)?;
        children.Clear()?;
        let empty = self.devices.is_empty();
        self.set_visible("DevicesEmpty", empty)?;
        let rows = self.build_device_rows(&local_id)?;
        for row in rows {
            children.Append(&as_ui_element(&row)?)?;
        }
        Ok(())
    }

    fn build_device_rows(&self, local_id: &Option<String>) -> Result<Vec<FrameworkElement>> {
        let mut rows = Vec::new();
        for device in &self.devices {
            let is_local = local_id.as_deref() == Some(device.device_id.as_str());
            let name = self.device_name_of(device);
            let online = self
                .online_override
                .get(&device.device_id)
                .copied()
                .unwrap_or(device.online);
            let lan = self.lan_codes.contains(&device.device_id);
            let row = widgets::load_device_row()?;
            let name_text: TextBlock = row.FindName("RowName")?.cast()?;
            name_text.SetText(HSTRING::from(&name))?;
            let code_text: TextBlock = row.FindName("RowCode")?.cast()?;
            code_text.SetText(HSTRING::from(&server_config::format_device_id(
                &device.device_id,
            )))?;
            widgets::set_device_online(&row, online, lan, is_local)?;
            {
                let sender = self.sender();
                let button: ButtonBase = row.FindName("RowConnect")?.cast()?;
                let id = device.device_id.clone();
                button.Click(RoutedEventHandler::new(move |_, _| {
                    sender.send(Event::Click(Action::DeviceConnect(id.clone())));
                    Ok(())
                }))?;
            }
            {
                let sender = self.sender();
                let item = widgets::find_menu_item(&row, "RowFavItem")?;
                let favorite = self.history.lock().unwrap().is_favorite(&device.device_id);
                item.SetText(HSTRING::from(if favorite {
                    "取消收藏"
                } else {
                    "收藏"
                }))?;
                let id = device.device_id.clone();
                item.Click(RoutedEventHandler::new(move |_, _| {
                    sender.send(Event::Click(Action::DeviceToggleFav(id.clone())));
                    Ok(())
                }))?;
            }
            {
                let sender = self.sender();
                let item = widgets::find_menu_item(&row, "RowRenameItem")?;
                let id = device.device_id.clone();
                item.Click(RoutedEventHandler::new(move |_, _| {
                    sender.send(Event::Click(Action::DeviceRename(id.clone())));
                    Ok(())
                }))?;
            }
            {
                let sender = self.sender();
                let item = widgets::find_menu_item(&row, "RowUnbindItem")?;
                let id = device.device_id.clone();
                item.Click(RoutedEventHandler::new(move |_, _| {
                    sender.send(Event::Click(Action::DeviceUnbind(id.clone())));
                    Ok(())
                }))?;
            }
            rows.push(row);
        }
        Ok(rows)
    }

    fn render_history(&mut self) -> Result<()> {
        if let Some(recent_tab) = self.find_opt::<ToggleButton>("HistoryTabRecent") {
            set_checked(&recent_tab, !self.history_tab_fav)?;
        }
        if let Some(fav_tab) = self.find_opt::<ToggleButton>("HistoryTabFav") {
            set_checked(&fav_tab, self.history_tab_fav)?;
        }
        let host: StackPanel = self.find("HistoryListHost")?;
        let children = panel_children(&host)?;
        children.Clear()?;
        let snapshot = self.history.lock().unwrap().clone();
        if self.history_tab_fav {
            let favorites = snapshot.favorites().to_vec();
            self.set_visible("HistoryEmpty", favorites.is_empty())?;
            for device_id in favorites {
                let name = self
                    .devices
                    .iter()
                    .find(|d| d.device_id == device_id)
                    .map(|d| self.device_name_of(d))
                    .unwrap_or_else(|| "未知设备".to_string());
                let row = self.recent_row(&device_id, &name, "")?;
                children.Append(&as_ui_element(&row)?)?;
            }
        } else {
            let recents = snapshot.recents().to_vec();
            self.set_visible("HistoryEmpty", recents.is_empty())?;
            for record in recents {
                let row = self.recent_row(
                    &record.device_id,
                    &record.name,
                    &relative_time(record.last_connected_at_sec),
                )?;
                children.Append(&as_ui_element(&row)?)?;
            }
        }
        Ok(())
    }

    fn recent_row(&self, device_id: &str, name: &str, when: &str) -> Result<FrameworkElement> {
        let row = widgets::load_recent_row()?;
        let name_text: TextBlock = row.FindName("RowName")?.cast()?;
        name_text.SetText(HSTRING::from(name))?;
        let code_text: TextBlock = row.FindName("RowCode")?.cast()?;
        code_text.SetText(HSTRING::from(&server_config::format_device_id(device_id)))?;
        let when_text: TextBlock = row.FindName("RowWhen")?.cast()?;
        when_text.SetText(HSTRING::from(when))?;
        {
            let sender = self.sender();
            let button: ButtonBase = row.FindName("RowConnect")?.cast()?;
            let id = device_id.to_string();
            button.Click(RoutedEventHandler::new(move |_, _| {
                sender.send(Event::Click(Action::RecentConnect(id.clone())));
                Ok(())
            }))?;
        }
        {
            let sender = self.sender();
            let button: ButtonBase = row.FindName("RowDelete")?.cast()?;
            let id = device_id.to_string();
            button.Click(RoutedEventHandler::new(move |_, _| {
                sender.send(Event::Click(Action::RecentDelete(id.clone())));
                Ok(())
            }))?;
        }
        Ok(row)
    }

    fn render_favorites(&mut self) -> Result<()> {
        let host: StackPanel = self.find("FavoritesListHost")?;
        let children = panel_children(&host)?;
        children.Clear()?;
        let snapshot = self.history.lock().unwrap().clone();
        let favorites = snapshot.favorites().to_vec();
        self.set_visible("FavoritesEmpty", favorites.is_empty())?;
        for device_id in favorites {
            let name = self
                .devices
                .iter()
                .find(|d| d.device_id == device_id)
                .map(|d| self.device_name_of(d))
                .unwrap_or_else(|| "未知设备".to_string());
            let row = widgets::load_device_row()?;
            let name_text: TextBlock = row.FindName("RowName")?.cast()?;
            name_text.SetText(HSTRING::from(&name))?;
            let code_text: TextBlock = row.FindName("RowCode")?.cast()?;
            code_text.SetText(HSTRING::from(&server_config::format_device_id(&device_id)))?;
            widgets::set_device_online(&row, false, false, false)?;
            {
                let sender = self.sender();
                let button: ButtonBase = row.FindName("RowConnect")?.cast()?;
                let id = device_id.clone();
                button.Click(RoutedEventHandler::new(move |_, _| {
                    sender.send(Event::Click(Action::DeviceConnect(id.clone())));
                    Ok(())
                }))?;
            }
            {
                let sender = self.sender();
                let item = widgets::find_menu_item(&row, "RowFavItem")?;
                let id = device_id.clone();
                item.Click(RoutedEventHandler::new(move |_, _| {
                    sender.send(Event::Click(Action::DeviceToggleFav(id.clone())));
                    Ok(())
                }))?;
            }
            if let Ok(rename_item) = widgets::find_menu_item(&row, "RowRenameItem") {
                as_ui_element(&rename_item)?.SetVisibility(Visibility::Collapsed)?;
            }
            if let Ok(unbind_item) = widgets::find_menu_item(&row, "RowUnbindItem") {
                as_ui_element(&unbind_item)?.SetVisibility(Visibility::Collapsed)?;
            }
            children.Append(&as_ui_element(&row)?)?;
        }
        Ok(())
    }

    /// 根布局等比缩放：DPI × 小屏适配系数，布局坐标系始终是 1000×660。
    fn apply_scale(&mut self) -> Result<()> {
        let dpi = unsafe { GetDpiForWindow(self.hwnd) };
        self.last_dpi = dpi;
        let fit = work_area_fit(self.hwnd);
        let scale = dpi as f64 / 96.0 * fit;
        let Some(grid) = self.find_opt::<windows::UI::Xaml::Controls::Grid>("RootLayout") else {
            return Ok(());
        };
        let scale_transform = ScaleTransform::new()?;
        scale_transform.SetScaleX(scale)?;
        scale_transform.SetScaleY(scale)?;
        let group = TransformGroup::new()?;
        let as_transform: Transform = scale_transform.cast()?;
        group.Children()?.Append(&as_transform)?;
        let as_group_transform: Transform = group.cast()?;
        let as_element: UIElement = grid.cast()?;
        as_element.SetRenderTransform(&as_group_transform)?;
        Ok(())
    }
}

/// 工作区小于设计基准时的等比缩小系数（≤1.0）。
fn work_area_fit(hwnd: windows::Win32::Foundation::HWND) -> f64 {
    let Some((left, top, right, bottom)) = shell::work_area(hwnd) else {
        return 1.0;
    };
    let work_w = (right - left) as f64;
    let work_h = (bottom - top) as f64;
    (work_w / DESIGN_WIDTH).min(work_h / DESIGN_HEIGHT).min(1.0)
}

fn install_styles() -> Result<()> {
    let dict = XamlReader::Load(include_str!("styles.xaml"))?
        .cast::<windows::UI::Xaml::ResourceDictionary>()?;
    let app = Application::Current()?;
    app.Resources()?.MergedDictionaries()?.Append(&dict)?;
    Ok(())
}

/// 仅保留数字（9 位码输入归一化）。
fn digit_of(text: &str) -> String {
    text.chars().filter(|c| c.is_ascii_digit()).collect()
}

fn copy_to_clipboard(text: &str) {
    use windows::ApplicationModel::DataTransfer::{Clipboard, DataPackage};
    let package = DataPackage::new().expect("创建剪贴板数据包");
    package
        .SetText(HSTRING::from(text))
        .expect("填充剪贴板文本");
    let _ = Clipboard::SetContent(&package);
}

fn relative_time(at_sec: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(at_sec);
    let delta = now.saturating_sub(at_sec);
    match delta {
        0..=59 => "刚刚".to_string(),
        60..=3599 => format!("{} 分钟前", delta / 60),
        3600..=86399 => format!("{} 小时前", delta / 3600),
        _ => format!("{} 天前", delta / 86400),
    }
}

/// 输入对话框占位（M1 收尾实现自绘 ContentDialog）。
fn prompt_text(_title: &str) -> Option<String> {
    None
}

/// Windows `VirtualKey` → DOM `code` 字符串（喂给 core 的 Set-1 表）。
fn vkey_to_dom(key: VirtualKey) -> Option<String> {
    use VirtualKey as V;
    let code = match key {
        V::A => "KeyA",
        V::B => "KeyB",
        V::C => "KeyC",
        V::D => "KeyD",
        V::E => "KeyE",
        V::F => "KeyF",
        V::G => "KeyG",
        V::H => "KeyH",
        V::I => "KeyI",
        V::J => "KeyJ",
        V::K => "KeyK",
        V::L => "KeyL",
        V::M => "KeyM",
        V::N => "KeyN",
        V::O => "KeyO",
        V::P => "KeyP",
        V::Q => "KeyQ",
        V::R => "KeyR",
        V::S => "KeyS",
        V::T => "KeyT",
        V::U => "KeyU",
        V::V => "KeyV",
        V::W => "KeyW",
        V::X => "KeyX",
        V::Y => "KeyY",
        V::Z => "KeyZ",
        V::Number0 => "Digit0",
        V::Number1 => "Digit1",
        V::Number2 => "Digit2",
        V::Number3 => "Digit3",
        V::Number4 => "Digit4",
        V::Number5 => "Digit5",
        V::Number6 => "Digit6",
        V::Number7 => "Digit7",
        V::Number8 => "Digit8",
        V::Number9 => "Digit9",
        V::Space => "Space",
        V::Enter => "Enter",
        V::Escape => "Escape",
        V::Back => "Backspace",
        V::Tab => "Tab",
        V::Delete => "Delete",
        V::Home => "Home",
        V::End => "End",
        V::PageUp => "PageUp",
        V::PageDown => "PageDown",
        V::Up => "ArrowUp",
        V::Down => "ArrowDown",
        V::Left => "ArrowLeft",
        V::Right => "ArrowRight",
        _ => return None,
    };
    Some(code.to_string())
}

/// StackPanel.Children 经 Panel 接口访问（0.37 投影返回 UIElementCollection）。
fn panel_children(panel: &StackPanel) -> Result<windows::UI::Xaml::Controls::UIElementCollection> {
    let as_panel: windows::UI::Xaml::Controls::Panel = panel.cast()?;
    as_panel.Children()
}

/// 0.37 投影下把任意元素降为 UIElement 便于集合追加。
fn as_ui_element<T: Interface>(element: &T) -> Result<UIElement> {
    element.cast()
}

/// ToggleButton.IsChecked 需要 IReference<bool>，0.37 投影经 PropertyValue 包装。
fn set_checked(toggle: &ToggleButton, on: bool) -> Result<()> {
    let value = windows::Foundation::PropertyValue::CreateBoolean(on)?;
    let reference: windows::Foundation::IReference<bool> = value.cast()?;
    toggle.SetIsChecked(&reference)
}

#[cfg(test)]
mod parse_smoke_tests {
    use super::*;

    #[test]
    #[ignore] // 实机诊断用：cargo test -p rc-ui parse_smoke -- --ignored
    fn parse_smoke() {
        let shell_init = unsafe {
            windows::Win32::System::WinRT::RoInitialize(
                windows::Win32::System::WinRT::RO_INIT_SINGLETHREADED,
            )
        };
        let _ = shell_init;
        let manager = windows::UI::Xaml::Hosting::WindowsXamlManager::InitializeForCurrentThread();
        println!("islands init: {:?}", manager.as_ref().map(|_| "ok"));
        println!(
            "styles parse: {:?}",
            XamlReader::Load(include_str!("styles.xaml"))
                .map(|_| "ok".to_string())
                .map_err(|e| e.to_string())
        );
        if let Ok(app) = Application::Current() {
            let parsed = XamlReader::Load(include_str!("styles.xaml"));
            println!("styles dict parse ok for merge");
            if let Ok(dict) = parsed.and_then(|d| d.cast::<windows::UI::Xaml::ResourceDictionary>())
            {
                let merged = app.Resources().and_then(|r| r.MergedDictionaries());
                match merged {
                    Ok(v) => println!("merge: {:?}", v.Append(dict).map(|_| "ok".to_string())),
                    Err(e) => println!("merged-dicts err: {e}"),
                }
            }
        }
        let view = XamlReader::Load(include_str!("view.xaml"));
        println!(
            "view parse: {:?}",
            view.as_ref()
                .map(|_| "ok".to_string())
                .map_err(|e| e.to_string())
        );
        let row = XamlReader::Load(crate::widgets::DEVICE_ROW_XAML_STR);
        println!(
            "row[device]: {:?}",
            row.as_ref()
                .map(|_| "ok".to_string())
                .map_err(|e| e.to_string())
        );
        let recent = XamlReader::Load(crate::widgets::RECENT_ROW_XAML_STR);
        println!(
            "row[recent]: {:?}",
            recent
                .as_ref()
                .map(|_| "ok".to_string())
                .map_err(|e| e.to_string())
        );
        const NS: &str = "xmlns=\"http://schemas.microsoft.com/winfx/2006/xaml/presentation\"";
        let variants: Vec<(&str, String)> = vec![
            ("b-rowlocal", format!("<Border {NS} x:Name=\"RowLocal\" Background=\"#EDF2FF\" CornerRadius=\"4\" Padding=\"6,1\" Visibility=\"Collapsed\"/>")),
            ("b-plain", format!("<Border {NS} Background=\"#EDF2FF\"/>")),
            ("t-benji", format!("<TextBlock {NS} Text=\"本机\"/>")),
            ("e-ge7f4", format!("<FontIcon {NS} Glyph=\"&#xE7F4;\"/>")),
            ("e-ge712", format!("<FontIcon {NS} Glyph=\"&#xE712;\"/>")),
            ("e-ge8c8", format!("<FontIcon {NS} Glyph=\"&#xE8C8;\"/>")),
            ("e-e74d", format!("<FontIcon {NS} Glyph=\"&#xE74D;\"/>")),
            ("flyout", format!("<Button {NS} Content=\"&#xE712;\"><Button.Flyout><MenuFlyout><MenuFlyoutItem x:Name=\"A\" Text=\"x\"/></MenuFlyout></Button.Flyout></Button>")),
            ("sp-rows", format!("<StackPanel {NS} Spacing=\"8\"/>")),
        ];
        for (name, text) in variants {
            let result = XamlReader::Load(text.as_str());
            println!(
                "min[{name}]: {:?}",
                result
                    .as_ref()
                    .map(|_| "ok".to_string())
                    .map_err(|e| e.to_string())
            );
        }
    }
}
