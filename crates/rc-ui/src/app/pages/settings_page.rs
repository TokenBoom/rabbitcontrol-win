//! 设置页：常规/安全/网络三 tab。

use super::{super::actions::*, super::App};
use windows::core::Result;

use crate::biz::settings as biz_settings;
use rc_client_core::app_prefs::{self, AppPrefs};
use windows::core::{Interface, HSTRING};
use windows::UI::Xaml::Controls::Primitives::ToggleButton;
use windows::UI::Xaml::Controls::{ComboBox, TextBox, ToggleSwitch};

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
        if let Some(toggle) = self.find_opt::<ToggleSwitch>("FileTransferToggle") {
            toggle.SetIsOn(prefs.allow_assist_file_transfer)?;
        }
        // 代理地址只显示不含密码的形式
        if let Some(box_) = self.find_opt::<windows::UI::Xaml::Controls::TextBox>("ProxyUrl") {
            box_.SetText(HSTRING::from(crate::biz::proxy_cfg::display(&prefs.proxy)))?;
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
