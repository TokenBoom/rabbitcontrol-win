//! 我的设备页：列表渲染、行操作，以及设备列表的刷新时机
//! （信令重新连上 / 本机设备码变化 / 窗口回到前台且超过冷却时间——与 macOS 端一致）。

use std::thread;

use super::{super::actions::*, super::App};
use windows::core::Result;

use windows::core::{Interface, HSTRING};
use windows::UI::Xaml::Controls::Primitives::ButtonBase;
use windows::UI::Xaml::RoutedEventHandler;
use windows::UI::Xaml::{
    Controls::{StackPanel, TextBlock},
    FrameworkElement,
};

use crate::app::{as_ui_element, panel_children};
use crate::biz::auth;
use crate::ui::widgets;
use rc_client_core::auth_client::MyDeviceInfo;
use rc_client_core::server_config;

/// 回到前台后多久之内不再重复刷新设备列表（tick × 50ms = 30 秒）。
const DEVICES_FOREGROUND_COOLDOWN_TICKS: u64 = 600;

impl App {
    /// 把服务端返回的设备列表装进界面（按设备码去重：服务端偶发重复条目）。
    pub(crate) fn install_devices(&mut self, mut devices: Vec<MyDeviceInfo>) -> Result<()> {
        devices.sort_by(|a, b| a.device_id.cmp(&b.device_id));
        devices.dedup_by(|a, b| a.device_id == b.device_id);
        self.devices = devices;
        self.devices_refreshed_tick = Some(self.tick);
        // 不清 online_override：实时事件可能比 REST 快照更新。
        self.render_devices()?;
        self.render_favorites()
    }

    /// 静默刷新设备列表：不置忙碌、失败不打扰。信令重新连上 / 窗口回到前台时用。
    pub(crate) fn refresh_devices_silently(&mut self) -> Result<()> {
        if !self.logged_in || self.devices_refreshing {
            return Ok(());
        }
        self.devices_refreshing = true;
        let manager = self.auth.manager();
        let events = self.sender();
        thread::spawn(move || {
            events.send(Event::DevicesRefreshed(auth::list_my_devices(&manager)));
        });
        Ok(())
    }

    /// 回到前台时用：距上次拿到设备列表不足 [`DEVICES_FOREGROUND_COOLDOWN_TICKS`]（30 秒）就不刷。
    pub(crate) fn refresh_devices_if_stale(&mut self) -> Result<()> {
        let stale = self.devices_refreshed_tick.map_or(true, |tick| {
            self.tick.saturating_sub(tick) >= DEVICES_FOREGROUND_COOLDOWN_TICKS
        });
        if stale {
            self.refresh_devices_silently()
        } else {
            Ok(())
        }
    }

    /// 信令刚连上（或本机设备码变了）：设备列表以服务端为准重新拉一遍。
    pub(crate) fn refresh_devices_on_signal(
        &mut self,
        online: bool,
        device_id: Option<&str>,
    ) -> Result<()> {
        let reconnecting = online && !self.signal_online;
        let id_changed = device_id != self.signal_device_id.as_deref();
        self.signal_online = online;
        self.signal_device_id = device_id.map(str::to_string);
        if (reconnecting || id_changed) && device_id.is_some() {
            self.refresh_devices_silently()?;
        }
        Ok(())
    }

    pub(crate) fn render_devices(&mut self) -> Result<()> {
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

    pub(crate) fn build_device_rows(
        &self,
        local_id: &Option<String>,
    ) -> Result<Vec<FrameworkElement>> {
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
                let item = widgets::find_menu_item(&row, "RowWolItem")?;
                // #7 WOL：离线且被控端上报 wol_capable 才显示
                let wol_visible = !online && device.wol_capable;
                item.SetText(HSTRING::from("远程唤醒 (WOL)"))?;
                as_ui_element(&item)?.SetVisibility(if wol_visible {
                    windows::UI::Xaml::Visibility::Visible
                } else {
                    windows::UI::Xaml::Visibility::Collapsed
                })?;
                let id = device.device_id.clone();
                item.Click(RoutedEventHandler::new(move |_, _| {
                    sender.send(Event::Click(Action::WolWake(id.clone())));
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

    pub(crate) fn device_name_of(&self, device: &MyDeviceInfo) -> String {
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
}
