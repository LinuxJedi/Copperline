// SPDX-License-Identifier: GPL-3.0-or-later

//! Host input dispatch: mouse capture and motion, sensitivity, scripted pointer targets, raw-device and Amiga key transitions, host modifiers, input recording.

use super::*;

impl App {
    pub(super) fn toggle_mouse_capture(&mut self) {
        if !self.mouse_captured && self.mouse_port().is_none() {
            self.show_osd("No mouse on either port".to_string());
            return;
        }
        // An explicit toggle settles the question either way: whatever the
        // UI borrowed earlier is no longer owed back.
        self.capture_suspended_by_ui = false;
        self.set_mouse_captured(!self.mouse_captured);
    }

    /// Release the mouse on behalf of a panel or tool window that needs the
    /// host cursor, remembering a live capture so
    /// `restore_mouse_capture_after_ui` can hand it back when the last of
    /// them closes. Without that, opening the debugger over a captured
    /// session left the machine uncaptured for good -- most visible in
    /// fullscreen, where there is no desktop to reach for anyway.
    ///
    /// This covers the routes that can be taken *while* captured, which is
    /// the keyboard shortcuts. The menu and status bar are not among them:
    /// their click targets are refused while the mouse is captured, so
    /// reaching them means releasing it by hand first, and that explicit
    /// release is not something to undo afterwards.
    pub(super) fn suspend_mouse_capture_for_ui(&mut self) {
        if self.mouse_captured {
            self.capture_suspended_by_ui = true;
            self.set_mouse_captured(false);
        }
    }

    /// Take the grab if `[input] mouse_capture = "auto"` and the moment is
    /// right for it: the window holds the focus, nothing else wants the
    /// cursor, and there is a mouse on a port to drive.
    ///
    /// Deliberately driven by discrete events (focus gain, entering
    /// fullscreen, the last panel closing) rather than polled per frame --
    /// a poll would re-take the grab the instant the operator released it
    /// with the shortcut, leaving no way to get the cursor back at all.
    pub(super) fn apply_auto_mouse_capture(&mut self) {
        if self.mouse_capture != crate::config::MouseCapture::Auto
            || self.mouse_captured
            || !self.main_window_focused
            || self.ui_wants_cursor()
            || self.mouse_port().is_none()
        {
            return;
        }
        self.set_mouse_captured(true);
        // Auto mode hides the host cursor without the operator having done
        // anything to ask for it, so say once how to get it back. Every
        // later focus gain re-grabs silently.
        if self.mouse_captured && !self.auto_capture_hint_shown {
            self.auto_capture_hint_shown = true;
            self.show_osd(format!(
                "Mouse captured ({HOST_SHORTCUT_MODIFIER_LABEL}+G releases)"
            ));
        }
    }

    /// Re-take a capture the UI borrowed, once nothing still wants the
    /// cursor. A no-op unless `suspend_mouse_capture_for_ui` recorded one,
    /// so a session that was never captured is never surprised by a grab.
    pub(super) fn restore_mouse_capture_after_ui(&mut self) {
        if !self.capture_suspended_by_ui || self.ui_wants_cursor() {
            return;
        }
        // Same guard the click-to-capture path applies: with no mouse left
        // on either port there is nothing to drive, and grabbing would only
        // trap a hidden cursor. Cheap insurance against a port device that
        // changed while the panel was open -- and the loan is void, not
        // outstanding, because no later event can repay it.
        if self.mouse_port().is_none() {
            self.capture_suspended_by_ui = false;
            return;
        }
        // A grab wants the focus. Closing a tool window hands the focus back
        // to the main window, but the order of that against this call is the
        // window manager's business: attempted too early the grab fails, and
        // clearing the loan on a failed grab would lose the capture for good
        // -- the very thing this mechanism exists to prevent. Leave it
        // outstanding and let the Focused(true) that follows retry.
        if !self.main_window_focused {
            return;
        }
        self.set_mouse_captured(true);
        // Only a grab that actually took discharges the loan.
        if self.mouse_captured {
            self.capture_suspended_by_ui = false;
        }
    }

    /// COPPERLINE_DIAG_CURSOR: trace how the most recent click maps from host
    /// physical coordinates through the scaler pass's clip rect into a
    /// canvas/region hit. The tool for diagnosing mouse capture on DPI scale
    /// changes and mixed-scale monitors (see the ScaleFactorChanged handler):
    /// if a status-bar click logs region=display(->capture), the surface and
    /// clip rect have drifted out of agreement.
    pub(super) fn log_cursor_diag(&self, button: MouseButton) {
        if !crate::envcfg::flag("COPPERLINE_DIAG_CURSOR") {
            return;
        }
        let display_src = self.display_canvas_src();
        let Some(r) = self.render.as_ref() else {
            return;
        };
        let scale_factor = r.window.scale_factor();
        let inner = r.window.inner_size();
        let phys = self.last_cursor_phys;
        let context = r.pixels.context();
        // The rect the display quad is actually drawn into -- the
        // sub-rect's under autocrop or per-axis scaling, the classic
        // letterbox otherwise -- so the trace shows the same mapping the
        // position below went through.
        let layout = main_present_layout(r, display_src);
        let clip = layout.display_dst;
        let texture = (context.texture_extent.width, context.texture_extent.height);
        let pos = phys.and_then(|p| layout.cursor_position(p));
        let region = match pos {
            Some(p) if cursor_in_status_bar(p) => "status_bar",
            Some(p) if cursor_in_display(p) => "display(->capture)",
            Some(_) => "other",
            None => "none",
        };
        info!(
            "[DIAG_CURSOR] button={button:?} phys={phys:?} scale_factor={scale_factor:.4} \
             inner={}x{} texture_scale={} clip_rect={clip:?} texture={}x{} mapped_pos={pos:?} \
             region={region} (present_h={} window_present_h={} fb_w={FB_WIDTH})",
            inner.width,
            inner.height,
            r.texture_scale,
            texture.0,
            texture.1,
            present_height(),
            window_present_height(),
        );
    }

    pub(super) fn set_mouse_captured(&mut self, captured: bool) {
        if self.mouse_captured == captured {
            return;
        }
        let Some(window) = self.render.as_ref().map(|r| r.window.clone()) else {
            return;
        };
        self.volume_dragging = false;
        self.analyzer_dragging = false;

        if captured {
            match window
                .set_cursor_grab(CursorGrabMode::Locked)
                .or_else(|locked_err| {
                    window
                        .set_cursor_grab(CursorGrabMode::Confined)
                        .map_err(|confined_err| (locked_err, confined_err))
                }) {
                Ok(()) => {
                    self.mouse_captured = true;
                    // The machine has the mouse, and with it the
                    // keyboard: a marker left on the bar would keep
                    // taking the guest's arrow keys and lighting
                    // buttons behind its back.
                    self.nav.clear();
                    self.cursor_pos = None;
                    self.last_display_cursor_pos = None;
                    self.mouse_delta_remainder = (0.0, 0.0);
                    window.set_cursor_visible(false);
                    window.set_title(&window_title_mouse_captured());
                    info!("mouse captured; press {HOST_SHORTCUT_MODIFIER_LABEL}+G to release");
                }
                Err((locked_err, confined_err)) => {
                    warn!("mouse capture failed (locked: {locked_err}; confined: {confined_err})")
                }
            }
        } else {
            if let Err(e) = window.set_cursor_grab(CursorGrabMode::None) {
                warn!("mouse release failed: {e}");
            }
            self.mouse_captured = false;
            self.cursor_pos = None;
            self.last_display_cursor_pos = None;
            self.mouse_delta_remainder = (0.0, 0.0);
            self.release_mouse_buttons();
            window.set_cursor_visible(true);
            window.set_title(window_title());
            info!("mouse released");
        }
    }

    pub(super) fn release_mouse_buttons(&mut self) {
        if self.netplay.is_some() {
            self.netplay_input.held.mouse_buttons = 0;
            self.netplay_input.mouse_pending = (0, 0);
            return;
        }
        if let Some(port) = self.mouse_port() {
            let input = &mut self.emu.bus_mut().input;
            for index in 0..3 {
                input.set_mouse_button(port, index, false);
            }
        }
    }

    pub(super) fn track_uncaptured_cursor_motion(&mut self, pos: Option<(i32, i32)>) {
        let Some(pos) = pos.filter(|p| cursor_in_display(*p)) else {
            self.last_display_cursor_pos = None;
            return;
        };
        if let Some(prev) = self.last_display_cursor_pos {
            let dx = pos.0 - prev.0;
            let dy = pos.1 - prev.1;
            if dx != 0 || dy != 0 {
                // Through the same scale the captured path uses, so the
                // sensitivity setting means something on both sides of a
                // grab instead of silently doing nothing until the mouse is
                // captured. The units still differ -- these are texture
                // pixels where a captured delta is a raw device count -- so
                // this equalises the operator's knob, not the underlying
                // ratio; at the default sensitivity the factor is 1.0 and
                // the long-standing 1:1 tracking is unchanged.
                self.add_host_mouse_delta(f64::from(dx), f64::from(dy));
            }
        }
        self.last_display_cursor_pos = Some(pos);
    }

    /// Move the mouse with the pad, in Gamepad Mouse mode.
    ///
    /// A stick is proportional where the pad has one: a slight
    /// deflection creeps and a full one crosses the screen, squared so
    /// the slow end has room to be slow in. A d-pad has only on and off,
    /// so it stands in with a hold that gathers speed -- a tap nudges,
    /// and holding a direction ramps up to the same top speed the stick
    /// reaches.
    ///
    /// Both go through the host mouse's own accumulator, so the machine
    /// is given one mouse with two hands on it rather than two mice, and
    /// the mouse-sensitivity setting means the same thing for both.
    pub(super) fn apply_pad_mouse_state(&mut self, port: usize, pad: crate::gamepad::PadState) {
        let now = Instant::now();
        // Against the clock rather than the loop: the pointer must not
        // move faster on a machine that polls more often. A long gap --
        // the loop was asleep, or the machine was paused -- is clamped
        // rather than spent all at once.
        let dt = self
            .pad_mouse_at
            .replace(now)
            .map(|then| now.saturating_duration_since(then))
            .unwrap_or_default()
            .min(PAD_MOUSE_MAX_STEP)
            .as_secs_f64();
        let js = pad.joystick;
        let (mut dx, mut dy) = (0.0, 0.0);
        let (sx, sy) = pad.stick;
        let deflection = (f64::from(sx).powi(2) + f64::from(sy).powi(2)).sqrt();
        if deflection > PAD_MOUSE_DEADZONE {
            // Past the dead zone, and squared: what is left of the throw
            // is spread over the whole speed range, so the first part of
            // it is genuinely slow.
            let travel = ((deflection - PAD_MOUSE_DEADZONE) / (1.0 - PAD_MOUSE_DEADZONE)).min(1.0);
            let speed = PAD_MOUSE_FAST * travel * travel;
            dx = f64::from(sx) / deflection * speed * dt;
            // A stick reads up as positive; the screen reads down as
            // positive.
            dy = -f64::from(sy) / deflection * speed * dt;
            self.pad_mouse_held = None;
        } else {
            let x = f64::from(i8::from(js.right) - i8::from(js.left));
            let y = f64::from(i8::from(js.down) - i8::from(js.up));
            if x != 0.0 || y != 0.0 {
                let held = self
                    .pad_mouse_held
                    .get_or_insert(now)
                    .elapsed()
                    .as_secs_f64();
                let ramp = (held / PAD_MOUSE_RAMP.as_secs_f64()).min(1.0);
                let speed = PAD_MOUSE_SLOW + (PAD_MOUSE_FAST - PAD_MOUSE_SLOW) * ramp;
                // Diagonals travel the same distance as the straights
                // rather than the square's diagonal.
                let length = (x * x + y * y).sqrt();
                dx = x / length * speed * dt;
                dy = y / length * speed * dt;
            } else {
                self.pad_mouse_held = None;
            }
        }
        if dx != 0.0 || dy != 0.0 {
            self.add_host_mouse_delta(dx, dy);
        }
        let input = &mut self.emu.bus_mut().input;
        input.set_mouse_button(port, 0, js.fire);
        input.set_mouse_button(port, 1, js.button2);
    }

    /// Let go of everything the pad was holding on the mouse, and forget
    /// how long it had been held: the UI has taken the pad, or it has
    /// been unplugged, and neither is a reason for a button to stick.
    pub(super) fn release_pad_mouse(&mut self, port: usize) {
        self.pad_mouse_held = None;
        self.pad_mouse_at = None;
        let input = &mut self.emu.bus_mut().input;
        input.set_mouse_button(port, 0, false);
        input.set_mouse_button(port, 1, false);
    }

    pub(super) fn add_host_mouse_delta(&mut self, dx: f64, dy: f64) {
        if !dx.is_finite() || !dy.is_finite() {
            return;
        }
        // The sensitivity scale is applied to the live host mouse only, here --
        // scripted --mouse-after deltas go through apply_scripted_mouse_delta
        // and stay exact, so the core is deterministic regardless of it.
        let scale = MOUSE_MOTION_SCALE * self.mouse_sensitivity_factor;
        self.mouse_delta_remainder.0 += dx * scale;
        self.mouse_delta_remainder.1 += dy * scale;
        let ix = take_integral_mouse_delta(&mut self.mouse_delta_remainder.0);
        let iy = take_integral_mouse_delta(&mut self.mouse_delta_remainder.1);
        if ix != 0 || iy != 0 {
            self.add_mouse_delta_i32(ix, iy);
        }
    }

    /// Set the host mouse sensitivity (0-100), recomputing the speed factor.
    pub(super) fn set_mouse_sensitivity(&mut self, sensitivity: u8) {
        self.mouse_sensitivity = sensitivity.min(100);
        self.mouse_sensitivity_factor = mouse_sensitivity_factor(self.mouse_sensitivity);
    }

    /// Nudge the mouse sensitivity by one, clamped to 0-100, with an on-screen
    /// readout. Bound to the Cmd/Alt+Shift+> / < shortcuts, which ramp while
    /// held via key repeat. A no-op when no port holds a mouse, since the scale
    /// would have nothing to act on.
    pub(super) fn step_mouse_sensitivity(&mut self, up: bool) {
        if self.mouse_port().is_none() {
            return;
        }
        let next = if up {
            self.mouse_sensitivity.saturating_add(1)
        } else {
            self.mouse_sensitivity.saturating_sub(1)
        };
        self.set_mouse_sensitivity(next);
        self.show_osd(format!(
            "Mouse sensitivity: {}",
            crate::config::mouse_sensitivity_label(self.mouse_sensitivity)
        ));
    }

    pub(super) fn add_mouse_delta_i32(&mut self, dx: i32, dy: i32) {
        let Some(port) = self.mouse_port() else {
            return;
        };
        if self.netplay.is_some() {
            self.netplay_input.add_mouse_delta(dx, dy);
            return;
        }
        self.apply_scripted_mouse_delta(port as u8, dx, dy);
    }

    /// Apply quadrature motion to an explicit port: scripted/CCP events
    /// drive the named port's counters whatever device is configured
    /// there, while live host-mouse motion goes through `mouse_port`.
    pub(super) fn apply_scripted_mouse_delta(&mut self, port: u8, dx: i32, dy: i32) {
        self.emu
            .bus_mut()
            .input
            .add_mouse_delta(port as usize, dx, dy);
        // Reverse-debug: note the motion so replay can reproduce it.
        self.emu
            .tt_note_input(crate::inputsched::ReplayAction::MouseMove { port, dx, dy });
    }

    /// Arm one scripted pointer target directly, for tests that do not go
    /// through the CLI.
    #[cfg(test)]
    pub(super) fn arm_scripted_pointer_target(&mut self, secs: f64, x: i32, y: i32, port: u8) {
        self.auto_mouse_to.push((secs, x, y, port));
    }

    /// Whether a scripted pointer servo is currently steering.
    #[cfg(test)]
    pub(super) fn scripted_pointer_target_active(&self) -> bool {
        self.active_mouse_to.is_some()
    }

    /// Advance the scripted `--mouse-to-after` pointer targets: start the
    /// next one that is due when nothing is steering, then give the
    /// running servo this frame's correction.
    ///
    /// One correction per frame is the servo's whole contract -- it has
    /// to see what the previous delta did before choosing the next -- and
    /// this runs once per emulated frame, in the same pass the other
    /// scheduled input fires from.
    pub(super) fn advance_scripted_pointer_targets(&mut self, emu_secs: f64) {
        if self.active_mouse_to.is_none() {
            if let Some(pos) = self
                .auto_mouse_to
                .iter()
                .position(|&(at, ..)| emu_secs >= at)
            {
                let (_, x, y, port) = self.auto_mouse_to.remove(pos);
                info!(
                    "auto-mouse-to: steering the pointer to ({x}, {y}) on port {}",
                    port + 1
                );
                self.active_mouse_to = Some(crate::pointer::PointerServo::new(
                    port,
                    (x, y),
                    crate::pointer::DEFAULT_TOLERANCE,
                    crate::pointer::DEFAULT_MAX_FRAMES,
                ));
            }
        }
        let Some(servo) = self.active_mouse_to.as_mut() else {
            return;
        };
        match servo.poll(self.emu.bus()) {
            crate::pointer::ServoStep::Move { port, dx, dy } => {
                self.apply_scripted_mouse_delta(port, dx, dy);
            }
            crate::pointer::ServoStep::Arrived { x, y, frames } => {
                info!("auto-mouse-to: pointer at ({x}, {y}) after {frames} frame(s)");
                self.active_mouse_to = None;
            }
            crate::pointer::ServoStep::Failed(why) => {
                warn!("auto-mouse-to: {why}");
                self.active_mouse_to = None;
            }
        }
    }

    pub(super) fn handle_raw_device_key_event(&mut self, event: RawKeyEvent) {
        let PhysicalKey::Code(code) = event.physical_key else {
            return;
        };
        let Some(rawkey) = raw_device_qualifier_rawkey(code) else {
            return;
        };

        let pressed = event.state == ElementState::Pressed;
        self.raw_device_held_rawkeys[rawkey_index(rawkey)] = pressed;
        if pressed && (!self.main_window_focused || self.modal_ui_active()) {
            return;
        }
        if self.handle_keyboard_joystick_key(code, pressed) {
            return;
        }
        self.handle_amiga_key_event(rawkey, pressed);
    }

    pub(super) fn activate_analyzer_pick_at(
        &mut self,
        kind: ToolPanelKind,
        pos: (i32, i32),
    ) -> bool {
        if kind != ToolPanelKind::FrameAnalyzer {
            return false;
        }
        let control = self.tool_panel_control_at(kind, pos);
        let Some(UiControl::AnalyzerPick { x, y, scanline }) = control else {
            return false;
        };
        self.frame_analyzer_select(x, y, scanline);
        self.request_redraw();
        true
    }

    pub(super) fn update_host_modifiers(&mut self, modifiers: ModifiersState) {
        self.modifiers = modifiers;
        if !modifiers.shift_key()
            && !raw_device_qualifier_family_held(
                &self.raw_device_held_rawkeys,
                AMIGA_RAWKEY_LEFT_SHIFT,
                AMIGA_RAWKEY_RIGHT_SHIFT,
            )
        {
            self.release_amiga_rawkey_if_held(AMIGA_RAWKEY_LEFT_SHIFT);
            self.release_amiga_rawkey_if_held(AMIGA_RAWKEY_RIGHT_SHIFT);
        }
        if !modifiers.alt_key()
            && !raw_device_qualifier_family_held(
                &self.raw_device_held_rawkeys,
                AMIGA_RAWKEY_LEFT_ALT,
                AMIGA_RAWKEY_RIGHT_ALT,
            )
        {
            self.release_amiga_rawkey_if_held(AMIGA_RAWKEY_LEFT_ALT);
            self.release_amiga_rawkey_if_held(AMIGA_RAWKEY_RIGHT_ALT);
        }
    }

    pub(super) fn release_amiga_rawkey_if_held(&mut self, rawkey: u8) {
        if rawkey_is_held(&self.held_rawkeys, rawkey) {
            self.handle_amiga_key_event(rawkey, false);
        }
    }

    /// Whether the machine is being told `rawkey` is down -- by the host
    /// keyboard, by the on-screen one, or by both.
    pub(super) fn amiga_rawkey_held(&self, rawkey: u8) -> bool {
        rawkey_is_held(&self.held_rawkeys, rawkey)
            || rawkey_is_held(&self.panel_held_rawkeys, rawkey)
    }

    /// A transition from the host keyboard.
    pub(super) fn handle_amiga_key_event(&mut self, rawkey: u8, pressed: bool) {
        if self.netplay.is_some() {
            self.netplay_input.held.set_key(rawkey, pressed);
            return;
        }
        self.handle_amiga_key_event_from(KeySource::Host, rawkey, pressed);
    }

    /// A transition from `source`, reaching the machine only when it moves
    /// the aggregate held state (see [`KeySource`]).
    pub(super) fn handle_amiga_key_event_from(
        &mut self,
        source: KeySource,
        rawkey: u8,
        pressed: bool,
    ) {
        let idx = rawkey_index(rawkey);
        let held = match source {
            KeySource::Host => &self.held_rawkeys,
            KeySource::Panel => &self.panel_held_rawkeys,
        };
        // Per-source: a winit auto-repeat re-presses a key this source
        // already has down, and a source can be told to let go of
        // something it never took.
        if rawkey_transition_is_duplicate(held, rawkey, pressed) {
            return;
        }
        let was_held = self.amiga_rawkey_held(rawkey);
        match source {
            KeySource::Host => self.held_rawkeys[idx] = pressed,
            KeySource::Panel => self.panel_held_rawkeys[idx] = pressed,
        }
        // The other source is holding the same key, so the aggregate did
        // not move: the machine already believes what this transition
        // would tell it, and a recorded or replayed copy of it would
        // reproduce the second holder rather than the keystroke.
        if was_held == self.amiga_rawkey_held(rawkey) {
            return;
        }

        // Ctrl+Amiga+Amiga is no longer consumed host-side: the chord
        // travels to the keyboard MCU like every other transition, and
        // the MCU runs the authentic $78 reset-warning / 500 ms KCLK
        // reset protocol.
        if let Some(rec) = self.input_recorder.as_mut() {
            rec.record_key(rawkey, pressed, self.emu.bus().emulated_seconds());
        }

        if pressed {
            self.emu.bus_mut().enqueue_key(rawkey);
        } else {
            self.emu.bus_mut().enqueue_key_event(rawkey, false);
        }
        // Reverse-debug: note the transition so replay can reproduce it.
        self.emu
            .tt_note_input(crate::inputsched::ReplayAction::Key { rawkey, pressed });
    }

    /// Start or stop the input recording (shortcut / menu item). On
    /// stop, the recorded session is written as a scripted-input file
    /// that `--script FILE` replays.
    pub(super) fn toggle_input_recording(&mut self) {
        match self.input_recorder.take() {
            Some(rec) => {
                let events = rec.events_recorded();
                let script = rec.finish();
                let path = crate::inputrec::auto_filename();
                match crate::paths::ensure_parent(&path)
                    .and_then(|()| std::fs::write(&path, script))
                {
                    Ok(()) => {
                        info!(
                            "input recording saved: {} ({events} events)",
                            path.display()
                        );
                        self.show_osd(format!(
                            "Saved {} ({events} events)",
                            display_file_name(&path)
                        ));
                    }
                    Err(e) => {
                        warn!("input recording save failed ({}): {e:#}", path.display());
                        self.show_osd("Input recording save failed (see log)");
                    }
                }
            }
            None => {
                let now = self.emu.bus().emulated_seconds();
                self.input_recorder = Some(crate::inputrec::InputRecorder::new(now));
                info!("input recording started at {now:.3}s emulated time");
                self.show_osd(format!(
                    "Recording input ({HOST_SHORTCUT_MODIFIER_LABEL}+Shift+R to stop)"
                ));
            }
        }
        self.request_redraw();
    }
}
