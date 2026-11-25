use reqwest::Url;
use std::sync::mpsc::{self, Receiver, Sender};
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
    auth, desktop,
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
}
enum Event {
    Click(Action),
    LoginFinished(std::result::Result<auth::Session, &'static str>),
    LogoutFinished(std::result::Result<(), &'static str>),
}

// Only values travel across threads. XAML and the swap chain stay on the STA.
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
    server: Url,
    busy: bool,
    session: Option<auth::Session>,
    surface: Option<Surface>,
    width: u32,
    height: u32,
}

impl App {
    pub fn new(shell: &Shell) -> std::result::Result<Self, Box<dyn std::error::Error>> {
        let root = XamlReader::Load(include_str!("view.xaml"))?.cast()?;
        let (tx, inbox) = mpsc::channel();
        let (width, height) = desktop::initial_monitor()?.dimensions();
        let app = Self {
            root,
            events: Events {
                tx,
                hwnd: shell.hwnd(),
            },
            inbox,
            server: settings::load()?,
            busy: false,
            session: None,
            surface: None,
            width,
            height,
        };
        for (name, action) in [
            ("SignIn", Action::Login),
            ("SignOut", Action::Logout),
            ("Settings", Action::Settings),
            ("Back", Action::Back),
            ("Save", Action::Save),
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
        let origin = self.server.as_str().trim_end_matches('/');
        self.label("DesktopServer", origin)?;
        self.label("ServerCaption", origin)
    }
    fn set_busy(&mut self, busy: bool) -> Result<()> {
        self.busy = busy;
        for name in ["SignIn", "SignOut", "Settings", "Email", "Password"] {
            self.find::<Control>(name)?.SetIsEnabled(!busy)?;
        }
        Ok(())
    }
    pub fn dispatch(&mut self) -> Result<()> {
        while let Ok(event) = self.inbox.try_recv() {
            match event {
                Event::Click(_) if self.busy => {}
                Event::Click(action) => self.on_click(action)?,
                Event::LoginFinished(result) => {
                    self.set_busy(false)?;
                    match result {
                        Err(message) => self.label("LoginError", message)?,
                        Ok(session) => self.open_desktop(session)?,
                    }
                }
                Event::LogoutFinished(result) => {
                    self.set_busy(false)?;
                    self.surface = None;
                    self.label("LoginError", result.err().unwrap_or(""))?;
                    self.show("LoginPage")?;
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
                    .SetText(self.server.as_str().trim_end_matches('/'))?;
                self.label("SettingsError", "")?;
                self.show("SettingsPage")
            }
            Action::Back => self.show("LoginPage"),
            Action::Save => {
                let input = self.find::<TextBox>("Origin")?.Text()?.to_string();
                match settings::parse_origin(&input) {
                    Err(message) => self.label("SettingsError", message),
                    Ok(server) => {
                        if settings::save(&server).is_err() {
                            return self
                                .label("SettingsError", "无法保存到当前 Windows 用户的设置。");
                        }
                        if server != self.server {
                            self.find::<PasswordBox>("Password")?.SetPassword("")?;
                        }
                        self.server = server;
                        self.update_caption()?;
                        self.label("LoginError", "")?;
                        self.show("LoginPage")
                    }
                }
            }
            Action::Logout => {
                if let Some(session) = self.session.take() {
                    self.set_busy(true)?;
                    let events = self.events.clone();
                    let server = self.server.clone();
                    std::thread::spawn(move || {
                        events.send(Event::LogoutFinished(auth::logout(&server, session)))
                    });
                }
                Ok(())
            }
        }
    }
    fn start_login(&mut self) -> Result<()> {
        if self.session.is_some() {
            return Ok(());
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
        let events = self.events.clone();
        let server = self.server.clone();
        std::thread::spawn(move || {
            events.send(Event::LoginFinished(auth::login(
                &server, &email, &password,
            )))
        });
        Ok(())
    }
    fn open_desktop(&mut self, session: auth::Session) -> Result<()> {
        self.find::<PasswordBox>("Password")?.SetPassword("")?;
        self.label("LoginError", "")?;
        self.label("Account", &session.user.email)?;
        self.label("DesktopSize", &format!("{} × {}", self.width, self.height))?;
        self.show("DesktopPage")?;
        let panel: SwapChainPanel = self.find("Framebuffer")?;
        let bounds: FrameworkElement = panel.cast()?;
        bounds.SetWidth(self.width.into())?;
        bounds.SetHeight(self.height.into())?;
        let surface = Surface::new(&panel, self.width, self.height)?;
        surface.present_idle()?;
        self.surface = Some(surface);
        self.session = Some(session);
        Ok(())
    }
}
