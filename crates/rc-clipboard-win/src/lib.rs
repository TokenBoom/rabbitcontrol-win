//! RabbitControl Windows 剪贴板后端。
//!
//! - 变更检测：`GetClipboardSequenceNumber` 计数轮询，或
//!   `AddClipboardFormatListener` + [`rc_win32core::MessageWindow`]
//!   订阅 `WM_CLIPBOARDUPDATE` 事件驱动；
//! - 文本读写：`OpenClipboard`/`CF_UNICODETEXT`，写剪贴板时先
//!   `EmptyClipboard` 再 `SetClipboardData`（`GMEM_MOVEABLE` 全局
//!   内存句柄所有权移交系统）；
//! - 回环抑制由 core 剪贴板通道负责，本 crate 只做平台读写。

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
