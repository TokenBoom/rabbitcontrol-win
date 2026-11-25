//! RabbitControl Windows 后台守护服务（对应 macOS `rc-daemon` / LaunchDaemon）。
//!
//! # 设计
//!
//! - 以 Windows Service 运行（`StartServiceCtrlDispatcherW`），LocalSystem
//!   权限，覆盖锁屏/UAC 安全桌面下的采集与输入注入；
//! - 双会话协同：服务进程负责被控会话与硬件访问，用户会话内的
//!   `rc-host-win` 托盘负责交互（对应 macOS LaunchDaemon/LaunchAgent 分工）；
//! - 与桌面端经 IPC（命名管道 `\\.\pipe\rabbitcontrol-win`）通信。
//!
//! # 当前状态
//!
//! 骨架占位：直接运行打印状态后退出，服务注册表集成待实现。

fn main() {
    rc_platform_win::register_all();
    println!(
        "rc-daemon {}（骨架：Windows Service 集成待实现）",
        env!("CARGO_PKG_VERSION")
    );
}
