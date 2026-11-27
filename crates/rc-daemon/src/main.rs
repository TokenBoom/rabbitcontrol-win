//! RabbitControl Windows 后台服务（LocalSystem，M5b）。
//!
//! # 设计
//!
//! - 以 Windows Service 运行（`StartServiceCtrlDispatcherW`），
//!   LocalSystem 权限；安装/卸载/启停/状态作为子命令暴露（需管理员）；
//! - 职责仅限需要提权或跨会话的操作：UAC 安全桌面/锁屏下的输入
//!   注入代理（`inject` 操作，LocalSystem 令牌穿透 UIPI）、显示模式
//!   切换（后续批次）、agent
//!   （rc-host-agent）进程监管——活动控制台会话出现时以用户令牌
//!   拉起 agent，注销时终止；用户从托盘退出后不再拉起（`standdown` /
//!   `resume`）；
//! - 与管理端经命名管道 `\\.\pipe\rabbitcontrol-daemon` 通信
//!   （行分隔 JSON，见 [`protocol`]）；管道只放行本机交互登录的用户、
//!   SYSTEM 与管理员，特权操作逐条核对调用方身份（见 [`security`]）；
//! - 实际的采集/编解码/渲染全部发生在用户会话的 agent 内，
//!   Session 0 摸不到用户桌面，本服务不碰媒体链路。
//!
//! # 用法
//!
//! ```text
//! rc-daemon run            # SCM 服务入口（勿手动运行）
//! rc-daemon install        # 注册服务（AUTO_START，需管理员）
//! rc-daemon uninstall      # 停止并删除服务
//! rc-daemon start | stop   # 启动/停止服务
//! rc-daemon status         # SCM 状态 / 管道探测
//! rc-daemon ping           # 管道探测（服务在跑才可达）
//! ```

mod firewall;
mod inject;
mod protocol;
mod security;
mod service;

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("run") => {
            if let Err(err) = service::run_service() {
                eprintln!("{err}");
                std::process::exit(1);
            }
        }
        Some("install") => report(service::install()),
        Some("uninstall") => report(service::uninstall()),
        Some("start") => report(service::control(true)),
        Some("stop") => report(service::control(false)),
        Some("status") => report(service::status()),
        Some("setup") => report(service::setup()),
        Some("teardown") => report(service::teardown()),
        Some("ping") => match rc_appcore::daemon_ipc::call(r#"{"op":"ping"}"#, 3_000) {
            Ok(reply) => println!("pong：{reply}"),
            Err(err) => {
                println!("ping 失败：{err}");
                std::process::exit(1);
            }
        },
        Some("--version") | Some("-V") => {
            println!("rc-daemon {}", env!("CARGO_PKG_VERSION"));
        }
        other => {
            let invoked = other.unwrap_or("<无参数>");
            println!(
                "rc-daemon {}（未知/缺省子命令：{invoked}）\n用法见文件头注释：run/install/uninstall/start/stop/status/ping",
                env!("CARGO_PKG_VERSION")
            );
        }
    }
}

fn report(result: Result<(), String>) {
    match result {
        Ok(()) => println!("完成"),
        Err(err) => {
            println!("失败：{err}");
            std::process::exit(1);
        }
    }
}
