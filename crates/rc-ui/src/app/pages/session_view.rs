//! 会话页：连接流、视图渲染与输入注入。

use super::{super::actions::*, super::App};
use windows::core::Result;

use std::{sync::Arc, thread};

use rc_client_core::platform::PreallocatedRenderSurface;
use rc_client_core::session_bridge::SessionHandle;
use rc_input::InputEvent;
use windows::core::Interface;
use windows::System::VirtualKey;
use windows::UI::Xaml::Controls::{PasswordBox, SwapChainPanel};
use windows::UI::Xaml::{Controls::TextBlock, FrameworkElement, UIElement};

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
        match &view {
            SessionView::Idle => {}
            SessionView::Connecting(text) => {
                let _ = self.label("SessionStatus", text);
                self.set_session_overlay(text, true);
            }
            SessionView::Negotiated {
                width,
                height,
                fps,
                backend,
                transport,
                mtls_active: _,
                watch_only,
            } => {
                let mode = if *watch_only { " · 观看" } else { "" };
                let status = format!(
                    "{target} · {w}×{h}@{fps}{mode}",
                    target = self
                        .session
                        .as_ref()
                        .map(|s| s.target_name.as_str())
                        .unwrap_or(""),
                    w = width,
                    h = height,
                    fps = fps,
                );
                let _ = self.label("SessionStatus", &status);
                let badge = format!("{backend} · {transport}",);
                let _ = self.label("SessionBackend", &badge);
                match self.host_screen_hint {
                    Some(hint) => self.set_session_overlay(hint, false),
                    None => self.set_session_overlay("", false),
                }
                self.apply_hud();
            }
            SessionView::Ended(reason) => {
                let _ = self.label("SessionStatus", &format!("已结束：{reason}"));
                self.set_session_overlay(&format!("会话已结束：{reason}"), false);
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

    /// 画质工具条：按被控端 DisplayConfig 重建候选并回填选中态。
    ///
    /// 候选列表仅在被控端能力（预设/编解码/最高帧率）变化时重建；
    /// 选中态每次 DisplayConfig 到达都对齐（热切换后被控端回传权威值）。
    pub(crate) fn sync_quality_bar(&mut self) -> Result<()> {
        use windows::UI::Xaml::Controls::ComboBox;

        let sig = format!(
            "{:x}|{:?}|{}|{:?}",
            {
                // 稳定签名：预设列表 + 编解码支持 + 最高帧率
                let mut acc: u64 = 14695981039346656037;
                for (w, h, l) in &self.quality.presets {
                    for byte in format!("{w}x{h}{l}").bytes() {
                        acc ^= byte as u64;
                        acc = acc.wrapping_mul(1099511628211);
                    }
                }
                acc
            },
            self.quality.supported_codecs,
            self.quality.max_fps,
            self.quality.client_supports_hevc
        );
        if sig != self.quality_sig {
            self.quality_sig = sig;
            // 分辨率：跟随当前 + 预设
            let mut res_labels = vec!["跟随当前".to_string()];
            res_labels.extend(self.quality.presets.iter().map(|(_, _, l)| l.clone()));
            self.reset_combo_items("StreamResolutionBox", &res_labels)?;
            // 帧率：跟随当前 + 候选
            let mut fps_labels = vec!["跟随当前".to_string()];
            fps_labels.extend(self.quality_fps_options.iter().map(|v| format!("{v} fps")));
            self.reset_combo_items("StreamFpsBox", &fps_labels)?;
            // 码率：与设置页同源
            self.reset_combo_items(
                "StreamBitrateBox",
                &[
                    "自适应".to_string(),
                    "8 Mbps".to_string(),
                    "14 Mbps".to_string(),
                    "30 Mbps".to_string(),
                ],
            )?;
            // 编解码：被控端支持列表；未上报 → 单项"自动"禁用
            let codec_labels: Vec<String> = if self.quality.supported_codecs.is_empty() {
                vec!["自动".to_string()]
            } else {
                let mut v = vec!["跟随当前".to_string()];
                v.extend(
                    self.quality
                        .supported_codecs
                        .iter()
                        .map(|c| match c.as_str() {
                            "hevc" => "HEVC".to_string(),
                            "h264" => "H.264".to_string(),
                            other => other.to_uppercase(),
                        }),
                );
                v
            };
            self.reset_combo_items("StreamCodecBox", &codec_labels)?;
            if let Some(box_) = self.find_opt::<ComboBox>("StreamCodecBox") {
                let _ = box_
                    .cast::<windows::UI::Xaml::Controls::Control>()?
                    .SetIsEnabled(!self.quality.supported_codecs.is_empty());
            }
        }

        // 回填选中态（对齐被控端权威值）
        let res_index = self
            .quality
            .presets
            .iter()
            .position(|(w, h, _)| *w == self.quality.width && *h == self.quality.height)
            .map(|i| i as i32 + 1)
            .unwrap_or(0);
        let fps_index = self
            .quality_fps_options
            .iter()
            .position(|v| *v == self.quality.fps)
            .map(|i| i as i32 + 1)
            .unwrap_or(0);
        let bitrate_preset = self.prefs.lock().unwrap().bitrate_preset.clone();
        let bitrate_index = match bitrate_preset.as_str() {
            "8m" => 1,
            "14m" => 2,
            "30m" => 3,
            _ => 0,
        };
        let codec_index = self
            .quality
            .codec
            .as_deref()
            .and_then(|c| self.quality.supported_codecs.iter().position(|s| s == c))
            .map(|i| i as i32 + 1)
            .unwrap_or(0);
        for (name, index) in [
            ("StreamResolutionBox", res_index),
            ("StreamFpsBox", fps_index),
            ("StreamBitrateBox", bitrate_index),
            ("StreamCodecBox", codec_index),
        ] {
            if let Some(selector) = self.find_opt::<ComboBox>(name).and_then(|b| {
                b.cast::<windows::UI::Xaml::Controls::Primitives::Selector>()
                    .ok()
            }) {
                let _ = selector.SetSelectedIndex(index);
            }
        }
        Ok(())
    }

    /// 重置下拉候选（清空后按标签重建）。
    fn reset_combo_items(&self, name: &str, labels: &[String]) -> Result<()> {
        use windows::core::HSTRING;
        use windows::UI::Xaml::Controls::{ComboBox, ComboBoxItem, ContentControl, ItemsControl};
        let Some(box_) = self.find_opt::<ComboBox>(name) else {
            return Ok(());
        };
        let items = box_.cast::<ItemsControl>()?.Items()?;
        while items.Size()? > 0 {
            items.RemoveAtEnd()?;
        }
        for label in labels {
            let item = ComboBoxItem::new()?;
            let content = windows::Foundation::PropertyValue::CreateString(HSTRING::from(label))?;
            item.cast::<ContentControl>()?.SetContent(content)?;
            items.Append(&item)?;
        }
        Ok(())
    }

    /// 画质下拉变更：读全部四个选择 → 预钳制 → 下发（选择与当前生效值一致时跳过，
    /// 避免程序化回填触发 SetStreamParams 空切换）。
    pub(crate) fn on_quality_changed(&mut self) -> Result<()> {
        use windows::UI::Xaml::Controls::{ComboBox, Primitives};
        if !matches!(self.session_view, SessionView::Negotiated { .. }) {
            return Ok(());
        }
        let Some(active) = self.session.as_ref() else {
            return Ok(());
        };
        let handle = &active.handle;

        let selected = |name: &str| -> i32 {
            self.find_opt::<ComboBox>(name)
                .and_then(|b| b.cast::<Primitives::Selector>().ok())
                .and_then(|s| s.SelectedIndex().ok())
                .unwrap_or(-1)
        };
        let res_idx = selected("StreamResolutionBox");
        let fps_idx = selected("StreamFpsBox");
        let bitrate_idx = selected("StreamBitrateBox");
        let codec_idx = selected("StreamCodecBox");

        // 分辨率 + 帧率：0/负 = 跟随当前
        let want = match res_idx {
            i if i > 0 => {
                let Some((w, h, _)) = self.quality.presets.get(i as usize - 1) else {
                    return Ok(());
                };
                (*w, *h)
            }
            _ => (0, 0),
        };
        let fps = match fps_idx {
            i if i > 0 => self
                .quality_fps_options
                .get(i as usize - 1)
                .copied()
                .unwrap_or(0),
            _ => 0,
        };
        // 选中态与当前生效值一致 → 程序化回填/用户无操作，跳过
        let unchanged = match want {
            (0, 0) => fps == 0,
            (w, h) => {
                w == self.quality.width
                    && h == self.quality.height
                    && (fps == 0 || fps == self.quality.fps)
            }
        };
        if unchanged {
            return Ok(());
        }
        // 预钳制（ios 踩坑：越界静默无流，绝不直接透传）
        let Some((w, h, fps)) = sess::clamp_stream_request(want.0, want.1, fps, &self.quality)
        else {
            return Ok(());
        };
        if w != 0 || h != 0 || fps != 0 {
            handle.send(
                rc_client_core::session_bridge::SessionCommand::SetStreamParams {
                    width: w,
                    height: h,
                    fps,
                },
            );
        }
        // 码率：自适应(0) 不下发
        let bps = [0u32, 8_000_000, 14_000_000, 30_000_000][bitrate_idx.clamp(0, 3) as usize];
        if bps != 0 {
            handle.send(rc_client_core::session_bridge::SessionCommand::SetBitrate { bps });
        }
        // 编解码：跟随当前(0) 不下发
        if codec_idx > 0 {
            if let Some(codec) = self.quality.supported_codecs.get(codec_idx as usize - 1) {
                let codec = match codec.as_str() {
                    "hevc" => rc_client_core::display_ctrl::StreamCodec::Hevc,
                    _ => rc_client_core::display_ctrl::StreamCodec::H264,
                };
                handle.send(rc_client_core::session_bridge::SessionCommand::SetCodec { codec });
            }
        }
        Ok(())
    }

    /// M3.8 显示器切换：与当前采集屏一致时跳过。
    pub(crate) fn on_display_changed(&mut self) -> Result<()> {
        use windows::UI::Xaml::Controls::{ComboBox, Primitives};
        if !matches!(self.session_view, SessionView::Negotiated { .. }) {
            return Ok(());
        }
        let index = self
            .find_opt::<ComboBox>("StreamDisplayBox")
            .and_then(|b| b.cast::<Primitives::Selector>().ok())
            .and_then(|s| s.SelectedIndex().ok())
            .unwrap_or(-1);
        if index < 0 {
            return Ok(());
        }
        if let Some((display_id, _)) = self.quality.displays.get(index as usize) {
            if *display_id != self.quality.active_display_id {
                if let Some(active) = self.session.as_ref() {
                    active.handle.send_display_ctrl(
                        rc_client_core::display_ctrl::DisplayControlMsg::SwitchDisplay {
                            display_id: *display_id,
                        },
                    );
                }
            }
        }
        Ok(())
    }

    /// 鼠标模式变更：与当前生效一致时跳过（防程序化回填空下发）。
    pub(crate) fn on_mouse_mode_changed(&mut self) -> Result<()> {
        use windows::UI::Xaml::Controls::{ComboBox, Primitives};
        if !matches!(self.session_view, SessionView::Negotiated { .. }) {
            return Ok(());
        }
        let index = self
            .find_opt::<ComboBox>("StreamMouseBox")
            .and_then(|b| b.cast::<Primitives::Selector>().ok())
            .and_then(|s| s.SelectedIndex().ok())
            .unwrap_or(0);
        let Some((mode, _)) = sess::MOUSE_MODES.get(index.max(0) as usize) else {
            return Ok(());
        };
        if self.quality.mouse_mode.as_deref() == Some(mode) {
            return Ok(());
        }
        if let Some(active) = self.session.as_ref() {
            active.handle.send_display_ctrl(
                rc_client_core::display_ctrl::DisplayControlMsg::SetMouseMode {
                    mode: (*mode).to_string(),
                },
            );
        }
        Ok(())
    }

    pub(crate) fn set_session_overlay(&self, text: &str, spinner: bool) {
        let _ = self.label("SessionOverlayText", text);
        if let Some(ring) =
            self.find_opt::<windows::UI::Xaml::Controls::ProgressRing>("SessionSpinner")
        {
            let _ = ring.SetIsActive(spinner);
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
