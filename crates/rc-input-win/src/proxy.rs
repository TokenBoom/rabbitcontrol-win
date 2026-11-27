//! UIPI 拒注入降级：SendInput 被安全桌面拦截时，把事件转发给
//! `rc-daemon`（LocalSystem）经命名管道代理注入。
//!
//! # 触发条件
//!
//! 普通用户进程的 `SendInput` 在 UAC 安全桌面 / 锁屏 / 提权窗口焦点下
//! 会被 UIPI 静默拦截（返回成功但目标收不到，或部分注入失败）。本模块
//! 在注入计数不足时把事件序列编码后发往守护管道。
//!
//! # 协议
//!
//! 复用 `rc-daemon` 的行分隔 JSON：`{"op":"inject","events":[…线格式Base64…]}`，
//! 事件负载用 `rc_input` 的线格式（与网络传输同编码），daemon 解码后用
//! 它自己的 SendInput（LocalSystem 令牌，可穿透 UIPI）注入。

#![allow(unsafe_code)] // 命名管道 FFI

use std::time::Duration;

use rc_input::{decode_events, encode_events, InputEvent};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_SHARE_NONE, OPEN_EXISTING,
};

/// 守护管道名称（与 rc-daemon `protocol::PIPE_NAME` 一致）。
const PIPE_NAME: &str = "\\\\.\\pipe\\rabbitcontrol-daemon";
/// 连接超时内的重试次数（daemon 忙时管道 busy）。
const CONNECT_RETRIES: u32 = 3;
/// 每次重试间隔。
const RETRY_INTERVAL: Duration = Duration::from_millis(30);

/// 尝试经守护管道代理注入一批事件。
///
/// 返回 `Ok(true)` = daemon 已接收；`Ok(false)` = daemon 不可达（调用方
/// 保留原错误）；`Err` = 编码或协议错误。
pub fn proxy_inject(events: &[InputEvent]) -> Result<bool, String> {
    if events.is_empty() {
        return Ok(true);
    }
    // 线格式编码（与网络传输同格式）→ Base64 装进 JSON
    let mut wire = Vec::new();
    encode_events(events, &mut wire);
    let b64 = base64_encode(&wire);
    let request = format!("{{\"op\":\"inject\",\"payload\":\"{b64}\"}}");
    match pipe_roundtrip(&request) {
        Some(reply) => {
            // daemon 回 {"ok":true} 视为接收
            Ok(reply.contains("\"ok\":true"))
        }
        None => Ok(false),
    }
}

/// 打开守护管道并一次请求-响应；不可达返回 `None`。
fn pipe_roundtrip(request: &str) -> Option<String> {
    let name: Vec<u16> = PIPE_NAME.encode_utf16().chain(std::iter::once(0)).collect();
    for attempt in 0..CONNECT_RETRIES {
        let handle: HANDLE = unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                (windows::Win32::Storage::FileSystem::FILE_GENERIC_READ
                    | windows::Win32::Storage::FileSystem::FILE_GENERIC_WRITE)
                    .0,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .ok()?;
        let mut data = request.as_bytes().to_vec();
        data.push(b'\n');
        let mut written = 0u32;
        let write_ok = unsafe { WriteFile(handle, Some(&data), Some(&mut written), None) }.is_ok();
        let mut buf = [0u8; 4096];
        let mut read = 0u32;
        let read_ok = unsafe { ReadFile(handle, Some(&mut buf), Some(&mut read), None) }.is_ok();
        let _ = unsafe { CloseHandle(handle) };
        if write_ok && read_ok {
            return Some(String::from_utf8_lossy(&buf[..read as usize]).into_owned());
        }
        if attempt + 1 < CONNECT_RETRIES {
            std::thread::sleep(RETRY_INTERVAL);
        }
    }
    None
}

/// 简易 Base64（标准字母表，无换行）。
fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// 供 daemon 侧解码代理负载（rc-daemon 依赖 rc-input 同版本）。
pub fn decode_proxy_payload(b64: &str) -> Result<Vec<InputEvent>, String> {
    let wire = base64_decode(b64).ok_or("Base64 解码失败")?;
    decode_events(&wire).map_err(|e| format!("事件解码失败：{e}"))
}

/// 简易 Base64 解码。
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    const INVALID: u8 = 255;
    fn val(c: u8) -> u8 {
        match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => INVALID,
        }
    }
    let bytes: Vec<u8> = text
        .bytes()
        .filter(|c| !c.is_ascii_whitespace() && *c != b'=')
        .collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        if chunk.len() < 2 {
            return None;
        }
        let mut n = u32::from(val(chunk[0])) << 18;
        n |= u32::from(val(chunk[1])) << 12;
        if chunk.len() > 2 {
            n |= u32::from(val(chunk[2])) << 6;
        }
        if chunk.len() > 3 {
            n |= u32::from(val(chunk[3]));
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_input::MouseButton;

    #[test]
    fn base64_roundtrip() {
        for input in [
            &b""[..],
            b"a",
            b"ab",
            b"abc",
            b"hello world, rabbit control injection payload!",
        ] {
            let encoded = base64_encode(input);
            let decoded = base64_decode(&encoded).expect("decode");
            assert_eq!(decoded, input, "roundtrip failed for {input:?} → {encoded}");
        }
    }

    #[test]
    fn proxy_payload_roundtrip() {
        let events = vec![
            InputEvent::MouseMove { x: 100, y: 200 },
            InputEvent::MouseButton {
                button: MouseButton::Left,
                pressed: true,
            },
            InputEvent::Key {
                scancode: 0x1D,
                pressed: false,
                modifiers: 0,
            },
        ];
        let mut wire = Vec::new();
        encode_events(&events, &mut wire);
        let b64 = base64_encode(&wire);
        let decoded = decode_proxy_payload(&b64).expect("decode payload");
        assert_eq!(decoded.len(), events.len());
        // 线格式逐字节还原语义（encode/decode 已有 core 单测锁定）
        let mut re_wire = Vec::new();
        encode_events(&decoded, &mut re_wire);
        assert_eq!(re_wire, wire);
    }

    #[test]
    fn daemon_unreachable_returns_false() {
        // daemon 未运行时管道连接失败 → Ok(false)，不 panic、不长阻塞
        let events = vec![InputEvent::MouseMove { x: 1, y: 2 }];
        let result = proxy_inject(&events);
        assert!(matches!(result, Ok(false)), "daemon 未运行应为 Ok(false)");
    }
}
