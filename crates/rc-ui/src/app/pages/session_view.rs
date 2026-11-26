//! 会话页：连接流、视图渲染与输入注入。

use super::{super::actions::*, super::App};
use windows::core::Result;

use std::{sync::Arc, thread};

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
        // 0.37 投影 CheckBox.IsChecked 需 UIA；M2 以勾选框视觉状态不可读，
        // 仅观看入口保留在 UI，后续经 GetTogglePattern 化（先读 IReference）。
        // M2：仅观看勾选框的视觉态读取在 M3 完善（0.37 无直接投影）。
        let watch_only = false;
        self.peer_watch_only = watch_only;
        let preset = self.prefs.lock().unwrap().bitrate_preset.clone();
        let params = sess::ConnectParams {
            password,
            watch_only,
            bitrate_bps: sess::bitrate_preset_to_bps(&preset),
            fps: 60,
            lan_direct: self
                .runtime
                .lan_peers()
                .iter()
                .find(|p| p.device_id.as_deref() == Some(code))
                .map(|p| p.addr.clone()),
        };
        let name = self
            .devices
            .iter()
            .find(|d| d.device_id == code)
            .map(|d| self.device_name_of(d))
            .unwrap_or_else(|| code.to_string());
        self.set_session_overlay("正在连接…", true);
        let _ = self.set_visible("SessionReconnect", false);
        self.page = Page::Session;
        self.show_page(Page::Session)?;
        let manager = self.auth.manager();
        let runtime = Arc::clone(&self.runtime);
        let events = self.sender();
        let code = code.to_string();
        thread::spawn(move || {
            let result = runtime
                .request_connect(&code, sess::CONNECT_TIMEOUT)
                .and_then(|grant| sess::build_session_config(&manager, grant, &params));
            let outcome = match result {
                Ok(cfg) => Ok(SessionHandle::spawn(cfg)),
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
                self.set_session_overlay("", false);
            }
            SessionView::Ended(reason) => {
                let _ = self.label("SessionStatus", &format!("已结束：{reason}"));
                self.set_session_overlay(&format!("会话已结束：{reason}"), false);
                let _ = self.set_visible("SessionReconnect", true);
            }
        }
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
