//! 客户端安全锁定（对齐 UU 远程的「安全锁定」）。
//!
//! 锁定期间：本地不能修改客户端设置、不能退出客户端；发起远程控制不受影响。
//! 解锁要输入安全密钥。密钥的派生与校验、按账号保存、失败冷却都在
//! [`rc_appcore::client_lock`]；这里只管流程与界面——横幅、禁用控件、拦下关窗、
//! 解锁弹窗、「远程协助期间自动锁定」，以及本设备卡片上验证码的打码显示。

use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use super::super::{actions::*, App, WithContext, BUSY_CONTROLS};
use rc_appcore::client_lock::{AccountLock, LockStore, UnlockAttempts, MAX_FAILURES};
use windows::core::{Interface, Result, HSTRING};
use windows::UI::Xaml::Controls::{Control, MenuFlyoutItem, PasswordBox};

use super::chrome::Modal;

/// 锁定期间禁用的控件：设置页的开关与输入、本设备卡片上和验证有关的控件。
const LOCKED_CONTROLS: [&str; 19] = [
    "HostSwitch",
    "AssistRotateCode",
    "AssistVerifySettings",
    "HostVerifyMode",
    "HostVerifyPassword",
    "HostVerifySaveButton",
    "HostRotateCodeButton",
    "AssistToggle",
    "FileTransferToggle",
    "LockOnExitToggle",
    "EncryptToggle",
    "HudToggle",
    "AutoStartToggle",
    "CertRenewButton",
    "ProxyApplyButton",
    "ProxyUrl",
    "QualityBox",
    "LockKeyButton",
    "NetworkOrigin",
];

/// 设了安全密钥才有意义的两个开关。
const KEY_REQUIRED_CONTROLS: [&str; 2] = ["LockAutoAssistToggle", "LockAutoStartToggle"];

/// 验证码 / 密码打码后的样子。
const MASKED: &str = "••••••••";

/// 安全锁定的运行时状态（当前登录的账号）。
#[derive(Default)]
pub(crate) struct ClientLock {
    /// 当前账号的配置；登录后才有。
    pub account: Option<AccountLock>,
    /// 现在是否处于锁定状态。
    pub locked: bool,
    /// 这次锁定是「远程协助期间自动锁定」触发的：协助一结束就自动解锁。
    pub auto_locked: bool,
    /// 解锁失败计数与冷却。
    pub attempts: UnlockAttempts,
    /// 本设备卡片上的验证码是否明文显示（眼睛按钮切出来的）。
    pub host_code_revealed: bool,
    /// 到这个 tick 之前，锁定状态下受保护的操作只静默丢弃、不弹解锁框：
    /// 刚登录时程序回填开关，开关会跟着动画晚一点发出 `Toggled`，那不是人在操作。
    pub silent_until: Option<u64>,
}

/// 回填开关的回声最多晚这么多个 tick 到（开关动画约 0.3 秒，留足余量）。
const ECHO_WINDOW_TICKS: u64 = 40;

impl ClientLock {
    /// 这个操作此刻要不要因为锁定而被**静默**丢弃（不弹解锁框）。
    ///
    /// 只在刚登录的那一小段时间里、对受保护的操作成立；真正的用户操作不会来得这么早，
    /// 而且那时设置类控件都还是禁用的。
    pub fn silences(&self, action: &Action, tick: u64) -> bool {
        self.locked
            && action.is_protected_by_lock()
            && self.silent_until.is_some_and(|until| tick < until)
    }
}

fn now_sec() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

impl App {
    fn lock_user_id(&self) -> Option<String> {
        self.auth
            .manager()
            .current_session()
            .map(|session| session.user_id)
    }

    fn lock_has_key(&self) -> bool {
        self.client_lock
            .account
            .as_ref()
            .and_then(|lock| lock.verifier.as_ref())
            .is_some()
    }

    /// 改当前账号的锁定配置并落盘；内存里那份同步刷新。
    fn change_lock(&mut self, change: impl FnOnce(&mut AccountLock)) -> Result<()> {
        let Some(user_id) = self.lock_user_id() else {
            return Ok(());
        };
        let path = LockStore::path();
        let mut store = LockStore::load_from(&path);
        let mut updated = AccountLock::default();
        store.update(&user_id, |lock| {
            change(lock);
            updated = lock.clone();
        });
        store.save_to(&path).map_err(|err| {
            windows::core::Error::new(windows::Win32::Foundation::E_FAIL, err.into())
        })?;
        self.client_lock.account = Some(updated);
        Ok(())
    }

    /// 登录 / 恢复会话后：按这个账号的配置决定是不是一上来就锁定。
    pub(crate) fn restore_client_lock(&mut self) -> Result<()> {
        self.client_lock.account = self
            .lock_user_id()
            .map(|user_id| LockStore::load_from(&LockStore::path()).account(&user_id));
        self.client_lock.auto_locked = false;
        self.client_lock.attempts.reset();
        self.client_lock.locked = self.client_lock.account.as_ref().is_some_and(|lock| {
            // 手动锁定过还没解锁，或者开了「启动时自动锁定」
            lock.locked || (lock.auto_lock_on_start && lock.verifier.is_some())
        });
        self.client_lock.silent_until = self
            .client_lock
            .locked
            .then(|| self.tick + ECHO_WINDOW_TICKS);
        self.apply_client_lock_ui()
    }

    /// 登出 / 回到登录页：内存里的锁定状态清掉，关窗不再被拦（配置留在盘上，下次登录再恢复）。
    pub(crate) fn reset_client_lock(&mut self) {
        self.client_lock = ClientLock::default();
        crate::ui::shell::set_close_guard(false);
    }

    /// 「更多」菜单的「安全锁定」，也是锁定横幅上的「立即解锁」。
    pub(crate) fn on_open_security_lock(&mut self) -> Result<()> {
        if self.client_lock.locked {
            return self.open_unlock_dialog();
        }
        if self.lock_has_key() {
            // 设过密钥：一键锁定，不再多问
            return self.lock_client(false);
        }
        self.label("LockSetupError", "")?;
        self.open_modal(Modal::LockSetup, "安全锁定")
    }

    /// 设置页「设置 / 修改安全密钥」。
    pub(crate) fn on_open_lock_settings(&mut self) -> Result<()> {
        self.label("LockSetupError", "")?;
        let title = if self.lock_has_key() {
            "修改安全密钥"
        } else {
            "安全锁定"
        };
        self.open_modal(Modal::LockSetup, title)
    }

    /// 进入锁定。`auto` = 「远程协助期间自动锁定」触发的：不落盘，协助结束自动解锁。
    fn lock_client(&mut self, auto: bool) -> Result<()> {
        if !auto {
            self.change_lock(|lock| lock.locked = true)?;
        }
        self.client_lock.locked = true;
        self.client_lock.auto_locked = auto;
        // 锁定后验证码不该还亮着
        self.client_lock.host_code_revealed = false;
        self.apply_client_lock_ui()?;
        self.refresh_host_code()?;
        if auto {
            Ok(())
        } else {
            self.show_toast("客户端已安全锁定")
        }
    }

    fn unlock_client(&mut self) -> Result<()> {
        self.change_lock(|lock| lock.locked = false)?;
        self.client_lock.locked = false;
        self.client_lock.auto_locked = false;
        self.client_lock.attempts.reset();
        self.apply_client_lock_ui()
    }

    fn open_unlock_dialog(&mut self) -> Result<()> {
        self.label("LockUnlockError", "")?;
        self.open_modal(Modal::LockUnlock, "客户端已安全锁定")
    }

    /// 锁定期间有人去动受保护的设置：弹解锁框，并说清楚为什么被拦下。
    pub(crate) fn explain_lock(&mut self) -> Result<()> {
        self.open_unlock_dialog()?;
        self.show_toast("客户端已安全锁定，解锁后才能修改设置")
    }

    /// 锁定弹窗点了右上角的关闭、点「取消」：确认框只是关掉，锁定状态不变。
    pub(crate) fn on_lock_dialog_dismissed(&mut self) -> Result<()> {
        self.close_modal()?;
        if self.client_lock.locked {
            self.show_toast("客户端仍处于锁定状态")
        } else {
            Ok(())
        }
    }

    /// 被控会话开始 / 结束：「远程协助本设备期间自动锁定」在这里生效。
    pub(crate) fn on_host_assist_active(&mut self, active: bool) -> Result<()> {
        let auto = self
            .client_lock
            .account
            .as_ref()
            .is_some_and(|lock| lock.auto_lock_on_assist && lock.verifier.is_some());
        if active && auto && !self.client_lock.locked {
            self.lock_client(true)?;
            return self.show_toast("远程协助期间已自动锁定客户端");
        }
        if !active && self.client_lock.locked && self.client_lock.auto_locked {
            self.unlock_client()?;
            return self.show_toast("远程协助已结束，客户端已解锁");
        }
        Ok(())
    }

    // ===== 设置 / 修改密钥 =====

    pub(crate) fn on_lock_setup_submit(&mut self) -> Result<()> {
        let (key, confirm) = (self.secret("LockKeyInput"), self.secret("LockKeyConfirm"));
        if key.trim() != confirm.trim() {
            return self.label("LockSetupError", "两次输入的安全密钥不一致");
        }
        if let Err(err) = rc_appcore::client_lock::validate_key(&key) {
            return self.label("LockSetupError", &err);
        }
        self.label("LockSetupError", "")?;
        // 派生要算一会儿（PBKDF2），放到后台去，界面不卡
        let events = self.sender();
        thread::spawn(move || {
            events.send(Event::LockKeyReady(rc_appcore::client_lock::make_verifier(
                &key,
            )));
        });
        Ok(())
    }

    pub(crate) fn on_lock_key_ready(
        &mut self,
        result: std::result::Result<rc_appcore::client_lock::Verifier, String>,
    ) -> Result<()> {
        match result {
            Ok(verifier) => {
                self.change_lock(|lock| {
                    lock.verifier = Some(verifier);
                    lock.locked = true;
                })?;
                self.close_modal()?;
                self.lock_client(false)?;
                self.show_toast("安全密钥已保存，客户端已锁定")
            }
            Err(err) => self.label("LockSetupError", &err),
        }
    }

    // ===== 解锁 =====

    pub(crate) fn on_lock_unlock_submit(&mut self) -> Result<()> {
        let left = self.client_lock.attempts.cooldown_left(now_sec());
        if left > 0 {
            return self.label(
                "LockUnlockError",
                &format!("尝试次数过多，请在 {left} 秒后重试"),
            );
        }
        let key = self.secret("LockUnlockInput");
        if key.trim().is_empty() {
            return self.label("LockUnlockError", "请输入安全密钥");
        }
        let Some(verifier) = self
            .client_lock
            .account
            .as_ref()
            .and_then(|lock| lock.verifier.clone())
        else {
            // 锁着却没有密钥（配置文件被删了一块）：直接解开，不能把人关在外面
            self.unlock_client()?;
            return self.close_modal();
        };
        self.label("LockUnlockError", "")?;
        self.set_enabled("LockUnlockSubmit", false)?;
        let events = self.sender();
        thread::spawn(move || {
            events.send(Event::LockVerified(rc_appcore::client_lock::verify(
                &verifier, &key,
            )))
        });
        Ok(())
    }

    pub(crate) fn on_lock_verified(&mut self, ok: bool) -> Result<()> {
        self.set_enabled("LockUnlockSubmit", true)?;
        if ok {
            self.unlock_client()?;
            self.close_modal()?;
            return self.show_toast("客户端已解锁");
        }
        let penalty = self.client_lock.attempts.record_failure(now_sec());
        self.label(
            "LockUnlockError",
            &unlock_failure_text(self.client_lock.attempts.failed(), penalty),
        )
    }

    /// 锁定弹窗里的找回路径：退出登录，并清掉这个账号的安全密钥。
    ///
    /// 重新登录要账号密码——正在远程协助这台机器的人没有，所以这条路绕不过锁定；
    /// 忘了密钥的本人在这里退出、重新登录，就能重新设置。
    pub(crate) fn on_lock_recover_logout(&mut self) -> Result<()> {
        self.change_lock(|lock| {
            lock.verifier = None;
            lock.locked = false;
        })?;
        self.unlock_client()?;
        self.close_modal()?;
        self.do_logout()
    }

    // ===== 设置页的两个自动锁定开关 =====

    pub(crate) fn on_lock_auto_assist_toggled(&mut self, on: bool) -> Result<()> {
        self.save_lock_auto(
            on,
            self.client_lock
                .account
                .as_ref()
                .is_some_and(|lock| lock.auto_lock_on_assist),
            "LockAutoAssistToggle",
            |lock| lock.auto_lock_on_assist = on,
        )
    }

    pub(crate) fn on_lock_auto_start_toggled(&mut self, on: bool) -> Result<()> {
        self.save_lock_auto(
            on,
            self.client_lock
                .account
                .as_ref()
                .is_some_and(|lock| lock.auto_lock_on_start),
            "LockAutoStartToggle",
            |lock| lock.auto_lock_on_start = on,
        )
    }

    /// 自动锁定开关的落盘。开关被程序拨到已保存的值时也会发 `Toggled`：
    /// 与已保存的值一致就不是用户在操作；保存失败就把开关拨回去。
    fn save_lock_auto(
        &mut self,
        on: bool,
        current: bool,
        switch: &str,
        change: impl FnOnce(&mut AccountLock),
    ) -> Result<()> {
        if on == current {
            return Ok(());
        }
        if let Err(err) = self.change_lock(change) {
            self.set_switch_on(switch, !on)?;
            return self.show_toast(&format!("保存安全锁定设置失败：{err}"));
        }
        Ok(())
    }

    // ===== 摆到界面上 =====

    /// 把锁定状态摆到界面上：横幅、菜单项的文字、哪些控件禁用、关窗拦不拦。
    ///
    /// 「锁定」优先于「忙碌」：忙碌本来就该禁用的控件，解锁时不能顺手把它们启用。
    pub(crate) fn apply_client_lock_ui(&mut self) -> Result<()> {
        let locked = self.client_lock.locked;
        self.set_visible("LockBanner", locked)?;
        if let Some(item) = self.find_opt::<MenuFlyoutItem>("MenuSecurityLock") {
            item.SetText(HSTRING::from(if locked {
                "立即解锁"
            } else {
                "安全锁定"
            }))?;
        }
        for name in LOCKED_CONTROLS {
            if let Some(control) = self.find_opt::<Control>(name) {
                let busy_disabled = self.busy && BUSY_CONTROLS.contains(&name);
                control
                    .SetIsEnabled(!locked && !busy_disabled)
                    .ctx(|| format!("设置 {name} {}", if locked { "锁定禁用" } else { "启用" }))?;
            }
        }
        let has_key = self.lock_has_key();
        for name in KEY_REQUIRED_CONTROLS {
            if let Some(control) = self.find_opt::<Control>(name) {
                control.SetIsEnabled(has_key && !locked)?;
            }
        }
        self.label(
            "LockKeyStatus",
            if has_key {
                "已设置。安全密钥不可查看，只能修改；忘了密钥，在锁定页面退出登录后重新设置"
            } else {
                "未设置。设置后可以从右上角「更多」一键锁定客户端"
            },
        )?;
        if let Some(button) =
            self.find_opt::<windows::UI::Xaml::Controls::ContentControl>("LockKeyButton")
        {
            let text = if has_key {
                "修改安全密钥"
            } else {
                "设置安全密钥"
            };
            button.SetContent(&windows::Foundation::PropertyValue::CreateString(
                HSTRING::from(text),
            )?)?;
        }
        self.set_switch_on(
            "LockAutoAssistToggle",
            self.client_lock
                .account
                .as_ref()
                .is_some_and(|lock| lock.auto_lock_on_assist),
        )?;
        self.set_switch_on(
            "LockAutoStartToggle",
            self.client_lock
                .account
                .as_ref()
                .is_some_and(|lock| lock.auto_lock_on_start),
        )?;
        crate::ui::shell::set_close_guard(locked);
        Ok(())
    }

    fn set_enabled(&self, name: &str, enabled: bool) -> Result<()> {
        let control: Control = self.find(name)?;
        control
            .SetIsEnabled(enabled)
            .ctx(|| format!("设置 {name} {}", if enabled { "启用" } else { "禁用" }))
    }

    fn secret(&self, name: &str) -> String {
        self.find_opt::<PasswordBox>(name)
            .and_then(|box_| box_.Password().ok())
            .map(|s| s.to_string())
            .unwrap_or_default()
    }

    // ===== 本设备卡片上的验证码 =====

    /// 把验证码刷到本设备卡片上（打码规则见 [`host_code_text`]）。
    pub(crate) fn refresh_host_code(&mut self) -> Result<()> {
        let custom = self.host_verify_custom.unwrap_or(false);
        let password_set = {
            let prefs = self.prefs.lock().unwrap();
            prefs
                .custom_host_password
                .as_deref()
                .is_some_and(|password| !password.is_empty())
        };
        let (text, hint) = host_code_text(
            custom,
            password_set,
            &self.host_rt.temp_code,
            self.client_lock.host_code_revealed,
        );
        self.label("HostCode", &text)?;
        self.label("HostCodeHint", hint)?;
        // 只有临时验证码能看明文：自定义密码保存后不在界面上回显
        self.set_visible(
            "AssistCodeEye",
            !custom && !self.host_rt.temp_code.is_empty(),
        )
    }

    /// 眼睛按钮：临时验证码在明文与打码之间切换。
    pub(crate) fn on_toggle_host_code_visibility(&mut self) -> Result<()> {
        self.client_lock.host_code_revealed = !self.client_lock.host_code_revealed;
        let revealed = self.client_lock.host_code_revealed;
        self.refresh_host_code()?;
        if let Some(icon) =
            self.find_opt::<windows::UI::Xaml::Controls::IconElement>("AssistCodeEyeIcon")
        {
            let icon: windows::UI::Xaml::Controls::FontIcon = icon.cast()?;
            icon.SetGlyph(HSTRING::from(if revealed {
                "\u{ED1A}"
            } else {
                "\u{E890}"
            }))?;
        }
        Ok(())
    }

    /// 本设备卡片下方的错误行：有字才出现。
    pub(crate) fn set_host_error(&mut self, text: &str) -> Result<()> {
        self.label("HostError", text)?;
        self.set_visible("HostError", !text.is_empty())
    }
}

/// 本设备卡片上验证码那一行显示什么、下面那句说明是什么。
///
/// - 自定义密码模式：设了就打码，没设提示去设置；说明密码是固定的；
/// - 临时验证码模式：被控没开时是占位；开了默认打码，点眼睛才明文。
fn host_code_text(
    custom: bool,
    password_set: bool,
    temp_code: &str,
    revealed: bool,
) -> (String, &'static str) {
    const TEMP_HINT: &str = "每次远程控制结束后自动刷新";
    if custom {
        let text = if password_set {
            MASKED.to_string()
        } else {
            "未设置".to_string()
        };
        return (text, "密码固定不变，验证通过后自动接受连接");
    }
    if temp_code.is_empty() {
        return ("开启后显示".to_string(), TEMP_HINT);
    }
    if revealed {
        (temp_code.to_string(), TEMP_HINT)
    } else {
        (MASKED.to_string(), TEMP_HINT)
    }
}

/// 输错安全密钥后的提示：平时报已失败几次；进入冷却后报还要等多久。
fn unlock_failure_text(failed: u32, penalty_secs: u32) -> String {
    if penalty_secs > 0 {
        format!("安全密钥错误，已连续失败 {failed} 次；请在 {penalty_secs} 秒后重试")
    } else {
        format!("安全密钥错误（已失败 {failed}/{MAX_FAILURES} 次）")
    }
}

#[cfg(test)]
mod tests {
    use super::{host_code_text, unlock_failure_text, Action, ClientLock, ECHO_WINDOW_TICKS};

    #[test]
    fn only_lock_echoes_inside_the_window_are_silenced() {
        let mut state = ClientLock {
            locked: true,
            silent_until: Some(ECHO_WINDOW_TICKS),
            ..ClientLock::default()
        };
        let toggle = Action::HostToggle(true);
        assert!(state.silences(&toggle, 0), "刚登录时的回声：静默丢弃");
        assert!(
            !state.silences(&toggle, ECHO_WINDOW_TICKS),
            "窗口过了就按用户操作处理"
        );
        assert!(
            !state.silences(&Action::DeviceConnect("419450905".into()), 0),
            "发起连接不受影响"
        );
        state.locked = false;
        assert!(!state.silences(&toggle, 0), "没锁定就没有静默这回事");
    }

    #[test]
    fn temp_code_is_masked_until_revealed() {
        assert_eq!(
            host_code_text(false, false, "", false),
            ("开启后显示".to_string(), "每次远程控制结束后自动刷新"),
            "被控没开：占位"
        );
        assert_eq!(
            host_code_text(false, false, "ABCD1234", false).0,
            "••••••••",
            "默认打码"
        );
        assert_eq!(host_code_text(false, false, "ABCD1234", true).0, "ABCD1234");
    }

    #[test]
    fn custom_password_never_shows_in_plain_text() {
        assert_eq!(
            host_code_text(true, true, "ABCD1234", true),
            (
                "••••••••".to_string(),
                "密码固定不变，验证通过后自动接受连接"
            ),
            "自定义密码模式下即使眼睛开着也不回显，也不用临时码"
        );
        assert_eq!(host_code_text(true, false, "ABCD1234", false).0, "未设置");
    }

    #[test]
    fn unlock_failure_mentions_the_cooldown_only_when_it_starts() {
        assert_eq!(unlock_failure_text(1, 0), "安全密钥错误（已失败 1/5 次）");
        assert_eq!(
            unlock_failure_text(5, 5),
            "安全密钥错误，已连续失败 5 次；请在 5 秒后重试"
        );
    }
}
