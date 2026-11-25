//! RabbitControl Windows 剪贴板后端（Win32，对应 macOS `rc-clipboard-macos` / Carbon Pasteboard）。
//!
//! # 设计
//!
//! - `GetClipboardSequenceNumber` 提供 [`ClipboardBackend::change_count`]
//!   变更计数（与 macOS `changeCount` 语义一致）；
//! - 文本读写经 `OpenClipboard` / `CF_UNICODETEXT`；
//! - 可选 `AddClipboardFormatListener` 事件驱动（替代轮询）。
//!
//! # 当前状态
//!
//! 骨架占位：读写返回未实现错误；[`register`] 已接入 core 提供者槽位。

#![deny(missing_docs)]
#![forbid(unsafe_code)]

use std::sync::Arc;

use rc_session::ClipboardBackend;
use rc_types::{Error, Result};

/// Win32 剪贴板后端（骨架占位）。
pub struct WinClipboard {
    count: u64,
}

impl WinClipboard {
    /// 创建后端实例。
    pub fn new() -> Result<Self> {
        Ok(Self { count: 0 })
    }
}

impl ClipboardBackend for WinClipboard {
    fn change_count(&mut self) -> Result<u64> {
        // TODO: GetClipboardSequenceNumber。
        Ok(self.count)
    }

    fn read_text(&mut self) -> Result<Option<String>> {
        // TODO: OpenClipboard + CF_UNICODETEXT。
        Err(Error::InvalidConfig(
            "clipboard read not yet implemented".into(),
        ))
    }

    fn write_text(&mut self, _text: &str) -> Result<()> {
        Err(Error::InvalidConfig(
            "clipboard write not yet implemented".into(),
        ))
    }
}

/// 注册 Win32 剪贴板提供者（幂等）。
pub fn register() -> bool {
    rc_session::register_clipboard_provider(Arc::new(|| {
        WinClipboard::new().map(|c| Box::new(c) as Box<dyn ClipboardBackend>)
    }))
}
