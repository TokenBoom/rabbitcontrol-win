//! 登录 / 恢复 / 登出 / 服务器设置。

use super::{super::actions::*, super::App};
use windows::core::Result;

use std::thread;

use crate::biz::auth::AccountSession;
use rc_client_core::server_config;
use rc_client_core::server_config::ServerStatusSnapshot;
use windows::core::Interface;
use windows::UI::Xaml::Controls::{PasswordBox, TextBox};
use windows::UI::Xaml::Shapes::Ellipse;

use crate::biz::{auth, settings};

impl App {
    pub(crate) fn start_login(&mut self) -> Result<()> {
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

    pub(crate) fn do_logout(&mut self) -> Result<()> {
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

    pub(crate) fn open_main(&mut self, account: &AccountSession) -> Result<()> {
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

    pub(crate) fn return_to_login(&mut self, message: String) -> Result<()> {
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

    pub(crate) fn save_server_settings(&mut self) -> Result<()> {
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

    pub(crate) fn update_caption(&self) -> Result<()> {
        let origin = settings::format_origin(&self.auth.server_config());
        self.label("ServerCaption", &origin)?;
        self.label("ServerOriginCaption", &origin)?;
        Ok(())
    }

    pub(crate) fn apply_status(
        &mut self,
        snapshot: &ServerStatusSnapshot,
        generation: u64,
    ) -> Result<()> {
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
}
