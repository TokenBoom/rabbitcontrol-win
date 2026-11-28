//! 登录 / 恢复 / 登出 / 服务器设置。

use super::super::{actions::*, App, WithContext};
use windows::core::Result;

use std::thread;

use crate::biz::auth::AccountSession;
use rc_client_core::server_config;
use rc_client_core::server_config::ServerStatusSnapshot;
use windows::UI::Xaml::Controls::{PasswordBox, TextBlock, TextBox};

/// 给界面操作的错误补上「是哪一步」（见 [`WithContext`]）。
pub(crate) fn ctx<T>(what: &str, result: windows::core::Result<T>) -> windows::core::Result<T> {
    result.ctx(|| what.to_string())
}

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
        self.stop_host_for_logout();
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
        // 登录成功：恢复标题栏元素（登录浮层隐藏后标题栏可见）
        let _ = self.set_visible("AppTitle", true);
        let _ = self.set_visible("TitleLogo", true);
        let _ = self.set_visible("Minimize", true);
        let _ = self.set_visible("CloseWindow", true);
        ctx(
            "清空密码框",
            self.find::<PasswordBox>("Password")?.SetPassword(""),
        )?;
        ctx("清空登录提示", self.label("LoginError", ""))?;
        ctx("显示账号", self.label("AccountEmail", &account.email))?;
        // 用登录响应携带的数据即时渲染首屏，REST 全量随后补齐。
        self.devices = account.devices.clone();
        if let Some(id) = account.device_id.clone() {
            ctx(
                "显示本机设备码",
                self.label("LocalIdText", &server_config::format_device_id(&id)),
            )?;
        }
        self.logged_in = true;
        ctx("隐藏登录浮层", self.set_visible("LoginOverlay", false))?;
        ctx("显示主界面", self.set_visible("MainShell", true))?;
        ctx("记录 DPI", self.apply_scale())?;
        ctx("载入设置", self.load_prefs_into_ui())?;
        if let Err(err) = self.runtime.sync() {
            ctx(
                "信令启动失败提示",
                self.label("AssistError", &format!("信令启动失败：{err}")),
            )?;
        }
        // 信令换了句柄：被控服务手里的那份跟着换
        self.sync_host_signal_client();
        ctx("渲染设备列表", self.render_devices())?;
        ctx("渲染历史记录", self.render_history())?;
        self.page = Page::Devices;
        ctx("切换到设备页", self.show_page(Page::Devices))?;
        ctx("刷新设备", self.on_click(Action::RefreshDevices))?;
        let events = self.sender();
        self.auth.start_maintainer(move |reason| {
            // 强制重登：后台维护线程回调，停运行时并回登录页——原因必须告诉用户。
            eprintln!("[auth] 需要重新登录：{reason}");
            events.send(Event::LogoutFinished(Err(format!("登录已失效：{reason}"))));
        });
        // 「允许被控」此前开着：后台进程已让位，由主界面把被控服务续上
        if rc_appcore::agent_state::AgentState::load().host_enabled {
            let _ = self.set_switch_on("HostSwitch", true);
            ctx("续上被控服务", self.on_host_toggle(true))?;
        }
        Ok(())
    }

    pub(crate) fn return_to_login(&mut self, message: String) -> Result<()> {
        self.auth.stop_maintainer();
        self.stop_host_for_logout();
        self.runtime.stop();
        self.logged_in = false;
        self.devices.clear();
        self.online_override.clear();
        self.signal_online = false;
        self.signal_device_id = None;
        self.devices_refreshing = false;
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
        // 状态靠文字颜色区分（在线 = 品牌绿）；标题栏里没有单独的状态圆点
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
                R: 0x51,
                G: 0x62,
                B: 0x78,
            }
        };
        let brush = windows::UI::Xaml::Media::SolidColorBrush::new()?;
        brush.SetColor(color)?;
        let status: TextBlock = self.find("StatusText")?;
        status
            .SetForeground(&brush)
            .ctx(|| "设置状态文字颜色".to_string())?;
        let state_text = match snapshot.error_message.as_deref() {
            Some(message) => format!("信令：{message}"),
            None if online => "信令在线".to_string(),
            None => "信令连接中…".to_string(),
        };
        self.label("StatusText", &state_text)?;
        self.refresh_devices_on_signal(online, snapshot.device_id.as_deref())?;
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
