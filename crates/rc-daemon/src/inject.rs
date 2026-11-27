//! UIPI 代理注入（LocalSystem）：解码 agent 转发的事件并用本进程
//! SendInput 注入——LocalSystem 令牌可穿透 UAC 安全桌面与锁屏。

use crate::protocol::Response;
use rc_input_win::proxy;

/// 解码并注入一批代理事件，返回协议响应。
pub fn inject_proxy_payload(payload: &str) -> Response {
    let events = match proxy::decode_proxy_payload(payload) {
        Ok(events) => events,
        Err(err) => return Response::err(err),
    };
    match rc_input_win::InputBridge::for_main_display() {
        Ok(mut bridge) => match rc_input::InputInjector::inject(&mut bridge, &events) {
            Ok(()) => Response::ok_json(format!("{{\"injected\":{}}}", events.len())),
            Err(err) => Response::err(err.to_string()),
        },
        Err(err) => Response::err(err.to_string()),
    }
}
