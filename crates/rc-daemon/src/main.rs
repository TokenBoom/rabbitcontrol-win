//! RabbitControl Windows 后台服务。
//!
//! # 设计
//!
//! - 以 Windows Service 运行（`StartServiceCtrlDispatcherW`），
//!   LocalSystem 权限；服务控制管理器命令（安装/启动/停止/卸载）
//!   作为子命令暴露；
//! - 职责仅限需要提权或跨会话的操作：UAC 安全桌面/锁屏下的输入
//!   注入代理、显示模式切换、agent（rc-host-win）进程监管——用户
//!   登录会话变化时（`WTSRegisterSessionNotification`）在对应会话
//!   拉起/回收 agent（`CreateProcessAsUserW`）；
//! - 与 agent 经命名管道 `\.\pipe\rabbitcontrol-win` 通信；
//! - 实际的采集/编解码/渲染全部发生在用户会话的 agent 内，
//!   Session 0 摸不到用户桌面，本服务不碰媒体链路。
//!
//! # 当前状态
//!
//! 骨架占位：直接运行打印状态后退出，服务控制分发待实现。

fn main() {
    rc_platform_win::register_all();
    println!(
        "rc-daemon {}（骨架：Windows Service 集成待实现）",
        env!("CARGO_PKG_VERSION")
    );
}
