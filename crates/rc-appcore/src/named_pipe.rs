//! 命名管道（本机进程间通信）：服务端骨架 + 带超时的客户端。
//!
//! `rc-daemon`（守护服务）与 `rc-host-agent`（后台被控进程）共用，避免各写一份。
//! 行分隔 JSON 帧（UTF-8），MESSAGE 模式下一次读就是一帧。
//!
//! 服务端是单线程循环：等客户端 → 读一帧 → 调用方处理并回写 → 断开 → 继续等。
//! 等待期间以固定间隔检查停止标志，不会被阻塞住退不出来。
//! 客户端每次调用「打开 → 写 → 读 → 关闭」，读写都带超时：对端卡住时调用方
//! 不会被挂住。

#![allow(unsafe_code)] // Win32 管道 FFI 边界

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
    HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_FIRST_PIPE_INSTANCE,
    FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_NONE, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, WaitNamedPipeW, PIPE_READMODE_MESSAGE,
    PIPE_TYPE_MESSAGE, PIPE_WAIT,
};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcessId, ResetEvent, WaitForSingleObject,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

/// 等待客户端连入时检查停止标志的间隔（毫秒）。
const WAIT_POLL_MS: u32 = 200;
/// 客户端遇到「管道忙」时的最多重试次数。
const BUSY_RETRIES: u32 = 4;
/// 写完应答后等客户端关闭连接的最长时间（毫秒）。
const CLIENT_CLOSE_WAIT_MS: u32 = 3_000;

/// 管道调用失败的种类。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipeError {
    /// 管道不存在：对端进程没在运行。
    NotFound,
    /// 对端在限定时间内没有完成这一步。
    TimedOut(&'static str),
    /// 打开或读写出错。
    Io(String),
}

impl fmt::Display for PipeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PipeError::NotFound => write!(f, "对端进程未运行"),
            PipeError::TimedOut(what) => write!(f, "{what}超时"),
            PipeError::Io(message) => write!(f, "{message}"),
        }
    }
}

/// 服务端管道实例。
pub struct PipeServer {
    handle: HANDLE,
    connect_event: HANDLE,
    buffer: Vec<u8>,
}

impl PipeServer {
    /// 创建监听管道实例（最多 4 个实例；缓冲区 = `max_frame_bytes`）。
    ///
    /// 带首个实例标志：同名管道已存在时直接失败——服务端不加入别人抢先创建的
    /// 同名管道，免得被冒充。`security` 为 `None` 时用进程默认 DACL。
    pub fn create(
        name: &str,
        max_frame_bytes: usize,
        security: Option<*const SECURITY_ATTRIBUTES>,
    ) -> windows::core::Result<Self> {
        let name = encode_wide(name);
        let connect_event = unsafe { CreateEventW(None, true, false, None)? };
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
                4,
                max_frame_bytes as u32,
                max_frame_bytes as u32,
                0,
                security,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            let err = windows::core::Error::from_win32();
            unsafe {
                let _ = CloseHandle(connect_event);
            }
            return Err(err);
        }
        Ok(Self {
            handle,
            connect_event,
            buffer: vec![0u8; max_frame_bytes],
        })
    }

    /// 底层句柄（供调用方校验对端进程身份）。
    pub fn raw_handle(&self) -> HANDLE {
        self.handle
    }

    /// 等一个客户端连入；停止标志置位或管道出错时返回 `false`。
    pub fn wait_client(&self, stop: &AtomicBool) -> bool {
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        let mut overlapped = OVERLAPPED {
            hEvent: self.connect_event,
            ..Default::default()
        };
        let connected = unsafe { ConnectNamedPipe(self.handle, Some(&mut overlapped)) };
        let pending = match connected {
            Ok(()) => false,
            // ERROR_IO_PENDING(997) = 连入中；ERROR_PIPE_CONNECTED(536) = 先于调用已连入
            Err(err) if err.code() == ERROR_IO_PENDING.to_hresult() => true,
            Err(err) if err.code() == ERROR_PIPE_CONNECTED.to_hresult() => false,
            Err(_) => return false,
        };
        if pending {
            loop {
                if stop.load(Ordering::SeqCst) {
                    return false;
                }
                match unsafe { WaitForSingleObject(self.connect_event, WAIT_POLL_MS) } {
                    WAIT_OBJECT_0 => break,
                    WAIT_TIMEOUT => continue,
                    _ => return false,
                }
            }
            let mut transferred = 0u32;
            if unsafe { GetOverlappedResult(self.handle, &overlapped, &mut transferred, false) }
                .is_err()
            {
                return false;
            }
        }
        true
    }

    /// 单帧上限（字节）。
    fn max_frame_bytes(&self) -> usize {
        self.buffer.len()
    }

    /// 读一帧请求（MESSAGE 模式单次读齐）。
    pub fn read_frame(&self) -> windows::core::Result<String> {
        let mut buffer = vec![0u8; self.max_frame_bytes()];
        let mut read = 0u32;
        unsafe {
            ReadFile(self.handle, Some(&mut buffer), Some(&mut read), None)?;
        }
        buffer.truncate(read as usize);
        Ok(String::from_utf8_lossy(&buffer).into_owned())
    }

    /// 写一帧响应（追加 `\n` 结尾）。
    pub fn write_frame(&self, frame: &str) -> windows::core::Result<()> {
        let mut data = frame.as_bytes().to_vec();
        data.push(b'\n');
        let mut written = 0u32;
        unsafe { WriteFile(self.handle, Some(&data), Some(&mut written), None).map(|_| ()) }
    }

    /// 结束本轮会话，准备接受下一个客户端。
    ///
    /// 断开前先等客户端自己关闭连接（最多 [`CLIENT_CLOSE_WAIT_MS`]）：管道断开时
    /// 客户端还没读走的数据会被系统直接丢弃——应答刚写完就断开的话，读得慢
    /// 一点的客户端会读到空。
    pub fn finish_session(&self) {
        self.wait_client_closed();
        unsafe {
            let _ = DisconnectNamedPipe(self.handle);
        }
    }

    /// 等客户端关闭连接（或超时）。对端关闭后，挂在管道上的读会以失败结束。
    fn wait_client_closed(&self) {
        let Ok(event) = (unsafe { CreateEventW(None, true, false, None) }) else {
            return;
        };
        let mut scratch = [0u8; 256];
        let mut overlapped = OVERLAPPED {
            hEvent: event,
            ..Default::default()
        };
        let mut read = 0u32;
        let pending = match unsafe {
            ReadFile(
                self.handle,
                Some(&mut scratch),
                Some(&mut read),
                Some(&mut overlapped),
            )
        } {
            // 客户端发来了多余数据：按本轮已结束处理
            Ok(()) => false,
            Err(err) if err.code() == ERROR_IO_PENDING.to_hresult() => true,
            // 对端已关闭（ERROR_BROKEN_PIPE / ERROR_NO_DATA 等）
            Err(_) => false,
        };
        if pending {
            let done = unsafe { WaitForSingleObject(event, CLIENT_CLOSE_WAIT_MS) } == WAIT_OBJECT_0;
            if !done {
                // 超时：取消挂起的读，并等它真正结束（缓冲区还在栈上）
                unsafe {
                    let _ = CancelIoEx(self.handle, None);
                    let mut leftover = 0u32;
                    let _ = GetOverlappedResult(self.handle, &overlapped, &mut leftover, true);
                }
            }
        }
        unsafe {
            let _ = CloseHandle(event);
        }
    }
}

impl Drop for PipeServer {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
            let _ = CloseHandle(self.connect_event);
        }
    }
}

/// RAII 关闭内核句柄。
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// UTF-16 编码（含 NUL 终止）。
pub fn encode_wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 当前进程所在的 Windows 会话号（取不到时按 0 处理）。
///
/// 快速用户切换时每个会话各有自己的后台进程，管道名带上会话号就不会串。
pub fn current_session_id() -> u32 {
    let mut session = 0u32;
    unsafe {
        let _ = ProcessIdToSessionId(GetCurrentProcessId(), &mut session);
    }
    session
}

/// 客户端：打开管道 → 写一帧 → 读一帧 → 关闭（每一步 ≤ `timeout_ms` 毫秒）。
pub fn round_trip(
    pipe: &str,
    frame: &str,
    max_frame_bytes: usize,
    timeout_ms: u32,
) -> Result<String, PipeError> {
    let name = encode_wide(pipe);
    // 服务端正忙着处理别的客户端时管道实例全部被占（ERROR_PIPE_BUSY）：
    // 稍等重试几次，而不是把「忙」当成「没在运行」
    let mut attempts = 0u32;
    let handle = loop {
        let opened = unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED as FILE_FLAGS_AND_ATTRIBUTES,
                None,
            )
        };
        match opened {
            Ok(handle) => break handle,
            Err(err) if err.code() == ERROR_PIPE_BUSY.to_hresult() && attempts < BUSY_RETRIES => {
                attempts += 1;
                unsafe {
                    let _ = WaitNamedPipeW(PCWSTR(name.as_ptr()), timeout_ms.min(2_000));
                }
            }
            Err(err) if err.code() == ERROR_FILE_NOT_FOUND.to_hresult() => {
                return Err(PipeError::NotFound);
            }
            Err(err) => return Err(PipeError::Io(format!("连接管道失败：{err}"))),
        }
    };
    let handle = OwnedHandle(handle);
    let event = OwnedHandle(
        unsafe { CreateEventW(None, true, false, None) }
            .map_err(|err| PipeError::Io(format!("创建事件失败：{err}")))?,
    );

    let mut data = frame.as_bytes().to_vec();
    data.push(b'\n');
    run_io(
        &handle,
        &event,
        timeout_ms,
        "发送请求",
        |overlapped| unsafe {
            let mut written = 0u32;
            WriteFile(handle.0, Some(&data), Some(&mut written), Some(overlapped))
        },
    )?;

    let mut buffer = vec![0u8; max_frame_bytes];
    let read = run_io(
        &handle,
        &event,
        timeout_ms,
        "读取响应",
        |overlapped| unsafe {
            let mut got = 0u32;
            ReadFile(
                handle.0,
                Some(&mut buffer),
                Some(&mut got),
                Some(overlapped),
            )
        },
    )?;
    if read as usize > max_frame_bytes {
        return Err(PipeError::Io("对端响应过长".to_string()));
    }
    let text = String::from_utf8_lossy(&buffer[..read as usize]).into_owned();
    Ok(text.trim_end_matches(['\n', '\r']).to_string())
}

/// 一次 overlapped I/O：立即完成则直接返回，否则等到事件触发；超时则取消。
fn run_io(
    handle: &OwnedHandle,
    event: &OwnedHandle,
    timeout_ms: u32,
    what: &'static str,
    io: impl FnOnce(*mut OVERLAPPED) -> windows::core::Result<()>,
) -> Result<u32, PipeError> {
    unsafe {
        let _ = ResetEvent(event.0);
    }
    let mut overlapped = OVERLAPPED {
        hEvent: event.0,
        ..Default::default()
    };
    let pending = match io(&mut overlapped) {
        Ok(()) => false,
        // ERROR_IO_PENDING(997)：已提交，等待事件
        Err(err) if err.code() == ERROR_IO_PENDING.to_hresult() => true,
        Err(err) => return Err(PipeError::Io(format!("{what}失败：{err}"))),
    };
    if pending {
        let wait = unsafe { WaitForSingleObject(event.0, timeout_ms) };
        if wait != WAIT_OBJECT_0 {
            // 取消后必须等到操作真正结束，缓冲区才能释放
            unsafe {
                let _ = CancelIoEx(handle.0, None);
                let mut leftover = 0u32;
                let _ = GetOverlappedResult(handle.0, &overlapped, &mut leftover, true);
            }
            return Err(PipeError::TimedOut(what));
        }
    }
    let mut transferred = 0u32;
    unsafe {
        GetOverlappedResult(handle.0, &overlapped, &mut transferred, false)
            .map_err(|err| PipeError::Io(format!("{what}失败：{err}")))?;
    }
    Ok(transferred)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_strings_are_nul_terminated() {
        assert_eq!(encode_wide("ab"), vec![97, 98, 0]);
        assert_eq!(encode_wide("").len(), 1);
    }

    #[test]
    fn wait_client_honors_an_already_set_stop_flag() {
        // 停止标志已置位：立即返回，不等待任何客户端（也不会挂住测试）
        let name = format!(r"\\.\pipe\rabbitcontrol-test-{}", std::process::id());
        let server = PipeServer::create(&name, 4096, None).expect("创建管道实例");
        let stop = AtomicBool::new(true);
        let started = std::time::Instant::now();
        assert!(!server.wait_client(&stop));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn a_taken_pipe_name_is_not_joined() {
        // 同名管道已经存在：带「首个实例」标志的创建必须失败，而不是加入它
        let name = format!(r"\\.\pipe\rabbitcontrol-test-first-{}", std::process::id());
        let first = PipeServer::create(&name, 4096, None).expect("首个实例");
        let second = PipeServer::create(&name, 4096, None);
        assert!(second.is_err(), "同名管道已存在时不应创建成功");
        drop(first);
        // 原实例释放后可以重新创建
        PipeServer::create(&name, 4096, None).expect("释放后重新创建");
    }

    #[test]
    fn error_texts_are_user_facing() {
        assert_eq!(PipeError::NotFound.to_string(), "对端进程未运行");
        assert_eq!(PipeError::TimedOut("读取响应").to_string(), "读取响应超时");
        assert_eq!(PipeError::Io("x".to_string()).to_string(), "x");
    }
}
