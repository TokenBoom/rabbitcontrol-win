//! 会话页：连接流、视图渲染与输入注入。

use super::{super::actions::*, super::App};
use windows::core::Result;

use std::{sync::Arc, thread};

use rc_client_core::platform::PreallocatedRenderSurface;
use rc_client_core::session_bridge::SessionHandle;
use rc_input::InputEvent;
use windows::core::Interface;
use windows::core::HSTRING;
use windows::System::VirtualKey;
use windows::UI::Xaml::Controls::Control;
use windows::UI::Xaml::Controls::{
    FontIcon, IconElement, MenuFlyout, MenuFlyoutItem, MenuFlyoutItemBase, MenuFlyoutSubItem,
    PasswordBox, SwapChainPanel,
};
use windows::UI::Xaml::Media::Animation::Storyboard;
use windows::UI::Xaml::RoutedEventHandler;
use windows::UI::Xaml::{Controls::TextBlock, FrameworkElement, UIElement};

use rc_client_core::session_bridge::SessionCommand;

use crate::biz::session as sess;
use crate::biz::session::SessionView;

impl App {
    pub(crate) fn stage_size(&self) -> (f64, f64) {
        self.find_opt::<SwapChainPanel>("SessionPanel")
            .and_then(|panel| panel.cast::<UIElement>().ok())
            .and_then(|e| e.cast::<FrameworkElement>().ok())
            .and_then(|element| {
                let w = element.ActualWidth().ok()?;
                let h = element.ActualHeight().ok()?;
                Some((w, h))
            })
            .unwrap_or((0.0, 0.0))
    }

    pub(crate) fn on_session_pointer(&mut self, event: SessionPointerEvent) -> Result<()> {
        // 直接字段访问：session 借用与 pressed_* 修改字段不相交。
        let Some(active) = self.session.as_ref() else {
            return Ok(());
        };
        if !matches!(self.session_view, SessionView::Negotiated { .. }) || active.watch_only {
            return Ok(());
        }
        let handle = &active.handle;
        let video = active.video;
        let stage = self.stage_size();
        if stage.0 <= 0.0 || stage.1 <= 0.0 {
            return Ok(());
        }
        match event {
            SessionPointerEvent::Move { x, y } => {
                let (nx, ny) = sess::normalize_in_video(x, y, stage.0, stage.1, video);
                handle.send_input(InputEvent::MouseMove { x: nx, y: ny });
            }
            SessionPointerEvent::Button {
                x,
                y,
                button,
                pressed,
            } => {
                let (nx, ny) = sess::normalize_in_video(x, y, stage.0, stage.1, video);
                handle.send_input(InputEvent::MouseMove { x: nx, y: ny });
                if pressed {
                    if !self.pressed_buttons.contains(&button) {
                        self.pressed_buttons.push(button);
                    }
                } else {
                    self.pressed_buttons.retain(|b| *b != button);
                }
                handle.send_input(InputEvent::MouseButton {
                    button: match button {
                        0 => rc_input::MouseButton::Left,
                        1 => rc_input::MouseButton::Right,
                        _ => rc_input::MouseButton::Middle,
                    },
                    pressed,
                });
            }
            SessionPointerEvent::Wheel { delta } => {
                handle.send_input(InputEvent::MouseWheel {
                    delta: sess::wheel_to_lines(delta as i16),
                    horizontal: false,
                });
            }
        }
        Ok(())
    }

    pub(crate) fn on_session_key(&mut self, event: SessionKeyEvent) -> Result<()> {
        // Esc：全屏态下本地消费（退出全屏，不注入远端）
        if event.pressed && event.key == VirtualKey::Escape && self.exit_fullscreen_if_active() {
            return Ok(());
        }
        let Some(active) = self.session.as_ref() else {
            return Ok(());
        };
        if !matches!(self.session_view, SessionView::Negotiated { .. }) || active.watch_only {
            return Ok(());
        }
        let handle = &active.handle;
        // 修饰键跟踪（供其余按键携带位标志）。
        let mod_bit = match event.key {
            VirtualKey::LeftControl | VirtualKey::RightControl => Some(0u8),
            VirtualKey::LeftShift | VirtualKey::RightShift => Some(1),
            VirtualKey::LeftMenu | VirtualKey::RightMenu => Some(2),
            VirtualKey::LeftWindows | VirtualKey::RightWindows => Some(3),
            _ => None,
        };
        if let Some(bit) = mod_bit {
            if event.pressed {
                self.pressed_modifiers |= 1 << bit;
            } else {
                self.pressed_modifiers &= !(1 << bit);
            }
        }
        let Some(code) = Self::vkey_to_dom(event.key) else {
            return Ok(());
        };
        let Some(scancode) = rc_client_core::input_bridge::scancode_for(&code) else {
            return Ok(());
        };
        if event.pressed {
            if !self.pressed_keys.contains(&scancode) {
                self.pressed_keys.push(scancode);
            }
        } else {
            self.pressed_keys.retain(|k| *k != scancode);
        }
        handle.send_input(InputEvent::Key {
            scancode,
            pressed: event.pressed,
            modifiers: self.pressed_modifiers,
        });
        Ok(())
    }

    pub(crate) fn release_pressed_input(&mut self) {
        let Some(active) = self.session.as_ref() else {
            return;
        };
        let modifiers = self.pressed_modifiers;
        for scancode in self.pressed_keys.drain(..) {
            active.handle.send_input(InputEvent::Key {
                scancode,
                pressed: false,
                modifiers,
            });
        }
        for button in self.pressed_buttons.drain(..) {
            active.handle.send_input(InputEvent::MouseButton {
                button: match button {
                    0 => rc_input::MouseButton::Left,
                    1 => rc_input::MouseButton::Right,
                    _ => rc_input::MouseButton::Middle,
                },
                pressed: false,
            });
        }
        self.pressed_modifiers = 0;
    }

    /// M3.6：确保独立会话窗口存在并显示（复用既有窗口；首次创建后接线）。
    pub(crate) fn ensure_session_window(&mut self, title: &str) -> Result<()> {
        if self.session_host.is_none() {
            self.session_host = Some(crate::ui::session_window::SessionWindow::new().map_err(
                |e| {
                    eprintln!("[session_window] 创建失败：{e}");
                    e
                },
            )?);
            self.wire_session_window()?;
        }
        if let Some(host) = self.session_host.as_ref() {
            host.set_title(&format!("{title} - RabbitControl 远程"));
            host.show();
        }
        Ok(())
    }

    /// 会话窗口画面面板的交换链（预分配渲染表面）。
    fn session_surface(&self) -> Result<PreallocatedRenderSurface> {
        let panel: SwapChainPanel = self.find("SessionPanel")?;
        crate::media::graphics::session_render_surface(&panel).map_err(|err| {
            windows::core::Error::new(
                windows::core::HRESULT(0x8000_4005u32 as i32), // E_FAIL
                format!("会话画面表面：{err}").into(),
            )
        })
    }

    /// Esc 退出全屏（在注入远端之前本地消费）。
    fn exit_fullscreen_if_active(&mut self) -> bool {
        if !self.fullscreen {
            return false;
        }
        if let Some(host) = self.session_host.as_ref() {
            self.fullscreen = host.toggle_fullscreen();
        }
        true
    }

    pub(crate) fn start_session(&mut self, code: &str) -> Result<()> {
        if !self.logged_in {
            self.label("AssistError", "请先登录。")?;
            return Ok(());
        }
        if self.runtime.snapshot().and_then(|s| s.device_id).is_none() {
            self.label("AssistError", "信令尚未在线，请稍候重试。")?;
            return Ok(());
        }
        let password = self
            .find_opt::<PasswordBox>("PeerPasswordBox")
            .map(|box_| box_.Password().unwrap_or_default().to_string())
            .filter(|p| !p.is_empty());
        // M3.3：仅观看勾选框读取（ToggleButton.IsChecked → IReference<bool>）
        let watch_only = self
            .find_opt::<windows::UI::Xaml::Controls::CheckBox>("WatchOnlyCheck")
            .and_then(|c| {
                c.cast::<windows::UI::Xaml::Controls::Primitives::ToggleButton>()
                    .ok()
            })
            .and_then(|t| t.IsChecked().ok())
            .and_then(|v| v.Value().ok())
            .unwrap_or(false);
        self.peer_watch_only = watch_only;
        let (bitrate_preset, encrypt_connection) = {
            let prefs = self.prefs.lock().unwrap();
            (
                prefs.bitrate_preset.clone(),
                prefs.default_encrypt_connection,
            )
        };
        let params = sess::ConnectParams {
            password,
            watch_only,
            bitrate_bps: sess::bitrate_preset_to_bps(&bitrate_preset),
            fps: 60,
            lan_direct: self
                .runtime
                .lan_peers()
                .iter()
                .find(|p| p.device_id.as_deref() == Some(code))
                .map(|p| p.addr.clone()),
            signal_client: self.runtime.signal_client(),
            encrypt_connection,
        };
        let name = self
            .devices
            .iter()
            .find(|d| d.device_id == code)
            .map(|d| self.device_name_of(d))
            .unwrap_or_else(|| code.to_string());
        // M3.6：会话 UI 迁入独立窗口；主窗口留在当前页。
        // 先把会话窗口建出来，再改它里面的元素（顺序反了查找会落空）。
        self.host_screen_hint = None;
        self.ensure_session_window(&name)?;
        self.set_session_overlay("正在连接…", true);
        let _ = self.set_visible("SessionReconnect", false);
        // 画面面板的交换链必须在界面线程创建，再随会话交给渲染器。不交的话，解码出来的
        // 画面只会画在一个看不见的离屏表面上，会话窗口一直是黑的。
        let surface = self.session_surface()?;
        let manager = self.auth.manager();
        let runtime = Arc::clone(&self.runtime);
        let events = self.sender();
        let code = code.to_string();
        thread::spawn(move || {
            let result = runtime
                .request_connect(&code, sess::CONNECT_TIMEOUT)
                .and_then(|grant| sess::build_session_config(&manager, grant, &params));
            let outcome = match result {
                Ok(cfg) => Ok(SessionHandle::spawn_with_surface(cfg, Some(surface))),
                Err(err) => Err(err),
            };
            events.send(Event::SessionLaunched(
                Box::new(SessionLaunchOutcome {
                    result: outcome,
                    target_id: code,
                    target_name: name,
                }),
                String::new(),
                String::new(),
            ));
        });
        Ok(())
    }

    pub(crate) fn apply_session_view(&mut self) {
        let view = self.session_view.clone();
        let target = self
            .session
            .as_ref()
            .map(|s| s.target_name.clone())
            .unwrap_or_default();
        match &view {
            SessionView::Idle => {}
            SessionView::Connecting(text) => {
                let _ = self.label("SessionStatus", text);
                let _ = self.session_dot(PENDING_DOT);
                let _ = self.set_visible("SessionBackendChip", false);
                self.set_session_overlay(text, true);
            }
            SessionView::Negotiated {
                width,
                height,
                fps,
                transport,
                mtls_active: _,
                watch_only,
                ..
            } => {
                // 状态条：左边只放设备名，细节收进旁边的小标签
                let _ = self.label("SessionStatus", &target);
                let _ = self.label(
                    "SessionBackend",
                    &sess::session_summary(*width, *height, *fps, transport, *watch_only),
                );
                let _ = self.set_visible("SessionBackendChip", true);
                let _ = self.session_dot(ONLINE_DOT);
                match self.host_screen_hint {
                    Some(hint) => self.set_session_overlay(hint, false),
                    None => self.set_session_overlay("", false),
                }
                self.apply_hud();
            }
            SessionView::Ended(reason) => {
                let _ = self.label("SessionStatus", &format!("{target} · 已断开"));
                let _ = self.session_dot(ENDED_DOT);
                let _ = self.set_visible("SessionBackendChip", false);
                self.set_session_overlay(&format!("连接已断开：{reason}"), false);
                let _ = self.set_visible("SessionReconnect", true);
                let _ = self.set_visible("SessionHudPanel", false);
            }
        }
    }

    /// HUD 快照 → 左下浮层（core 1s 节流；受设置页 show_hud 开关控制）。
    pub(crate) fn apply_hud(&mut self) {
        let negotiated = matches!(self.session_view, SessionView::Negotiated { .. });
        let show = self.prefs.lock().unwrap().show_hud;
        if !negotiated || !show {
            let _ = self.set_visible("SessionHudPanel", false);
            return;
        }
        let Some(snap) = self.session_hud.clone() else {
            return;
        };
        let _ = self.label("SessionHud", &sess::format_hud(&snap));
        let _ = self.set_visible("SessionHudPanel", true);
    }

    /// 「显示与画质」菜单：候选只在被控端能力（预设 / 编解码 / 最高帧率）变化时重建；
    /// 当前生效项每次都对齐（热切换后被控端回传权威值）。
    pub(crate) fn sync_quality_menu(&mut self) -> Result<()> {
        let bitrate_preset = self.prefs.lock().unwrap().bitrate_preset.clone();
        let groups =
            sess::setting_groups(&self.quality, &self.quality_fps_options, &bitrate_preset);
        let signature: String = groups
            .iter()
            .map(|group| format!("{:?}|{:?}|{}", group.setting, group.options, group.enabled))
            .collect();
        if signature != self.quality_sig {
            self.quality_sig = signature;
            self.rebuild_quality_menu(&groups)?;
        }
        self.quality_menu = groups;
        self.mark_quality_selection()?;
        // 没谈下来之前没有可调的东西（演示窗口例外：值是摆的，菜单要能点）
        if let Some(button) = self.find_opt::<Control>("SessionQuality") {
            button.SetIsEnabled(self.quality_menu_interactive())?;
        }
        Ok(())
    }

    /// 画质菜单此刻能不能操作：谈成了，或者正在演示。
    fn quality_menu_interactive(&self) -> bool {
        if matches!(self.session_view, SessionView::Negotiated { .. }) {
            return true;
        }
        #[cfg(debug_assertions)]
        if self.in_terminal_demo() {
            return true;
        }
        false
    }

    /// 终端演示窗口（仅调试构建有）：没有会话，也没有真实被控端。
    #[cfg(debug_assertions)]
    pub(super) fn in_terminal_demo(&self) -> bool {
        self.terminal_demo.is_some()
    }

    /// 按选项组重建菜单：每组一个带图标的子菜单，选项点了发 `StreamSettingPicked`。
    /// 菜单项元素留着：之后只改它们前面的勾，不重建。
    fn rebuild_quality_menu(&mut self, groups: &[sess::SettingGroup]) -> Result<()> {
        let Some(flyout) = self.find_opt::<MenuFlyout>("QualityMenu") else {
            return Ok(());
        };
        let items = flyout.Items()?;
        items.Clear()?;
        self.quality_items.clear();
        for group in groups {
            // 只有一块显示器（或对端没报）：不摆「显示器」这一项
            if group.setting == StreamSetting::Display && group.options.len() < 2 {
                continue;
            }
            let submenu = MenuFlyoutSubItem::new()?;
            // 0.37 投影不把 Text / Icon / Items 挂到基类上：设置用具体类型，入列表时再转基类
            submenu.SetText(HSTRING::from(group.setting.title()))?;
            submenu.SetIcon(&menu_icon(group.setting.icon(), 14.0)?)?;
            submenu.cast::<Control>()?.SetIsEnabled(group.enabled)?;
            let mut row = Vec::new();
            for (index, option) in group.options.iter().enumerate() {
                let item = MenuFlyoutItem::new()?;
                item.SetText(HSTRING::from(option.as_str()))?;
                let sender = self.sender();
                let setting = group.setting;
                item.Click(RoutedEventHandler::new(move |_, _| {
                    sender.send(Event::Click(Action::StreamSettingPicked(setting, index)));
                    Ok(())
                }))?;
                submenu
                    .Items()?
                    .Append(&item.clone().cast::<MenuFlyoutItemBase>()?)?;
                row.push(item);
            }
            items.Append(&submenu.cast::<MenuFlyoutItemBase>()?)?;
            self.quality_items.push(row);
        }
        Ok(())
    }

    /// 给当前生效项打勾（其它项留一个空图标占位，文案才对齐）。
    fn mark_quality_selection(&self) -> Result<()> {
        for (group, row) in self.quality_menu.iter().zip(&self.quality_items) {
            for (index, item) in row.iter().enumerate() {
                item.SetIcon(&menu_icon(
                    if index == group.selected {
                        "\u{E73E}"
                    } else {
                        " "
                    },
                    12.0,
                )?)?;
            }
        }
        Ok(())
    }

    /// 菜单里选了一项：预钳制后下发（与当前生效值一致时跳过，避免空切换）。
    pub(crate) fn on_stream_setting_picked(
        &mut self,
        setting: StreamSetting,
        index: usize,
    ) -> Result<()> {
        if !self.quality_menu_interactive() {
            return Ok(());
        }
        // 演示窗口：没有会话可下发，只挪菜单里的勾，也不动真实的偏好
        #[cfg(debug_assertions)]
        let demo = self.in_terminal_demo();
        #[cfg(not(debug_assertions))]
        let demo = false;
        let group = |setting| {
            self.quality_menu
                .iter()
                .find(|group| group.setting == setting)
        };
        let selected = |setting| group(setting).map_or(0, |group| group.selected);
        if let Some(active) = self.session.as_ref().filter(|_| !demo) {
            let handle = &active.handle;
            match setting {
                StreamSetting::Resolution | StreamSetting::Fps => {
                    let (resolution, fps) = if setting == StreamSetting::Resolution {
                        (index, selected(StreamSetting::Fps))
                    } else {
                        (selected(StreamSetting::Resolution), index)
                    };
                    let (width, height, fps) = sess::stream_request(
                        resolution,
                        fps,
                        &self.quality,
                        &self.quality_fps_options,
                    );
                    if sess::stream_request_unchanged((width, height, fps), &self.quality) {
                        return Ok(());
                    }
                    // 预钳制（ios 踩坑：越界静默无流，绝不直接透传）
                    let Some((width, height, fps)) =
                        sess::clamp_stream_request(width, height, fps, &self.quality)
                    else {
                        return Ok(());
                    };
                    handle.send(SessionCommand::SetStreamParams { width, height, fps });
                }
                // 码率：自适应（0）不下发
                StreamSetting::Bitrate => {
                    let Some(bps) = sess::BITRATE_STEPS_BPS.get(index).copied() else {
                        return Ok(());
                    };
                    if bps != 0 {
                        handle.send(SessionCommand::SetBitrate { bps });
                    }
                }
                // 编解码：跟随当前（0）不下发
                StreamSetting::Codec => {
                    if let Some(codec) = self
                        .quality
                        .supported_codecs
                        .get(index.saturating_sub(1))
                        .filter(|_| index > 0)
                    {
                        let codec = if codec == "hevc" {
                            rc_client_core::display_ctrl::StreamCodec::Hevc
                        } else {
                            rc_client_core::display_ctrl::StreamCodec::H264
                        };
                        handle.send(SessionCommand::SetCodec { codec });
                    }
                }
                // 鼠标模式：与当前生效一致时跳过
                StreamSetting::Mouse => {
                    if let Some((mode, _)) = sess::MOUSE_MODES.get(index) {
                        if self.quality.mouse_mode.as_deref() != Some(*mode) {
                            handle.send_display_ctrl(
                                rc_client_core::display_ctrl::DisplayControlMsg::SetMouseMode {
                                    mode: (*mode).to_string(),
                                },
                            );
                        }
                    }
                }
                StreamSetting::Display => {
                    if let Some((display_id, _)) = self.quality.displays.get(index) {
                        if *display_id != self.quality.active_display_id {
                            handle.send_display_ctrl(
                                rc_client_core::display_ctrl::DisplayControlMsg::SwitchDisplay {
                                    display_id: *display_id,
                                },
                            );
                        }
                    }
                }
            }
        }
        if setting == StreamSetting::Bitrate && !demo {
            // 码率档位记进偏好：下次连接沿用，菜单的勾也以它为准
            let preset = sess::BITRATE_PRESETS[index.min(sess::BITRATE_PRESETS.len() - 1)];
            self.save_prefs_patch(|prefs| {
                prefs.apply_stream_patch(rc_client_core::app_prefs::StreamPrefsPatch {
                    bitrate_preset: Some(preset.to_string()),
                    ..Default::default()
                })
            })?;
        }
        if let Some(group) = self.quality_menu.iter_mut().find(|g| g.setting == setting) {
            group.selected = index;
        }
        self.mark_quality_selection()
    }

    /// 状态条左侧的圆点：连接中琥珀色、已连上绿色、已断开红色。
    pub(super) fn session_dot(&self, color: windows::UI::Color) -> Result<()> {
        let Some(dot) = self.find_opt::<windows::UI::Xaml::Shapes::Ellipse>("SessionDot") else {
            return Ok(());
        };
        let brush = windows::UI::Xaml::Media::SolidColorBrush::new()?;
        brush.SetColor(color)?;
        let shape: windows::UI::Xaml::Shapes::Shape = dot.cast()?;
        shape.SetFill(&brush)
    }

    pub(crate) fn set_session_overlay(&self, text: &str, spinner: bool) {
        let _ = self.label("SessionOverlayText", text);
        let _ = self.set_visible("SessionSpinner", spinner);
        // 连接中的三点脉冲是一段循环动画：开始 / 停下由这里控制
        if let Some(storyboard) = self.find_opt::<Storyboard>("SpinnerPulse") {
            if spinner {
                let _ = storyboard.Begin();
            } else {
                let _ = storyboard.Stop();
            }
        }
        let visible = !text.is_empty() || spinner;
        let _ = self.set_visible("SessionOverlay", visible);
    }

    pub(crate) fn session_overlay_text(&self) -> String {
        self.find_opt::<TextBlock>("SessionOverlayText")
            .and_then(|t| t.Text().ok())
            .map(|t| t.to_string())
            .unwrap_or_default()
    }

    pub(super) fn vkey_to_dom(key: VirtualKey) -> Option<String> {
        use VirtualKey as V;
        let code = match key {
            V::A => "KeyA",
            V::B => "KeyB",
            V::C => "KeyC",
            V::D => "KeyD",
            V::E => "KeyE",
            V::F => "KeyF",
            V::G => "KeyG",
            V::H => "KeyH",
            V::I => "KeyI",
            V::J => "KeyJ",
            V::K => "KeyK",
            V::L => "KeyL",
            V::M => "KeyM",
            V::N => "KeyN",
            V::O => "KeyO",
            V::P => "KeyP",
            V::Q => "KeyQ",
            V::R => "KeyR",
            V::S => "KeyS",
            V::T => "KeyT",
            V::U => "KeyU",
            V::V => "KeyV",
            V::W => "KeyW",
            V::X => "KeyX",
            V::Y => "KeyY",
            V::Z => "KeyZ",
            V::Number0 => "Digit0",
            V::Number1 => "Digit1",
            V::Number2 => "Digit2",
            V::Number3 => "Digit3",
            V::Number4 => "Digit4",
            V::Number5 => "Digit5",
            V::Number6 => "Digit6",
            V::Number7 => "Digit7",
            V::Number8 => "Digit8",
            V::Number9 => "Digit9",
            V::Space => "Space",
            V::Enter => "Enter",
            V::Escape => "Escape",
            V::Back => "Backspace",
            V::Tab => "Tab",
            V::Delete => "Delete",
            V::Home => "Home",
            V::End => "End",
            V::PageUp => "PageUp",
            V::PageDown => "PageDown",
            V::Up => "ArrowUp",
            V::Down => "ArrowDown",
            V::Left => "ArrowLeft",
            V::Right => "ArrowRight",
            _ => return None,
        };
        Some(code.to_string())
    }
}

/// 状态点的三种颜色（与会话窗口的深色令牌一致）。
const PENDING_DOT: windows::UI::Color = windows::UI::Color {
    A: 255,
    R: 0xF0,
    G: 0xB4,
    B: 0x29,
};
const ONLINE_DOT: windows::UI::Color = windows::UI::Color {
    A: 255,
    R: 0x3F,
    G: 0xB9,
    B: 0x50,
};
const ENDED_DOT: windows::UI::Color = windows::UI::Color {
    A: 255,
    R: 0xFF,
    G: 0x6B,
    B: 0x6B,
};
/// 终端演示窗口的状态点：品牌蓝——不是在连，也不是连上了，只是「演示」。
#[cfg(debug_assertions)]
pub(super) const DEMO_DOT: windows::UI::Color = windows::UI::Color {
    A: 255,
    R: 0x7F,
    G: 0xA6,
    B: 0xFF,
};

/// 菜单里的图标（Segoe MDL2 Assets）。
fn menu_icon(glyph: &str, size: f64) -> Result<IconElement> {
    let icon = FontIcon::new()?;
    icon.SetFontFamily(
        windows::UI::Xaml::Media::FontFamily::CreateInstanceWithName(HSTRING::from(
            "Segoe MDL2 Assets",
        ))?,
    )?;
    icon.SetGlyph(HSTRING::from(glyph))?;
    icon.SetFontSize(size)?;
    icon.cast()
}
