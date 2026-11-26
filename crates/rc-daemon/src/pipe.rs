//! 命名管道：服务端（守护进程）与客户端（CLI/agent 状态查询）。
//!
//! 服务端：overlapped `ConnectNamedPipe` + 500ms 轮询停止标志，阻塞不阻碍
//! 退出；单请求-单响应后 `DisconnectNamedPipe` 继续等下一个客户端。

#![allow(unsafe_code)] // Win32 管道 FFI 边界

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, FILE_SHARE_NONE, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PeekNamedPipe, PIPE_READMODE_MESSAGE,
    PIPE_TYPE_MESSAGE, PIPE_WAIT,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::IO::OVERLAPPED;

use crate::protocol::{MAX_FRAME_BYTES, PIPE_NAME};

/// 客户端连接等待间隔（毫秒；停止标志轮询粒度）。
const WAIT_POLL_MS: u32 = 500;

/// 服务端管道句柄封装。
pub struct PipeServer {
    handle: HANDLE,
    connect_event: HANDLE,
}

impl PipeServer {
    /// 创建监听管道实例（单实例复用：accept-处理-再 accept 循环）。
    pub fn new() -> windows::core::Result<Self> {
        let name = encode_wide(PIPE_NAME);
        let connect_event = unsafe { CreateEventW(None, true, false, None)? };
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
                4,
                MAX_FRAME_BYTES as u32,
                MAX_FRAME_BYTES as u32,
                0,
                None,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(windows::core::Error::from_win32());
        }
        Ok(Self {
            handle,
            connect_event,
        })
    }

    /// 等待一个客户端连入（单次尝试）；`stop` 置位时返回 `None`（500ms 粒度退出，
    /// 服务端外层循环反复调用本方法实现轮询）。
    pub fn wait_client(&self, stop: &std::sync::atomic::AtomicBool) -> Option<()> {
        use std::sync::atomic::Ordering;
        if stop.load(Ordering::SeqCst) {
            return None;
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
            Err(_) => return None,
        };
        if pending {
            loop {
                if stop.load(Ordering::SeqCst) {
                    return None;
                }
                match unsafe { WaitForSingleObject(self.connect_event, WAIT_POLL_MS) } {
                    WAIT_OBJECT_0 => break,
                    WAIT_TIMEOUT => continue,
                    _ => return None,
                }
            }
            let mut transferred = 0u32;
            if unsafe {
                windows::Win32::System::IO::GetOverlappedResult(
                    self.handle,
                    &overlapped,
                    &mut transferred,
                    false,
                )
            }
            .is_err()
            {
                return None;
            }
        }
        Some(())
    }

    /// 读一帧请求（MESSAGE 模式单次读齐；超长帧截断报错）。
    pub fn read_frame(&self) -> windows::core::Result<String> {
        let mut buf = [0u8; MAX_FRAME_BYTES];
        let mut read = 0u32;
        unsafe { ReadFile(self.handle, Some(&mut buf), Some(&mut read), None) }?;
        if read as usize > MAX_FRAME_BYTES {
            return Err(windows::core::Error::from_win32());
        }
        Ok(String::from_utf8_lossy(&buf[..read as usize]).into_owned())
    }

    /// 写一帧响应（含 `\n` 结尾）。
    pub fn write_frame(&self, frame: &str) -> windows::core::Result<()> {
        let mut data = frame.as_bytes().to_vec();
        data.push(b'\n');
        let mut written = 0u32;
        unsafe { WriteFile(self.handle, Some(&data), Some(&mut written), None) }.map(|_| ())
    }

    /// 结束本轮会话，继续接受下一客户端。
    pub fn finish_session(&self) {
        unsafe {
            let _ = DisconnectNamedPipe(self.handle);
        }
    }

    /// 管道是否仍有未读取数据（退出前丢弃）。
    pub fn has_pending(&self) -> bool {
        let mut available = 0u32;
        unsafe {
            let _ = PeekNamedPipe(self.handle, None, 0, None, Some(&mut available), None);
        }
        available > 0
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

/// 打开管道客户端连接（CLI status/ping 用）。
pub fn connect_client() -> windows::core::Result<HANDLE> {
    let name = encode_wide(PIPE_NAME);
    unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            (windows::Win32::Storage::FileSystem::FILE_GENERIC_READ
                | windows::Win32::Storage::FileSystem::FILE_GENERIC_WRITE)
                .0,
            FILE_SHARE_NONE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            None,
        )
    }
}

/// 客户端一次请求-响应（打开 → 写 → 读 → 关闭）。
pub fn call(frame: &str) -> Result<String, String> {
    let handle =
        connect_client().map_err(|e| format!("连接守护管道失败（服务未运行或权限不足）：{e}"))?;
    let mut data = frame.as_bytes().to_vec();
    data.push(b'\n');
    let mut written = 0u32;
    let write_ok = unsafe {
        windows::Win32::Storage::FileSystem::WriteFile(
            handle,
            Some(&data),
            Some(&mut written),
            None,
        )
    };
    if write_ok.is_err() {
        unsafe {
            let _ = CloseHandle(handle);
        }
        return Err("请求写入失败".into());
    }
    let mut buf = [0u8; MAX_FRAME_BYTES];
    let mut read = 0u32;
    let read_ok = unsafe { ReadFile(handle, Some(&mut buf), Some(&mut read), None) };
    let _ = unsafe { CloseHandle(handle) };
    if read_ok.is_err() {
        return Err("响应读取失败".into());
    }
    Ok(String::from_utf8_lossy(&buf[..read as usize]).into_owned())
}

/// UTF-16 编码（含 NUL 终止）。
pub(crate) fn encode_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
