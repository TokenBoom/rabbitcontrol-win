//! M4 工具面板：终端（真终端）/ 文件传输 / 端口映射。
//!
//! 事件驱动：on_tick 排空 `try_recv_terminal_event` / `try_recv_file_event`，
//! 经 dirty 标志在本 tick 末统一渲染（终端输出密集时只重画一次）。
//! 面板宽度随 tab 变化，也可以拖右侧分隔条；停在终端 tab 时，画面尺寸决定终端的行列数。

use std::sync::{Arc, Mutex};

use super::super::actions::*;
use super::super::App;
use rc_tools::file_transfer::TransferDirection;
use rc_tools::file_transfer::TransferStatus;
use rc_tools::portmap::PortMapStatus;
use windows::core::{Result, HSTRING};
use windows::UI::Core::{CoreCursor, CoreCursorType, CoreWindow};
use windows::UI::Xaml::Controls::StackPanel;
use windows::UI::Xaml::Controls::TextBox;
use windows::UI::Xaml::FrameworkElement;
use windows::UI::Xaml::Input::PointerEventHandler;
use windows::UI::Xaml::Media::Animation::Storyboard;
use windows::UI::Xaml::RoutedEventHandler;
use windows::UI::Xaml::UIElement;

use crate::app::{as_ui_element, panel_children};
use crate::ui::session_window::SessionWindow;
use crate::ui::widgets;

/// 面板还没排过版（刚打开会话窗口）时，开终端用的行列数：与 macOS 端一致。
pub(crate) const FALLBACK_COLS: usize = 80;
pub(crate) const FALLBACK_ROWS: usize = 24;
/// 工具面板各 tab 的默认宽度（逻辑像素）：终端要放得下一屏命令，另两个窄一些。
pub(crate) const DEFAULT_TOOL_WIDTHS: [f64; 3] = [600.0, 340.0, 340.0];
const MIN_TOOL_WIDTH: f64 = 280.0;
/// 无论面板拖多宽，都给远程画面留这么多。
const MIN_STAGE_WIDTH: f64 = 360.0;
/// 换掉窗口里的鼠标指针形状。
fn set_cursor(kind: CoreCursorType) -> Result<()> {
    CoreWindow::GetForCurrentThread()?.SetPointerCursor(CoreCursor::CreateCursor(kind, 1)?)
}

impl App {
    /// 工具面板显隐切换。
    pub(crate) fn on_tool_toggle(&mut self) -> Result<()> {
        self.tool_panel_visible = !self.tool_panel_visible;
        self.set_visible("ToolPanel", self.tool_panel_visible)?;
        self.set_visible("ToolSplitter", self.tool_panel_visible)?;
        // 面板开关是切换按钮：选中态 = 面板开着
        self.set_checked_named("ToolToggle", self.tool_panel_visible)?;
        if self.tool_panel_visible {
            self.apply_tool_width()?;
            self.sync_tool_actions()?;
            self.play_storyboard("PanelIn")?;
            self.terminal_dirty = true;
        }
        Ok(())
    }

    /// 工具 tab 切换（0 终端 / 1 文件 / 2 端口）。
    pub(crate) fn on_tool_tab(&mut self, tab: u8) -> Result<()> {
        let previous = self.tool_tab;
        self.tool_tab = tab;
        self.set_visible("TerminalPane", tab == 0)?;
        self.set_visible("FilesPane", tab == 1)?;
        self.set_visible("PortmapPane", tab == 2)?;
        self.set_checked_named("ToolTabTerminal", tab == 0)?;
        self.set_checked_named("ToolTabFiles", tab == 1)?;
        self.set_checked_named("ToolTabPortmap", tab == 2)?;
        self.apply_tool_width()?;
        self.sync_tool_actions()?;
        if previous != tab {
            // 换了一屏内容：轻微淡入，视线跟得上
            self.play_storyboard("PaneIn")?;
        }
        match tab {
            0 => self.terminal_dirty = true,
            1 => self.render_transfers()?,
            _ => self.render_portmaps()?,
        }
        Ok(())
    }

    /// 面板头部右侧的操作只留当前 tab 用得上的那几个（终端：＋ / ×；文件：↑ ↓）。
    fn sync_tool_actions(&self) -> Result<()> {
        self.set_visible("TerminalActions", self.tool_tab == 0)?;
        self.set_visible("FileActions", self.tool_tab == 1)
    }

    /// 放一段写在会话窗口 XAML 里的动画（按名字取）。
    pub(crate) fn play_storyboard(&self, name: &str) -> Result<()> {
        match self.find_opt::<Storyboard>(name) {
            Some(storyboard) => storyboard.Begin(),
            None => Ok(()),
        }
    }

    /// 点了已经选中的 tab：ToggleButton 会把它取消选中，可面板其实还停在这个 tab 上——把选中态补回来。
    pub(crate) fn on_tool_tab_unchecked(&mut self, tab: u8) -> Result<()> {
        if self.tool_tab == tab {
            self.set_checked_named(
                match tab {
                    0 => "ToolTabTerminal",
                    1 => "ToolTabFiles",
                    _ => "ToolTabPortmap",
                },
                true,
            )?;
        }
        Ok(())
    }

    /// 按当前 tab 设定工具面板宽度（夹在可用范围里）；窗口拉大 / 缩小后可用范围变了，再调一次就会跟上。
    pub(crate) fn apply_tool_width(&self) -> Result<()> {
        let (min, max) = self.tool_width_range();
        let width = self.tool_widths[usize::from(self.tool_tab)].clamp(min, max);
        let panel: FrameworkElement = self.find("ToolPanel")?;
        if (panel.ActualWidth()? - width).abs() < 0.5 {
            return Ok(());
        }
        panel.SetWidth(width)
    }

    /// 面板宽度的可用范围：上限保证远程画面还有 [`MIN_STAGE_WIDTH`] 宽。
    fn tool_width_range(&self) -> (f64, f64) {
        let window = self
            .session_host
            .as_ref()
            .map_or(1024.0, SessionWindow::client_width);
        (
            MIN_TOOL_WIDTH,
            (window - MIN_STAGE_WIDTH).max(MIN_TOOL_WIDTH),
        )
    }

    /// 工具面板右缘分隔条：按下时抓住指针，拖动按相对按下点的横向位移改面板宽度。
    pub(crate) fn wire_tool_splitter(&self) -> Result<()> {
        let splitter: UIElement = self.find("ToolSplitter")?;
        let sender = self.sender();
        // 按下点的横坐标；拖动期间指针发来的位移都相对它
        let origin: Arc<Mutex<Option<f64>>> = Arc::new(Mutex::new(None));
        splitter.PointerPressed(PointerEventHandler::new({
            let (origin, sender, splitter) = (origin.clone(), sender.clone(), splitter.clone());
            move |_, args| {
                if let Some(args) = args {
                    let x = f64::from(args.GetCurrentPoint(None)?.Position()?.X);
                    *origin.lock().unwrap_or_else(|e| e.into_inner()) = Some(x);
                    if let Ok(pointer) = args.Pointer() {
                        let _ = splitter.CapturePointer(&pointer);
                    }
                    args.SetHandled(true)?;
                    sender.send(Event::ToolSplitter(Some(0.0)));
                }
                Ok(())
            }
        }))?;
        splitter.PointerMoved(PointerEventHandler::new({
            let (origin, sender) = (origin.clone(), sender.clone());
            move |_, args| {
                if let Some(args) = args {
                    let pressed = *origin.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(pressed) = pressed {
                        let x = f64::from(args.GetCurrentPoint(None)?.Position()?.X);
                        sender.send(Event::ToolSplitter(Some(x - pressed)));
                    }
                }
                Ok(())
            }
        }))?;
        splitter.PointerReleased(PointerEventHandler::new({
            let (origin, sender) = (origin, sender);
            move |_, args| {
                if let Some(args) = args {
                    args.SetHandled(true)?;
                    *origin.lock().unwrap_or_else(|e| e.into_inner()) = None;
                    sender.send(Event::ToolSplitter(None));
                }
                Ok(())
            }
        }))?;
        // 鼠标移到分隔条上换成左右箭头并亮起蓝线，离开再换回来
        let glow: UIElement = self.find("ToolSplitterGlow")?;
        let glow_in = glow.clone();
        splitter.PointerEntered(PointerEventHandler::new(move |_, _| {
            let _ = glow_in.SetOpacity(1.0);
            set_cursor(CoreCursorType::SizeWestEast)
        }))?;
        splitter.PointerExited(PointerEventHandler::new(move |_, _| {
            let _ = glow.SetOpacity(0.0);
            set_cursor(CoreCursorType::Arrow)
        }))?;
        Ok(())
    }

    /// 分隔条事件：`Some(位移)` 是按下 / 拖动，`None` 是松开。
    pub(crate) fn on_tool_splitter(&mut self, drag: Option<f64>) -> Result<()> {
        let tab = usize::from(self.tool_tab);
        match drag {
            Some(delta) => {
                let panel: FrameworkElement = self.find("ToolPanel")?;
                let start = *self
                    .tool_splitter_drag
                    .get_or_insert_with(|| panel.ActualWidth().unwrap_or(DEFAULT_TOOL_WIDTHS[tab]));
                let (min, max) = self.tool_width_range();
                self.tool_widths[tab] = (start + delta).clamp(min, max);
                self.apply_tool_width()
            }
            None => {
                self.tool_splitter_drag = None;
                Ok(())
            }
        }
    }

    /// 设置复选类按钮（`ToggleButton`：工具 tab 等）的选中状态。
    pub(crate) fn set_checked_named(&self, name: &str, on: bool) -> Result<()> {
        let toggle: windows::UI::Xaml::Controls::Primitives::ToggleButton = self.find(name)?;
        set_checked(&toggle, on)
    }

    /// 设置拨动开关（`ToggleSwitch`：被控开关、自启开关等）的开 / 关。
    ///
    /// `ToggleSwitch` 不是 `ToggleButton`：对它用 [`Self::set_checked_named`] 会因为
    /// 类型不符而落空——开关显示的状态就和实际对不上了。
    pub(crate) fn set_switch_on(&self, name: &str, on: bool) -> Result<()> {
        let switch: windows::UI::Xaml::Controls::ToggleSwitch = self.find(name)?;
        switch.SetIsOn(on)
    }

    /// 打开新终端：按画面实际放得下的行列数开（面板还没排过版就用默认值）。
    pub(crate) fn on_terminal_new(&mut self) -> Result<()> {
        let Some(active) = self.session.as_ref() else {
            #[cfg(debug_assertions)]
            if self.in_terminal_demo() {
                return self.terminal_demo_toggle(true);
            }
            return self.set_terminal_hint("未连接会话。");
        };
        // 把要用的值先取出来：后面要可变借用 self（提示、重置画面）
        let (watch_only, tools) = (active.watch_only, active.handle.tools());
        if watch_only {
            return self.set_terminal_hint("只读观看模式下不能使用终端。");
        }
        if !self.terminal_allowed {
            return self.set_terminal_hint("对方不允许本会话使用终端。");
        }
        let (cols, rows) = if self.terminal_view.measured() {
            self.terminal.size()
        } else {
            (FALLBACK_COLS, FALLBACK_ROWS)
        };
        // 新终端从干净的画面开始；随后远端送来的第一条就是它的提示符
        self.terminal.reset();
        self.terminal_dirty = true;
        self.set_terminal_hint("正在打开终端…")?;
        let dimension = |value: usize| u16::try_from(value).unwrap_or(u16::MAX);
        match tools.terminal_open_with_options(dimension(cols), dimension(rows), None, true) {
            Ok(_) => Ok(()),
            Err(err) => self.set_terminal_hint(&format!("打开失败：{err}")),
        }
    }

    /// 结束当前终端（远端的进程随之退出，画面留到远端报告结束）。
    pub(crate) fn on_terminal_close(&mut self) -> Result<()> {
        let Some(term_id) = self.terminal_id else {
            return Ok(());
        };
        let Some(active) = self.session.as_ref() else {
            #[cfg(debug_assertions)]
            if self.in_terminal_demo() {
                return self.terminal_demo_toggle(false);
            }
            return Ok(());
        };
        if let Err(err) = active.handle.tools().terminal_close(term_id) {
            return self.set_terminal_hint(&format!("结束失败：{err}"));
        }
        Ok(())
    }

    /// 把终端当前的行列数告诉远端（画面尺寸变了 / 刚开好或附着上终端时）。
    fn notify_terminal_resize(&mut self) -> Result<()> {
        let (Some(term_id), Some(active)) = (self.terminal_id, self.session.as_ref()) else {
            return Ok(());
        };
        let (cols, rows) = self.terminal.size();
        let dimension = |value: usize| u16::try_from(value).unwrap_or(u16::MAX);
        match active
            .handle
            .tools()
            .terminal_resize(term_id, dimension(cols), dimension(rows))
        {
            Ok(()) => Ok(()),
            Err(err) => self.set_terminal_hint(&format!("调整终端尺寸失败：{err}")),
        }
    }

    /// 终端事件排空（on_tick 调用；终端输出交给屏幕模型，由 dirty 标志在本 tick 末统一重画）。
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
                // 刚被结束的那个终端迟到的输出不再上屏
                TerminalEvent::Output { term_id, bytes, .. }
                    if Some(term_id) == self.terminal_id =>
                {
                    self.terminal.feed_output(&bytes);
                    self.terminal_blink.touch(self.tick);
                    self.terminal_dirty = true;
                }
                TerminalEvent::Opened {
                    term_id, ok, error, ..
                } => {
                    if ok {
                        self.open_terminal(term_id);
                    } else {
                        let _ = self
                            .set_terminal_hint(&format!("打开失败：{}", error.unwrap_or_default()));
                    }
                }
                TerminalEvent::Attached {
                    term_id, ok, error, ..
                } => {
                    if ok {
                        self.open_terminal(term_id);
                    } else {
                        let _ = self
                            .set_terminal_hint(&format!("附着失败：{}", error.unwrap_or_default()));
                    }
                }
                TerminalEvent::Exited { term_id, .. } if self.terminal_id == Some(term_id) => {
                    self.terminal_id = None;
                    self.terminal.feed_output(
                        format!("\r\n\x1b[90m[终端 #{term_id} 已结束]\x1b[0m\r\n").as_bytes(),
                    );
                    self.terminal_dirty = true;
                    let _ = self.set_terminal_hint("终端已结束，可点击「新终端」重开");
                }
                _ => {}
            }
        }
        // 屏幕模型对远端查询（DA / DSR）的应答写回去：依赖应答的全屏程序不必干等
        let replies = self.terminal.take_responses();
        if !replies.is_empty() {
            let _ = self.send_terminal_input(&replies);
        }
    }

    /// 远端确认终端开好 / 附着上了：对齐它与本地的行列数。
    fn open_terminal(&mut self, term_id: u32) {
        self.terminal_id = Some(term_id);
        self.terminal_dirty = true;
        let _ = self.notify_terminal_resize();
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

    /// 传输队列：每条一张卡片（方向 / 文件名 / 进度 / 状态），传输中的可以取消。
    pub(crate) fn render_transfers(&mut self) -> Result<()> {
        let Some(panel) = self.find_opt::<StackPanel>("TransferList") else {
            return Ok(());
        };
        let children = panel_children(&panel)?;
        children.Clear()?;
        let items = if let Some(active) = self.session.as_ref() {
            active.handle.tools().file_list_transfers()
        } else {
            self.transfers.clone()
        };
        if items.is_empty() {
            children.Append(&as_ui_element(&widgets::empty_hint(
                "还没有传输任务",
                "用面板右上角的 ↑ 上传文件、↓ 下载远端文件",
            )?)?)?;
            return Ok(());
        }
        for item in items.iter().rev().take(30) {
            let upload = matches!(item.direction, TransferDirection::Upload);
            let percent = if item.total_bytes > 0 {
                (item.done_bytes as f64 / item.total_bytes as f64 * 100.0)
                    .round()
                    .clamp(0.0, 100.0)
            } else if item.status == TransferStatus::Done {
                100.0
            } else {
                0.0
            };
            let (state, status) = match item.status {
                TransferStatus::Active => (
                    widgets::TransferRowState::Active,
                    format!(
                        "传输中 · {} / {}",
                        fmt_bytes(item.done_bytes),
                        fmt_bytes(item.total_bytes)
                    ),
                ),
                TransferStatus::Done => (
                    widgets::TransferRowState::Done,
                    format!(
                        "已完成 · {}",
                        fmt_bytes(item.done_bytes.max(item.total_bytes))
                    ),
                ),
                TransferStatus::Failed => (
                    widgets::TransferRowState::Failed,
                    format!(
                        "失败 · {}",
                        item.error.clone().unwrap_or_else(|| "未知原因".to_string())
                    ),
                ),
                TransferStatus::Cancelled => (
                    widgets::TransferRowState::Cancelled,
                    format!("已取消 · {}", fmt_bytes(item.done_bytes)),
                ),
            };
            let (row, cancel) = widgets::transfer_row(upload, &item.name, &status, percent, state)?;
            if let Some(cancel) = cancel {
                let sender = self.sender();
                let id = item.id;
                cancel.Click(RoutedEventHandler::new(move |_, _| {
                    sender.send(Event::Click(Action::TransferCancel(id)));
                    Ok(())
                }))?;
            }
            children.Append(&as_ui_element(&row)?)?;
        }
        Ok(())
    }

    /// 端口映射列表：每条一张卡片（名称 / 目标 / 收发量 / 状态），可暂停 / 启用、删除。
    pub(crate) fn render_portmaps(&mut self) -> Result<()> {
        let Some(panel) = self.find_opt::<StackPanel>("PortmapList") else {
            return Ok(());
        };
        let children = panel_children(&panel)?;
        children.Clear()?;
        let rules = if let Some(active) = self.session.as_ref() {
            active.handle.tools().portmap_list()
        } else {
            Vec::new()
        };
        if rules.is_empty() {
            children.Append(&as_ui_element(&widgets::empty_hint(
                "还没有映射规则",
                "填好上面的表单，点「添加映射」",
            )?)?)?;
            return Ok(());
        }
        for rule in &rules {
            let name = if rule.name.trim().is_empty() {
                format!("本地端口 {}", rule.local_port)
            } else {
                rule.name.clone()
            };
            let detail = format!(
                "{}:{} · 收 {} · 发 {}",
                rule.target_addr,
                rule.target_port,
                fmt_bytes(rule.bytes_rx),
                fmt_bytes(rule.bytes_tx)
            );
            let state = if !rule.enabled {
                widgets::PortRowState::Paused
            } else {
                match rule.status {
                    PortMapStatus::Fail => widgets::PortRowState::Failed,
                    PortMapStatus::Ok | PortMapStatus::Pending => widgets::PortRowState::Running,
                }
            };
            let (row, toggle, delete) = widgets::portmap_row(&name, &detail, state)?;
            {
                // 没启用就是「启用」，否则是「暂停」
                let sender = self.sender();
                let (id, enable) = (rule.id, !rule.enabled);
                toggle.Click(RoutedEventHandler::new(move |_, _| {
                    sender.send(Event::Click(Action::PortmapRule(
                        id,
                        if enable { 1 } else { 2 },
                    )));
                    Ok(())
                }))?;
            }
            {
                let sender = self.sender();
                let id = rule.id;
                delete.Click(RoutedEventHandler::new(move |_, _| {
                    sender.send(Event::Click(Action::PortmapRule(id, 0)));
                    Ok(())
                }))?;
            }
            children.Append(&as_ui_element(&row)?)?;
        }
        Ok(())
    }

    /// 本 tick 末统一渲染工具面板（脏标志驱动）。
    pub(crate) fn finish_tools_render(&mut self) -> Result<()> {
        if self.tool_tab == 0 {
            self.terminal_tick()?;
        }
        if self.transfers_dirty {
            self.transfers_dirty = false;
            if self.tool_tab == 1 {
                self.render_transfers()?;
            }
        }
        Ok(())
    }

    /// 会话结束清理（清空终端 / 传输状态）。
    pub(crate) fn reset_tools_state(&mut self) {
        self.terminal.reset();
        self.terminal_view.invalidate();
        self.terminal_id = None;
        self.terminal_allowed = true;
        self.terminal_lock = Default::default();
        self.terminal_lock_username = None;
        self.terminal_dirty = true;
        self.transfers.clear();
        self.transfers_dirty = true;
        if self.session_host.is_some() {
            let _ = self.sync_terminal_lock();
            let _ = self.set_terminal_hint("");
        }
    }
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
