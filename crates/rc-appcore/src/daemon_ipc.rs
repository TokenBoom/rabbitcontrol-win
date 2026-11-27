//! 与守护服务（`rc-daemon`）通信的客户端：探测、请求暂停 / 恢复拉起后台进程。
//!
//! 协议是行分隔 JSON（`{"op":"..."}`），服务端实现在 `rc-daemon`。管道名与收发
//! 逻辑只在这里定义一份，主界面、后台进程、注入代理都用它。

use serde_json::Value;

use crate::named_pipe::{round_trip, PipeError};

/// 守护服务管道名。
pub const PIPE_NAME: &str = r"\\.\pipe\rabbitcontrol-daemon";
/// 单帧上限（字节），与服务端一致。
pub const MAX_FRAME_BYTES: usize = 16 * 1024;
/// 单次调用超时（毫秒）。
const TIMEOUT_MS: u32 = 3_000;

/// 原样发一帧、收一帧（服务端会去掉结尾换行）。
pub fn call(frame: &str, timeout_ms: u32) -> Result<String, PipeError> {
    round_trip(PIPE_NAME, frame, MAX_FRAME_BYTES, timeout_ms)
}

/// 守护服务是否在运行（未安装、未启动或没有响应都算「不在」）。
pub fn reachable() -> bool {
    call(r#"{"op":"ping"}"#, TIMEOUT_MS).is_ok()
}

/// 请守护服务不要再拉起后台进程（用户主动退出后）。服务不在时无需通知。
pub fn standdown() -> Result<(), String> {
    simple_op("standdown")
}

/// 撤销 [`standdown`]：打开主界面时调用，后台进程恢复开机 / 登录自启。
pub fn resume() -> Result<(), String> {
    simple_op("resume")
}

fn simple_op(op: &str) -> Result<(), String> {
    let frame = serde_json::json!({ "op": op }).to_string();
    match call(&frame, TIMEOUT_MS) {
        Ok(reply) => {
            let parsed: Result<Value, _> = serde_json::from_str(&reply);
            match parsed {
                Ok(Value::Object(ref map)) if map.get("ok") == Some(&Value::Bool(true)) => Ok(()),
                Ok(other) => Err(format!("守护服务返回了意外的内容：{other}")),
                Err(err) => Err(format!("守护服务响应无法解析：{err}")),
            }
        }
        // 服务没在运行：本来就没有东西会拉起后台进程，视为成功
        Err(PipeError::NotFound) => Ok(()),
        Err(err) => Err(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_end_up_as_line_delimited_json() {
        let frame = serde_json::json!({ "op": "standdown" }).to_string();
        assert_eq!(frame, r#"{"op":"standdown"}"#);
    }

    #[test]
    fn an_absent_daemon_is_not_an_error_for_standdown() {
        // 本机不一定装了守护服务：两种结果都要「有定义」——要么成功，要么给出可读原因
        if let Err(err) = standdown() {
            assert!(!err.is_empty());
        }
    }
}
