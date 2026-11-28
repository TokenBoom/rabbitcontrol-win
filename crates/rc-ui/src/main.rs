// #![windows_subsystem = "windows"] // 调试：启用控制台

mod app;
mod biz;
mod connect_probe;
#[allow(dead_code)] // M5a 接线后移除
mod media;
mod ui;

use std::error::Error;
use std::os::windows::process::CommandExt;

use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, MessageBoxW, SetForegroundWindow, ShowWindow, MB_ICONERROR, SW_RESTORE,
};

use windows::core::{HSTRING, PCWSTR};

use rc_appcore::agent_ipc::{self, GUI_WINDOW_CLASS as WINDOW_CLASS};

/// 与后台被控进程握手的超时（它可能正在收尾一个远端会话）。
const AGENT_HANDSHAKE_MS: u32 = 5_000;

/// 进程级单实例守卫：已存在实例时唤起其窗口并返回 false。
fn acquire_single_instance() -> bool {
    let class = HSTRING::from(WINDOW_CLASS);
    let existing = unsafe { FindWindowW(PCWSTR(class.as_wide().as_ptr()), None) };
    if existing.0 != 0 {
        // 已有实例：激活窗口并退出。
        unsafe {
            let _ = ShowWindow(existing, SW_RESTORE);
            let _ = SetForegroundWindow(existing);
        }
        return false;
    }
    true
}

/// 与后台被控进程（`rc-host-agent`）交接。
///
/// 同一设备只能有一条信令连接：主界面打开期间由主界面托管信令与被控，后台进程
/// 让位；主界面退出后它再接管（见 `rc-appcore::signal_runtime` 的说明）。所以
/// 启动时先让它让位；它没在运行就拉起一个（带上本进程 PID，它据此等待）。
/// 同时请守护服务恢复「登录时拉起后台进程」——用户上次可能是从托盘主动退出的。
fn handshake_with_agent() {
    let pid = std::process::id();
    let yielded = agent_ipc::call(&agent_ipc::Request::GuiOnline { pid }, AGENT_HANDSHAKE_MS)
        .is_ok_and(|response| response.ok);
    if !yielded {
        spawn_agent(pid);
    }
    let _ = rc_appcore::daemon_ipc::resume();
}

/// 拉起后台被控进程（独立于本进程的生存期）。
fn spawn_agent(gui_pid: u32) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(dir) = exe.parent() else {
        return;
    };
    let agent = dir.join(agent_ipc::AGENT_EXE);
    // 旧版打包里没有后台进程：主界面照常独立运行
    if !agent.exists() {
        return;
    }
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    let mut command = std::process::Command::new(&agent);
    command
        .arg("--gui-pid")
        .arg(gui_pid.to_string())
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    if let Err(err) = command.spawn() {
        eprintln!("[agent] 拉起后台被控进程失败：{err}");
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    if !acquire_single_instance() {
        return Ok(());
    }
    // 必须先于登录恢复与信令连接：否则主界面会与后台进程互踢信令。
    // 诊断时可以跳过（本进程用 RC_CONFIG_DIR 扮演第二台设备，不该让本机后台进程让位）。
    let no_handshake = std::env::args()
        .skip(1)
        .any(|arg| arg == "--no-agent-handshake");
    if !no_handshake {
        handshake_with_agent();
    }
    rc_platform_win::register_all();
    // P2P 打洞要用信令注册时的本地 UDP 端口；探测套接字在后台进程手里时经管道向它租用
    rc_appcore::p2p_port_lease::register_hook();
    let shell = ui::shell::Shell::new()?;
    let mut app = app::App::new(&shell)?;
    let _ = shell.show_and_run(|| app.dispatch());
    Ok(())
}

fn main() {
    // 内部事件日志：设了环境变量 RC_TRACE 才生效（诊断用）
    rc_appcore::trace_log::init_to_stderr("RC_TRACE");
    // 无界面连接诊断：在初始化 XAML 之前处理（它不创建窗口）。
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--connect-probe") {
        std::process::exit(connect_probe::run(&args));
    }
    // M6：--version 供打包脚本读取版本（XAML 初始化前退出）
    if std::env::args()
        .skip(1)
        .any(|arg| arg == "--version" || arg == "-V")
    {
        println!("rabbitcontrol-win {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let result = unsafe {
        windows::Win32::System::WinRT::RoInitialize(
            windows::Win32::System::WinRT::RO_INIT_SINGLETHREADED,
        )
    }
    .map_err(Box::<dyn Error>::from)
    .and_then(|()| {
        let result = run();
        unsafe { windows::Win32::System::WinRT::RoUninitialize() };
        result
    });
    if let Err(error) = result {
        let message = format!("Could not start RabbitControl.\n\n{error}");
        eprintln!("{message}");
        unsafe { MessageBoxW(None, message.as_str(), "RabbitControl", MB_ICONERROR) };
        std::process::exit(1);
    }
}
