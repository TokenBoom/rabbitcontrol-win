//! `rc-host-agent`：RabbitControl Windows 后台被控进程的入口。
//!
//! 没有主窗口，常驻在登录用户的会话里：主界面关掉以后继续提供信令与被控服务，
//! 托盘图标是它在运行时的唯一可见痕迹。见 `rc_host_win` 的模块说明。
//!
//! ```text
//! rc-host-agent                     常规启动（登录时由守护服务拉起，或被主界面拉起）
//! rc-host-agent --gui-pid <pid>     主界面刚启动时带着自己的 PID 拉起：后台先让位
//! rc-host-agent --version           版本号
//! ```
//!
//! 调试构建另支持两个演示开关：`--demo-approval` 启动后弹一个演示用的审批小窗；
//! `--demo-tray-menu X,Y` 启动后在屏幕坐标 (X, Y) 弹出托盘菜单。

#![windows_subsystem = "windows"]

use rc_host_win::app::{self, Hooks};
use rc_host_win::{diag, diag_log, engine::Engine, gui_probe};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("rc-host-agent {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    diag::init(&diag::path());
    rc_appcore::trace_log::init_to_file("RC_TRACE", &diag::path());
    diag_log!(
        "rc-host-agent {} 启动（pid {}，会话 {}）",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        rc_appcore::named_pipe::current_session_id()
    );
    if let Err(err) = run(&args) {
        diag_log!("异常退出：{err}");
        std::process::exit(1);
    }
    diag_log!("已退出");
}

fn run(args: &[String]) -> Result<(), String> {
    let _com = app::init_com()?;
    rc_platform_win::register_all();
    let mut engine = Engine::new()?;
    // 主界面在运行（或本进程正是由主界面拉起的）：先让位，等它退出再接管。
    // 否则什么都不做，交给引擎线程去接管——登录恢复可能要几秒。
    let gui_pid = flag_value(args, "--gui-pid")
        .and_then(|value| value.parse::<u32>().ok())
        .or_else(gui_probe::gui_pid);
    if let Some(pid) = gui_pid {
        let _ = engine.suspend_for_gui(gui_probe::process_probe(pid));
        diag_log!("主界面在运行（pid {pid}）：后台让位");
    }
    let demo_prompt = cfg!(debug_assertions) && args.iter().any(|arg| arg == "--demo-approval");
    // `--demo-tray-menu X,Y`：启动后在屏幕坐标 (X, Y) 弹出托盘菜单（仅调试构建）
    let demo_tray_menu = flag_value(args, "--demo-tray-menu").and_then(|value| {
        let (x, y) = value.split_once(',')?;
        Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
    });
    app::run(
        engine,
        Hooks {
            open_gui: Box::new(gui_probe::open_main_gui),
            after_user_quit: Box::new(|| {
                // 用户主动退出：请守护服务别再把自己拉起来（下次打开主界面时恢复）
                if let Err(err) = rc_appcore::daemon_ipc::standdown() {
                    diag_log!("通知守护服务暂停拉起失败：{err}");
                }
            }),
            demo_prompt,
            demo_tray_menu: cfg!(debug_assertions).then(|| demo_tray_menu).flatten(),
        },
    )
}

/// 取命令行里某个开关的下一个参数。
fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let index = args.iter().position(|arg| arg == flag)?;
    args.get(index + 1).map(String::as_str)
}
