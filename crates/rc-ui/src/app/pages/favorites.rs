//! 收藏设备页。

use super::{super::actions::*, super::App};
use windows::core::Result;

use rc_client_core::server_config;
use windows::core::{Interface, HSTRING};
use windows::UI::Xaml::Controls::Primitives::ButtonBase;
use windows::UI::Xaml::{
    Controls::{StackPanel, TextBlock},
    RoutedEventHandler, Visibility,
};

use crate::app::{as_ui_element, panel_children};
use crate::ui::widgets;

impl App {
    pub(crate) fn render_favorites(&mut self) -> Result<()> {
        let host: StackPanel = self.find("FavoritesListHost")?;
        let children = panel_children(&host)?;
        children.Clear()?;
        let snapshot = self.history.lock().unwrap().clone();
        let favorites = snapshot.favorites().to_vec();
        self.set_visible("FavoritesEmpty", favorites.is_empty())?;
        for device_id in favorites {
            let name = self
                .devices
                .iter()
                .find(|d| d.device_id == device_id)
                .map(|d| self.device_name_of(d))
                .unwrap_or_else(|| "未知设备".to_string());
            let row = widgets::load_device_row()?;
            let name_text: TextBlock = row.FindName("RowName")?.cast()?;
            name_text.SetText(HSTRING::from(&name))?;
            let code_text: TextBlock = row.FindName("RowCode")?.cast()?;
            code_text.SetText(HSTRING::from(&server_config::format_device_id(&device_id)))?;
            widgets::set_device_online(&row, false, false, false)?;
            {
                let sender = self.sender();
                let button: ButtonBase = row.FindName("RowConnect")?.cast()?;
                let id = device_id.clone();
                button.Click(RoutedEventHandler::new(move |_, _| {
                    sender.send(Event::Click(Action::DeviceConnect(id.clone())));
                    Ok(())
                }))?;
            }
            {
                let sender = self.sender();
                let item = widgets::find_menu_item(&row, "RowFavItem")?;
                let id = device_id.clone();
                item.Click(RoutedEventHandler::new(move |_, _| {
                    sender.send(Event::Click(Action::DeviceToggleFav(id.clone())));
                    Ok(())
                }))?;
            }
            if let Ok(rename_item) = widgets::find_menu_item(&row, "RowRenameItem") {
                as_ui_element(&rename_item)?.SetVisibility(Visibility::Collapsed)?;
            }
            if let Ok(unbind_item) = widgets::find_menu_item(&row, "RowUnbindItem") {
                as_ui_element(&unbind_item)?.SetVisibility(Visibility::Collapsed)?;
            }
            children.Append(&as_ui_element(&row)?)?;
        }
        Ok(())
    }
}
