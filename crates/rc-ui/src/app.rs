use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use rc_client_core::auth_client::MyDeviceInfo;
use rc_client_core::server_config;
use windows::{
    core::{Interface, Result},
    System::VirtualKey,
    Win32::{Foundation::HWND, UI::WindowsAndMessaging::PostMessageW},
    UI::Xaml::{
        Controls::Primitives::ButtonBase,
        Controls::{Control, PasswordBox, SwapChainPanel, TextBlock, TextBox},
        FrameworkElement,
        Input::KeyEventHandler,
        Markup::XamlReader,
        RoutedEventHandler, UIElement, Visibility,
    },
};

use crate::{
    auth::{self, AccountSession, AuthRuntime},
    desktop,
    graphics::Surface,
    settings,
    shell::{Shell, UI_EVENT},
};

#[derive(Clone, Copy)]
enum Action {
    Login,
    Settings,
    Back,
    Save,
    Logout,
    RefreshDevices,
}

enum Event {
    Click(Action),
    SaveServerFinished(std::result::Result<server_config::ServerConfig, String>),
    RestoreFinished(std::result::Result<Option<AccountSession>, String>),
    LoginFinished(std::result::Result<AccountSession, String>),
    LogoutFinished(std::result::Result<(), String>),
    DevicesFinished(std::result::Result<Vec<MyDeviceInfo>, String>),
    ReloginRequired(String),
}

// 仅向 UI 线程传递纯值消息，XAML 控件与交换链始终停留在 STA 线程内。
#[derive(Clone)]
struct Events {
    tx: Sender<Event>,
    hwnd: HWND,
}

impl Events {
    fn send(&self, event: Event) {
        if self.tx.send(event).is_ok() {
            unsafe {
                PostMessageW(self.hwnd, UI_EVENT, None, None);
            }
        }
    }
}

pub struct App {
    root: FrameworkElement,
    events: Events,
    inbox: Receiver<Event>,
    auth: AuthRuntime,
    busy: bool,
    logged_in: bool,
    surface: Option<Surface>,
    width: u32,
    height: u32,
}

impl App {
    pub fn new(shell: &Shell) -> std::result::Result<Self, Box<dyn std::error::Error>> {
        let root = XamlReader::Load(include_str!("view.xaml"))?.cast()?;
        let (tx, inbox) = mpsc::channel();
        let resolution = desktop::initial_resolution();
        let auth = AuthRuntime::new().map_err(std::io::Error::other)?;
        let mut app = Self {
            root,
            events: Events {
                tx,
                hwnd: shell.hwnd(),
            },
            inbox,
            auth,
            busy: false,
            logged_in: false,
            surface: None,
            width: resolution.width,
            height: resolution.height,
        };
        for (name, action) in [
            ("SignIn", Action::Login),
            ("SignOut", Action::Logout),
            ("Settings", Action::Settings),
            ("Back", Action::Back),
            ("Save", Action::Save),
            ("RefreshDevices", Action::RefreshDevices),
        ] {
            let events = app.events.clone();
            app.find::<ButtonBase>(name)?
                .Click(RoutedEventHandler::new(move |_, _| {
                    events.send(Event::Click(action));
                    Ok(())
                }))?;
        }
        for (name, action) in [("Password", Action::Login), ("Origin", Action::Save)] {
            let events = app.events.clone();
            app.find::<UIElement>(name)?
                .KeyDown(KeyEventHandler::new(move |_, args| {
                    if let Some(args) = args {
                        if args.Key()? == VirtualKey::Enter {
                            args.SetHandled(true)?;
                            events.send(Event::Click(action));
                        }
                    }
                    Ok(())
                }))?;
        }
        app.update_caption()?;
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

    fn find<T: Interface>(&self, name: &str) -> Result<T> {
        self.root.FindName(name)?.cast()
    }

    fn label(&self, name: &str, value: &str) -> Result<()> {
        self.find::<TextBlock>(name)?.SetText(value)
    }

    fn show(&self, page: &str) -> Result<()> {
        for name in ["LoginPage", "SettingsPage", "DesktopPage"] {
            self.find::<UIElement>(name)?
                .SetVisibility(if name == page {
                    Visibility::Visible
                } else {
                    Visibility::Collapsed
                })?;
        }
        Ok(())
    }

    fn update_caption(&self) -> Result<()> {
        let origin = settings::format_origin(&self.auth.server_config());
        self.label("DesktopServer", &origin)?;
        self.label("ServerCaption", &origin)
    }

    fn set_busy(&mut self, busy: bool) -> Result<()> {
        self.busy = busy;
        for name in [
            "SignIn",
            "SignOut",
            "Settings",
            "Email",
            "Password",
            "Origin",
            "Save",
            "Back",
            "RefreshDevices",
        ] {
            self.find::<Control>(name)?.SetIsEnabled(!busy)?;
        }
        Ok(())
    }

    pub fn dispatch(&mut self) -> Result<()> {
        while let Ok(event) = self.inbox.try_recv() {
            match event {
                Event::Click(_) if self.busy => {}
                Event::Click(action) => self.on_click(action)?,
                Event::SaveServerFinished(result) => {
                    self.set_busy(false)?;
                    match result {
                        Err(message) => self.label("SettingsError", &message)?,
                        Ok(_) => {
                            self.find::<PasswordBox>("Password")?.SetPassword("")?;
                            self.update_caption()?;
                            self.label("SettingsError", "")?;
                            self.label("LoginError", "")?;
                            self.show("LoginPage")?;
                        }
                    }
                }
                Event::RestoreFinished(result) => {
                    self.set_busy(false)?;
                    match result {
                        Ok(Some(account)) => self.open_desktop(account)?,
                        Ok(None) => self.label("LoginError", "")?,
                        Err(message) => self.label("LoginError", &message)?,
                    }
                }
                Event::LoginFinished(result) => {
                    self.set_busy(false)?;
                    match result {
                        Err(message) => self.label("LoginError", &message)?,
                        Ok(account) => self.open_desktop(account)?,
                    }
                }
                Event::LogoutFinished(result) => {
                    self.set_busy(false)?;
                    self.return_to_login(result.err().unwrap_or_default())?;
                }
                Event::DevicesFinished(result) => {
                    self.set_busy(false)?;
                    match result {
                        Ok(devices) => {
                            let local_id = self.auth.server_config().cached_device_id;
                            self.update_device_summary(local_id.as_deref(), &devices)?;
                        }
                        Err(message) => {
                            if !self.auth.is_logged_in() {
                                self.return_to_login(message)?;
                            } else {
                                self.label(
                                    "DeviceSummary",
                                    &format!("刷新设备列表失败：{message}"),
                                )?;
                            }
                        }
                    }
                }
                Event::ReloginRequired(reason) => {
                    if self.logged_in {
                        self.set_busy(false)?;
                        self.return_to_login(reason)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn on_click(&mut self, action: Action) -> Result<()> {
        match action {
            Action::Login => self.start_login(),
            Action::Settings => {
                self.find::<TextBox>("Origin")?
                    .SetText(settings::editable_origin(&self.auth.server_config()))?;
                self.label("SettingsError", "")?;
                self.show("SettingsPage")
            }
            Action::Back => self.show("LoginPage"),
            Action::Save => {
                let input = self.find::<TextBox>("Origin")?.Text()?.to_string();
                self.set_busy(true)?;
                self.label("SettingsError", "正在连接并校验服务器…")?;
                let manager = self.auth.manager();
                let server_path = self.auth.server_path();
                let events = self.events.clone();
                thread::spawn(move || {
                    events.send(Event::SaveServerFinished(auth::probe_and_save_server(
                        &manager,
                        &server_path,
                        &input,
                    )));
                });
                Ok(())
            }
            Action::Logout => {
                if self.logged_in {
                    self.auth.stop_maintainer();
                    self.set_busy(true)?;
                    let manager = self.auth.manager();
                    let events = self.events.clone();
                    thread::spawn(move || {
                        events.send(Event::LogoutFinished(auth::logout(&manager)));
                    });
                }
                Ok(())
            }
            Action::RefreshDevices => {
                if self.logged_in {
                    self.set_busy(true)?;
                    let manager = self.auth.manager();
                    let events = self.events.clone();
                    thread::spawn(move || {
                        events.send(Event::DevicesFinished(auth::list_my_devices(&manager)));
                    });
                }
                Ok(())
            }
        }
    }

    fn start_login(&mut self) -> Result<()> {
        if self.logged_in {
            return Ok(());
        }
        if !self.auth.server_config().is_configured() {
            return self.label("LoginError", "请先点击「服务器设置」连接并保存服务器地址。");
        }
        let email = self.find::<TextBox>("Email")?.Text()?.to_string();
        let email = email.trim().to_owned();
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
        let events = self.events.clone();
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

    fn open_desktop(&mut self, account: AccountSession) -> Result<()> {
        self.find::<PasswordBox>("Password")?.SetPassword("")?;
        self.label("LoginError", "")?;
        self.label("Account", &account.email)?;
        let formatted_id = account
            .device_id
            .as_deref()
            .map(server_config::format_device_id)
            .unwrap_or_else(|| "未分配设备码".to_string());
        self.label("LocalDeviceId", &format!("本机：{formatted_id}"))?;
        self.label("DesktopSize", &format!("{} × {}", self.width, self.height))?;
        self.update_device_summary(account.device_id.as_deref(), &account.devices)?;
        self.update_caption()?;
        self.show("DesktopPage")?;
        let panel: SwapChainPanel = self.find("Framebuffer")?;
        let bounds: FrameworkElement = panel.cast()?;
        bounds.SetWidth(self.width.into())?;
        bounds.SetHeight(self.height.into())?;
        let surface = Surface::new(&panel, self.width, self.height)?;
        surface.present_idle()?;
        self.surface = Some(surface);
        self.logged_in = true;
        let events = self.events.clone();
        self.auth.start_maintainer(move |reason| {
            events.send(Event::ReloginRequired(reason));
        });
        Ok(())
    }

    fn return_to_login(&mut self, message: String) -> Result<()> {
        self.auth.stop_maintainer();
        self.surface = None;
        self.logged_in = false;
        self.label("LocalDeviceId", "")?;
        self.label("DeviceSummary", "")?;
        self.label("LoginError", &message)?;
        self.show("LoginPage")
    }

    fn update_device_summary(
        &self,
        local_device_id: Option<&str>,
        devices: &[MyDeviceInfo],
    ) -> Result<()> {
        self.label(
            "DeviceSummary",
            &format_device_summary(local_device_id, devices),
        )
    }
}

fn format_device_summary(local_device_id: Option<&str>, devices: &[MyDeviceInfo]) -> String {
    if devices.is_empty() {
        return "账号下暂无已绑定设备记录".to_string();
    }
    let online_count = devices.iter().filter(|d| d.online).count();
    let preview = devices
        .iter()
        .take(4)
        .map(|d| {
            let name = if !d.alias.trim().is_empty() {
                d.alias.trim()
            } else if !d.hostname.trim().is_empty() {
                d.hostname.trim()
            } else {
                d.device_id.as_str()
            };
            let code = if d.formatted_id.trim().is_empty() {
                server_config::format_device_id(&d.device_id)
            } else {
                d.formatted_id.clone()
            };
            let local_tag = if local_device_id.is_some_and(|local| local == d.device_id) {
                " [本机]"
            } else {
                ""
            };
            let state = if d.online { "在线" } else { "离线" };
            format!("{name}（{code}{local_tag}·{state}）")
        })
        .collect::<Vec<_>>()
        .join(" / ");
    if devices.len() > 4 {
        format!(
            "已绑定 {} 台设备（{} 台在线）：{preview} 等",
            devices.len(),
            online_count
        )
    } else {
        format!(
            "已绑定 {} 台设备（{} 台在线）：{preview}",
            devices.len(),
            online_count
        )
    }
}
