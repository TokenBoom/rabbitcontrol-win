//! 我的设备页：列表渲染与行操作。

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
use crate::ui::widgets;
use rc_client_core::auth_client::MyDeviceInfo;
use rc_client_core::server_config;

impl App {
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
