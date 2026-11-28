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
        let empty = self.filtered_devices().is_empty();
        self.set_visible("DevicesEmpty", empty)?;
        // 没有设备 / 搜索没命中，是两种不同的空
        let (title, hint) = if self.devices.is_empty() {
            ("账号下暂无设备", "在本机或其他设备登录后，设备会出现在这里")
        } else {
            ("没有匹配的设备", "换个关键词，或清空搜索框")
        };
        self.label("DevicesEmptyTitle", title)?;
        self.label("DevicesEmptyHint", hint)?;
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
        for device in &self.filtered_devices() {
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

    /// 按搜索框过滤后的设备（见 [`device_matches`]）。
    pub(crate) fn filtered_devices(&self) -> Vec<&MyDeviceInfo> {
        let filter = self.device_filter.trim().to_lowercase();
        let digits = filter.replace(' ', "");
        self.devices
            .iter()
            .filter(|device| {
                device_matches(
                    &self.device_name_of(device),
                    &device.device_id,
                    &filter,
                    &digits,
                )
            })
            .collect()
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

/// 设备是否命中搜索关键词：设备名或设备码包含关键词，不分大小写；空关键词全部命中。
///
/// `filter` 是小写后的关键词，`digits` 是去掉空格的那一份：设备码在界面上是按
/// `419 450 905` 分组显示的，照着这个样子输入也要能搜到 `419450905`。
fn device_matches(name: &str, device_id: &str, filter: &str, digits: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    name.to_lowercase().contains(filter) || device_id.contains(digits)
}

#[cfg(test)]
mod tests {
    use super::device_matches;

    fn hits(filter: &str) -> bool {
        let filter = filter.trim().to_lowercase();
        device_matches("LUOXIAO-PC", "419450905", &filter, &filter.replace(' ', ""))
    }

    #[test]
    fn search_ignores_case_and_code_grouping() {
        assert!(hits("luoxiao"), "设备名不分大小写");
        assert!(hits("PC"), "设备名的一部分");
        assert!(hits("419 450"), "照界面上的分组写法搜设备码");
        assert!(hits("419450"), "连着写也行");
        assert!(hits("450 905"), "设备码的任意一段");
        assert!(hits(""), "空关键词显示全部");
        assert!(hits("   "), "只有空格也算空");
        assert!(!hits("zzz"), "不沾边的关键词不命中");
    }
}
