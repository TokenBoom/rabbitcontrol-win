//! 应用装配核心：App 结构、事件循环、动作调度与全局辅助。

//! UI 线程应用壳：路由、页面装配与事件分发。
//!
//! 线程模型：仅向 UI 线程传递纯值消息（XAML 控件始终停留在 STA 线程
//! 内）；后台线程经 [`Events`] 回投 [`Event`]，由 50ms 定时器与显式通知
//! 共同驱动 `dispatch` 排空。

use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;

use rc_client_core::app_prefs::{self, AppPrefs};
use rc_client_core::auth_client::MyDeviceInfo;
use rc_client_core::server_config::{self};
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
        RoutedEventHandler, UIElement, Visibility,
    },
};

use rc_client_core::session_bridge::{SessionHandle, SessionUpdate};
use windows::UI::Xaml::Controls::SwapChainPanel;
use windows::UI::Xaml::Input::PointerEventHandler;

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
    session_hud: Option<rc_client_core::hud::HudSnapshot>,
    quality: sess::QualityState,
    quality_fps_options: Vec<u32>,
    quality_sig: String,
    /// M3.7 自动重连：待触发的 tick / 目标设备 / 已尝试次数。
    pending_reconnect_tick: Option<u64>,
    reconnect_target: Option<String>,
    reconnect_attempts: u32,
    /// 用户主动断开 → 不自动重连。
    manual_disconnect: bool,
    /// M3.6 独立会话窗口宿主。
    session_host: Option<crate::ui::session_window::SessionWindow>,
    /// 会话窗口是否处于全屏（Esc 退出用）。
    fullscreen: bool,
    peer_watch_only: bool,
    pressed_keys: Vec<u16>,
    pressed_buttons: Vec<u8>,
    pressed_modifiers: u8,
}

mod actions;
mod pages;

pub(crate) use actions::{Action, Event, Events, Page};
use actions::{SessionKeyEvent, SessionPointerEvent};

use crate::biz::{
    auth::{self, AuthRuntime},
    history::{self, TargetHistory},
    runtime::SignalRuntime,
    session::{self as sess, SessionView},
    settings,
};
use crate::ui::{shell, Shell, DESIGN_HEIGHT, DESIGN_WIDTH};

impl App {
    pub fn new(shell: &Shell) -> std::result::Result<Self, Box<dyn std::error::Error>> {
        install_styles()?;
        let root = XamlReader::Load(include_str!("../ui/assets/view.xaml"))?.cast()?;
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
            session_hud: None,
            quality: sess::QualityState::default(),
            quality_fps_options: Vec::new(),
            quality_sig: String::new(),
            pending_reconnect_tick: None,
            reconnect_target: None,
            reconnect_attempts: 0,
            manual_disconnect: false,
            session_host: None,
            fullscreen: false,
            peer_watch_only: false,
            pressed_keys: Vec::new(),
            pressed_buttons: Vec::new(),
            pressed_modifiers: 0,
        };
        eprintln!("[App::new] pre-wire");
        app.wire().map_err(|e| {
            eprintln!("[App::new] wire FAIL: {e}");
            e
        })?;
        eprintln!("[App::new] wire OK");
        app.apply_brand();
        crate::ui::session_window::set_main_hwnd(shell.hwnd());
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
        eprintln!("[App::new] pre-set_content");
        shell.set_content(&app.root).map_err(|e| {
            eprintln!("[App::new] set_content FAIL: {e}");
            e
        })?;
        eprintln!("[App::new] set_content OK");
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
        Ok(())
    }

    /// M3.6：会话窗口创建后的专属接线（元素全部位于会话窗口根；
    /// wire() 在 App::new 时执行，彼时会话窗口尚不存在，故独立于此绑定）。
    fn wire_session_window(&mut self) -> Result<()> {
        let hwnd = self
            .session_host
            .as_ref()
            .map(|h| h.hwnd())
            .ok_or_else(windows::core::Error::from_win32)?;
        // 会话工具条（元素位于会话窗口根，find() 自动路由）
        self.click("SessionDisconnect", Action::SessionDisconnect)?;
        self.click("SessionReconnect", Action::SessionReconnect)?;
        self.click("SessionQuality", Action::SessionQualityToggle)?;
        self.click("SessionFullscreen", Action::SessionFullscreen)?;
        // M3.2 画质工具条：任一下拉变更 → 统一预钳制后下发
        for name in [
            "StreamResolutionBox",
            "StreamFpsBox",
            "StreamBitrateBox",
            "StreamCodecBox",
        ] {
            let sender = self.sender();
            let combo: windows::UI::Xaml::Controls::Primitives::Selector = self.find(name)?;
            combo.SelectionChanged(
                windows::UI::Xaml::Controls::SelectionChangedEventHandler::new(move |_, _| {
                    let sender = sender.clone();
                    sender.send(Event::Click(Action::StreamQualityChanged));
                    Ok(())
                }),
            )?;
        }
        // 会话舞台尺寸变化 → StageRect（渲染器 letterbox 基准，物理像素）
        {
            let sender = self.sender();
            let panel: SwapChainPanel = self.find("SessionPanel")?;
            let panel_fe: windows::UI::Xaml::FrameworkElement = panel.cast()?;
            panel_fe.SizeChanged(windows::UI::Xaml::SizeChangedEventHandler::new(
                move |_, args| {
                    if let Some(args) = args {
                        let scale = unsafe { GetDpiForWindow(hwnd) } as f64 / 96.0;
                        let size = args.NewSize()?;
                        let w = (size.Width as f64 * scale).round().max(1.0) as u32;
                        let h = (size.Height as f64 * scale).round().max(1.0) as u32;
                        let sender = sender.clone();
                        sender.send(Event::Click(Action::StageRectChanged(w, h)));
                    }
                    Ok(())
                },
            ))?;
        }
        // M3.3 鼠标模式下拉
        {
            let sender = self.sender();
            let combo: windows::UI::Xaml::Controls::Primitives::Selector =
                self.find("StreamMouseBox")?;
            combo.SelectionChanged(
                windows::UI::Xaml::Controls::SelectionChangedEventHandler::new(move |_, _| {
                    let sender = sender.clone();
                    sender.send(Event::Click(Action::MouseModeChanged));
                    Ok(())
                }),
            )?;
        }
        // M3.8 显示器下拉
        {
            let sender = self.sender();
            let combo: windows::UI::Xaml::Controls::Primitives::Selector =
                self.find("StreamDisplayBox")?;
            combo.SelectionChanged(
                windows::UI::Xaml::Controls::SelectionChangedEventHandler::new(move |_, _| {
                    let sender = sender.clone();
                    sender.send(Event::Click(Action::DisplayChanged));
                    Ok(())
                }),
            )?;
        }
        // 会话舞台输入（M2：鼠标 + 滚轮 + 键盘；watch_only 时不发送）
        {
            let sender = self.sender();
            let panel: SwapChainPanel = self.find("SessionPanel")?;
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
            let stage: UIElement = self.find("SessionStage")?;
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

    fn find_opt<T: Interface>(&self, name: &str) -> Option<T> {
        self.find_in(&self.root, name).or_else(|| {
            self.session_host
                .as_ref()
                .and_then(|host| self.find_in(&host.root, name))
        })
    }

    /// M3.6：主窗口与独立会话窗口双根查找（会话 UI 元素迁入会话窗口）。
    fn find<T: Interface>(&self, name: &str) -> Result<T> {
        if let Some(found) = self.find_in(&self.root, name) {
            return Ok(found);
        }
        if let Some(host) = &self.session_host {
            if let Some(found) = self.find_in(&host.root, name) {
                return Ok(found);
            }
        }
        // 未找到：走原路径返回原始错误（保持既有错误语义）
        let element = self.root.FindName(name)?;
        element.cast()
    }

    fn find_in<T: Interface>(&self, root: &FrameworkElement, name: &str) -> Option<T> {
        root.FindName(name).ok().and_then(|e| e.cast().ok())
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
        if let Err(e) = self.dispatch_inner() {
            eprintln!("[dispatch] non-fatal error (continuing): {}", e);
        }
        Ok(())
    }

    fn dispatch_inner(&mut self) -> Result<()> {
        // DPI 变化：window_proc 已重设窗口尺寸，这里同步根布局缩放。
        let dpi = unsafe { GetDpiForWindow(self.hwnd) };
        if dpi != self.last_dpi {
            self.last_dpi = dpi;
            self.apply_scale()?;
        }
        while let Ok(event) = self.inbox.try_recv() {
            eprintln!("[dispatch] processing event...");
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
                            self.ensure_session_window(&target_name)?;
                            self.session = Some(ActiveSession {
                                handle,
                                video: None,
                                watch_only,
                                target_name,
                                target_id,
                            });
                            self.session_view = SessionView::Idle;
                            self.session_hud = None;
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
        // M3.6：会话窗口关闭确认 → 释放输入 + 断开 + 销毁窗口
        if crate::ui::session_window::take_close_request() {
            self.release_pressed_input();
            self.manual_disconnect = true;
            self.pending_reconnect_tick = None;
            if let Some(active) = self.session.as_ref() {
                sess::disconnect(&active.handle);
            }
            if let Some(host) = self.session_host.take() {
                host.close();
            }
        }
        if !self.logged_in {
            return Ok(());
        }
        eprintln!("[on_tick] logged_in");
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
        let mut hud_dirty = false;
        if let Some(active) = self.session.as_ref() {
            session_running = active.handle.is_running();
            while let Some(update) = active.handle.try_recv_update() {
                session_updates.push(update);
            }
        }
        for update in session_updates {
            if let SessionUpdate::Hud(snap) = &update {
                self.session_hud = Some(snap.clone());
                hud_dirty = true;
            }
            if let SessionUpdate::Negotiated { width, height, .. } = &update {
                if let Some(active) = self.session.as_mut() {
                    active.video = Some((*width, *height));
                }
            }
            // M3.2：被控端显示能力（首次入流与每次热切换后回传）→ 画质工具条
            if let SessionUpdate::DisplayConfig {
                width,
                height,
                fps,
                max_fps,
                presets,
                codec,
                supported_codecs,
                client_supports_hevc,
                ..
            } = &update
            {
                self.quality = sess::QualityState {
                    presets: presets
                        .iter()
                        .map(|p| (p.width, p.height, p.label.clone()))
                        .collect(),
                    width: *width,
                    height: *height,
                    fps: *fps,
                    max_fps: *max_fps,
                    codec: codec.clone(),
                    supported_codecs: supported_codecs.clone(),
                    client_supports_hevc: *client_supports_hevc,
                    // DisplayConfig 不携带鼠标模式/显示器列表，保留现值
                    mouse_mode: self.quality.mouse_mode.clone(),
                    displays: self.quality.displays.clone(),
                    active_display_id: self.quality.active_display_id,
                };
                self.quality_fps_options = sess::fps_candidates(*max_fps);
                self.sync_quality_bar()?;
            }
            // M3.3：鼠标模式回传 → 工具条选中态
            if let SessionUpdate::MouseModeState { mode } = &update {
                self.quality.mouse_mode = Some(mode.clone());
                self.sync_quality_bar()?;
            }
            // M3.8：显示器列表回传 → 下拉重建
            if let SessionUpdate::DisplaysState {
                displays,
                active_display_id,
            } = &update
            {
                self.quality.displays = displays
                    .iter()
                    .map(|d| {
                        let tag = if d.is_virtual {
                            "（虚拟）"
                        } else if d.is_main {
                            "（主）"
                        } else {
                            ""
                        };
                        (d.id, format!("{} {}", d.name, tag))
                    })
                    .collect();
                self.quality.active_display_id = *active_display_id;
                self.sync_quality_bar()?;
            }
            if matches!(update, SessionUpdate::Ended { .. }) {
                session_running = false;
            }
            // M3.7：远端切换桌面 → 短延迟自动重连（不计入次数）
            if let SessionUpdate::SessionSwitching { reason } = &update {
                self.reconnect_target = self.session.as_ref().map(|s| s.target_id.clone());
                self.pending_reconnect_tick = Some(self.tick + 40); // 2s
                self.set_session_overlay(&format!("{reason}，自动重连中…"), true);
            }
            // M3.7：异常断开（非用户主动）→ 最多 3 次自动重连，3s 间隔
            if let SessionUpdate::Ended { reason } = &update {
                if !self.manual_disconnect && self.reconnect_attempts < 3 {
                    self.reconnect_target = self.session.as_ref().map(|s| s.target_id.clone());
                    self.pending_reconnect_tick = Some(self.tick + 60); // 3s
                    self.set_session_overlay(
                        &format!(
                            "连接已断开（{reason}），3 秒后自动重连（第 {}/3 次）…",
                            self.reconnect_attempts + 1
                        ),
                        true,
                    );
                }
            }
            // 协商成功 → 重置重连计数
            if matches!(
                update,
                SessionUpdate::Negotiated { .. } | SessionUpdate::DisplayConfig { .. }
            ) {
                self.reconnect_attempts = 0;
            }
            self.session_view = sess::view_state_of(&update, &self.session_view);
        }

        // M3.7：到点触发自动重连（会话已结束时启动新连接）。
        if let (Some(at), Some(target)) =
            (self.pending_reconnect_tick, self.reconnect_target.clone())
        {
            if self.tick >= at && self.session.as_ref().map(|s| s.handle.is_running()) != Some(true)
            {
                self.pending_reconnect_tick = None;
                self.reconnect_attempts += 1;
                self.manual_disconnect = false;
                self.start_session(&target)?;
            }
        }
        if self.session.is_some() {
            self.apply_session_view();
            if hud_dirty {
                self.apply_hud();
            }
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
                self.manual_disconnect = true;
                self.pending_reconnect_tick = None;
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
            Action::DisplayChanged => {
                self.on_display_changed()?;
                Ok(())
            }
            Action::SessionFullscreen => {
                if let Some(host) = self.session_host.as_ref() {
                    self.fullscreen = host.toggle_fullscreen();
                }
                Ok(())
            }
            Action::SessionQualityToggle => {
                let visible = self
                    .find_opt::<UIElement>("QualityBar")
                    .and_then(|e| e.Visibility().ok())
                    .map(|v| v != Visibility::Visible)
                    .unwrap_or(false);
                self.set_visible("QualityBar", visible)
            }
            Action::StreamQualityChanged => {
                self.on_quality_changed()?;
                Ok(())
            }
            Action::MouseModeChanged => {
                self.on_mouse_mode_changed()?;
                Ok(())
            }
            Action::StageRectChanged(width, height) => {
                if let Some(active) = self.session.as_ref() {
                    active
                        .handle
                        .send(rc_client_core::session_bridge::SessionCommand::StageRect {
                            width,
                            height,
                        });
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

    fn peer_advanced_visible(&self) -> bool {
        self.find_opt::<StackPanel>("PeerAdvancedRow")
            .and_then(|panel| panel.cast::<UIElement>().ok())
            .and_then(|element| element.Visibility().ok())
            .is_some_and(|v| v == Visibility::Visible)
    }

    /// 标题栏 + 登录页使用仓库 `app.ico` 的真实 logo（解码失败时保留占位）。
    fn apply_brand(&self) {
        let Some(bitmap) = crate::ui::brand::icon_bitmap() else {
            return;
        };
        let Ok(source) = bitmap.cast::<windows::UI::Xaml::Media::ImageSource>() else {
            return;
        };
        for name in ["TitleLogo", "BrandLogo"] {
            if let Some(img) = self.find_opt::<windows::UI::Xaml::Controls::Image>(name) {
                let _ = img.SetSource(&source);
            }
        }
    }

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

pub(crate) fn set_checked(toggle: &ToggleButton, on: bool) -> Result<()> {
    let value = windows::Foundation::PropertyValue::CreateBoolean(on)?;
    let reference: windows::Foundation::IReference<bool> = value.cast()?;
    toggle.SetIsChecked(&reference)
}

fn panel_children(panel: &StackPanel) -> Result<windows::UI::Xaml::Controls::UIElementCollection> {
    let as_panel: windows::UI::Xaml::Controls::Panel = panel.cast()?;
    as_panel.Children()
}

fn as_ui_element<T: Interface>(element: &T) -> Result<UIElement> {
    element.cast()
}

fn work_area_fit(hwnd: windows::Win32::Foundation::HWND) -> f64 {
    let Some((left, top, right, bottom)) = shell::work_area(hwnd) else {
        return 1.0;
    };
    let work_w = (right - left) as f64;
    let work_h = (bottom - top) as f64;
    (work_w / DESIGN_WIDTH).min(work_h / DESIGN_HEIGHT).min(1.0)
}

fn install_styles() -> Result<()> {
    let dict = XamlReader::Load(include_str!("../ui/assets/styles.xaml"))
        .map_err(|e| {
            windows::core::Error::new(e.code(), HSTRING::from(format!("styles parse: {e}")))
        })?
        .cast::<windows::UI::Xaml::ResourceDictionary>()
        .map_err(|e| {
            windows::core::Error::new(e.code(), HSTRING::from(format!("styles cast: {e}")))
        })?;
    let app = Application::Current().map_err(|e| {
        windows::core::Error::new(e.code(), HSTRING::from(format!("App::Current: {e}")))
    })?;
    let res = app.Resources().map_err(|e| {
        windows::core::Error::new(e.code(), HSTRING::from(format!("Resources: {e}")))
    })?;
    let merged = res.MergedDictionaries().map_err(|e| {
        windows::core::Error::new(e.code(), HSTRING::from(format!("MergedDicts: {e}")))
    })?;
    merged
        .Append(&dict)
        .map_err(|e| windows::core::Error::new(e.code(), HSTRING::from(format!("Append: {e}"))))?;
    Ok(())
}

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

fn prompt_text(_title: &str) -> Option<String> {
    None
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
            XamlReader::Load(include_str!("../ui/assets/styles.xaml"))
                .map(|_| "ok".to_string())
                .map_err(|e| e.to_string())
        );
        if let Ok(app) = Application::Current() {
            let parsed = XamlReader::Load(include_str!("../ui/assets/styles.xaml"));
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
        let view = XamlReader::Load(include_str!("../ui/assets/view.xaml"));
        println!(
            "view parse: {:?}",
            view.as_ref()
                .map(|_| "ok".to_string())
                .map_err(|e| e.to_string())
        );
        let row = XamlReader::Load(crate::ui::widgets::DEVICE_ROW_XAML_STR);
        println!(
            "row[device]: {:?}",
            row.as_ref()
                .map(|_| "ok".to_string())
                .map_err(|e| e.to_string())
        );
        let recent = XamlReader::Load(crate::ui::widgets::RECENT_ROW_XAML_STR);
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
