//! M4 工具面板：终端（文本视图）/ 文件传输 / 端口映射。
//!
//! 事件驱动：on_tick 排空 `try_recv_terminal_event` / `try_recv_file_event`，
//! 经 dirty 标志在本 tick 末统一渲染（终端输出密集时只 SetText 一次）。

use super::super::actions::*;
use super::super::App;
use rc_tools::file_transfer::TransferDirection;
use rc_tools::file_transfer::TransferStatus;
use rc_tools::portmap::PortMapStatus;
use windows::core::{Interface, Result, HSTRING};
use windows::UI::Xaml::Controls::{StackPanel, TextBox};

impl App {
    /// 工具面板显隐切换。
    pub(crate) fn on_tool_toggle(&mut self) -> Result<()> {
        self.tool_panel_visible = !self.tool_panel_visible;
        self.set_visible("ToolPanel", self.tool_panel_visible)?;
        Ok(())
    }

    /// 工具 tab 切换（0 终端 / 1 文件 / 2 端口）。
    pub(crate) fn on_tool_tab(&mut self, tab: u8) -> Result<()> {
        self.tool_tab = tab;
        self.set_visible("TerminalPane", tab == 0)?;
        self.set_visible("FilesPane", tab == 1)?;
        self.set_visible("PortmapPane", tab == 2)?;
        self.set_checked_named("ToolTabTerminal", tab == 0)?;
        self.set_checked_named("ToolTabFiles", tab == 1)?;
        self.set_checked_named("ToolTabPortmap", tab == 2)?;
        if tab == 1 {
            self.render_transfers()?;
        }
        if tab == 2 {
            self.render_portmaps()?;
        }
        Ok(())
    }

    pub(crate) fn set_checked_named(&self, name: &str, on: bool) -> Result<()> {
        let toggle: windows::UI::Xaml::Controls::Primitives::ToggleButton = self.find(name)?;
        super::super::set_checked(&toggle, on)
    }

    /// 打开新终端（attach）。
    pub(crate) fn on_terminal_new(&mut self) -> Result<()> {
        let Some(active) = self.session.as_ref() else {
            self.label("TerminalHint", "未连接会话。")?;
            return Ok(());
        };
        self.label("TerminalHint", "正在打开终端…")?;
        match active.handle.tools().terminal_open_with_options(
            super::terminal_view::TERMINAL_COLS,
            super::terminal_view::TERMINAL_ROWS,
            None,
            true,
        ) {
            Ok(_term_id) => Ok(()),
            Err(err) => self.label("TerminalHint", &format!("打开失败：{err}")),
        }
    }

    /// 发送输入行（补 \n；本地回显）。
    pub(crate) fn on_terminal_send(&mut self) -> Result<()> {
        let Some(term_id) = self.terminal_id else {
            return Ok(());
        };
        let input = self
            .find_opt::<TextBox>("TerminalInput")
            .and_then(|b| b.Text().ok())
            .map(|t| t.to_string())
            .unwrap_or_default();
        if input.is_empty() {
            return Ok(());
        }
        let Some(active) = self.session.as_ref() else {
            return Ok(());
        };
        let mut data = input.into_bytes();
        data.push(b'\n');
        if let Err(err) = active.handle.tools().terminal_input(term_id, &data) {
            self.label("TerminalHint", &format!("发送失败：{err}"))?;
            return Ok(());
        }
        if let Some(box_) = self.find_opt::<TextBox>("TerminalInput") {
            let _ = box_.SetText(HSTRING::new());
        }
        Ok(())
    }

    /// 终端事件排空（on_tick 调用；dirty 后由 finish_tools_render 统一刷 UI）。
    pub(crate) fn drain_terminal_events(&mut self) {
        let mut events = Vec::new();
        if let Some(active) = self.session.as_ref() {
            while let Some(ev) = active.handle.try_recv_terminal_event() {
                events.push(ev);
            }
        }
        for ev in events {
            use rc_tools::terminal::TerminalEvent;
            match ev {
                TerminalEvent::Output { bytes, .. } => {
                    // 原始字节交给 VT 屏幕模型（跨块的 UTF-8 / 转义序列在模型内拼接）
                    self.terminal.feed_bytes(&bytes);
                    self.terminal_dirty = true;
                }
                TerminalEvent::Opened {
                    term_id, ok, error, ..
                } => {
                    if ok {
                        self.terminal_id = Some(term_id);
                        self.terminal.full_reset();
                        self.terminal_dirty = true;
                        let _ = self.label("TerminalHint", &format!("终端 #{term_id} 已就绪"));
                    } else {
                        let _ = self.label(
                            "TerminalHint",
                            &format!("打开失败：{}", error.unwrap_or_default()),
                        );
                    }
                }
                TerminalEvent::Attached {
                    term_id, ok, error, ..
                } => {
                    if ok {
                        self.terminal_id = Some(term_id);
                        let _ = self.label("TerminalHint", &format!("终端 #{term_id} 已附着"));
                    } else {
                        let _ = self.label(
                            "TerminalHint",
                            &format!("附着失败：{}", error.unwrap_or_default()),
                        );
                    }
                }
                TerminalEvent::Exited { term_id, .. } if self.terminal_id == Some(term_id) => {
                    self.terminal_id = None;
                    self.terminal
                        .feed(&format!("\r\n[终端 #{term_id} 已退出]\r\n"));
                    self.terminal_dirty = true;
                    let _ = self.label("TerminalHint", "终端已退出，可点击「新终端」重开");
                }
                _ => {}
            }
        }
    }

    /// 文件事件排空（on_tick 调用）。
    pub(crate) fn drain_file_events(&mut self) {
        let mut events = Vec::new();
        if let Some(active) = self.session.as_ref() {
            while let Some(ev) = active.handle.try_recv_file_event() {
                events.push(ev);
            }
        }
        for ev in events {
            use rc_tools::file_transfer::FileEvent;
            if let FileEvent::TransferUpdated(item) = ev {
                if let Some(slot) = self.transfers.iter_mut().find(|t| t.id == item.id) {
                    *slot = item;
                } else {
                    self.transfers.push(item);
                }
                self.transfers_dirty = true;
            }
        }
    }

    /// 上传：文件选择对话框 → 远端 home。
    pub(crate) fn on_file_upload_pick(&mut self) -> Result<()> {
        let Some(path) = crate::ui::dialogs::pick_file() else {
            return Ok(());
        };
        let Some(active) = self.session.as_ref() else {
            return Ok(());
        };
        let file_name = path
            .rsplit(['\\', '/'])
            .next()
            .unwrap_or("file")
            .to_string();
        match active.handle.tools().file_upload(&path, "~") {
            Ok(_id) => {
                self.label("FileHint", &format!("已开始上传：{file_name}"))?;
            }
            Err(err) => {
                self.label("FileHint", &format!("上传失败：{err}"))?;
            }
        }
        Ok(())
    }

    /// 下载第一步：弹出输入浮层（远端路径）。
    pub(crate) fn on_file_download_pick(&mut self) -> Result<()> {
        self.prompt_pending = 1;
        self.label("PromptTitle", "输入远端文件完整路径")?;
        if let Some(box_) = self.find_opt::<TextBox>("PromptInput") {
            let _ = box_.SetText(HSTRING::new());
        }
        self.set_visible("PromptOverlay", true)
    }

    /// 提示浮层确认（prompt_pending 决定后续动作）。
    pub(crate) fn on_prompt_ok(&mut self) -> Result<()> {
        let value = self
            .find_opt::<TextBox>("PromptInput")
            .and_then(|b| b.Text().ok())
            .map(|t| t.to_string())
            .unwrap_or_default();
        self.set_visible("PromptOverlay", false)?;
        let pending = std::mem::replace(&mut self.prompt_pending, 0);
        if pending != 1 || value.trim().is_empty() {
            return Ok(());
        }
        let remote = value.trim().to_string();
        let Some(active) = self.session.as_ref() else {
            return Ok(());
        };
        let local_dir = std::env::var("USERPROFILE")
            .map(|home| format!("{home}\\Downloads"))
            .unwrap_or_else(|_| ".".to_string());
        match active.handle.tools().file_download(&remote, &local_dir) {
            Ok(_id) => {
                self.label("FileHint", "已开始下载，保存到本机「下载」目录。")?;
            }
            Err(err) => {
                self.label("FileHint", &format!("下载失败：{err}"))?;
            }
        }
        Ok(())
    }

    pub(crate) fn on_prompt_cancel(&mut self) -> Result<()> {
        self.prompt_pending = 0;
        self.set_visible("PromptOverlay", false)
    }

    pub(crate) fn on_transfer_cancel(&mut self, id: u64) -> Result<()> {
        if let Some(active) = self.session.as_ref() {
            let _ = active.handle.tools().file_cancel(id);
        }
        Ok(())
    }

    /// 端口映射添加（表单校验走 core）。
    pub(crate) fn on_portmap_add(&mut self) -> Result<()> {
        let text = |name: &str| -> String {
            self.find_opt::<TextBox>(name)
                .and_then(|b| b.Text().ok())
                .map(|t| t.to_string())
                .unwrap_or_default()
        };
        let name = text("PortmapName");
        let local = text("PortmapLocalPort");
        let addr = text("PortmapTargetAddr");
        let target = text("PortmapTargetPort");
        let parsed = (|| -> std::result::Result<(u16, String, u16), String> {
            let local: u16 = local
                .trim()
                .parse()
                .map_err(|_| "本地端口非法".to_string())?;
            let target: u16 = target
                .trim()
                .parse()
                .map_err(|_| "目标端口非法".to_string())?;
            let addr = if addr.trim().is_empty() {
                "127.0.0.1".to_string()
            } else {
                addr.trim().to_string()
            };
            Ok((local, addr, target))
        })();
        let Some(active) = self.session.as_ref() else {
            return Ok(());
        };
        match parsed.and_then(|(local, addr, target)| {
            let name = if name.trim().is_empty() {
                format!("rule-{local}")
            } else {
                name
            };
            active
                .handle
                .tools()
                .portmap_add_with_enabled(name, local, addr, target, true)
                .map(|(_, all)| all)
        }) {
            Ok(all) => {
                self.portmaps = all;
                self.label("PortmapError", "")?;
                self.render_portmaps()?;
            }
            Err(err) => {
                self.label("PortmapError", &err)?;
            }
        }
        Ok(())
    }

    pub(crate) fn on_portmap_rule(&mut self, id: u64, op: u8) -> Result<()> {
        let Some(active) = self.session.as_ref() else {
            return Ok(());
        };
        let tools = active.handle.tools();
        let outcome = match op {
            0 => tools.portmap_delete(id),
            1 => tools.portmap_toggle(id, true).map(|(_, all)| all),
            2 => tools.portmap_toggle(id, false).map(|(_, all)| all),
            _ => Ok(Vec::new()),
        };
        match outcome {
            Ok(all) => {
                self.portmaps = all;
                self.render_portmaps()?;
            }
            Err(err) => {
                self.label("PortmapError", &err)?;
            }
        }
        Ok(())
    }

    /// 传输列表渲染（简单行：名称 进度 状态 + 取消按钮）。
    pub(crate) fn render_transfers(&mut self) -> Result<()> {
        use windows::UI::Xaml::Controls;
        let Some(panel) = self.find_opt::<StackPanel>("TransferList") else {
            return Ok(());
        };
        let children = {
            let as_panel: Controls::Panel = panel.cast()?;
            as_panel.Children()?
        };
        while children.Size()? > 0 {
            children.RemoveAtEnd()?;
        }
        let items = if let Some(active) = self.session.as_ref() {
            active.handle.tools().file_list_transfers()
        } else {
            self.transfers.clone()
        };
        for item in items.iter().rev().take(30) {
            let pct = if item.total_bytes > 0 {
                (item.done_bytes as f64 / item.total_bytes as f64 * 100.0).round() as u32
            } else if item.status == TransferStatus::Done {
                100
            } else {
                0
            };
            let status = match item.status {
                TransferStatus::Active => format!("传输中 {pct}%"),
                TransferStatus::Done => "完成".to_string(),
                TransferStatus::Failed => format!(
                    "失败：{}",
                    item.error.clone().unwrap_or_else(|| "未知".into())
                ),
                TransferStatus::Cancelled => "已取消".to_string(),
            };
            let dir = if matches!(item.direction, TransferDirection::Upload) {
                "↑"
            } else {
                "↓"
            };
            let row = format!("{dir} {} · {}", item.name, status);
            let text = Controls::TextBlock::new()?;
            text.SetText(HSTRING::from(&row))?;
            text.SetFontSize(12f64)?;
            children.Append(&text)?;
            if item.status == TransferStatus::Active {
                let cancel = Controls::Button::new()?;
                cancel
                    .cast::<Controls::ContentControl>()?
                    .SetContent(content_object("取消")?)?;
                cancel.cast::<Controls::Control>()?.SetFontSize(11f64)?;
                children.Append(&cancel)?;
                let sender = self.sender();
                let id = item.id;
                let btn: windows::UI::Xaml::Controls::Primitives::ButtonBase = cancel.cast()?;
                btn.Click(windows::UI::Xaml::RoutedEventHandler::new(move |_, _| {
                    sender.send(Event::Click(Action::TransferCancel(id)));
                    Ok(())
                }))?;
            }
        }
        Ok(())
    }

    /// 端口映射列表渲染。
    pub(crate) fn render_portmaps(&mut self) -> Result<()> {
        use windows::UI::Xaml::Controls;
        let Some(panel) = self.find_opt::<StackPanel>("PortmapList") else {
            return Ok(());
        };
        let children = {
            let as_panel: Controls::Panel = panel.cast()?;
            as_panel.Children()?
        };
        while children.Size()? > 0 {
            children.RemoveAtEnd()?;
        }
        let rules = if let Some(active) = self.session.as_ref() {
            active.handle.tools().portmap_list()
        } else {
            Vec::new()
        };
        if rules.is_empty() {
            let text = Controls::TextBlock::new()?;
            text.SetText(HSTRING::from("暂无映射规则"))?;
            text.SetFontSize(12f64)?;
            children.Append(&text)?;
            return Ok(());
        }
        for rule in &rules {
            let status = match rule.status {
                PortMapStatus::Ok => "运行中",
                PortMapStatus::Fail => "错误",
                PortMapStatus::Pending => "暂停",
            };
            let row = format!(
                "{} · :{} → {}:{} · {} · 收 {} / 发 {}",
                rule.name,
                rule.local_port,
                rule.target_addr,
                rule.target_port,
                status,
                fmt_bytes(rule.bytes_rx),
                fmt_bytes(rule.bytes_tx),
            );
            let text = Controls::TextBlock::new()?;
            text.SetText(HSTRING::from(&row))?;
            text.SetFontSize(12f64)?;
            children.Append(&text)?;
            // 开关 + 删除
            let toggle = Controls::Button::new()?;
            toggle
                .cast::<Controls::ContentControl>()?
                .SetContent(content_object(if rule.enabled {
                    "暂停"
                } else {
                    "启用"
                })?)?;
            toggle.cast::<Controls::Control>()?.SetFontSize(11f64)?;
            children.Append(&toggle)?;
            let del = Controls::Button::new()?;
            del.cast::<Controls::ContentControl>()?
                .SetContent(content_object("删除")?)?;
            del.cast::<Controls::Control>()?.SetFontSize(11f64)?;
            children.Append(&del)?;
            let sender = self.sender();
            let (id, enable) = (rule.id, !rule.enabled);
            let toggle_btn: windows::UI::Xaml::Controls::Primitives::ButtonBase = toggle.cast()?;
            toggle_btn.Click(windows::UI::Xaml::RoutedEventHandler::new(move |_, _| {
                sender.send(Event::Click(Action::PortmapRule(
                    id,
                    if enable { 1 } else { 2 },
                )));
                Ok(())
            }))?;
            let sender = self.sender();
            let id = rule.id;
            let del_btn: windows::UI::Xaml::Controls::Primitives::ButtonBase = del.cast()?;
            del_btn.Click(windows::UI::Xaml::RoutedEventHandler::new(move |_, _| {
                sender.send(Event::Click(Action::PortmapRule(id, 0)));
                Ok(())
            }))?;
        }
        Ok(())
    }

    /// 本 tick 末统一渲染工具面板（脏标志驱动）。
    pub(crate) fn finish_tools_render(&mut self) -> Result<()> {
        if self.terminal_dirty {
            self.terminal_dirty = false;
            self.render_terminal()?;
            if let Some(scroll) =
                self.find_opt::<windows::UI::Xaml::Controls::ScrollViewer>("TerminalScroll")
            {
                let offset = windows::Foundation::PropertyValue::CreateDouble(1.0e6)?;
                let reference: windows::Foundation::IReference<f64> = offset.cast()?;
                let _ = scroll.ChangeView(None, &reference, None);
            }
        }
        if self.transfers_dirty {
            self.transfers_dirty = false;
            if self.tool_tab == 1 {
                self.render_transfers()?;
            }
        }
        Ok(())
    }

    /// 会话结束清理（清空终端/传输状态）。
    pub(crate) fn reset_tools_state(&mut self) {
        self.terminal.full_reset();
        self.terminal_id = None;
        self.transfers.clear();
        self.terminal_dirty = true;
        self.transfers_dirty = true;
    }
}

/// HSTRING → IInspectable（0.37 SetContent 参数要求）。
fn content_object(s: &str) -> windows::core::Result<windows::core::IInspectable> {
    windows::Foundation::PropertyValue::CreateString(HSTRING::from(s))
}

/// 字节数人类可读。
fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < 3 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}
