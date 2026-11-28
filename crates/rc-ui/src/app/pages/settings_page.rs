//! 设置页：常规/安全/网络三 tab。
//!
//! 安全 tab 的每一项都真的生效（对齐 macOS 的「安全」设置）：
//!
//! - **默认开启加密连接**：决定新会话是否做端到端 mTLS / E2EE，以及被控端的局域网直连
//!   监听是否强制 mTLS；
//! - **断开后锁定远程屏幕**：主动断开前请被控端锁屏；
//! - **被控验证方式**：临时验证码（每次远控结束自动刷新，可手动刷新）⇄ 自定义密码；
//! - **本机 mTLS 设备证书**：序列号 / 到期时间，可立即续签；
//! - **被远程协助时允许文件互传**。
//!
//! 保存结果写在页内的状态行上（失败时开关拨回原位），不再写到别的页去。

use super::{super::actions::*, super::App};
use windows::core::Result;

use crate::biz::settings as biz_settings;
use rc_client_core::app_prefs::{self, AppPrefs, SecurityPrefsPatch};
use rc_client_core::cert_manager::DeviceCertMetadata;
use windows::core::{Interface, HSTRING};
use windows::UI::Xaml::Controls::Primitives::{Selector, ToggleButton};
use windows::UI::Xaml::Controls::{ComboBox, PasswordBox, TextBox, ToggleSwitch};

/// 「被控验证方式」下拉：临时验证码。
const VERIFY_MODE_TEMP: i32 = 0;
/// 「被控验证方式」下拉：自定义密码。
const VERIFY_MODE_CUSTOM: i32 = 1;

/// 本机设备证书的状态文案（与 macOS 设置页同一口径）。
pub(crate) fn cert_status_text(metadata: Option<&DeviceCertMetadata>, now_unix_sec: u64) -> String {
    match metadata {
        Some(cert) if cert.expires_at_unix_sec > now_unix_sec => {
            let days_left = (cert.expires_at_unix_sec - now_unix_sec) / 86_400;
            format!(
                "mTLS 设备证书有效 · 还有 {days_left} 天到期 · 序列号 {}",
                cert.serial_hex
            )
        }
        Some(_) => "mTLS 设备证书已过期，请点击右侧立即续签设备证书".to_string(),
        None => "未签发（请先登录）".to_string(),
    }
}

/// 当前 Unix 时间（秒）；取不到时按 0 处理（只会让证书显示成「已过期」，不会误报有效）。
fn now_unix_sec() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

impl App {
    pub(crate) fn apply_settings_tab(&self, index: u8) -> Result<()> {
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
        if index == 1 {
            self.refresh_cert_status()?;
        }
        if index == 2 {
            self.settings_tab_current()?;
        }
        Ok(())
    }

    pub(crate) fn settings_tab_current(&self) -> Result<()> {
        let origin = biz_settings::format_origin(&self.auth.server_config());
        self.find::<TextBox>("NetworkOrigin")?
            .SetText(HSTRING::from(&origin))?;
        self.label("CurrentServerCaption", &format!("当前：{origin}"))
    }

    pub(crate) fn load_prefs_into_ui(&mut self) -> Result<()> {
        let prefs = self.prefs.lock().unwrap().clone();
        let index = match prefs.bitrate_preset.as_str() {
            "auto" => 0,
            "8m" => 1,
            "14m" => 2,
            "30m" => 3,
            _ => 1,
        };
        if let Some(combo) = self.find_opt::<ComboBox>("QualityBox") {
            let selector: Selector = combo.cast()?;
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
        if let Some(toggle) = self.find_opt::<ToggleSwitch>("FileTransferToggle") {
            toggle.SetIsOn(prefs.allow_assist_file_transfer)?;
        }
        if let Some(toggle) = self.find_opt::<ToggleSwitch>("AssistToggle") {
            toggle.SetIsOn(prefs.allow_remote_assist)?;
        }
        // 代理地址只显示不含密码的形式
        if let Some(box_) = self.find_opt::<TextBox>("ProxyUrl") {
            box_.SetText(HSTRING::from(crate::biz::proxy_cfg::display(&prefs.proxy)))?;
        }
        // 自定义访问密码（留空 = 使用临时码）：后台被控进程接管时沿用同一份偏好
        let custom_password = prefs
            .custom_host_password
            .clone()
            .filter(|password| !password.is_empty());
        self.sync_host_password_box(custom_password.as_deref())?;
        self.show_host_verify_mode(custom_password.is_some())?;
        self.label(
            "VersionText",
            &format!("版本 {}", env!("CARGO_PKG_VERSION")),
        )?;
        self.refresh_cert_status()
    }

    /// 把「被控验证方式」下拉与随附控件摆成指定模式；`remember` = 同时记住为当前模式
    /// （载入设置时用，免得下拉的变更事件被当成用户操作）。
    fn show_host_verify_mode(&mut self, custom: bool) -> Result<()> {
        self.host_verify_custom = Some(custom);
        if let Some(combo) = self.find_opt::<ComboBox>("HostVerifyMode") {
            let selector: Selector = combo.cast()?;
            selector.SetSelectedIndex(if custom {
                VERIFY_MODE_CUSTOM
            } else {
                VERIFY_MODE_TEMP
            })?;
        }
        self.set_visible("HostVerifyCustomPanel", custom)?;
        self.set_visible("HostRotateCodeButton", !custom)
    }

    /// 设备页与设置页两处都能填自定义访问密码：一处改了，另一处跟着回填。
    fn sync_host_password_box(&self, password: Option<&str>) -> Result<()> {
        if let Some(box_) = self.find_opt::<PasswordBox>("HostPassword") {
            box_.SetPassword(password.unwrap_or_default())?;
        }
        if let Some(box_) = self.find_opt::<PasswordBox>("HostVerifyPassword") {
            box_.SetPassword(password.unwrap_or_default())?;
        }
        Ok(())
    }

    /// 应用并保存一项安全设置，成功后页内提示 `done`。
    ///
    /// 返回 `Err` 时设置没有保存：调用方负责把控件拨回原位。
    fn apply_security_patch(
        &mut self,
        patch: SecurityPrefsPatch,
        done: &str,
    ) -> std::result::Result<(), String> {
        let mut prefs = self.prefs.lock().unwrap();
        if let Err(err) = prefs.apply_security_patch(patch) {
            drop(prefs);
            let _ = self.label("SecurityStatus", &format!("保存安全设置失败：{err}"));
            return Err(err);
        }
        if let Err(err) = app_prefs::save_to_path(&app_prefs::default_prefs_path(), &prefs) {
            drop(prefs);
            let _ = self.label("SecurityStatus", &format!("写入安全设置失败：{err}"));
            return Err(err);
        }
        drop(prefs);
        let _ = self.label("SecurityStatus", done);
        Ok(())
    }

    /// 安全 tab 的开关变了：写入偏好并落盘，成功后页内提示 `done`。
    ///
    /// 返回是否保存成功；失败时开关已被拨回原位（显示与实际必须一致），
    /// 调用方据此决定要不要把新值热切换给运行中的被控服务。
    pub(crate) fn on_security_toggle(
        &mut self,
        name: &str,
        apply: impl FnOnce(&mut AppPrefs) -> std::result::Result<(), String>,
        done: &str,
    ) -> Result<bool> {
        let after = self
            .find_opt::<ToggleSwitch>(name)
            .and_then(|toggle| toggle.IsOn().ok())
            .unwrap_or(false);
        let saved = {
            let mut prefs = self.prefs.lock().unwrap();
            apply(&mut prefs)
                .and_then(|_| app_prefs::save_to_path(&app_prefs::default_prefs_path(), &prefs))
        };
        match saved {
            Ok(()) => {
                let _ = self.label("SecurityStatus", done);
                Ok(true)
            }
            Err(err) => {
                let _ = self.label("SecurityStatus", &format!("保存安全设置失败：{err}"));
                let _ = self.set_switch_on(name, !after);
                Ok(false)
            }
        }
    }

    /// 「被控验证方式」下拉变了：临时验证码 ⇄ 自定义密码。
    pub(crate) fn on_host_verify_mode(&mut self) -> Result<()> {
        let custom = self
            .find_opt::<ComboBox>("HostVerifyMode")
            .and_then(|combo| combo.cast::<Selector>().ok())
            .and_then(|selector| selector.SelectedIndex().ok())
            == Some(VERIFY_MODE_CUSTOM);
        // 载入设置时拨动下拉也会触发本事件：模式和记着的一样就不是用户操作
        if self.host_verify_custom == Some(custom) {
            return Ok(());
        }
        if custom {
            // 选了自定义密码只是把输入框摆出来；密码要点「保存密码」才生效
            self.host_verify_custom = Some(true);
            self.set_visible("HostVerifyCustomPanel", true)?;
            return self.set_visible("HostRotateCodeButton", false);
        }
        // 回到临时验证码：清掉自定义密码；被控服务在跑就立刻换一个新的临时码
        let patch = SecurityPrefsPatch {
            clear_custom_host_password: Some(true),
            ..Default::default()
        };
        if self
            .apply_security_patch(patch, "已切换为临时验证码（每次远控结束自动刷新）")
            .is_err()
        {
            return self.show_host_verify_mode(true);
        }
        self.host_verify_custom = Some(false);
        self.sync_host_password_box(None)?;
        self.set_visible("HostVerifyCustomPanel", false)?;
        self.set_visible("HostRotateCodeButton", true)?;
        if self.host_rt.handle.is_some() {
            self.host_rt.set_password_mode(None);
            self.refresh_host_code()?;
        }
        Ok(())
    }

    /// 设置页「保存密码」：写入偏好；被控服务在跑就对下一次握手生效。
    pub(crate) fn on_host_verify_save(&mut self) -> Result<()> {
        let password = self
            .find_opt::<PasswordBox>("HostVerifyPassword")
            .and_then(|box_| box_.Password().ok())
            .map(|s| s.to_string())
            .unwrap_or_default();
        if password.is_empty() {
            return self.label(
                "SecurityStatus",
                "自定义密码不能为空；不想用密码请把验证方式改回临时验证码",
            );
        }
        let patch = SecurityPrefsPatch {
            custom_host_password: Some(password.clone()),
            ..Default::default()
        };
        if self
            .apply_security_patch(patch, "已保存自定义访问密码")
            .is_err()
        {
            return Ok(());
        }
        self.host_verify_custom = Some(true);
        self.sync_host_password_box(Some(&password))?;
        if self.host_rt.handle.is_some() {
            self.host_rt.set_password_mode(Some(password.clone()));
            self.refresh_host_code()?;
        }
        Ok(())
    }

    /// 设置页「刷新临时验证码」：被控服务在跑才有的刷；刷完立刻生效。
    pub(crate) fn on_host_rotate_code(&mut self) -> Result<()> {
        if self.host_rt.handle.is_none() {
            return self.label("SecurityStatus", "被控未开启，没有可刷新的临时验证码");
        }
        self.host_rt.set_password_mode(None);
        self.refresh_host_code()?;
        self.label("SecurityStatus", "临时验证码已刷新，旧码立即作废")
    }

    /// 设置页「立即续签」：后台清掉旧证书、重新绑定并签发。
    pub(crate) fn on_cert_renew(&mut self) -> Result<()> {
        if !self.logged_in {
            return self.label("SecurityStatus", "请先登录");
        }
        let manager = self.auth.manager();
        let server_path = self.auth.server_path();
        let events = self.sender();
        self.label("SecurityStatus", "正在续签设备证书…")?;
        std::thread::spawn(move || {
            events.send(Event::CertRenewFinished(
                crate::biz::auth::renew_device_cert(&manager, &server_path),
            ));
        });
        Ok(())
    }

    pub(crate) fn on_cert_renew_finished(
        &mut self,
        result: std::result::Result<String, String>,
    ) -> Result<()> {
        match result {
            Ok(device_id) => {
                // 被控服务留着的是旧证书的 mTLS 束：换成新的，在途会话不受影响
                let accept_mtls_lan = self.prefs.lock().unwrap().default_encrypt_connection;
                if let Err(err) = self
                    .host_rt
                    .refresh_mtls(&self.auth.manager(), accept_mtls_lan)
                {
                    return self.label(
                        "SecurityStatus",
                        &format!("设备证书续签成功，但被控服务更新证书失败：{err}"),
                    );
                }
                self.label(
                    "LocalIdText",
                    &rc_client_core::server_config::format_device_id(&device_id),
                )?;
                self.refresh_cert_status()?;
                self.label("SecurityStatus", "设备证书续签成功")
            }
            Err(err) => self.label("SecurityStatus", &format!("续签设备证书失败：{err}")),
        }
    }

    /// 刷新「本机 mTLS 设备证书」状态行。
    pub(crate) fn refresh_cert_status(&self) -> Result<()> {
        let metadata = self
            .auth
            .manager()
            .cert_manager()
            .load_device_cert_metadata();
        self.label(
            "CertStatusText",
            &cert_status_text(metadata.as_ref(), now_unix_sec()),
        )
    }

    pub(crate) fn save_prefs_patch(
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cert(expires_at_unix_sec: u64) -> DeviceCertMetadata {
        DeviceCertMetadata {
            serial_hex: "0123abcd".to_string(),
            user_id: "u".to_string(),
            device_id: "419450905".to_string(),
            expires_at_unix_sec,
            fingerprint_sha256: String::new(),
            issued_at_unix_sec: 0,
        }
    }

    #[test]
    fn cert_status_distinguishes_valid_expired_and_missing() {
        let now = 1_000_000;
        assert_eq!(cert_status_text(None, now), "未签发（请先登录）");
        assert_eq!(
            cert_status_text(Some(&cert(now + 86_400 * 12 + 5)), now),
            "mTLS 设备证书有效 · 还有 12 天到期 · 序列号 0123abcd"
        );
        assert_eq!(
            cert_status_text(Some(&cert(now)), now),
            "mTLS 设备证书已过期，请点击右侧立即续签设备证书"
        );
        assert_eq!(
            cert_status_text(Some(&cert(now - 1)), now),
            "mTLS 设备证书已过期，请点击右侧立即续签设备证书"
        );
    }
}
