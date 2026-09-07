// SPDX-License-Identifier: GPL-3.0-or-later

//! winit + pixels integration. The emulator core runs synchronously on the
//! main thread inside `about_to_wait`; by default a worker renders the
//! completed frame while the main thread advances the next frame. winit and
//! wgpu presentation stay on the main thread.

use super::deinterlace::{Deinterlacer, OUT_HEIGHT};
#[cfg(feature = "game-library")]
use super::launcher::StatusKind;
use super::launcher::{LauncherField, LauncherState, MachineSetup, StatusMessage};
use super::ui::{self, Panel, UiControl, UiState};
use super::{
    bitplane, font, present_height, FB_HEIGHT, FB_PIXELS, FB_WIDTH, HOST_SHORTCUT_MODIFIER_LABEL,
    MAX_CANVAS_PIXELS, MAX_VISIBLE_LINES, PRESENT_HEIGHT_SQUARE,
};
use crate::audio::{AudioSink, CpalSink};
use crate::bus::{BeamWriteSource, FrontPanelStatus, PortDevice, VideoRenderFrameTiming};
use crate::config::{
    BezelStyle, Config, DisplayScaling, Overscan, PixelAspect, RawConfig, WarpSpeed,
};
use crate::emulator::Emulator;
use crate::heatmap;
use crate::keymap;
use crate::screenshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButtonKind {
    Left,
    Right,
    Middle,
}

/// A port-2 joystick/CD32-pad control scripted with `--joy-after`. Red
/// and Blue are the pad's fire/second buttons (a plain joystick's fire
/// is Red); the other five only exist in the CD32 pad's serial report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoyButtonKind {
    Up,
    Down,
    Left,
    Right,
    Red,
    Blue,
    Green,
    Yellow,
    Play,
    Rewind,
    Forward,
}

impl JoyButtonKind {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "up" => Self::Up,
            "down" => Self::Down,
            "left" => Self::Left,
            "right" => Self::Right,
            "red" | "fire" | "button1" => Self::Red,
            "blue" | "button2" => Self::Blue,
            "green" => Self::Green,
            "yellow" => Self::Yellow,
            "play" | "pause" => Self::Play,
            "rwd" | "rewind" | "reverse" => Self::Rewind,
            "ffw" | "forward" => Self::Forward,
            _ => return None,
        })
    }
}

// The host input source for the emulated port-2 joystick/CD32 pad is a
// configurable value, so it lives with the other config enums; re-exported
// here because the window/menu/ui code refers to it as
// `crate::video::window::JoystickInputMode`.
pub use crate::config::JoystickInputMode;

/// Where each host input source lands this quantum; see
/// [`host_routing_for`]. Ports are 0-based.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HostRouting {
    /// Port the host mouse drives: the lowest-numbered mouse port.
    pub(crate) mouse: Option<usize>,
    /// Port the physical gamepad drives (joystick/CD32 devices only).
    pub(crate) gamepad: Option<usize>,
    /// Port the gamepad drives as a mouse, in Gamepad Mouse mode: the
    /// same port the host mouse has, since the machine is given one
    /// mouse and two hands on it rather than two mice.
    pub(crate) gamepad_mouse: Option<usize>,
    /// Port keyboard mapping 0 (cursor keys) drives; the device there may
    /// be a joystick/pad or a mouse (keyboard mouse emulation).
    pub(crate) keyboard: Option<usize>,
    /// Port keyboard mapping 1 (numpad) drives, as the gamepad port's
    /// stand-in in a two-controller setup.
    pub(crate) keyboard2: Option<usize>,
}

/// The host input sources' port assignment for a device wiring and
/// joystick-input mode. Pure, and shared by the live input pump and the
/// launcher's Input-tab summary, so what the GUI promises is exactly what
/// the pump does. The rules are documented on [`App::host_routing`].
pub(crate) fn host_routing_for(devices: [PortDevice; 2], mode: JoystickInputMode) -> HostRouting {
    let mouse = devices.iter().position(|&d| d.is_mouse());
    let mut remaining = (0..2).filter(|&p| {
        Some(p) != mouse
            && matches!(
                devices[p],
                PortDevice::Mouse
                    | PortDevice::GamepadMouse
                    | PortDevice::Joystick
                    | PortDevice::Cd32Pad
            )
    });
    let first = remaining.next();
    let second = remaining.next();
    let (gamepad, keyboard, keyboard2) = match (first, second, mode) {
        (None, _, _) => (None, None, None),
        (Some(p), None, JoystickInputMode::Gamepad) => {
            if devices[p].is_mouse() {
                (None, None, None)
            } else {
                (Some(p), None, None)
            }
        }
        (Some(p), None, JoystickInputMode::Keyboard) => (None, Some(p), None),
        // Two leftover ports are always joysticks/pads: a second mouse
        // would itself have been claimed as the mouse port.
        (Some(p), Some(q), JoystickInputMode::Gamepad) => (Some(p), Some(q), Some(p)),
        (Some(p), Some(q), JoystickInputMode::Keyboard) => (Some(q), Some(p), Some(q)),
        // The pad is spent on the mouse, so no joystick port hears from
        // it: what is left over goes to the keyboard, which keeps its
        // joystick on the other port rather than losing it to the mode.
    };
    // A pad spent on the mouse is not also a joystick: whatever port it
    // would have driven goes to the keyboard instead, which keeps its
    // own joystick rather than losing it to the choice of mouse.
    let pad_mouse = devices.iter().position(|&d| d == PortDevice::GamepadMouse);
    let (gamepad, keyboard) = match (pad_mouse, gamepad) {
        (Some(_), Some(p)) => (None, keyboard.or(Some(p))),
        _ => (gamepad, keyboard),
    };
    HostRouting {
        mouse,
        gamepad,
        gamepad_mouse: pad_mouse,
        keyboard,
        keyboard2,
    }
}

/// How the pad moves the mouse in Gamepad Mouse mode, in quadrature
/// counts per second: where a held direction starts, and where both a
/// held direction and a fully deflected stick end up. The slow end is
/// about half the keyboard's mouse emulation, slow enough to place a
/// pointer on a gadget; the fast end crosses a PAL screen in about a
/// second.
const PAD_MOUSE_SLOW: f64 = 80.0;
const PAD_MOUSE_FAST: f64 = 640.0;
/// How long a held direction takes to reach the top speed.
const PAD_MOUSE_RAMP: std::time::Duration = std::time::Duration::from_millis(650);
/// How far a stick must be pushed before it is being pushed at all.
/// Smaller than the threshold the digital directions use: a mouse wants
/// the slow part of the throw that a switch has no use for.
const PAD_MOUSE_DEADZONE: f64 = 0.15;
/// The longest step the pointer is moved for in one pass, however long
/// the loop was away. A pause or a stall should not fling it.
const PAD_MOUSE_MAX_STEP: std::time::Duration = std::time::Duration::from_millis(50);

/// Quadrature counter steps per scheduler quantum (~one frame) while a
/// keyboard-mouse direction key is held: ~150 counts/second at PAL frame
/// rate, a comfortable Workbench pointer speed.
const KEYBOARD_MOUSE_COUNTS_PER_QUANTUM: i32 = 3;

/// One port's controls currently held by `--joy-after` scripting.
#[derive(Debug, Default, Clone, Copy)]
struct AutoJoyHeld {
    up: bool,
    down: bool,
    left: bool,
    right: bool,
    red: bool,
    blue: bool,
    green: bool,
    yellow: bool,
    play: bool,
    rwd: bool,
    ffw: bool,
}

impl AutoJoyHeld {
    fn set(&mut self, button: JoyButtonKind, held: bool) {
        match button {
            JoyButtonKind::Up => self.up = held,
            JoyButtonKind::Down => self.down = held,
            JoyButtonKind::Left => self.left = held,
            JoyButtonKind::Right => self.right = held,
            JoyButtonKind::Red => self.red = held,
            JoyButtonKind::Blue => self.blue = held,
            JoyButtonKind::Green => self.green = held,
            JoyButtonKind::Yellow => self.yellow = held,
            JoyButtonKind::Play => self.play = held,
            JoyButtonKind::Rewind => self.rwd = held,
            JoyButtonKind::Forward => self.ffw = held,
        }
    }
}

pub const DEFAULT_KEY_HOLD_MS: u32 = 100;
/// Emulated-frame gap between reverse-debug snapshots when the ring is
/// auto-armed by opening the debugger window. Larger than the headless
/// default to keep the per-snapshot serialize off the interactive path.
const DEBUGGER_REVERSE_INTERVAL_FRAMES: u64 = 10;
const MAX_TEXTURE_SCALE: usize = 2;
/// Cap on the integer-scaling supersample factor, which follows the window
/// fit rather than the host DPI (see `plan_present_scaling`). Bounds the
/// backing texture and the per-frame present copy on very large displays;
/// at 4x the canvas the texture is already 2864 physical pixels wide. The
/// *displayed* multiple is not capped: the scaler pass draws the planned
/// multiple whatever the texture factor, and its point sampling of the
/// replicated display region stays exact past the cap (see `scaler`).
const MAX_INTEGER_TEXTURE_SCALE: usize = 4;
const STATUS_BAR_HEIGHT: usize = 44;
/// How long the pointer rests on a menu category before it opens.
const MENU_SUBMENU_DWELL: std::time::Duration = std::time::Duration::from_millis(500);
/// Logical window height: the presentation canvas for the active pixel aspect,
/// plus the status bar below it unless it is hidden (in which case the display
/// scales to fill the whole window).
fn window_present_height() -> usize {
    present_height()
        + mt32_panel_height()
        + csynth_panel_height()
        + keyboard_panel_height()
        + status_bar_height()
}

/// Scanlines the CRT pass draws across the display rect: the emulated field
/// lines the present copy actually puts on screen, rescaled when that copy
/// letterboxes them inside the rect.
///
/// `tv_aperture_rows` mirrors `copy_window_present_frame`'s own branch
/// condition: Some when that path shows a TV-aperture crop instead of the
/// whole woven buffer, carrying the crop's row count, so the line count
/// comes from the aperture, not from `present_rows` -- 270 lines, not 285,
/// in the default 50 Hz TV-overscan presentation, 214 on a 60 Hz scan. The
/// square-pixel canvas is taller than the aperture and pads it with bezel
/// rows (`tv_aperture_source_row`), so the count scales back up by the
/// rect/content ratio to keep the pitch right across the whole viewport.
fn crt_scanline_count(
    present_rows: usize,
    present_h: usize,
    tv_aperture_rows: Option<usize>,
) -> f32 {
    let (woven_rows, content_rows) = if let Some(aperture_rows) = tv_aperture_rows {
        let pad = if present_h == PRESENT_HEIGHT_SQUARE {
            present_h.saturating_sub(aperture_rows) / 2
        } else {
            0
        };
        (aperture_rows, present_h - 2 * pad)
    } else {
        (present_rows, present_h)
    };
    // Two woven rows per emulated field line. The pass never runs on a
    // programmable scan, whose fields are not woven at all.
    let lines = (woven_rows / 2).max(1);
    if content_rows == 0 {
        return lines as f32;
    }
    (lines * present_h) as f32 / content_rows as f32
}

/// The status bar's height, or 0 while it is hidden.
fn status_bar_height() -> usize {
    if super::status_bar_hidden() {
        0
    } else {
        STATUS_BAR_HEIGHT
    }
}

/// Where the status bar starts: below the display and below whichever
/// strips are up. The bar sits at the very bottom either way; the strips
/// take the room immediately above it.
fn status_bar_top() -> usize {
    keyboard_panel_top() + keyboard_panel_height()
}

/// The MT-32 panel's height, or 0 while it is not shown. It sits between the
/// display and the status bar, the way the real unit sits under the monitor.
fn mt32_panel_height() -> usize {
    #[cfg(feature = "mt32")]
    if super::mt32_panel_shown() {
        return mt32panel::MT32_PANEL_HEIGHT;
    }
    0
}

/// Coppersynth's panel height, or 0 while it is not shown.
fn csynth_panel_height() -> usize {
    #[cfg(feature = "coppersynth")]
    if super::csynth_panel_shown() {
        return csynthpanel::CSYNTH_PANEL_HEIGHT;
    }
    0
}

/// Where Coppersynth's panel starts: under the display, below the
/// MT-32's strip when that one is up as well.
fn csynth_panel_top() -> usize {
    present_height() + mt32_panel_height()
}

/// Where the on-screen keyboard starts: under the display and under the
/// synth panels, the way a keyboard sits below whatever is on the desk.
fn keyboard_panel_top() -> usize {
    csynth_panel_top() + csynth_panel_height()
}

/// The on-screen keyboard's height, or 0 while it is not shown.
fn keyboard_panel_height() -> usize {
    if super::keyboard_panel_shown() {
        kbdpanel::KBD_PANEL_HEIGHT
    } else {
        0
    }
}
const STATUS_LABEL_X: usize = 18;
const STATUS_LED_X: usize = 58;
const STATUS_LED_Y_OFFSET: usize = 1;
const STATUS_LED_W: usize = 58;
const STATUS_LED_H: usize = 9;
// LED rows (PWR/FDD always; HDD and CD when the machine has them) are
// spaced like the original three fixed rows up to three rows, and packed
// tighter when a machine shows all four.
const LED_ROW_START_Y: usize = 4;
const LED_ROW_PITCH: usize = 14;
const LED_ROW_START_Y_TIGHT: usize = 1;
const LED_ROW_PITCH_TIGHT: usize = 11;
const STATUS_CONTROL_H: usize = 22;
const STATUS_CONTROL_Y: usize = (STATUS_BAR_HEIGHT - STATUS_CONTROL_H) / 2;
const VOLUME_STEP_PERCENT: i16 = 5;
/// Per-press step of the live sampler input-gain control, in decibels; the
/// range is the sampler's [`crate::sampler::MIN_SAMPLER_GAIN_DB`]..
/// [`crate::sampler::MAX_SAMPLER_GAIN_DB`].
const SAMPLER_GAIN_STEP_DB: f32 = 3.0;

/// Label a sampler gain in decibels for the OSD/menu, e.g. `0 dB`, `+6 dB`.
fn sampler_gain_osd(gain_db: f32) -> String {
    if gain_db.abs() < 0.05 {
        "0 dB".to_string()
    } else {
        format!("{gain_db:+.0} dB")
    }
}
// Media (floppy/CD) button clusters. Each connected drive gets a wide
// load button plus narrow swap and eject buttons; a CD machine gets a
// load and an eject button after the drives.
const MEDIA_CLUSTER_X: usize = 198;
// With three or four drives the clusters stack two-up in slightly
// shorter rows, so the bar never has to shed the track counter.
const MEDIA_STACKED_H: usize = 19;
const MEDIA_STACKED_ROW0_Y: usize = 2;
const MEDIA_STACKED_PITCH: usize = 21;
const MEDIA_CLUSTER_GAP: usize = 6;
const MEDIA_CD_GAP: usize = 12;
const MEDIA_LOAD_W: usize = 22;
const MEDIA_SMALL_W: usize = 16;
const MEDIA_INNER_GAP: usize = 2;
const MEDIA_CLUSTER_W: usize = MEDIA_LOAD_W + 2 * (MEDIA_INNER_GAP + MEDIA_SMALL_W);
// Screenshot button, menu button, and volume control, pinned on the
// right ahead of the pause/power/reboot block. The menu button anchor
// lives in `ui` so the pop-up menu can align with it.
const SHOT_BUTTON_X: usize = FB_WIDTH - 190;
const SHOT_BUTTON_W: usize = 22;
const VOLUME_SLIDER_X: usize = ui::MENU_BUTTON_X - 10 - VOLUME_SLIDER_W;
const VOLUME_SLIDER_Y: usize = STATUS_CONTROL_Y + 7;
// The slider is as wide as the slot between the media controls and the menu
// button leaves once the two icon toggles below have taken their 24 pixels
// each: the bar has one free run of x, and every control on it competes for
// the same worst case (four floppies plus a CD, ending at x=372).
const VOLUME_SLIDER_W: usize = 48;
const VOLUME_SLIDER_H: usize = 8;
const VOLUME_KNOB_W: usize = 8;
const VOLUME_KNOB_H: usize = 16;
const VOLUME_GLYPH_X: usize = VOLUME_SLIDER_X - 16;
// Joystick input-source and on-screen-keyboard toggles: compact icon buttons
// just left of the volume glyph, in the otherwise-free slot before the
// right-hand control cluster. The widest media layout (four floppies plus a
// CD) ends at x=372, so the pair of 22px buttons here clears both the media
// controls and the speaker glyph; this is verified by
// `joystick_toggle_clears_worst_case_media` and
// `keyboard_toggle_clears_worst_case_media`.
const JOY_TOGGLE_W: usize = 22;
const JOY_TOGGLE_X: usize = VOLUME_GLYPH_X - 2 - JOY_TOGGLE_W;
const KBD_TOGGLE_W: usize = 22;
const KBD_TOGGLE_X: usize = JOY_TOGGLE_X - 2 - KBD_TOGGLE_W;
// The standard-window and TV-aperture constants live in
// `video/present_common.rs` with the presentation helpers they anchor
// (re-exported through `use present::*` below). Both the live window and
// the PNG paths present the captured aperture -- every glass pixel
// derives from real framebuffer pixels, the standard window and the
// visible raster both exactly centred. On the 4:3 canvas the aperture
// resamples onto the full glass width; the square-pixel canvas keeps
// unit columns and centres the aperture between these black side pads
// instead.
const TV_LIVE_PAD_X: usize = (FB_WIDTH - TV_CAPTURED_WIDTH) / 2;
// Symmetric pads are what centres the square-pixel raster; a change to
// the captured aperture's width that breaks this must rethink the live
// layout.
const _: () = assert!(TV_LIVE_PAD_X * 2 + TV_CAPTURED_WIDTH == FB_WIDTH);
pub(super) const STATUS_BG: u32 = rgba(28, 28, 26);
pub(super) const STATUS_TOP: u32 = rgba(78, 76, 70);
const STATUS_BOTTOM: u32 = rgba(12, 12, 11);
const LED_BEZEL_DARK: u32 = rgba(8, 8, 7);
const LED_BEZEL_LIGHT: u32 = rgba(78, 76, 68);
// The power LED is lit whenever the machine is powered, driven by CIA-A's
// /LED line the way it drives the LED on an A500 rev 6 or later board:
// POWER_LED_BRIGHT while the guest holds /LED engaged (Paula's filter on),
// falling to the clearly dimmer -- but still lit -- POWER_LED_DIM once it
// releases the line. Earlier boards extinguished the LED instead; the
// panel models the common two-level behaviour. POWER_LED_OFF is the
// unpowered bezel.
const POWER_LED_BRIGHT: u32 = rgba(255, 38, 28);
const POWER_LED_DIM: u32 = rgba(150, 24, 18);
const POWER_LED_OFF: u32 = rgba(66, 12, 10);
const FDD_LED_ON: u32 = rgba(236, 142, 28);
const FDD_LED_OFF: u32 = rgba(72, 38, 10);
const HDD_LED_ON: u32 = rgba(44, 200, 80);
const HDD_LED_OFF: u32 = rgba(14, 56, 24);
const CD_LED_ON: u32 = rgba(64, 170, 234);
const CD_LED_OFF: u32 = rgba(16, 46, 70);
const TRACK_DISPLAY_BG: u32 = rgba(6, 8, 6);
const TRACK_SEGMENT_ON: u32 = rgba(27, 220, 71);
const TRACK_SEGMENT_OFF: u32 = rgba(11, 45, 19);
const TRACK_SEGMENT_HIGHLIGHT: u32 = rgba(119, 255, 141);
const CD_TRACK_SEGMENT_ON: u32 = CD_LED_ON;
const CD_TRACK_SEGMENT_OFF: u32 = CD_LED_OFF;
const CD_TRACK_SEGMENT_HIGHLIGHT: u32 = rgba(140, 214, 255);
pub(super) const BUTTON_FACE: u32 = rgba(46, 46, 43);
pub(super) const BUTTON_FACE_HOVER: u32 = rgba(62, 62, 58);
pub(super) const BUTTON_EDGE_LIGHT: u32 = rgba(118, 116, 106);
pub(super) const BUTTON_EDGE_DARK: u32 = rgba(13, 13, 12);
const BUTTON_GLYPH: u32 = rgba(0, 174, 0);
/// Glyph colour for visible-but-inactive controls (eject with no disk,
/// swap with no other disk queued).
const BUTTON_GLYPH_DISABLED: u32 = rgba(96, 94, 86);
const POWER_GLYPH_ON: u32 = rgba(0, 174, 0);
const POWER_GLYPH_OFF: u32 = rgba(150, 36, 30);
const RESET_GLYPH: u32 = rgba(250, 200, 40);
const DISK_BODY: u32 = rgba(28, 82, 184);
const DISK_BODY_HIGHLIGHT: u32 = rgba(74, 139, 238);
const DISK_BODY_SHADOW: u32 = rgba(8, 26, 84);
const DISK_SHUTTER: u32 = rgba(184, 191, 196);
const DISK_SHUTTER_DARK: u32 = rgba(83, 91, 98);
const DISK_LABEL: u32 = rgba(238, 240, 232);
const DISK_LABEL_LINE: u32 = rgba(130, 139, 150);
const CD_BODY: u32 = rgba(186, 193, 202);
const CD_SHEEN: u32 = rgba(240, 244, 250);
const CD_HUB: u32 = rgba(120, 124, 130);
const CD_HOLE: u32 = rgba(24, 24, 26);
const CAMERA_BODY: u32 = rgba(190, 188, 178);
const CAMERA_LENS: u32 = rgba(20, 22, 24);
pub(super) const STATUS_TEXT: u32 = rgba(174, 170, 154);
const VOLUME_FILL: u32 = rgba(44, 178, 94);
const VOLUME_FILL_HIGHLIGHT: u32 = rgba(128, 244, 150);
const WINDOW_TITLE: &str = concat!("Copperline ", env!("COPPERLINE_DISPLAY_VERSION"));

/// The title on the window: Copperline's own, unless a player build adopted
/// the game's through [`crate::video::set_branding`].
fn window_title() -> &'static str {
    crate::video::branding_title().unwrap_or(WINDOW_TITLE)
}
const COPPERLINE_LOGO_PNG: &[u8] = include_bytes!("../../assets/brand/copperline-logo.png");
const COPPERLINE_ICON_PNG: &[u8] = include_bytes!("../../assets/brand/copperline-icon.png");
const MOUSE_MOTION_SCALE: f64 = 1.0;

/// Which front `Cmd/Alt+M` should switch back on for a starting style: the
/// style itself when one is chosen, and otherwise the default front, so the
/// shortcut has something to turn on from a session that starts with the
/// bezel off.
fn last_bezel_style(style: BezelStyle) -> BezelStyle {
    if style.is_on() {
        style
    } else {
        BezelStyle::Model1084
    }
}

/// Whether a window's logical inner size equals the presentation canvas
/// (FB_WIDTH x `canvas_height`) within a small rounding tolerance -- i.e. the
/// user has not manually resized it.
fn logical_size_is_canvas(logical_w: f64, logical_h: f64, canvas_height: usize) -> bool {
    (logical_w - FB_WIDTH as f64).abs() < 2.0 && (logical_h - canvas_height as f64).abs() < 2.0
}

const CANVAS_SNAP_RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// What a canvas change still owes the window, when it could not be paid
/// at the time: fullscreen was holding the window and nothing could be
/// resized.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum CanvasFollow {
    /// The window was the canvas's own size: put it back on the new one.
    Snap,
    /// The window is the user's: move its height by what the canvas moved,
    /// in logical pixels, and leave the width they chose alone.
    Nudge(i32),
}

/// Whether a resize still belongs to the presentation canvas. The first
/// event after an asynchronous `request_inner_size` is the platform's answer
/// even when a window-manager limit clamps it far away from the requested
/// dimensions. A platform may ignore the request entirely, so the ownership
/// expires rather than swallowing a user resize much later.
fn resize_is_canvas_owned(
    snap_request_deadline: &mut Option<Instant>,
    now: Instant,
    logical_w: f64,
    logical_h: f64,
    canvas_height: usize,
) -> bool {
    snap_request_deadline
        .take()
        .is_some_and(|deadline| now <= deadline)
        || logical_size_is_canvas(logical_w, logical_h, canvas_height)
}

/// Host mouse speed multiplier for a 0-100 sensitivity. Exponential so 50 is
/// exactly 1:1 (2^0), 0 is a quarter speed (2^-2) and 100 quadruple (2^2),
/// with perceptually even steps between.
fn mouse_sensitivity_factor(sensitivity: u8) -> f64 {
    2.0_f64.powf((f64::from(sensitivity.min(100)) - 50.0) / 25.0)
}
/// How long a transient on-screen overlay message (screenshot saved,
/// disk swapped) stays visible.
const OSD_DURATION: std::time::Duration = std::time::Duration::from_millis(2500);

/// How long the pad's Quit hotkey (Select on the standard layout, a
/// calibrated Quit or Menu control otherwise) must be held before the
/// application exits: long enough that a stray press cannot end the
/// session, short enough to feel immediate.
const GAMEPAD_QUIT_HOLD: std::time::Duration = std::time::Duration::from_millis(1500);

/// OSD countdown shown while the Quit hotkey is held; also the marker for
/// withdrawing that countdown when the hold is released early.
const GAMEPAD_QUIT_OSD_PREFIX: &str = "Quitting: keep holding ";

/// On-screen overlay colours (packed R,G,B,A in memory order).
const OSD_TEXT: u32 = rgba(236, 236, 232);
/// Amber, for a message about something that did not go as asked.
const OSD_TEXT_WARNING: u32 = rgba(248, 205, 78);
const OSD_SHADOW: u32 = rgba(0, 0, 0);
const OSD_BG: u32 = rgba(10, 10, 12);
const RECORD_DOT: u32 = rgba(229, 56, 48);
const AMIGA_RAWKEY_LEFT_SHIFT: u8 = 0x60;
const AMIGA_RAWKEY_RIGHT_SHIFT: u8 = 0x61;
const AMIGA_RAWKEY_LEFT_ALT: u8 = 0x64;
const AMIGA_RAWKEY_RIGHT_ALT: u8 = 0x65;

/// The quick-save slot a number-row key selects: `1`..`9` are slots 1-9 and
/// `0` is slot 10, so the ten slots sit under the row in printed order.
/// `None` for every other key.
fn save_slot_for_key(code: KeyCode) -> Option<usize> {
    Some(match code {
        KeyCode::Digit1 => 1,
        KeyCode::Digit2 => 2,
        KeyCode::Digit3 => 3,
        KeyCode::Digit4 => 4,
        KeyCode::Digit5 => 5,
        KeyCode::Digit6 => 6,
        KeyCode::Digit7 => 7,
        KeyCode::Digit8 => 8,
        KeyCode::Digit9 => 9,
        KeyCode::Digit0 => crate::savestate::SLOT_COUNT,
        _ => return None,
    })
}

fn host_shortcut_modifier_pressed(modifiers: ModifiersState) -> bool {
    if cfg!(target_os = "macos") {
        modifiers.super_key()
    } else {
        modifiers.alt_key()
    }
}

/// Display name for a GDB-style register index (see `debug_set_register`):
/// D0-D7, A0-A7, SR, PC.
fn gdb_reg_label(reg: usize) -> String {
    match reg {
        0..=7 => format!("D{reg}"),
        8..=15 => format!("A{}", reg - 8),
        16 => "SR".to_string(),
        17 => "PC".to_string(),
        _ => format!("r{reg}"),
    }
}

fn window_title_mouse_captured() -> String {
    format!(
        "{} - Mouse captured ({HOST_SHORTCUT_MODIFIER_LABEL}+G releases)",
        window_title()
    )
}

/// A transient on-screen overlay message drawn over the display (but not
/// captured in screenshots, since it is painted into the presentation
/// texture, never into the emulated framebuffer `fb`).
struct Osd {
    text: String,
    expires_at: Instant,
    /// Drawn in amber rather than white: something did not go as asked.
    warning: bool,
}

/// How often the performance overlay resamples its counters. Twice a
/// second keeps the numbers readable; per-frame updates flicker.
const PERF_SAMPLE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// Live readout state behind the performance overlay (Cmd/Alt+P,
/// `[display] perf_overlay`): the formatted lines drawn each frame plus
/// the counter baseline the next sample's deltas are taken against.
#[derive(Default)]
struct PerfOverlay {
    lines: Vec<String>,
    /// Bumped whenever `lines` changes so `MainRedrawState` repaints.
    revision: u64,
    baseline: Option<PerfBaseline>,
}

/// One sample of the cumulative counters the overlay derives rates from.
struct PerfBaseline {
    at: Instant,
    /// Whether the machine was advancing when this sample was taken. Rates
    /// across a pause/resume boundary would mix running and idle time, so a
    /// flip publishes the idle readout and re-baselines instead.
    running: bool,
    emulated_frames: u64,
    emulated_seconds: f64,
    busy: std::time::Duration,
    audio_underrun_frames: u64,
}

/// The numbers behind one refresh of the performance overlay, derived from
/// counter deltas by `perf_readout` and formatted by `perf_overlay_lines`.
/// Kept as plain values so both steps are testable.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct PerfReadout {
    /// Emulated video frames retired per host second.
    fps: f64,
    /// Emulated seconds advanced per host second (1.0 = locked to real time).
    speed: f64,
    /// Host milliseconds of emulation work per emulated frame (pacing
    /// sleeps excluded).
    emu_frame_ms: f64,
    /// Share of host wall time spent emulating, in percent. In paced mode
    /// this equals `emu_frame_ms` over the frame period (20.0 ms PAL,
    /// 16.7 ms NTSC): the share of the host frame budget used.
    host_percent: f64,
    /// Live audio output lead in milliseconds (the underrun cushion).
    audio_lead_ms: f64,
    /// Audio underrun frames per host second.
    audio_underruns_per_s: f64,
    /// Pacer catch-up events since the last guest reset: the machine fell
    /// hopelessly behind real time and dropped emulated time. Frames are
    /// never skipped in paced mode; this counts the only case where time is.
    pacer_slips: u32,
}

fn perf_readout(
    base: &PerfBaseline,
    current: &PerfBaseline,
    audio_lead_ms: f64,
    pacer_slips: u32,
) -> PerfReadout {
    let dt = current.at.duration_since(base.at).as_secs_f64();
    if dt <= 0.0 {
        return PerfReadout {
            audio_lead_ms,
            pacer_slips,
            ..Default::default()
        };
    }
    // Counters that moved backwards (a guest reset cleared the stats, a
    // timeline jump rewound the machine) saturate to an empty window; the
    // next sample is taken against the fresh values.
    let frames = current.emulated_frames.saturating_sub(base.emulated_frames) as f64;
    let busy = current.busy.saturating_sub(base.busy).as_secs_f64();
    let emulated = (current.emulated_seconds - base.emulated_seconds).max(0.0);
    let underruns = current
        .audio_underrun_frames
        .saturating_sub(base.audio_underrun_frames) as f64;
    PerfReadout {
        fps: frames / dt,
        speed: emulated / dt,
        emu_frame_ms: if frames > 0.0 {
            busy * 1000.0 / frames
        } else {
            0.0
        },
        host_percent: busy / dt * 100.0,
        audio_lead_ms,
        audio_underruns_per_s: underruns / dt,
        pacer_slips,
    }
}

/// One line per data point, top to bottom as drawn.
fn perf_overlay_lines(r: &PerfReadout) -> Vec<String> {
    vec![
        format!("{:.1} fps", r.fps),
        format!("x{:.2}", r.speed),
        format!("emu {:.1} ms", r.emu_frame_ms),
        format!("host {:.0}%", r.host_percent),
        format!("audio {:.0} ms", r.audio_lead_ms),
        format!("xrun {:.0}", r.audio_underruns_per_s),
        format!("slip {}", r.pacer_slips),
    ]
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeyPressSpec {
    pub secs: f32,
    pub rawkey: u8,
    pub hold_ms: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FrameDumpSpec {
    pub dir: PathBuf,
    pub start_secs: f32,
    pub count: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiskInsertSpec {
    pub secs: f32,
    pub drive_idx: usize,
    pub path: PathBuf,
    pub write_protected: bool,
}

use anyhow::{anyhow, Context, Result};
use log::{error, info, warn};
use pixels::{Pixels, PixelsBuilder, ScalingMode, SurfaceTexture};
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::{
    mpsc::{self, Receiver, SyncSender, TryRecvError},
    Arc, OnceLock,
};
use std::thread::JoinHandle;
use std::time::Instant;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalSize};
use winit::event::{
    DeviceEvent, DeviceId, ElementState, KeyEvent, MouseButton, MouseScrollDelta, RawKeyEvent,
    WindowEvent,
};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::{CursorGrabMode, Fullscreen, Icon, Window, WindowAttributes, WindowId};

fn rawkey_index(rawkey: u8) -> usize {
    (rawkey & 0x7F) as usize
}

/// Where a rawkey transition came from.
///
/// Two things can have a key down at once -- a finger on the host keyboard
/// and a click on the on-screen keyboard -- and each keeps its own held
/// table so its own repeats and stale releases are dropped. What the
/// machine is told is the *aggregate*: the key is down for it while either
/// source holds it, and only a change in that aggregate is enqueued,
/// recorded, or noted for replay. Without that, pressing a cap the host is
/// already holding would be swallowed as a duplicate while its release
/// still went through, cutting the host's key short -- and the strip would
/// go on drawing a latch the keyboard MCU never heard about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeySource {
    /// The host keyboard, both the focused and raw-device paths.
    Host,
    /// The on-screen keyboard strip.
    Panel,
}

fn rawkey_is_held(held_rawkeys: &[bool; 128], rawkey: u8) -> bool {
    held_rawkeys[rawkey_index(rawkey)]
}

fn rawkey_transition_is_duplicate(held_rawkeys: &[bool; 128], rawkey: u8, pressed: bool) -> bool {
    rawkey_is_held(held_rawkeys, rawkey) == pressed
}

fn repeated_main_key_should_drop(
    held_rawkeys: &[bool; 128],
    code: KeyCode,
    state: ElementState,
    repeat: bool,
    ui_accepts_repeat: bool,
) -> bool {
    if !repeat || state != ElementState::Pressed || ui_accepts_repeat {
        return false;
    }
    match host_to_amiga_rawkey(code) {
        Some(rawkey) => rawkey_is_held(held_rawkeys, rawkey),
        None => true,
    }
}

fn animated_focus_needs_continuous_redraw(running: bool, showing: bool) -> bool {
    running && showing
}

fn raw_device_qualifier_rawkey(code: KeyCode) -> Option<u8> {
    match code {
        KeyCode::ShiftLeft => Some(AMIGA_RAWKEY_LEFT_SHIFT),
        KeyCode::ShiftRight => Some(AMIGA_RAWKEY_RIGHT_SHIFT),
        KeyCode::AltLeft => Some(AMIGA_RAWKEY_LEFT_ALT),
        KeyCode::AltRight => Some(AMIGA_RAWKEY_RIGHT_ALT),
        _ => None,
    }
}

fn raw_device_qualifier_family_held(held_rawkeys: &[bool; 128], left: u8, right: u8) -> bool {
    rawkey_is_held(held_rawkeys, left) || rawkey_is_held(held_rawkeys, right)
}

/// Every heat-map toucher, in [`heatmap::Toucher`] code order. The Memory
/// tab's census lists all of them, including the ones holding nothing, so
/// the column doubles as the map's legend and its rows never move.
const HEAT_TOUCHERS: [heatmap::Toucher; 8] = [
    heatmap::Toucher::CpuRead,
    heatmap::Toucher::CpuWrite,
    heatmap::Toucher::Blitter,
    heatmap::Toucher::Copper,
    heatmap::Toucher::Disk,
    heatmap::Toucher::Bitplane,
    heatmap::Toucher::Sprite,
    heatmap::Toucher::Audio,
];

/// The Memory tab's window presets: one per fitted RAM bank, then the
/// whole 24-bit space.
///
/// They come from the machine's decoded bank map rather than a fixed list
/// of addresses because that map is what the fitted machine actually has:
/// a Zorro board sits wherever autoconfig placed it, and the motherboard,
/// CPU-slot and Zorro III banks a 32-bit CPU sees live above the 24-bit
/// space entirely -- which is exactly why the heat map's window is
/// movable. A fixed list would either name banks this machine does not
/// have or fail to reach the ones it does.
fn analyzer_heat_presets(bus: &crate::bus::Bus) -> Vec<ui::HeatPreset> {
    let mb_base = bus.mem.mb_ram_base() as u32;
    let mut presets: Vec<ui::HeatPreset> = bus
        .writable_ram_regions()
        .into_iter()
        .map(|(base, len)| {
            let label = if base == crate::memory::CHIP_RAM_BASE as u32 {
                "Chip"
            } else if base == crate::memory::SLOW_RAM_BASE as u32 {
                "Slow"
            } else if base == mb_base {
                "MB"
            } else if base == crate::memory::ACCEL_RAM_BASE as u32 {
                "CPU"
            } else if base < 0x0100_0000 {
                // What is left is a RAM board: one autoconfigured into the
                // Zorro II space, or a Zorro III board above it.
                "Z2"
            } else {
                "Z3"
            };
            ui::HeatPreset {
                label: label.to_string(),
                base,
                // The fitted bank length, so a preset's window covers the
                // RAM that exists rather than the select window it decodes
                // in (a smaller bank repeats inside its window).
                span: len,
            }
        })
        .collect();
    // Guest-registered debug resources (crate::uaelib) are windows too: a
    // program that told the emulator where its bitmaps, palettes and
    // copper lists live gets a preset per resource, named by the guest.
    // Rebuilt on tab entry, so a resource registered while the tab is up
    // appears the next time the Memory tab is entered.
    if let Some(uaelib) = bus.uaelib.as_ref() {
        // Bounded so a large registry cannot push the machine windows'
        // trailing presets (notably 24-bit) off the single preset row;
        // the Resources tab lists and scrolls the full registry.
        const RESOURCE_PRESETS_MAX: usize = 4;
        for resource in uaelib.resources().iter().take(RESOURCE_PRESETS_MAX) {
            // By characters, not bytes: the name is guest data run
            // through from_utf8_lossy, and a byte-indexed truncate can
            // panic inside a multibyte character.
            let label: String = resource.name.chars().take(8).collect();
            presets.push(ui::HeatPreset {
                label,
                base: resource.address,
                span: resource.size.max(1),
            });
        }
    }
    // Two boards of the same kind would otherwise offer two buttons with
    // the same name; the base address tells them apart.
    let labels: Vec<String> = presets.iter().map(|preset| preset.label.clone()).collect();
    for (index, preset) in presets.iter_mut().enumerate() {
        let label = preset.label.clone();
        if labels
            .iter()
            .enumerate()
            .any(|(other, other_label)| other != index && *other_label == label)
        {
            preset.label = format!("{label} ${:X}", preset.base);
        }
    }
    presets.push(ui::HeatPreset {
        label: "24-bit".to_string(),
        base: 0,
        span: heatmap::DEFAULT_SPAN,
    });
    presets
}

/// The window the Memory tab arms when nothing has armed the map yet: the
/// chip RAM bank. It is the bank every chip-bus engine works out of and
/// usually the smallest fitted one, so its cells cover the fewest bytes
/// each -- the most legible default. A machine with no chip bank at all
/// falls back to the 24-bit overview.
fn analyzer_default_heat_window(bus: &crate::bus::Bus) -> (u32, u32) {
    bus.writable_ram_regions()
        .into_iter()
        .find(|(base, _)| *base == crate::memory::CHIP_RAM_BASE as u32)
        .unwrap_or((crate::memory::CHIP_RAM_BASE as u32, heatmap::DEFAULT_SPAN))
}

pub struct App {
    netplay: Option<crate::netplay::Session>,
    netplay_input: crate::netplay::Input,
    netplay_keyboard_controller: bool,
    netplay_setup: Option<crate::video::launcher::NetplaySetup>,
    // Linux serves clipboard selections from the owning instance.
    host_clipboard: Option<arboard::Clipboard>,
    emu: Emulator,
    fb: Vec<u32>,
    /// Merges rendered fields into the double-height presentation
    /// buffer that the window texture, screenshots, and frame dumps
    /// read (see [`deinterlace`](super::deinterlace)).
    deinterlacer: Deinterlacer,
    /// The machine's resolved deinterlace and phosphor settings. Carried
    /// in every render job so the worker's own deinterlacer follows
    /// them; also applied to `deinterlacer` for the synchronous fallback
    /// path.
    deinterlace: bool,
    phosphor: f32,
    /// Active presentation buffer, already deinterlaced/line-doubled and
    /// post-processed. The first `present_rows * FB_WIDTH` pixels are valid.
    present_fb: Vec<u32>,
    present_rows: usize,
    /// Pixels per `present_fb` row: FB_WIDTH classically, twice that for
    /// a 35 ns super-hi-res canvas.
    present_width: usize,
    /// TV-aperture crop rows for the presented frame when it is a standard
    /// 15 kHz scan with the standard horizontal window (None otherwise);
    /// applied by the present copy under `Overscan::Tv`.
    present_tv_aperture_rows: Option<usize>,
    /// Aperture/recentring decisions latched across border-only frames:
    /// the blank frames a screen change emits keep the previous
    /// presentation geometry instead of snapping to the full framebuffer,
    /// so the picture does not jump at every Kickstart mode change.
    presentation_latch: PresentationLatch,
    /// The smoothed playfield content envelope the autocrop presentation
    /// shows, in woven presentation-buffer space (see [`AutocropLatch`]);
    /// `None` until a standard frame has painted playfield.
    present_content_rect: Option<bitplane::ContentRect>,
    /// The last layout the COPPERLINE_DIAG_AUTOCROP trace logged, so the
    /// trace reports changes rather than every redraw. Unused while the
    /// flag is off.
    last_diag_layout: Option<PresentLayout>,
    /// The grow-fast/shrink-stable smoothing behind it.
    autocrop_latch: AutocropLatch,
    /// Whether the presented frame came from a programmable (multisync) scan
    /// rather than a woven 15 kHz one. Those fields reach the presentation
    /// buffer at their native height, so neither the CRT pass nor its
    /// two-rows-per-line count applies to them.
    present_programmable: bool,
    /// Scratch for composing an RTG board frame (Z3660 scanout); reused
    /// across frames to avoid a per-frame allocation.
    rtg_fb: Vec<u32>,
    /// Native (width, height) of the RTG frame in `rtg_fb` when the last
    /// presented frame was RTG; `None` when the chipset drives the display.
    /// The draw path uploads `rtg_fb` to the RTG texture when set.
    rtg_present_dims: Option<(u32, u32)>,
    render: Option<Render>,
    debugger_tool_window: Option<ToolWindow>,
    frame_analyzer_tool_window: Option<ToolWindow>,
    console_tool_window: Option<ToolWindow>,
    /// When the frame loop last requested a paced tool window repaint
    /// (see TOOL_REDRAW_INTERVAL).
    last_tool_redraw: Instant,
    debugger_panel: Option<ui::DebuggerPanel>,
    frame_analyzer_panel: Option<ui::FrameAnalyzerPanel>,
    /// The debugger console: a GDB-flavoured command line in its own tool
    /// window, so it can sit beside the debugger and Frame Analyzer.
    console_panel: Option<ui::ConsolePanel>,
    /// Beam-space render of the analyzer trace's frame for the picture
    /// underlay: unlike `fb`, no presentation recentring or TV masking is
    /// applied, so its pixels line up with the DMA trace's beam grid.
    /// Shared with the per-redraw view via Rc to avoid copying the frame.
    analyzer_underlay_fb: std::rc::Rc<Vec<u32>>,
    /// Rows valid in `analyzer_underlay_fb` (the traced frame's scan height).
    analyzer_underlay_rows: usize,
    /// Pixels per `analyzer_underlay_fb` row (the frame's canvas width).
    analyzer_underlay_width: usize,
    /// Emulated frame `analyzer_underlay_fb` was rendered for.
    analyzer_underlay_frame: Option<u64>,
    /// Recycled snapshot buffers for the underlay's side-effect-free render.
    analyzer_underlay_input: Option<bitplane::RenderInput>,
    /// Tracks whether the Frame Analyzer armed the bus heat map, so closing
    /// it releases only a map it owns (a map armed over the control
    /// protocol is left alone).
    heatmap_armed_by_panel: bool,
    render_worker: Option<RenderWorker>,
    render_recycle_fb: Vec<u32>,
    /// Spent frame snapshot handed back by the render worker; reused by the
    /// next `RenderInput::refill_from_bus` to avoid re-allocating its
    /// buffers (the chip-RAM copy alone is up to 2 MiB) every frame.
    render_recycle_input: Option<bitplane::RenderInput>,
    cpu_halted: bool,
    /// Host-level power state. When false the emulator does not step;
    /// the machine sits powered off until the status-bar power button
    /// is clicked. Distinct from the emulated (CIA-driven) power LED.
    powered_on: bool,
    /// Whether the next `run_machine` keeps the configuration's own
    /// `power_on` rather than the Run button's power-it-on. Set around the
    /// two automatic launches (`auto_launch_if_asked`, Load...): nobody
    /// pressed Run there, so nobody overrode the file.
    run_honors_power_on: bool,
    /// Host-level pause state. When true the emulator does not step but
    /// stays powered on, so the last rendered frame is held on screen and
    /// emulation resumes from the same point when unpaused.
    paused: bool,
    /// Scheduled --screenshot-after captures, earliest deadline first.
    /// The flag repeats, so a run can bracket several moments; the run
    /// ends once the last of them has been saved.
    auto_shot: Vec<(f32, PathBuf)>,
    pending_auto_shot: Vec<(f32, PathBuf)>,
    /// Scheduled --save-state-after captures, earliest deadline first:
    /// write a save state once emulated time reaches each deadline, then
    /// keep running. Repeats like --screenshot-after.
    auto_save_state: Vec<(f32, PathBuf)>,
    pending_auto_save_state: Vec<(f32, PathBuf)>,
    frame_dump: Option<FrameDumpState>,
    pending_frame_dump: Option<FrameDumpSpec>,
    auto_keys: Vec<ScheduledKey>,
    pending_auto_keys: Vec<KeyPressSpec>,
    /// Scheduled mouse-button press/release events from --click-after.
    /// `Press` and `Release` deadlines per requested click.
    auto_clicks: Vec<ScheduledClick>,
    pending_auto_clicks: Vec<(f32, MouseButtonKind, u32, u8)>,
    /// Scheduled joystick/CD32-pad events from --joy-after, plus the
    /// controls currently held per port. An `auto_joy_engaged` entry stays
    /// true once any scripted joy event has fired on that port so the state
    /// keeps overriding the (absent) physical pad, including the final
    /// release.
    auto_joys: Vec<ScheduledJoy>,
    pending_auto_joys: Vec<(f32, JoyButtonKind, u32, u8)>,
    auto_joy_held: [AutoJoyHeld; 2],
    auto_joy_engaged: [bool; 2],
    /// The windowed control-protocol server (`--control-gui`), attached
    /// after construction via [`App::attach_control`]; its commands are
    /// drained at the top of `about_to_wait` (see window/control.rs).
    #[cfg(feature = "control")]
    control: Option<control::ControlState>,
    /// The windowed GDB stub (`--gdb-gui`), when attached.
    #[cfg(feature = "gdb")]
    gdb: Option<gdb::GdbGuiState>,
    /// Rasterization cache for the guest's fn-88 debug overlay.
    guest_overlay_cache: statusbar::GuestOverlayCache,
    /// Scheduled relative port-1 mouse motions from --mouse-after,
    /// one-shot per entry; (at_emulated_secs, dx, dy).
    auto_mouse: Vec<(f64, i32, i32, u8)>,
    pending_auto_mouse: Vec<(f32, i32, i32, u8)>,
    /// `--mouse-to-after` requests waiting for their timestamp, and the
    /// one servo currently steering the pointer. Only one runs at a time:
    /// two servos fighting over the same quadrature counters would each
    /// mis-measure the other's motion as its own.
    auto_mouse_to: Vec<(f64, i32, i32, u8)>,
    pending_auto_mouse_to: Vec<(f32, i32, i32, u8)>,
    active_mouse_to: Option<crate::pointer::PointerServo>,
    /// Scheduled analogue pot positions from --pot-after (one-shot each).
    auto_pots: Vec<(f64, u8, u8, u8)>,
    pending_auto_pots: Vec<(f32, u8, u8, u8)>,
    auto_disk_inserts: Vec<ScheduledDiskInsert>,
    pending_auto_disk_inserts: Vec<DiskInsertSpec>,
    /// Scheduled CD swaps from --insert-cd-after (one-shot each);
    /// (at_emulated_secs, image path).
    auto_cd_inserts: Vec<(f64, PathBuf)>,
    pending_auto_cd_inserts: Vec<(f32, PathBuf)>,
    /// Scheduled freezer-cartridge button presses from --freeze-after
    /// (one-shot each), at emulated seconds.
    auto_freezes: Vec<f64>,
    pending_auto_freezes: Vec<f32>,
    /// Warp launch (`--run`): warp from power-on until the guest OS loads
    /// the target program, then return to real-time pacing. One-shot;
    /// None once finished or cancelled. Host-side only, never serialized.
    warp_launch: Option<crate::runprog::WarpLaunch>,
    /// LoadSeg observer feeding the warp-launch gate. Only meaningful
    /// while `warp_launch` is Some.
    warp_launch_tracker: crate::amigaos::LibraryTracker,
    /// Warp boot (`--warp-boot`/`--warp-until`): warp from power-on until
    /// the storage-idle or timestamp condition holds, then return to
    /// real-time pacing (src/warpboot.rs). One-shot; None once finished
    /// or cancelled. Host-side only, never serialized.
    warp_boot: Option<crate::warpboot::WarpBootGate>,
    /// The warps a control client, a GDB client, or the guest engaged
    /// (`App::set_warp`): each mutes live audio like the boot gates,
    /// outlives a gate that ends under it, and is released per holder
    /// (its own warp-off or disconnect), all at once by the manual toggle
    /// or power off. Host-side only, never serialized.
    warp_holds: app_session::WarpHolds,
    /// Live-input recorder: logs every input event that reaches the
    /// emulated machine and writes a --script-replayable file on stop.
    /// None while not recording.
    input_recorder: Option<crate::inputrec::InputRecorder>,
    /// --record-input destination: when set, the recorder runs for the
    /// whole session and the script is written here on exit (the Drop
    /// impl catches every exit path, including the headless captures).
    record_input_path: Option<PathBuf>,
    modifiers: ModifiersState,
    /// Rawkeys the host keyboard is holding down (both the focused
    /// `KeyboardInput` path and the raw-device qualifier path feed it).
    /// One of the two sources behind [`App::amiga_rawkey_held`]; see
    /// [`KeySource`] for why the machine is told about the aggregate
    /// rather than about either source on its own.
    held_rawkeys: [bool; 128],
    /// Rawkeys the on-screen keyboard is holding down: the cap under the
    /// mouse and every latched qualifier. The other source.
    panel_held_rawkeys: [bool; 128],
    /// Physical state of the qualifier keys as the raw-device listener
    /// sees them, which is not a source of its own: it is what stops a
    /// winit `ModifiersState` update from releasing a qualifier the
    /// hardware still has down (see `update_host_modifiers`).
    raw_device_held_rawkeys: [bool; 128],
    main_window_focused: bool,
    /// Whether the user has sized the main window themselves, in which case
    /// the canvas reflows into it instead of the window snapping to the
    /// canvas. Tracked from the resizes that arrive rather than measured
    /// from the current size so a snap the platform clamped or rounded does
    /// not read as the user's own drag and disable future snaps.
    window_manually_sized: bool,
    /// Deadline for the asynchronous response to the last canvas snap, so a
    /// platform-clamped result is not counted as the user's resize. Bounded
    /// because a window manager may ignore the request entirely.
    snap_request_deadline: Option<Instant>,
    /// A canvas change that could not size the window because it was
    /// fullscreen, waiting for the window to come back.
    pending_canvas_follow: Option<CanvasFollow>,
    cursor_pos: Option<(i32, i32)>,
    last_display_cursor_pos: Option<(i32, i32)>,
    /// Most recent raw host cursor position (physical pixels) from the last
    /// CursorMoved while the pointer is inside the window (cleared on
    /// CursorLeft). The redraw re-maps `cursor_pos` from it whenever the
    /// presentation layout changes under a stationary pointer (autocrop
    /// latch adoption, a menu suspending the crop), and the
    /// COPPERLINE_DIAG_CURSOR click trace logs it alongside the mapped
    /// pixel.
    last_cursor_phys: Option<winit::dpi::PhysicalPosition<f64>>,
    volume_dragging: bool,
    /// A scroll arrow held down: which control, and when its next repeat is
    /// due. A click moves one row and lets go; keeping the button down
    /// starts the list running after a pause, the way a held key does. Any
    /// of the launcher's scrolling lists can be the one being held.
    scroll_hold: Option<(UiControl, Instant)>,
    /// A launcher stepper held down: which control, when its next step is
    /// due, and when the hold began -- the ramping fields read their pace
    /// off how long they have been held.
    cycle_hold: Option<(UiControl, Instant, Instant)>,
    /// Where the keyboard's or the pad's focus stands on the interface,
    /// and whether it is being shown. Empty until something asks for it.
    nav: crate::video::nav::Nav,
    /// When the focus was last shown, for its breath.
    nav_shown_at: Instant,
    /// The button the current page was entered by, so going back leaves
    /// the page the way it was come into rather than closing the whole
    /// launcher. Forgotten once it has been gone back to.
    nav_entered_from: Option<crate::video::nav::NavTarget>,
    /// The pad's walk across the interface, while a surface is open.
    pad_nav: PadNav,
    /// When the text caret next changes between lit and dark. `None` when
    /// nothing is being typed into, which is also what puts it back to lit
    /// for whichever box opens next.
    caret_flip_at: Option<Instant>,
    /// True while the frame analyzer selector is following a held left
    /// mouse button.
    analyzer_dragging: bool,
    mouse_captured: bool,
    /// Set when a menu, overlay panel, or tool window took the mouse away
    /// from a capture that was live, so closing the last of them can hand
    /// it back. The Cmd/Alt+G toggle clears it: the capture is then off
    /// because the operator asked for it, not because the UI borrowed the
    /// cursor. Focus loss deliberately does not clear it -- opening a tool
    /// window unfocuses the main window as a matter of course, and that
    /// must not count as the operator letting the capture go.
    capture_suspended_by_ui: bool,
    mouse_delta_remainder: (f64, f64),
    last_rendered_emulated_frame: Option<u64>,
    last_submitted_render_frame: Option<u64>,
    render_generation: u64,
    last_fdd_track: Option<u8>,
    /// Last status/UI state paired with a requested main-window redraw.
    /// When both this and the exact presentation pixels are unchanged, the
    /// existing GPU texture can be held instead of uploaded and presented at
    /// the emulated field rate.
    last_main_redraw_state: Option<MainRedrawState>,
    /// A newly processed frame changed the active presentation buffer. Kept
    /// separate from the render methods' boolean ("a frame was processed") so
    /// recordings still receive exact duplicate frames.
    main_presentation_dirty: bool,
    /// Transient on-screen overlay message (screenshot saved, disk
    /// swapped), or None when nothing is being shown.
    osd: Option<Osd>,
    /// True while a file drag hovers over the main window; draws the
    /// drop-hint overlay. winit sends no HoveredFileCancelled after a
    /// successful drop, so DroppedFile clears this too.
    drop_hover: bool,
    /// Files from DroppedFile events, coalesced in about_to_wait: winit
    /// delivers one event per file, and a multi-file drop must act once.
    pending_dropped_files: Vec<PathBuf>,
    /// A support archive being fetched by the WHDLoad page.
    #[cfg(feature = "game-library")]
    whdload_job: Option<WhdloadDownload>,
    /// A library scan, while one is running. Held here rather than in the
    /// launcher state for the same reason the other jobs are: it owns a
    /// worker and a channel, neither of which a cloned state could carry.
    #[cfg(feature = "game-library")]
    library_scan: Option<crate::gamelib::Scan>,
    /// A sign-in in flight.
    #[cfg(feature = "game-library")]
    login_job: Option<LoginJob>,
    /// When a status line that reports something finished should go away.
    /// A failure stays until something replaces it; only a success has a
    /// reason to clear itself.
    status_until: Option<std::time::Instant>,
    /// A disk image being written by the launcher's workshop. A large,
    /// fully-allocated image takes long enough that writing it on this
    /// thread would look like a hang, so it runs on a worker and the loop
    /// stays awake to collect it.
    image_job: Option<ImageJob>,
    /// Per-drive disk-swap playlists: the ordered image paths the user can
    /// cycle through for each drive with the disk-swap shortcut. Lets a
    /// multi-disk demo run on a single drive.
    disk_playlists: [Vec<PathBuf>; 4],
    /// Write-protect flag applied to disks swapped in from each playlist.
    disk_write_protected: [bool; 4],
    /// Index of the currently inserted disk within each drive's playlist.
    disk_playlist_index: [usize; 4],
    /// Whether to horizontally recentre a standard (non-overscan) display for
    /// full-overscan presentation. On by default; set COPPERLINE_HCENTER=0 to
    /// disable. TV presentation keeps the framebuffer's fixed source origin.
    hcenter: bool,
    /// Presentation-level overscan handling ([display] overscan): Tv masks
    /// the deep-overscan margins with black like a CRT bezel.
    overscan: Overscan,
    /// Where the TV presentation centres the picture on the glass
    /// ([display] tv_h_centre / tv_v_centre, Video Settings), a monitor's
    /// H-CENTER/V-CENTER controls. Followed by captures, like `overscan`;
    /// the menu steps change the live value without affecting the
    /// configured start-up one.
    tv_centre: crate::config::TvCentre,
    /// Window shader pass in effect ([display] shader). Presentation only:
    /// screenshots, frame dumps and recordings never go through it.
    crt_shader_kind: crate::config::ShaderKind,
    /// Source file behind `ShaderKind::Custom`, kept so the menu can cycle
    /// back to a user shader (and re-read an edited one) after leaving it.
    custom_shader_path: Option<std::path::PathBuf>,
    /// How strongly the shader pass is mixed in, 0.0 to 1.0 ([display]
    /// shader_strength).
    shader_strength: f32,
    /// Which monitor front the bezel pass draws, if any ([display] bezel,
    /// Video Settings). Presentation only, like the shader pass: captures
    /// never include it.
    bezel: BezelStyle,
    /// The style `Cmd/Alt+M` switches back on, so the shortcut is an
    /// on-off for whichever front was chosen rather than a third way of
    /// choosing one. Never [`BezelStyle::None`]: turning the bezel off
    /// leaves this pointing at what was on.
    bezel_last: BezelStyle,
    /// Folder of PNG stickers drawn onto the bezel ([display]
    /// bezel_stickers), kept so a machine switch can re-read it. The
    /// loaded sheet lives in [`Render::sticker_pass`].
    bezel_stickers_path: Option<std::path::PathBuf>,
    /// Performance overlay in effect ([display] perf_overlay, Cmd/Alt+P):
    /// a live emulation-performance readout in the top-right of the
    /// display. Presentation only, like the OSD: captures never include it.
    perf_overlay: bool,
    /// The overlay's formatted lines and sampling baseline.
    perf: PerfOverlay,
    /// Screen tint in effect ([display] tint). Presentation only, applied
    /// to the chipset display region of the window frame: captures and
    /// RTG board scanout are never tinted.
    tint: crate::config::Tint,
    /// Luma-indexed colour table for `tint`; `None` when the tint is off,
    /// which skips the pass entirely.
    tint_lut: Option<Box<[u32; 256]>>,
    /// Open the window fullscreen when it is first created ([display]
    /// full_screen). Applied once in `resumed`; the runtime toggle takes over
    /// after that.
    start_fullscreen: bool,
    /// Host USB gamepad reader (pure-Rust, no SDL2), mapped to the emulated
    /// port-2 digital joystick via a per-pad calibration. A no-op when no
    /// input backend is available (e.g. headless CI) or the pad is not yet
    /// calibrated.
    gamepad: crate::gamepad::GamepadReader,
    /// When the pad's Quit hotkey started being held, if it is down right
    /// now. Cleared by a release before the hold completes.
    gamepad_quit_hold: Option<Instant>,
    /// The tool window last given the keyboard, which is the one in
    /// front and so the one a "close this" means. `None` until one has
    /// been opened.
    tool_window_front: Option<ToolPanelKind>,
    /// Whether the pad has been handed the calibration panel's buttons
    /// and settled on one, so that is done once rather than every pass.
    cal_pad_drives: bool,
    /// When the pad last moved the mouse, and since when its held
    /// direction has been building speed. Both empty while the pad is
    /// not being spent on the mouse.
    pad_mouse_at: Option<Instant>,
    pad_mouse_held: Option<Instant>,
    /// The pad state the last input pump published, for the menu bridge.
    /// `None` while no pad is connected or the calibration panel owns it.
    pad_last: Option<crate::gamepad::PadState>,
    /// What the bridge saw last pass, for edge detection: a held button
    /// must not re-fire every poll.
    pad_prev: crate::gamepad::PadState,
    /// Quit was asked for -- the pad's Quit-hotkey hold completed, or the
    /// menu's Quit row was picked; the next event-loop pass exits.
    quit_requested: bool,
    /// Host source policy for the emulated port-2 joystick/CD32 pad.
    joystick_input_mode: JoystickInputMode,
    /// Host mouse sensitivity, 0-100 ([input] mouse_sensitivity), and the speed
    /// multiplier derived from it. A host-input scale only: it multiplies the
    /// live host mouse delta, never scripted --mouse-after input or the core.
    mouse_sensitivity: u8,
    mouse_sensitivity_factor: f64,
    /// When the host mouse is grabbed ([input] mouse_capture): on a display
    /// click, automatically whenever the window holds the focus, or only on
    /// the Cmd/Alt+G shortcut.
    mouse_capture: crate::config::MouseCapture,
    /// Whether the "press Cmd/Alt+G to release" hint has been shown for an
    /// automatic capture yet. Auto mode grabs on every focus gain, and a
    /// message on each one would be noise; the operator only needs telling
    /// how to get the cursor back the first time.
    auto_capture_hint_shown: bool,
    /// Whether the serial port is bridged to MIDI, so the runtime menu offers
    /// the device items. Fixed for the machine's life.
    serial_is_midi: bool,
    /// Host audio output selection for machines started from this session:
    /// system default, a named device (from `[audio] output_device` /
    /// `--audio-device`), or Disabled (no sound, GUI-only). A session-level
    /// setting: the config-screen launcher rebuilds the machine config from its
    /// own fields, so this is held here rather than read back from that config.
    audio_output: crate::audio::AudioOutput,
    /// The session's `[emulation] realtime_priority` request (config value, before
    /// the env override). Re-fed to `priority::requested` whenever the audio sink
    /// is rebuilt live (device switch, disconnect recovery, post-load install) so
    /// the new stream/callback thread keeps the same scheduling as the first sink.
    realtime_priority: bool,
    /// Parallel-port sampler request for this session (from `[parallel]` /
    /// `--parallel sampler` or the launcher). Re-applied whenever a machine
    /// session starts, and edited live from the runtime menu / gain shortcut.
    sampler: crate::sampler::SamplerRequest,
    /// The live cpal capture stream feeding the attached sampler. The stream is
    /// `!Send`, so it is kept here on the main thread while its `Send` read-port
    /// sits in the bus; `None` when no sampler is attached.
    sampler_stream: Option<cpal::Stream>,
    /// Output frame-skip level for warp/turbo mode: how many emulated frames
    /// are retired per presented frame while warp is engaged. Presentation is
    /// vsync-gated, so this is what decouples warp speed from the host monitor
    /// refresh rate. Adjustable from the Emulator menu and the keyboard.
    warp_speed: WarpSpeed,
    /// Rewind capture settings from `[emulation]`, kept so the Rewind menu
    /// item can re-arm the ring with the configured budget after it is
    /// toggled off. `rewind_armed` tracks the user's intent independently of
    /// `Emulator::time_travel_enabled`, which the debugger also arms.
    rewind_budget_mb: usize,
    rewind_interval_frames: u64,
    rewind_armed: bool,
    /// The MT-32's front panel: what it is showing and what it believes
    /// each value to be. The synth has no panel of its own -- on the
    /// hardware this is firmware -- so it is kept here.
    #[cfg(feature = "mt32")]
    mt32_panel: mt32panel::Mt32Panel,
    /// Coppersynth's panel pointer mechanics: latched buttons, the
    /// momentary flash, the knob's grab. Everything semantic lives in
    /// Coppersynth's own panel inside the device.
    #[cfg(feature = "coppersynth")]
    csynth_panel: csynthpanel::CsynthPanel,
    /// The clock the panel's glass is composed against, and the knob's
    /// last position, kept so the fascia holds it while switched off.
    #[cfg(feature = "coppersynth")]
    csynth_panel_epoch: std::time::Instant,
    #[cfg(feature = "coppersynth")]
    csynth_volume: f32,
    /// What the glass last drew, and when, so the panel redraws only
    /// when its pixels would differ -- and never fast enough to crowd
    /// the audio.
    #[cfg(feature = "coppersynth")]
    csynth_panel_drawn: Option<(Option<crate::csynth::Screen>, bool, bool)>,
    #[cfg(feature = "coppersynth")]
    csynth_panel_redraw_at: std::time::Instant,
    /// The on-screen Amiga keyboard: which cap the mouse is holding, which
    /// qualifiers are latched, and which legends the caps wear. Whether the
    /// strip is up at all is `video::keyboard_panel_shown`, because the
    /// canvas height is derived from it.
    kbd_panel: kbdpanel::KbdPanelState,
    /// Mapped host keys currently held for keyboard joystick emulation.
    keyboard_joy_held: [keymap::HeldKeys; keymap::MAPPING_COUNT],
    /// Host-key to controller-control bindings, loaded from the per-user
    /// `keymap.toml` (defaults when there is none) and editable from the
    /// Input Mapping panel.
    keymap: keymap::KeyMap,
    /// Autofire rate in Hz for the fire button on both ports, 0 = off. A
    /// host input policy, not machine state: it gates a *held* fire button
    /// into a pulse train, so nothing changes unless the user holds fire.
    autofire_hz: u8,
    /// Run-ahead frames for input-latency reduction, 0 = off (see
    /// `[emulation] run_ahead_frames`). A presentation policy: the machine
    /// state at every committed frame boundary is identical with it on or off.
    run_ahead_frames: u8,
    /// Static incompatibility derived from the resolved machine config.
    /// Dynamic media/peripheral checks live on the Bus.
    runahead_machine_block: Option<&'static str>,
    /// Pop-up menu and main-window overlay state. Debugger and frame
    /// analyzer panes live in separate tool-window state so they can be
    /// open at the same time.
    ui: UiState,
    /// A submenu the pointer is resting on, waiting out the dwell before
    /// it opens: `(depth, row, since)`. Hovering elsewhere disarms it.
    menu_hover_arm: Option<(usize, usize, Instant)>,
    /// When the About panel opened, driving its entrance animation, and
    /// when its animation last asked for a frame.
    about_opened_at: Instant,
    about_redraw_at: Instant,
    /// Emulated-machine summary lines for the About window.
    about_machine_lines: Vec<String>,
    /// Raw config of the running (or last-applied) machine, so the "Machine
    /// Configuration..." menu item reopens the launcher showing the current
    /// settings.
    machine_config: RawConfig,
    /// Host pause state before the debugger forced a pause, restored when
    /// the debugger window closes (unless Run was used inside it).
    paused_before_debugger: bool,
    /// Host pause state before the frame analyzer forced a pause, restored
    /// when the analyzer pane closes unless Run was used inside it.
    paused_before_analyzer: bool,
    /// Host pause state before the console forced a pause, restored when
    /// the console closes unless run/pause was used inside it.
    paused_before_console: bool,
    /// The reason for the last interactive breakpoint/watchpoint stop,
    /// shown on the debugger's Break tab until execution resumes.
    last_debug_stop: Option<String>,
    /// A CPU double-fault halt has been reported (reset when the CPU
    /// leaves the halted state, e.g. by reset or a state load).
    reported_double_fault: bool,
    /// The console's running memory hunt (HUNT delta search), if any.
    hunt: Option<console::HuntState>,
    /// Active video+audio capture (shortcut or the menu's Record Video item),
    /// or None when not recording. Frames and the matching mixer audio are
    /// appended on emulated-frame boundaries, so captures stay in sync
    /// even under warp or host stutter.
    recorder: Option<crate::recorder::VideoRecorder>,
    /// Scratch presentation-scaled framebuffer for the recorder (same
    /// vertical resample as screenshots).
    record_fb: Vec<u32>,
    /// Scratch for narrowing a 35 ns-canvas presentation to the recorder's
    /// fixed FB_WIDTH frame.
    record_scratch_fb: Vec<u32>,
}

#[derive(Debug, Clone, Copy)]
struct ScheduledClick {
    press_at_emulated_secs: f64,
    release_at_emulated_secs: f64,
    button: MouseButtonKind,
    /// 0-based controller port the click lands on.
    port: u8,
    pressed: bool,
}

#[derive(Debug, Clone, Copy)]
struct ScheduledKey {
    press_at_emulated_secs: f64,
    release_at_emulated_secs: f64,
    rawkey: u8,
    pressed: bool,
}

#[derive(Debug, Clone, Copy)]
struct ScheduledJoy {
    press_at_emulated_secs: f64,
    release_at_emulated_secs: f64,
    button: JoyButtonKind,
    /// 0-based controller port the control belongs to.
    port: u8,
    pressed: bool,
}

#[derive(Debug, Clone)]
struct ScheduledDiskInsert {
    insert_at_emulated_secs: f64,
    drive_idx: usize,
    path: PathBuf,
    write_protected: bool,
}

#[derive(Debug, Clone)]
struct FrameDumpState {
    start_secs: f32,
    dir: PathBuf,
    count: u32,
    dumped: u32,
    last_saved_emulated_frame: Option<u64>,
}

struct Render {
    window: Arc<Window>,
    pixels: Pixels<'static>,
    texture_scale: usize,
    /// The pass that puts the composited buffer on the surface (see
    /// [`scaler`]): the emulator window's replacement for the `pixels`
    /// built-in scaling renderer, so the displayed integer multiple is
    /// not welded to the texture's supersample factor.
    scaler: scaler::PresentScaler,
    /// Native-resolution RTG display texture, drawn over the UI buffer in
    /// the `pixels` render pass (see [`rtg_texture`]). Present whenever the
    /// window is (its pipeline uses the same GPU device as `pixels`).
    rtg_texture: rtg_texture::RtgTexture,
    /// The optional CRT/scanline pass drawn over the display region in the
    /// same `pixels` render pass (see [`crt_shader`]). Built with the
    /// window, whatever preset is selected: the pass is skipped per frame,
    /// not per session, so the menu can turn it on live.
    crt_shader: crt_shader::CrtShader,
    /// The optional monitor-bezel pass drawn under the CRT pass (see
    /// [`bezel`]). Built with the window whatever the setting, like the
    /// CRT pass, so switching style or turning it off works live; each
    /// style's shader is compiled the first time that style is drawn.
    bezel_shader: bezel::BezelShader,
    /// The sticker decals drawn over the bezel pass (see [`stickers`]).
    /// Built with the window whatever the setting, like the passes above;
    /// it draws nothing until a sheet is loaded into it.
    sticker_pass: stickers::StickerPass,
    /// True while the host window is minimized (Windows delivers a 0x0
    /// Resized). Presenting while minimized deadlocks on Windows: DWM stops
    /// consuming swapchain frames, so once the in-flight buffers fill,
    /// pixels.render() blocks the main thread, the message pump dies, and
    /// the window can never be restored (which is what would unblock the
    /// present). Skip all rendering until a nonzero resize restores it.
    minimized: bool,
    /// The physical surface size `pixels` was last configured with, so a
    /// redraw can tell that the host window has outgrown it (see
    /// `resync_surface_size`).
    surface_size: (u32, u32),
}

impl Render {
    /// Resize the presentation surface, recording the size it was configured
    /// with. Every resize goes through here (the first configure is
    /// `build_pixels_for_window`'s, whose size this struct is built with):
    /// `pixels` reconfigures its swapchain from its own copy of this size and
    /// nothing else can correct it, so the record must never lag behind what
    /// `pixels` holds.
    fn resize_surface(&mut self, size: PhysicalSize<u32>) -> Result<(), pixels::TextureError> {
        let (width, height) = (size.width.max(1), size.height.max(1));
        self.pixels.resize_surface(width, height)?;
        self.surface_size = (width, height);
        Ok(())
    }
}

struct ToolWindow {
    window: Arc<Window>,
    pixels: Pixels<'static>,
    texture_scale: usize,
    cursor_pos: Option<(i32, i32)>,
    /// Same Windows minimized-present deadlock hazard as Render::minimized.
    minimized: bool,
    /// Same configured-surface-size record as Render::surface_size.
    surface_size: (u32, u32),
}

impl ToolWindow {
    /// Tool-window counterpart of `Render::resize_surface`.
    fn resize_surface(&mut self, size: PhysicalSize<u32>) -> Result<(), pixels::TextureError> {
        let (width, height) = (size.width.max(1), size.height.max(1));
        self.pixels.resize_surface(width, height)?;
        self.surface_size = (width, height);
        Ok(())
    }
}

/// Frame-loop repaints of the tool windows (debugger, frame analyzer) are
/// paced to this wall-clock interval (20 Hz). Each repaint costs a full
/// panel raster plus a whole-texture GPU upload on the emulation thread, so
/// repainting at the 50 Hz emulated frame rate can push the loop past its
/// frame budget and underrun the audio ring. Interactive updates (hover,
/// clicks, stepping, debug stops) request immediate redraws and are not
/// paced.
const TOOL_REDRAW_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolPanelKind {
    Debugger,
    FrameAnalyzer,
    Console,
}

impl ToolPanelKind {
    /// Every kind of tool window. The lifecycle passes and, above all,
    /// `request_redraw` iterate this rather than naming windows one by one:
    /// a window left out of the redraw only shows what it drew last, and the
    /// panels that pause the machine have nothing else to repaint them.
    const ALL: [Self; 3] = [Self::Debugger, Self::FrameAnalyzer, Self::Console];
}

struct RenderJob {
    generation: u64,
    input: bitplane::RenderInput,
    h_shift: usize,
    overscan: Overscan,
    /// Deinterlacing and phosphor persistence for this frame. They travel
    /// per job (like `h_shift`/`overscan`) so the worker's deinterlacer
    /// always follows the App's current settings; a value captured at
    /// worker spawn would go stale when the launcher starts a machine
    /// with a different config.
    deinterlace: bool,
    phosphor: f32,
    presentation_fb: Vec<u32>,
}

struct RenderWorkerResult {
    generation: u64,
    emulated_frame: u64,
    timing: VideoRenderFrameTiming,
    /// The worker proved this frame's complete render/presentation inputs
    /// identical to the previous progressive frame. `presentation_fb` is then
    /// merely the unused recycle buffer from the job; the main thread keeps
    /// presenting its current buffer without copying it.
    reused_previous: bool,
    presentation_fb: Vec<u32>,
    present_rows: usize,
    present_width: usize,
    /// The frame's aperture classification; the App resolves it through
    /// its `PresentationLatch` when the result lands, so border-only
    /// frames keep the previous geometry.
    tv_aperture: TvApertureFrame,
    programmable: bool,
    /// Playfield content envelope in presentation-buffer space (x in
    /// buffer columns, y in buffer rows: the woven field of a standard
    /// scan, the sync-anchored glass of a programmable one), for the
    /// autocrop presentation; `None` for a border-only frame.
    content_rect: Option<bitplane::ContentRect>,
    /// The job's frame snapshot, handed back for buffer reuse.
    input: bitplane::RenderInput,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct MainRedrawState {
    status: FrontPanelStatus,
    media: MediaBar,
    powered_on: bool,
    paused: bool,
    joystick_input_mode: JoystickInputMode,
    control_connected: bool,
    recording: bool,
    input_recording: bool,
    warp: bool,
    /// The performance overlay's line revision (0 while hidden), so a
    /// resample repaints an otherwise static frame.
    perf_revision: u64,
    /// The MT-32 panel's face -- its display line and lamp -- folded to a
    /// fingerprint, so a program writing to the LCD repaints an otherwise
    /// static frame. Zero while the panel is hidden.
    #[cfg(feature = "mt32")]
    mt32_face: u64,
}

struct RenderWorker {
    job_tx: Option<SyncSender<RenderJob>>,
    result_rx: Receiver<RenderWorkerResult>,
    handle: Option<JoinHandle<()>>,
}

impl RenderWorker {
    fn new() -> Self {
        let (job_tx, job_rx) = mpsc::sync_channel::<RenderJob>(1);
        let (result_tx, result_rx) = mpsc::channel::<RenderWorkerResult>();
        let handle = std::thread::Builder::new()
            .name("copperline-render".to_string())
            .spawn(move || {
                let mut fb = vec![0u32; MAX_CANVAS_PIXELS];
                let mut deinterlacer = Deinterlacer::new();
                let mut repeated_frame_cache = RepeatedPresentationCache::default();
                let mut last_generation = None;
                while let Ok(job) = job_rx.recv() {
                    // A generation bump marks a presentation discontinuity
                    // (machine swap, reset, state load): nothing from the
                    // previous stream may weave or glow into this frame.
                    if last_generation != Some(job.generation) {
                        deinterlacer.reset_history();
                        repeated_frame_cache.clear();
                        last_generation = Some(job.generation);
                    }
                    let result = render_job_to_presentation(
                        job,
                        &mut fb,
                        &mut deinterlacer,
                        &mut repeated_frame_cache,
                    );
                    if result_tx.send(result).is_err() {
                        break;
                    }
                }
            })
            .expect("spawn render worker");
        Self {
            job_tx: Some(job_tx),
            result_rx,
            handle: Some(handle),
        }
    }

    /// On failure (worker thread gone) the whole job is handed back so the
    /// caller can recycle its presentation buffer and frame snapshot.
    #[allow(clippy::result_large_err)]
    fn send(&self, job: RenderJob) -> std::result::Result<(), RenderJob> {
        match self
            .job_tx
            .as_ref()
            .expect("render worker sender missing")
            .send(job)
        {
            Ok(()) => Ok(()),
            Err(err) => Err(err.0),
        }
    }

    fn try_recv(&self) -> std::result::Result<RenderWorkerResult, TryRecvError> {
        self.result_rx.try_recv()
    }

    fn recv(&self) -> std::result::Result<RenderWorkerResult, mpsc::RecvError> {
        self.result_rx.recv()
    }
}

impl Drop for RenderWorker {
    fn drop(&mut self) {
        self.job_tx.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Which of the two Create Image pages asked for a file, and what it asked
/// for. The choice is made while the launcher state is still borrowed, and
/// acted on after the save dialog has come back.
enum ImageToMake {
    Floppy(crate::diskimage::FloppySpec),
    Hard(crate::diskimage::HardSpec),
}

impl ImageToMake {
    /// Whether the file will be left with holes in it. A floppy is written
    /// whole -- it is under two megabytes -- so it never is.
    fn is_sparse(&self) -> bool {
        match self {
            ImageToMake::Floppy(_) => false,
            ImageToMake::Hard(spec) => spec.sparse,
        }
    }

    /// How much room the finished file will take on the host. For a hard
    /// drive that is the geometry's own size, which a hand-set geometry
    /// decides rather than the size box.
    fn bytes_on_disk(&self) -> u64 {
        match self {
            ImageToMake::Floppy(spec) => crate::diskimage::floppy_bytes(spec),
            ImageToMake::Hard(spec) => spec
                .geometry
                .unwrap_or_else(|| crate::diskimage::Geometry::for_size(spec.bytes))
                .bytes(),
        }
    }
}

/// A WHDLoad support archive being fetched, and the row waiting for it.
#[cfg(feature = "game-library")]
/// The most a metadata field takes.
///
/// Long enough for the longest game title anyone has, short enough that a
/// paste of a whole document does not become the name of a game. The
/// version is shorter still: the page shows it on two lines under the
/// cover, and there is no use in typing what it cannot show.
#[cfg(feature = "game-library")]
const META_FIELD_MAX: usize = 120;

#[cfg(feature = "game-library")]
fn meta_field_max(field: crate::video::launcher::MetaField) -> usize {
    match field {
        crate::video::launcher::MetaField::Version => crate::video::ui::library_version_max(),
        _ => META_FIELD_MAX,
    }
}

/// The first line of the host clipboard, trimmed.
///
/// One line of it: what these boxes hold is a password or a title, not a
/// document, and a newline in the middle of one is a paste that went wrong.
fn clipboard_line() -> String {
    match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
        Ok(text) => text.lines().next().unwrap_or_default().trim().to_string(),
        Err(e) => {
            log::warn!("launcher: clipboard unavailable: {e}");
            String::new()
        }
    }
}

/// How long a status line that reports something finished stays up. Long
/// enough to read, short enough that it is gone before it can be mistaken
/// for the state of things now.
#[cfg(feature = "game-library")]
const STATUS_LINGER: std::time::Duration = std::time::Duration::from_secs(4);

/// How long the text caret stays lit, and then out. Half a second each way
/// is about the rate a host's own text cursor blinks at; slower reads as
/// something still loading, faster as something wrong.
const CARET_BLINK: std::time::Duration = std::time::Duration::from_millis(500);

/// How long the WHDLoad machine-type line stays. Shorter than
/// [`STATUS_LINGER`]: it explains a setting that has just been changed, and
/// is read straight away, rather than reporting the end of something that
/// finished while you were looking elsewhere.
const WHDLOAD_MACHINE_LINGER: std::time::Duration = std::time::Duration::from_secs(3);

/// How long a scroll arrow must be held before it starts repeating. Long
/// enough that clicking to move one row never runs on by accident, short
/// enough that holding it does not feel broken -- the pause a host keyboard
/// takes before a held key repeats.
const SCROLL_HOLD_DELAY: std::time::Duration = std::time::Duration::from_millis(350);
/// The gap between repeats thereafter. Well inside the window
/// [`ScrollRate`](crate::video::launcher::ScrollRate) counts as one
/// continued scroll, so the run builds through its stages while the button
/// is down rather than restarting under it.
const SCROLL_HOLD_EVERY: std::time::Duration = std::time::Duration::from_millis(60);

/// Which of the launcher's scrolling lists an arrow belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollList {
    #[cfg(feature = "game-library")]
    Games,
    #[cfg(feature = "game-library")]
    Favourites,
    HostDisks,
}

/// Whether a control is a scroll arrow, and if so which way it goes (`-1`
/// up, `1` down) and which list it drives.
fn scroll_arrow_of(control: UiControl) -> Option<(isize, ScrollList)> {
    match control {
        #[cfg(feature = "game-library")]
        UiControl::LauncherLibraryScroll(d) => Some((d.signum(), ScrollList::Games)),
        #[cfg(feature = "game-library")]
        UiControl::LauncherLibraryFavouriteScroll(d) => Some((d.signum(), ScrollList::Favourites)),
        UiControl::LauncherHostDiskScroll(d) => Some((d.signum(), ScrollList::HostDisks)),
        _ => None,
    }
}

/// The control that scrolls `list` by `rows`.
fn scroll_arrow_for(list: ScrollList, rows: isize) -> UiControl {
    match list {
        #[cfg(feature = "game-library")]
        ScrollList::Games => UiControl::LauncherLibraryScroll(rows),
        #[cfg(feature = "game-library")]
        ScrollList::Favourites => UiControl::LauncherLibraryFavouriteScroll(rows),
        ScrollList::HostDisks => UiControl::LauncherHostDiskScroll(rows),
    }
}

/// A sign-in in flight: the request runs on a worker so a slow or
/// unreachable service cannot freeze the launcher mid-dialog.
#[cfg(feature = "game-library")]
struct LoginJob {
    rx: std::sync::mpsc::Receiver<
        Result<crate::gamelib::openretro::Session, crate::gamelib::openretro::Error>,
    >,
}

/// A WHDLoad support archive being fetched, and the row waiting for it.
#[cfg(feature = "game-library")]
struct WhdloadDownload {
    rx: std::sync::mpsc::Receiver<Result<PathBuf, crate::gamelib::support::Error>>,
    field: crate::video::launcher::LauncherField,
    archive: crate::gamelib::support::Archive,
}

/// An image being written on a worker thread, and what to call it when it
/// lands.
struct ImageJob {
    rx: std::sync::mpsc::Receiver<std::io::Result<crate::diskimage::Created>>,
    path: PathBuf,
    /// The file's own name, for the line that reports it landing.
    name: String,
}

/// Bytes free on the filesystem a not-yet-created file would land on.
///
/// The file itself does not exist, so the question is asked of the
/// directory holding it -- and of *that* directory, not of Copperline's
/// own: saving onto a second drive is measured against the second drive.
/// `None` when the host will not say, in which case there is nothing to
/// warn about and the write simply goes ahead.
fn free_space_for_new_file(path: &std::path::Path) -> Option<u64> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty())?;
    crate::filesys::host_fs_usage(dir).map(|(_, avail)| avail)
}

impl App {
    pub fn new(
        emu: Emulator,
        power_on: bool,
        screenshot_after: Vec<(f32, PathBuf)>,
        save_state_after: Vec<(f32, PathBuf)>,
        frame_dump: Option<FrameDumpSpec>,
        press_after: Vec<KeyPressSpec>,
        click_after: Vec<(f32, MouseButtonKind, u32, u8)>,
        joy_after: Vec<(f32, JoyButtonKind, u32, u8)>,
        mouse_after: Vec<(f32, i32, i32, u8)>,
        mouse_to_after: Vec<(f32, i32, i32, u8)>,
        pot_after: Vec<(f32, u8, u8, u8)>,
        disk_insert_after: Vec<DiskInsertSpec>,
        cd_insert_after: Vec<(f32, PathBuf)>,
        freeze_after: Vec<f32>,
        record_input: Option<PathBuf>,
        run_warp_target: Option<crate::runprog::WarpLaunch>,
        warp_boot_gate: Option<crate::warpboot::WarpBootGate>,
        disk_playlists: [Vec<PathBuf>; 4],
        disk_write_protected: [bool; 4],
        overscan: Overscan,
        tv_centre: crate::config::TvCentre,
        deinterlace: bool,
        phosphor: f32,
        shader: crate::config::ShaderMode,
        shader_strength: f32,
        bezel: BezelStyle,
        bezel_stickers: Option<PathBuf>,
        perf_overlay: bool,
        tint: crate::config::Tint,
        start_fullscreen: bool,
        hide_status_bar: bool,
        warp_speed: WarpSpeed,
        joystick_input_mode: JoystickInputMode,
        mouse_sensitivity: u8,
        mouse_capture: crate::config::MouseCapture,
        about_machine_lines: Vec<String>,
        machine_config: RawConfig,
        runahead_machine_block: Option<&'static str>,
        // Effective live-audio state for this machine: for a real machine the
        // caller's --audio/--noaudio-resolved value; for the config-screen
        // placeholder the config intent (so a state loaded over it gets sound).
        audio_output_enabled: bool,
        // Parallel-port sampler request (disabled for the config-screen
        // placeholder; run_machine re-derives it from the launcher's config).
        sampler: crate::sampler::SamplerRequest,
    ) -> Self {
        // The status-bar visibility is a process-global read from deep in the
        // presentation code; seed it before the window is built so the initial
        // window size accounts for it.
        super::set_status_bar_hidden(hide_status_bar);
        // Headless capture runs drive themselves off emulated time, so a
        // powered-off start would simply hang. Force power on for those.
        let powered_on = power_on
            || !screenshot_after.is_empty()
            || !save_state_after.is_empty()
            || frame_dump.is_some();
        let render_worker = threaded_render_enabled().then(|| {
            info!("threaded render pipeline enabled");
            RenderWorker::new()
        });
        // MIDI needs a &mut to probe the sink; rebind so the parameter is not
        // needlessly `mut` in a build without the feature.
        #[cfg(feature = "midi")]
        let (serial_is_midi, emu) = {
            let mut emu = emu;
            let is_midi = emu.bus_mut().midi_serial_mut().is_some();
            (is_midi, emu)
        };
        #[cfg(not(feature = "midi"))]
        let serial_is_midi = false;
        // `audio_output_enabled` is the effective state the caller resolved
        // (--audio/--noaudio applied over output_enabled for a real machine, or
        // the config intent for the silent config-screen placeholder), so the
        // menu label matches what is actually running. from_config treats a
        // blank device name as the default.
        let audio_output = crate::audio::AudioOutput::from_config(
            audio_output_enabled,
            machine_config.audio.output_device.as_deref(),
        );
        // Config's realtime-priority request, re-fed to priority::requested when
        // the audio sink is rebuilt live so those streams keep the same setting.
        let realtime_priority = machine_config.emulation.realtime_priority.unwrap_or(false);
        let rewind_budget_mb = machine_config
            .emulation
            .rewind_budget_mb
            .unwrap_or(crate::config::REWIND_DEFAULT_BUDGET_MB)
            .max(1);
        let rewind_interval_frames = machine_config
            .emulation
            .rewind_interval_frames
            .unwrap_or(crate::config::REWIND_DEFAULT_INTERVAL_FRAMES)
            .max(1);
        let rewind_armed = machine_config.emulation.rewind.unwrap_or(false);
        let autofire_hz = machine_config
            .input
            .autofire_hz
            .unwrap_or(0)
            .min(crate::config::AUTOFIRE_MAX_HZ);
        let run_ahead_frames = machine_config
            .emulation
            .run_ahead_frames
            .unwrap_or(0)
            .min(crate::config::RUN_AHEAD_MAX_FRAMES);
        // The canvas rule reads the bezel through its own mirror
        // (`present_height`), and the window is sized from the canvas
        // before the first frame: seed it with the style the window opens
        // with, as `main` seeds the aspect and the scaling.
        crate::video::set_bezel_shown(bezel.is_on());
        let mut app = Self {
            emu,
            serial_is_midi,
            audio_output,
            realtime_priority,
            sampler,
            sampler_stream: None,
            fb: vec![0u32; MAX_CANVAS_PIXELS],
            deinterlacer: Deinterlacer::with_settings(deinterlace, phosphor),
            deinterlace,
            phosphor,
            present_fb: vec![0u32; FB_WIDTH * OUT_HEIGHT],
            present_rows: OUT_HEIGHT,
            present_width: FB_WIDTH,
            rtg_fb: Vec::new(),
            rtg_present_dims: None,
            present_tv_aperture_rows: Some(TV_PAL_PRESENT_HEIGHT),
            presentation_latch: PresentationLatch::default(),
            present_content_rect: None,
            last_diag_layout: None,
            autocrop_latch: AutocropLatch::default(),
            present_programmable: false,
            render: None,
            debugger_tool_window: None,
            frame_analyzer_tool_window: None,
            console_tool_window: None,
            last_tool_redraw: Instant::now(),
            debugger_panel: None,
            frame_analyzer_panel: None,
            console_panel: None,
            analyzer_underlay_fb: std::rc::Rc::new(Vec::new()),
            analyzer_underlay_rows: 0,
            analyzer_underlay_width: FB_WIDTH,
            analyzer_underlay_frame: None,
            analyzer_underlay_input: None,
            heatmap_armed_by_panel: false,
            render_worker,
            render_recycle_fb: Vec::new(),
            render_recycle_input: None,
            cpu_halted: false,
            powered_on,
            run_honors_power_on: false,
            paused: false,
            auto_shot: Vec::new(),
            pending_auto_shot: screenshot_after,
            auto_save_state: Vec::new(),
            pending_auto_save_state: save_state_after,
            frame_dump: None,
            pending_frame_dump: frame_dump,
            auto_keys: Vec::new(),
            pending_auto_keys: press_after,
            auto_clicks: Vec::new(),
            pending_auto_clicks: click_after,
            auto_joys: Vec::new(),
            pending_auto_joys: joy_after,
            auto_joy_held: [AutoJoyHeld::default(); 2],
            auto_joy_engaged: [false; 2],
            #[cfg(feature = "control")]
            control: None,
            #[cfg(feature = "gdb")]
            gdb: None,
            guest_overlay_cache: Default::default(),
            auto_mouse: Vec::new(),
            pending_auto_mouse: mouse_after,
            auto_mouse_to: Vec::new(),
            pending_auto_mouse_to: mouse_to_after,
            active_mouse_to: None,
            auto_pots: Vec::new(),
            pending_auto_pots: pot_after,
            auto_disk_inserts: Vec::new(),
            pending_auto_disk_inserts: disk_insert_after,
            auto_cd_inserts: Vec::new(),
            pending_auto_cd_inserts: cd_insert_after,
            auto_freezes: Vec::new(),
            pending_auto_freezes: freeze_after,
            warp_launch: run_warp_target,
            warp_launch_tracker: crate::amigaos::LibraryTracker::default(),
            warp_boot: warp_boot_gate,
            warp_holds: app_session::WarpHolds::default(),
            input_recorder: record_input
                .is_some()
                .then(|| crate::inputrec::InputRecorder::new(0.0)),
            record_input_path: record_input,
            modifiers: ModifiersState::empty(),
            held_rawkeys: [false; 128],
            panel_held_rawkeys: [false; 128],
            raw_device_held_rawkeys: [false; 128],
            main_window_focused: false,
            window_manually_sized: false,
            snap_request_deadline: None,
            pending_canvas_follow: None,
            cursor_pos: None,
            last_display_cursor_pos: None,
            last_cursor_phys: None,
            volume_dragging: false,
            scroll_hold: None,
            cycle_hold: None,
            nav: crate::video::nav::Nav::default(),
            nav_shown_at: Instant::now(),
            nav_entered_from: None,
            pad_nav: PadNav::default(),
            caret_flip_at: None,
            analyzer_dragging: false,
            mouse_captured: false,
            capture_suspended_by_ui: false,
            mouse_delta_remainder: (0.0, 0.0),
            last_rendered_emulated_frame: None,
            last_submitted_render_frame: None,
            render_generation: 0,
            last_fdd_track: None,
            last_main_redraw_state: None,
            main_presentation_dirty: true,
            osd: None,
            drop_hover: false,
            pending_dropped_files: Vec::new(),
            #[cfg(feature = "game-library")]
            library_scan: None,
            #[cfg(feature = "game-library")]
            login_job: None,
            status_until: None,
            #[cfg(feature = "game-library")]
            whdload_job: None,
            image_job: None,
            disk_playlists,
            disk_write_protected,
            disk_playlist_index: [0; 4],
            hcenter: hcenter_enabled(),
            overscan,
            tv_centre,
            crt_shader_kind: shader.kind(),
            custom_shader_path: match &shader {
                crate::config::ShaderMode::Custom(path) => Some(path.clone()),
                _ => None,
            },
            shader_strength,
            bezel,
            bezel_last: last_bezel_style(bezel),
            bezel_stickers_path: bezel_stickers,
            perf_overlay,
            perf: PerfOverlay::default(),
            tint,
            tint_lut: tint_lut(tint),
            start_fullscreen,
            gamepad: crate::gamepad::GamepadReader::new(),
            gamepad_quit_hold: None,
            tool_window_front: None,
            cal_pad_drives: false,
            pad_mouse_at: None,
            pad_mouse_held: None,
            pad_last: None,
            pad_prev: crate::gamepad::PadState::default(),
            quit_requested: false,
            joystick_input_mode,
            mouse_sensitivity,
            mouse_sensitivity_factor: mouse_sensitivity_factor(mouse_sensitivity),
            mouse_capture,
            auto_capture_hint_shown: false,
            warp_speed,
            rewind_budget_mb,
            rewind_interval_frames,
            rewind_armed,
            #[cfg(feature = "mt32")]
            mt32_panel: mt32panel::Mt32Panel::default(),
            #[cfg(feature = "coppersynth")]
            csynth_panel: csynthpanel::CsynthPanel::default(),
            #[cfg(feature = "coppersynth")]
            csynth_panel_epoch: std::time::Instant::now(),
            #[cfg(feature = "coppersynth")]
            csynth_volume: 1.0,
            #[cfg(feature = "coppersynth")]
            csynth_panel_drawn: None,
            #[cfg(feature = "coppersynth")]
            csynth_panel_redraw_at: std::time::Instant::now(),
            kbd_panel: kbdpanel::KbdPanelState::default(),
            keyboard_joy_held: [keymap::HeldKeys::default(); keymap::MAPPING_COUNT],
            keymap: keymap::KeyMap::load(),
            autofire_hz,
            netplay: None,
            netplay_input: Default::default(),
            netplay_keyboard_controller: true,
            netplay_setup: None,
            host_clipboard: None,
            run_ahead_frames,
            runahead_machine_block,
            ui: UiState::default(),
            menu_hover_arm: None,
            about_opened_at: Instant::now(),
            about_redraw_at: Instant::now(),
            about_machine_lines,
            machine_config,
            paused_before_debugger: false,
            paused_before_analyzer: false,
            paused_before_console: false,
            last_debug_stop: None,
            reported_double_fault: false,
            hunt: None,
            recorder: None,
            record_fb: Vec::new(),
            record_scratch_fb: Vec::new(),
        };
        // Attach the sampler now for a directly-booted machine; the config-screen
        // placeholder passes a disabled request and attaches on Run instead.
        app.attach_session_sampler();
        if app.rewind_armed {
            app.arm_rewind_ring();
        }
        if let (true, Some(reason)) = (
            app.run_ahead_frames > 0 && app.powered_on,
            app.runahead_block_reason(),
        ) {
            warn!(
                "run-ahead ({} frames) configured but inactive: {reason}",
                app.run_ahead_frames
            );
        }
        app
    }

    /// Start recording rewind history with the configured budget/interval.
    /// The debugger arms the same ring on its own terms; this re-arms it so
    /// the rewind hotkey gets the interval the user asked for.
    fn arm_rewind_ring(&mut self) {
        self.emu
            .enable_time_travel(self.rewind_budget_mb, self.rewind_interval_frames);
        // Take the first anchor now rather than at the end of the next frame,
        // so rewind can reach back to the moment recording was switched on.
        // Callers are always between frames (App::new before the first one,
        // the menu/hotkey handlers between two), which is where the renderer's
        // capture buffers are consistent enough to serialize.
        if let Err(e) = self.emu.debug_ensure_time_travel_anchor() {
            warn!("rewind: could not take the initial snapshot: {e:#}");
        }
        info!(
            "rewind: recording history ({} MiB budget, one snapshot every {} frames)",
            self.rewind_budget_mb, self.rewind_interval_frames
        );
    }

    /// Menu/hotkey toggle for rewind capture. Turning it off releases the
    /// retained snapshots, which is the point: the ring is the memory cost.
    fn toggle_rewind(&mut self) {
        self.rewind_armed = !self.rewind_armed;
        if self.rewind_armed {
            self.arm_rewind_ring();
            self.show_osd("Rewind recording on");
        } else {
            // Leave the ring alone if the debugger armed it for its own
            // reverse controls; only the user's rewind recording stops.
            if !self.debugger_wants_time_travel() {
                self.emu.disable_time_travel();
            }
            self.show_osd("Rewind recording off");
        }
    }

    /// Whether a debugger-family window is open and relying on the snapshot
    /// ring for its reverse controls.
    fn debugger_wants_time_travel(&self) -> bool {
        self.debugger_panel.is_some() || self.console_panel.is_some()
    }

    /// Rewind the machine one capture point. Unlike the debugger's reverse
    /// controls this leaves the run state alone: rewinding a running machine
    /// keeps it running, from the earlier point.
    fn rewind_one_step(&mut self) {
        use crate::timetravel::ReverseOutcome;
        if !self.emu.time_travel_enabled() {
            self.show_osd("Rewind is off (Emulator menu > Rewind)");
            return;
        }
        match self.emu.tt_rewind_step() {
            Ok(ReverseOutcome::Found(_)) => {
                let secs = self.emu.rewind_history_seconds().unwrap_or(0.0);
                self.show_osd(format!("Rewind ({secs:.0}s of history left)"));
            }
            Ok(ReverseOutcome::NotFound | ReverseOutcome::BeyondHistory) => {
                self.show_osd("Rewind: start of recorded history")
            }
            Err(e) => {
                error!("rewind step halted: {e:?}");
                self.show_osd("Rewind failed (see log)");
            }
        }
        // The restore rewrote the whole machine, including the renderer's
        // capture buffers; repaint from the restored frame rather than
        // leaving the pre-rewind image on screen.
        self.finish_render_for_current_frame();
    }

    /// Build the parallel-port sampler for the current [`self.sampler`] request
    /// and attach it to the live machine, replacing any previous one. The cpal
    /// capture stream is kept here on the main thread (it is `!Send`); its `Send`
    /// read-port goes into the bus. A disabled request detaches the port. A
    /// device-open failure logs and leaves the port empty rather than aborting.
    fn attach_session_sampler(&mut self) {
        // Drop any prior stream first so the old capture device is released
        // before a new one opens.
        self.sampler_stream = None;
        if !self.sampler.enabled {
            return;
        }
        match crate::sampler::CpalSampler::open(
            self.sampler.input_device.as_deref(),
            self.sampler.gain_db,
        ) {
            Ok((stream, port)) => {
                info!(
                    "parallel: sampler attached (input {:?})",
                    port.device_label()
                );
                self.emu.bus_mut().attach_parallel_port(Box::new(port));
                self.sampler_stream = Some(stream);
            }
            Err(e) => warn!("parallel: sampler failed to attach: {e}"),
        }
    }

    /// The port live host-mouse input drives: the lowest-numbered port with
    /// a mouse plugged in. With no mouse on either port, live mouse input is
    /// dropped.
    fn mouse_port(&self) -> Option<usize> {
        if let Some(peer) = &self.netplay {
            let port = peer.player();
            return self.emu.bus().input.ports[port]
                .device
                .is_mouse()
                .then_some(port);
        }
        self.emu
            .bus()
            .input
            .ports
            .iter()
            .position(|p| p.device.is_mouse())
    }

    /// Which port each host input source drives this quantum. The host
    /// mouse claims the lowest-numbered mouse port; the ports left over
    /// that a host source can drive (joysticks, CD32 pads, and a second
    /// mouse) are assigned by count:
    ///
    /// - One port: the [`JoystickInputMode`] picks its source. `Gamepad`
    ///   leaves the keyboard passing through to the Amiga -- and cannot
    ///   drive a second mouse, which is then undriven until the mode is
    ///   flipped to `Keyboard`.
    /// - Two ports (a two-controller setup): the gamepad -- backed by the
    ///   numpad keyboard mapping whenever no physical pad is present --
    ///   and the cursor-key mapping drive one each, the mode picking
    ///   which source pair gets the lower-numbered port.
    ///
    /// The cursor-key mapping drives whatever device its port carries:
    /// direction lines on a joystick/pad, pointer motion and buttons on a
    /// mouse.
    fn host_routing(&self) -> HostRouting {
        let input = &self.emu.bus().input;
        host_routing_for(
            [input.ports[0].device, input.ports[1].device],
            self.joystick_input_mode,
        )
    }

    /// Poll the host input sources and drive the emulated port(s). Called
    /// once per scheduler quantum. Scripted --joy-after state beats the
    /// keyboard mapping on a shared port and asserts alone on ports no
    /// host source drives; a present physical pad beats the scripted
    /// state on its port, as it always has.
    fn pump_joystick_input(&mut self) {
        if self.netplay.is_some() {
            self.pump_netplay_input();
            return;
        }
        let r = self.host_routing();
        // Poll the pad whether or not it drives a port: the Quit hotkey
        // and Menu button are host controls and work regardless of routing.
        let pad = self.gamepad.poll();
        self.track_gamepad_quit_hold(pad.is_some_and(|state| state.quit));
        // Published for the menu bridge, which runs at the about_to_wait
        // boundary where the event loop is at hand.
        self.pad_last = pad;
        // While the menu or an overlay panel is up, the pad walks the UI
        // rather than the game -- the same arbitration the keyboard gets
        // from the modal overlay -- so the port lines are released rather
        // than held mid-move.
        let ui_owns_pad = self.modal_ui_active();
        if let Some(port) = r.gamepad {
            match pad {
                Some(state) if !ui_owns_pad => self.apply_joystick_state(port, state.joystick),
                Some(_) => self.release_joystick_lines(port),
                // No physical pad but --joy-after scripting has fired: keep
                // asserting the scripted state so it survives this release
                // path and drives the upcoming scheduler quantum.
                None if self.auto_joy_engaged[port] => self.apply_auto_joy_state(port),
                // No pad in a two-controller setup: the numpad keyboard
                // mapping stands in for it.
                None if r.keyboard2 == Some(port) => {
                    self.apply_joystick_state(port, self.keyboard_joystick_state(1))
                }
                // Pad gone/uncalibrated: release the port so nothing sticks.
                None => self.release_joystick_lines(port),
            }
        }
        // In Gamepad Mouse mode the pad is spent on the mouse port
        // instead of a joystick one: the routing has already taken it
        // off the joysticks, so this is the only place it is heard.
        if let Some(port) = r.gamepad_mouse {
            match pad {
                Some(state) if !ui_owns_pad => self.apply_pad_mouse_state(port, state),
                // The UI has it, or it has gone: let go of the buttons
                // rather than leaving one down over the guest.
                _ => self.release_pad_mouse(port),
            }
        }
        if let Some(port) = r.keyboard {
            if self.emu.bus().input.device(port).is_mouse() {
                self.apply_keyboard_mouse_state(port);
            } else if self.auto_joy_engaged[port] {
                self.apply_auto_joy_state(port);
            } else {
                self.apply_joystick_state(port, self.keyboard_joystick_state(0));
            }
        }
        // Scripted joy state on ports no host source drives asserts
        // independently.
        for port in 0..2 {
            if Some(port) != r.gamepad && Some(port) != r.keyboard && self.auto_joy_engaged[port] {
                self.apply_auto_joy_state(port);
            }
        }
    }

    /// Whether the pad has been handed the calibration panel's buttons,
    /// and the pad state to walk them with.
    ///
    /// `None` while a control is still being captured or tested, which is
    /// what keeps a press meaning "test this" until a hold says otherwise.
    fn calibration_pad_drives(&mut self) -> Option<crate::gamepad::PadState> {
        let Some(Panel::Calibration(session)) = self.ui.panel.as_ref() else {
            self.cal_pad_drives = false;
            return None;
        };
        if !session.handed_over() {
            return None;
        }
        let live = session.live_pad();
        if !self.cal_pad_drives {
            self.cal_pad_drives = true;
            // Whatever is held right now is what completed the hold, not
            // a press meant for the buttons: both the walk's own edges
            // and the Menu button's start from here, so nothing already
            // down acts on arrival. Cancel is where it starts -- the
            // safe one of the two, and the near one.
            self.pad_prev = live;
            self.seed_pad_nav(live.joystick);
            self.nav_show(Some(crate::video::nav::NavTarget::Ui(UiControl::CalCancel)));
        }
        Some(live)
    }

    /// Let the pad walk the interface, at the about_to_wait boundary
    /// where the event loop is at hand.
    ///
    /// The Menu control (a calibrated binding, or Select/guide on the
    /// database's standard layout) is the way in: it opens the menu, or
    /// closes an open overlay panel. From there the pad walks whatever
    /// is up -- the menu, the status bar, the machine configuration and
    /// the panels beyond it -- because its directions, its fire and its
    /// second button mean to the focus exactly what the arrow keys,
    /// Return and Escape mean. The press is edged against the previous
    /// pass, so a held button acts once; the walk keeps its own
    /// hold-to-repeat.
    fn drive_interface_with_pad(&mut self, event_loop: &ActiveEventLoop) {
        let pad = self.pad_last.unwrap_or_default();
        let prev = std::mem::replace(&mut self.pad_prev, pad);
        // Not while a calibration is up: the pad is there to work that
        // panel, and a Menu binding just captured would put it away
        // before it could be tested.
        let calibrating = matches!(self.ui.panel, Some(Panel::Calibration(_)));
        if pad.menu && !prev.menu && !calibrating {
            self.toggle_pad_interface();
            return;
        }
        // The tool windows are deliberately not walked -- a debugger is
        // not a thing to step around with a d-pad -- but a pad must be
        // able to put one away again, since opening one from the menu is
        // something a pad can do.
        if let Some(kind) = self.topmost_tool_panel() {
            if pad.joystick.button2 && !prev.joystick.button2 {
                self.close_tool_panel(kind);
                self.request_redraw();
                return;
            }
        }
        // Everything a surface offers is walked the same way, whichever
        // hand is doing the walking: the pad's directions, its fire and
        // its second button are the arrow keys, Return and Escape, and
        // the focus knows what each means where it is standing.
        // Any surface being up is enough: the pad does not have to be
        // told twice that it is driving the interface, and needing a
        // keyboard arrow first to wake it is the wrong way round for a
        // machine being played from a sofa.
        if self.nav.showing() || self.modal_ui_active() {
            self.pad_drives_interface(pad.joystick, Some(event_loop));
        }
    }

    /// Track the pad's Quit hotkey across polls: a hold of
    /// [`GAMEPAD_QUIT_HOLD`] requests an application exit, with an OSD
    /// countdown while it is in progress. Releasing earlier cancels the
    /// hold and withdraws the countdown. On the standard layout the hotkey
    /// is Select, which also opens the menu on its press edge: the menu
    /// is up while the countdown runs, and an early release leaves it
    /// there, which is what a tap would have done anyway.
    fn track_gamepad_quit_hold(&mut self, held: bool) {
        if !held {
            if self.gamepad_quit_hold.take().is_some()
                && self
                    .osd
                    .as_ref()
                    .is_some_and(|osd| osd.text.starts_with(GAMEPAD_QUIT_OSD_PREFIX))
            {
                self.osd = None;
                self.request_redraw();
            }
            return;
        }
        let start = *self.gamepad_quit_hold.get_or_insert_with(Instant::now);
        // A hold of exactly GAMEPAD_QUIT_HOLD counts as complete: the
        // previous checked_sub formulation required *strictly more* than
        // the hold, so equality showed a "0.0s" countdown instead of
        // quitting -- observable in practice on Windows, whose clock
        // granularity lets two Instant::now readings land on the same
        // tick (the unit test's backdated-by-exactly-the-hold start hit
        // it as an intermittent CI failure).
        let elapsed = start.elapsed();
        if elapsed >= GAMEPAD_QUIT_HOLD {
            self.quit_requested = true;
        } else {
            self.show_osd(format!(
                "{GAMEPAD_QUIT_OSD_PREFIX}{:.1}s",
                (GAMEPAD_QUIT_HOLD - elapsed).as_secs_f32()
            ));
        }
    }

    /// Whether a keyboard mapping owns its keys right now: mapping 0
    /// (cursor keys) when a port routes to the keyboard source, mapping 1
    /// (numpad) when it is the gamepad port's stand-in.
    fn keyboard_mapping_active(&self, mapping: usize) -> bool {
        let r = self.host_routing();
        if mapping == 0 {
            r.keyboard.is_some()
        } else {
            r.keyboard2.is_some()
        }
    }

    /// Drive a mouse port from the cursor-key mapping: held direction
    /// keys become steady pointer motion, the fire keys the left button,
    /// X the right, D the middle.
    fn apply_keyboard_mouse_state(&mut self, port: usize) {
        let state = self.keyboard_joystick_state(0);
        let dx =
            KEYBOARD_MOUSE_COUNTS_PER_QUANTUM * (i32::from(state.right) - i32::from(state.left));
        let dy = KEYBOARD_MOUSE_COUNTS_PER_QUANTUM * (i32::from(state.down) - i32::from(state.up));
        if dx != 0 || dy != 0 {
            self.apply_scripted_mouse_delta(port as u8, dx, dy);
        }
        let input = &mut self.emu.bus_mut().input;
        input.set_mouse_button(port, 0, state.fire);
        input.set_mouse_button(port, 1, state.button2);
        input.set_mouse_button(port, 2, state.green);
    }

    /// The controller state keyboard mapping `index` is producing right now.
    fn keyboard_joystick_state(&self, index: usize) -> crate::gamepad::JoystickState {
        self.keymap
            .mapping(index)
            .joystick_state(&self.keyboard_joy_held[index])
    }

    fn apply_joystick_state(&mut self, port: usize, mut state: crate::gamepad::JoystickState) {
        // Autofire gates a *held* fire button into a pulse train. It is a host
        // input convenience applied before the port sees anything, so the
        // emulated machine reads ordinary presses and releases on /FIRx --
        // nothing downstream knows autofire exists. Scripted --joy-after input
        // deliberately bypasses this (see apply_auto_joy_state): a recorded or
        // scripted run must replay the events it was given, verbatim.
        if state.fire
            && !crate::config::autofire_asserted(
                self.autofire_hz,
                self.emu.bus().emulated_seconds(),
            )
        {
            state.fire = false;
        }
        let input = &mut self.emu.bus_mut().input;
        input.set_joystick(
            port,
            state.up,
            state.down,
            state.left,
            state.right,
            state.fire,
            state.button2,
        );
        input.set_cd32_buttons(
            port,
            state.play,
            state.rwd,
            state.ffw,
            state.green,
            state.yellow,
        );
    }

    /// Release every control on a joystick port. A no-op unless a
    /// joystick/pad is engaged there, so a mouse sharing the line fields is
    /// never clobbered.
    fn release_joystick_lines(&mut self, port: usize) {
        let input = &mut self.emu.bus_mut().input;
        if matches!(
            input.device(port),
            PortDevice::Joystick | PortDevice::Cd32Pad
        ) {
            input.set_joystick(port, false, false, false, false, false, false);
            input.set_cd32_buttons(port, false, false, false, false, false);
        }
    }

    /// Hot-plug a controller device into a port, as if swapping the
    /// physical plug: the old device's lines release, the quadrature
    /// counters hold, and any stale scripted --joy-after ownership of the
    /// port is dropped so it cannot re-engage the old device kind on the
    /// next quantum. Not journaled for reverse replay -- like a media
    /// change, the plugged device is host state.
    fn hot_plug_port_device(&mut self, port: usize, device: PortDevice) {
        self.auto_joy_engaged[port] = false;
        self.auto_joy_held[port] = AutoJoyHeld::default();
        self.emu.bus_mut().input.set_port_device(port, device);
    }

    fn cycle_joystick_input_mode(&mut self) {
        self.set_joystick_input_mode(self.joystick_input_mode.next());
    }

    /// Current MIDI input/output device names for the runtime menu (empty when
    /// the serial port is not in MIDI mode).
    #[cfg(feature = "midi")]
    fn midi_menu_labels(&mut self) -> (String, String) {
        match self.emu.bus_mut().midi_serial_mut() {
            Some(sink) => (sink.input_label(), sink.output_label()),
            None => (String::new(), String::new()),
        }
    }

    #[cfg(not(feature = "midi"))]
    fn midi_menu_labels(&mut self) -> (String, String) {
        (String::new(), String::new())
    }

    /// Step the live audio output through "Default", the host devices, then
    /// "Disabled", rebuilding the sink so the change takes effect at once.
    /// "Disabled" swaps in a null sink -- live audio off, exactly like
    /// `--noaudio`. Freshly re-reads the device list so a just-connected device
    /// appears.
    fn cycle_audio_output(&mut self) {
        let devices = crate::audio::picker_output_devices();
        self.audio_output = self.audio_output.cycle(&devices, true);
        let realtime = crate::priority::requested(self.realtime_priority);
        match crate::audio::open_output_sink(realtime, &self.audio_output) {
            Ok(sink) => {
                self.emu.bus_mut().paula.audio.set_master(sink);
                self.sync_live_audio_suspension();
            }
            Err(e) => {
                warn!("audio: could not open the selected device; keeping silence: {e:#}");
                self.emu
                    .bus_mut()
                    .paula
                    .audio
                    .set_master(Box::new(crate::audio::NullSink));
            }
        }
        self.show_osd(format!("Audio output: {}", self.audio_output.label()));
    }

    /// Cycle Paula's analogue filter override: Auto (guest-driven) -> On -> Off.
    /// Applies live and updates the PWR LED brightness on the next redraw.
    fn cycle_audio_filter(&mut self) {
        use crate::config::AudioFilterMode;
        let next = match self.emu.bus().paula.led_filter_mode() {
            AudioFilterMode::Auto => AudioFilterMode::On,
            AudioFilterMode::On => AudioFilterMode::Off,
            AudioFilterMode::Off => AudioFilterMode::Auto,
        };
        self.emu.bus_mut().paula.set_led_filter_mode(next);
        let label = match next {
            AudioFilterMode::Auto => "Auto",
            AudioFilterMode::On => "Enabled",
            AudioFilterMode::Off => "Disabled",
        };
        self.show_osd(format!("Audio filter: {label}"));
        self.request_redraw();
    }

    /// Raise (`forward`) or lower the live sampler input gain by one
    /// [`SAMPLER_GAIN_STEP_DB`] step, clamped to the sampler's dB range,
    /// rebuilding the capture so the new preamp level takes effect at once. A
    /// no-op when no sampler is attached. Bound to the runtime menu and the
    /// gain shortcut.
    fn step_sampler_gain(&mut self, forward: bool) {
        if !self.sampler.enabled {
            return;
        }
        let delta = if forward {
            SAMPLER_GAIN_STEP_DB
        } else {
            -SAMPLER_GAIN_STEP_DB
        };
        let gain_db = (self.sampler.gain_db + delta).clamp(
            crate::sampler::MIN_SAMPLER_GAIN_DB,
            crate::sampler::MAX_SAMPLER_GAIN_DB,
        );
        if gain_db == self.sampler.gain_db {
            return;
        }
        self.sampler.gain_db = gain_db;
        self.attach_session_sampler();
        self.show_osd(format!("Sampler gain: {}", sampler_gain_osd(gain_db)));
    }

    fn set_joystick_input_mode(&mut self, mode: JoystickInputMode) {
        if self.joystick_input_mode == mode {
            return;
        }
        self.joystick_input_mode = mode;
        if !matches!(self.ui.panel, Some(Panel::Calibration(_))) {
            self.pump_joystick_input();
        }
        info!("joystick input mode: {}", mode.label());
        let routing = self.host_routing();
        let osd = match (mode, routing.keyboard, routing.gamepad) {
            (JoystickInputMode::Keyboard, Some(port), _) => {
                format!("Joystick input: keyboard (port {})", port + 1)
            }
            (JoystickInputMode::Gamepad, _, Some(port)) => {
                format!("Joystick input: gamepad (port {})", port + 1)
            }
            _ => format!("Joystick input: {}", mode.label()),
        };
        self.show_osd(osd);
    }

    /// Open the Input Mapping panel on a working copy of the live map, so
    /// closing without saving changes nothing.
    fn open_input_mapping(&mut self) {
        self.ui.panel = Some(Panel::InputMap(Box::new(ui::InputMapPanel::new(
            self.keymap.clone(),
        ))));
        self.request_redraw();
    }

    fn input_map_panel_mut(&mut self) -> Option<&mut ui::InputMapPanel> {
        match self.ui.panel.as_mut() {
            Some(Panel::InputMap(panel)) => Some(panel),
            _ => None,
        }
    }

    fn input_map_select_mapping(&mut self, set: usize) {
        if let Some(panel) = self.input_map_panel_mut() {
            panel.mapping = set.min(keymap::MAPPING_COUNT - 1);
            // An armed row belongs to the mapping it was armed on.
            panel.capturing = None;
            panel.message = "Click Set, then press the key to bind.".to_string();
        }
        self.request_redraw();
    }

    fn input_map_arm_capture(&mut self, index: usize) {
        if let Some(panel) = self.input_map_panel_mut() {
            let Some(control) = keymap::CONTROLS.get(index).copied() else {
                return;
            };
            panel.capturing = Some(control);
            panel.message = format!("Press a key for {}... (Esc cancels)", control.label());
        }
        self.request_redraw();
    }

    fn input_map_clear(&mut self, index: usize) {
        if let Some(panel) = self.input_map_panel_mut() {
            let Some(control) = keymap::CONTROLS.get(index).copied() else {
                return;
            };
            let mapping = panel.mapping;
            panel.map.mapping_mut(mapping).clear(control);
            panel.capturing = None;
            panel.message = format!("{} unbound.", control.label());
        }
        self.request_redraw();
    }

    fn input_map_defaults(&mut self) {
        if let Some(panel) = self.input_map_panel_mut() {
            panel.map = keymap::KeyMap::default();
            panel.capturing = None;
            panel.message = "Restored the built-in bindings (not saved yet).".to_string();
        }
        self.request_redraw();
    }

    /// Commit the edited map: apply it to the live session and persist it.
    /// Any keys held under the old map are released first, so a binding that
    /// just moved cannot leave a controller line stuck asserted.
    fn input_map_save(&mut self) {
        let Some(map) = self.input_map_panel_mut().map(|panel| panel.map.clone()) else {
            return;
        };
        self.keyboard_joy_held = [keymap::HeldKeys::default(); keymap::MAPPING_COUNT];
        self.keymap = map;
        match self.keymap.save() {
            Ok(()) => self.show_osd("Input mapping saved"),
            Err(e) => {
                warn!("saving the keyboard map failed: {e:#}");
                self.show_osd("Input mapping applied (not saved; see log)");
            }
        }
        self.pump_joystick_input();
        self.close_panel();
    }

    /// Feed a key press to an armed Input Mapping row. Returns true when the
    /// panel consumed it, which also keeps the key out of the emulated
    /// machine while the panel is open.
    fn input_map_handle_key(&mut self, code: KeyCode) -> bool {
        let armed = matches!(
            self.ui.panel.as_ref(),
            Some(Panel::InputMap(panel)) if panel.capturing.is_some()
        );
        if !armed {
            return false;
        }
        if code == KeyCode::Escape {
            if let Some(panel) = self.input_map_panel_mut() {
                panel.capturing = None;
                panel.message = "Binding cancelled.".to_string();
            }
            self.request_redraw();
            return true;
        }
        if let Some(panel) = self.input_map_panel_mut() {
            panel.capture_key(code);
        }
        self.request_redraw();
        true
    }

    /// Consume a mapped host key as joystick input when keyboard joystick
    /// emulation is active. Releases for previously consumed mapped keys
    /// are also swallowed, even if a gamepad has taken over meanwhile.
    fn handle_keyboard_joystick_key(&mut self, code: KeyCode, pressed: bool) -> bool {
        let Some((mapping, _control)) = self.keymap.lookup(code) else {
            return false;
        };
        let active = self.keyboard_mapping_active(mapping);
        let was_held = self.keyboard_joy_held[mapping].is_set(code);
        if !active && !was_held {
            return false;
        }
        self.keyboard_joy_held[mapping].set(code, pressed);
        if active {
            // Re-run the input pump so the transition lands this quantum
            // on whatever port and device the mapping drives.
            self.pump_joystick_input();
        }
        true
    }

    /// Drive a port's emulated joystick/CD32 pad from the --joy-after
    /// held-control set.
    fn apply_auto_joy_state(&mut self, port: usize) {
        let held = self.auto_joy_held[port];
        if let Some(session) = &self.netplay {
            if port == session.player() {
                self.netplay_input.buttons = [
                    held.up,
                    held.down,
                    held.left,
                    held.right,
                    held.red,
                    held.blue,
                    held.play,
                    held.rwd,
                    held.ffw,
                    held.green,
                    held.yellow,
                ]
                .into_iter()
                .enumerate()
                .fold(0, |bits, (bit, on)| bits | (u16::from(on) << bit));
            }
            return;
        }
        let input = &mut self.emu.bus_mut().input;
        input.set_joystick(
            port, held.up, held.down, held.left, held.right, held.red, held.blue,
        );
        input.set_cd32_buttons(port, held.play, held.rwd, held.ffw, held.green, held.yellow);
        // Reverse-debug: note the held state so replay can reproduce it.
        self.emu
            .tt_note_input(crate::inputsched::ReplayAction::Joy {
                port: port as u8,
                state: crate::inputsched::JoyState {
                    up: held.up,
                    down: held.down,
                    left: held.left,
                    right: held.right,
                    red: held.red,
                    blue: held.blue,
                    play: held.play,
                    rwd: held.rwd,
                    ffw: held.ffw,
                    green: held.green,
                    yellow: held.yellow,
                },
            });
    }

    pub fn run(self) -> Result<()> {
        let event_loop = EventLoop::new().map_err(|e| anyhow!("EventLoop::new: {e}"))?;
        event_loop.set_control_flow(ControlFlow::Poll);
        let mut app = self;
        // Start the control server's socket threads with a wake that
        // kicks the loop out of ControlFlow::Wait, so a command arriving
        // while the machine is paused is serviced promptly.
        #[cfg(feature = "control")]
        if let Some(ctl) = app.control.as_mut() {
            let proxy = event_loop.create_proxy();
            ctl.handle.start(Box::new(move || {
                let _ = proxy.send_event(());
            }));
        }
        // Same for the windowed GDB stub's socket threads.
        #[cfg(feature = "gdb")]
        if let Some(gdb) = app.gdb.as_mut() {
            let proxy = event_loop.create_proxy();
            gdb.handle.start(Box::new(move || {
                let _ = proxy.send_event(());
            }));
        }
        event_loop
            .run_app(&mut app)
            .map_err(|e| anyhow!("event loop: {e}"))?;
        Ok(())
    }
}

/// The event-loop-free half of the session driver: arming and firing the
/// scheduled capture/input flags is shared verbatim between the windowed
/// event loop (about_to_wait) and the windowless capture loop below, so a
/// capture run produces byte-identical output either way.
impl App {
    /// Drive a scheduled capture run (--screenshot-after / --dump-frames)
    /// to completion without a host window or event loop. Scheduled input
    /// and captures fire on emulated time exactly as in the windowed loop,
    /// and the run ends when the first capture completes, matching the
    /// windowed exit. Never touching winit means no display-server
    /// connection is made, so capture runs work over SSH and in sandboxes
    /// without window-server access.
    pub fn run_headless(mut self) -> Result<()> {
        self.arm_scheduled_events();
        if self.auto_shot.is_empty() && self.frame_dump.is_none() {
            return Err(anyhow!(
                "windowless capture run needs --screenshot-after or --dump-frames"
            ));
        }
        loop {
            // The windowed loop parks a halted machine so the user can
            // inspect it; with no window the captures can never fire, so
            // surface the halt as the run's failure.
            let step = if self.netplay.is_some() {
                match self.step_netplay() {
                    Ok(false) => continue,
                    result => result.map(|_| ()),
                }
            } else {
                self.emu.step_frame()
            };
            if let Err(e) = step {
                return Err(anyhow!(
                    "emulator halted before the scheduled captures completed: {e:#}"
                ));
            }
            self.confirm_netplay_capture()?;
            // Audio may be live (--screenshot-after without --noaudio):
            // recover a lost output device exactly as the windowed loop does.
            self.recover_audio_if_device_lost();
            self.render_emulated_frame_if_needed();
            if self.dump_frame_if_due() {
                return Ok(());
            }
            self.fire_scheduled_events();
            self.fire_auto_save_state();
            if self.fire_auto_shot() {
                return Ok(());
            }
        }
    }

    /// Arm every scheduled capture and input flag: pending (parse-time)
    /// entries become live ones gated on emulated time. Scheduled events
    /// are gated on emulated time (like disk inserts and the
    /// auto-screenshot): headless runs are unthrottled, so wall-clock
    /// scheduling would fire at the wrong emulated point or never fire at
    /// all before the run exits.
    fn arm_scheduled_events(&mut self) {
        for (secs, path) in std::mem::take(&mut self.pending_auto_shot) {
            info!(
                "auto-screenshot armed: will save {} after {:.1}s emulated time",
                path.display(),
                secs
            );
            self.auto_shot.push((secs.max(0.0), path));
        }
        // Deadline order, not command-line order, so the run ends on the
        // latest capture however the flags were written. The sort is
        // stable, so captures sharing a deadline keep their given order.
        self.auto_shot.sort_by(|(a, _), (b, _)| a.total_cmp(b));
        for (secs, path) in std::mem::take(&mut self.pending_auto_save_state) {
            info!(
                "auto-save-state armed: will save {} after {:.1}s emulated time",
                path.display(),
                secs
            );
            self.auto_save_state.push((secs.max(0.0), path));
        }
        self.auto_save_state
            .sort_by(|(a, _), (b, _)| a.total_cmp(b));
        if let Some(spec) = self.pending_frame_dump.take() {
            info!(
                "frame dump armed: will save {} frames to {} after {:.1}s emulated time",
                spec.count,
                spec.dir.display(),
                spec.start_secs
            );
            self.frame_dump = Some(FrameDumpState {
                start_secs: spec.start_secs.max(0.0),
                dir: spec.dir,
                count: spec.count,
                dumped: 0,
                last_saved_emulated_frame: None,
            });
        }
        // Scheduled keys/clicks are gated on emulated time (like disk
        // inserts and the auto-screenshot): headless runs are unthrottled,
        // so wall-clock scheduling would fire at the wrong emulated point
        // or never fire at all before the run exits.
        for key in self.pending_auto_keys.drain(..) {
            let press_at = key.secs.max(0.0) as f64;
            let release_at = press_at + key.hold_ms as f64 / 1000.0;
            info!(
                "auto-key armed: rawkey {:#04X} press at {:.1}s emulated, hold {}ms",
                key.rawkey, key.secs, key.hold_ms
            );
            self.auto_keys.push(ScheduledKey {
                press_at_emulated_secs: press_at,
                release_at_emulated_secs: release_at,
                rawkey: key.rawkey,
                pressed: false,
            });
        }
        for (secs, button, dur_ms, port) in self.pending_auto_clicks.drain(..) {
            let press_at = secs.max(0.0) as f64;
            let release_at = press_at + dur_ms as f64 / 1000.0;
            info!(
                "auto-click armed: {:?} press at {:.1}s emulated, hold {}ms, port {}",
                button,
                secs,
                dur_ms,
                port + 1
            );
            self.auto_clicks.push(ScheduledClick {
                press_at_emulated_secs: press_at,
                release_at_emulated_secs: release_at,
                button,
                port,
                pressed: false,
            });
        }
        for (secs, dx, dy, port) in self.pending_auto_mouse.drain(..) {
            self.auto_mouse.push((secs.max(0.0) as f64, dx, dy, port));
        }
        for (secs, x, y, port) in self.pending_auto_mouse_to.drain(..) {
            self.auto_mouse_to.push((secs.max(0.0) as f64, x, y, port));
        }
        if !self.auto_mouse_to.is_empty() {
            // Earliest first: the firing pass takes one at a time, and a
            // servo holds the pointer until it lands or gives up.
            self.auto_mouse_to
                .sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            info!(
                "auto-mouse-to armed: {} scheduled pointer targets",
                self.auto_mouse_to.len()
            );
        }
        if !self.auto_mouse.is_empty() {
            info!(
                "auto-mouse armed: {} scheduled motions",
                self.auto_mouse.len()
            );
        }
        for (secs, x, y, port) in self.pending_auto_pots.drain(..) {
            self.auto_pots.push((secs.max(0.0) as f64, x, y, port));
        }
        if !self.auto_pots.is_empty() {
            info!(
                "auto-pot armed: {} scheduled positions",
                self.auto_pots.len()
            );
        }
        for (secs, button, dur_ms, port) in self.pending_auto_joys.drain(..) {
            let press_at = secs.max(0.0) as f64;
            let release_at = press_at + dur_ms as f64 / 1000.0;
            info!(
                "auto-joy armed: {:?} press at {:.1}s emulated, hold {}ms, port {}",
                button,
                secs,
                dur_ms,
                port + 1
            );
            self.auto_joys.push(ScheduledJoy {
                press_at_emulated_secs: press_at,
                release_at_emulated_secs: release_at,
                button,
                port,
                pressed: false,
            });
        }
        for insert in self.pending_auto_disk_inserts.drain(..) {
            let insert_at_emulated_secs = insert.secs.max(0.0) as f64;
            info!(
                "auto-disk armed: df{} insert {} at {:.1}s emulated time",
                insert.drive_idx,
                insert.path.display(),
                insert.secs
            );
            self.auto_disk_inserts.push(ScheduledDiskInsert {
                insert_at_emulated_secs,
                drive_idx: insert.drive_idx,
                path: insert.path,
                write_protected: insert.write_protected,
            });
        }
        for (secs, path) in self.pending_auto_cd_inserts.drain(..) {
            info!(
                "auto-cd armed: insert {} at {secs:.1}s emulated time",
                path.display()
            );
            self.auto_cd_inserts.push((secs.max(0.0) as f64, path));
        }
        for secs in self.pending_auto_freezes.drain(..) {
            info!("auto-freeze armed: cartridge freeze at {secs:.1}s emulated time");
            self.auto_freezes.push(secs.max(0.0) as f64);
        }
    }

    /// Fire any scheduled key/click/joy/mouse/pot/disk/CD events whose
    /// emulated timestamps have passed, then let the input recorder
    /// observe the resulting machine-visible input state once for this
    /// quantum. Runs after step_frame so events land at frame boundaries.
    fn fire_scheduled_events(&mut self) {
        // Fire any scheduled key/click/disk events on emulated time
        // (mirroring the auto-screenshot below): headless runs are
        // unthrottled, so wall-clock gating would land the events at the
        // wrong emulated point or after the run already exited.
        let emu_secs = self.emu.bus().emulated_seconds();
        let mut key_events = Vec::new();
        self.auto_keys.retain_mut(|key| {
            if !key.pressed && emu_secs >= key.press_at_emulated_secs {
                info!("auto-key pressing: rawkey {:#04X}", key.rawkey);
                key_events.push((key.rawkey, true));
                key.pressed = true;
            }
            if key.pressed && emu_secs >= key.release_at_emulated_secs {
                info!("auto-key releasing: rawkey {:#04X}", key.rawkey);
                key_events.push((key.rawkey, false));
                false
            } else {
                true
            }
        });
        for (rawkey, pressed) in key_events {
            self.handle_amiga_key_event(rawkey, pressed);
        }
        // Fire any scheduled --click-after events: transition the
        // corresponding button to pressed at press_at, released at
        // release_at, then drop the entry.
        self.auto_clicks.retain_mut(|c| {
            if !c.pressed && emu_secs >= c.press_at_emulated_secs {
                info!("auto-click pressing: {:?} (port {})", c.button, c.port + 1);
                set_mouse_button(&mut self.emu, c.port, c.button, true);
                c.pressed = true;
            }
            if c.pressed && emu_secs >= c.release_at_emulated_secs {
                info!("auto-click releasing: {:?}", c.button);
                set_mouse_button(&mut self.emu, c.port, c.button, false);
                return false;
            }
            true
        });
        // Fire any scheduled --joy-after events into the named port's
        // joystick/CD32-pad state, then assert the held sets (input polling
        // re-applies them every quantum while scripting is engaged).
        let mut joy_changed = [false; 2];
        let held = &mut self.auto_joy_held;
        self.auto_joys.retain_mut(|j| {
            let port = usize::from(j.port != 0);
            if !j.pressed && emu_secs >= j.press_at_emulated_secs {
                info!("auto-joy pressing: {:?} (port {})", j.button, port + 1);
                held[port].set(j.button, true);
                j.pressed = true;
                joy_changed[port] = true;
            }
            if j.pressed && emu_secs >= j.release_at_emulated_secs {
                info!("auto-joy releasing: {:?}", j.button);
                held[port].set(j.button, false);
                joy_changed[port] = true;
                return false;
            }
            true
        });
        for port in 0..2 {
            if joy_changed[port] {
                self.auto_joy_engaged[port] = true;
                self.apply_auto_joy_state(port);
            }
        }
        // Fire any scheduled --mouse-after relative motions (one-shot
        // each); these land on the named port's quadrature counters,
        // whatever device is configured there (the lines are the lines).
        // Held back while a --mouse-to-after servo is steering: the servo
        // measures the pointer's response to its own counts to learn the
        // guest's acceleration, so motion from another source in the same
        // frame is attributed to it and corrupts the estimate. The
        // deferral is bounded by the servo's own frame budget.
        //
        // The servo is advanced first so a target coming due this frame
        // takes ownership before the deltas are considered, and so the
        // frame it finishes on releases them again immediately.
        // Only while the machine is actually advancing. fire_scheduled_events
        // runs on every event-loop pass, including while paused or powered
        // off; polling the servo there would compare the same frame to
        // itself, inject counts the guest never gets to act on, and then
        // call a reachable target stuck -- with the phantom deltas landing
        // on unpause.
        if self.powered_on && !self.paused && !self.cpu_halted {
            self.advance_scripted_pointer_targets(emu_secs);
        }
        let mut mouse_deltas = Vec::new();
        if self.active_mouse_to.is_none() {
            self.auto_mouse.retain(|&(at, dx, dy, port)| {
                if emu_secs >= at {
                    mouse_deltas.push((dx, dy, port));
                    false
                } else {
                    true
                }
            });
        }
        for (dx, dy, port) in mouse_deltas {
            self.apply_scripted_mouse_delta(port, dx, dy);
        }
        // Fire any scheduled --pot-after analogue positions (one-shot
        // each).
        let mut pot_sets = Vec::new();
        self.auto_pots.retain(|&(at, x, y, port)| {
            if emu_secs >= at {
                pot_sets.push((x, y, port));
                false
            } else {
                true
            }
        });
        for (x, y, port) in pot_sets {
            info!("auto-pot: position ({x}, {y}) on port {}", port + 1);
            self.emu.bus_mut().input.set_analogue(port as usize, x, y);
            self.emu
                .tt_note_input(crate::inputsched::ReplayAction::Pot { port, x, y });
        }
        let mut disk_inserts = Vec::new();
        self.auto_disk_inserts.retain(|insert| {
            if emu_secs >= insert.insert_at_emulated_secs {
                disk_inserts.push(insert.clone());
                false
            } else {
                true
            }
        });
        for insert in disk_inserts {
            self.insert_disk_image(insert.drive_idx, insert.path, insert.write_protected);
        }
        // Fire any scheduled --insert-cd-after swaps (one-shot each).
        let mut cd_inserts = Vec::new();
        self.auto_cd_inserts.retain(|(at, path)| {
            if emu_secs >= *at {
                cd_inserts.push(path.clone());
                false
            } else {
                true
            }
        });
        for path in cd_inserts {
            if self.emu.bus().cd_drive_present() {
                self.insert_cd_image_from_path(&path);
            } else {
                warn!(
                    "--insert-cd-after {}: no CD drive on this machine",
                    path.display()
                );
            }
        }
        // Fire any scheduled --freeze-after presses (one-shot each).
        let mut freezes = 0usize;
        self.auto_freezes.retain(|&at| {
            if emu_secs >= at {
                freezes += 1;
                false
            } else {
                true
            }
        });
        for _ in 0..freezes {
            if self.emu.cartridge_fitted() {
                self.freeze_cartridge();
            } else {
                warn!("--freeze-after: no freezer cartridge on this machine ([cartridge] model)");
            }
        }
        // Input recording: with every input source for this quantum
        // applied (live, gamepad, and the scheduled events above), diff
        // the machine-visible input state once at this quantum's emulated
        // timestamp. Skipped while the core is not advancing so paused
        // wall-clock time records nothing.
        if self.powered_on && !self.cpu_halted && !self.paused {
            if let Some(rec) = self.input_recorder.as_mut() {
                rec.observe(&self.emu.bus().input, emu_secs);
            }
        }
    }

    /// Scheduled --save-state-after capture. Runs after step_frame for
    /// the quantum, so the machine is at the frame-boundary quiescent
    /// point save states require. Unlike the auto-screenshot this does not
    /// end the run: a state save is a capture along the way, not the end
    /// of a verification run.
    fn fire_auto_save_state(&mut self) {
        if self.auto_save_state.is_empty() {
            return;
        }
        let now = self.emu.bus().emulated_seconds();
        // Deadline-ordered, so everything still pending starts at the first
        // entry that is not due yet.
        let due = self
            .auto_save_state
            .iter()
            .take_while(|(secs, _)| now >= *secs as f64)
            .count();
        for (_, path) in self.auto_save_state.drain(..due).collect::<Vec<_>>() {
            match self.emu.save_state(&path) {
                Ok(()) => info!("auto-save-state saved: {}", path.display()),
                Err(e) => warn!("auto-save-state failed ({}): {e:#}", path.display()),
            }
        }
    }

    /// Fire every scheduled --screenshot-after capture whose emulated
    /// timestamp has passed. Returns true when the last one has been saved
    /// and the run is complete (both loops exit on it); captures stay armed
    /// while the target frame is still being rendered, and a run with more
    /// captures still pending keeps going.
    fn fire_auto_shot(&mut self) -> bool {
        if self.auto_shot.is_empty() {
            return false;
        }
        let now = self.emu.bus().emulated_seconds();
        // Deadline-ordered, so everything still pending starts at the first
        // entry that is not due yet.
        let due = self
            .auto_shot
            .iter()
            .take_while(|(secs, _)| now >= *secs as f64)
            .count();
        if due == 0 {
            return false;
        }
        let emulated_frame = self.emu.bus().emulated_frames();
        self.finish_render_for_current_frame();
        if self.last_rendered_emulated_frame != Some(emulated_frame) {
            return false;
        }
        // Captures that came due together all describe this frame, so they
        // all get it rather than drifting a frame apart.
        for (_, path) in self.auto_shot.drain(..due).collect::<Vec<_>>() {
            self.save_screenshot(&path);
        }
        if !self.auto_shot.is_empty() {
            return false;
        }
        self.emu.report_stats();
        self.emu.bus().poll_stats.dump_top("at screenshot");
        // Evaluate an untargeted reverse watchpoint at run end.
        if let Err(e) = self.emu.tt_finalize_reverse_watch() {
            warn!("reverse watchpoint evaluation failed: {e:#}");
        }
        true
    }
}

impl Drop for App {
    /// Flush a whole-run `--record-input` recording on any exit path
    /// (auto-screenshot exit, window close, shortcut quit). The interactive
    /// recording toggle writes its file when stopped, so by the time the app
    /// drops there is nothing left for it here.
    fn drop(&mut self) {
        let (Some(rec), Some(path)) = (self.input_recorder.take(), self.record_input_path.take())
        else {
            return;
        };
        let events = rec.events_recorded();
        match std::fs::write(&path, rec.finish()) {
            Ok(()) => info!(
                "input recording saved: {} ({events} events)",
                path.display()
            ),
            Err(e) => warn!("input recording save failed ({}): {e:#}", path.display()),
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.render.is_some() {
            return;
        }
        // Keep the internal overscan field buffer, but present it with
        // the configured pixel aspect: a standard 4:3 Amiga display by
        // default, or square pixels ([display] pixel_aspect = "square").
        let size = LogicalSize::new(FB_WIDTH as f64, window_present_height() as f64);
        // Headless capture (screenshot / frame dump) renders into the
        // framebuffer for the saved PNG but has no interactive viewer, so
        // create the window hidden: it avoids flashing an empty window on
        // screen and removes the vsync present gate, letting the run
        // advance as fast as the host allows. Emulated state is identical.
        let headless_capture =
            !self.pending_auto_shot.is_empty() || self.pending_frame_dump.is_some();
        // Start fullscreen only for an interactive window ([display] full_screen
        // / --full-screen); a headless capture window stays hidden and windowed.
        let fullscreen =
            (self.start_fullscreen && !headless_capture).then(|| Fullscreen::Borderless(None));
        let attrs = WindowAttributes::default()
            .with_title(window_title())
            .with_window_icon(copperline_window_icon())
            .with_visible(!headless_capture)
            .with_fullscreen(fullscreen)
            .with_inner_size(size)
            .with_min_inner_size(LogicalSize::new(
                FB_WIDTH as f64 / 2.0,
                window_present_height() as f64 / 2.0,
            ));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                error!("create_window failed: {e}");
                event_loop.exit();
                return;
            }
        };
        // winit's with_window_icon above does nothing for the macOS dock; set
        // the application icon explicitly now that NSApplication exists.
        #[cfg(target_os = "macos")]
        set_macos_dock_icon();
        let inner = window.inner_size();
        let texture_scale = plan_present_scaling(
            integer_scaling_requested(),
            window.scale_factor(),
            (inner.width.max(1), inner.height.max(1)),
        )
        .texture_scale;
        // On Linux, restrict wgpu to the Vulkan backend. wgpu's GL fallback
        // initializes its EGL instance without a display handle (pixels uses
        // InstanceDescriptor::new_without_display_handle), so EGL drops to the
        // Mesa "surfaceless" platform, which is not compatible with an on-screen
        // window surface -- adapter selection then fails with "No suitable
        // wgpu::Adapter found" on any machine that lacks a hardware Vulkan
        // driver. Vulkan does not need the display handle at instance creation,
        // so it works; GPUs without a hardware Vulkan driver (pre-Skylake Intel
        // and other pre-2015 parts) can fall back to the software lavapipe ICD.
        // An explicit WGPU_BACKEND override is still honoured for debugging.
        // Other platforms keep wgpu's default backend set (Metal on macOS,
        // DX12/Vulkan on Windows). cfg!() (not #[cfg]) keeps the Linux branch
        // type-checked on every host.
        let pixels = match build_pixels_for_window(window.clone(), texture_scale, true) {
            Ok(p) => p,
            Err(e) => {
                error!("pixels init failed: {e}");
                if cfg!(target_os = "linux") {
                    error!(
                        "Copperline requires a Vulkan driver on Linux. Update your GPU \
                         drivers, or install a software Vulkan ICD (lavapipe): \
                         'vulkan-swrast' on Arch, 'mesa-vulkan-drivers' on Debian/Ubuntu, \
                         'mesa-vulkan-drivers' (or 'vulkan-loader') on Fedora."
                    );
                }
                event_loop.exit();
                return;
            }
        };
        info!(
            "window + pixels surface ready ({}x{}, texture {}x{})",
            inner.width,
            inner.height,
            texture_width(texture_scale),
            texture_height(texture_scale)
        );
        let scaler = scaler::PresentScaler::new(pixels.device(), pixels.render_texture_format());
        let rtg_texture =
            rtg_texture::RtgTexture::new(pixels.device(), pixels.render_texture_format());
        let mut crt_shader =
            crt_shader::CrtShader::new(pixels.device(), pixels.render_texture_format());
        let bezel_shader = bezel::BezelShader::new(pixels.device(), pixels.render_texture_format());
        let sticker_pass =
            stickers::StickerPass::new(pixels.device(), pixels.render_texture_format());
        // A user shader can only be compiled once the device exists (too
        // early for `reload_custom_shader`, which needs the built `Render`).
        // A bad one drops back to no shader rather than failing the session:
        // the log takes naga's whole multi-line diagnostic, the overlay
        // below only its first line.
        let shader_error = match (self.crt_shader_kind, self.custom_shader_path.as_deref()) {
            (crate::config::ShaderKind::Custom, Some(path)) => crt_shader
                .load_custom(pixels.device(), pixels.render_texture_format(), path)
                .err()
                .map(|msg| {
                    error!("[display] shader: {msg}");
                    msg.lines().next().unwrap_or_default().to_string()
                }),
            _ => None,
        };
        if shader_error.is_some() {
            self.crt_shader_kind = crate::config::ShaderKind::None;
        }
        self.render = Some(Render {
            window,
            pixels,
            texture_scale,
            scaler,
            rtg_texture,
            crt_shader,
            bezel_shader,
            sticker_pass,
            minimized: false,
            surface_size: (inner.width.max(1), inner.height.max(1)),
        });
        // After the window exists, so the overlay has somewhere to be drawn.
        if let Some(msg) = shader_error {
            self.show_osd(format!("CRT shader: off (custom failed: {msg})"));
        }
        // The sticker sheet is CPU-side, but its pass lives in `Render`, so
        // it loads here too; a bad folder falls back to no stickers.
        if let Err(msg) = self.reload_bezel_stickers() {
            self.show_osd(format!("Bezel stickers: off ({msg})"));
        }
        // Paint at least once so the status bar (and power button) is
        // visible immediately, even when the machine starts powered off
        // and no emulated frame is being produced yet. A powered-off
        // start shows the test screen rather than a black display.
        if !self.powered_on {
            paint_test_screen(&mut self.fb);
            self.deinterlacer
                .push_field(&self.fb, FB_HEIGHT, FB_WIDTH, false, true, true);
            self.refresh_present_from_deinterlacer();
        }
        self.request_redraw();
        self.arm_scheduled_events();
        self.engage_warp_launch();
        self.engage_warp_boot();
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if self.netplay_window_event(event_loop, &event) {
            return;
        }
        if let Some(kind) = self.tool_window_kind(window_id) {
            self.handle_tool_window_event(event_loop, kind, event);
            return;
        }
        if self
            .render
            .as_ref()
            .is_some_and(|render| render.window.id() != window_id)
        {
            return;
        }
        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
            }
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        state,
                        physical_key: PhysicalKey::Code(code),
                        repeat,
                        text,
                        ..
                    },
                ..
            } => {
                if self.should_drop_repeated_main_key(code, state, repeat) {
                    return;
                }
                // An open menu takes the keyboard first: while it is up the
                // cursor keys walk it rather than reaching the Amiga.
                if self.ui.menu_open
                    && !self.ui.menu_rows.is_empty()
                    && state == ElementState::Pressed
                    && self.handle_menu_key(code, event_loop)
                {
                    return;
                }
                match (code, state) {
                    (KeyCode::KeyQ, ElementState::Pressed)
                        if host_shortcut_modifier_pressed(self.modifiers) =>
                    {
                        event_loop.exit()
                    }
                    (KeyCode::KeyS, ElementState::Pressed)
                        if host_shortcut_modifier_pressed(self.modifiers)
                            && self.modifiers.shift_key() =>
                    {
                        if self.save_states_allowed() {
                            self.save_state_interactive()
                        }
                    }
                    (KeyCode::KeyL, ElementState::Pressed)
                        if host_shortcut_modifier_pressed(self.modifiers)
                            && self.modifiers.shift_key() =>
                    {
                        if self.save_states_allowed() {
                            self.load_state_from_dialog(Some(event_loop))
                        }
                    }
                    (KeyCode::KeyS, ElementState::Pressed)
                        if host_shortcut_modifier_pressed(self.modifiers) =>
                    {
                        self.take_screenshot()
                    }
                    (KeyCode::KeyD, ElementState::Pressed)
                        if host_shortcut_modifier_pressed(self.modifiers) =>
                    {
                        self.cycle_disk()
                    }
                    (KeyCode::KeyG, ElementState::Pressed)
                        if host_shortcut_modifier_pressed(self.modifiers) =>
                    {
                        // Capturing the mouse under an open menu/panel would
                        // hide the cursor the panel needs.
                        if !self.modal_ui_active() {
                            self.toggle_mouse_capture()
                        }
                    }
                    (KeyCode::KeyE, ElementState::Pressed)
                        if host_shortcut_modifier_pressed(self.modifiers) =>
                    {
                        self.toggle_menu();
                    }
                    (KeyCode::KeyB, ElementState::Pressed)
                        if host_shortcut_modifier_pressed(self.modifiers)
                            && self.modifiers.shift_key() =>
                    {
                        // The freezer cartridge's button: acts on the
                        // running machine, so not under a menu or panel,
                        // and never in a player build.
                        if !crate::video::player_profile() && !self.modal_ui_active() {
                            self.freeze_cartridge();
                        }
                    }
                    (KeyCode::KeyB, ElementState::Pressed)
                        if host_shortcut_modifier_pressed(self.modifiers) =>
                    {
                        // A player build ships no debugging surface.
                        if !crate::video::player_profile() {
                            self.toggle_debugger();
                            self.ensure_tool_windows_for_open_panels(event_loop);
                        }
                    }
                    (KeyCode::KeyK, ElementState::Pressed)
                        if host_shortcut_modifier_pressed(self.modifiers) =>
                    {
                        if !crate::video::player_profile() {
                            self.toggle_console();
                            self.ensure_tool_windows_for_open_panels(event_loop);
                        }
                    }
                    (KeyCode::KeyJ, ElementState::Pressed)
                        if host_shortcut_modifier_pressed(self.modifiers) =>
                    {
                        self.cycle_joystick_input_mode();
                        self.persist_player_prefs();
                    }
                    (KeyCode::KeyM, ElementState::Pressed)
                        if host_shortcut_modifier_pressed(self.modifiers) =>
                    {
                        self.toggle_bezel();
                        self.persist_player_prefs();
                    }
                    (KeyCode::KeyP, ElementState::Pressed)
                        if host_shortcut_modifier_pressed(self.modifiers) =>
                    {
                        if !crate::video::player_profile() {
                            self.toggle_perf_overlay()
                        }
                    }
                    (KeyCode::KeyF, ElementState::Pressed)
                        if host_shortcut_modifier_pressed(self.modifiers)
                            && self.modifiers.shift_key() =>
                    {
                        // The player has no status bar to bring back.
                        if !crate::video::player_profile() {
                            self.toggle_status_bar()
                        }
                    }
                    (KeyCode::KeyF, ElementState::Pressed)
                        if host_shortcut_modifier_pressed(self.modifiers) =>
                    {
                        self.toggle_fullscreen();
                        self.persist_player_prefs();
                    }
                    (KeyCode::KeyR, ElementState::Pressed)
                        if host_shortcut_modifier_pressed(self.modifiers)
                            && self.modifiers.shift_key() =>
                    {
                        // Input recordings are replay scripts, and the
                        // player has no way to play one back: a diagnostic
                        // surface, so off in shipped games. Screenshots
                        // and video recording stay -- they are end-user
                        // features, not debugging ones.
                        if !crate::video::player_profile() {
                            self.toggle_input_recording()
                        }
                    }
                    (KeyCode::KeyR, ElementState::Pressed)
                        if host_shortcut_modifier_pressed(self.modifiers) =>
                    {
                        self.toggle_recording()
                    }
                    (KeyCode::KeyW, ElementState::Pressed)
                        if host_shortcut_modifier_pressed(self.modifiers)
                            && self.modifiers.shift_key() =>
                    {
                        self.cycle_warp_speed()
                    }
                    (KeyCode::KeyZ, ElementState::Pressed)
                        if host_shortcut_modifier_pressed(self.modifiers) =>
                    {
                        // Rewind acts on the running machine, so ignore it
                        // while a menu or panel has the foreground.
                        if !self.modal_ui_active() {
                            self.rewind_one_step()
                        }
                    }
                    (KeyCode::KeyW, ElementState::Pressed)
                        if host_shortcut_modifier_pressed(self.modifiers) =>
                    {
                        self.toggle_warp()
                    }
                    (KeyCode::KeyA, ElementState::Pressed)
                        if host_shortcut_modifier_pressed(self.modifiers)
                            && self.modifiers.shift_key() =>
                    {
                        // Cycle the live audio output (Default -> devices ->
                        // Disabled), same as the menu's Audio Out item. Ignored
                        // while a menu/panel is open, so it acts only on the
                        // running machine, not the config-screen placeholder.
                        if !self.modal_ui_active() {
                            self.cycle_audio_output();
                            self.persist_player_prefs();
                        }
                    }
                    (KeyCode::KeyA, ElementState::Pressed)
                        if host_shortcut_modifier_pressed(self.modifiers) =>
                    {
                        // Cycle Paula's analogue filter (auto -> on -> off),
                        // the counterpart to Cmd/Alt+Shift+A. Ignored while a
                        // menu or panel is open.
                        if !self.modal_ui_active() {
                            self.cycle_audio_filter();
                            self.persist_player_prefs();
                        }
                    }
                    // Quick save/load slots on the number row: the modifier
                    // alone saves, adding Shift loads. Matched on the
                    // physical key so the mapping holds on non-QWERTY
                    // layouts, and `0` is the tenth slot as it sits.
                    (code, ElementState::Pressed)
                        if host_shortcut_modifier_pressed(self.modifiers)
                            && save_slot_for_key(code).is_some()
                            && !self.modal_ui_active() =>
                    {
                        let slot = save_slot_for_key(code).expect("guarded above");
                        if self.save_states_allowed() {
                            if self.modifiers.shift_key() {
                                self.quick_load_state(slot, Some(event_loop));
                            } else {
                                self.quick_save_state(slot);
                            }
                        }
                    }
                    (KeyCode::Equal, ElementState::Pressed)
                        if host_shortcut_modifier_pressed(self.modifiers)
                            && self.modifiers.shift_key() =>
                    {
                        // Raise the sampler input gain (Shift+= is "+"). A no-op
                        // unless a sampler is attached; ignored under a menu/panel.
                        if !self.modal_ui_active() {
                            self.step_sampler_gain(true)
                        }
                    }
                    (KeyCode::Minus, ElementState::Pressed)
                        if host_shortcut_modifier_pressed(self.modifiers)
                            && self.modifiers.shift_key() =>
                    {
                        // Lower the sampler input gain (Shift+- is "_").
                        if !self.modal_ui_active() {
                            self.step_sampler_gain(false)
                        }
                    }
                    (KeyCode::Period, ElementState::Pressed)
                        if host_shortcut_modifier_pressed(self.modifiers)
                            && self.modifiers.shift_key() =>
                    {
                        // Raise the host mouse sensitivity (Shift+. is ">").
                        if !self.modal_ui_active() {
                            self.step_mouse_sensitivity(true)
                        }
                    }
                    (KeyCode::Comma, ElementState::Pressed)
                        if host_shortcut_modifier_pressed(self.modifiers)
                            && self.modifiers.shift_key() =>
                    {
                        // Lower the host mouse sensitivity (Shift+, is "<").
                        if !self.modal_ui_active() {
                            self.step_mouse_sensitivity(false)
                        }
                    }
                    (other, state) => {
                        let pressed = state == ElementState::Pressed;
                        if pressed && self.ui_handle_key(other, text.as_deref(), Some(event_loop)) {
                            return;
                        }
                        // Open panels are modal: key presses must not leak
                        // into the emulated machine. Releases still pass so
                        // a key held across opening a panel is not stuck.
                        if pressed && self.modal_ui_active() {
                            return;
                        }
                        if self.handle_keyboard_joystick_key(other, pressed) {
                            return;
                        }
                        if let Some(rawkey) = host_to_amiga_rawkey(other) {
                            self.handle_amiga_key_event(rawkey, pressed);
                        }
                    }
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.update_host_modifiers(modifiers.state());
            }
            WindowEvent::CursorMoved { position, .. } => {
                let previous_cursor_pos = self.cursor_pos;
                self.last_cursor_phys = Some(position);
                let display_src = self.display_canvas_src();
                let pos = self
                    .render
                    .as_ref()
                    .and_then(|r| main_cursor_position(r, display_src, position));
                // A button held on the dial turns it by following the hand
                // round the face.
                #[cfg(feature = "mt32")]
                if self.mt32_panel.dial_held() {
                    if let Some(pos) = pos {
                        self.drag_mt32_dial(pos);
                    }
                }
                #[cfg(feature = "coppersynth")]
                if self.csynth_panel.dial_held() {
                    if let Some(pos) = pos {
                        self.drag_csynth_dial(pos);
                    }
                }
                if self.mouse_captured {
                    self.cursor_pos = None;
                    self.last_display_cursor_pos = None;
                } else {
                    // While a menu/panel is open, the host cursor is
                    // operating the UI; don't feed its motion to the
                    // emulated mouse underneath.
                    if self.modal_ui_active() {
                        self.last_display_cursor_pos = None;
                    } else {
                        self.track_uncaptured_cursor_motion(pos);
                    }
                    // A hand actually moving the mouse takes over from
                    // the keyboard: the marker goes away, though where it
                    // stood is remembered, so going back to the keys
                    // resumes from there. Only real motion counts, or a
                    // pointer merely sitting still would keep taking it.
                    if pos != previous_cursor_pos && self.nav.showing() {
                        self.nav.hide();
                        self.request_redraw();
                    }
                    self.cursor_pos = pos;
                    if self.volume_dragging {
                        if let Some(pos) = pos {
                            self.set_output_volume_from_pos(pos);
                        }
                    }
                }
                // The pointer moves the same cursor the keys do, so hovering
                // a row lights it and Return would take it.
                if self.follow_menu_hover() {
                    self.request_redraw();
                }
                let layout = bar_layout(&self.media_bar());
                // The MT-32's buttons light under the pointer as the bar's
                // do, so they need the same redraw when it crosses one.
                #[cfg(feature = "mt32")]
                let mt32_hover_changed =
                    mt32panel::shown_panel_rect(present_height()).is_some_and(|panel| {
                        mt32panel::hover_changed(panel, previous_cursor_pos, self.cursor_pos)
                    });
                #[cfg(not(feature = "mt32"))]
                let mt32_hover_changed = false;
                #[cfg(feature = "coppersynth")]
                let csynth_hover_changed = csynthpanel::shown_panel_rect(csynth_panel_top())
                    .is_some_and(|panel| {
                        csynthpanel::hover_changed(panel, previous_cursor_pos, self.cursor_pos)
                    });
                #[cfg(not(feature = "coppersynth"))]
                let csynth_hover_changed = false;
                // The keycaps light under the pointer the same way.
                let kbd_hover_changed = kbdpanel::shown_panel_rect(keyboard_panel_top())
                    .is_some_and(|panel| {
                        kbdpanel::hover_changed(panel, previous_cursor_pos, self.cursor_pos)
                    });
                if bar_hover_changed(&layout, previous_cursor_pos, self.cursor_pos)
                    || mt32_hover_changed
                    || csynth_hover_changed
                    || kbd_hover_changed
                    || self.main_ui_hover_changed(previous_cursor_pos, self.cursor_pos)
                {
                    self.request_redraw();
                }
            }
            WindowEvent::CursorLeft { .. } => {
                let previous_cursor_pos = self.cursor_pos;
                self.cursor_pos = None;
                self.last_cursor_phys = None;
                self.last_display_cursor_pos = None;
                self.volume_dragging = false;
                self.analyzer_dragging = false;
                self.menu_hover_arm = None;
                let layout = bar_layout(&self.media_bar());
                if bar_hover_changed(&layout, previous_cursor_pos, self.cursor_pos) {
                    self.request_redraw();
                }
            }
            WindowEvent::HoveredFile(_) => {
                // One event per hovered file; flag once and redraw once.
                if !self.drop_hover {
                    self.drop_hover = true;
                    self.request_redraw();
                }
            }
            WindowEvent::HoveredFileCancelled => {
                self.drop_hover = false;
                self.request_redraw();
            }
            WindowEvent::DroppedFile(path) => {
                // No HoveredFileCancelled follows a successful drop, so the
                // hint is cleared here. One event arrives per dropped file;
                // they are coalesced into a single action in about_to_wait.
                self.drop_hover = false;
                self.pending_dropped_files.push(path);
                self.request_redraw();
            }
            WindowEvent::Focused(focused) => {
                self.main_window_focused = focused;
                if focused {
                    // A capture a panel borrowed can only be repaid to a
                    // focused window, so a panel that closed while the focus
                    // was still elsewhere left the loan outstanding for this
                    // moment.
                    self.restore_mouse_capture_after_ui();
                    // In auto mode the grab follows the focus, so the
                    // window that has the keyboard also has the pointer and
                    // no host cursor is ever loose over the display. This is
                    // also the start-up grab: the first Focused(true) is the
                    // one that arrives when the window opens.
                    self.apply_auto_mouse_capture();
                } else {
                    self.volume_dragging = false;
                    self.analyzer_dragging = false;
                    self.set_mouse_captured(false);
                    // The button that was holding a keycap will lift over
                    // some other window, where no MouseInput reaches us.
                    self.release_keyboard_panel_key();
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let pressed = state == ElementState::Pressed;
                if pressed {
                    self.log_cursor_diag(button);
                }
                if button == MouseButton::Left {
                    if pressed {
                        self.analyzer_dragging = false;
                    } else {
                        let was_volume_dragging = self.volume_dragging;
                        self.volume_dragging = false;
                        self.analyzer_dragging = false;
                        self.scroll_hold = None;
                        self.cycle_hold = None;
                        if was_volume_dragging {
                            return;
                        }
                    }
                }
                if pressed && !self.mouse_captured && self.modal_ui_active() {
                    if button == MouseButton::Left {
                        if let Some(control) =
                            self.cursor_pos.and_then(|p| self.main_ui_control_at(p))
                        {
                            // The pointer takes the focus with it: the line
                            // goes out, and coming back to the keyboard
                            // resumes from whatever the hand last pressed.
                            self.nav
                                .follow_pointer(crate::video::nav::NavTarget::Ui(control));
                            // A scroll arrow keeps running while it is held,
                            // and a fresh press starts the ramp again rather
                            // than picking up the speed the last one reached.
                            if scroll_arrow_of(control).is_some() {
                                self.scroll_hold =
                                    Some((control, Instant::now() + SCROLL_HOLD_DELAY));
                                self.reset_scroll_rate(control);
                            }
                            // Every stepper in the launcher runs on while
                            // it is held, so a setting with a long list is
                            // never a hundred clicks away.
                            if let UiControl::LauncherCycle { field, .. } = control {
                                let now = Instant::now();
                                self.cycle_hold =
                                    Some((control, now + cycle_hold_delay(field), now));
                            }
                            self.activate_ui_control_with_event_loop(control, Some(event_loop));
                            self.ensure_tool_windows_for_open_panels(event_loop);
                            return;
                        }
                    }
                    if self.ui.menu_open {
                        // A click anywhere off the menu closes it, except on
                        // the menu button itself, whose own handler toggles.
                        if !self
                            .cursor_pos
                            .is_some_and(|p| menu_button_rect().contains(p))
                        {
                            self.close_menu();
                            self.request_redraw();
                        }
                    }
                    // Swallow display clicks under an open menu/panel so
                    // they neither capture the mouse nor reach the Amiga;
                    // status-bar controls below stay clickable.
                    if self.cursor_pos.is_some_and(cursor_in_display) {
                        return;
                    }
                }
                // The MT-32's panel sits above the status bar and takes its
                // own clicks, the way the bar does.
                #[cfg(feature = "mt32")]
                if !pressed {
                    self.mt32_panel.release_dial();
                    // The buttons are momentary: one lights while the mouse
                    // is down on it and comes back out when it lifts.
                    self.mt32_panel.release_press();
                    self.request_redraw();
                }
                #[cfg(feature = "coppersynth")]
                if !pressed {
                    self.csynth_panel.release_dial();
                    self.csynth_panel.release_press();
                    self.request_redraw();
                }
                #[cfg(feature = "mt32")]
                if pressed
                    && !self.mouse_captured
                    && matches!(button, MouseButton::Left | MouseButton::Right)
                {
                    if let (Some(pos), Some(panel)) = (
                        self.cursor_pos,
                        mt32panel::shown_panel_rect(present_height()),
                    ) {
                        if panel.contains(pos) {
                            if let Some(control) = mt32panel::control_at(panel, pos) {
                                let left = button == MouseButton::Left;
                                self.press_mt32_control(control, left, pos);
                            }
                            return;
                        }
                    }
                }
                #[cfg(feature = "coppersynth")]
                if pressed
                    && !self.mouse_captured
                    && matches!(button, MouseButton::Left | MouseButton::Right)
                {
                    if let (Some(pos), Some(panel)) = (
                        self.cursor_pos,
                        csynthpanel::shown_panel_rect(csynth_panel_top()),
                    ) {
                        if panel.contains(pos) {
                            if let Some(control) = csynthpanel::control_at(panel, pos) {
                                let left = button == MouseButton::Left;
                                self.press_csynth_control(control, left, pos);
                            }
                            return;
                        }
                    }
                }
                // A cap on the on-screen keyboard is let go wherever the
                // pointer has got to: one mouse button holds one key, so
                // the lift ends it even if the hand slid off the strip.
                if !pressed && button == MouseButton::Left && self.release_keyboard_panel_key() {
                    return;
                }
                // The keyboard strip sits between the MT-32's panel and the
                // status bar and takes its own clicks, as they both do.
                if pressed && !self.mouse_captured && button == MouseButton::Left {
                    if let (Some(pos), Some(panel)) = (
                        self.cursor_pos,
                        kbdpanel::shown_panel_rect(keyboard_panel_top()),
                    ) {
                        if panel.contains(pos) {
                            if let Some(control) = kbdpanel::control_at(panel, pos) {
                                self.press_keyboard_panel_control(control);
                            }
                            return;
                        }
                    }
                }
                if pressed
                    && !self.mouse_captured
                    && self.cursor_pos.is_some_and(cursor_in_status_bar)
                {
                    if button == MouseButton::Left {
                        if let Some(pos) = self.cursor_pos {
                            let layout = bar_layout(&self.media_bar());
                            match control_at(pos, &layout) {
                                Some(BarControl::Volume) => {
                                    self.nav.follow_pointer(crate::video::nav::NavTarget::Bar(
                                        BarControl::Volume,
                                    ));
                                    self.volume_dragging = true;
                                    self.set_output_volume_from_pos(pos);
                                }
                                Some(control) => {
                                    self.nav
                                        .follow_pointer(crate::video::nav::NavTarget::Bar(control));
                                    self.activate_bar_control(control)
                                }
                                None => {}
                            }
                        }
                    }
                    return;
                }
                if pressed
                    && !self.mouse_captured
                    && self.mouse_capture != crate::config::MouseCapture::Manual
                    && self.cursor_pos.is_some_and(cursor_in_display)
                {
                    // With no mouse on either port there is nothing to
                    // drive: grabbing and hiding the host cursor would
                    // just trap it.
                    if self.mouse_port().is_some() {
                        self.set_mouse_captured(true);
                        // The click that takes the grab is a window-management
                        // action, not an Amiga click. Forwarding it as well
                        // lands a press on the guest immediately before the
                        // first intended one, and two presses that close
                        // together are what Intuition's double-click window is
                        // looking for -- so a single deliberate click on a
                        // gadget arrives as a double click. Swallow it, but
                        // only once the grab has actually taken: if it failed
                        // the mouse stays uncaptured and this click is the only
                        // thing the guest is going to get.
                        if self.mouse_captured {
                            return;
                        }
                    } else {
                        self.show_osd("No mouse on either port".to_string());
                    }
                }
                if let Some(port) = self.mouse_port() {
                    let input = &mut self.emu.bus_mut().input;
                    match button {
                        MouseButton::Left => input.set_mouse_button(port, 0, pressed),
                        MouseButton::Right => input.set_mouse_button(port, 1, pressed),
                        MouseButton::Middle => input.set_mouse_button(port, 2, pressed),
                        _ => {}
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                // An open menu claims the wheel and does nothing with it,
                // rather than letting it reach the volume slider underneath.
                if self.ui.menu_open {
                } else if !self.mouse_captured
                    && self
                        .cursor_pos
                        .is_some_and(|pos| volume_control_hit_rect().contains(pos))
                {
                    if let Some(steps) = volume_scroll_steps(delta) {
                        self.adjust_output_volume(steps * VOLUME_STEP_PERCENT);
                    }
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // The host DPI changed -- the GNOME/Wayland scale setting was
                // altered while running, or (the common case) the window was
                // dragged onto a monitor with a different scale factor. winit
                // resizes the surface via the Resized event that follows, but
                // the backing texture is sized FB_WIDTH x window height
                // times an integer supersample factor captured at window
                // creation. Left stale, the UI drawn into it keeps the old
                // monitor's crispness. Re-plan for the new scale (which also
                // re-fits integer scaling's texture factor, which tracks the
                // surface rather than the DPI); the redraw recomputes the
                // clip rect from the same plan.
                if let Some(r) = self.render.as_mut() {
                    let surface = r.window.inner_size();
                    if let Err(e) = sync_main_present_scaling(r, (surface.width, surface.height)) {
                        warn!("resize texture buffer for scale {scale_factor} failed: {e}");
                    }
                }
                self.request_redraw();
            }
            WindowEvent::Resized(size) => {
                self.note_window_resize(size);
                self.apply_surface_size(size);
            }
            WindowEvent::RedrawRequested => {
                self.resync_surface_size();
                if self.render.as_ref().is_some_and(|r| r.minimized) {
                    return;
                }
                // The presentation layout can change under a stationary
                // pointer -- the autocrop latch adopting a new crop, a menu
                // suspending it, a toggle -- and every such change requests
                // a redraw, so re-mapping the cached host position here
                // keeps hover and click hit-testing aligned with the
                // pixels this frame actually shows.
                let display_src = self.display_canvas_src();
                if let (Some(phys), Some(r)) = (self.last_cursor_phys, self.render.as_ref()) {
                    self.cursor_pos = main_cursor_position(r, display_src, phys);
                }
                let status = status_with_latched_fdd_track(
                    self.emu.bus().front_panel_status(),
                    &mut self.last_fdd_track,
                );
                let media = self.media_bar();
                let hover = self
                    .cursor_pos
                    .and_then(|pos| control_at(pos, &bar_layout(&media)));
                let control_connected = {
                    #[allow(unused_mut)]
                    let mut connected = false;
                    #[cfg(feature = "control")]
                    {
                        connected |= self.control.as_ref().is_some_and(|c| c.handle.connected());
                    }
                    #[cfg(feature = "gdb")]
                    {
                        connected |= self.gdb.as_ref().is_some_and(|g| g.handle.connected());
                    }
                    connected
                };
                let view = StatusBarView {
                    status,
                    powered_on: self.powered_on,
                    paused: self.paused,
                    media,
                    joystick_input_mode: self.joystick_input_mode,
                    keyboard_panel_shown: super::keyboard_panel_shown(),
                    hover,
                    control_connected,
                };
                let osd = self.active_osd_text();
                // Hoisted before the render borrow: the guest's fn-88
                // overlay list is machine state on the bus.
                let guest_overlay: Vec<crate::uaelib::OverlayCmd> =
                    self.emu.uaelib_overlay().to_vec();
                let ui_hover = self.cursor_pos.and_then(|p| self.main_ui_control_at(p));
                // Decided out here: inside the render borrow the frame is
                // the renderer's, and the focus is the window's business.
                let (nav_target, nav_mix) = self.nav_light();
                let nav_is_open = self.nav.open();
                let (nav_bar_target, nav_bar_mix) = self.nav_bar_light();
                let recording = self.recorder.is_some();
                // The MT-32's panel reads its display live off the engine, so
                // it is only gathered when the panel is actually up.
                #[cfg(feature = "mt32")]
                let mt32_panel = super::mt32_panel_shown()
                    .then(|| self.mt32_panel_view())
                    .flatten();
                #[cfg(feature = "coppersynth")]
                let csynth_panel = super::csynth_panel_shown()
                    .then(|| self.csynth_panel_view())
                    .flatten();
                // The Caps Lock lamp is the MCU's, so it is read fresh
                // every frame rather than mirrored from the clicks.
                let kbd_panel = super::keyboard_panel_shown().then(|| self.keyboard_panel_view());
                let ui_data = self.build_panel_view_data();
                if let Some(r) = self.render.as_mut() {
                    // RTG with a working GPU pipeline presents the native frame
                    // through its own texture in the GPU render pass below.
                    //
                    // Not while the UI is up, though: that pass overdraws the
                    // display region after the UI has been drawn into it, so
                    // an open menu or panel would be painted over and vanish.
                    // Fall back to the CPU present, which composites the UI on
                    // top as usual, at the cost of the FB_WIDTH downscale for
                    // as long as the overlay is open.
                    // The guest's fn-88 overlay is composited on the CPU
                    // canvas too, so it needs the same fallback: the GPU
                    // RTG pass would overdraw it.
                    let rtg_gpu = self.rtg_present_dims.is_some()
                        && !self.ui.active()
                        && guest_overlay.is_empty();
                    // The CRT pass re-draws the display rect from the same
                    // buffer, so it also re-draws whatever the UI composited
                    // into it -- through curvature and a phosphor mask, which
                    // is unreadable for menu and panel text. Suspend it while
                    // an overlay is open, as the RTG arm above does for its
                    // own reason.
                    //
                    // Off for two kinds of frame that have no 15 kHz line
                    // structure to reproduce: RTG board scanout (which reaches
                    // the surface through the RTG texture, not the buffer this
                    // pass samples) and programmable multisync scans (amifb's
                    // 31 kHz console, DblPAL, SHRES), whose fields are not
                    // woven, so the pass's two-rows-per-line assumption would
                    // not hold either.
                    //
                    // Interlaced content is drawn at field-line pitch: the
                    // gaps land every other emulated line over the woven
                    // frame, the look of a 15 kHz set showing an interlaced
                    // signal, rather than one gap per woven row.
                    let crt_active = self.crt_shader_kind != crate::config::ShaderKind::None
                        && !self.ui.active()
                        && self.rtg_present_dims.is_none()
                        && !self.present_programmable;
                    // The bezel is content-agnostic (a frame has no 15 kHz
                    // structure to get wrong), so unlike the CRT pass it
                    // stays on for programmable multisync scans. It shares
                    // the other two suspensions: an open overlay must not
                    // be overdrawn, and RTG scanout reaches the surface
                    // through its own texture, which this pass does not
                    // sample.
                    let bezel_active =
                        self.bezel.is_on() && !self.ui.active() && self.rtg_present_dims.is_none();
                    if let Some((w, h)) = self.rtg_present_dims.filter(|_| rtg_gpu) {
                        r.rtg_texture.upload(
                            r.pixels.device(),
                            r.pixels.queue(),
                            &self.rtg_fb,
                            w,
                            h,
                        );
                    }
                    let frame = r.pixels.frame_mut();
                    if rtg_gpu {
                        // The GPU pass overdraws the display region; black it
                        // out so nothing stale shows at the seams.
                        let rows = present_height() * r.texture_scale;
                        let stride = texture_width(r.texture_scale) * 4;
                        frame[..rows * stride].fill(0);
                    } else {
                        copy_window_present_frame(
                            &self.present_fb,
                            self.present_rows,
                            self.present_width,
                            frame,
                            r.texture_scale,
                            self.overscan,
                            self.tv_centre,
                            // The TV aperture is a chipset crop rect. An RTG
                            // frame fills the buffer on its own terms, so
                            // applying it here would show a sub-rect of the
                            // board's screen.
                            self.present_tv_aperture_rows
                                .filter(|_| self.rtg_present_dims.is_none()),
                            // A drawn bezel shows the tube aperture. Keyed to
                            // the style alone, not bezel_active: an open
                            // overlay suspends the bezel *pass*, and the
                            // picture must not jump between apertures when a
                            // panel opens over it.
                            self.bezel.is_on(),
                        );
                        // The tint models the monitor on the Amiga's video
                        // output, so RTG board scanout stays untinted here
                        // too, matching the GPU RTG path (which never sees
                        // this buffer).
                        if self.rtg_present_dims.is_none() {
                            if let Some(lut) = &self.tint_lut {
                                tint_display_rows(frame, r.texture_scale, lut);
                            }
                        }
                    }
                    #[cfg(feature = "mt32")]
                    if let Some(panel) = &mt32_panel {
                        mt32panel::draw(frame, panel, present_height(), r.texture_scale);
                    }
                    #[cfg(feature = "coppersynth")]
                    if let Some(panel) = &csynth_panel {
                        csynthpanel::draw(frame, panel, csynth_panel_top(), r.texture_scale);
                    }
                    if let Some(panel) = &kbd_panel {
                        kbdpanel::draw(frame, panel, keyboard_panel_top(), r.texture_scale);
                    }
                    if !super::status_bar_hidden() {
                        // The bar lights its focus the way the
                        // surfaces light theirs, and knows when the
                        // marker is up on one of them instead.
                        statusbar::set_nav_light(nav_bar_target, nav_bar_mix, nav_target.is_some());
                        draw_status_bar(frame, &view, r.texture_scale);
                        statusbar::set_nav_light(None, 0.0, false);
                    }
                    // The picture loses its corners to a front's aperture
                    // and to a preset's bowed face; all three overlays live
                    // in one, so all three come in far enough to clear
                    // whichever is drawn (nil when the picture is a plain
                    // rectangle). Worked out before any of them is drawn.
                    let corner = bezel::corner_inset(
                        self.bezel,
                        if crt_active {
                            crt_shader::face_curvature(self.crt_shader_kind)
                        } else {
                            0.0
                        },
                        self.shader_strength,
                        r.texture_scale,
                    );
                    // The corner overlays anchor to the display region the
                    // viewer actually sees: the whole display classically,
                    // the sub-rect while autocrop or per-axis scaling
                    // presents one.
                    let overlay_anchor = display_src.map(|src| src.rect).unwrap_or((
                        0,
                        0,
                        FB_WIDTH,
                        present_height(),
                    ));
                    if !guest_overlay.is_empty() {
                        // The guest's debug overlay sits directly on the
                        // picture, under every piece of host chrome, and --
                        // like everything from here down -- never in a
                        // capture.
                        draw_guest_overlay(
                            frame,
                            &mut self.guest_overlay_cache,
                            &guest_overlay,
                            r.texture_scale,
                            overlay_anchor,
                        );
                    }
                    if recording {
                        // Painted into the presentation texture only, so
                        // the badge never appears in the recorded file.
                        draw_record_badge(frame, r.texture_scale, corner, overlay_anchor);
                    }
                    if self.perf_overlay {
                        draw_perf_overlay(
                            frame,
                            &self.perf.lines,
                            r.texture_scale,
                            recording,
                            corner,
                            overlay_anchor,
                        );
                    }
                    if let Some((text, warning)) = &osd {
                        draw_osd(
                            frame,
                            text,
                            *warning,
                            r.texture_scale,
                            corner,
                            overlay_anchor,
                        );
                    }
                    // The focus lights its control the way the pointer
                    // does, breathing between the two.
                    ui::set_nav_light(nav_target, nav_mix, nav_is_open, nav_bar_target.is_some());
                    ui::draw(frame, r.texture_scale, &self.ui, ui_hover, ui_data.as_ref());
                    ui::set_nav_light(None, 0.0, false, false);
                    // The drag hint sits on top of everything: the drop will
                    // land wherever the drag is released, panels or not. The
                    // launcher refuses drops, so no hint over it.
                    if self.drop_hover && !matches!(self.ui.panel, Some(Panel::Launcher(_))) {
                        ui::draw_drop_hint(frame, r.texture_scale);
                    }
                    // The scaler pass draws the composited buffer with this
                    // layout; every overlay pass below keys its viewport off
                    // the same display rect, and the cursor mapping inverts
                    // the same layout, so all of them agree by construction.
                    // The branches that draw over the display (RTG, CRT,
                    // bezel) only run when the sub-rect modes are
                    // suspended, so their layout is the classic
                    // whole-texture letterbox.
                    let layout = main_present_layout(r, display_src);
                    // COPPERLINE_DIAG_AUTOCROP: the window half of the
                    // renderer-side envelope trace -- the exact rects the
                    // scaler pass draws this frame, logged when they
                    // change, with the mode spelled out: a classic layout
                    // must say WHY it is classic (both modes off, or which
                    // condition suspended them), because the envelope
                    // line prints whether or not a mode is on and the
                    // difference is invisible from it alone. The factors
                    // are the whole-number draw's pixels per canvas
                    // column and per field line, the shape is the glass
                    // ratio a per-axis draw aims at, and the scan says
                    // which window model the envelope came through (a
                    // programmable scan crops too, at the uniform
                    // multiple).
                    if crate::envcfg::flag("COPPERLINE_DIAG_AUTOCROP")
                        && self.last_diag_layout != Some(layout)
                    {
                        let autocrop = crate::video::autocrop();
                        let per_axis = per_axis_scaling_requested();
                        let mode = if !autocrop && !per_axis {
                            "off"
                        } else if self.rtg_present_dims.is_some() {
                            "suspended(rtg)"
                        } else if self.bezel.is_on() {
                            "suspended(bezel)"
                        } else if autocrop && per_axis {
                            "active(autocrop+per-axis)"
                        } else if per_axis {
                            "active(per-axis)"
                        } else {
                            "active(autocrop)"
                        };
                        info!(
                            "[DIAG_AUTOCROP] layout mode={} scan={} surface={:?} \
                             src_canvas={:?} display_dst={:?} chrome_dst={:?} filter={:?} \
                             factors={:?} shape={:?}",
                            mode,
                            if self.present_programmable {
                                "programmable"
                            } else {
                                "standard"
                            },
                            r.surface_size,
                            layout.src_canvas,
                            layout.display_dst,
                            layout.chrome_dst,
                            layout.filter,
                            layout.factors,
                            display_src.map(|src| src.par),
                        );
                        self.last_diag_layout = Some(layout);
                    }
                    let present_clip = layout.display_dst;
                    let present_draws = layout.draws();
                    let scaler = &mut r.scaler;
                    let render_result = if rtg_gpu {
                        // Draw the UI buffer, then overdraw the display region
                        // with the native RTG texture (GPU-scaled). The display
                        // rect is the top present_height fraction of the buffer's
                        // letterboxed clip rect on the surface.
                        let rtg = &r.rtg_texture;
                        // The board frame is drawn straight to the surface,
                        // so integer scaling applies to it in its own native
                        // pixels rather than through the canvas texture the
                        // scaler pass letterboxed above.
                        let integer_scaling = integer_scaling_requested();
                        r.pixels.render_with(|encoder, target, ctx| {
                            scaler.render(
                                &ctx.device,
                                &ctx.queue,
                                &ctx.texture,
                                encoder,
                                target,
                                &present_draws,
                            );
                            let (cx, cy, cw, ch) = present_clip;
                            let disp_h = ch as f32 * present_height() as f32
                                / window_present_height() as f32;
                            rtg.render(
                                &ctx.queue,
                                encoder,
                                target,
                                (cx as f32, cy as f32, cw as f32, disp_h),
                                integer_scaling,
                            );
                            Ok(())
                        })
                    } else if crt_active || bezel_active {
                        // Draw the composited buffer, then re-draw the display
                        // rect. Bezel alone: one pass draws the frame with the
                        // picture scaled into its opening. Preset alone: the
                        // pass covers the display rect. Both: the preset paints
                        // the picture into the opening first and the bezel
                        // frames it on top in frame-only mode -- the plastic
                        // overlaps the tube face, so the frame's rounded
                        // corners and chamfer clip the preset's square viewport
                        // rather than being buried under it. One CRT beam pass
                        // per emulated field line the copy above actually
                        // shows.
                        let scanlines = crt_scanline_count(
                            self.present_rows,
                            present_height(),
                            // The same branch copy_window_present_frame took,
                            // tube aperture included: the line count follows
                            // the rows the copy actually put on the glass.
                            self.present_tv_aperture_rows
                                .filter(|_| {
                                    self.overscan == Overscan::Tv
                                        && self.rtg_present_dims.is_none()
                                        && self.present_width == FB_WIDTH
                                })
                                .map(|rows| {
                                    if self.bezel.is_on() {
                                        tube_aperture_rows(rows)
                                    } else {
                                        rows
                                    }
                                }),
                        );
                        let kind = self.crt_shader_kind;
                        let strength = self.shader_strength;
                        let bezel_style = self.bezel;
                        // Under a sub-rect layout the preset re-draws the
                        // rect into the rect's own viewport -- the same
                        // sub-rect the scaler pass just drew -- with the
                        // beam-line count scaled to the rows the rect shows.
                        // The bezel suspends the sub-rect modes, so this is
                        // never the bezel case.
                        let crt_crop = display_src.map(|_| {
                            (
                                layout.display_dst,
                                layout.src_canvas,
                                scanlines * layout.src_canvas.3 as f32
                                    / present_height().max(1) as f32,
                            )
                        });
                        // The closure is FnOnce and captures `r`, so the
                        // shaders have to be split out of it as separate
                        // borrows rather than reached through `r` inside.
                        let crt = &mut r.crt_shader;
                        let bezel_shader = &mut r.bezel_shader;
                        let sticker_pass = &mut r.sticker_pass;
                        r.pixels.render_with(|encoder, target, ctx| {
                            scaler.render(
                                &ctx.device,
                                &ctx.queue,
                                &ctx.texture,
                                encoder,
                                target,
                                &present_draws,
                            );
                            let texture_extent =
                                (ctx.texture_extent.width, ctx.texture_extent.height);
                            let (uniforms, viewport) = match crt_crop {
                                Some((dst, src, crop_scanlines)) => crt_shader::uniforms_for_rect(
                                    kind,
                                    strength,
                                    dst,
                                    src,
                                    (FB_WIDTH, window_present_height()),
                                    texture_extent,
                                    crop_scanlines,
                                ),
                                None => crt_shader::uniforms_for(
                                    kind,
                                    strength,
                                    present_clip,
                                    present_height(),
                                    window_present_height(),
                                    texture_extent,
                                    scanlines,
                                ),
                            };
                            if bezel_active {
                                let opening = bezel::opening_rect(bezel_style, viewport);
                                if crt_active {
                                    crt.render(
                                        &ctx.device,
                                        &ctx.queue,
                                        &ctx.texture,
                                        encoder,
                                        target,
                                        opening,
                                        kind,
                                        uniforms.with_viewport(opening),
                                    );
                                }
                                bezel_shader.render(
                                    &ctx.device,
                                    &ctx.queue,
                                    &ctx.texture,
                                    encoder,
                                    target,
                                    viewport,
                                    bezel_style,
                                    bezel::uniforms_from(&uniforms, viewport, opening, crt_active),
                                );
                                // Decals stick to the plastic, so they ride
                                // the bezel pass: suspended with it, never
                                // drawn over a bare picture.
                                sticker_pass.render(
                                    &ctx.device,
                                    &ctx.queue,
                                    encoder,
                                    target,
                                    viewport,
                                    opening,
                                );
                            } else {
                                crt.render(
                                    &ctx.device,
                                    &ctx.queue,
                                    &ctx.texture,
                                    encoder,
                                    target,
                                    viewport,
                                    kind,
                                    uniforms,
                                );
                            }
                            Ok(())
                        })
                    } else {
                        r.pixels.render_with(|encoder, target, ctx| {
                            scaler.render(
                                &ctx.device,
                                &ctx.queue,
                                &ctx.texture,
                                encoder,
                                target,
                                &present_draws,
                            );
                            Ok(())
                        })
                    };
                    if let Err(e) = render_result {
                        error!("pixels.render: {e}");
                    }
                }
            }
            _ => {}
        }
    }

    fn device_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _device_id: DeviceId,
        event: DeviceEvent,
    ) {
        if self.netplay.is_some() {
            if let DeviceEvent::MouseMotion { delta } = event {
                if self.mouse_captured && self.main_window_focused {
                    self.add_host_mouse_delta(delta.0, delta.1);
                }
            }
            return;
        }
        match event {
            DeviceEvent::MouseMotion { delta } => {
                if self.mouse_captured {
                    self.add_host_mouse_delta(delta.0, delta.1);
                }
            }
            DeviceEvent::Key(event) => self.handle_raw_device_key_event(event),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Drain remote control commands first, before this pass's run
        // state is computed, so they land at a frame boundary. Sits
        // ahead of the render guard so tests can drive the drain on an
        // App that never opened a window.
        #[cfg(feature = "control")]
        {
            self.drain_control();
            if self.control_exit_requested() {
                event_loop.exit();
                return;
            }
        }
        #[cfg(feature = "gdb")]
        self.drain_gdb();
        // Frames retired outside the burst (a control step, a debugger
        // step) may have carried a guest warp request.
        if self.netplay.is_none() {
            self.service_uaelib();
        }
        if self.render.is_none() {
            return;
        }
        // Act on a completed drop before the OSD/control-flow computation
        // below, so a drop-raised OSD keeps the loop awake for its fade.
        if !self.pending_dropped_files.is_empty() {
            let files = std::mem::take(&mut self.pending_dropped_files);
            self.handle_dropped_files(files);
        }
        let running = self.powered_on && !self.cpu_halted && !self.paused;
        // While a transient overlay is up, keep the loop awake (and, when
        // the machine is paused/off, request repaints) so the message
        // fades on schedule instead of freezing on the last drawn frame.
        let osd_active = match &self.osd {
            Some(osd) if Instant::now() < osd.expires_at => true,
            Some(_) => {
                self.osd = None;
                self.request_redraw();
                false
            }
            None => false,
        };
        // The calibration panel polls raw gamepad events, so it needs the
        // loop awake even while the machine is paused or powered off.
        let calibrating = matches!(self.ui.panel, Some(Panel::Calibration(_)));
        // Likewise the pad's host-side controls (Quit hotkey, Menu
        // button): gilrs is polled, not evented, so pressing one cannot
        // wake a waiting loop. While a pad carrying one is connected, a
        // paused/powered-off loop keeps polling (paced to a human rate
        // below) or the press could never be seen.
        let pad_hotkey_watch = !calibrating && self.gamepad.host_hotkey_present();
        // An image being written on a worker has to be collected, and the
        // launcher is up with the machine off -- nothing else would wake
        // the loop to notice it finished.
        self.poll_image_job();
        #[cfg(feature = "game-library")]
        self.poll_whdload_download();
        #[cfg(feature = "game-library")]
        self.poll_login();
        #[cfg(feature = "game-library")]
        self.poll_library_scan();
        self.repeat_held_scroll();
        self.repeat_held_cycle();
        #[cfg(feature = "game-library")]
        if self
            .launcher_state_mut()
            .is_some_and(|state| state.poll_library_covers())
        {
            self.request_redraw();
        }
        // A line that reported something finished takes itself down.
        if self
            .status_until
            .is_some_and(|at| std::time::Instant::now() >= at)
        {
            self.status_until = None;
            if let Some(state) = self.launcher_state_mut() {
                state.status = None;
            }
            self.request_redraw();
        }
        #[cfg(feature = "game-library")]
        let downloading = self.whdload_job.is_some()
            || self.library_scan.is_some()
            || self.login_job.is_some()
            // A cover on its way needs the loop awake to collect it. It
            // arrives on a worker, and nothing else would wake the loop to
            // notice -- which is a picture that only appears the next time
            // you happen to press something.
            || self
                .launcher_state()
                .is_some_and(|state| state.library.covers.pending());
        #[cfg(not(feature = "game-library"))]
        let downloading = false;
        // A status line waiting to clear itself keeps the loop awake too,
        // or it would sit there until something else woke it.
        let writing_image = self.image_job.is_some() || downloading || self.status_until.is_some();
        // Likewise the caret: it has no event of its own either, and a panel
        // with a box open in it is otherwise perfectly still.
        let typing = self.blink_caret();
        // A held scroll arrow has no event of its own to wake on: the button
        // went down once and the repeats are this loop's own doing.
        let scrolling = self.scroll_hold.is_some() || self.cycle_hold.is_some();
        // The focus breathes, which is the window's own doing: nothing
        // else would wake the loop to draw the next step of it.
        let focused = self.nav.showing();
        // The About panel animates on the clock, so it too must keep the
        // loop awake while it is up.
        let about_open = matches!(self.ui.panel, Some(Panel::About));
        event_loop.set_control_flow(
            if running
                || osd_active
                || calibrating
                || writing_image
                || scrolling
                || typing
                || about_open
                || focused
                || pad_hotkey_watch
            {
                ControlFlow::Poll
            } else {
                ControlFlow::Wait
            },
        );
        if writing_image && !running {
            // Nothing else paces the loop while the machine is off; check
            // back at a human rate rather than spinning a core on it.
            std::thread::sleep(std::time::Duration::from_millis(16));
            self.request_redraw();
        }
        if pad_hotkey_watch && !running && !writing_image {
            // Poll the pad at a human rate rather than spinning a core;
            // plenty of granularity inside the quit hotkey's 1.5 s hold.
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
        if focused && !running && !writing_image {
            // A human rate for the breath, not a spinning core.
            std::thread::sleep(std::time::Duration::from_millis(16));
            self.request_redraw();
        }
        if about_open && !running && !writing_image {
            // The About animation likewise paces itself when the machine
            // is off: a human-rate tick, not a spinning core.
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
        if osd_active && !running {
            self.request_redraw();
        }
        if calibrating {
            // Feed raw pad input to the calibration session instead of the
            // emulated port-2 joystick, releasing anything already held.
            if let Some(Panel::Calibration(session)) = self.ui.panel.as_mut() {
                if self.gamepad.calibration_tick(session) {
                    self.request_redraw();
                }
            }
            for port in 0..2 {
                self.release_joystick_lines(port);
            }
            // The Quit-hotkey hold cannot be observed while the panel owns
            // the pad; drop any hold in progress rather than resuming it,
            // and give the menu bridge nothing stale to act on.
            self.gamepad_quit_hold = None;
            // Once every step is captured, a press still means "test this
            // control" -- so it is a *hold* that hands the panel's own
            // buttons over, and from then on the pad walks them with the
            // very bindings it has just been taught. Without this,
            // finishing a calibration needs a mouse.
            self.pad_last = self.calibration_pad_drives();
            if !running {
                // Nothing paces the loop while the machine is not stepping;
                // don't busy-spin just to poll the pad.
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        } else {
            self.pump_joystick_input();
        }
        if self.netplay.is_none() {
            self.drive_interface_with_pad(event_loop);
        }
        if self.quit_requested {
            event_loop.exit();
            return;
        }
        // Headless capture (screenshot/frame-dump) builds the framebuffer for
        // the saved PNG but presents nothing: it already runs unthrottled at one
        // frame per loop (request_redraw is skipped below), and every captured
        // frame must be rendered, so warp's output frame-skip burst must not
        // apply there.
        let headless_capture = self.headless_capture_active();
        // A completed run-ahead burst renders its future frame before the
        // anchor is restored. Suppress the generic post-step render in that
        // case or it would immediately replace the future image with the
        // rewound anchor.
        let mut runahead_presented = false;
        // Run one scheduler quantum. Rebuild the host framebuffer only
        // when Agnus has crossed into a new frame; the expensive renderer
        // reconstructs a completed hardware frame, not an instruction slice.
        if running {
            // If the live output device vanished (unplugged), reopen on the
            // current default so sound continues.
            self.recover_audio_if_device_lost();
            // Presentation is vsync-gated, so emulating exactly one frame per
            // presented frame would cap warp at the host monitor refresh rate
            // (about 1.2x for 50 Hz PAL on a 60 Hz display). In warp, retire
            // several frames per presented frame (output frame skip): only the
            // last frame of the burst is rendered and presented, so the
            // effective speed is the warp level times the refresh rate, host
            // CPU permitting. Real-time pacing and headless capture stay at one
            // frame per loop.
            // Run-ahead retires one extra speculative frame per configured
            // level on top of the presented frame: input sampled this pass
            // lands up to that many frames earlier relative to what is on
            // screen. The committed anchor supplies audio and host output;
            // later frames are silent speculation, with only the last image
            // presented before the anchor snapshot restores machine state.
            if self.netplay.is_some() {
                if let Err(error) = self.step_netplay() {
                    error!("netplay stopped: {error:#}");
                    if self.netplay_setup.is_some() {
                        self.leave_netplay(Some(error.to_string()));
                    } else {
                        self.show_osd(format!("Netplay stopped: {error}"));
                        self.cpu_halted = true;
                        self.sync_live_audio_suspension();
                    }
                }
            } else {
                let (total_frames, runahead, time_budget) = self.burst_frames(headless_capture);
                let burst_start = Instant::now();
                let mut frames_done = 0usize;
                let mut anchor_end_seconds: Option<f64> = None;
                let mut anchor_snapshot: Option<Vec<u8>> = None;
                let mut burst_complete = true;
                self.emu.set_runahead_phase(runahead > 0);
                loop {
                    let speculative = runahead > 0 && frames_done > 0;
                    self.emu.set_runahead_speculative(speculative);
                    #[cfg(feature = "control")]
                    let step = self.control_step_frame();
                    #[cfg(not(feature = "control"))]
                    let step = self.emu.step_frame();
                    if let Err(e) = step {
                        error!("emulator step halted: {e:?}");
                        self.cpu_halted = true;
                        self.sync_live_audio_suspension();
                        burst_complete = false;
                        break;
                    }
                    #[cfg(feature = "control")]
                    if !speculative {
                        self.control_emit_events();
                    }
                    frames_done += 1;
                    if runahead > 0 && frames_done == 1 {
                        anchor_end_seconds = Some(self.emu.bus().emulated_seconds());
                        match self.emu.runahead_snapshot() {
                            Ok(blob) => anchor_snapshot = Some(blob),
                            Err(e) => {
                                warn!("run-ahead disabled: anchor snapshot failed: {e:?}");
                                self.run_ahead_frames = 0;
                                burst_complete = false;
                                break;
                            }
                        }
                    }
                    // Surface a CPU stop before any host gate can end this
                    // burst. Otherwise a simultaneous warp transition defers the
                    // pending stop until the next pass, which retires one extra
                    // instruction before the debugger receives it.
                    let stopped = self.surface_debug_stop();
                    // Poll even when stopped so automatic warp ends at the same
                    // load boundary and stays coherent with the paused target.
                    if self.poll_warp_launch() || stopped {
                        burst_complete = false;
                        break;
                    }
                    // The warp-boot gate ends its warp the frame its
                    // timestamp or storage-idle condition holds.
                    if self.poll_warp_boot() {
                        burst_complete = false;
                        break;
                    }
                    // The guest's warpmode() through the uaelib trap starts or
                    // ends its warp at the frame it was made. Committed frames
                    // only: a flip on a speculative frame would abandon the
                    // run-ahead anchor mid-burst.
                    if !speculative && self.service_uaelib() {
                        burst_complete = false;
                        break;
                    }
                    // A remote run_until frame/cck target completes the
                    // pending resume and pauses at its boundary.
                    #[cfg(feature = "control")]
                    if self.control_run_target_reached() {
                        burst_complete = false;
                        break;
                    }
                    if frames_done >= total_frames {
                        break;
                    }
                    if let Some(budget) = time_budget {
                        if burst_start.elapsed() >= budget {
                            burst_complete = false;
                            break;
                        }
                    }
                }
                self.emu.set_runahead_phase(false);
                self.emu.set_runahead_speculative(false);
                if runahead > 0 {
                    let speculated = frames_done > 1;
                    if burst_complete && speculated {
                        // Rendering must finish while the future Bus is still
                        // live. The worker otherwise races the restore and the
                        // generic renderer below can present the anchor instead.
                        runahead_presented = self.finish_render_for_current_frame();
                        if !runahead_presented {
                            error!("run-ahead disabled: speculative frame was not renderable");
                            self.run_ahead_frames = 0;
                            burst_complete = false;
                        }
                    }
                    self.restore_runahead_anchor(
                        anchor_snapshot.as_deref(),
                        burst_complete,
                        speculated,
                    );
                    // Include snapshot, render, and restore cost in the same
                    // display-period budget by pacing last against the anchor.
                    if let Some(anchor_end) = anchor_end_seconds {
                        self.emu.pace_runahead_burst(anchor_end);
                    }
                }
            }
            self.refresh_tool_windows_paced(event_loop);
            // A disk swapped by hand in a bridged bay is the one media change
            // no menu or drop initiated, so its message is raised here from
            // the drive's own report -- the same style an image insert or
            // eject shows. On a drive the configuration lets write, the tab
            // is the fact the user just changed disks to check, so it rides
            // along; a config-protected drive says nothing about it, since
            // nothing can be written either way.
            #[cfg(feature = "fluxbridge")]
            for (bay, present, tab) in self.emu.bus_mut().floppy.take_bridge_media_events() {
                self.show_osd(match (present, tab) {
                    (false, _) => format!("DF{bay}: disk ejected"),
                    (true, None) => format!("DF{bay}: disk inserted"),
                    (true, Some(true)) => format!("DF{bay}: disk inserted (write protected)"),
                    (true, Some(false)) => format!("DF{bay}: disk inserted (writable)"),
                });
            }
        }
        // Resample the performance overlay after the step so its revision
        // is current when the redraw decision below is taken.
        self.update_perf_overlay(running);
        self.poll_menu_hover_arm();
        // The About panel's entrance and wave play on the clock; keep
        // the frames coming while it is up.
        if matches!(self.ui.panel, Some(Panel::About))
            && self.about_redraw_at.elapsed() >= std::time::Duration::from_millis(40)
        {
            self.about_redraw_at = std::time::Instant::now();
            self.request_redraw();
        }
        #[cfg(feature = "coppersynth")]
        {
            self.repeat_csynth_dial();
            self.repeat_csynth_buttons();
            self.animate_csynth_panel();
        }
        #[cfg(feature = "mt32")]
        {
            self.repeat_mt32_dial();
            // A machine booted straight into an MT-32 has its port fitted
            // before there is a window to say anything on, so the fault is
            // picked up here instead. Taking it means this says it once.
            if self.serial_is_midi {
                self.report_mt32_fault();
            }
        }
        // Coppersynth's faults and display lines ride the same
        // frame poll; the display only ever has lines while a game writes
        // them, so this is a cheap drain of an empty Vec almost always.
        #[cfg(feature = "coppersynth")]
        if self.serial_is_midi {
            self.report_csynth();
        }
        // While powered off, leave the parked test screen in place; the
        // emulator is not advancing, so there is no new frame to show.
        let mut rendered =
            self.powered_on && (runahead_presented || self.render_emulated_frame_if_needed());
        if self.recorder.is_some() && self.powered_on {
            rendered |= self.finish_render_for_current_frame();
        }
        self.capture_recorder_output(rendered);
        // Skipping request_redraw for headless capture avoids the vsync gate so
        // the run advances as fast as the host allows; emulated state is
        // identical either way. (`headless_capture` was resolved above, before
        // the step, to decide the warp burst.) When the exact presentation and
        // the window chrome are both unchanged, retain the existing GPU texture
        // instead of uploading/presenting it again at the field rate.
        if !headless_capture {
            let redraw_state = self.main_redraw_state();
            let chrome_changed = self.last_main_redraw_state != Some(redraw_state);
            if self.main_presentation_dirty
                || chrome_changed
                // Opening a static menu/panel is not itself a reason to
                // upload and present the whole framebuffer every scheduler
                // pass. New emulated pictures and UI interactions already
                // dirty it. Only the keyboard/pad focus needs clock-driven
                // frames for its breathing highlight.
                || animated_focus_needs_continuous_redraw(running, self.nav.showing())
                || self.drop_hover
                || osd_active
                || calibrating
            {
                self.last_main_redraw_state = Some(redraw_state);
                self.main_presentation_dirty = false;
                self.request_main_redraw();
            }
        }

        if self.dump_frame_if_due() {
            event_loop.exit();
            return;
        }

        self.fire_scheduled_events();
        self.fire_auto_save_state();
        if self.fire_auto_shot() {
            event_loop.exit();
        }
    }
}

/// The file name of a path for on-screen messages, falling back to the
/// full path when there is none.
fn display_file_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// What a file dropped on the window should be treated as. Extension-based:
/// only floppies get content-sniffed (by the insert path itself), and cue
/// sheets/hard disks/ROMs have no shared magic worth probing here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DroppedMediaKind {
    /// Anything the floppy loader may accept (floppy::IMAGE_EXTENSIONS and
    /// unknown extensions): FloppyImage::from_bytes sniffs the content and
    /// rejects what it cannot read, surfacing a clean OSD failure.
    Floppy,
    /// A CD image (cue sheet, bare ISO, Nero NRG, or CHD) for the CD drive.
    Cd,
    /// Hard disk images cannot be hot-attached; point at the config screen.
    HardDisk,
    /// Kickstart ROMs load from the config screen, not at runtime.
    Rom,
    /// A WHDLoad game package (.lha), or a .slave inside an extracted one:
    /// something to boot into (src/whdload.rs), not media to insert.
    WhdloadGame,
}

fn classify_dropped_media(path: &std::path::Path) -> DroppedMediaKind {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase());
    match ext.as_deref() {
        Some("cue") | Some("iso") | Some("nrg") | Some("chd") => DroppedMediaKind::Cd,
        Some("hdf") | Some("hdz") | Some("img") => DroppedMediaKind::HardDisk,
        Some("rom") => DroppedMediaKind::Rom,
        // Every shape `package` accepts, or a dropped zip would be taken
        // for a disk image and handed to the floppy bay.
        Some(ext)
            if crate::package::EXTENSIONS.contains(&ext)
                || crate::package::SLAVE_EXTENSIONS.contains(&ext) =>
        {
            DroppedMediaKind::WhdloadGame
        }
        _ => DroppedMediaKind::Floppy,
    }
}

/// A WHDLoad game path as the configuration stores it. Picking or dropping a
/// bare `.slave` means its directory (an already-extracted package), which is
/// what `whdload::prepare` mounts; an `.lha` archive or a directory is taken
/// as given.
fn whdload_game_config_path(path: PathBuf) -> PathBuf {
    let is_slave = path
        .file_name()
        .is_some_and(|name| crate::package::is_slave_name(&name.to_string_lossy()));
    if is_slave {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            return dir.to_path_buf();
        }
    }
    path
}

/// Shorten any filesystem path in a status message so its file name stays
/// visible: a long path keeps its final component behind a "..." prefix instead
/// of running past the panel. Windows and Unix paths both work.
///
/// `anyhow`'s alternate Display joins the error chain with `": "`, and a path
/// sits at the end of its segment (`reading ROM <path>`), so each segment is
/// clipped as one span. That keeps paths containing spaces intact instead of
/// splitting them into several fragments.
fn shorten_status_paths(msg: &str) -> String {
    // Enough for the file name plus a directory or two, leaving room for the
    // cause after it; the status line holds roughly eighty characters.
    const MAX_PATH_CHARS: usize = 28;
    msg.split(": ")
        .map(|segment| {
            let Some(sep) = segment.find(['/', '\\']) else {
                return segment.to_string();
            };
            // The path runs from the token holding the first separator (back up
            // to the preceding space) to the end of the segment.
            let start = segment[..sep].rfind(' ').map_or(0, |i| i + 1);
            let (prose, path) = segment.split_at(start);
            format!("{prose}{}", ui::clip_path_to_chars(path, MAX_PATH_CHARS))
        })
        .collect::<Vec<_>>()
        .join(": ")
}

/// A one-line, length-bounded form of an error for the configuration panel's
/// status line. `{:#}` walks the whole chain, so the cause is kept: showing
/// only the outermost context turned "reading ROM <path>" into what looked like
/// a progress message when the ROM was simply not there. Paths are shortened to
/// their file name and the first letter capitalised so it reads as a sentence
/// instead of trailing off past the edge of the panel.
fn short_status_error(err: &anyhow::Error) -> String {
    let msg = format!("{err:#}");
    let first_line = msg.lines().next().unwrap_or("").trim();
    let shortened = shorten_status_paths(first_line);
    let mut chars = shortened.chars();
    let sentence = match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    };
    sentence.chars().take(96).collect()
}

fn set_mouse_button(emu: &mut Emulator, port: u8, button: MouseButtonKind, pressed: bool) {
    let index = match button {
        MouseButtonKind::Left => 0,
        MouseButtonKind::Right => 1,
        MouseButtonKind::Middle => 2,
    };
    emu.bus_mut()
        .input
        .set_mouse_button(port as usize, index, pressed);
    // Reverse-debug: note the transition so replay can reproduce it.
    emu.tt_note_input(crate::inputsched::ReplayAction::MouseButton {
        port,
        index,
        pressed,
    });
}

impl Rect {
    pub(super) fn contains(self, pos: (i32, i32)) -> bool {
        let (x, y) = pos;
        x >= self.x as i32
            && y >= self.y as i32
            && x < (self.x + self.w) as i32
            && y < (self.y + self.h) as i32
    }
}

pub(super) use crate::video::present_common::rgba;

struct EmbeddedRgbaImage {
    width: usize,
    height: usize,
    rgba: Vec<u8>,
}

fn copperline_logo_image() -> Option<&'static EmbeddedRgbaImage> {
    static LOGO: OnceLock<Option<EmbeddedRgbaImage>> = OnceLock::new();
    LOGO.get_or_init(|| match decode_embedded_png(COPPERLINE_LOGO_PNG) {
        Ok(image) => Some(image),
        Err(e) => {
            warn!("embedded Copperline logo decode failed: {e:#}");
            None
        }
    })
    .as_ref()
}

fn copperline_icon_image() -> Option<&'static EmbeddedRgbaImage> {
    static ICON: OnceLock<Option<EmbeddedRgbaImage>> = OnceLock::new();
    ICON.get_or_init(|| match decode_embedded_png(window_icon_png()) {
        Ok(image) => Some(image),
        Err(e) => {
            warn!("embedded window icon decode failed: {e:#}");
            None
        }
    })
    .as_ref()
}

/// The PNG behind the window and dock icon: Copperline's own, unless a
/// player build adopted the game's through [`crate::video::set_branding`].
fn window_icon_png() -> &'static [u8] {
    crate::video::branding_icon_png().unwrap_or(COPPERLINE_ICON_PNG)
}

/// Set the macOS dock/application icon from the embedded PNG.
///
/// winit's `with_window_icon` is ignored on macOS (the title bar has no icon
/// and the dock icon comes from the app bundle or `NSApplication`), so a bare
/// `target/release/copperline` run otherwise shows the generic executable icon.
/// `NSImage` decodes the PNG itself, so we hand it the embedded bytes directly.
/// Runs once; repeated `resumed` events do not re-decode.
#[cfg(target_os = "macos")]
fn set_macos_dock_icon() {
    use objc2::{AnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;
    use std::sync::Once;

    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // setApplicationIconImage must be touched on the main thread; the winit
        // event loop calls resumed there, but guard anyway.
        let Some(mtm) = MainThreadMarker::new() else {
            warn!("skipping macOS dock icon: not on the main thread");
            return;
        };
        let data = NSData::with_bytes(window_icon_png());
        match NSImage::initWithData(NSImage::alloc(), &data) {
            Some(image) => {
                let app = NSApplication::sharedApplication(mtm);
                // SAFETY: FFI into AppKit; `image` is a valid NSImage and the
                // call only borrows it for the duration of the message send.
                unsafe { app.setApplicationIconImage(Some(&image)) };
            }
            None => warn!("macOS dock icon: NSImage rejected the embedded PNG"),
        }
    });
}

fn copperline_window_icon() -> Option<Icon> {
    let image = copperline_icon_image()?;
    match Icon::from_rgba(image.rgba.clone(), image.width as u32, image.height as u32) {
        Ok(icon) => Some(icon),
        Err(e) => {
            warn!("embedded Copperline icon rejected by window system: {e}");
            None
        }
    }
}

fn decode_embedded_png(bytes: &[u8]) -> Result<EmbeddedRgbaImage> {
    let decoder = png::Decoder::new(Cursor::new(bytes));
    let mut reader = decoder.read_info()?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| anyhow!("PNG dimensions overflow"))?;
    let mut buf = vec![0; size];
    let info = reader.next_frame(&mut buf)?;
    let width = info.width as usize;
    let height = info.height as usize;
    let src = &buf[..info.buffer_size()];
    let rgba = match (info.color_type, info.bit_depth) {
        (png::ColorType::Rgba, png::BitDepth::Eight) => src.to_vec(),
        (png::ColorType::Rgb, png::BitDepth::Eight) => {
            let mut out = Vec::with_capacity(width * height * 4);
            for px in src.chunks_exact(3) {
                out.extend_from_slice(&[px[0], px[1], px[2], 0xFF]);
            }
            out
        }
        (color, depth) => {
            anyhow::bail!("unsupported PNG format: {color:?} {depth:?}");
        }
    };
    if rgba.len() != width * height * 4 {
        anyhow::bail!(
            "decoded PNG size mismatch: got {} bytes, expected {}x{}x4",
            rgba.len(),
            width,
            height
        );
    }
    Ok(EmbeddedRgbaImage {
        width,
        height,
        rgba,
    })
}

impl App {
    fn set_output_volume_from_pos(&mut self, pos: (i32, i32)) {
        self.emu
            .bus_mut()
            .set_output_volume_percent(volume_percent_from_pos(pos));
        self.request_redraw();
    }

    fn adjust_output_volume(&mut self, delta: i16) {
        self.emu.bus_mut().adjust_output_volume_percent(delta);
        self.request_redraw();
    }

    /// Whether this drive is backed by a real one on a bridge.
    fn drive_is_bridged(&self, idx: usize) -> bool {
        #[cfg(feature = "fluxbridge")]
        {
            self.emu.bus().floppy.is_bridged(idx)
        }
        #[cfg(not(feature = "fluxbridge"))]
        {
            let _ = idx;
            false
        }
    }

    /// Removable-media status for the bar controls: which drives exist,
    /// what is inserted, and whether a CD drive is fitted this session.
    fn media_bar(&self) -> MediaBar {
        let bus = self.emu.bus();
        let drives = std::array::from_fn(|idx| DriveBar {
            connected: bus.floppy.drive_connected(idx),
            inserted: bus.floppy.disk_inserted(idx),
            multi: self.disk_playlists[idx].len() > 1,
            #[cfg(feature = "fluxbridge")]
            bridged: bus.floppy.is_bridged(idx),
            #[cfg(not(feature = "fluxbridge"))]
            bridged: false,
        });
        let cd = bus.cd_drive_present().then(|| bus.cd_disc_inserted());
        MediaBar { drives, cd }
    }

    fn main_redraw_state(&mut self) -> MainRedrawState {
        let mut status = self.emu.bus().front_panel_status();
        if status.fdd_track.is_none() {
            status.fdd_track = self.last_fdd_track;
        }
        let control_connected = {
            #[allow(unused_mut)]
            let mut connected = false;
            #[cfg(feature = "control")]
            {
                connected |= self.control.as_ref().is_some_and(|c| c.handle.connected());
            }
            #[cfg(feature = "gdb")]
            {
                connected |= self.gdb.as_ref().is_some_and(|g| g.handle.connected());
            }
            connected
        };
        #[cfg(feature = "mt32")]
        let mt32_face = if crate::video::mt32_panel_shown() {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            self.emu
                .bus_mut()
                .midi_serial_mut()
                .and_then(crate::midi::MidiSerialSink::mt32_mut)
                .map(|mt32| mt32.synth_mut().display_raw())
                .hash(&mut h);
            h.finish()
        } else {
            0
        };
        MainRedrawState {
            status,
            media: self.media_bar(),
            powered_on: self.powered_on,
            paused: self.paused,
            joystick_input_mode: self.joystick_input_mode,
            control_connected,
            recording: self.recorder.is_some(),
            input_recording: self.input_recorder.is_some(),
            warp: !self.emu.paced(),
            perf_revision: if self.perf_overlay {
                self.perf.revision
            } else {
                0
            },
            #[cfg(feature = "mt32")]
            mt32_face,
        }
    }

    fn main_ui_control_at(&self, pos: (i32, i32)) -> Option<UiControl> {
        self.ui.control_at(pos)
    }

    fn main_ui_hover_changed(
        &self,
        previous: Option<(i32, i32)>,
        current: Option<(i32, i32)>,
    ) -> bool {
        previous.and_then(|pos| self.main_ui_control_at(pos))
            != current.and_then(|pos| self.main_ui_control_at(pos))
    }

    /// Whether main-window UI is claiming the keyboard and pointer: the
    /// menu, or an overlay panel drawn over the display. The debugger,
    /// frame analyzer, and console are separate tool windows with their
    /// own event routing, so they are deliberately not modal here -- while
    /// one is open (even mid-run), the main window keeps driving the Amiga.
    fn modal_ui_active(&self) -> bool {
        self.ui.active()
    }

    /// Whether this session offers save states: always in the full build; in
    /// a player, only when the game's manifest opted in -- some titles treat
    /// quick saves as part of the game and some as cheating.
    fn save_states_allowed(&self) -> bool {
        !crate::video::player_profile() || crate::video::player_save_states()
    }

    /// Write the player's end-user settings through to `settings.toml` in
    /// the per-game config directory.
    ///
    /// The full build keeps its session-only menu semantics -- settings
    /// belong to the config file the user points at. A player has no config
    /// file for its user to edit, so what they set in the menu is what they
    /// expect to find on the next launch. The file holds the complete set
    /// of player-controlled settings as an ordinary configuration fragment,
    /// which the player's startup layers over the manifest's defaults. A
    /// no-op outside the player profile, and a write failure costs the
    /// persistence, not the session.
    fn persist_player_prefs(&self) {
        if !crate::video::player_profile() {
            return;
        }
        let Some(path) = crate::paths::config_file(crate::config::PLAYER_SETTINGS_FILE) else {
            return;
        };
        let mut raw = crate::config::RawConfig::default();
        raw.display.pixel_aspect =
            Some(super::launcher::pixel_aspect_name(crate::video::pixel_aspect()).to_string());
        raw.display.scaling = Some(
            super::launcher::display_scaling_name(crate::video::display_scaling()).to_string(),
        );
        raw.display.menu_scale = Some(crate::video::menu_scale().label().to_string());
        // A custom shader is named by its path, which the player menu never
        // offers; skipping it keeps whatever the manifest said.
        if self.crt_shader_kind != crate::config::ShaderKind::Custom {
            raw.display.shader = Some(self.crt_shader_kind.label().to_string());
        }
        raw.display.shader_strength = Some(self.shader_strength);
        raw.display.tint = Some(super::launcher::tint_name(self.tint).to_string());
        raw.display.bezel = Some(crate::config::RawBezel::Named(self.bezel.label().into()));
        raw.display.tv_h_centre = Some(self.tv_centre.h);
        raw.display.tv_v_centre = Some(self.tv_centre.v);
        raw.display.full_screen = Some(
            self.render
                .as_ref()
                .is_some_and(|r| r.window.fullscreen().is_some()),
        );
        raw.input.port1 = Some(self.emu.bus().input.device(0).label().to_string());
        raw.input.port2 = Some(self.emu.bus().input.device(1).label().to_string());
        raw.input.joystick = Some(self.joystick_input_mode.label().to_string());
        raw.input.autofire_hz = Some(self.autofire_hz);
        match &self.audio_output {
            crate::audio::AudioOutput::Default => {}
            crate::audio::AudioOutput::Device(name) => raw.audio.output_device = Some(name.clone()),
            crate::audio::AudioOutput::Disabled => raw.audio.output_enabled = Some(false),
        }
        raw.audio.audio_filter = Some(
            match self.emu.bus().paula.led_filter_mode() {
                crate::config::AudioFilterMode::Auto => "auto",
                crate::config::AudioFilterMode::On => "on",
                crate::config::AudioFilterMode::Off => "off",
            }
            .to_string(),
        );
        let written = raw.to_toml_string().and_then(|text| {
            crate::paths::ensure_parent(&path)?;
            std::fs::write(&path, text)?;
            Ok(())
        });
        if let Err(e) = written {
            warn!("player settings not saved to {}: {e:#}", path.display());
        }
    }

    /// Whether any UI surface has a claim on the host cursor: main-window
    /// UI, or an open tool window. Tool windows are not modal over the
    /// main window's input, but an automatic grab taken while one is open
    /// would trap the cursor its controls need, so the auto-capture paths
    /// wait until the last of them closes. An explicit capture (a display
    /// click or the shortcut) is always honoured.
    fn ui_wants_cursor(&self) -> bool {
        self.ui.active()
            || ToolPanelKind::ALL
                .into_iter()
                .any(|kind| self.tool_panel_is_open(kind))
    }

    fn tool_window(&self, kind: ToolPanelKind) -> Option<&ToolWindow> {
        match kind {
            ToolPanelKind::Debugger => self.debugger_tool_window.as_ref(),
            ToolPanelKind::FrameAnalyzer => self.frame_analyzer_tool_window.as_ref(),
            ToolPanelKind::Console => self.console_tool_window.as_ref(),
        }
    }

    fn tool_window_mut(&mut self, kind: ToolPanelKind) -> Option<&mut ToolWindow> {
        match kind {
            ToolPanelKind::Debugger => self.debugger_tool_window.as_mut(),
            ToolPanelKind::FrameAnalyzer => self.frame_analyzer_tool_window.as_mut(),
            ToolPanelKind::Console => self.console_tool_window.as_mut(),
        }
    }

    fn tool_window_slot(&mut self, kind: ToolPanelKind) -> &mut Option<ToolWindow> {
        match kind {
            ToolPanelKind::Debugger => &mut self.debugger_tool_window,
            ToolPanelKind::FrameAnalyzer => &mut self.frame_analyzer_tool_window,
            ToolPanelKind::Console => &mut self.console_tool_window,
        }
    }

    fn tool_panel_for_kind(&self, kind: ToolPanelKind) -> Option<Panel> {
        match kind {
            ToolPanelKind::Debugger => self
                .debugger_panel
                .as_ref()
                .map(|panel| Panel::Debugger(panel.clone())),
            ToolPanelKind::FrameAnalyzer => self
                .frame_analyzer_panel
                .as_ref()
                .map(|panel| Panel::FrameAnalyzer(panel.clone())),
            ToolPanelKind::Console => self
                .console_panel
                .as_ref()
                .map(|panel| Panel::Console(panel.clone())),
        }
    }

    fn tool_panel_control_at(&self, kind: ToolPanelKind, pos: (i32, i32)) -> Option<UiControl> {
        self.tool_panel_for_kind(kind)
            .as_ref()
            .and_then(|panel| ui::panel_control_at(panel, pos))
    }

    fn tool_hover_changed(
        &self,
        kind: ToolPanelKind,
        previous: Option<(i32, i32)>,
        current: Option<(i32, i32)>,
    ) -> bool {
        previous.and_then(|pos| self.tool_panel_control_at(kind, pos))
            != current.and_then(|pos| self.tool_panel_control_at(kind, pos))
    }

    fn tool_window_kind(&self, window_id: WindowId) -> Option<ToolPanelKind> {
        ToolPanelKind::ALL.into_iter().find(|&kind| {
            self.tool_window(kind)
                .is_some_and(|tool| tool.window.id() == window_id)
        })
    }

    fn ui_key_accepts_repeat(&self, kind: Option<ToolPanelKind>, code: KeyCode) -> bool {
        match kind {
            Some(ToolPanelKind::FrameAnalyzer) => matches!(
                code,
                KeyCode::ArrowLeft | KeyCode::ArrowRight | KeyCode::ArrowUp | KeyCode::ArrowDown
            ),
            // A command line wants held-key repeat for typing and editing.
            Some(ToolPanelKind::Console) => true,
            #[cfg(feature = "game-library")]
            _ => self.launcher_accepts_repeat(code),
            #[cfg(not(feature = "game-library"))]
            _ => false,
        }
    }

    /// Whether a held key should keep arriving on a launcher page.
    ///
    /// The Library list walks with the arrows and speeds up while they are
    /// held, which needs the repeats to reach it -- without this, holding
    /// one does nothing at all. The sign-in dialog is a text field, where
    /// held Backspace clearing a box is what anyone would expect.
    #[cfg(feature = "game-library")]
    fn launcher_accepts_repeat(&self, code: KeyCode) -> bool {
        use crate::video::launcher::LauncherTab;
        let Some(state) = self.launcher_state() else {
            return false;
        };
        if state.login.is_some() {
            return true;
        }
        state.tab == LauncherTab::WhdloadLibrary
            && matches!(
                code,
                KeyCode::ArrowUp | KeyCode::ArrowDown | KeyCode::Home | KeyCode::End
            )
    }

    fn should_drop_repeated_main_key(
        &self,
        code: KeyCode,
        state: ElementState,
        repeat: bool,
    ) -> bool {
        repeated_main_key_should_drop(
            &self.held_rawkeys,
            code,
            state,
            repeat,
            self.ui_key_accepts_repeat(None, code),
        )
    }

    fn activate_ui_control_with_event_loop(
        &mut self,
        control: UiControl,
        event_loop: Option<&ActiveEventLoop>,
    ) {
        match control {
            // A row of the tree menu: a category opens, a leaf acts. The
            // pointer can land on any level, so the path is set to where it
            // landed rather than stepped into.
            UiControl::MenuRow { depth, row } => {
                let path = self.menu_path_to(depth);
                self.ui.menu_nav.open_path(path, Some(row));
                self.activate_menu_row(event_loop);
            }
            UiControl::RemapSet(set) => self.input_map_select_mapping(set),
            UiControl::RemapBind(index) => self.input_map_arm_capture(index),
            UiControl::RemapClear(index) => self.input_map_clear(index),
            UiControl::RemapDefaults => self.input_map_defaults(),
            UiControl::RemapSave => self.input_map_save(),
            UiControl::PanelClose | UiControl::CalCancel => self.close_panel(),
            UiControl::PanelBody => {}
            UiControl::CalSkip => {
                if let Some(Panel::Calibration(session)) = self.ui.panel.as_mut() {
                    session.skip_current();
                }
            }
            UiControl::CalSave => self.save_calibration(),
            UiControl::DebugTab(tab) => {
                if let Some(panel) = self.debugger_panel.as_mut() {
                    panel.tab = tab;
                }
            }
            UiControl::DebugRun => self.activate_tool_control(ToolPanelKind::Debugger, control),
            UiControl::DebugStep => self.activate_tool_control(ToolPanelKind::Debugger, control),
            UiControl::DebugStepOver => {
                self.activate_tool_control(ToolPanelKind::Debugger, control)
            }
            UiControl::DebugStepOut => self.activate_tool_control(ToolPanelKind::Debugger, control),
            UiControl::DebugStepFrame => {
                self.activate_tool_control(ToolPanelKind::Debugger, control)
            }
            UiControl::DebugRunTo => self.activate_tool_control(ToolPanelKind::Debugger, control),
            UiControl::DebugRunLine => self.activate_tool_control(ToolPanelKind::Debugger, control),
            UiControl::DebugReverseStep => {
                self.activate_tool_control(ToolPanelKind::Debugger, control)
            }
            UiControl::DebugReverseFrame => {
                self.activate_tool_control(ToolPanelKind::Debugger, control)
            }
            UiControl::DebugReverseRun => {
                self.activate_tool_control(ToolPanelKind::Debugger, control)
            }
            UiControl::DebugMemPrev => self.activate_tool_control(ToolPanelKind::Debugger, control),
            UiControl::DebugMemNext => self.activate_tool_control(ToolPanelKind::Debugger, control),
            UiControl::DebugPoke => self.activate_tool_control(ToolPanelKind::Debugger, control),
            UiControl::DebugEntry => {
                if let Some(panel) = self.debugger_panel.as_mut() {
                    panel.entry_active = true;
                }
            }
            UiControl::DebugBreakToggle => {
                self.activate_tool_control(ToolPanelKind::Debugger, control)
            }
            UiControl::DebugWatchToggle => {
                self.activate_tool_control(ToolPanelKind::Debugger, control)
            }
            UiControl::DebugRegToggle => {
                self.activate_tool_control(ToolPanelKind::Debugger, control)
            }
            UiControl::DebugBeamToggle => {
                self.activate_tool_control(ToolPanelKind::Debugger, control)
            }
            UiControl::DebugCatchToggle => {
                self.activate_tool_control(ToolPanelKind::Debugger, control)
            }
            UiControl::DebugCopperBreakToggle => {
                self.activate_tool_control(ToolPanelKind::Debugger, control)
            }
            UiControl::DebugCopperStep => {
                self.activate_tool_control(ToolPanelKind::Debugger, control)
            }
            UiControl::DebugMemFind => self.activate_tool_control(ToolPanelKind::Debugger, control),
            UiControl::DebugMemSave => self.activate_tool_control(ToolPanelKind::Debugger, control),
            UiControl::DebugMemWriter => {
                self.activate_tool_control(ToolPanelKind::Debugger, control)
            }
            UiControl::DebugMemBits => self.activate_tool_control(ToolPanelKind::Debugger, control),
            UiControl::DebugPlaneToggle(_) => {
                self.activate_tool_control(ToolPanelKind::Debugger, control)
            }
            UiControl::DebugSpriteToggle(_) => {
                self.activate_tool_control(ToolPanelKind::Debugger, control)
            }
            UiControl::DebugBreaksClear => {
                self.activate_tool_control(ToolPanelKind::Debugger, control)
            }
            UiControl::DebugWaveArm => self.activate_tool_control(ToolPanelKind::Debugger, control),
            UiControl::DebugWaveStop => {
                self.activate_tool_control(ToolPanelKind::Debugger, control)
            }
            UiControl::DebugAudioMute(_) => {
                self.activate_tool_control(ToolPanelKind::Debugger, control)
            }
            UiControl::AnalyzerRun => {
                self.activate_tool_control(ToolPanelKind::FrameAnalyzer, control)
            }
            UiControl::AnalyzerFrame => {
                self.activate_tool_control(ToolPanelKind::FrameAnalyzer, control)
            }
            UiControl::AnalyzerUnderlay => {
                self.activate_tool_control(ToolPanelKind::FrameAnalyzer, control)
            }
            UiControl::AnalyzerScrub => {
                self.activate_tool_control(ToolPanelKind::FrameAnalyzer, control)
            }
            UiControl::AnalyzerRunTo => {
                self.activate_tool_control(ToolPanelKind::FrameAnalyzer, control)
            }
            UiControl::AnalyzerPick { x, y, scanline } => {
                self.frame_analyzer_select(x, y, scanline)
            }
            UiControl::LauncherModel(model) => {
                if let Some(state) = self.launcher_state_mut() {
                    state.setup.select_model(Some(model));
                    state.status = None;
                }
            }
            // Either way in goes to the same page.
            UiControl::LauncherTab(tab) | UiControl::LauncherNavTab(tab) => {
                if let Some(state) = self.launcher_state_mut() {
                    state.tab = tab;
                    // A message reports what the page just did, so it belongs
                    // to that page: leaving clears it.
                    state.status = None;
                    // Opening the Host Disk page is the moment to look at the
                    // host's storage: a card pushed in since the launcher
                    // opened should be there when the page is.
                    if tab == crate::video::launcher::LauncherTab::HostDisk {
                        state.setup.refresh_host_disks();
                    }
                    // Likewise the Library: a package added since the
                    // launcher opened should be in the list when the page
                    // is, and the database may have been synced meanwhile.
                    #[cfg(feature = "game-library")]
                    if tab == crate::video::launcher::LauncherTab::WhdloadLibrary {
                        let at = crate::paths::library_root();
                        state.library.games = crate::gamelib::Library::default();
                        state.refresh_library(&at);
                    }
                }
            }
            UiControl::LauncherCycle { field, forward } => {
                if let Some(state) = self.launcher_state_mut() {
                    // Reaching for another control ends the typing, the way
                    // Enter does: what is in the box counts. A value the
                    // commit refuses keeps the focus and blocks the click --
                    // cycling on regardless could hide the very row being
                    // edited (the serial mode carries its address box).
                    state.edit_commit();
                    if state.editing().is_none() {
                        if field.is_netplay() {
                            state.netplay.cycle(field, forward);
                        } else if LauncherState::is_workshop(field) {
                            state.workshop_cycle(field, forward);
                        } else {
                            state.setup.cycle(field, forward);
                        }
                        state.status = None;
                    }
                }
                // Which machine a WHDLoad game boots on is the one cycle
                // here whose two settings look alike from the outside: both
                // say a word, and neither word says what it does.
                self.say_whdload_machine(field);
            }
            UiControl::LauncherToggle(field) => {
                if let Some(state) = self.launcher_state_mut() {
                    state.edit_commit();
                    if state.editing().is_none() {
                        if field == LauncherField::NetplayEnabled {
                            state.toggle_netplay();
                        } else if LauncherState::is_workshop(field) {
                            state.workshop_toggle_flip(field);
                        } else {
                            state.setup.toggle(field);
                        }
                        state.status = None;
                    }
                }
            }
            UiControl::LauncherClear(field) => {
                if let Some(state) = self.launcher_state_mut() {
                    state.edit_cancel();
                    if field == LauncherField::FmvRom {
                        state.setup.toggle_fmv_module();
                    } else {
                        state.setup.clear_path(field);
                    }
                    state.status = None;
                }
            }
            UiControl::LauncherDriveNameEdit(field) => {
                if let Some(state) = self.launcher_state_mut() {
                    state.begin_edit_drive_name(field);
                }
            }
            UiControl::LauncherDriveFilesystemToggle(field) => {
                if let Some(state) = self.launcher_state_mut() {
                    state.setup.cycle_drive_filesystem(field);
                    state.status = None;
                }
            }
            UiControl::LauncherNewImageEdit(field) => {
                if let Some(state) = self.launcher_state_mut() {
                    state.begin_edit_new_image(field);
                }
            }
            UiControl::LauncherSerialHostEdit(field) | UiControl::LauncherSerialPortEdit(field) => {
                if let Some(state) = self.launcher_state_mut() {
                    // Reaching for the other address box ends the typing in
                    // this one, the way Enter does; a value it refuses
                    // keeps the focus where the mistake is.
                    state.edit_commit();
                    if state.editing().is_none() {
                        state.begin_edit_serial_addr(
                            field,
                            matches!(control, UiControl::LauncherSerialPortEdit(_)),
                        );
                    }
                }
            }
            UiControl::LauncherRamPatternEdit => {
                if let Some(state) = self.launcher_state_mut() {
                    state.edit_commit();
                    if state.editing().is_none() {
                        state.begin_edit_ram_pattern();
                    }
                }
            }
            UiControl::LauncherNetplayEdit(field) => {
                if let Some(state) = self.launcher_state_mut() {
                    state.edit_commit();
                    if state.editing().is_none() {
                        state.begin_edit_netplay(field);
                    }
                }
            }
            UiControl::LauncherNetplayAction(field) => self.launcher_netplay_action(field),
            UiControl::LauncherNewImageCreate(field) => self.launcher_create_image(field),
            #[cfg(feature = "game-library")]
            UiControl::LauncherWhdloadDownload(field) => self.whdload_download(field),
            #[cfg(feature = "game-library")]
            UiControl::LauncherLibraryScroll(delta) => {
                // How many rows are on screen comes from the panel's own
                // size, so the scroll stops where the list does.
                let whdload_entry = self
                    .launcher_state()
                    .is_some_and(|state| state.setup.whdload_enabled());
                let visible = crate::video::ui::launcher_panel_rect(&self.ui)
                    .map(|rect| crate::video::ui::library_visible_rows(rect, whdload_entry))
                    .unwrap_or(1);
                if let Some(state) = self.launcher_state_mut() {
                    state.scroll_library(delta, visible);
                }
            }
            #[cfg(feature = "game-library")]
            UiControl::LauncherLibraryPick(drawn) => {
                if let Some(state) = self.launcher_state_mut() {
                    let at = state.library.scroll + drawn;
                    state.select_library_game(at);
                }
            }
            #[cfg(feature = "game-library")]
            UiControl::LauncherLibraryFavourite(drawn) => {
                let config = crate::paths::library_root();
                if let Some(state) = self.launcher_state_mut() {
                    let at = state.library.scroll + drawn;
                    state.toggle_library_favourite(at);
                    // Written out as it changes, so a favourite survives a
                    // session that ends any way at all.
                    state.save_library_database(&config);
                }
            }
            #[cfg(feature = "game-library")]
            UiControl::LauncherLibraryFavouritePick(drawn) => {
                if let Some(state) = self.launcher_state_mut() {
                    let at = state.library.favourite_scroll + drawn;
                    state.select_favourite(at);
                }
            }
            #[cfg(feature = "game-library")]
            UiControl::LauncherLibraryFavouriteRemove(drawn) => {
                let config = crate::paths::library_root();
                if let Some(state) = self.launcher_state_mut() {
                    let at = state.library.favourite_scroll + drawn;
                    state.remove_favourite(at);
                    state.save_library_database(&config);
                }
            }
            #[cfg(feature = "game-library")]
            UiControl::LauncherLibraryJump(bucket) => {
                let whdload_entry = self
                    .launcher_state()
                    .is_some_and(|state| state.setup.whdload_enabled());
                let visible = crate::video::ui::launcher_panel_rect(&self.ui)
                    .map(|rect| crate::video::ui::library_visible_rows(rect, whdload_entry))
                    .unwrap_or(1);
                if let Some(state) = self.launcher_state_mut() {
                    state.jump_to_bucket(bucket, visible);
                }
            }
            #[cfg(feature = "game-library")]
            UiControl::LauncherLibraryFavouriteScroll(delta) => {
                let whdload_entry = self
                    .launcher_state()
                    .is_some_and(|state| state.setup.whdload_enabled());
                let visible = crate::video::ui::launcher_panel_rect(&self.ui)
                    .map(|rect| crate::video::ui::library_favourite_rows(rect, whdload_entry))
                    .unwrap_or(1);
                if let Some(state) = self.launcher_state_mut() {
                    state.scroll_favourites(delta, visible);
                }
            }
            #[cfg(feature = "game-library")]
            UiControl::LauncherLibraryRefresh => self.library_refresh(),
            #[cfg(feature = "game-library")]
            UiControl::LauncherLibraryUpdate => self.library_update_metadata(),
            #[cfg(feature = "game-library")]
            UiControl::LauncherOpenRetroLogin => self.openretro_login_or_out(),
            #[cfg(feature = "game-library")]
            UiControl::LoginField(field) => {
                if let Some(login) = self.launcher_login_mut() {
                    login.focus_on(field);
                }
            }
            #[cfg(feature = "game-library")]
            UiControl::LauncherLibraryEdit => {
                if let Some(state) = self.launcher_state_mut() {
                    state.edit_commit();
                    if !state.open_meta_editor() {
                        state.status = Some(StatusMessage::err("Choose a game first"));
                    }
                }
                self.request_redraw();
            }
            #[cfg(feature = "game-library")]
            UiControl::MetaField(field) => {
                if let Some(meta) = self.launcher_meta_mut() {
                    meta.focus_on(field);
                }
            }
            #[cfg(feature = "game-library")]
            UiControl::MetaArt => self.meta_choose_art(),
            #[cfg(feature = "game-library")]
            UiControl::MetaSave => self.meta_save(),
            #[cfg(feature = "game-library")]
            UiControl::MetaClear => {
                if let Some(meta) = self.launcher_meta_mut() {
                    // Only in the editor: nothing is lost until Save.
                    meta.values = Default::default();
                    meta.art = None;
                }
                self.request_redraw();
            }
            #[cfg(feature = "game-library")]
            UiControl::MetaCancel => {
                if let Some(state) = self.launcher_state_mut() {
                    state.meta = None;
                }
                self.request_redraw();
            }
            #[cfg(feature = "game-library")]
            UiControl::LoginOk => self.login_submit(),
            #[cfg(feature = "game-library")]
            UiControl::LoginCancel => self.login_close(),
            UiControl::LauncherFsFamily { field, family } => {
                if let Some(state) = self.launcher_state_mut() {
                    state.edit_commit();
                    state.workshop_set_fs_family(field, family);
                    state.status = None;
                }
            }
            UiControl::LauncherFsVariant { field, variant } => {
                if let Some(state) = self.launcher_state_mut() {
                    state.edit_commit();
                    state.workshop_set_fs_variant(field, variant);
                    state.status = None;
                }
            }
            UiControl::LauncherNewImageUnit => {
                if let Some(state) = self.launcher_state_mut() {
                    // The size in the box is in the unit being left, so it
                    // has to be taken before the unit changes under it.
                    state.edit_commit();
                    state.workshop.flip_size_unit();
                    state.status = None;
                }
            }
            UiControl::LauncherGeometryAuto | UiControl::LauncherGeometryCustom => {
                let by_hand = control == UiControl::LauncherGeometryCustom;
                if let Some(state) = self.launcher_state_mut() {
                    state.edit_commit();
                    // Entering hand-set geometry starts from what Auto
                    // would have produced, so the figures are never blank
                    // and never disagree with the size.
                    if by_hand && !state.workshop.geometry_custom {
                        state.workshop.geometry_from_size();
                    }
                    state.workshop.geometry_custom = by_hand;
                    state.status = None;
                }
            }
            UiControl::LauncherDriveBootpriEdit(field) => {
                if let Some(state) = self.launcher_state_mut() {
                    state.begin_edit_drive_bootpri(field);
                }
            }
            UiControl::LauncherDriveBridgeToggle(bay) => {
                if let Some(state) = self.launcher_state_mut() {
                    state.edit_cancel();
                    let on = !state.setup.drive_bridged(bay);
                    state.setup.set_drive_bridged(bay, on);
                    state.status = None;
                }
            }
            UiControl::LauncherHostDiskSelect(index) | UiControl::LauncherHostDiskEnable(index) => {
                if let Some(state) = self.launcher_state_mut() {
                    state.setup.select_host_disk(index);
                    // A refused tick reports itself on the status line, where
                    // every other warning is looked for.
                    state.status = state.setup.host_disk_warning().map(StatusMessage::err);
                }
            }
            UiControl::LauncherHostDiskWritable(index) => {
                if let Some(state) = self.launcher_state_mut() {
                    state.setup.toggle_host_disk_writable(index);
                    state.status = None;
                }
            }
            UiControl::LauncherHostDiskAttach(index) => {
                if let Some(state) = self.launcher_state_mut() {
                    state.setup.cycle_host_disk_attach(index, true);
                    state.status = None;
                }
            }
            UiControl::LauncherHostDiskUnmount(field) => {
                if let Some(state) = self.launcher_state_mut() {
                    let attach = crate::video::launcher::MachineSetup::host_disk_attach_of(field);
                    let removed = attach.and_then(|a| state.setup.unmount_host_disk(a));
                    state.status = Some(match (removed, attach) {
                        (Some(device), Some(attach)) => {
                            // Off the machine and back to the host in one act.
                            // Mounting took the disk, so letting go has to
                            // close what that opened, or the host would be
                            // told it may have a disk this process still
                            // holds.
                            let released = crate::blockdev::release_device(&device);
                            log::info!("host disk: {device} taken off {}", attach.label());
                            StatusMessage::ok(if released {
                                format!("{device} released")
                            } else {
                                // The setting is gone either way, but a
                                // machine already running with this disk holds
                                // it in its own right, and saying "released"
                                // of a disk still locked is how somebody ends
                                // up wondering why they cannot have it back.
                                format!(
                                    "{device} released; a running machine keeps it until it stops"
                                )
                            })
                        }
                        _ => StatusMessage::err("Nothing to unmount"),
                    });
                }
            }
            UiControl::LauncherHostDiskUnmountSelected => {
                if let Some(state) = self.launcher_state_mut() {
                    let released = state.setup.unmount_selected_host_disks();
                    state.status = Some(if released.is_empty() {
                        StatusMessage::err("Nothing to unmount")
                    } else {
                        // Off the machine and back to the host in one act,
                        // exactly as the Storage rows' Unmount does it.
                        for device in &released {
                            crate::blockdev::release_device(device);
                            log::info!("host disk: {device} taken off the machine");
                        }
                        StatusMessage::ok(match released.as_slice() {
                            [device] => format!("{device} released"),
                            many => format!("{} released", many.join(", ")),
                        })
                    });
                }
            }
            UiControl::LauncherHostDiskScroll(delta) => {
                if let Some(state) = self.launcher_state_mut() {
                    state
                        .setup
                        .scroll_host_disks(delta, crate::video::ui::HOST_DISK_VISIBLE_ROWS);
                }
            }
            UiControl::LauncherHostDiskRefresh => {
                if let Some(state) = self.launcher_state_mut() {
                    state.setup.refresh_host_disks();
                    let found = state.setup.host_disks().len();
                    state.status = Some(StatusMessage::ok(match found {
                        0 => "No supported disks found on the host system".to_string(),
                        1 => "1 disk found".to_string(),
                        n => format!("{n} disks found"),
                    }));
                }
            }
            UiControl::LauncherHostDiskMount => {
                if let Some(state) = self.launcher_state_mut() {
                    // Mounting rearranges the machine -- a disk takes a slot,
                    // and whatever image was in it goes. If the host then
                    // refuses the disk, none of that should have happened, so
                    // the whole setup is put back rather than unpicked
                    // step by step and one step forgotten.
                    let before = state.setup.clone();
                    let status = match state.setup.mount_host_disks() {
                        Ok(disks) => {
                            // The host gives the disk up here, not when the
                            // machine starts. A real disk needs permission on
                            // some hosts, and this is where somebody has just
                            // asked for one -- a dialog minutes later, behind
                            // a machine booting, belongs to nothing they did.
                            // Every disk the machine is set up to have, not
                            // only the ones just ticked: this says which disks
                            // are wanted, and anything held that is not on the
                            // list goes back to the host.
                            let asked: Vec<(String, Option<String>, bool, bool)> = state
                                .setup
                                .host_disks_attached()
                                .iter()
                                .map(|disk| {
                                    (
                                        disk.device.clone(),
                                        disk.fingerprint.clone(),
                                        disk.writable,
                                        disk.identity_confirmed,
                                    )
                                })
                                .collect();
                            let refused = match crate::blockdev::reserve_devices(&asked) {
                                Ok(()) => {
                                    for disk in &disks {
                                        log::info!(
                                            "host disk: {} attached to {} ({})",
                                            disk.device,
                                            disk.attach.label(),
                                            if disk.writable {
                                                "read/write"
                                            } else {
                                                "read only"
                                            }
                                        );
                                    }
                                    None
                                }
                                Err(error) => {
                                    // The outermost sentence only. These are
                                    // written to be read, and flattening the
                                    // chain onto one line ends in a raw OS
                                    // code with the useful half cut out of the
                                    // middle. The log keeps all of it.
                                    log::warn!("host disk: not attached: {error:#}");
                                    Some(error.to_string())
                                }
                            };
                            match refused {
                                Some(reason) => {
                                    // Nothing stays attached that the host did
                                    // not give up, including the disks taken
                                    // before the one that failed: a machine
                                    // must not start expecting a disk that was
                                    // refused here.
                                    for disk in &disks {
                                        crate::blockdev::release_device(&disk.device);
                                    }
                                    state.setup = before;
                                    log::warn!("host disk: not attached: {reason}");
                                    StatusMessage::err(reason)
                                }
                                None => {
                                    let places: Vec<_> = disks.iter().map(|d| d.attach).collect();
                                    let where_to =
                                        crate::config::HostDiskAttach::describe_all(&places);
                                    StatusMessage::ok(if disks.len() == 1 {
                                        format!("Host disk attached to {where_to}")
                                    } else {
                                        format!("Host disks attached to {where_to}")
                                    })
                                }
                            }
                        }
                        Err(reason) => {
                            log::warn!("host disk: not attached: {reason}");
                            StatusMessage::err(reason)
                        }
                    };
                    state.status = Some(status);
                }
            }
            UiControl::LauncherBridgeConfigure(bay) => {
                if let Some(state) = self.launcher_state_mut() {
                    state.setup.set_bridge_edit_drive(bay);
                    state.tab = crate::video::launcher::LauncherTab::FluxBridge;
                    state.status = None;
                }
            }
            UiControl::LauncherDriveBootToggle(field) => {
                if let Some(state) = self.launcher_state_mut() {
                    state.edit_cancel();
                    state.setup.toggle_drive_boot(field);
                    state.status = None;
                }
            }
            UiControl::LauncherZorroRemove(idx) => {
                if let Some(state) = self.launcher_state_mut() {
                    state.edit_cancel();
                    state.setup.remove_zorro(idx);
                    state.status = None;
                }
            }
            UiControl::LauncherBoardCycle {
                board,
                opt,
                forward,
            } => {
                if let Some(state) = self.launcher_state_mut() {
                    state.edit_cancel();
                    state.setup.zorro_option_cycle(board, opt, forward);
                    state.status = None;
                }
            }
            UiControl::LauncherBoardToggle { board, opt } => {
                if let Some(state) = self.launcher_state_mut() {
                    state.edit_cancel();
                    state.setup.zorro_option_toggle(board, opt);
                    state.status = None;
                }
            }
            UiControl::LauncherBoardClear { board, opt } => {
                if let Some(state) = self.launcher_state_mut() {
                    state.edit_cancel();
                    state.setup.zorro_option_clear(board, opt);
                    state.status = None;
                }
            }
            UiControl::LauncherBoardEdit { board, opt } => {
                if let Some(state) = self.launcher_state_mut() {
                    state.begin_edit_board(board, opt);
                }
            }
            UiControl::LauncherBoardBrowse { board, opt } => self.launcher_board_browse(board, opt),
            UiControl::LauncherDefaults => {
                if let Some(state) = self.launcher_state_mut() {
                    let model = state.setup.model();
                    state.setup = MachineSetup::default();
                    state.setup.select_model(model);
                    // The defaults have no second boot page to stand on.
                    state.tab = state.setup.settle_tab(state.tab);
                    state.setup.refresh_host_devices();
                    state.status = Some(StatusMessage::ok("Reset to defaults"));
                }
            }
            UiControl::LauncherBrowse(field) => self.launcher_browse(field),
            UiControl::LauncherZorroAdd => self.launcher_add_zorro(),
            UiControl::LauncherLoad => self.launcher_load(),
            UiControl::LauncherSave => self.launcher_toggle_save_dialog(),
            UiControl::LauncherSaveAs => self.launcher_save(),
            UiControl::LauncherSaveDefault => self.launcher_save_default(),
            UiControl::LauncherResetDefault => self.launcher_reset_default(),
            UiControl::LauncherConfirmReset => self.launcher_reset_default_confirmed(),
            UiControl::LauncherCancelReset => self.launcher_close_confirm(),
            // Whichever is up. Only one ever is: opening the confirm puts
            // the Save dialog away rather than stacking on it.
            UiControl::LauncherDialogClose => {
                self.launcher_close_confirm();
                self.launcher_close_save_dialog();
            }
            UiControl::LauncherRun => self.launcher_run(),
            UiControl::DropDrive(drive_idx) => self.drop_chooser_route(drive_idx),
            UiControl::AnalyzerTab(_)
            | UiControl::AnalyzerHeatPreset(_)
            | UiControl::AnalyzerResourceRow(_)
            | UiControl::AnalyzerResourceSave
            | UiControl::AnalyzerBlitRow(_)
            | UiControl::AnalyzerCpuWait
            | UiControl::AnalyzerHeatPick { .. } => {
                self.activate_tool_control(ToolPanelKind::FrameAnalyzer, control)
            }
        }
        // A control may have put a different image in a ROM row (Browse,
        // Clear, Reset to defaults, Load configuration), so the ROM tab's
        // identification is brought up to date here, once per click, rather
        // than on the redraw below: it keys on the path and only opens a file
        // when a new one is chosen.
        if let Some(state) = self.launcher_state_mut() {
            state.sync_rom_notes();
        }
        self.request_redraw();
    }

    /// Console keyboard input: printable characters append to the command
    /// line; editing, history, and scrollback keys do the rest.
    fn ui_handle_console_key(&mut self, code: KeyCode) -> bool {
        if self.console_panel.is_none() {
            return false;
        }
        if matches!(code, KeyCode::Enter | KeyCode::NumpadEnter) {
            self.console_submit();
            self.request_redraw();
            return true;
        }
        let Some(panel) = self.console_panel.as_mut() else {
            return false;
        };
        if let Some(ch) = entry_char_for_key(code) {
            panel.push_input_char(ch);
            self.request_redraw();
            return true;
        }
        match code {
            KeyCode::Backspace => {
                panel.input.pop();
                panel.history_pos = None;
            }
            KeyCode::ArrowUp => panel.history_step(-1),
            KeyCode::ArrowDown => panel.history_step(1),
            KeyCode::PageUp => {
                panel.scroll = (panel.scroll + 10).min(ui::CONSOLE_SCROLLBACK_LINES);
            }
            KeyCode::PageDown => panel.scroll = panel.scroll.saturating_sub(10),
            _ => return false,
        }
        self.request_redraw();
        true
    }
}

mod app_debugger;
mod app_display;
mod app_input;
mod app_launcher;
mod app_media;
mod app_menus;
mod app_nav;
mod app_netplay;
use app_nav::{cycle_hold_delay, PadNav};
mod app_panels;
mod app_session;
mod bezel;
mod console;
#[cfg(feature = "control")]
mod control;
mod crt_shader;
#[cfg(feature = "coppersynth")]
mod csynthpanel;
#[cfg(feature = "gdb")]
mod gdb;
mod host_input;
mod kbdpanel;
#[cfg(feature = "mt32")]
mod mt32panel;
mod present;
mod rtg_texture;
mod scaler;
pub(in crate::video) mod statusbar;
mod stickers;
pub(super) use present::{scale_rect, texture_height, texture_width, Rect};
pub(super) use statusbar::{draw_rect_bevel, fill_rect, fill_rect_blend};

pub use host_input::parse_amiga_key;
use host_input::*;
use present::*;
use statusbar::*;

#[cfg(test)]
mod tests;
