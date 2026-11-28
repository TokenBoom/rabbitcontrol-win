//! 开始协助页：最近/收藏、临时验证码区、连接入口。

use super::{super::actions::*, super::App};
use windows::core::Result;

use rc_client_core::server_config;
use windows::core::{Interface, HSTRING};
use windows::UI::Xaml::Controls::Primitives::ButtonBase;
use windows::UI::Xaml::Controls::Primitives::ToggleButton;
use windows::UI::Xaml::Controls::{Control, StackPanel, TextBlock, TextBox};
use windows::UI::Xaml::RoutedEventHandler;
use windows::UI::Xaml::{FrameworkElement, VisualStateManager};

use crate::app::{as_ui_element, panel_children, set_checked};
use crate::biz::history;
use crate::ui::widgets;

impl App {
    /// 「连接」按钮能不能点，跟着伙伴 ID 输入框走。
    ///
    /// `force` = 把视觉状态重新推一遍。自定义模板在「一加载就是禁用」时不会进 Disabled 状态，
    /// 按钮会一直亮着像能点；所以每次进这一页都按当前可用态把状态机推到对应的状态。
    pub(crate) fn sync_connect_enabled(&self, force: bool) -> Result<()> {
        let digits: String = self
            .find_opt::<TextBox>("PeerCodeBox")
            .and_then(|box_| box_.Text().ok())
            .map(|text| super::super::digit_of(&text.to_string()))
            .unwrap_or_default();
        let enabled = digits.len() == 9;
        let connect: Control = self.find("ConnectPeerButton")?;
        connect.SetIsEnabled(enabled)?;
        if force {
            let state = if enabled { "Normal" } else { "Disabled" };
            VisualStateManager::GoToState(&connect, HSTRING::from(state), false)?;
        }
        Ok(())
    }

    pub(crate) fn render_history(&mut self) -> Result<()> {
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

    pub(crate) fn recent_row(
        &self,
        device_id: &str,
        name: &str,
        when: &str,
    ) -> Result<FrameworkElement> {
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

    pub(crate) fn prefill_connect(&mut self, code: &str) -> Result<()> {
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

    pub(crate) fn persist_history(&self) -> Result<()> {
        let path = history::default_history_path();
        let result = self.history.lock().unwrap().save_to(&path);
        if let Err(err) = result {
            self.label("AssistError", &err)?;
        }
        Ok(())
    }
}

pub(super) fn relative_time(at_sec: u64) -> String {
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
