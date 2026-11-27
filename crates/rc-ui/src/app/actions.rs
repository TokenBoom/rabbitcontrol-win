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
    TerminalSend,
    TerminalNew,
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
    DeviceActionFinished(std::result::Result<(), String>),
    SessionLaunched(Box<SessionLaunchOutcome>, String, String),
    SessionPointer(SessionPointerEvent),
    SessionKey(SessionKeyEvent),
    HostStarted(std::result::Result<rc_client_core::host_bridge::HostHandle, String>),
    AutoStartFinished(std::result::Result<bool, String>),
    UpdateCheckFinished(std::result::Result<String, String>),
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
