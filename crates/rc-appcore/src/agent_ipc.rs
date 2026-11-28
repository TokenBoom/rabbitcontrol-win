//! 主界面 ↔ 后台被控进程（`rc-host-agent`）的命名管道协议。
//!
//! 单请求-单响应，行分隔 JSON（UTF-8）。管道由后台进程创建，Windows 默认
//! DACL 只允许创建它的用户以及 SYSTEM / 管理员读写——也就是只有同一用户的
//! 进程能下发指令。管道名带会话号：快速用户切换时每个会话各有一条，互不串扰。
//!
//! 服务端实现在 `rc-host-win`；这里只有协议类型与客户端调用（收发在 [`crate::named_pipe`]）。

use serde::{Deserialize, Serialize};

use crate::named_pipe::current_session_id;

/// 管道名前缀；完整名为此前缀加当前会话号。
pub const PIPE_PREFIX: &str = r"\\.\pipe\rabbitcontrol-agent-s";
/// 单帧上限（字节）。
pub const MAX_FRAME_BYTES: usize = 64 * 1024;
/// 主界面顶层窗口的类名：后台进程据此判断主界面是否在运行（与 `rc-ui` 共用）。
pub const GUI_WINDOW_CLASS: &str = "RabbitControlWindow";
/// 后台被控进程的可执行文件名（与主界面、守护服务同目录）。
pub const AGENT_EXE: &str = "rc-host-agent.exe";

/// 当前会话对应的管道名。
pub fn pipe_name() -> String {
    format!("{PIPE_PREFIX}{}", current_session_id())
}

/// 主界面（或其他本机进程）发给后台被控进程的请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Request {
    /// 存活探针。
    Ping,
    /// 查询后台被控进程完整快照。
    Snapshot,
    /// 主界面已启动（携带主界面进程 PID）：后台让位，由主界面托管信令与被控；
    /// 后台盯着这个进程，它退出后自动接管。
    GuiOnline {
        /// 主界面进程 ID。
        pid: u32,
    },
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
    /// 请对端临时让出某个 UDP 探测端口（P2P 打洞要独占同一个本地端口），返回租约号
    /// `lease_id`。对端没持有该端口的探测槽位时失败。见 [`crate::p2p_port_lease`]。
    AcquireP2pUdpPort {
        /// 本地 UDP 端口。
        port: u16,
        /// 租约时长（毫秒，0 = 默认 30 秒；夹到 5–60 秒）。
        lease_ttl_ms: u64,
    },
    /// 续租：租约到期后对端会恢复该端口的探测。
    RenewP2pUdpPort {
        /// 租约号。
        lease_id: u64,
        /// 新的租约时长（毫秒）。
        lease_ttl_ms: u64,
    },
    /// 归还租约：对端立即恢复该端口的探测。
    ReleaseP2pUdpPort {
        /// 租约号。
        lease_id: u64,
    },
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
    let reply = crate::named_pipe::round_trip(pipe, &frame, MAX_FRAME_BYTES, timeout_ms).map_err(
        |err| match err {
            crate::named_pipe::PipeError::NotFound => NOT_RUNNING.to_string(),
            other => other.to_string(),
        },
    )?;
    serde_json::from_str(&reply).map_err(|e| format!("后台响应无法解析：{e}"))
}

/// 对端没在运行时的提示（主界面据此决定要不要拉起后台进程）。
const NOT_RUNNING: &str = "后台被控进程未运行";

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
            (
                Request::AcquireP2pUdpPort {
                    port: 44350,
                    lease_ttl_ms: 30_000,
                },
                r#"{"op":"acquire-p2p-udp-port","port":44350,"lease_ttl_ms":30000}"#,
            ),
            (
                Request::RenewP2pUdpPort {
                    lease_id: 7,
                    lease_ttl_ms: 30_000,
                },
                r#"{"op":"renew-p2p-udp-port","lease_id":7,"lease_ttl_ms":30000}"#,
            ),
            (
                Request::ReleaseP2pUdpPort { lease_id: 7 },
                r#"{"op":"release-p2p-udp-port","lease_id":7}"#,
            ),
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
