//! 主界面 ↔ 后台被控进程（`rc-host-agent`）的命名管道协议。
//!
//! 单请求-单响应，行分隔 JSON（UTF-8）。管道由后台进程创建，Windows 默认
//! DACL 只允许创建它的用户以及 SYSTEM / 管理员读写——也就是只有同一用户的
//! 进程能下发指令。管道名带会话号：快速用户切换时每个会话各有一条，互不串扰。
//!
//! 服务端实现在 `rc-host-win`；这里只有协议类型与客户端调用。

#![allow(unsafe_code)] // 命名管道 FFI 边界

use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_OVERLAPPED,
    FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_NONE, OPEN_EXISTING,
};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcessId, ResetEvent, WaitForSingleObject,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

/// 管道名前缀；完整名为此前缀加当前会话号。
pub const PIPE_PREFIX: &str = r"\\.\pipe\rabbitcontrol-agent-s";
/// 单帧上限（字节）。
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

/// 当前会话对应的管道名。
pub fn pipe_name() -> String {
    let mut session = 0u32;
    // 取不到会话号时按 0 处理（仍是一条确定的名字）
    unsafe {
        let _ = ProcessIdToSessionId(GetCurrentProcessId(), &mut session);
    }
    format!("{PIPE_PREFIX}{session}")
}

/// 主界面（或其他本机进程）发给后台被控进程的请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Request {
    /// 存活探针。
    Ping,
    /// 查询后台被控进程完整快照。
    Snapshot,
    /// 主界面已启动（携带主界面进程 PID）：后台让位，由主界面托管信令与被控。
    GuiOnline {
        /// 主界面进程 ID；后台据此监视其退出并接管。
        pid: u32,
    },
    /// 主界面即将退出：提示后台准备接管（以进程真正退出为准）。
    GuiOffline,
    /// 开关「允许被控」。
    SetHostEnabled {
        /// 是否允许本机被控。
        enabled: bool,
    },
    /// 应答待审批的连接请求。
    RespondApproval {
        /// 审批代次（过期代次被静默丢弃）。
        generation: u64,
        /// `true` 放行，`false` 拒绝。
        accept: bool,
    },
    /// 断开当前正在控制本机的远端。
    DisconnectClient,
    /// 请求后台打开或激活主界面。
    OpenGui,
    /// 整体退出后台进程（并请求守护服务暂不再拉起）。
    Quit,
}

/// 后台被控进程的状态快照。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentSnapshot {
    /// 本机是否已登录 RabbitControl 账号。
    pub logged_in: bool,
    /// 信令服务是否在线。
    pub signal_online: bool,
    /// 本机 9 位设备码（未分配时为 `None`）。
    pub device_id: Option<String>,
    /// 「允许被控」开关。
    pub host_enabled: bool,
    /// 当前临时验证码（被控服务运行且使用临时码时）。
    pub temp_code: Option<String>,
    /// 被控会话进行中。
    pub session_active: bool,
    /// 正在控制本机的远端描述。
    pub connected_peer: Option<String>,
    /// 有连接请求等待审批。
    pub pending_approval: Option<PendingApproval>,
    /// 后台已让位（主界面正在托管信令与被控）。
    pub suspended: bool,
    /// 最近一次错误说明（无则为 `None`）。
    pub last_error: Option<String>,
}

/// 一条等待审批的连接请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingApproval {
    /// 审批代次。
    pub generation: u64,
    /// 请求方地址。
    pub remote_addr: String,
    /// 请求方是否已通过密码验证。
    pub password_verified: bool,
}

/// 后台对请求的响应：`ok` + 附加字段，或 `ok:false` + `error`。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// 请求是否成功。
    pub ok: bool,
    /// 失败原因。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 附加结果字段。
    #[serde(flatten)]
    pub data: serde_json::Map<String, serde_json::Value>,
}

impl Response {
    /// 成功响应。
    pub fn ok(data: serde_json::Value) -> Self {
        Self {
            ok: true,
            error: None,
            data: match data {
                serde_json::Value::Object(map) => map,
                serde_json::Value::Null => serde_json::Map::new(),
                other => serde_json::Map::from_iter([("value".to_string(), other)]),
            },
        }
    }

    /// 失败响应。
    pub fn err(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            data: serde_json::Map::new(),
        }
    }

    /// 读取一个附加字段的值。
    pub fn get<T: serde::de::DeserializeOwned>(&self, key: &str) -> Option<T> {
        self.data
            .get(key)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }
}

/// 发送一次请求并等待响应（≤ `timeout_ms` 毫秒）。
///
/// 后台进程未运行、管道不可达或超时都返回 `Err`（可直接展示的中文原因）。
pub fn call(request: &Request, timeout_ms: u32) -> Result<Response, String> {
    call_pipe(&pipe_name(), request, timeout_ms)
}

/// 向指定管道名发送一次请求（[`call`] 的底层形式；测试与诊断用）。
pub fn call_pipe(pipe: &str, request: &Request, timeout_ms: u32) -> Result<Response, String> {
    let frame = serde_json::to_string(request).map_err(|e| format!("请求编码失败：{e}"))?;
    let reply = raw_call(pipe, &frame, timeout_ms)?;
    serde_json::from_str(&reply).map_err(|e| format!("后台响应无法解析：{e}"))
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
fn encode_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 一次 overlapped I/O：立即完成则直接返回，否则等到事件触发；超时则取消。
fn run_io(
    handle: &OwnedHandle,
    event: &OwnedHandle,
    timeout_ms: u32,
    what: &str,
    io: impl FnOnce(*mut OVERLAPPED) -> windows::core::Result<()>,
) -> Result<u32, String> {
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
        Err(err) if err.code() == windows::Win32::Foundation::ERROR_IO_PENDING.to_hresult() => true,
        Err(err) => return Err(format!("{what}失败：{err}")),
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
            return Err(format!("{what}超时"));
        }
    }
    let mut transferred = 0u32;
    unsafe {
        GetOverlappedResult(handle.0, &overlapped, &mut transferred, false)
            .map_err(|err| format!("{what}失败：{err}"))?;
    }
    Ok(transferred)
}

/// 打开管道 → 写一帧 → 读一帧 → 关闭。
fn raw_call(pipe: &str, frame: &str, timeout_ms: u32) -> Result<String, String> {
    let name = encode_wide(pipe);
    let handle = unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
            FILE_SHARE_NONE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED as FILE_FLAGS_AND_ATTRIBUTES,
            None,
        )
    }
    .map_err(|err| {
        if err.code() == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND.to_hresult() {
            "后台被控进程未运行".to_string()
        } else {
            format!("连接后台被控进程失败：{err}")
        }
    })?;
    let handle = OwnedHandle(handle);
    let event = OwnedHandle(
        unsafe { CreateEventW(None, true, false, None) }
            .map_err(|err| format!("创建事件失败：{err}"))?,
    );

    let mut data = frame.as_bytes().to_vec();
    data.push(b'\n');
    run_io(&handle, &event, timeout_ms, "发送请求", |ov| unsafe {
        let mut written = 0u32;
        WriteFile(handle.0, Some(&data), Some(&mut written), Some(ov))
    })?;

    let mut buf = [0u8; MAX_FRAME_BYTES];
    let read = run_io(&handle, &event, timeout_ms, "读取响应", |ov| unsafe {
        let mut got = 0u32;
        ReadFile(handle.0, Some(&mut buf), Some(&mut got), Some(ov))
    })?;
    if read as usize > MAX_FRAME_BYTES {
        return Err("后台响应过长".to_string());
    }
    let text = String::from_utf8_lossy(&buf[..read as usize]).into_owned();
    Ok(text.trim_end_matches(['\n', '\r']).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_wire_format_matches_documented_ops() {
        let cases = [
            (Request::Ping, r#"{"op":"ping"}"#),
            (Request::Snapshot, r#"{"op":"snapshot"}"#),
            (
                Request::GuiOnline { pid: 4242 },
                r#"{"op":"gui-online","pid":4242}"#,
            ),
            (Request::GuiOffline, r#"{"op":"gui-offline"}"#),
            (
                Request::SetHostEnabled { enabled: true },
                r#"{"op":"set-host-enabled","enabled":true}"#,
            ),
            (
                Request::RespondApproval {
                    generation: 7,
                    accept: false,
                },
                r#"{"op":"respond-approval","generation":7,"accept":false}"#,
            ),
            (Request::DisconnectClient, r#"{"op":"disconnect-client"}"#),
            (Request::OpenGui, r#"{"op":"open-gui"}"#),
            (Request::Quit, r#"{"op":"quit"}"#),
        ];
        for (request, wire) in cases {
            assert_eq!(serde_json::to_string(&request).unwrap(), wire);
            assert_eq!(&serde_json::from_str::<Request>(wire).unwrap(), &request);
        }
    }

    #[test]
    fn response_carries_data_and_errors() {
        let ok = Response::ok(json!({"host_enabled": true, "pid": 9}));
        let text = serde_json::to_string(&ok).unwrap();
        let parsed: Response = serde_json::from_str(&text).unwrap();
        assert!(parsed.ok);
        assert_eq!(parsed.get::<bool>("host_enabled"), Some(true));
        assert_eq!(parsed.get::<u32>("pid"), Some(9));
        assert_eq!(parsed.get::<bool>("missing"), None);

        let err: Response = serde_json::from_str(r#"{"ok":false,"error":"信令未运行"}"#).unwrap();
        assert!(!err.ok);
        assert_eq!(err.error.as_deref(), Some("信令未运行"));
        assert_eq!(Response::ok(serde_json::Value::Null).data.len(), 0);
        assert_eq!(
            Response::ok(json!("x")).get::<String>("value").as_deref(),
            Some("x")
        );
    }

    #[test]
    fn snapshot_defaults_and_roundtrip() {
        let text = serde_json::to_string(&AgentSnapshot::default()).unwrap();
        let parsed: AgentSnapshot = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, AgentSnapshot::default());
        // 旧版后台缺字段：按默认值补齐
        let partial: AgentSnapshot = serde_json::from_str(r#"{"logged_in":true}"#).unwrap();
        assert!(partial.logged_in && !partial.signal_online);
    }

    #[test]
    fn pipe_name_is_session_scoped() {
        let name = pipe_name();
        assert!(
            name.starts_with(r"\\.\pipe\rabbitcontrol-agent-s"),
            "{name}"
        );
        assert!(name[r"\\.\pipe\rabbitcontrol-agent-s".len()..]
            .chars()
            .all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn calling_a_missing_pipe_fails_fast() {
        // 不依赖本机是否恰有后台进程在跑：用一个绝不存在的管道名
        let missing = format!(
            r"\\.\pipe\rabbitcontrol-agent-missing-{}",
            std::process::id()
        );
        let started = std::time::Instant::now();
        let result = call_pipe(&missing, &Request::Ping, 5_000);
        assert_eq!(result.unwrap_err(), "后台被控进程未运行");
        // 管道不存在时立即失败，而不是等满超时
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
    }
}
