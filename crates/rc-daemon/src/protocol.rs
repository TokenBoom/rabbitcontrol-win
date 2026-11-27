//! 命名管道 IPC 协议（M5b.2）：行分隔 JSON 帧。
//!
//! 请求 `{"op":"ping"}` / `status` / `inject` / `stop-daemon` / `standdown` /
//! `resume`，响应统一 `{"ok":bool,"data":{...},"error":string?}`。
//! 每条请求都先经 [`authorize`] 核对调用方身份（`inject` 是提权原语）。帧以 `\n` 结尾、
//! 单帧上限 [`MAX_FRAME_BYTES`]（防失控输出撑爆读取缓冲）。

use serde::{Deserialize, Serialize};

use crate::security::CallerIdentity;

/// 单帧上限（字节），与客户端共用同一处定义。
pub const MAX_FRAME_BYTES: usize = rc_appcore::daemon_ipc::MAX_FRAME_BYTES;

/// 守护管道名称（LocalSystem 创建，DACL 见 `security`）。
pub const PIPE_NAME: &str = rc_appcore::daemon_ipc::PIPE_NAME;

/// 客户端请求。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Request {
    /// 存活与版本探测。
    Ping,
    /// 代理注入一批输入事件（Base64(rc_input 线格式)）。
    ///
    /// 仅 LocalSystem daemon 可穿透 UIPI（锁屏/UAC/提权窗口）。
    #[serde(rename = "inject")]
    Inject {
        /// Base64 编码的线格式事件序列。
        payload: String,
    },
    /// 服务状态（版本/运行时长/监管 agent 状态）。
    Status,
    /// 请求服务停止（服务控制管理器之外的软停通道）。
    StopDaemon,
    /// 暂停拉起后台进程（用户从托盘主动退出后；主界面打开时 `resume`）。
    Standdown,
    /// 恢复拉起后台进程。
    Resume,
}

/// 一条请求对调用方身份的要求。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Privilege {
    /// 本机交互登录的用户即可。
    Interactive,
    /// 必须是当前活动桌面会话的登录用户本人。
    ConsoleUser,
    /// 必须是已提权的管理员（或服务自身）。
    Elevated,
}

impl Request {
    /// 这条请求要求的身份。
    pub fn privilege(&self) -> Privilege {
        match self {
            Request::Ping | Request::Status | Request::Standdown | Request::Resume => {
                Privilege::Interactive
            }
            // 服务会替调用方注入键鼠：只接受注入目标会话的登录用户本人
            Request::Inject { .. } => Privilege::ConsoleUser,
            Request::StopDaemon => Privilege::Elevated,
        }
    }
}

/// 判定调用方能否执行这条请求。
///
/// `console` 为 `(活动控制台会话号, 该会话登录用户的 SID)`，没有登录桌面时为 `None`。
pub fn authorize(
    request: &Request,
    caller: Option<&CallerIdentity>,
    console: Option<(u32, &[u8])>,
) -> Result<(), String> {
    let Some(caller) = caller else {
        return Err("无法识别调用方".to_string());
    };
    match request.privilege() {
        Privilege::Interactive => {
            if caller.session_id != 0 || caller.elevated {
                Ok(())
            } else {
                Err("仅限本机交互登录的用户".to_string())
            }
        }
        Privilege::ConsoleUser => {
            let Some((session_id, sid)) = console else {
                return Err("当前没有登录的桌面会话".to_string());
            };
            if caller.session_id == session_id && caller.sid == sid {
                Ok(())
            } else {
                Err("仅限当前桌面会话的登录用户".to_string())
            }
        }
        Privilege::Elevated => {
            if caller.elevated {
                Ok(())
            } else {
                Err("需要管理员权限".to_string())
            }
        }
    }
}

/// 服务端响应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Response {
    /// 是否成功。
    pub ok: bool,
    /// 成功时的载荷（JSON 对象字符串，避免逐字段建模）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    /// 失败原因。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    /// 成功响应。
    pub fn ok_json(data: String) -> Self {
        Self {
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    /// 失败响应。
    pub fn err(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(message.into()),
        }
    }
}

/// 解析一帧请求（`#[serde(tag)]` + kebab-case：`{"op":"status"}`）。
pub fn parse_request(frame: &str) -> Result<Request, String> {
    serde_json::from_str(frame).map_err(|e| format!("请求解析失败：{e}"))
}

/// 序列化响应为单行帧（不含换行符）。
pub fn render_response(response: &Response) -> String {
    serde_json::to_string(response)
        .unwrap_or_else(|_| r#"{"ok":false,"error":"响应序列化失败"}"#.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip_all_ops() {
        for (frame, expect) in [
            (r#"{"op":"ping"}"#, Request::Ping),
            (r#"{"op":"status"}"#, Request::Status),
            (r#"{"op":"stop-daemon"}"#, Request::StopDaemon),
            (r#"{"op":"standdown"}"#, Request::Standdown),
            (r#"{"op":"resume"}"#, Request::Resume),
        ] {
            assert_eq!(parse_request(frame).expect(frame), expect);
        }
    }

    #[test]
    fn request_rejects_garbage() {
        assert!(parse_request("").is_err());
        assert!(parse_request("not json").is_err());
        assert!(parse_request(r#"{"op":"unknown"}"#).is_err());
    }

    fn caller(session_id: u32, sid: &[u8], elevated: bool) -> CallerIdentity {
        CallerIdentity {
            session_id,
            sid: sid.to_vec(),
            elevated,
        }
    }

    #[test]
    fn interactive_ops_need_an_identified_interactive_caller() {
        let user = caller(1, b"S-1-5-21-a", false);
        assert!(authorize(&Request::Ping, Some(&user), None).is_ok());
        assert!(authorize(&Request::Standdown, Some(&user), None).is_ok());
        assert!(authorize(&Request::Resume, Some(&user), None).is_ok());
        // 身份不明：一律拒绝
        assert!(authorize(&Request::Ping, None, None).is_err());
        assert!(authorize(&Request::Standdown, None, None).is_err());
        // 服务会话里的非提权进程不算「交互用户」；提权的（SYSTEM / 管理员）可以
        let service = caller(0, b"S-1-5-18", false);
        assert!(authorize(&Request::Ping, Some(&service), None).is_err());
        let system = caller(0, b"S-1-5-18", true);
        assert!(authorize(&Request::Ping, Some(&system), None).is_ok());
    }

    #[test]
    fn inject_is_limited_to_the_console_session_user() {
        let inject = Request::Inject {
            payload: "AA==".to_string(),
        };
        let console = (1u32, &b"S-1-5-21-a"[..]);
        let owner = caller(1, b"S-1-5-21-a", false);
        assert!(authorize(&inject, Some(&owner), Some(console)).is_ok());
        // 快速用户切换下，另一个已登录用户的会话
        let other_session = caller(2, b"S-1-5-21-a", false);
        assert!(authorize(&inject, Some(&other_session), Some(console)).is_err());
        // 同一会话号但用户不对
        let other_user = caller(1, b"S-1-5-21-b", false);
        assert!(authorize(&inject, Some(&other_user), Some(console)).is_err());
        // 没有登录桌面：谁都不行；身份不明：也不行
        assert!(authorize(&inject, Some(&owner), None).is_err());
        assert!(authorize(&inject, None, Some(console)).is_err());
    }

    #[test]
    fn stopping_the_service_needs_elevation() {
        let user = caller(1, b"S-1-5-21-a", false);
        assert!(authorize(&Request::StopDaemon, Some(&user), None).is_err());
        let admin = caller(1, b"S-1-5-21-a", true);
        assert!(authorize(&Request::StopDaemon, Some(&admin), None).is_ok());
    }

    #[test]
    fn response_frame_roundtrip() {
        let response = Response::ok_json("{\"version\":\"0.1.0\"}".to_string());
        let frame = render_response(&response);
        let parsed: Response = serde_json::from_str(&frame).expect("回读");
        assert_eq!(parsed, response);
        let err = Response::err("boom");
        let frame = render_response(&err);
        assert!(frame.contains("\"error\":\"boom\""));
    }
}
