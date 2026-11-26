//! 页面子模块：各页面行为与渲染（`impl App` 跨文件分块）。

pub mod assist;
pub mod devices;
pub mod favorites;
pub mod login;
pub mod session_view;
pub mod settings_page;
pub mod tools;

use windows::UI::Xaml::Controls::Primitives::ToggleButton;

use super::actions::set_checked;
use super::actions::Page;
use super::App;
use windows::core::Result;

impl App {
    pub(crate) fn show_page(&mut self, page: Page) -> Result<()> {
        self.set_visible("DevicesPage", page == Page::Devices)?;
        self.set_visible("AssistPage", page == Page::Assist)?;
        self.set_visible("FavoritesPage", page == Page::Favorites)?;
        self.set_visible("SettingsPage", page == Page::Settings)?;
        self.select_nav(page)?;
        if page == Page::Favorites {
            self.render_favorites()?;
        }
        if page == Page::Assist {
            self.render_history()?;
        }
        if page == Page::Settings {
            self.settings_tab_current()?;
        }
        Ok(())
    }

    pub(crate) fn select_nav(&self, page: Page) -> Result<()> {
        for (name, target) in [
            ("NavDevices", Page::Devices),
            ("NavAssist", Page::Assist),
            ("NavFavorites", Page::Favorites),
            ("NavSettings", Page::Settings),
            // Session 页无导航高亮。
        ] {
            if let Some(button) = self.find_opt::<ToggleButton>(name) {
                set_checked(&button, page == target)?;
            }
        }
        Ok(())
    }
}
