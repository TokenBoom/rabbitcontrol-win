//! 主窗口的「外壳」：标题栏按钮与「更多」菜单、模态框、轻提示、导航高亮。
//!
//! 这些不属于任何一个页面（账号、更新、关于横跨所有页面），单独成一块。
//! 安全锁定的流程在 [`super::client_lock`]，只借用这里的模态框和轻提示。

use std::thread;

use super::super::{actions::*, App};
use windows::core::{Interface, Result};

use windows::UI::Xaml::Controls::{IconElement, Image, MenuFlyoutItem, PasswordBox};
use windows::UI::Xaml::Media::Animation::Storyboard;
use windows::UI::Xaml::{FrameworkElement, RoutedEventHandler};

use crate::biz::update_check::{self, UpdateCheck};
use crate::ui::widgets;

/// 顶部轻提示显示这么多个 tick（50ms 一个）后消失。
const TOAST_TICKS: u64 = 60;

/// 导航项图标：选中品牌蓝，其余深灰。
const NAV_ICON_ACTIVE: &str = "#346CFF";
const NAV_ICON_IDLE: &str = "#4E5969";

/// 当前打开的模态框。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Modal {
    Account,
    ChangePassword,
    Update,
    LockSetup,
    LockUnlock,
    About,
}

/// 模态框的面板与它的名字（XAML 里同名的 `StackPanel`）。
const MODAL_PANELS: [(&str, Modal); 6] = [
    ("ModalAccount", Modal::Account),
    ("ModalChangePassword", Modal::ChangePassword),
    ("ModalUpdate", Modal::Update),
    ("ModalLockSetup", Modal::LockSetup),
    ("ModalLockUnlock", Modal::LockUnlock),
    ("ModalAbout", Modal::About),
];

/// 关模态框时顺手清空的密码框（不清的话下次打开还留着上次输到一半的）。
const MODAL_SECRETS: [&str; 6] = [
    "PasswordCurrent",
    "PasswordNew",
    "PasswordNewConfirm",
    "LockKeyInput",
    "LockKeyConfirm",
    "LockUnlockInput",
];

/// 更新弹窗此刻要显示的内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UpdateView {
    /// 正在检查。
    Checking,
    /// 有新版本。
    Found(UpdateCheck),
    /// 检查完了但没什么可更新的（已是最新 / 尚无发布版本）。
    Message(String),
    /// 没检查成（网络、限流…），可以重试。
    Failed(String),
}

/// 外壳状态。
#[derive(Debug, Default)]
pub(crate) struct Chrome {
    /// 当前打开的模态框；`None` = 没有弹窗。
    pub modal: Option<Modal>,
    /// 轻提示到点收起的 tick。
    pub toast_until: Option<u64>,
    /// 主界面自己的偏好（自动检查更新等）。
    pub local_prefs: crate::biz::local_prefs::LocalPrefs,
    /// 最近一次更新检查的结果：有新版本时提醒按钮常驻，弹窗二次打开直接用它。
    pub update: Option<UpdateView>,
}

impl App {
    /// 标题栏、弹窗、导航之外的那些接线（页面自己的在各自的模块里）。
    pub(crate) fn wire_chrome(&mut self) -> Result<()> {
        self.click("TitleAccount", Action::OpenAccount)?;
        self.click("TitleUpdate", Action::OpenUpdate)?;
        self.click("ModalClose", Action::ModalClose)?;
        self.menu_item("MenuSecurityLock", Action::OpenSecurityLock)?;
        self.menu_item("MenuFeedback", Action::OpenFeedback)?;
        self.menu_item("MenuCheckUpdate", Action::CheckUpdate)?;
        self.menu_item("MenuAbout", Action::OpenAbout)?;
        // 账号弹窗（退出登录按钮也在这里，沿用原来的接线和名字）
        self.click("AccountChangePassword", Action::OpenChangePassword)?;
        self.click("PasswordChangeCancel", Action::ChangePasswordCancel)?;
        self.click("PasswordChangeSubmit", Action::ChangePasswordSubmit)?;
        // 更新弹窗
        self.switch_toggled("UpdateAutoCheck", Action::UpdateAutoCheckToggled(true))?;
        self.click("UpdateDownload", Action::UpdateDownload)?;
        self.click("UpdateOpenPage", Action::UpdateOpenPage)?;
        self.click("UpdateRetry", Action::UpdateRetry)?;
        // 关于弹窗
        self.click("AboutRepo", Action::OpenRepo)?;
        self.click("AboutIssues", Action::OpenIssues)?;
        // 安全锁定（流程在 client_lock.rs）
        self.switch_toggled("LockAutoAssistToggle", Action::LockAutoAssistToggled(true))?;
        self.switch_toggled("LockAutoStartToggle", Action::LockAutoStartToggled(true))?;
        self.click("LockKeyButton", Action::OpenLockSettings)?;
        self.click("LockBannerUnlock", Action::OpenSecurityLock)?;
        self.click("LockSetupSubmit", Action::LockSetupSubmit)?;
        self.click("LockSetupCancel", Action::LockSetupCancel)?;
        self.click("LockUnlockSubmit", Action::LockUnlockSubmit)?;
        self.click("LockRecoverLogout", Action::LockRecoverLogout)?;
        // 本设备卡片上的两个图标
        self.click("AssistCodeEye", Action::ToggleHostCodeVisibility)?;
        self.click("AssistVerifySettings", Action::OpenVerifySettings)?;
        Ok(())
    }

    /// 「更多」菜单里的一项：菜单项不是按钮，`click` 接不上，单独挂。
    fn menu_item(&self, name: &str, action: Action) -> Result<()> {
        let sender = self.sender();
        let item: MenuFlyoutItem = self.find(name)?;
        item.Click(RoutedEventHandler::new(move |_, _| {
            sender.send(Event::Click(action.clone()));
            Ok(())
        }))?;
        Ok(())
    }

    // ===== 模态框 =====

    /// 打开模态框并切到指定面板。
    pub(crate) fn open_modal(&mut self, modal: Modal, title: &str) -> Result<()> {
        self.chrome.modal = Some(modal);
        self.label("ModalTitle", title)?;
        for (name, panel) in MODAL_PANELS {
            self.set_visible(name, panel == modal)?;
        }
        self.set_visible("ModalOverlay", true)
    }

    pub(crate) fn close_modal(&mut self) -> Result<()> {
        self.chrome.modal = None;
        for name in MODAL_SECRETS {
            if let Some(box_) = self.find_opt::<PasswordBox>(name) {
                box_.SetPassword("")?;
            }
        }
        for name in ["PasswordChangeError", "LockSetupError", "LockUnlockError"] {
            let _ = self.label(name, "");
        }
        if let Some(button) =
            self.find_opt::<windows::UI::Xaml::Controls::Control>("PasswordChangeSubmit")
        {
            button.SetIsEnabled(true)?;
        }
        self.set_visible("ModalOverlay", false)
    }

    /// 顶部轻提示：一句话的反馈，3 秒后自动消失。
    pub(crate) fn show_toast(&mut self, text: &str) -> Result<()> {
        self.chrome.toast_until = Some(self.tick + TOAST_TICKS);
        self.label("ToastText", text)?;
        // 锁定横幅也贴着顶上：锁着的时候提示条往下挪，别压在横幅上
        let top = if self.client_lock.locked { 116.0 } else { 52.0 };
        self.find::<FrameworkElement>("Toast")?
            .SetMargin(windows::UI::Xaml::Thickness {
                Left: 0.0,
                Top: top,
                Right: 0.0,
                Bottom: 0.0,
            })?;
        self.set_visible("Toast", true)
    }

    /// 到点收起轻提示（每个 tick 调一次）。
    pub(crate) fn tick_toast(&mut self) -> Result<()> {
        if self
            .chrome
            .toast_until
            .is_some_and(|until| self.tick >= until)
        {
            self.chrome.toast_until = None;
            return self.set_visible("Toast", false);
        }
        Ok(())
    }

    // ===== 标题栏 =====

    /// 登录前 / 登出后把标题栏收起来（沉浸式登录页），登录后放回来。
    pub(crate) fn set_title_bar_visible(&self, visible: bool) -> Result<()> {
        for name in [
            "AppTitle",
            "TitleLogo",
            "TitleMore",
            "TitleAccount",
            "TitleDivider",
            "Minimize",
            "CloseWindow",
        ] {
            let _ = self.set_visible(name, visible);
        }
        // 新版本提醒只在登录后、且确实有新版本时出现
        let _ = self.set_visible("TitleUpdate", visible && self.has_update());
        Ok(())
    }

    fn has_update(&self) -> bool {
        matches!(self.chrome.update, Some(UpdateView::Found(_)))
    }

    // ===== 导航 =====

    /// 导航项图标：选中品牌蓝，其余深灰（底色与竖条由模板的选中态负责）。
    pub(crate) fn paint_nav_icons(&self, page: Page) -> Result<()> {
        for (name, target) in [
            ("NavDevicesIcon", Page::Devices),
            ("NavAssistIcon", Page::Assist),
            ("NavFavoritesIcon", Page::Favorites),
            ("NavSettingsIcon", Page::Settings),
        ] {
            if let Some(icon) = self.find_opt::<IconElement>(name) {
                let brush = widgets::solid_brush(if page == target {
                    NAV_ICON_ACTIVE
                } else {
                    NAV_ICON_IDLE
                })?;
                icon.SetForeground(&brush)?;
            }
        }
        Ok(())
    }

    // ===== 账号 =====

    /// 右上角的账号按钮：显示当前登录的是谁。
    pub(crate) fn on_open_account(&mut self) -> Result<()> {
        let session = self.auth.manager().current_session();
        let (email, display_name, role) = session
            .map(|s| (s.email, s.display_name, s.role))
            .unwrap_or_default();
        self.label("AccountEmail", &email)?;
        self.tooltip("AccountEmail", &email)?;
        let initial = email
            .chars()
            .next()
            .map(|c| c.to_uppercase().collect::<String>())
            .unwrap_or_default();
        self.label("AccountAvatar", &initial)?;
        self.label("AccountMeta", &account_meta(&display_name, &role, &email))?;
        let device = self
            .auth
            .server_config()
            .cached_device_id
            .as_deref()
            .map(rc_client_core::server_config::format_device_id)
            .unwrap_or_default();
        self.label("AccountDevice", &format!("本机设备 ID　{device}"))?;
        self.open_modal(Modal::Account, "账号")
    }

    pub(crate) fn on_open_change_password(&mut self) -> Result<()> {
        self.open_modal(Modal::ChangePassword, "修改密码")
    }

    pub(crate) fn on_change_password_submit(&mut self) -> Result<()> {
        let read = |name: &str| {
            self.find_opt::<PasswordBox>(name)
                .and_then(|box_| box_.Password().ok())
                .map(|s| s.to_string())
                .unwrap_or_default()
        };
        let (current, new, confirm) = (
            read("PasswordCurrent"),
            read("PasswordNew"),
            read("PasswordNewConfirm"),
        );
        if let Err(err) = validate_new_password(&current, &new, &confirm) {
            return self.label("PasswordChangeError", &err);
        }
        self.label("PasswordChangeError", "")?;
        self.find::<windows::UI::Xaml::Controls::Control>("PasswordChangeSubmit")?
            .SetIsEnabled(false)?;
        let manager = self.auth.manager();
        let events = self.sender();
        thread::spawn(move || {
            let result = manager
                .change_password(&current, &new)
                .map(|_| ())
                .map_err(rc_appcore::auth_text::format_auth_error);
            events.send(Event::PasswordChanged(result));
        });
        Ok(())
    }

    pub(crate) fn on_password_changed(
        &mut self,
        result: std::result::Result<(), String>,
    ) -> Result<()> {
        self.find::<windows::UI::Xaml::Controls::Control>("PasswordChangeSubmit")?
            .SetIsEnabled(true)?;
        match result {
            Ok(()) => {
                self.close_modal()?;
                self.show_toast("密码已修改")
            }
            Err(err) => self.label("PasswordChangeError", &err),
        }
    }

    // ===== 更新 =====

    /// 手动检查更新（「更多」菜单 / 设置页）：打开弹窗，转着圈等结果。
    pub(crate) fn on_check_update(&mut self) -> Result<()> {
        self.open_modal(Modal::Update, "更新")?;
        self.chrome.update = Some(UpdateView::Checking);
        self.render_update_modal()?;
        let events = self.sender();
        thread::spawn(move || events.send(Event::UpdateCheckFinished(update_check::check())));
        Ok(())
    }

    /// 右上角的新版本提醒：直接看已拿到的更新信息，不重新检查。
    pub(crate) fn on_open_update(&mut self) -> Result<()> {
        if self.chrome.update.is_none() {
            return self.on_check_update();
        }
        self.open_modal(Modal::Update, "更新")?;
        self.render_update_modal()
    }

    pub(crate) fn on_update_check_finished(
        &mut self,
        result: std::result::Result<UpdateCheck, String>,
    ) -> Result<()> {
        self.chrome.update = Some(match result {
            Ok(check) if check.is_newer => UpdateView::Found(check),
            Ok(check) => UpdateView::Message(check.status_text()),
            Err(err) => UpdateView::Failed(err),
        });
        self.sync_update_status_label()?;
        self.render_update_modal()
    }

    /// 启动时的后台检查：有新版本就让右上角的提醒按钮出现。
    pub(crate) fn on_update_available(&mut self, check: UpdateCheck) -> Result<()> {
        self.chrome.update = Some(UpdateView::Found(check));
        self.sync_update_status_label()?;
        let _ = self.set_visible("TitleUpdate", self.logged_in);
        if self.chrome.modal == Some(Modal::Update) {
            return self.render_update_modal();
        }
        Ok(())
    }

    /// 更新弹窗里的「立即更新」（直奔安装包）与「查看发布页面」。
    pub(crate) fn open_update_link(&mut self, installer: bool) -> Result<()> {
        let (package, page) = self.update_urls();
        let url = if installer { package.or(page) } else { page };
        self.open_url(url.as_deref().unwrap_or(update_check::REPO_URL))
    }

    /// 更新弹窗里两个按钮要打开的地址：`(安装包下载, 发布页面)`；还没检查出来时都是 `None`。
    pub(crate) fn update_urls(&self) -> (Option<String>, Option<String>) {
        match &self.chrome.update {
            Some(UpdateView::Found(check)) => (
                Some(check.install_url().to_string()),
                Some(check.url.clone()),
            ),
            _ => (None, None),
        }
    }

    /// 设置页「更新」那一行的状态文字，与弹窗说的是同一件事。
    fn sync_update_status_label(&self) -> Result<()> {
        let text = match &self.chrome.update {
            Some(UpdateView::Found(check)) => check.status_text(),
            Some(UpdateView::Message(text)) | Some(UpdateView::Failed(text)) => text.clone(),
            Some(UpdateView::Checking) | None => String::new(),
        };
        if text.is_empty() {
            return Ok(());
        }
        self.label("UpdateStatus", &text)
    }

    /// 把当前状态摆进更新弹窗（弹窗开着的时候调用）。
    pub(crate) fn render_update_modal(&mut self) -> Result<()> {
        let view = self.chrome.update.clone();
        let (found, line, failed, checking) = match &view {
            Some(UpdateView::Found(check)) => (Some(check.clone()), None, false, false),
            Some(UpdateView::Message(text)) => (None, Some(text.clone()), false, false),
            Some(UpdateView::Failed(text)) => (None, Some(text.clone()), true, false),
            Some(UpdateView::Checking) | None => (None, None, false, true),
        };
        self.set_visible("UpdateFound", found.is_some())?;
        self.set_visible("UpdateMessage", checking || line.is_some())?;
        self.set_visible("UpdateRetry", failed)?;
        // 检查中：三点脉冲一直转；检查完就停
        self.set_visible("UpdateSpinner", checking)?;
        if checking {
            self.play_storyboard("UpdateSpinnerPulse")?;
        } else if let Some(pulse) = self.find_opt::<Storyboard>("UpdateSpinnerPulse") {
            pulse.Stop()?;
        }
        let _ = self.label(
            "UpdateStatusLine",
            if checking {
                "正在检查更新…"
            } else {
                line.as_deref().unwrap_or("")
            },
        );
        if let Some(check) = found {
            let version = check.latest.as_deref().unwrap_or_default();
            self.label("UpdateVersion", &format!("发现新版本 {version}"))?;
            let date = check.published_date();
            self.label("UpdateDate", &format!("发布于 {date}"))?;
            let notes = check.plain_notes();
            self.label(
                "UpdateNotes",
                if notes.is_empty() {
                    "这次更新没有写更新说明"
                } else {
                    &notes
                },
            )?;
        }
        self.set_switch_on("UpdateAutoCheck", self.chrome.local_prefs.auto_check_update)
    }

    /// 「自动检查更新」开关。程序把开关拨到已保存的值时它也会发 `Toggled`：
    /// 与已保存的值一致就不是用户在操作。
    pub(crate) fn on_update_auto_check_toggled(&mut self, on: bool) -> Result<()> {
        if on == self.chrome.local_prefs.auto_check_update {
            return Ok(());
        }
        self.chrome.local_prefs.auto_check_update = on;
        if let Err(err) = self
            .chrome
            .local_prefs
            .save_to(&crate::biz::local_prefs::LocalPrefs::path())
        {
            return self.show_toast(&err);
        }
        Ok(())
    }

    // ===== 关于 / 链接 =====

    pub(crate) fn on_open_about(&mut self) -> Result<()> {
        self.label(
            "AboutVersion",
            &format!("版本 {}", update_check::CURRENT_VERSION),
        )?;
        if let Some(bitmap) = crate::ui::brand::icon_bitmap(144) {
            if let Ok(source) = bitmap.cast::<windows::UI::Xaml::Media::ImageSource>() {
                if let Some(image) = self.find_opt::<Image>("AboutLogo") {
                    image.SetSource(&source)?;
                }
            }
        }
        self.open_modal(Modal::About, "关于 RabbitControl")
    }

    /// 打开一个网页（问题反馈 / 开源仓库 / 安装包下载）；打不开就在轻提示里说。
    pub(crate) fn open_url(&mut self, url: &str) -> Result<()> {
        match crate::ui::shell::open_url(url) {
            Ok(()) => Ok(()),
            Err(err) => self.show_toast(&err),
        }
    }
}

/// 账号弹窗里邮箱下面那一行：昵称（和邮箱不一样才显示）· 角色。
fn account_meta(display_name: &str, role: &str, email: &str) -> String {
    let role_text = match role {
        "admin" => "管理员",
        "user" => "普通成员",
        "" => "",
        other => other,
    };
    let mut parts: Vec<&str> = Vec::new();
    let name = display_name.trim();
    if !name.is_empty() && name != email {
        parts.push(name);
    }
    if !role_text.is_empty() {
        parts.push(role_text);
    }
    parts.join(" · ")
}

/// 修改密码前的本地检查（服务端还会按同一规则再校验一遍，这里先把明显的问题拦住）。
fn validate_new_password(
    current: &str,
    new: &str,
    confirm: &str,
) -> std::result::Result<(), String> {
    if current.is_empty() {
        return Err("请输入当前密码".into());
    }
    if !(8..=128).contains(&new.len()) {
        return Err("新密码长度需为 8–128 位".into());
    }
    if new != confirm {
        return Err("两次输入的新密码不一致".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{account_meta, validate_new_password};

    #[test]
    fn account_meta_shows_nickname_only_when_it_differs_from_the_email() {
        assert_eq!(account_meta("", "user", "a@b.c"), "普通成员");
        assert_eq!(
            account_meta("a@b.c", "admin", "a@b.c"),
            "管理员",
            "昵称就是邮箱时不重复显示"
        );
        assert_eq!(account_meta("张三", "user", "a@b.c"), "张三 · 普通成员");
        assert_eq!(account_meta("张三", "", "a@b.c"), "张三");
        assert_eq!(
            account_meta("", "owner", "a@b.c"),
            "owner",
            "不认识的角色原样显示"
        );
    }

    #[test]
    fn new_password_is_checked_before_sending() {
        assert_eq!(
            validate_new_password("", "abcd1234", "abcd1234").unwrap_err(),
            "请输入当前密码"
        );
        assert_eq!(
            validate_new_password("old", "short", "short").unwrap_err(),
            "新密码长度需为 8–128 位"
        );
        assert_eq!(
            validate_new_password("old", "abcd1234", "abcd1235").unwrap_err(),
            "两次输入的新密码不一致"
        );
        assert!(validate_new_password("old", "abcd1234", "abcd1234").is_ok());
    }
}
