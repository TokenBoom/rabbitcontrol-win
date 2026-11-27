//! Windows Service 集成（M5b.1）：SCM 分发、状态上报、停止信号、会话监管。
//!
//! - `run_service`：`StartServiceCtrlDispatcherW` 进入服务主循环；
//! - 控制处理器只接受 STOP；服务内两个线程：管道 IPC 服务端 + 会话监管
//!   （轮询活动控制台会话，`CreateProcessAsUserW` 拉起 agent，注销回收）；
//! - 停止：控制处理器置停止标志 → 管道线程退出 → 主线程上报 STOPPED。

#![allow(unsafe_code)] // SCM / WTS / 进程创建 FFI 边界

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, ERROR_SERVICE_DOES_NOT_EXIST, HANDLE};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::Services::*;
use windows::Win32::System::Threading::CreateProcessAsUserW;

use crate::pipe::PipeServer;
use crate::protocol::{parse_request, render_response, Request, Response};

/// 服务注册名（SCM 键）。
pub const SERVICE_NAME: &str = "RabbitControlDaemon";
/// 服务显示名。
pub const SERVICE_DISPLAY: &str = "RabbitControl 后台服务";
/// 服务描述。
pub const SERVICE_DESC: &str = "RabbitControl 被控提权代理与 agent 监管（LocalSystem）";
/// agent 可执行名（与 rc-daemon 同目录）。
pub const AGENT_EXE: &str = "rabbitcontrol-win.exe";

/// 全局停止标志（控制处理器 / 管道线程共享）。
static STOP: AtomicBool = AtomicBool::new(false);
/// 服务状态句柄（SetServiceStatus 用；服务生命周期内有效）。
static STATUS_HANDLE: std::sync::OnceLock<isize> = std::sync::OnceLock::new();

/// SCM 服务主入口（子命令 `run`）。
pub fn run_service() -> Result<(), String> {
    let mut name = crate::pipe::encode_wide(SERVICE_NAME);
    let mut table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: windows::core::PWSTR(name.as_mut_ptr()),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    let ok = unsafe { StartServiceCtrlDispatcherW(table.as_mut_ptr()) };
    ok.map_err(|e| format!("服务分发失败（请以服务方式由 SCM 启动，勿直接运行）：{e}"))
}

/// 服务主函数（SCM 线程调用）。
unsafe extern "system" fn service_main(_argc: u32, _argv: *mut windows::core::PWSTR) {
    let Ok(handler) = RegisterServiceCtrlHandlerExW(
        PCWSTR(crate::pipe::encode_wide(SERVICE_NAME).as_ptr()),
        Some(control_handler),
        None,
    ) else {
        return;
    };
    STATUS_HANDLE.set(handler.0 as isize).ok();
    report_status(SERVICE_START_PENDING, 0);
    // 平台钩子（后续 host 能力下沉 daemon 时直接可用；当前无副作用）
    rc_platform_win::register_all();

    let stop_flag = Arc::new(AtomicBool::new(false));
    let pipe_thread = std::thread::Builder::new()
        .name("rc-daemon-pipe".into())
        .spawn({
            let stop_flag = Arc::clone(&stop_flag);
            move || pipe_serve_loop(&stop_flag)
        });
    let supervisor_thread = std::thread::Builder::new()
        .name("rc-daemon-supervisor".into())
        .spawn({
            let stop_flag = Arc::clone(&stop_flag);
            move || supervise_sessions(&stop_flag)
        });

    report_status(SERVICE_RUNNING, SERVICE_ACCEPT_STOP);

    // 主线程等待停止标志（控制处理器置位或管道软停通道）
    loop {
        if STOP.load(Ordering::SeqCst) {
            stop_flag.store(true, Ordering::SeqCst);
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }

    report_status(SERVICE_STOP_PENDING, 3000);
    if let Ok(t) = pipe_thread {
        let _ = t.join();
    }
    if let Ok(t) = supervisor_thread {
        let _ = t.join();
    }
    report_status(SERVICE_STOPPED, 0);
}

/// SCM 控制处理器：仅接受 STOP。
unsafe extern "system" fn control_handler(
    control: u32,
    _event_type: u32,
    _event_data: *mut core::ffi::c_void,
    _context: *mut core::ffi::c_void,
) -> u32 {
    if control == SERVICE_CONTROL_STOP {
        STOP.store(true, Ordering::SeqCst);
        return windows::Win32::Foundation::NO_ERROR.0;
    }
    windows::Win32::Foundation::ERROR_CALL_NOT_IMPLEMENTED.0
}

/// 上报服务状态。
fn report_status(state: SERVICE_STATUS_CURRENT_STATE, wait_hint_ms: u32) {
    let Some(raw) = STATUS_HANDLE.get() else {
        return;
    };
    let handle = SERVICE_STATUS_HANDLE(*raw as *mut core::ffi::c_void);
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING {
            SERVICE_ACCEPT_STOP
        } else {
            0
        },
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: wait_hint_ms,
    };
    unsafe {
        let _ = SetServiceStatus(handle, &status);
    }
}

/// 管道 IPC 服务循环（软停通道：`stop-daemon` 请求置 STOP）。
fn pipe_serve_loop(stop: &AtomicBool) {
    let server = match PipeServer::new() {
        Ok(server) => server,
        Err(err) => {
            eprintln!("[rc-daemon] 管道创建失败：{err}");
            return;
        }
    };
    while !stop.load(Ordering::SeqCst) && !STOP.load(Ordering::SeqCst) {
        if server.wait_client(stop).is_none() {
            break;
        }
        let frame = match server.read_frame() {
            Ok(frame) => frame,
            Err(_) => {
                server.finish_session();
                continue;
            }
        };
        let response = match parse_request(frame.trim_end_matches(['\n', '\r'])) {
            Ok(Request::Ping) => {
                Response::ok_json(format!("{{\"version\":\"{}\"}}", env!("CARGO_PKG_VERSION")))
            }
            Ok(Request::Inject { payload }) => crate::inject::inject_proxy_payload(&payload),
            Ok(Request::Status) => Response::ok_json(format!(
                "{{\"version\":\"{}\",\"agent_managed\":true,\"uptime_sec\":{}}}",
                env!("CARGO_PKG_VERSION"),
                uptime_sec()
            )),
            Ok(Request::StopDaemon) => {
                STOP.store(true, Ordering::SeqCst);
                Response::ok_json("{}".to_string())
            }
            Err(err) => Response::err(err),
        };
        let _ = server.write_frame(&render_response(&response));
        while server.has_pending() {
            let _ = server.read_frame();
        }
        server.finish_session();
    }
}

/// 进程启动时刻（服务运行时长基准）。
fn start_instant() -> std::time::Instant {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    *START.get_or_init(std::time::Instant::now)
}

fn uptime_sec() -> u64 {
    start_instant().elapsed().as_secs()
}

/// 会话监管轮询周期。
const SUPERVISE_INTERVAL: Duration = Duration::from_secs(5);
/// 受管 agent 进程句柄（会话注销时终止；isize 便于跨线程，0 = 无）。
static AGENT_PROCESS: std::sync::Mutex<Option<isize>> = std::sync::Mutex::new(None);

/// 活动控制台会话监管：有会话 → 拉起 agent；会话消失 → 终止 agent。
fn supervise_sessions(stop: &AtomicBool) {
    loop {
        if stop.load(Ordering::SeqCst) {
            terminate_agent();
            return;
        }
        let session_id = unsafe { WTSGetActiveConsoleSessionId() };
        if session_id == 0xFFFF_FFFF {
            // 无活动控制台会话（注销中）
            terminate_agent();
        } else {
            ensure_agent(session_id);
        }
        std::thread::sleep(SUPERVISE_INTERVAL);
    }
}

/// 确保活动会话内有 agent 在跑（已有则跳过）。
fn ensure_agent(session_id: u32) {
    {
        let guard = AGENT_PROCESS.lock().expect("agent 锁");
        if guard.is_some() {
            return;
        }
    }
    unsafe {
        let mut token = HANDLE::default();
        if WTSQueryUserToken(session_id, &mut token).is_err() {
            return; // 会话未登录完成 / 系统服务会话
        }
        let mut env = std::ptr::null_mut();
        let has_env = CreateEnvironmentBlock(&mut env, token, false).is_ok();
        let exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join(AGENT_EXE)))
            .filter(|p| p.exists());
        let Some(exe_path) = exe else {
            let _ = CloseHandle(token);
            if has_env {
                let _ = DestroyEnvironmentBlock(env);
            }
            return; // agent 未随包部署
        };
        let mut command = crate::pipe::encode_wide(&format!("\"{}\"", exe_path.display()));
        let mut desktop = crate::pipe::encode_wide("winsta0\\default");
        let startup = windows::Win32::System::Threading::STARTUPINFOW {
            cb: std::mem::size_of::<windows::Win32::System::Threading::STARTUPINFOW>() as u32,
            lpDesktop: windows::core::PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };
        let mut process_info = windows::Win32::System::Threading::PROCESS_INFORMATION::default();
        let ok = CreateProcessAsUserW(
            token,
            PCWSTR::null(),
            windows::core::PWSTR(command.as_mut_ptr()),
            None,
            None,
            false,
            windows::Win32::System::Threading::CREATE_NO_WINDOW
                | windows::Win32::System::Threading::CREATE_UNICODE_ENVIRONMENT,
            if has_env { Some(env) } else { None },
            None,
            &startup,
            &mut process_info,
        );
        let _ = DestroyEnvironmentBlock(env);
        let _ = CloseHandle(token);
        if ok.is_ok() {
            let _ = CloseHandle(process_info.hThread);
            *AGENT_PROCESS.lock().expect("agent 锁") = Some(process_info.hProcess.0 as isize);
        }
    }
}

/// 终止受管 agent（会话注销 / 服务停止）。
fn terminate_agent() {
    let handle = AGENT_PROCESS.lock().expect("agent 锁").take();
    if let Some(raw) = handle {
        let handle = HANDLE(raw as *mut core::ffi::c_void);
        unsafe {
            let _ = windows::Win32::System::Threading::TerminateProcess(handle, 0);
            let _ = CloseHandle(handle);
        }
    }
}

// ── SCM CLI（install/uninstall/start/stop）──

/// 安装服务（AUTO_START；需管理员）。
pub fn install() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("定位自身失败：{e}"))?;
    let bin = crate::pipe::encode_wide(&format!("\"{}\" run", exe.display()));
    unsafe {
        let scm = OpenSCManagerW(None, None, SC_MANAGER_CREATE_SERVICE)
            .map_err(|e| format!("打开 SCM 失败（需要管理员权限）：{e}"))?;
        let name = crate::pipe::encode_wide(SERVICE_NAME);
        let display = crate::pipe::encode_wide(SERVICE_DISPLAY);
        let service = CreateServiceW(
            scm,
            PCWSTR(name.as_ptr()),
            PCWSTR(display.as_ptr()),
            SERVICE_QUERY_STATUS
                | SERVICE_START
                | SERVICE_STOP
                | windows::Win32::Storage::FileSystem::DELETE.0,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            PCWSTR(bin.as_ptr()),
            None,
            None,
            None,
            None,
            None,
        );
        let _ = CloseServiceHandle(scm);
        let service = service.map_err(|e| match e.code() {
            code if code == windows::Win32::Foundation::ERROR_SERVICE_EXISTS.to_hresult() => {
                "服务已安装".to_string()
            }
            code if code == windows::Win32::Foundation::ERROR_ACCESS_DENIED.to_hresult() => {
                "拒绝访问：请以管理员身份运行".to_string()
            }
            code => format!("创建服务失败：{code}"),
        })?;
        // 描述
        let desc = crate::pipe::encode_wide(SERVICE_DESC);
        let mut desc_union = SERVICE_DESCRIPTIONW {
            lpDescription: windows::core::PWSTR(desc.as_ptr() as *mut u16),
        };
        let _ = ChangeServiceConfig2W(
            service,
            SERVICE_CONFIG_DESCRIPTION,
            Some(&mut desc_union as *mut SERVICE_DESCRIPTIONW as *const core::ffi::c_void),
        );
        let _ = CloseServiceHandle(service);
    }
    if let Err(err) = crate::firewall::register() {
        eprintln!("[rc-daemon] 防火墙规则注册失败：{err}");
    }
    Ok(())
}

/// 卸载服务（先停后删）。
pub fn uninstall() -> Result<(), String> {
    unsafe {
        let scm = OpenSCManagerW(None, None, SC_MANAGER_ALL_ACCESS)
            .map_err(|e| format!("打开 SCM 失败（需要管理员权限）：{e}"))?;
        let name = crate::pipe::encode_wide(SERVICE_NAME);
        let service = OpenServiceW(
            scm,
            PCWSTR(name.as_ptr()),
            SERVICE_QUERY_STATUS
                | SERVICE_START
                | SERVICE_STOP
                | windows::Win32::Storage::FileSystem::DELETE.0,
        )
        .map_err(|e| {
            if e.code() == ERROR_SERVICE_DOES_NOT_EXIST.to_hresult() {
                "服务未安装".to_string()
            } else {
                format!("打开服务失败：{e}")
            }
        });
        let _ = CloseServiceHandle(scm);
        let service = service?;
        let mut status = SERVICE_STATUS::default();
        let _ = ControlService(service, SERVICE_CONTROL_STOP, &mut status);
        let deleted = DeleteService(service).is_ok();
        let _ = CloseServiceHandle(service);
        if deleted {
            Ok(())
        } else {
            Err("删除服务失败（可能仍在停止中，稍后重试）".into())
        }
    }
}

/// 启动 / 停止服务。
pub fn control(start: bool) -> Result<(), String> {
    unsafe {
        let scm = OpenSCManagerW(None, None, SC_MANAGER_ALL_ACCESS)
            .map_err(|e| format!("打开 SCM 失败（需要管理员权限）：{e}"))?;
        let name = crate::pipe::encode_wide(SERVICE_NAME);
        let opened = OpenServiceW(
            scm,
            PCWSTR(name.as_ptr()),
            SERVICE_QUERY_STATUS | SERVICE_START | SERVICE_STOP,
        );
        let _ = CloseServiceHandle(scm);
        let service = opened.map_err(|e| format!("打开服务失败（未安装？）：{e}"))?;
        let outcome = if start {
            StartServiceW(service, None).map_err(|e| e.to_string())
        } else {
            let mut status = SERVICE_STATUS::default();
            ControlService(service, SERVICE_CONTROL_STOP, &mut status).map_err(|e| e.to_string())
        };
        let _ = CloseServiceHandle(service);
        outcome
    }
}

/// 一键安装并启动（GUI UAC 提权单次调用入口）。
pub fn setup() -> Result<(), String> {
    install()?;
    control(true)?;
    if let Err(err) = crate::firewall::register() {
        eprintln!("[rc-daemon] 防火墙规则注册失败：{err}");
    }
    // 就绪探测：管道可达即成功（最长 ~5s）
    for _ in 0..10 {
        if crate::pipe::call(r#"{"op":"ping"}"#).is_ok() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Err("服务已启动但管道未就绪".into())
}

/// 一键停止并卸载（GUI UAC 提权单次调用入口）。
pub fn teardown() -> Result<(), String> {
    uninstall()
}

/// CLI status：优先查 SCM；SCM 不可用时尝试管道 Ping（区分服务未装 vs 停止）。
pub fn status() -> Result<(), String> {
    unsafe {
        let scm = OpenSCManagerW(None, None, SERVICE_QUERY_STATUS);
        if let Ok(scm) = scm {
            let name = crate::pipe::encode_wide(SERVICE_NAME);
            let service = OpenServiceW(scm, PCWSTR(name.as_ptr()), SERVICE_QUERY_STATUS);
            let _ = CloseServiceHandle(scm);
            if let Ok(service) = service {
                let mut status = SERVICE_STATUS::default();
                let ok = QueryServiceStatus(service, &mut status);
                let _ = CloseServiceHandle(service);
                if ok.is_ok() {
                    let state = match status.dwCurrentState {
                        SERVICE_RUNNING => "运行中",
                        SERVICE_STOPPED => "已停止",
                        SERVICE_START_PENDING => "启动中",
                        SERVICE_STOP_PENDING => "停止中",
                        _ => "未知",
                    };
                    println!("服务 {SERVICE_NAME}：{state}");
                    return Ok(());
                }
            }
        }
    }
    match crate::pipe::call(r#"{"op":"ping"}"#) {
        Ok(reply) => println!("管道探测成功：{reply}"),
        Err(err) => println!("服务未运行：{err}"),
    }
    Ok(())
}
