// SPDX-License-Identifier: GPL-3.0-or-later

//! Netplay owns every machine input; the surrounding window still presents it.

use super::*;

pub(super) type DiskPicker =
    std::pin::Pin<Box<dyn std::future::Future<Output = Option<Vec<rfd::FileHandle>>>>>;

impl App {
    pub fn attach_netplay(&mut self, session: crate::netplay::Session) {
        self.netplay_setup = Some(crate::video::launcher::NetplaySetup::from(
            &session.options(),
        ));
        self.netplay = Some(session);
        self.netplay_input = Default::default();
        self.netplay_keyboard_controller = self.mouse_port().is_none();
        self.mouse_delta_remainder = (0.0, 0.0);
        self.last_display_cursor_pos = None;
        self.show_osd("Netplay: waiting for peer (F11 to cancel)".to_string());
    }

    pub(super) fn remember_netplay_setup(&mut self) {
        if let Some(state) = self.launcher_state_mut() {
            state.edit_commit();
            self.netplay_setup = Some(state.netplay.clone());
        }
    }

    pub(super) fn launcher_netplay_action(&mut self, field: LauncherField) {
        let Some(state) = self.launcher_state_mut() else {
            return;
        };
        state.edit_commit();
        if !state.netplay.field_enabled(field) || state.editing().is_some() {
            return;
        }
        if field == LauncherField::NetplayNewCode {
            state.status = Some(match state.netplay.generate_code() {
                Ok(()) => StatusMessage::ok("Copy this code to the other player, then press Run"),
                Err(error) => StatusMessage::err(error.to_string()),
            });
        } else if field == LauncherField::NetplayCopyCode {
            let valid = if state.netplay.internet {
                state.netplay.connection_options().map(|_| ())
            } else {
                crate::netplay::parse_session_id(&state.netplay.code).map(|_| ())
            };
            if let Err(error) = valid {
                state.status = Some(StatusMessage::err(error.to_string()));
                return;
            }
            let code = state.netplay.code.clone();
            match self.copy_netplay_code(code) {
                Ok(()) => self.set_launcher_status(StatusMessage::ok("Session code copied")),
                Err(error) => {
                    self.set_launcher_status(StatusMessage::err(format!("Clipboard: {error}")))
                }
            }
        }
    }

    fn copy_netplay_code(&mut self, code: String) -> std::result::Result<(), arboard::Error> {
        // Keep the selection owner alive after this click on X11/Wayland.
        if self.host_clipboard.is_none() {
            self.host_clipboard = Some(arboard::Clipboard::new()?);
        }
        self.host_clipboard.as_mut().unwrap().set_text(code)
    }

    pub(super) fn leave_netplay(&mut self, error: Option<String>) {
        self.netplay_disk_picker = None;
        self.set_mouse_captured(false);
        self.netplay = None;
        self.netplay_input = Default::default();
        self.keyboard_joy_held = Default::default();
        self.paused = true;
        self.sync_live_audio_suspension();
        self.open_launcher();
        if let Some(state) = self.launcher_state_mut() {
            state.tab = crate::video::launcher::LauncherTab::Netplay;
            state.status = Some(error.map_or_else(
                || StatusMessage::ok("Disconnected. Run starts a new session"),
                |error| StatusMessage::err(format!("Netplay stopped: {error}")),
            ));
        }
        self.nav.park(self.nav_home());
        self.request_redraw();
    }

    pub(super) fn pump_netplay_input(&mut self) {
        if self.mouse_port().is_some() {
            return;
        }
        let pad = self.gamepad.poll();
        let port = self.netplay.as_ref().unwrap().player();
        if pad.is_none() && self.auto_joy_engaged[port] {
            self.apply_auto_joy_state(port);
            return;
        }
        let mut state = pad.map_or_else(|| self.keyboard_joystick_state(0), |p| p.joystick);
        if (!self.netplay_keyboard_controller && pad.is_none()) || !self.main_window_focused {
            state = Default::default();
        }
        state.fire &=
            crate::config::autofire_asserted(self.autofire_hz, self.emu.bus().emulated_seconds());
        self.netplay_input.held.buttons = [
            state.up,
            state.down,
            state.left,
            state.right,
            state.fire,
            state.button2,
            state.play,
            state.rwd,
            state.ffw,
            state.green,
            state.yellow,
        ]
        .into_iter()
        .enumerate()
        .fold(0, |bits, (bit, on)| bits | (u16::from(on) << bit));
    }

    pub(super) fn step_netplay(&mut self) -> Result<bool> {
        if let Some((drive, picker)) = &mut self.netplay_disk_picker {
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            if let std::task::Poll::Ready(files) = picker.as_mut().poll(&mut context) {
                let drive = *drive;
                self.netplay_disk_picker = None;
                if let Some(files) = files {
                    self.insert_disk_playlist(
                        drive,
                        files.into_iter().map(|f| f.path().to_path_buf()).collect(),
                    );
                }
            }
        }
        let session = self.netplay.as_mut().unwrap();
        let before = session.status();
        let connected = before.connected;
        let stepped = session.step_local(&mut self.emu, &mut self.netplay_input, true)?;
        let after = session.status();
        let route = session.route();
        let config = session.take_config();
        let progress = session.take_progress();
        let guest = session.player() == 1;
        if let Some(cfg) = config {
            self.netplay_input = Default::default();
            self.netplay_keyboard_controller = self.mouse_port().is_none();
            self.keyboard_joy_held = Default::default();
            self.about_machine_lines = crate::config::about_machine_lines(&cfg);
            self.disk_write_protected = std::array::from_fn(|drive| {
                self.emu
                    .bus()
                    .floppy
                    .disk_image_write_protected(drive)
                    .unwrap_or(true)
            });
            if guest {
                self.disk_playlists = Default::default();
                self.disk_playlist_index = [0; 4];
            }
            self.reset_render_pipeline();
        }
        if let Some(progress) = progress {
            log::debug!(target: "copperline::netplay", "netplay: {progress}");
            self.show_osd(progress);
        }
        if after.rollbacks != before.rollbacks {
            self.reset_render_pipeline();
            // Continuous remote motion can correct every frame. Finish this
            // image before the next correction invalidates the worker result,
            // or the desktop can keep displaying the same old framebuffer.
            if !self.headless_capture_active() {
                self.finish_render_for_current_frame();
            }
        }
        if !connected && after.connected {
            let controls = if self.mouse_port().is_some() {
                "Netplay connected: mouse controls your port"
            } else {
                "Netplay connected: arrows + right Ctrl, or gamepad"
            };
            self.show_osd(format!("{controls} ({route})"));
        }
        if !stepped {
            self.emu.reanchor_realtime_clock();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        Ok(stepped)
    }

    /// Scheduled captures wait for actual input so PNGs never preserve a
    /// prediction that the following network packet would correct.
    pub(super) fn confirm_netplay_capture(&mut self) -> Result<()> {
        let now = self.emu.bus().emulated_seconds();
        let due = self
            .auto_shot
            .first()
            .is_some_and(|(at, _)| now >= f64::from(*at))
            || self
                .frame_dump
                .as_ref()
                .is_some_and(|dump| now >= f64::from(dump.start_secs));
        if !due || self.netplay.is_none() {
            return Ok(());
        }
        loop {
            let session = self.netplay.as_mut().unwrap();
            if session.ready_to_capture() {
                return Ok(());
            }
            let before = session.status().rollbacks;
            session.step_local(&mut self.emu, &mut self.netplay_input, false)?;
            let status = session.status();
            let ready = session.ready_to_capture();
            if status.rollbacks != before {
                self.reset_render_pipeline();
            }
            if ready {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    /// Consume machine-changing window events before normal shortcuts, menus,
    /// drag-and-drop, mouse input, or focus-loss key release can reach the Bus.
    pub(super) fn netplay_window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        event: &WindowEvent,
    ) -> bool {
        if self.netplay.is_none() {
            return false;
        }
        match event {
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        physical_key: PhysicalKey::Code(code),
                        state,
                        repeat,
                        ..
                    },
                ..
            } => {
                if *repeat {
                    return true;
                }
                let pressed = *state == ElementState::Pressed;
                if host_shortcut_modifier_pressed(self.modifiers) && pressed {
                    match code {
                        KeyCode::KeyQ => event_loop.exit(),
                        KeyCode::KeyF => self.toggle_fullscreen(),
                        KeyCode::KeyD if self.netplay.as_ref().is_some_and(|s| s.player() == 0) => {
                            self.cycle_disk()
                        }
                        KeyCode::KeyG if self.mouse_port().is_some() => {
                            self.set_mouse_captured(!self.mouse_captured)
                        }
                        _ => {}
                    }
                    return true;
                }
                if *code == KeyCode::F11 {
                    if pressed {
                        self.leave_netplay(None);
                    }
                    return true;
                }
                if *code == KeyCode::F12 {
                    if pressed && self.mouse_port().is_none() {
                        self.netplay_keyboard_controller = !self.netplay_keyboard_controller;
                        self.keyboard_joy_held = Default::default();
                        self.netplay_input = Default::default();
                        self.show_osd(
                            if self.netplay_keyboard_controller {
                                "Netplay: keyboard controls the joystick (F12 to type)"
                            } else {
                                "Netplay: Amiga keyboard typing (F12 for joystick)"
                            }
                            .to_string(),
                        );
                    }
                    return true;
                }
                if self.netplay_keyboard_controller
                    && matches!(self.keymap.lookup(*code), Some((0, _)))
                {
                    self.keyboard_joy_held[0].set(*code, pressed);
                } else if let Some(rawkey) = host_to_amiga_rawkey(*code) {
                    self.netplay_input.held.set_key(rawkey, pressed);
                }
                true
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
                true
            }
            WindowEvent::Focused(focused) => {
                self.main_window_focused = *focused;
                if !focused {
                    self.set_mouse_captured(false);
                    self.mouse_delta_remainder = (0.0, 0.0);
                    self.last_display_cursor_pos = None;
                    self.netplay_input = Default::default();
                    self.keyboard_joy_held = Default::default();
                }
                true
            }
            WindowEvent::CursorMoved { position, .. } => {
                let previous_cursor_pos = self.cursor_pos;
                self.last_cursor_phys = Some(*position);
                let display_src = self.display_canvas_src();
                let pos = self
                    .render
                    .as_ref()
                    .and_then(|r| main_cursor_position(r, display_src, *position));
                self.cursor_pos = pos;
                if bar_hover_changed(&bar_layout(&self.media_bar()), previous_cursor_pos, pos) {
                    self.request_redraw();
                }
                if !self.mouse_captured && self.main_window_focused {
                    self.track_uncaptured_cursor_motion(pos);
                }
                true
            }
            WindowEvent::CursorLeft { .. } => {
                let previous_cursor_pos = self.cursor_pos;
                self.cursor_pos = None;
                if bar_hover_changed(&bar_layout(&self.media_bar()), previous_cursor_pos, None) {
                    self.request_redraw();
                }
                self.last_display_cursor_pos = None;
                if !self.mouse_captured {
                    self.release_mouse_buttons();
                }
                true
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if *state == ElementState::Pressed
                    && *button == MouseButton::Left
                    && !self.mouse_captured
                    && self.cursor_pos.is_some_and(cursor_in_status_bar)
                {
                    if let Some(pos) = self.cursor_pos {
                        let layout = bar_layout(&self.media_bar());
                        if let Some(
                            control @ (BarControl::DriveLoad(_)
                            | BarControl::DriveSwap(_)
                            | BarControl::DriveEject(_)),
                        ) = control_at(pos, &layout)
                        {
                            self.activate_bar_control(control);
                        }
                    }
                    return true;
                }
                if self.mouse_port().is_some() && self.main_window_focused {
                    let pressed = *state == ElementState::Pressed;
                    let over_display = self.cursor_pos.is_some_and(cursor_in_display);
                    if pressed
                        && !self.mouse_captured
                        && over_display
                        && self.mouse_capture != crate::config::MouseCapture::Manual
                    {
                        self.set_mouse_captured(true);
                        if self.mouse_captured {
                            return true;
                        }
                    }
                    if !pressed || self.mouse_captured || over_display {
                        let index = match button {
                            MouseButton::Left => Some(0),
                            MouseButton::Right => Some(1),
                            MouseButton::Middle => Some(2),
                            _ => None,
                        };
                        if let Some(index) = index {
                            self.netplay_input.held.set_mouse_button(index, pressed);
                        }
                    }
                }
                true
            }
            WindowEvent::CloseRequested
            | WindowEvent::Resized(_)
            | WindowEvent::ScaleFactorChanged { .. }
            | WindowEvent::RedrawRequested
            | WindowEvent::Occluded(_) => false,
            WindowEvent::HoveredFile(_)
            | WindowEvent::HoveredFileCancelled
            | WindowEvent::DroppedFile(_) => false,
            _ => true,
        }
    }
}
