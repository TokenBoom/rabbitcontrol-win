//! 应用动作/事件/纯值类型（跨线程消息包；不含 XAML 对象）。

use std::sync::mpsc::Sender;

use crate::biz::auth::AccountSession;
use rc_client_core::auth_client::MyDeviceInfo;
use rc_client_core::session_bridge::SessionHandle;
use windows::core::Interface;
use windows::System::VirtualKey;
use windows::UI::Xaml::Controls::Primitives::ToggleButton;

use crate::ui::ShellNotifier;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Page {
    Devices,
    Assist,
    Favorites,
    Settings,
}

#[derive(Clone)]
pub(crate) enum Action {
    Login,
    OpenServerSettings,
    SaveServer,
    Logout,
    RefreshDevices,
    Nav(Page),
    CopyId,
    AssistToggled(bool),
    ToggleCodeVisibility,
    AdvancedToggle,
    HistoryTabFav(bool),
    DeviceConnect(String),
    ConnectSubmit,
    DeviceToggleFav(String),
    DeviceRename(String),
    DeviceUnbind(String),
    RecentDelete(String),
    RecentConnect(String),
    SettingsTab(u8),
    QualityChanged,
    HudToggled(bool),
    EncryptToggled(bool),
    LockOnExitToggled(bool),
    ProbeServer,
    BannerDisconnect,
    SessionDisconnect,
    SessionReconnect,
    SessionQualityToggle,
    StreamQualityChanged,
    MouseModeChanged,
    StageRectChanged(u32, u32),
    SessionFullscreen,
    DisplayChanged,
    ToolToggle,
    ToolTab(u8),
    /// 点了已经选中的工具 tab（ToggleButton 把它取消了选中）。
    ToolTabUnchecked(u8),
    TerminalNew,
    TerminalClose,
    /// 翻看历史时有新输出：点一下回到底部继续跟随。
    TerminalToBottom,
    /// 终端锁屏校验：提交被控端系统密码。
    TerminalLockSubmit,
    FileUploadPick,
    FileDownloadPick,
    TransferCancel(u64),
    PortmapAdd,
    PortmapRule(u64, u8),
    PromptOk,
    PromptCancel,
    ProxyApply,
    FileTransferToggled(bool),
    CheckUpdate,
    /// 设置页「被控验证方式」下拉变了。
    HostVerifyModeChanged,
    /// 设置页保存自定义访问密码。
    HostVerifySave,
    /// 设置页刷新临时验证码。
    HostRotateCode,
    /// 设置页「立即续签」本机设备证书。
    CertRenew,
    BackToLogin,
    WolWake(String),
    HostToggle(bool),
    HostApprove,
    HostReject,
    RenamePromptOk,
    RenamePromptCancel,
    AutoStartToggled(bool),
}

pub(crate) enum Event {
    Click(Action),
    RestoreFinished(std::result::Result<Option<AccountSession>, String>),
    LoginFinished(std::result::Result<AccountSession, String>),
    LogoutFinished(std::result::Result<(), String>),
    SaveServerFinished(std::result::Result<(), String>),
    DevicesFinished(std::result::Result<Vec<MyDeviceInfo>, String>),
    /// 静默刷新设备列表的结果（信令重新连上 / 回到前台时触发；不置忙碌，失败不打扰）。
    DevicesRefreshed(std::result::Result<Vec<MyDeviceInfo>, String>),
    DeviceActionFinished(std::result::Result<(), String>),
    SessionLaunched(Box<SessionLaunchOutcome>, String, String),
    SessionPointer(SessionPointerEvent),
    SessionKey(SessionKeyEvent),
    Terminal(TerminalEvent),
    HostStarted(std::result::Result<rc_appcore::host::StartedHost, String>),
    AutoStartFinished(std::result::Result<bool, String>),
    UpdateCheckFinished(std::result::Result<String, String>),
    /// 启动时的后台检查：只在有新版本时发出，仅更新文案、不改 busy 状态。
    UpdateAvailable(String),
    WolFinished(String, std::result::Result<String, String>),
    /// 设备证书续签结果（后台线程）。
    CertRenewFinished(std::result::Result<String, String>),
    /// 剪贴板文本读回来了（终端粘贴用）。
    ClipboardRead(std::result::Result<String, String>),
    /// 工具面板分隔条：按下 / 拖动（相对按下点的横向位移，逻辑像素）/ 松开。
    ToolSplitter(Option<f64>),
}

/// 会话启动结果（Box 降低 Event 枚举整体尺寸）。
pub(crate) struct SessionLaunchOutcome {
    pub(crate) result: std::result::Result<SessionHandle, String>,
    pub(crate) target_id: String,
    pub(crate) target_name: String,
}

/// 会话舞台鼠标事件（纯值）。
#[derive(Debug, Clone, Copy)]
pub(crate) enum SessionPointerEvent {
    Move {
        x: f64,
        y: f64,
    },
    Button {
        x: f64,
        y: f64,
        button: u8,
        pressed: bool,
    },
    Wheel {
        delta: i32,
    },
}

/// 会话键盘事件（纯值）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct SessionKeyEvent {
    pub(crate) key: VirtualKey,
    pub(crate) pressed: bool,
}

/// 终端面板事件（纯值）。会话窗口里的元素经事件通道回传，App 在 UI 线程统一处理。
#[derive(Debug, Clone, Copy)]
pub(crate) enum TerminalEvent {
    /// 输入框 `PreviewKeyDown` 里被本地消费的按键（虚拟键码 + 修饰键）：
    /// 交给输入框解析的字符键不发——它们走 [`TerminalEvent::TextCommitted`]。
    Key {
        code: u32,
        shift: bool,
        ctrl: bool,
        alt: bool,
    },
    /// 输入框里有了文本（可打印字符 / 输入法上屏）。
    TextCommitted,
    /// 输入法组合输入开始 / 结束。
    Composition(bool),
    /// 输入框获得 / 失去键盘焦点。
    Focus(bool),
    /// 终端画面上的指针事件（坐标相对画面左上角，逻辑像素）。
    Pointer(TerminalPointer),
    /// 终端画面尺寸变了（App 自己去读实际宽高）。
    Resized,
}

/// 终端画面上的指针事件（纯值）。
#[derive(Debug, Clone, Copy)]
pub(crate) enum TerminalPointer {
    /// 按下：`button` 0 左 / 1 右 / 2 中，`time_us` 是事件时间戳（微秒，数连击用）。
    Press {
        x: f64,
        y: f64,
        button: u8,
        time_us: u64,
    },
    /// 移动。
    Move { x: f64, y: f64 },
    /// 松开。
    Release,
    /// 滚轮（`delta` 为正向上滚）。
    Wheel { delta: i32 },
    /// 滚动条滑块：`pressed` 为真是按下（`y` 落在滑块上就是抓住它，落在轨道别处就是跳过去），
    /// 为假是按着拖动；`y` 是指针离画面顶端的距离。
    Thumb { y: f64, pressed: bool },
}

impl Action {
    /// 开关类操作：由开关的 `Toggled` 事件派发，`on` = 开关当前是否打开。
    ///
    /// 返回 `None` 表示这个操作不是开关类操作——接线时必须报错，不能兜底成别的操作
    /// （曾经兜底成「切换性能 HUD」，被控 / 自启 / 文件互传开关因此全都无效）。
    pub(crate) fn from_switch(kind: &Action, on: bool) -> Option<Action> {
        match kind {
            Action::HudToggled(_) => Some(Action::HudToggled(on)),
            Action::EncryptToggled(_) => Some(Action::EncryptToggled(on)),
            Action::LockOnExitToggled(_) => Some(Action::LockOnExitToggled(on)),
            Action::HostToggle(_) => Some(Action::HostToggle(on)),
            Action::AutoStartToggled(_) => Some(Action::AutoStartToggled(on)),
            Action::FileTransferToggled(_) => Some(Action::FileTransferToggled(on)),
            Action::AssistToggled(_) => Some(Action::AssistToggled(on)),
            _ => None,
        }
    }

    /// 轻量开关类操作：不发起耗时流程，主界面忙碌时也应立即生效。
    ///
    /// 这些开关在忙碌时并未禁用；若把它们的点击拦下，开关显示的状态就和实际对不上。
    /// 耗时流程（登录 / 刷新 / 连接 / 被控开关 / 自启）的控件在忙碌时被禁用，走不到这里。
    pub(crate) fn is_light_toggle(&self) -> bool {
        matches!(
            self,
            Action::HudToggled(_)
                | Action::EncryptToggled(_)
                | Action::LockOnExitToggled(_)
                | Action::AssistToggled(_)
                | Action::FileTransferToggled(_)
        )
    }
}

/// 全局禁用控件清单（busy 态）；缺失的条目自动跳过。

#[derive(Clone)]
pub(crate) struct Events {
    pub(crate) tx: Sender<Event>,
    pub(crate) notifier: ShellNotifier,
}

impl Events {
    pub(crate) fn send(&self, event: Event) {
        let _ = self.tx.send(event);
        self.notifier.notify();
    }
}

/// 包装 PropertyValue→IReference 的 ToggleButton 选中态设置（0.37 投影）。
pub(crate) fn set_checked(toggle: &ToggleButton, on: bool) -> windows::core::Result<()> {
    let value = windows::Foundation::PropertyValue::CreateBoolean(on)?;
    let reference: windows::Foundation::IReference<bool> = value.cast()?;
    toggle.SetIsChecked(&reference)
}

#[cfg(test)]
mod switch_tests {
    use super::*;

    #[test]
    fn every_switch_action_carries_the_switch_state() {
        let cases: &[Action] = &[
            Action::HudToggled(true),
            Action::EncryptToggled(true),
            Action::LockOnExitToggled(true),
            Action::HostToggle(true),
            Action::AutoStartToggled(true),
            Action::FileTransferToggled(true),
            Action::AssistToggled(true),
        ];
        for kind in cases {
            let on = Action::from_switch(kind, true).expect("开关类操作必须能派发");
            let off = Action::from_switch(kind, false).expect("开关类操作必须能派发");
            // 派发出的必须是同一种操作（不能再兜底成别的），且带上开关的实际状态
            assert!(
                matches!(
                    (kind, &on),
                    (Action::HudToggled(_), Action::HudToggled(true))
                        | (Action::EncryptToggled(_), Action::EncryptToggled(true))
                        | (
                            Action::LockOnExitToggled(_),
                            Action::LockOnExitToggled(true)
                        )
                        | (Action::HostToggle(_), Action::HostToggle(true))
                        | (Action::AutoStartToggled(_), Action::AutoStartToggled(true))
                        | (
                            Action::FileTransferToggled(_),
                            Action::FileTransferToggled(true)
                        )
                        | (Action::AssistToggled(_), Action::AssistToggled(true))
                ),
                "打开开关却派发出了别的操作 / 状态"
            );
            assert!(
                matches!(
                    (kind, &off),
                    (Action::HudToggled(_), Action::HudToggled(false))
                        | (Action::EncryptToggled(_), Action::EncryptToggled(false))
                        | (
                            Action::LockOnExitToggled(_),
                            Action::LockOnExitToggled(false)
                        )
                        | (Action::HostToggle(_), Action::HostToggle(false))
                        | (Action::AutoStartToggled(_), Action::AutoStartToggled(false))
                        | (
                            Action::FileTransferToggled(_),
                            Action::FileTransferToggled(false)
                        )
                        | (Action::AssistToggled(_), Action::AssistToggled(false))
                ),
                "关闭开关却派发出了别的操作 / 状态"
            );
        }
    }

    #[test]
    fn non_switch_actions_are_rejected_instead_of_retargeted() {
        assert!(Action::from_switch(&Action::Login, true).is_none());
        assert!(Action::from_switch(&Action::Logout, true).is_none());
        assert!(Action::from_switch(&Action::RefreshDevices, true).is_none());
        assert!(Action::from_switch(&Action::CopyId, true).is_none());
    }

    #[test]
    fn only_instant_toggles_pass_the_busy_gate() {
        for light in [
            Action::HudToggled(true),
            Action::EncryptToggled(false),
            Action::LockOnExitToggled(true),
            Action::AssistToggled(false),
            Action::FileTransferToggled(true),
        ] {
            assert!(light.is_light_toggle());
        }
        for heavy in [
            Action::HostToggle(true),
            Action::AutoStartToggled(true),
            Action::Login,
            Action::RefreshDevices,
        ] {
            assert!(!heavy.is_light_toggle());
        }
    }
}
