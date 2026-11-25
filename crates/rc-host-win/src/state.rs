//! 被控端会话状态机（对应 macOS `rc-host-macos` 的 `state` 模块）。

/// 被控端会话状态。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HostSessionState {
    /// 空闲，无控制会话。
    #[default]
    Idle,
    /// 有待审批的连接请求。
    PendingApproval,
    /// 会话进行中。
    Active,
    /// 会话结束过渡态（恢复分辨率/壁纸等）。
    Teardown,
}

/// 被控端状态跟踪。
#[derive(Debug, Default)]
pub struct HostState {
    state: HostSessionState,
}

impl HostState {
    /// 当前状态。
    pub fn state(&self) -> HostSessionState {
        self.state
    }

    /// 迁移状态。
    pub fn transition(&mut self, next: HostSessionState) {
        self.state = next;
    }
}
