//! 命名管道 IPC 协议（M5b.2）：行分隔 JSON 帧。
//!
//! 请求 `{"op":"status"}` / `{"op":"ping"}` / `{"op":"stop-daemon"}`，
//! 响应统一 `{"ok":bool,"data":{...},"error":string?}`。帧以 `\n` 结尾、
//! 单帧上限 [`MAX_FRAME_BYTES`]（防失控输出撑爆读取缓冲）。

use serde::{Deserialize, Serialize};

/// 单帧上限（字节）。
pub const MAX_FRAME_BYTES: usize = 16 * 1024;

/// 守护管道名称（LocalSystem 创建；默认 DACL 仅 SYSTEM/管理员可连）。
pub const PIPE_NAME: &str = "\\\\.\\pipe\\rabbitcontrol-daemon";

/// 客户端请求。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Request {
    /// 存活与版本探测。
    Ping,
    /// 服务状态（版本/运行时长/监管 agent 状态）。
    Status,
    /// 请求服务停止（服务控制管理器之外的软停通道）。
    StopDaemon,
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
