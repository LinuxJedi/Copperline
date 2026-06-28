// SPDX-License-Identifier: GPL-3.0-or-later

//! winit + pixels integration. The emulator core runs synchronously on the
//! main thread inside `about_to_wait`; by default a worker renders the
//! completed frame while the main thread advances the next frame. winit and
//! wgpu presentation stay on the main thread.

use super::deinterlace::{Deinterlacer, OUT_HEIGHT};
use super::launcher::{LauncherField, LauncherState, MachineSetup, StatusMessage};
use super::ui::{self, Panel, UiControl, UiState};
use super::{
    bitplane, blend_rgba, font, FrameGeometry, FB_HEIGHT, FB_PIXELS, FB_WIDTH,
    HOST_SHORTCUT_MODIFIER_LABEL, MAX_FB_PIXELS, PRESENT_HEIGHT,
};
use crate::audio::{AudioSink, CpalSink};
use crate::bus::{
    BeamWriteSource, FrontPanelStatus, RenderRegisterSnapshot, VideoRenderFrameTiming,
};
use crate::config::{Config, Overscan, RawConfig, WarpSpeed};
use crate::emulator::Emulator;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyboardJoystickKey {
    Up,
    Down,
    Left,
    Right,
    FireRightCtrl,
    FireRightAlt,
    Red,
    Blue,
    Green,
    Yellow,
    Play,
    Rewind,
    Forward,
}

/// Host keys currently held for keyboard joystick emulation. Fire has
/// multiple aliases, so this tracks individual keys and resolves them to
/// one port state when applied.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct KeyboardJoystickHeld {
    up: bool,
    down: bool,
    left: bool,
    right: bool,
    fire_right_ctrl: bool,
    fire_right_alt: bool,
    red: bool,
    blue: bool,
    green: bool,
    yellow: bool,
    play: bool,
    rwd: bool,
    ffw: bool,
}

impl KeyboardJoystickHeld {
    fn set(&mut self, key: KeyboardJoystickKey, held: bool) {
        match key {
            KeyboardJoystickKey::Up => self.up = held,
            KeyboardJoystickKey::Down => self.down = held,
            KeyboardJoystickKey::Left => self.left = held,
            KeyboardJoystickKey::Right => self.right = held,
            KeyboardJoystickKey::FireRightCtrl => self.fire_right_ctrl = held,
            KeyboardJoystickKey::FireRightAlt => self.fire_right_alt = held,
            KeyboardJoystickKey::Red => self.red = held,
            KeyboardJoystickKey::Blue => self.blue = held,
            KeyboardJoystickKey::Green => self.green = held,
            KeyboardJoystickKey::Yellow => self.yellow = held,
            KeyboardJoystickKey::Play => self.play = held,
            KeyboardJoystickKey::Rewind => self.rwd = held,
            KeyboardJoystickKey::Forward => self.ffw = held,
        }
    }

    fn is_set(&self, key: KeyboardJoystickKey) -> bool {
        match key {
            KeyboardJoystickKey::Up => self.up,
            KeyboardJoystickKey::Down => self.down,
            KeyboardJoystickKey::Left => self.left,
            KeyboardJoystickKey::Right => self.right,
            KeyboardJoystickKey::FireRightCtrl => self.fire_right_ctrl,
            KeyboardJoystickKey::FireRightAlt => self.fire_right_alt,
            KeyboardJoystickKey::Red => self.red,
            KeyboardJoystickKey::Blue => self.blue,
            KeyboardJoystickKey::Green => self.green,
            KeyboardJoystickKey::Yellow => self.yellow,
            KeyboardJoystickKey::Play => self.play,
            KeyboardJoystickKey::Rewind => self.rwd,
            KeyboardJoystickKey::Forward => self.ffw,
        }
    }

    fn joystick_state(&self) -> crate::gamepad::JoystickState {
        crate::gamepad::JoystickState {
            up: self.up,
            down: self.down,
            left: self.left,
            right: self.right,
            fire: self.fire_right_ctrl || self.fire_right_alt || self.red,
            button2: self.blue,
            green: self.green,
            yellow: self.yellow,
            play: self.play,
            rwd: self.rwd,
            ffw: self.ffw,
        }
    }
}

/// FS-UAE-compatible keyboard joystick/gamepad emulation:
/// cursor keys for directions; Right Ctrl/Right Alt for fire; CD32 extras
/// on C/X/D/S/Return/Z/A.
fn keyboard_joystick_key_for(code: KeyCode) -> Option<KeyboardJoystickKey> {
    Some(match code {
        KeyCode::ArrowUp => KeyboardJoystickKey::Up,
        KeyCode::ArrowDown => KeyboardJoystickKey::Down,
        KeyCode::ArrowLeft => KeyboardJoystickKey::Left,
        KeyCode::ArrowRight => KeyboardJoystickKey::Right,
        KeyCode::ControlRight => KeyboardJoystickKey::FireRightCtrl,
        KeyCode::AltRight => KeyboardJoystickKey::FireRightAlt,
        KeyCode::KeyC => KeyboardJoystickKey::Red,
        KeyCode::KeyX => KeyboardJoystickKey::Blue,
        KeyCode::KeyD => KeyboardJoystickKey::Green,
        KeyCode::KeyS => KeyboardJoystickKey::Yellow,
        KeyCode::Enter | KeyCode::NumpadEnter => KeyboardJoystickKey::Play,
        KeyCode::KeyZ => KeyboardJoystickKey::Rewind,
        KeyCode::KeyA => KeyboardJoystickKey::Forward,
        _ => return None,
    })
}

/// Whether the active mode routes the keyboard joystick mapping to port 2. With
/// only the two explicit modes this is a direct read of the mode -- `Keyboard`
/// captures the arrow/fire keys, `Gamepad` lets every key reach the Amiga.
fn joystick_mode_uses_keyboard(mode: JoystickInputMode) -> bool {
    matches!(mode, JoystickInputMode::Keyboard)
}

/// The port-2 controls currently held by `--joy-after` scripting.
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
const STATUS_BAR_HEIGHT: usize = 44;
const WINDOW_PRESENT_HEIGHT: usize = PRESENT_HEIGHT + STATUS_BAR_HEIGHT;
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
const VOLUME_SLIDER_W: usize = 72;
const VOLUME_SLIDER_H: usize = 8;
const VOLUME_KNOB_W: usize = 8;
const VOLUME_KNOB_H: usize = 16;
const VOLUME_GLYPH_X: usize = VOLUME_SLIDER_X - 16;
// Joystick input-source toggle: a compact icon button just left of the volume
// glyph, in the otherwise-free slot before the right-hand control cluster. The
// widest media layout (four floppies plus a CD) ends at x=372, so a 22px button
// here clears both the media controls and the speaker glyph; this is verified by
// `joystick_toggle_clears_worst_case_media`.
const JOY_TOGGLE_W: usize = 22;
const JOY_TOGGLE_X: usize = VOLUME_GLYPH_X - 2 - JOY_TOGGLE_W;
const STANDARD_PAL_VISIBLE_WIDTH: usize = 320 * 2;
const STANDARD_PAL_VISIBLE_LINES: usize = 256;
const STANDARD_PAL_VISIBLE_START_VPOS: u32 = 0x2C;
// Default TV presentation keeps a small consumer-visible overscan margin while
// still hiding the deep edge columns that often contain unfinished effects.
const TV_HORIZONTAL_OVERSCAN_MARGIN: usize = 24 * 2;
const TV_TOP_OVERSCAN_MARGIN: usize = 8;
const TV_BOTTOM_CROP_ROWS: usize = 1;
const STATUS_BG: u32 = rgba(28, 28, 26);
const STATUS_TOP: u32 = rgba(78, 76, 70);
const STATUS_BOTTOM: u32 = rgba(12, 12, 11);
const LED_BEZEL_DARK: u32 = rgba(8, 8, 7);
const LED_BEZEL_LIGHT: u32 = rgba(78, 76, 68);
const POWER_LED_ON: u32 = rgba(232, 31, 24);
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
const STATUS_TEXT: u32 = rgba(174, 170, 154);
const VOLUME_FILL: u32 = rgba(44, 178, 94);
const VOLUME_FILL_HIGHLIGHT: u32 = rgba(128, 244, 150);
const WINDOW_TITLE: &str = concat!("Copperline ", env!("COPPERLINE_DISPLAY_VERSION"));
const COPPERLINE_LOGO_PNG: &[u8] = include_bytes!("../../assets/brand/copperline-logo.png");
const COPPERLINE_ICON_PNG: &[u8] = include_bytes!("../../assets/brand/copperline-icon.png");
const MOUSE_MOTION_SCALE: f64 = 1.0;
/// How long a transient on-screen overlay message (screenshot saved,
/// disk swapped) stays visible.
const OSD_DURATION: std::time::Duration = std::time::Duration::from_millis(2500);
/// On-screen overlay colours (packed R,G,B,A in memory order).
const OSD_TEXT: u32 = rgba(236, 236, 232);
const OSD_SHADOW: u32 = rgba(0, 0, 0);
const OSD_BG: u32 = rgba(10, 10, 12);
const RECORD_DOT: u32 = rgba(229, 56, 48);

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
    format!("{WINDOW_TITLE} - Mouse captured ({HOST_SHORTCUT_MODIFIER_LABEL}+G releases)")
}

/// A transient on-screen overlay message drawn over the display (but not
/// captured in screenshots, since it is painted into the presentation
/// texture, never into the emulated framebuffer `fb`).
struct Osd {
    text: String,
    expires_at: Instant,
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

use anyhow::{anyhow, Result};
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
use winit::dpi::LogicalSize;
use winit::event::{
    DeviceEvent, DeviceId, ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent,
};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::{CursorGrabMode, Icon, Window, WindowAttributes, WindowId};

pub struct App {
    emu: Emulator,
    fb: Vec<u32>,
    /// Merges rendered fields into the double-height presentation
    /// buffer that the window texture, screenshots, and frame dumps
    /// read (see [`deinterlace`](super::deinterlace)).
    deinterlacer: Deinterlacer,
    /// Active presentation buffer, already deinterlaced/line-doubled and
    /// post-processed. The first `present_rows * FB_WIDTH` pixels are valid.
    present_fb: Vec<u32>,
    present_rows: usize,
    render: Option<Render>,
    debugger_tool_window: Option<ToolWindow>,
    frame_analyzer_tool_window: Option<ToolWindow>,
    debugger_panel: Option<ui::DebuggerPanel>,
    frame_analyzer_panel: Option<ui::FrameAnalyzerPanel>,
    render_worker: Option<RenderWorker>,
    render_recycle_fb: Vec<u32>,
    cpu_halted: bool,
    /// Host-level power state. When false the emulator does not step;
    /// the machine sits powered off until the status-bar power button
    /// is clicked. Distinct from the emulated (CIA-driven) power LED.
    powered_on: bool,
    /// Host-level pause state. When true the emulator does not step but
    /// stays powered on, so the last rendered frame is held on screen and
    /// emulation resumes from the same point when unpaused.
    paused: bool,
    auto_shot: Option<(f32, PathBuf)>,
    pending_auto_shot: Option<(f32, PathBuf)>,
    /// Scheduled --save-state-after capture: write a save state once
    /// emulated time reaches the deadline, then keep running.
    auto_save_state: Option<(f32, PathBuf)>,
    pending_auto_save_state: Option<(f32, PathBuf)>,
    frame_dump: Option<FrameDumpState>,
    pending_frame_dump: Option<FrameDumpSpec>,
    auto_keys: Vec<ScheduledKey>,
    pending_auto_keys: Vec<KeyPressSpec>,
    /// Scheduled mouse-button press/release events from --click-after.
    /// `Press` and `Release` deadlines per requested click.
    auto_clicks: Vec<ScheduledClick>,
    pending_auto_clicks: Vec<(f32, MouseButtonKind, u32)>,
    /// Scheduled port-2 joystick/CD32-pad events from --joy-after, plus
    /// the controls currently held. `auto_joy_engaged` stays true once
    /// any scripted joy event has fired so the state keeps overriding
    /// the (absent) physical pad, including the final release.
    auto_joys: Vec<ScheduledJoy>,
    pending_auto_joys: Vec<(f32, JoyButtonKind, u32)>,
    auto_joy_held: AutoJoyHeld,
    auto_joy_engaged: bool,
    /// Scheduled relative port-1 mouse motions from --mouse-after,
    /// one-shot per entry; (at_emulated_secs, dx, dy).
    auto_mouse: Vec<(f64, i32, i32)>,
    pending_auto_mouse: Vec<(f32, i32, i32)>,
    auto_disk_inserts: Vec<ScheduledDiskInsert>,
    pending_auto_disk_inserts: Vec<DiskInsertSpec>,
    /// Live-input recorder: logs every input event that reaches the
    /// emulated machine and writes a --script-replayable file on stop.
    /// None while not recording.
    input_recorder: Option<crate::inputrec::InputRecorder>,
    /// --record-input destination: when set, the recorder runs for the
    /// whole session and the script is written here on exit (the Drop
    /// impl catches every exit path, including the headless captures).
    record_input_path: Option<PathBuf>,
    modifiers: ModifiersState,
    held_rawkeys: [bool; 128],
    cursor_pos: Option<(i32, i32)>,
    last_display_cursor_pos: Option<(i32, i32)>,
    volume_dragging: bool,
    /// True while the frame analyzer selector is following a held left
    /// mouse button.
    analyzer_dragging: bool,
    mouse_captured: bool,
    mouse_delta_remainder: (f64, f64),
    last_rendered_emulated_frame: Option<u64>,
    last_submitted_render_frame: Option<u64>,
    render_generation: u64,
    last_fdd_track: Option<u8>,
    /// Transient on-screen overlay message (screenshot saved, disk
    /// swapped), or None when nothing is being shown.
    osd: Option<Osd>,
    /// Per-drive disk-swap playlists: the ordered image paths the user can
    /// cycle through for each drive with the disk-swap shortcut. Lets a
    /// multi-disk demo run on a single drive.
    disk_playlists: [Vec<PathBuf>; 4],
    /// Write-protect flag applied to disks swapped in from each playlist.
    disk_write_protected: [bool; 4],
    /// Index of the currently inserted disk within each drive's playlist.
    disk_playlist_index: [usize; 4],
    /// Whether to horizontally recentre a standard (non-overscan) display
    /// for presentation. On by default; set COPPERLINE_HCENTER=0 to disable.
    hcenter: bool,
    /// Presentation-level overscan handling ([display] overscan): Tv masks
    /// the deep-overscan margins with black like a CRT bezel.
    overscan: Overscan,
    /// Host USB gamepad reader (pure-Rust, no SDL2), mapped to the emulated
    /// port-2 digital joystick via a per-pad calibration. A no-op when no
    /// input backend is available (e.g. headless CI) or the pad is not yet
    /// calibrated.
    gamepad: crate::gamepad::GamepadReader,
    /// Host source policy for the emulated port-2 joystick/CD32 pad.
    joystick_input_mode: JoystickInputMode,
    /// Output frame-skip level for warp/turbo mode: how many emulated frames
    /// are retired per presented frame while warp is engaged. Presentation is
    /// vsync-gated, so this is what decouples warp speed from the host monitor
    /// refresh rate. Adjustable from the Emulator menu and the keyboard.
    warp_speed: WarpSpeed,
    /// Mapped host keys currently held for keyboard joystick emulation.
    keyboard_joy_held: KeyboardJoystickHeld,
    /// Pop-up menu and main-window overlay state. Debugger and frame
    /// analyzer panes live in separate tool-window state so they can be
    /// open at the same time.
    ui: UiState,
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
    /// The reason for the last interactive breakpoint/watchpoint stop,
    /// shown on the debugger's Break tab until execution resumes.
    last_debug_stop: Option<String>,
    /// Active video+audio capture (shortcut or the menu's Record Video item),
    /// or None when not recording. Frames and the matching mixer audio are
    /// appended on emulated-frame boundaries, so captures stay in sync
    /// even under warp or host stutter.
    recorder: Option<crate::recorder::VideoRecorder>,
    /// Scratch presentation-scaled framebuffer for the recorder (same
    /// vertical resample as screenshots).
    record_fb: Vec<u32>,
}

#[derive(Debug, Clone, Copy)]
struct ScheduledClick {
    press_at_emulated_secs: f64,
    release_at_emulated_secs: f64,
    button: MouseButtonKind,
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
}

struct ToolWindow {
    window: Arc<Window>,
    pixels: Pixels<'static>,
    texture_scale: usize,
    cursor_pos: Option<(i32, i32)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolPanelKind {
    Debugger,
    FrameAnalyzer,
}

struct RenderJob {
    generation: u64,
    input: bitplane::RenderInput,
    h_shift: usize,
    overscan: Overscan,
    presentation_fb: Vec<u32>,
}

struct RenderWorkerResult {
    generation: u64,
    emulated_frame: u64,
    timing: VideoRenderFrameTiming,
    presentation_fb: Vec<u32>,
    present_rows: usize,
}

struct RenderWorker {
    job_tx: Option<SyncSender<RenderJob>>,
    result_rx: Receiver<RenderWorkerResult>,
    handle: Option<JoinHandle<()>>,
}

impl RenderWorker {
    fn new(phosphor: f32) -> Self {
        let (job_tx, job_rx) = mpsc::sync_channel::<RenderJob>(1);
        let (result_tx, result_rx) = mpsc::channel::<RenderWorkerResult>();
        let handle = std::thread::Builder::new()
            .name("copperline-render".to_string())
            .spawn(move || {
                let mut fb = vec![0u32; MAX_FB_PIXELS];
                let mut deinterlacer = Deinterlacer::with_phosphor(phosphor);
                while let Ok(mut job) = job_rx.recv() {
                    let result = render_job_to_presentation(
                        job.generation,
                        &job.input,
                        &mut fb,
                        &mut deinterlacer,
                        job.h_shift,
                        job.overscan,
                        &mut job.presentation_fb,
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

    fn send(&self, job: RenderJob) -> std::result::Result<(), Vec<u32>> {
        match self
            .job_tx
            .as_ref()
            .expect("render worker sender missing")
            .send(job)
        {
            Ok(()) => Ok(()),
            Err(err) => Err(err.0.presentation_fb),
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

impl App {
    pub fn new(
        emu: Emulator,
        power_on: bool,
        screenshot_after: Option<(f32, PathBuf)>,
        save_state_after: Option<(f32, PathBuf)>,
        frame_dump: Option<FrameDumpSpec>,
        press_after: Vec<KeyPressSpec>,
        click_after: Vec<(f32, MouseButtonKind, u32)>,
        joy_after: Vec<(f32, JoyButtonKind, u32)>,
        mouse_after: Vec<(f32, i32, i32)>,
        disk_insert_after: Vec<DiskInsertSpec>,
        record_input: Option<PathBuf>,
        disk_playlists: [Vec<PathBuf>; 4],
        disk_write_protected: [bool; 4],
        overscan: Overscan,
        phosphor: f32,
        warp_speed: WarpSpeed,
        joystick_input_mode: JoystickInputMode,
        about_machine_lines: Vec<String>,
        machine_config: RawConfig,
    ) -> Self {
        // Headless capture runs drive themselves off emulated time, so a
        // powered-off start would simply hang. Force power on for those.
        let powered_on = power_on
            || screenshot_after.is_some()
            || save_state_after.is_some()
            || frame_dump.is_some();
        let render_worker = threaded_render_enabled().then(|| {
            info!("threaded render pipeline enabled");
            RenderWorker::new(phosphor)
        });
        Self {
            emu,
            fb: vec![0u32; MAX_FB_PIXELS],
            deinterlacer: Deinterlacer::with_phosphor(phosphor),
            present_fb: vec![0u32; FB_WIDTH * OUT_HEIGHT],
            present_rows: OUT_HEIGHT,
            render: None,
            debugger_tool_window: None,
            frame_analyzer_tool_window: None,
            debugger_panel: None,
            frame_analyzer_panel: None,
            render_worker,
            render_recycle_fb: Vec::new(),
            cpu_halted: false,
            powered_on,
            paused: false,
            auto_shot: None,
            pending_auto_shot: screenshot_after,
            auto_save_state: None,
            pending_auto_save_state: save_state_after,
            frame_dump: None,
            pending_frame_dump: frame_dump,
            auto_keys: Vec::new(),
            pending_auto_keys: press_after,
            auto_clicks: Vec::new(),
            pending_auto_clicks: click_after,
            auto_joys: Vec::new(),
            pending_auto_joys: joy_after,
            auto_joy_held: AutoJoyHeld::default(),
            auto_joy_engaged: false,
            auto_mouse: Vec::new(),
            pending_auto_mouse: mouse_after,
            auto_disk_inserts: Vec::new(),
            pending_auto_disk_inserts: disk_insert_after,
            input_recorder: record_input
                .is_some()
                .then(|| crate::inputrec::InputRecorder::new(0.0)),
            record_input_path: record_input,
            modifiers: ModifiersState::empty(),
            held_rawkeys: [false; 128],
            cursor_pos: None,
            last_display_cursor_pos: None,
            volume_dragging: false,
            analyzer_dragging: false,
            mouse_captured: false,
            mouse_delta_remainder: (0.0, 0.0),
            last_rendered_emulated_frame: None,
            last_submitted_render_frame: None,
            render_generation: 0,
            last_fdd_track: None,
            osd: None,
            disk_playlists,
            disk_write_protected,
            disk_playlist_index: [0; 4],
            hcenter: hcenter_enabled(),
            overscan,
            gamepad: crate::gamepad::GamepadReader::new(),
            joystick_input_mode,
            warp_speed,
            keyboard_joy_held: KeyboardJoystickHeld::default(),
            ui: UiState::default(),
            about_machine_lines,
            machine_config,
            paused_before_debugger: false,
            paused_before_analyzer: false,
            last_debug_stop: None,
            recorder: None,
            record_fb: Vec::new(),
        }
    }

    /// Poll the active host joystick source and drive the emulated port-2
    /// joystick. Called once per scheduler quantum. In Gamepad mode a calibrated
    /// physical pad drives the port; in Keyboard mode the keyboard mapping does,
    /// so port 2 stays usable without a physical controller.
    fn pump_joystick_input(&mut self) {
        let gamepad_state = match self.joystick_input_mode {
            JoystickInputMode::Keyboard => None,
            JoystickInputMode::Gamepad => self.gamepad.poll(),
        };

        match gamepad_state {
            Some(state) => self.apply_joystick_state(state),
            // No physical pad but --joy-after scripting has fired: keep
            // asserting the scripted state so it survives this release
            // path and drives the upcoming scheduler quantum.
            None if self.auto_joy_engaged => self.apply_auto_joy_state(),
            None if self.keyboard_joystick_enabled() => self.apply_keyboard_joystick_state(),
            // Pad gone/uncalibrated and keyboard fallback disabled: release
            // everything so nothing sticks.
            None if self.emu.bus().input.joystick_port2 => {
                self.release_port2_joystick();
            }
            None => {}
        }
    }

    fn keyboard_joystick_enabled(&self) -> bool {
        joystick_mode_uses_keyboard(self.joystick_input_mode)
    }

    fn apply_joystick_state(&mut self, state: crate::gamepad::JoystickState) {
        let input = &mut self.emu.bus_mut().input;
        input.set_joystick_port2(
            state.up,
            state.down,
            state.left,
            state.right,
            state.fire,
            state.button2,
        );
        input.set_cd32_buttons_port2(state.play, state.rwd, state.ffw, state.green, state.yellow);
    }

    fn apply_keyboard_joystick_state(&mut self) {
        self.apply_joystick_state(self.keyboard_joy_held.joystick_state());
    }

    fn release_port2_joystick(&mut self) {
        let input = &mut self.emu.bus_mut().input;
        input.set_joystick_port2(false, false, false, false, false, false);
        input.set_cd32_buttons_port2(false, false, false, false, false);
    }

    fn cycle_joystick_input_mode(&mut self) {
        self.set_joystick_input_mode(self.joystick_input_mode.next());
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
        self.show_osd(format!("Joystick input: {}", mode.label()));
    }

    /// Consume a mapped host key as joystick input when keyboard joystick
    /// emulation is active. Releases for previously consumed mapped keys
    /// are also swallowed, even if a gamepad has taken over meanwhile.
    fn handle_keyboard_joystick_key(&mut self, code: KeyCode, pressed: bool) -> bool {
        let Some(key) = keyboard_joystick_key_for(code) else {
            return false;
        };
        let was_held = self.keyboard_joy_held.is_set(key);
        if !self.keyboard_joystick_enabled() && !was_held {
            return false;
        }
        self.keyboard_joy_held.set(key, pressed);
        if self.keyboard_joystick_enabled() {
            self.apply_keyboard_joystick_state();
        }
        true
    }

    /// Drive the emulated port-2 joystick/CD32 pad from the --joy-after
    /// held-control set.
    fn apply_auto_joy_state(&mut self) {
        let held = self.auto_joy_held;
        let input = &mut self.emu.bus_mut().input;
        input.set_joystick_port2(
            held.up, held.down, held.left, held.right, held.red, held.blue,
        );
        input.set_cd32_buttons_port2(held.play, held.rwd, held.ffw, held.green, held.yellow);
        // Reverse-debug: note the held state so replay can reproduce it.
        self.emu.tt_note_input(crate::inputsched::ReplayAction::Joy(
            crate::inputsched::JoyState {
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
        ));
    }

    pub fn run(self) -> Result<()> {
        let event_loop = EventLoop::new().map_err(|e| anyhow!("EventLoop::new: {e}"))?;
        event_loop.set_control_flow(ControlFlow::Poll);
        let mut app = self;
        event_loop
            .run_app(&mut app)
            .map_err(|e| anyhow!("event loop: {e}"))?;
        Ok(())
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
        // Keep the internal overscan field buffer, but present it as a
        // standard 4:3 Amiga display.
        let size = LogicalSize::new(FB_WIDTH as f64, WINDOW_PRESENT_HEIGHT as f64);
        // Headless capture (screenshot / frame dump) renders into the
        // framebuffer for the saved PNG but has no interactive viewer, so
        // create the window hidden: it avoids flashing an empty window on
        // screen and removes the vsync present gate, letting the run
        // advance as fast as the host allows. Emulated state is identical.
        let headless_capture =
            self.pending_auto_shot.is_some() || self.pending_frame_dump.is_some();
        let attrs = WindowAttributes::default()
            .with_title(WINDOW_TITLE)
            .with_window_icon(copperline_window_icon())
            .with_visible(!headless_capture)
            .with_inner_size(size)
            .with_min_inner_size(LogicalSize::new(
                FB_WIDTH as f64 / 2.0,
                WINDOW_PRESENT_HEIGHT as f64 / 2.0,
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
        let texture_scale = texture_scale_for_window(&window);
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
        let pixels = match build_pixels_for_window(window.clone(), texture_scale) {
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
        self.render = Some(Render {
            window,
            pixels,
            texture_scale,
        });
        // Paint at least once so the status bar (and power button) is
        // visible immediately, even when the machine starts powered off
        // and no emulated frame is being produced yet. A powered-off
        // start shows the test screen rather than a black display.
        if !self.powered_on {
            paint_test_screen(&mut self.fb);
            self.deinterlacer
                .push_field(&self.fb, FB_HEIGHT, false, true, true);
            self.refresh_present_from_deinterlacer();
        }
        self.request_redraw();
        if let Some((secs, path)) = self.pending_auto_shot.take() {
            info!(
                "auto-screenshot armed: will save {} after {:.1}s emulated time",
                path.display(),
                secs
            );
            self.auto_shot = Some((secs.max(0.0), path));
        }
        if let Some((secs, path)) = self.pending_auto_save_state.take() {
            info!(
                "auto-save-state armed: will save {} after {:.1}s emulated time",
                path.display(),
                secs
            );
            self.auto_save_state = Some((secs.max(0.0), path));
        }
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
        for (secs, button, dur_ms) in self.pending_auto_clicks.drain(..) {
            let press_at = secs.max(0.0) as f64;
            let release_at = press_at + dur_ms as f64 / 1000.0;
            info!(
                "auto-click armed: {:?} press at {:.1}s emulated, hold {}ms",
                button, secs, dur_ms
            );
            self.auto_clicks.push(ScheduledClick {
                press_at_emulated_secs: press_at,
                release_at_emulated_secs: release_at,
                button,
                pressed: false,
            });
        }
        for (secs, dx, dy) in self.pending_auto_mouse.drain(..) {
            self.auto_mouse.push((secs.max(0.0) as f64, dx, dy));
        }
        if !self.auto_mouse.is_empty() {
            info!(
                "auto-mouse armed: {} scheduled motions",
                self.auto_mouse.len()
            );
        }
        for (secs, button, dur_ms) in self.pending_auto_joys.drain(..) {
            let press_at = secs.max(0.0) as f64;
            let release_at = press_at + dur_ms as f64 / 1000.0;
            info!(
                "auto-joy armed: {:?} press at {:.1}s emulated, hold {}ms",
                button, secs, dur_ms
            );
            self.auto_joys.push(ScheduledJoy {
                press_at_emulated_secs: press_at,
                release_at_emulated_secs: release_at,
                button,
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
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
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
                        ..
                    },
                ..
            } => {
                if repeat && !self.ui_key_accepts_repeat(None, code) {
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
                        self.save_state_interactive()
                    }
                    (KeyCode::KeyL, ElementState::Pressed)
                        if host_shortcut_modifier_pressed(self.modifiers)
                            && self.modifiers.shift_key() =>
                    {
                        self.load_state_from_dialog(Some(event_loop))
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
                    (KeyCode::KeyB, ElementState::Pressed)
                        if host_shortcut_modifier_pressed(self.modifiers) =>
                    {
                        self.toggle_debugger();
                        self.ensure_tool_windows_for_open_panels(event_loop);
                    }
                    (KeyCode::KeyJ, ElementState::Pressed)
                        if host_shortcut_modifier_pressed(self.modifiers) =>
                    {
                        self.cycle_joystick_input_mode()
                    }
                    (KeyCode::KeyR, ElementState::Pressed)
                        if host_shortcut_modifier_pressed(self.modifiers)
                            && self.modifiers.shift_key() =>
                    {
                        self.toggle_input_recording()
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
                    (KeyCode::KeyW, ElementState::Pressed)
                        if host_shortcut_modifier_pressed(self.modifiers) =>
                    {
                        self.toggle_warp()
                    }
                    (other, state) => {
                        let pressed = state == ElementState::Pressed;
                        if pressed && self.ui_handle_key(other) {
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
                self.modifiers = modifiers.state();
            }
            WindowEvent::CursorMoved { position, .. } => {
                let previous_cursor_pos = self.cursor_pos;
                let pos = self
                    .render
                    .as_ref()
                    .and_then(|r| cursor_texture_position(&r.pixels, position, r.texture_scale));
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
                    self.cursor_pos = pos;
                    if self.volume_dragging {
                        if let Some(pos) = pos {
                            self.set_output_volume_from_pos(pos);
                        }
                    }
                }
                let layout = bar_layout(&self.media_bar());
                if bar_hover_changed(&layout, previous_cursor_pos, self.cursor_pos)
                    || self.main_ui_hover_changed(previous_cursor_pos, self.cursor_pos)
                {
                    self.request_redraw();
                }
            }
            WindowEvent::CursorLeft { .. } => {
                let previous_cursor_pos = self.cursor_pos;
                self.cursor_pos = None;
                self.last_display_cursor_pos = None;
                self.volume_dragging = false;
                self.analyzer_dragging = false;
                let layout = bar_layout(&self.media_bar());
                if bar_hover_changed(&layout, previous_cursor_pos, self.cursor_pos) {
                    self.request_redraw();
                }
            }
            WindowEvent::Focused(false) => {
                self.volume_dragging = false;
                self.analyzer_dragging = false;
                self.set_mouse_captured(false);
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let pressed = state == ElementState::Pressed;
                if button == MouseButton::Left {
                    if pressed {
                        self.analyzer_dragging = false;
                    } else {
                        let was_volume_dragging = self.volume_dragging;
                        self.volume_dragging = false;
                        self.analyzer_dragging = false;
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
                            self.ui.menu_open = false;
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
                if pressed
                    && !self.mouse_captured
                    && self.cursor_pos.is_some_and(cursor_in_status_bar)
                {
                    if button == MouseButton::Left {
                        if let Some(pos) = self.cursor_pos {
                            let layout = bar_layout(&self.media_bar());
                            match control_at(pos, &layout) {
                                Some(BarControl::Volume) => {
                                    self.volume_dragging = true;
                                    self.set_output_volume_from_pos(pos);
                                }
                                Some(control) => self.activate_bar_control(control),
                                None => {}
                            }
                        }
                    }
                    return;
                }
                if pressed && !self.mouse_captured && self.cursor_pos.is_some_and(cursor_in_display)
                {
                    self.set_mouse_captured(true);
                }
                let input = &mut self.emu.bus_mut().input;
                match button {
                    MouseButton::Left => input.lmb_port1 = pressed,
                    MouseButton::Right => input.rmb_port1 = pressed,
                    MouseButton::Middle => input.mmb_port1 = pressed,
                    _ => {}
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if !self.mouse_captured
                    && self
                        .cursor_pos
                        .is_some_and(|pos| volume_control_hit_rect().contains(pos))
                {
                    if let Some(steps) = volume_scroll_steps(delta) {
                        self.adjust_output_volume(steps * VOLUME_STEP_PERCENT);
                    }
                }
            }
            WindowEvent::Resized(size) => {
                if let Some(r) = self.render.as_mut() {
                    let _ = r
                        .pixels
                        .resize_surface(size.width.max(1), size.height.max(1));
                }
                // Resizing the surface discards its contents, leaving it
                // blank (white) until the next present. When the machine is
                // powered off (or paused) the event loop is in Wait mode and
                // produces no frames, so without an explicit repaint here the
                // window can sit white after the scale-factor/resize event
                // that macOS delivers right after window creation.
                self.request_redraw();
            }
            WindowEvent::RedrawRequested => {
                let status = status_with_latched_fdd_track(
                    self.emu.bus().front_panel_status(),
                    &mut self.last_fdd_track,
                );
                let media = self.media_bar();
                let hover = self
                    .cursor_pos
                    .and_then(|pos| control_at(pos, &bar_layout(&media)));
                let view = StatusBarView {
                    status,
                    powered_on: self.powered_on,
                    paused: self.paused,
                    media,
                    joystick_input_mode: self.joystick_input_mode,
                    hover,
                };
                let osd = self.active_osd_text();
                let ui_hover = self.cursor_pos.and_then(|p| self.main_ui_control_at(p));
                let warp = !self.emu.paced();
                let warp_speed = self.warp_speed;
                let recording = self.recorder.is_some();
                let input_recording = self.input_recorder.is_some();
                let ui_data = self.build_panel_view_data();
                if let Some(r) = self.render.as_mut() {
                    let frame = r.pixels.frame_mut();
                    copy_present_frame(&self.present_fb, self.present_rows, frame, r.texture_scale);
                    draw_status_bar(frame, &view, r.texture_scale);
                    if recording {
                        // Painted into the presentation texture only, so
                        // the badge never appears in the recorded file.
                        draw_record_badge(frame, r.texture_scale);
                    }
                    if let Some(text) = &osd {
                        draw_osd(frame, text, r.texture_scale);
                    }
                    ui::draw(
                        frame,
                        r.texture_scale,
                        &self.ui,
                        ui_hover,
                        ui_data.as_ref(),
                        warp,
                        warp_speed,
                        recording,
                        input_recording,
                        self.joystick_input_mode,
                    );
                    if let Err(e) = r.pixels.render() {
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
        if let DeviceEvent::MouseMotion { delta } = event {
            if self.mouse_captured {
                self.add_host_mouse_delta(delta.0, delta.1);
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.render.is_none() {
            return;
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
        event_loop.set_control_flow(if running || osd_active || calibrating {
            ControlFlow::Poll
        } else {
            ControlFlow::Wait
        });
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
            let input = &mut self.emu.bus_mut().input;
            if input.joystick_port2 {
                input.set_joystick_port2(false, false, false, false, false, false);
                input.set_cd32_buttons_port2(false, false, false, false, false);
            }
            if !running {
                // Nothing paces the loop while the machine is not stepping;
                // don't busy-spin just to poll the pad.
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        } else {
            self.pump_joystick_input();
        }
        // Headless capture (screenshot/frame-dump) builds the framebuffer for
        // the saved PNG but presents nothing: it already runs unthrottled at one
        // frame per loop (request_redraw is skipped below), and every captured
        // frame must be rendered, so warp's output frame-skip burst must not
        // apply there.
        let headless_capture = self.auto_shot.is_some() || self.frame_dump.is_some();
        // Run one scheduler quantum. Rebuild the host framebuffer only
        // when Agnus has crossed into a new frame; the expensive renderer
        // reconstructs a completed hardware frame, not an instruction slice.
        if running {
            // Presentation is vsync-gated, so emulating exactly one frame per
            // presented frame would cap warp at the host monitor refresh rate
            // (about 1.2x for 50 Hz PAL on a 60 Hz display). In warp, retire
            // several frames per presented frame (output frame skip): only the
            // last frame of the burst is rendered and presented, so the
            // effective speed is the warp level times the refresh rate, host
            // CPU permitting. Real-time pacing and headless capture stay at one
            // frame per loop.
            let (frame_cap, time_budget) = self.warp_burst_plan(headless_capture);
            let burst_start = Instant::now();
            let mut frames_done = 0usize;
            loop {
                if let Err(e) = self.emu.step_frame() {
                    error!("emulator step halted: {e:?}");
                    self.cpu_halted = true;
                    self.sync_live_audio_suspension();
                    break;
                }
                frames_done += 1;
                // A breakpoint/watchpoint hit pauses the machine and brings
                // the debugger window up with the reason; end the burst so the
                // stop surfaces at the frame where it happened.
                if self.surface_debug_stop() {
                    break;
                }
                if frames_done >= frame_cap {
                    break;
                }
                if let Some(budget) = time_budget {
                    if burst_start.elapsed() >= budget {
                        break;
                    }
                }
            }
            self.ensure_tool_windows_for_open_panels(event_loop);
        }
        let now = Instant::now();
        // While powered off, leave the parked test screen in place; the
        // emulator is not advancing, so there is no new frame to show.
        let mut rendered = self.powered_on && self.render_emulated_frame_if_needed();
        if self.recorder.is_some() && self.powered_on {
            rendered |= self.finish_render_for_current_frame();
        }
        self.capture_recorder_output(rendered);
        // Skipping request_redraw for headless capture avoids the vsync gate so
        // the run advances as fast as the host allows; emulated state is
        // identical either way. (`headless_capture` was resolved above, before
        // the step, to decide the warp burst.)
        if rendered && !headless_capture {
            self.request_redraw();
        }

        if self.dump_frame_if_due(now, event_loop) {
            return;
        }

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
                info!("auto-click pressing: {:?}", c.button);
                set_mouse_button(&mut self.emu, c.button, true);
                c.pressed = true;
            }
            if c.pressed && emu_secs >= c.release_at_emulated_secs {
                info!("auto-click releasing: {:?}", c.button);
                set_mouse_button(&mut self.emu, c.button, false);
                return false;
            }
            true
        });
        // Fire any scheduled --joy-after events into the port-2
        // joystick/CD32-pad state, then assert the held set (input polling
        // re-applies it every quantum while scripting is engaged).
        let mut joy_changed = false;
        let held = &mut self.auto_joy_held;
        self.auto_joys.retain_mut(|j| {
            if !j.pressed && emu_secs >= j.press_at_emulated_secs {
                info!("auto-joy pressing: {:?}", j.button);
                held.set(j.button, true);
                j.pressed = true;
                joy_changed = true;
            }
            if j.pressed && emu_secs >= j.release_at_emulated_secs {
                info!("auto-joy releasing: {:?}", j.button);
                held.set(j.button, false);
                joy_changed = true;
                return false;
            }
            true
        });
        if joy_changed {
            self.auto_joy_engaged = true;
            self.apply_auto_joy_state();
        }
        // Fire any scheduled --mouse-after relative motions (one-shot
        // each); these land on the same port-1 quadrature counters as
        // live captured-mouse movement.
        let mut mouse_deltas = Vec::new();
        self.auto_mouse.retain(|&(at, dx, dy)| {
            if emu_secs >= at {
                mouse_deltas.push((dx, dy));
                false
            } else {
                true
            }
        });
        for (dx, dy) in mouse_deltas {
            self.add_mouse_delta_i32(dx, dy);
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
        // Scheduled --save-state-after capture. step_frame has completed for
        // this quantum, so the machine is at the frame-boundary quiescent
        // point save states require. Unlike the auto-screenshot this does
        // not exit: a state save is a capture along the way, not the end of
        // a verification run.
        if let Some((secs, path)) = self.auto_save_state.take() {
            if self.emu.bus().emulated_seconds() >= secs as f64 {
                match self.emu.save_state(&path) {
                    Ok(()) => info!("auto-save-state saved: {}", path.display()),
                    Err(e) => warn!("auto-save-state failed ({}): {e:#}", path.display()),
                }
            } else {
                self.auto_save_state = Some((secs, path));
            }
        }
        if let Some((secs, path)) = self.auto_shot.take() {
            if self.emu.bus().emulated_seconds() >= secs as f64 {
                let emulated_frame = self.emu.bus().emulated_frames();
                self.finish_render_for_current_frame();
                if self.last_rendered_emulated_frame != Some(emulated_frame) {
                    self.auto_shot = Some((secs, path));
                    return;
                }
                self.save_screenshot(&path);
                self.emu.report_stats();
                self.emu.bus().poll_stats.dump_top("at screenshot");
                // Evaluate an untargeted reverse watchpoint at run end.
                if let Err(e) = self.emu.tt_finalize_reverse_watch() {
                    warn!("reverse watchpoint evaluation failed: {e:#}");
                }
                event_loop.exit();
            } else {
                self.auto_shot = Some((secs, path));
            }
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

/// A one-line, length-bounded form of an error for the configuration panel's
/// status line (the full chain still goes to the log).
fn short_status_error(err: &anyhow::Error) -> String {
    let msg = err.to_string();
    let first_line = msg.lines().next().unwrap_or("").trim();
    first_line.chars().take(96).collect()
}

fn set_mouse_button(emu: &mut Emulator, button: MouseButtonKind, pressed: bool) {
    let input = &mut emu.bus_mut().input;
    let index = match button {
        MouseButtonKind::Left => {
            input.lmb_port1 = pressed;
            0
        }
        MouseButtonKind::Right => {
            input.rmb_port1 = pressed;
            1
        }
        MouseButtonKind::Middle => {
            input.mmb_port1 = pressed;
            2
        }
    };
    // Reverse-debug: note the transition so replay can reproduce it.
    emu.tt_note_input(crate::inputsched::ReplayAction::MouseButton { index, pressed });
}

fn render_job_to_presentation(
    generation: u64,
    input: &bitplane::RenderInput,
    fb: &mut [u32],
    deinterlacer: &mut Deinterlacer,
    h_shift: usize,
    overscan: Overscan,
    presentation_fb: &mut Vec<u32>,
) -> RenderWorkerResult {
    let render_result = bitplane::render_from_input(input, fb);
    let geometry = input.geometry();
    let visible_start_vpos = input.visible_start_vpos();
    let field_rows =
        post_process_rendered_field(fb, geometry, visible_start_vpos, h_shift, overscan);
    let base = input.render_base();
    deinterlacer.push_field(
        fb,
        field_rows,
        base.bplcon0 & 0x0004 != 0,
        base.long_field,
        !geometry.programmable,
    );
    let present_rows = deinterlacer.output_rows();
    let active = present_rows * FB_WIDTH;
    presentation_fb.resize(active, 0);
    presentation_fb.copy_from_slice(&deinterlacer.output()[..active]);
    RenderWorkerResult {
        generation,
        emulated_frame: input.emulated_frames(),
        timing: render_result.timing,
        presentation_fb: std::mem::take(presentation_fb),
        present_rows,
    }
}

fn post_process_rendered_field(
    fb: &mut [u32],
    geometry: FrameGeometry,
    visible_start_vpos: u32,
    h_shift: usize,
    overscan: Overscan,
) -> usize {
    let field_rows = geometry.visible_lines.min(fb.len() / FB_WIDTH);
    // Vertical/horizontal recentring and the TV bezel mask are 15 kHz
    // CRT concepts anchored to the standard PAL/NTSC window; a
    // programmable scan defines its own window and presents in full,
    // like a multisync monitor.
    if !geometry.programmable {
        center_present_frame_for_visible_start(fb, visible_start_vpos);
        center_present_frame_horizontally(fb, h_shift);
        if overscan == Overscan::Tv {
            mask_present_frame_to_tv(fb, h_shift, standard_window_top_row(visible_start_vpos));
        }
    } else if geometry.line_cck != 227 {
        // A multisync monitor's horizontal deflection is time-linear:
        // each colour clock of this scan's shorter/longer line covers
        // 227/line_cck of the glass a standard line's clock would.
        screenshot::stretch_rows_x(fb, FB_WIDTH, field_rows, geometry.line_cck, 227);
    }
    field_rows
}

fn log_frame_dump_metadata(index: u32, emu: &Emulator) {
    let bus = emu.bus();
    let base = bus.frame_render_base();
    let (top, bottom, bottom_valid) = bus.frame_palette_split();
    let events = bus.frame_render_events();
    let captured_rows = bus
        .frame_captured_bitplane_rows()
        .iter()
        .filter(|row| row.is_some())
        .count();
    let mut cpu_colors = 0usize;
    let mut copper_colors = 0usize;
    let mut control_events = 0usize;
    for event in events {
        let off = event.offset & 0x01FE;
        if matches!(off, 0x180..=0x1BE) {
            match event.source {
                BeamWriteSource::Cpu | BeamWriteSource::CpuCopperIrq => cpu_colors += 1,
                BeamWriteSource::Copper => copper_colors += 1,
            }
        }
        if matches!(off, 0x100 | 0x102 | 0x104) {
            control_events += 1;
        }
    }
    info!(
        "frame-meta idx={} emu_frame={} emu_secs={:.3} pc={:#08X} beam=({}, {}) dmacon={:#06X} bplcon0={:#06X} bplcon1={:#06X} bplcon2={:#06X} bplpt={:06X?} sprpt={:06X?} sprctl={:04X?} bplmod=({},{}) ddf=({:#05X},{:#05X}) diw=({:#06X},{:#06X}) base_pal={:03X?} top_pal={:03X?} bottom_valid={} bottom_pal={:03X?} events={} cpu_colors={} copper_colors={} controls={} captured_rows={}",
        index,
        bus.emulated_frames(),
        bus.emulated_seconds(),
        emu.machine.pc(),
        bus.agnus.vpos,
        bus.agnus.hpos,
        base.dmacon,
        base.bplcon0,
        base.bplcon1,
        base.bplcon2,
        base.bplpt,
        base.sprpt,
        base.sprctl,
        base.bpl1mod,
        base.bpl2mod,
        base.ddfstrt,
        base.ddfstop,
        base.diwstrt,
        base.diwstop,
        &base.palette.hi_words()[..16],
        &top.hi_words()[..16],
        bottom_valid,
        &bottom.hi_words()[..16],
        events.len(),
        cpu_colors,
        copper_colors,
        control_events,
        captured_rows
    );
    if crate::envcfg::flag("COPPERLINE_DUMP_RENDER_META_VERBOSE") {
        let render_events: Vec<_> = events
            .iter()
            .map(|event| {
                (
                    event.vpos,
                    event.hpos,
                    event.offset & 0x01FE,
                    event.value,
                    event.source,
                )
            })
            .collect();
        info!(
            "frame-meta-events idx={} events={:03X?}",
            index, render_events
        );
        let color_events: Vec<_> = events
            .iter()
            .filter_map(|event| {
                let off = event.offset & 0x01FE;
                matches!(off, 0x180..=0x1BE).then_some((
                    event.vpos,
                    event.hpos,
                    off,
                    event.value & 0x0FFF,
                    event.source,
                ))
            })
            .collect();
        info!(
            "frame-meta-colors idx={} events={:03X?}",
            index, color_events
        );
        let setup_events: Vec<_> = events
            .iter()
            .filter_map(|event| {
                let off = event.offset & 0x01FE;
                matches!(
                    off,
                    0x08E | 0x090 | 0x092 | 0x094 | 0x100..=0x10A | 0x0E0..=0x0F7
                )
                .then_some((
                    event.vpos,
                    event.hpos,
                    off,
                    event.value,
                    event.source,
                ))
            })
            .collect();
        info!(
            "frame-meta-setup idx={} events={:03X?}",
            index, setup_events
        );
        let mut nonzero_ranges: [(Option<usize>, Option<usize>); 6] =
            std::array::from_fn(|_| (None, None));
        let mut row_shape_ranges: Vec<(usize, usize, usize, usize)> = Vec::new();
        for (y, row) in bus.frame_captured_bitplane_rows().iter().enumerate() {
            let Some(row) = row else {
                continue;
            };
            match row_shape_ranges.last_mut() {
                Some((_, end, nplanes, words_per_row))
                    if *end + 1 == y
                        && *nplanes == row.nplanes
                        && *words_per_row == row.words_per_row =>
                {
                    *end = y;
                }
                _ => row_shape_ranges.push((y, y, row.nplanes, row.words_per_row)),
            }
            for (plane, range) in nonzero_ranges.iter_mut().enumerate() {
                if row.planes[plane].iter().any(|word| *word != 0) {
                    if range.0.is_none() {
                        range.0 = Some(y);
                    }
                    range.1 = Some(y);
                }
            }
        }
        info!(
            "frame-meta-bitplanes idx={} nonzero_ranges={:?} row_shape_ranges={:?}",
            index, nonzero_ranges, row_shape_ranges
        );
        let bottom_events: Vec<_> = bus
            .frame_bottom_palette_events()
            .iter()
            .map(|event| {
                (
                    event.vpos,
                    event.hpos,
                    event.offset & 0x01FE,
                    event.value & 0x0FFF,
                    event.source,
                )
            })
            .collect();
        info!(
            "frame-meta-bottom-events idx={} events={:03X?}",
            index, bottom_events
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Rect {
    pub(super) x: usize,
    pub(super) y: usize,
    pub(super) w: usize,
    pub(super) h: usize,
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

pub(super) const fn rgba(r: u32, g: u32, b: u32) -> u32 {
    0xFF00_0000 | (b << 16) | (g << 8) | r
}

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
    ICON.get_or_init(|| match decode_embedded_png(COPPERLINE_ICON_PNG) {
        Ok(image) => Some(image),
        Err(e) => {
            warn!("embedded Copperline icon decode failed: {e:#}");
            None
        }
    })
    .as_ref()
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
        let data = NSData::with_bytes(COPPERLINE_ICON_PNG);
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
    let mut buf = vec![0; reader.output_buffer_size()];
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

fn texture_scale_for_window(window: &Window) -> usize {
    (window.scale_factor().round() as usize).clamp(1, MAX_TEXTURE_SCALE)
}

fn build_pixels_for_window(
    window: Arc<Window>,
    texture_scale: usize,
) -> std::result::Result<Pixels<'static>, pixels::Error> {
    let inner = window.inner_size();
    let surface = SurfaceTexture::new(inner.width.max(1), inner.height.max(1), window);
    let builder = PixelsBuilder::new(
        texture_width(texture_scale) as u32,
        texture_height(texture_scale) as u32,
        surface,
    );
    let builder = if cfg!(target_os = "linux") {
        builder.wgpu_backend(
            pixels::wgpu::Backends::from_env().unwrap_or(pixels::wgpu::Backends::VULKAN),
        )
    } else {
        builder
    };
    let mut pixels = builder.build()?;
    pixels.set_scaling_mode(ScalingMode::Fill);
    Ok(pixels)
}

pub(super) fn texture_width(scale: usize) -> usize {
    FB_WIDTH * scale
}

pub(super) fn texture_height(scale: usize) -> usize {
    WINDOW_PRESENT_HEIGHT * scale
}

pub(super) fn scale_rect(rect: Rect, scale: usize) -> Rect {
    Rect {
        x: rect.x * scale,
        y: rect.y * scale,
        w: rect.w * scale,
        h: rect.h * scale,
    }
}

fn cursor_texture_position(
    pixels: &Pixels<'_>,
    position: winit::dpi::PhysicalPosition<f64>,
    texture_scale: usize,
) -> Option<(i32, i32)> {
    let (x, y) = pixels
        .window_pos_to_pixel((position.x as f32, position.y as f32))
        .ok()?;
    Some(((x / texture_scale) as i32, (y / texture_scale) as i32))
}

fn cursor_in_status_bar(pos: (i32, i32)) -> bool {
    pos.1 >= PRESENT_HEIGHT as i32 && pos.1 < WINDOW_PRESENT_HEIGHT as i32
}

fn cursor_in_display(pos: (i32, i32)) -> bool {
    pos.0 >= 0 && pos.0 < FB_WIDTH as i32 && pos.1 >= 0 && pos.1 < PRESENT_HEIGHT as i32
}

fn volume_percent_from_pos(pos: (i32, i32)) -> u8 {
    let track = volume_slider_track_rect();
    let x = pos.0.clamp(track.x as i32, (track.x + track.w - 1) as i32) as usize;
    let range = track.w.saturating_sub(1).max(1);
    (((x - track.x) * 100 + range / 2) / range) as u8
}

fn volume_scroll_steps(delta: MouseScrollDelta) -> Option<i16> {
    let amount = match delta {
        MouseScrollDelta::LineDelta(_, y) => f64::from(y),
        MouseScrollDelta::PixelDelta(pos) => pos.y,
    };
    if amount > 0.0 {
        Some(1)
    } else if amount < 0.0 {
        Some(-1)
    } else {
        None
    }
}

fn owner_name_from_code(code: u8) -> &'static str {
    match code {
        b'R' => "refresh",
        b'B' => "bitplane",
        b'S' => "sprite",
        b'D' => "disk",
        b'A' => "audio",
        b'C' => "copper",
        b'L' => "blitter",
        b'P' => "cpu",
        _ => "idle",
    }
}

/// Source-row sample position for presentation output row `y` of
/// `dst_rows`: the centre-aligned bilinear position (y + 0.5) *
/// src_rows / dst_rows - 0.5 as a whole source row plus a 0..256
/// blend fraction toward the next row.
#[inline]
fn present_row_sample(y: usize, src_rows: usize, dst_rows: usize) -> (usize, u32) {
    let pos = ((2 * y as i64 + 1) * src_rows as i64 * 128 / dst_rows as i64 - 128)
        .clamp(0, ((src_rows - 1) as i64) << 8) as usize;
    (pos >> 8, (pos & 0xFF) as u32)
}

fn copy_present_frame(src_fb: &[u32], src_rows: usize, frame: &mut [u8], texture_scale: usize) {
    debug_assert!(src_fb.len() >= src_rows * FB_WIDTH);
    debug_assert_eq!(
        frame.len(),
        texture_width(texture_scale) * texture_height(texture_scale) * 4
    );
    let dst_stride = texture_width(texture_scale) * 4;
    let out_rows = PRESENT_HEIGHT * texture_scale;
    // The 570 woven scanlines map onto the 537-row 4:3 presentation (times
    // the HiDPI texture scale). Nearest-neighbour subsampling here made
    // line thickness uneven across the picture (visible on crosshatch and
    // dot test patterns), so resample vertically with a centre-aligned
    // bilinear filter at full texture resolution: every source line gets
    // equal coverage and the HiDPI rows carry the sub-line positioning.
    let mut blended = [0u32; FB_WIDTH];
    for y in 0..out_rows {
        let (src_y0, frac) = present_row_sample(y, src_rows, out_rows);
        let row0 = &src_fb[src_y0 * FB_WIDTH..(src_y0 + 1) * FB_WIDTH];
        let row: &[u32] = if frac == 0 || src_y0 + 1 >= src_rows {
            row0
        } else {
            let row1 = &src_fb[(src_y0 + 1) * FB_WIDTH..(src_y0 + 2) * FB_WIDTH];
            for (d, (&a, &b)) in blended.iter_mut().zip(row0.iter().zip(row1.iter())) {
                *d = blend_rgba(a, b, frac);
            }
            &blended
        };

        let dst_off = y * dst_stride;
        match texture_scale {
            1 => unsafe {
                std::ptr::copy_nonoverlapping(
                    row.as_ptr() as *const u8,
                    frame.as_mut_ptr().add(dst_off),
                    FB_WIDTH * 4,
                );
            },
            2 => {
                for (x, &pixel) in row.iter().enumerate() {
                    let pair = pixel as u64 | ((pixel as u64) << 32);
                    unsafe {
                        (frame.as_mut_ptr().add(dst_off + x * 8) as *mut u64).write_unaligned(pair);
                    }
                }
            }
            _ => {
                let dst = &mut frame[dst_off..dst_off + dst_stride];
                for x in 0..FB_WIDTH * texture_scale {
                    dst[x * 4..x * 4 + 4].copy_from_slice(&row[x / texture_scale].to_le_bytes());
                }
            }
        }
    }
}

/// Paint a TV-style test pattern into the presentation framebuffer,
/// shown while the machine is powered off. SMPTE-style colour bars over
/// a grayscale step wedge: instantly readable as "no signal", and handy
/// for setting up video capture levels before the machine boots.
fn paint_test_screen(fb: &mut [u32]) {
    debug_assert!(fb.len() >= FB_PIXELS);
    const BARS: [u32; 7] = [
        rgba(192, 192, 192), // grey
        rgba(192, 192, 0),   // yellow
        rgba(0, 192, 192),   // cyan
        rgba(0, 192, 0),     // green
        rgba(192, 0, 192),   // magenta
        rgba(192, 0, 0),     // red
        rgba(0, 0, 192),     // blue
    ];
    const STEPS: usize = 8;
    let bars_h = FB_HEIGHT * 4 / 5;
    for y in 0..FB_HEIGHT {
        let row = &mut fb[y * FB_WIDTH..(y + 1) * FB_WIDTH];
        if y < bars_h {
            for (x, px) in row.iter_mut().enumerate() {
                *px = BARS[x * BARS.len() / FB_WIDTH];
            }
        } else {
            for (x, px) in row.iter_mut().enumerate() {
                let level = (x * STEPS / FB_WIDTH) as u32 * 255 / (STEPS as u32 - 1);
                *px = rgba(level, level, level);
            }
        }
    }
    draw_test_screen_logo(fb, bars_h);
}

fn draw_test_screen_logo(fb: &mut [u32], bars_h: usize) {
    let Some(image) = copperline_logo_image() else {
        return;
    };
    let x = FB_WIDTH.saturating_sub(image.width) / 2;
    let y = bars_h.saturating_sub(image.height) / 2;
    alpha_blit_rgba(fb, FB_WIDTH, FB_HEIGHT, x, y, image);
}

fn alpha_blit_rgba(
    dst: &mut [u32],
    dst_w: usize,
    dst_h: usize,
    x0: usize,
    y0: usize,
    src: &EmbeddedRgbaImage,
) {
    for sy in 0..src.height {
        let dy = y0 + sy;
        if dy >= dst_h {
            break;
        }
        for sx in 0..src.width {
            let dx = x0 + sx;
            if dx >= dst_w {
                break;
            }
            let src_off = (sy * src.width + sx) * 4;
            let sr = src.rgba[src_off] as u32;
            let sg = src.rgba[src_off + 1] as u32;
            let sb = src.rgba[src_off + 2] as u32;
            let sa = src.rgba[src_off + 3] as u32;
            if sa == 0 {
                continue;
            }
            let dst_px = &mut dst[dy * dst_w + dx];
            *dst_px = if sa == 0xFF {
                rgba(sr, sg, sb)
            } else {
                blend_rgba_over_opaque(*dst_px, sr, sg, sb, sa)
            };
        }
    }
}

fn blend_rgba_over_opaque(dst: u32, sr: u32, sg: u32, sb: u32, sa: u32) -> u32 {
    let [dr, dg, db, _] = dst.to_le_bytes();
    let inv = 0xFF - sa;
    let r = (sr * sa + u32::from(dr) * inv + 127) / 0xFF;
    let g = (sg * sa + u32::from(dg) * inv + 127) / 0xFF;
    let b = (sb * sa + u32::from(db) * inv + 127) / 0xFF;
    rgba(r, g, b)
}

#[allow(clippy::too_many_arguments)]
fn draw_status_bar(frame: &mut [u8], view: &StatusBarView, texture_scale: usize) {
    let status = view.status;
    let layout = bar_layout(&view.media);
    let hover = view.hover;
    fill_rect(
        frame,
        scale_rect(status_bar_rect(), texture_scale),
        STATUS_BG,
        texture_scale,
    );
    draw_hline(
        frame,
        PRESENT_HEIGHT * texture_scale,
        STATUS_TOP,
        texture_scale,
    );
    draw_hline(
        frame,
        WINDOW_PRESENT_HEIGHT * texture_scale - 1,
        STATUS_BOTTOM,
        texture_scale,
    );
    let rows = led_rows(&status, view.powered_on);
    for (row, spec) in rows.iter().enumerate() {
        draw_text(
            frame,
            STATUS_LABEL_X * texture_scale,
            (PRESENT_HEIGHT + led_row_label_y(row, rows.len())) * texture_scale,
            spec.label,
            STATUS_TEXT,
            texture_scale,
        );
        draw_led(
            frame,
            scale_rect(led_row_rect(row, rows.len()), texture_scale),
            spec.on,
            spec.on_color,
            spec.off_color,
            spec.highlight_on,
            spec.highlight_off,
            texture_scale,
        );
    }
    draw_fdd_track_counter(frame, status.fdd_track, texture_scale);
    for idx in 0..4 {
        let drive = view.media.drives[idx];
        if let Some(rect) = layout.drive_load[idx] {
            draw_disk_button(
                frame,
                scale_rect(rect, texture_scale),
                idx,
                hover == Some(BarControl::DriveLoad(idx)),
                texture_scale,
            );
        }
        if let Some(rect) = layout.drive_swap[idx] {
            draw_swap_button(
                frame,
                scale_rect(rect, texture_scale),
                drive.multi,
                hover == Some(BarControl::DriveSwap(idx)),
                texture_scale,
            );
        }
        if let Some(rect) = layout.drive_eject[idx] {
            draw_eject_button(
                frame,
                scale_rect(rect, texture_scale),
                drive.inserted,
                hover == Some(BarControl::DriveEject(idx)),
                texture_scale,
            );
        }
    }
    if let Some(rect) = layout.cd_load {
        draw_cd_button(
            frame,
            scale_rect(rect, texture_scale),
            hover == Some(BarControl::CdLoad),
            texture_scale,
        );
    }
    if let Some(rect) = layout.cd_eject {
        draw_eject_button(
            frame,
            scale_rect(rect, texture_scale),
            view.media.cd == Some(true),
            hover == Some(BarControl::CdEject),
            texture_scale,
        );
    }
    draw_joystick_button(
        frame,
        scale_rect(joystick_toggle_rect(), texture_scale),
        view.joystick_input_mode,
        hover == Some(BarControl::Joystick),
        texture_scale,
    );
    draw_volume_control(frame, status.output_volume_percent, texture_scale);
    draw_menu_button(
        frame,
        scale_rect(menu_button_rect(), texture_scale),
        hover == Some(BarControl::Menu),
        texture_scale,
    );
    draw_shot_button(
        frame,
        scale_rect(shot_button_rect(), texture_scale),
        hover == Some(BarControl::Screenshot),
        texture_scale,
    );
    draw_pause_button(
        frame,
        scale_rect(pause_button_rect(), texture_scale),
        view.paused,
        hover == Some(BarControl::Pause),
        texture_scale,
    );
    draw_power_button(
        frame,
        scale_rect(power_button_rect(), texture_scale),
        view.powered_on,
        hover == Some(BarControl::Power),
        texture_scale,
    );
    draw_reboot_button(
        frame,
        scale_rect(reboot_button_rect(), texture_scale),
        hover == Some(BarControl::Reboot),
        texture_scale,
    );
}

/// Per-drive status feeding the media controls in the status bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct DriveBar {
    /// Drive is wired up this session; unconnected drives get no controls.
    connected: bool,
    /// A disk is currently inserted (enables the eject button).
    inserted: bool,
    /// More than one image is queued for this drive (enables swap).
    multi: bool,
}

/// Removable-media status for the bar: the floppy drives plus the CD
/// drive (None on machines without one, Some(disc inserted) otherwise).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MediaBar {
    drives: [DriveBar; 4],
    cd: Option<bool>,
}

/// Everything draw_status_bar needs for one frame.
struct StatusBarView {
    status: FrontPanelStatus,
    powered_on: bool,
    paused: bool,
    media: MediaBar,
    /// Active host joystick source, shown by the status-bar toggle icon.
    joystick_input_mode: JoystickInputMode,
    hover: Option<BarControl>,
}

/// A clickable status-bar control, used for hit-testing and hover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BarControl {
    Power,
    Pause,
    Reboot,
    Screenshot,
    Menu,
    Joystick,
    Volume,
    DriveLoad(usize),
    DriveSwap(usize),
    DriveEject(usize),
    CdLoad,
    CdEject,
}

/// Computed positions of the variable (media) part of the status bar.
/// The fixed controls (volume, screenshot, pause, power, reboot) keep
/// their own rect functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BarLayout {
    drive_load: [Option<Rect>; 4],
    drive_swap: [Option<Rect>; 4],
    drive_eject: [Option<Rect>; 4],
    cd_load: Option<Rect>,
    cd_eject: Option<Rect>,
}

/// Lay out the media controls left to right after the track counter.
/// One or two drives sit in a single full-height row; three or four
/// stack two-up in shorter rows, so even the worst case (four drives
/// plus CD) keeps the counter and ends clear of the volume control.
fn bar_layout(media: &MediaBar) -> BarLayout {
    let mut layout = BarLayout {
        drive_load: [None; 4],
        drive_swap: [None; 4],
        drive_eject: [None; 4],
        cd_load: None,
        cd_eject: None,
    };
    let connected: Vec<usize> = (0..4).filter(|&idx| media.drives[idx].connected).collect();
    let stacked = connected.len() > 2;

    let cluster = |x: usize, y: usize, h: usize| {
        let button = |x: usize, w: usize| Rect { x, y, w, h };
        (
            button(x, MEDIA_LOAD_W),
            button(x + MEDIA_LOAD_W + MEDIA_INNER_GAP, MEDIA_SMALL_W),
            button(
                x + MEDIA_LOAD_W + 2 * MEDIA_INNER_GAP + MEDIA_SMALL_W,
                MEDIA_SMALL_W,
            ),
        )
    };

    let mut drives_end_x = MEDIA_CLUSTER_X;
    for (pos, &idx) in connected.iter().enumerate() {
        let (x, y, h) = if stacked {
            // Row-major two-column grid: DF0 DF1 over DF2 DF3.
            let col = pos % 2;
            let row = pos / 2;
            (
                MEDIA_CLUSTER_X + col * (MEDIA_CLUSTER_W + MEDIA_CLUSTER_GAP),
                PRESENT_HEIGHT + MEDIA_STACKED_ROW0_Y + row * MEDIA_STACKED_PITCH,
                MEDIA_STACKED_H,
            )
        } else {
            (
                MEDIA_CLUSTER_X + pos * (MEDIA_CLUSTER_W + MEDIA_CLUSTER_GAP),
                PRESENT_HEIGHT + STATUS_CONTROL_Y,
                STATUS_CONTROL_H,
            )
        };
        let (load, swap, eject) = cluster(x, y, h);
        layout.drive_load[idx] = Some(load);
        layout.drive_swap[idx] = Some(swap);
        layout.drive_eject[idx] = Some(eject);
        drives_end_x = drives_end_x.max(x + MEDIA_CLUSTER_W);
    }

    if media.cd.is_some() {
        let x = if connected.is_empty() {
            MEDIA_CLUSTER_X
        } else {
            drives_end_x + MEDIA_CD_GAP
        };
        // The CD cluster is load plus eject only; eject takes the slot a
        // drive cluster gives to swap.
        let (load, eject, _) = cluster(x, PRESENT_HEIGHT + STATUS_CONTROL_Y, STATUS_CONTROL_H);
        layout.cd_load = Some(load);
        layout.cd_eject = Some(eject);
    }
    layout
}

/// Map a cursor position to the status-bar control under it.
fn control_at(pos: (i32, i32), layout: &BarLayout) -> Option<BarControl> {
    for idx in 0..4 {
        if layout.drive_load[idx].is_some_and(|r| r.contains(pos)) {
            return Some(BarControl::DriveLoad(idx));
        }
        if layout.drive_swap[idx].is_some_and(|r| r.contains(pos)) {
            return Some(BarControl::DriveSwap(idx));
        }
        if layout.drive_eject[idx].is_some_and(|r| r.contains(pos)) {
            return Some(BarControl::DriveEject(idx));
        }
    }
    if layout.cd_load.is_some_and(|r| r.contains(pos)) {
        return Some(BarControl::CdLoad);
    }
    if layout.cd_eject.is_some_and(|r| r.contains(pos)) {
        return Some(BarControl::CdEject);
    }
    if shot_button_rect().contains(pos) {
        return Some(BarControl::Screenshot);
    }
    if menu_button_rect().contains(pos) {
        return Some(BarControl::Menu);
    }
    if pause_button_rect().contains(pos) {
        return Some(BarControl::Pause);
    }
    if power_button_rect().contains(pos) {
        return Some(BarControl::Power);
    }
    if reboot_button_rect().contains(pos) {
        return Some(BarControl::Reboot);
    }
    if joystick_toggle_rect().contains(pos) {
        return Some(BarControl::Joystick);
    }
    if volume_control_hit_rect().contains(pos) {
        return Some(BarControl::Volume);
    }
    None
}

fn status_bar_rect() -> Rect {
    Rect {
        x: 0,
        y: PRESENT_HEIGHT,
        w: FB_WIDTH,
        h: STATUS_BAR_HEIGHT,
    }
}

/// One LED row of the front-panel block (label plus LED palette).
struct LedRowSpec {
    label: &'static str,
    on: bool,
    on_color: u32,
    off_color: u32,
    highlight_on: u32,
    highlight_off: u32,
}

/// The LED rows present this session: PWR and FDD always, HDD on IDE
/// machines, CD on CDTV/CD32.
fn led_rows(status: &FrontPanelStatus, powered_on: bool) -> Vec<LedRowSpec> {
    let mut rows = vec![
        LedRowSpec {
            label: "PWR",
            on: powered_on && status.power_led_on,
            on_color: POWER_LED_ON,
            off_color: POWER_LED_OFF,
            highlight_on: rgba(255, 91, 82),
            highlight_off: rgba(90, 27, 24),
        },
        LedRowSpec {
            label: "FDD",
            on: status.fdd_led_on,
            on_color: FDD_LED_ON,
            off_color: FDD_LED_OFF,
            highlight_on: rgba(255, 190, 70),
            highlight_off: rgba(100, 58, 18),
        },
    ];
    if let Some(on) = status.hdd_led {
        rows.push(LedRowSpec {
            label: "HDD",
            on,
            on_color: HDD_LED_ON,
            off_color: HDD_LED_OFF,
            highlight_on: rgba(120, 255, 150),
            highlight_off: rgba(26, 88, 40),
        });
    }
    if let Some(on) = status.cd_led {
        rows.push(LedRowSpec {
            label: "CD",
            on,
            on_color: CD_LED_ON,
            off_color: CD_LED_OFF,
            highlight_on: rgba(140, 214, 255),
            highlight_off: rgba(32, 74, 104),
        });
    }
    rows
}

/// Label y (bar-local) for LED row `row` of `count`. Up to three rows
/// use the classic spacing; four rows pack tighter to stay inside the
/// bar.
fn led_row_label_y(row: usize, count: usize) -> usize {
    if count <= 3 {
        LED_ROW_START_Y + row * LED_ROW_PITCH
    } else {
        LED_ROW_START_Y_TIGHT + row * LED_ROW_PITCH_TIGHT
    }
}

fn led_row_rect(row: usize, count: usize) -> Rect {
    Rect {
        x: STATUS_LED_X,
        y: PRESENT_HEIGHT + led_row_label_y(row, count) + STATUS_LED_Y_OFFSET,
        w: STATUS_LED_W,
        h: STATUS_LED_H,
    }
}

fn fdd_track_counter_rect() -> Rect {
    Rect {
        x: 132,
        y: PRESENT_HEIGHT + STATUS_CONTROL_Y,
        w: 58,
        h: STATUS_CONTROL_H,
    }
}

fn fdd_track_digit_rect(index: usize) -> Rect {
    let display = fdd_track_counter_rect();
    Rect {
        x: display.x + 5 + index * 17,
        y: display.y + 3,
        w: 12,
        h: 16,
    }
}

fn shot_button_rect() -> Rect {
    Rect {
        x: SHOT_BUTTON_X,
        y: PRESENT_HEIGHT + STATUS_CONTROL_Y,
        w: SHOT_BUTTON_W,
        h: STATUS_CONTROL_H,
    }
}

fn menu_button_rect() -> Rect {
    Rect {
        x: ui::MENU_BUTTON_X,
        y: PRESENT_HEIGHT + STATUS_CONTROL_Y,
        w: ui::MENU_BUTTON_W,
        h: STATUS_CONTROL_H,
    }
}

fn volume_control_hit_rect() -> Rect {
    Rect {
        x: VOLUME_SLIDER_X - 8,
        y: PRESENT_HEIGHT + STATUS_CONTROL_Y,
        w: VOLUME_SLIDER_W + 16,
        h: STATUS_CONTROL_H,
    }
}

fn joystick_toggle_rect() -> Rect {
    Rect {
        x: JOY_TOGGLE_X,
        y: PRESENT_HEIGHT + STATUS_CONTROL_Y,
        w: JOY_TOGGLE_W,
        h: STATUS_CONTROL_H,
    }
}

fn volume_slider_track_rect() -> Rect {
    Rect {
        x: VOLUME_SLIDER_X,
        y: PRESENT_HEIGHT + VOLUME_SLIDER_Y,
        w: VOLUME_SLIDER_W,
        h: VOLUME_SLIDER_H,
    }
}

fn volume_slider_knob_rect(percent: u8) -> Rect {
    let track = volume_slider_track_rect();
    let range = track.w.saturating_sub(1).max(1);
    let center = track.x + range * usize::from(percent.min(100)) / 100;
    Rect {
        x: center.saturating_sub(VOLUME_KNOB_W / 2),
        y: PRESENT_HEIGHT + STATUS_CONTROL_Y + (STATUS_CONTROL_H - VOLUME_KNOB_H) / 2,
        w: VOLUME_KNOB_W,
        h: VOLUME_KNOB_H,
    }
}

fn reboot_button_rect() -> Rect {
    Rect {
        x: FB_WIDTH - 58,
        y: PRESENT_HEIGHT + STATUS_CONTROL_Y,
        w: 42,
        h: STATUS_CONTROL_H,
    }
}

fn power_button_rect() -> Rect {
    Rect {
        x: FB_WIDTH - 108,
        y: PRESENT_HEIGHT + STATUS_CONTROL_Y,
        w: 42,
        h: STATUS_CONTROL_H,
    }
}

fn pause_button_rect() -> Rect {
    Rect {
        x: FB_WIDTH - 158,
        y: PRESENT_HEIGHT + STATUS_CONTROL_Y,
        w: 42,
        h: STATUS_CONTROL_H,
    }
}

fn bar_hover_changed(
    layout: &BarLayout,
    previous: Option<(i32, i32)>,
    current: Option<(i32, i32)>,
) -> bool {
    previous.and_then(|pos| control_at(pos, layout))
        != current.and_then(|pos| control_at(pos, layout))
}

fn draw_fdd_track_counter(frame: &mut [u8], track: Option<u8>, texture_scale: usize) {
    let rect = scale_rect(fdd_track_counter_rect(), texture_scale);
    fill_rect(frame, rect, LED_BEZEL_DARK, texture_scale);
    draw_rect_bevel(frame, rect, LED_BEZEL_LIGHT, STATUS_BOTTOM, texture_scale);
    let inset = 2 * texture_scale;
    fill_rect(
        frame,
        Rect {
            x: rect.x + inset,
            y: rect.y + inset,
            w: rect.w.saturating_sub(inset * 2),
            h: rect.h.saturating_sub(inset * 2),
        },
        TRACK_DISPLAY_BG,
        texture_scale,
    );

    let digits = track.map_or([b'-', b'-', b'-'], |track| {
        [
            b'0' + track / 100,
            b'0' + (track / 10) % 10,
            b'0' + track % 10,
        ]
    });
    for (idx, ch) in digits.into_iter().enumerate() {
        draw_seven_segment_digit(
            frame,
            scale_rect(fdd_track_digit_rect(idx), texture_scale),
            ch as char,
            texture_scale,
        );
    }
}

fn draw_volume_control(frame: &mut [u8], percent: u8, texture_scale: usize) {
    let percent = percent.min(100);
    draw_speaker_glyph(frame, texture_scale);

    let rect = scale_rect(volume_slider_track_rect(), texture_scale);
    fill_rect(frame, rect, LED_BEZEL_DARK, texture_scale);
    draw_rect_bevel(frame, rect, LED_BEZEL_LIGHT, STATUS_BOTTOM, texture_scale);

    let inset = 2 * texture_scale;
    let inner = Rect {
        x: rect.x + inset,
        y: rect.y + inset,
        w: rect.w.saturating_sub(inset * 2),
        h: rect.h.saturating_sub(inset * 2),
    };
    fill_rect(frame, inner, TRACK_DISPLAY_BG, texture_scale);

    let fill_w = inner.w * usize::from(percent) / 100;
    if fill_w != 0 {
        let filled = Rect {
            x: inner.x,
            y: inner.y,
            w: fill_w,
            h: inner.h,
        };
        fill_rect(frame, filled, VOLUME_FILL, texture_scale);
        draw_hline_span(
            frame,
            filled.y,
            filled.x,
            filled.x + filled.w,
            VOLUME_FILL_HIGHLIGHT,
            texture_scale,
        );
    }

    let knob = scale_rect(volume_slider_knob_rect(percent), texture_scale);
    fill_rect(frame, knob, BUTTON_FACE, texture_scale);
    draw_rect_bevel(
        frame,
        knob,
        BUTTON_EDGE_LIGHT,
        BUTTON_EDGE_DARK,
        texture_scale,
    );
}

fn draw_button_base(frame: &mut [u8], rect: Rect, hover: bool, texture_scale: usize) {
    let face = if hover {
        BUTTON_FACE_HOVER
    } else {
        BUTTON_FACE
    };
    fill_rect(frame, rect, face, texture_scale);
    draw_rect_bevel(
        frame,
        rect,
        BUTTON_EDGE_LIGHT,
        BUTTON_EDGE_DARK,
        texture_scale,
    );
}

fn draw_disk_button(
    frame: &mut [u8],
    rect: Rect,
    drive_idx: usize,
    hover: bool,
    texture_scale: usize,
) {
    draw_button_base(frame, rect, hover, texture_scale);
    draw_disk_glyph(frame, rect, drive_idx, texture_scale);
}

/// Swap button: two opposed horizontal arrows (cycle to the next queued
/// disk). Drawn dim when there is nothing to swap to.
fn draw_swap_button(
    frame: &mut [u8],
    rect: Rect,
    enabled: bool,
    hover: bool,
    texture_scale: usize,
) {
    draw_button_base(frame, rect, hover && enabled, texture_scale);
    let color = if enabled {
        BUTTON_GLYPH
    } else {
        BUTTON_GLYPH_DISABLED
    };
    let s = texture_scale;
    // Glyph coordinates are designed for a full-height (22) button;
    // recentre vertically for the shorter stacked buttons.
    let dy = glyph_dy(rect, s);
    let fx = rect.x as f32;
    let fy = rect.y as f32 + dy as f32 * s as f32;
    let fs = s as f32;
    let uy = |v: i32| (rect.y as i32 + (v + dy) * s as i32) as usize;
    // Top arrow pointing right.
    fill_rect(
        frame,
        Rect {
            x: rect.x + 2 * s,
            y: uy(7),
            w: 8 * s,
            h: 2 * s,
        },
        color,
        texture_scale,
    );
    fill_triangle(
        frame,
        [
            (fx + 10.0 * fs, fy + 5.0 * fs),
            (fx + 10.0 * fs, fy + 11.0 * fs),
            (fx + 13.5 * fs, fy + 8.0 * fs),
        ],
        color,
        texture_scale,
    );
    // Bottom arrow pointing left.
    fill_rect(
        frame,
        Rect {
            x: rect.x + 6 * s,
            y: uy(13),
            w: 8 * s,
            h: 2 * s,
        },
        color,
        texture_scale,
    );
    fill_triangle(
        frame,
        [
            (fx + 6.0 * fs, fy + 11.0 * fs),
            (fx + 6.0 * fs, fy + 17.0 * fs),
            (fx + 2.5 * fs, fy + 14.0 * fs),
        ],
        color,
        texture_scale,
    );
}

/// Vertical recentring (in unscaled pixels) for glyph art designed for a
/// full-height control drawn in a shorter (stacked) button.
fn glyph_dy(rect: Rect, texture_scale: usize) -> i32 {
    ((rect.h / texture_scale) as i32 - STATUS_CONTROL_H as i32) / 2
}

/// Eject button: up triangle over a bar. Drawn dim when no media is in.
fn draw_eject_button(
    frame: &mut [u8],
    rect: Rect,
    enabled: bool,
    hover: bool,
    texture_scale: usize,
) {
    draw_button_base(frame, rect, hover && enabled, texture_scale);
    let color = if enabled {
        BUTTON_GLYPH
    } else {
        BUTTON_GLYPH_DISABLED
    };
    let s = texture_scale;
    let dy = glyph_dy(rect, s);
    let fx = rect.x as f32;
    let fy = rect.y as f32 + dy as f32 * s as f32;
    let fs = s as f32;
    fill_triangle(
        frame,
        [
            (fx + 8.0 * fs, fy + 5.0 * fs),
            (fx + 2.5 * fs, fy + 12.0 * fs),
            (fx + 13.5 * fs, fy + 12.0 * fs),
        ],
        color,
        texture_scale,
    );
    fill_rect(
        frame,
        Rect {
            x: rect.x + 3 * s,
            y: (rect.y as i32 + (14 + dy) * s as i32) as usize,
            w: 10 * s,
            h: 2 * s,
        },
        color,
        texture_scale,
    );
}

/// CD load/swap button: a compact disc.
fn draw_cd_button(frame: &mut [u8], rect: Rect, hover: bool, texture_scale: usize) {
    draw_button_base(frame, rect, hover, texture_scale);
    let s = texture_scale;
    // Disc centre and radii in unscaled button-local pixels.
    let cx = (rect.x + 11 * s) as f32;
    let cy = rect.y as f32 + rect.h as f32 / 2.0;
    let fs = s as f32;
    for py in rect.y..rect.y + rect.h {
        for px in rect.x..rect.x + rect.w {
            let dx = (px as f32 + 0.5 - cx) / fs;
            let dy = (py as f32 + 0.5 - cy) / fs;
            let r2 = dx * dx + dy * dy;
            let color = if r2 <= 2.2 {
                CD_HOLE
            } else if r2 <= 6.2 {
                CD_HUB
            } else if r2 <= 64.0 {
                // A sheen wedge across the upper-left of the data area.
                if r2 >= 30.0 && dx + dy < -3.0 {
                    CD_SHEEN
                } else {
                    CD_BODY
                }
            } else {
                continue;
            };
            put_pixel(frame, px, py, color, texture_scale);
        }
    }
}

/// Menu button: three stacked bars (opens the pop-up menu).
fn draw_menu_button(frame: &mut [u8], rect: Rect, hover: bool, texture_scale: usize) {
    draw_button_base(frame, rect, hover, texture_scale);
    let s = texture_scale;
    for row in 0..3 {
        fill_rect(
            frame,
            Rect {
                x: rect.x + 4 * s,
                y: rect.y + (6 + row * 4) * s,
                w: 14 * s,
                h: 2 * s,
            },
            BUTTON_GLYPH,
            texture_scale,
        );
    }
}

/// Screenshot button: a small camera.
fn draw_shot_button(frame: &mut [u8], rect: Rect, hover: bool, texture_scale: usize) {
    draw_button_base(frame, rect, hover, texture_scale);
    let s = texture_scale;
    // Viewfinder bump, then the body, then the lens.
    fill_rect(
        frame,
        Rect {
            x: rect.x + 8 * s,
            y: rect.y + 5 * s,
            w: 6 * s,
            h: 3 * s,
        },
        CAMERA_BODY,
        texture_scale,
    );
    fill_rect(
        frame,
        Rect {
            x: rect.x + 3 * s,
            y: rect.y + 7 * s,
            w: 16 * s,
            h: 10 * s,
        },
        CAMERA_BODY,
        texture_scale,
    );
    let cx = (rect.x + 11 * s) as f32;
    let cy = (rect.y + 12 * s) as f32;
    let fs = s as f32;
    for py in rect.y + 7 * s..rect.y + 17 * s {
        for px in rect.x + 5 * s..rect.x + 17 * s {
            let dx = (px as f32 + 0.5 - cx) / fs;
            let dy = (py as f32 + 0.5 - cy) / fs;
            let r2 = dx * dx + dy * dy;
            let color = if r2 <= 5.5 {
                CAMERA_LENS
            } else if r2 <= 12.5 {
                BUTTON_GLYPH
            } else {
                continue;
            };
            put_pixel(frame, px, py, color, texture_scale);
        }
    }
}

/// Speaker glyph labelling the volume slider: a driver box, a cone, and
/// two sound arcs.
fn draw_speaker_glyph(frame: &mut [u8], texture_scale: usize) {
    let s = texture_scale;
    let x = VOLUME_GLYPH_X * s;
    let y = (PRESENT_HEIGHT + STATUS_CONTROL_Y) * s;
    let fs = s as f32;
    fill_rect(
        frame,
        Rect {
            x: x + s,
            y: y + 9 * s,
            w: 3 * s,
            h: 5 * s,
        },
        STATUS_TEXT,
        texture_scale,
    );
    fill_triangle(
        frame,
        [
            (x as f32 + 4.0 * fs, y as f32 + 11.5 * fs),
            (x as f32 + 8.0 * fs, y as f32 + 5.5 * fs),
            (x as f32 + 8.0 * fs, y as f32 + 17.5 * fs),
        ],
        STATUS_TEXT,
        texture_scale,
    );
    draw_vline_span(
        frame,
        x + 10 * s,
        y + 9 * s,
        y + 14 * s,
        STATUS_TEXT,
        texture_scale,
    );
    draw_vline_span(
        frame,
        x + 12 * s,
        y + 6 * s,
        y + 17 * s,
        STATUS_TEXT,
        texture_scale,
    );
}

fn draw_disk_glyph(frame: &mut [u8], rect: Rect, drive_idx: usize, texture_scale: usize) {
    let s = texture_scale;
    // Centre the 16px disk body vertically (full-height buttons give the
    // original 3px margin; stacked buttons less).
    let body_margin_y = (rect.h / s).saturating_sub(16) / 2;
    let body = Rect {
        x: rect.x + 3 * s,
        y: rect.y + body_margin_y * s,
        w: 16 * s,
        h: 16 * s,
    };
    fill_rect(frame, body, DISK_BODY_SHADOW, texture_scale);
    fill_rect(
        frame,
        Rect {
            x: body.x + s,
            y: body.y + s,
            w: body.w.saturating_sub(2 * s),
            h: body.h.saturating_sub(2 * s),
        },
        DISK_BODY,
        texture_scale,
    );
    fill_rect(
        frame,
        Rect {
            x: body.x + s,
            y: body.y + s,
            w: body.w.saturating_sub(2 * s),
            h: s,
        },
        DISK_BODY_HIGHLIGHT,
        texture_scale,
    );
    fill_rect(
        frame,
        Rect {
            x: body.x + s,
            y: body.y + s,
            w: s,
            h: body.h.saturating_sub(2 * s),
        },
        DISK_BODY_HIGHLIGHT,
        texture_scale,
    );
    fill_rect(
        frame,
        Rect {
            x: body.x + 5 * s,
            y: body.y + 2 * s,
            w: 8 * s,
            h: 5 * s,
        },
        DISK_SHUTTER,
        texture_scale,
    );
    fill_rect(
        frame,
        Rect {
            x: body.x + 6 * s,
            y: body.y + 3 * s,
            w: 5 * s,
            h: s,
        },
        DISK_LABEL,
        texture_scale,
    );
    fill_rect(
        frame,
        Rect {
            x: body.x + 5 * s,
            y: body.y + 6 * s,
            w: 8 * s,
            h: s,
        },
        DISK_SHUTTER_DARK,
        texture_scale,
    );
    fill_rect(
        frame,
        Rect {
            x: body.x + 3 * s,
            y: body.y + 9 * s,
            w: 10 * s,
            h: 6 * s,
        },
        DISK_LABEL,
        texture_scale,
    );
    fill_rect(
        frame,
        Rect {
            x: body.x + 4 * s,
            y: body.y + 11 * s,
            w: 5 * s,
            h: s,
        },
        DISK_LABEL_LINE,
        texture_scale,
    );
    fill_rect(
        frame,
        Rect {
            x: body.x + 4 * s,
            y: body.y + 13 * s,
            w: 4 * s,
            h: s,
        },
        DISK_LABEL_LINE,
        texture_scale,
    );
    // The drive number, written on the right of the disk label.
    draw_tiny_digit(
        frame,
        body.x + 9 * s,
        body.y + 9 * s,
        drive_idx as u8,
        DISK_BODY_SHADOW,
        texture_scale,
    );
}

/// 3x5 pixel digits 0-3 for the drive number on the disk-button label.
fn draw_tiny_digit(
    frame: &mut [u8],
    x: usize,
    y: usize,
    digit: u8,
    color: u32,
    texture_scale: usize,
) {
    const GLYPHS: [[u8; 5]; 4] = [
        [0b111, 0b101, 0b101, 0b101, 0b111],
        [0b010, 0b110, 0b010, 0b010, 0b111],
        [0b111, 0b001, 0b111, 0b100, 0b111],
        [0b111, 0b001, 0b011, 0b001, 0b111],
    ];
    let Some(rows) = GLYPHS.get(usize::from(digit)) else {
        return;
    };
    let s = texture_scale;
    for (row, bits) in rows.iter().enumerate() {
        for col in 0..3 {
            if bits & (0b100 >> col) != 0 {
                fill_rect(
                    frame,
                    Rect {
                        x: x + col * s,
                        y: y + row * s,
                        w: s,
                        h: s,
                    },
                    color,
                    texture_scale,
                );
            }
        }
    }
}

fn draw_seven_segment_digit(frame: &mut [u8], rect: Rect, ch: char, texture_scale: usize) {
    const SEG_A: u8 = 1 << 0;
    const SEG_B: u8 = 1 << 1;
    const SEG_C: u8 = 1 << 2;
    const SEG_D: u8 = 1 << 3;
    const SEG_E: u8 = 1 << 4;
    const SEG_F: u8 = 1 << 5;
    const SEG_G: u8 = 1 << 6;

    let mask = match ch {
        '0' => SEG_A | SEG_B | SEG_C | SEG_D | SEG_E | SEG_F,
        '1' => SEG_B | SEG_C,
        '2' => SEG_A | SEG_B | SEG_D | SEG_E | SEG_G,
        '3' => SEG_A | SEG_B | SEG_C | SEG_D | SEG_G,
        '4' => SEG_B | SEG_C | SEG_F | SEG_G,
        '5' => SEG_A | SEG_C | SEG_D | SEG_F | SEG_G,
        '6' => SEG_A | SEG_C | SEG_D | SEG_E | SEG_F | SEG_G,
        '7' => SEG_A | SEG_B | SEG_C,
        '8' => SEG_A | SEG_B | SEG_C | SEG_D | SEG_E | SEG_F | SEG_G,
        '9' => SEG_A | SEG_B | SEG_C | SEG_D | SEG_F | SEG_G,
        '-' => SEG_G,
        _ => 0,
    };
    let thickness = 2 * texture_scale;
    let short = 5 * texture_scale;
    let horizontal = 8 * texture_scale;

    let segments = [
        (
            SEG_A,
            Rect {
                x: rect.x + thickness,
                y: rect.y,
                w: horizontal,
                h: thickness,
            },
        ),
        (
            SEG_B,
            Rect {
                x: rect.x + rect.w - thickness,
                y: rect.y + thickness,
                w: thickness,
                h: short,
            },
        ),
        (
            SEG_C,
            Rect {
                x: rect.x + rect.w - thickness,
                y: rect.y + rect.h - thickness - short,
                w: thickness,
                h: short,
            },
        ),
        (
            SEG_D,
            Rect {
                x: rect.x + thickness,
                y: rect.y + rect.h - thickness,
                w: horizontal,
                h: thickness,
            },
        ),
        (
            SEG_E,
            Rect {
                x: rect.x,
                y: rect.y + rect.h - thickness - short,
                w: thickness,
                h: short,
            },
        ),
        (
            SEG_F,
            Rect {
                x: rect.x,
                y: rect.y + thickness,
                w: thickness,
                h: short,
            },
        ),
        (
            SEG_G,
            Rect {
                x: rect.x + thickness,
                y: rect.y + rect.h / 2 - thickness / 2,
                w: horizontal,
                h: thickness,
            },
        ),
    ];

    for (segment, segment_rect) in segments {
        let lit = mask & segment != 0;
        fill_rect(
            frame,
            segment_rect,
            if lit {
                TRACK_SEGMENT_ON
            } else {
                TRACK_SEGMENT_OFF
            },
            texture_scale,
        );
        if lit {
            draw_hline_span(
                frame,
                segment_rect.y,
                segment_rect.x,
                segment_rect.x + segment_rect.w,
                TRACK_SEGMENT_HIGHLIGHT,
                texture_scale,
            );
        }
    }
}

fn draw_led(
    frame: &mut [u8],
    rect: Rect,
    on: bool,
    on_color: u32,
    off_color: u32,
    on_highlight: u32,
    off_highlight: u32,
    texture_scale: usize,
) {
    fill_rect(frame, rect, LED_BEZEL_DARK, texture_scale);
    draw_rect_bevel(frame, rect, LED_BEZEL_LIGHT, STATUS_BOTTOM, texture_scale);
    let inset = 2 * texture_scale;
    let inner = Rect {
        x: rect.x + inset,
        y: rect.y + inset,
        w: rect.w.saturating_sub(inset * 2),
        h: rect.h.saturating_sub(inset * 2),
    };
    fill_rect(
        frame,
        inner,
        if on { on_color } else { off_color },
        texture_scale,
    );
    for dy in 0..texture_scale {
        draw_hline_span(
            frame,
            inner.y + dy,
            inner.x,
            inner.x + inner.w,
            if on { on_highlight } else { off_highlight },
            texture_scale,
        );
    }
}

fn draw_reboot_button(frame: &mut [u8], rect: Rect, hover: bool, texture_scale: usize) {
    fill_rect(
        frame,
        rect,
        if hover {
            BUTTON_FACE_HOVER
        } else {
            BUTTON_FACE
        },
        texture_scale,
    );
    draw_rect_bevel(
        frame,
        rect,
        BUTTON_EDGE_LIGHT,
        BUTTON_EDGE_DARK,
        texture_scale,
    );
    let cx = rect.x + rect.w / 2;
    let cy = rect.y + rect.h / 2;
    draw_reset_glyph(frame, cx, cy, texture_scale);
}

fn draw_power_button(
    frame: &mut [u8],
    rect: Rect,
    powered_on: bool,
    hover: bool,
    texture_scale: usize,
) {
    fill_rect(
        frame,
        rect,
        if hover {
            BUTTON_FACE_HOVER
        } else {
            BUTTON_FACE
        },
        texture_scale,
    );
    draw_rect_bevel(
        frame,
        rect,
        BUTTON_EDGE_LIGHT,
        BUTTON_EDGE_DARK,
        texture_scale,
    );
    let cx = rect.x + rect.w / 2;
    let cy = rect.y + rect.h / 2;
    let color = if powered_on {
        POWER_GLYPH_ON
    } else {
        POWER_GLYPH_OFF
    };
    draw_power_glyph(frame, cx, cy, color, texture_scale);
}

fn draw_pause_button(
    frame: &mut [u8],
    rect: Rect,
    paused: bool,
    hover: bool,
    texture_scale: usize,
) {
    fill_rect(
        frame,
        rect,
        if hover {
            BUTTON_FACE_HOVER
        } else {
            BUTTON_FACE
        },
        texture_scale,
    );
    draw_rect_bevel(
        frame,
        rect,
        BUTTON_EDGE_LIGHT,
        BUTTON_EDGE_DARK,
        texture_scale,
    );
    let cx = rect.x + rect.w / 2;
    let cy = rect.y + rect.h / 2;
    // Show the action the button performs: a play triangle while paused
    // (click to resume), the twin pause bars while running.
    if paused {
        draw_play_glyph(frame, cx, cy, BUTTON_GLYPH, texture_scale);
    } else {
        draw_pause_glyph(frame, cx, cy, BUTTON_GLYPH, texture_scale);
    }
}

/// Joystick input-source toggle: shows the device currently driving the
/// emulated port-2 joystick (a gamepad in `Gamepad` mode, a keyboard in
/// `Keyboard` mode). Clicking it flips between the two, so the active source is
/// always visible rather than hidden behind a key combination.
fn draw_joystick_button(
    frame: &mut [u8],
    rect: Rect,
    mode: JoystickInputMode,
    hover: bool,
    texture_scale: usize,
) {
    draw_button_base(frame, rect, hover, texture_scale);
    match mode {
        JoystickInputMode::Gamepad => draw_gamepad_glyph(frame, rect, texture_scale),
        JoystickInputMode::Keyboard => draw_keyboard_glyph(frame, rect, texture_scale),
    }
}

/// A small gamepad: a rounded green body with a recessed d-pad on the left and
/// two action buttons on the right.
fn draw_gamepad_glyph(frame: &mut [u8], rect: Rect, texture_scale: usize) {
    let s = texture_scale;
    let mut cell = |x: usize, y: usize, w: usize, h: usize, color: u32| {
        fill_rect(
            frame,
            Rect {
                x: rect.x + x * s,
                y: rect.y + y * s,
                w: w * s,
                h: h * s,
            },
            color,
            texture_scale,
        );
    };
    // Body and the two grip bumps.
    cell(4, 8, 14, 8, BUTTON_GLYPH);
    cell(3, 13, 3, 3, BUTTON_GLYPH);
    cell(16, 13, 3, 3, BUTTON_GLYPH);
    // D-pad cross, cut into the body on the left.
    cell(7, 9, 2, 5, BUTTON_EDGE_DARK);
    cell(5, 11, 6, 2, BUTTON_EDGE_DARK);
    // Two action buttons on the right.
    cell(13, 10, 2, 2, BUTTON_EDGE_DARK);
    cell(15, 12, 2, 2, BUTTON_EDGE_DARK);
}

/// A small keyboard: a recessed dark case holding two rows of green keys and a
/// space bar.
fn draw_keyboard_glyph(frame: &mut [u8], rect: Rect, texture_scale: usize) {
    let s = texture_scale;
    let mut cell = |x: usize, y: usize, w: usize, h: usize, color: u32| {
        fill_rect(
            frame,
            Rect {
                x: rect.x + x * s,
                y: rect.y + y * s,
                w: w * s,
                h: h * s,
            },
            color,
            texture_scale,
        );
    };
    // Case.
    cell(3, 6, 16, 11, BUTTON_EDGE_DARK);
    // Two rows of keys.
    for &kx in &[5, 8, 11, 14] {
        cell(kx, 8, 2, 2, BUTTON_GLYPH);
        cell(kx, 11, 2, 2, BUTTON_GLYPH);
    }
    // Space bar.
    cell(7, 14, 8, 2, BUTTON_GLYPH);
}

/// The pause symbol: two short vertical bars flanking the centre.
fn draw_pause_glyph(frame: &mut [u8], cx: usize, cy: usize, color: u32, texture_scale: usize) {
    let bar_w = 2 * texture_scale;
    let bar_h = 11 * texture_scale;
    let gap = 3 * texture_scale;
    let top = cy.saturating_sub(bar_h / 2);
    let left = cx.saturating_sub(gap / 2 + bar_w);
    let right = cx + gap / 2;
    for x in [left, right] {
        fill_rect(
            frame,
            Rect {
                x,
                y: top,
                w: bar_w,
                h: bar_h,
            },
            color,
            texture_scale,
        );
    }
}

/// The play symbol: a right-pointing filled triangle.
fn draw_play_glyph(frame: &mut [u8], cx: usize, cy: usize, color: u32, texture_scale: usize) {
    let s = texture_scale as f32;
    let half_h = 6.0 * s;
    let width = 11.0 * s;
    let left = cx as f32 - width / 2.0 + 1.0;
    let cyf = cy as f32 + 0.5;
    fill_triangle(
        frame,
        [
            (left, cyf - half_h),
            (left, cyf + half_h),
            (left + width, cyf),
        ],
        color,
        texture_scale,
    );
}

/// The IEC power symbol: a near-closed ring broken at the top, with a
/// vertical bar dropping through the gap toward the centre.
fn draw_power_glyph(frame: &mut [u8], cx: usize, cy: usize, color: u32, texture_scale: usize) {
    let scale = texture_scale as f32;
    let ccx = cx as f32 + 0.5;
    let ccy = cy as f32 + 0.5 + 0.5 * scale;
    let radius = 5.5 * scale;
    let stroke = 1.35 * scale;

    // Ring, swept clockwise from just right of top all the way around to
    // just left of top, leaving a gap centred on 12 o'clock.
    let gap = 0.6_f32;
    let top = -std::f32::consts::FRAC_PI_2;
    let start = top + gap;
    let end = top + std::f32::consts::TAU - gap;
    let steps = 32;
    let mut prev = (ccx + radius * start.cos(), ccy + radius * start.sin());
    for step in 1..=steps {
        let t = start + (end - start) * step as f32 / steps as f32;
        let next = (ccx + radius * t.cos(), ccy + radius * t.sin());
        draw_thick_line(
            frame,
            prev.0,
            prev.1,
            next.0,
            next.1,
            stroke,
            color,
            texture_scale,
        );
        prev = next;
    }

    // Vertical bar from above the ring down to its centre.
    draw_thick_line(
        frame,
        ccx,
        ccy - radius - 1.5 * scale,
        ccx,
        ccy - 0.5 * scale,
        stroke,
        color,
        texture_scale,
    );
}

fn draw_reset_glyph(frame: &mut [u8], cx: usize, cy: usize, texture_scale: usize) {
    let svg_scale = 0.17 * texture_scale as f32;
    let ox = cx as f32 - 8.5 * texture_scale as f32;
    let oy = cy as f32 - 8.5 * texture_scale as f32;
    let map = |x: f32, y: f32| (ox + x * svg_scale, oy + y * svg_scale);
    let stroke = 1.35 * texture_scale as f32;

    let (x0, y0) = map(50.0, 10.0);
    let (x1, y1) = map(50.0, 45.0);
    draw_thick_line(frame, x0, y0, x1, y1, stroke, BUTTON_GLYPH, texture_scale);

    draw_cubic(
        frame,
        [(20.0, 29.0), (4.0, 52.0), (15.0, 90.0), (50.0, 90.0)],
        svg_scale,
        ox,
        oy,
        stroke,
        BUTTON_GLYPH,
        texture_scale,
    );
    draw_cubic(
        frame,
        [(50.0, 90.0), (85.0, 90.0), (100.0, 47.0), (74.0, 20.0)],
        svg_scale,
        ox,
        oy,
        stroke,
        BUTTON_GLYPH,
        texture_scale,
    );

    let arrow = [map(2.0, 21.0), map(31.0, 19.0), map(33.0, 48.0)];
    fill_triangle(frame, arrow, BUTTON_GLYPH, texture_scale);
}

fn draw_cubic(
    frame: &mut [u8],
    p: [(f32, f32); 4],
    scale: f32,
    ox: f32,
    oy: f32,
    radius: f32,
    color: u32,
    texture_scale: usize,
) {
    let mut prev = (ox + p[0].0 * scale, oy + p[0].1 * scale);
    for step in 1..=18 {
        let t = step as f32 / 18.0;
        let mt = 1.0 - t;
        let x = mt * mt * mt * p[0].0
            + 3.0 * mt * mt * t * p[1].0
            + 3.0 * mt * t * t * p[2].0
            + t * t * t * p[3].0;
        let y = mt * mt * mt * p[0].1
            + 3.0 * mt * mt * t * p[1].1
            + 3.0 * mt * t * t * p[2].1
            + t * t * t * p[3].1;
        let next = (ox + x * scale, oy + y * scale);
        draw_thick_line(
            frame,
            prev.0,
            prev.1,
            next.0,
            next.1,
            radius,
            color,
            texture_scale,
        );
        prev = next;
    }
}

fn draw_thick_line(
    frame: &mut [u8],
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    radius: f32,
    color: u32,
    texture_scale: usize,
) {
    let min_x = (x0.min(x1) - radius - 1.0).floor().max(0.0) as usize;
    let max_x = (x0.max(x1) + radius + 1.0)
        .ceil()
        .min((texture_width(texture_scale) - 1) as f32) as usize;
    let min_y = (y0.min(y1) - radius - 1.0).floor().max(0.0) as usize;
    let max_y = (y0.max(y1) + radius + 1.0)
        .ceil()
        .min((texture_height(texture_scale) - 1) as f32) as usize;
    let dx = x1 - x0;
    let dy = y1 - y0;
    let len2 = dx * dx + dy * dy;
    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let t = if len2 == 0.0 {
                0.0
            } else {
                (((px - x0) * dx + (py - y0) * dy) / len2).clamp(0.0, 1.0)
            };
            let nearest_x = x0 + t * dx;
            let nearest_y = y0 + t * dy;
            let dist_x = px - nearest_x;
            let dist_y = py - nearest_y;
            let dist = (dist_x * dist_x + dist_y * dist_y).sqrt();
            let coverage = (radius + 0.5 - dist).clamp(0.0, 1.0);
            if coverage > 0.0 {
                blend_pixel(frame, x, y, color, coverage, texture_scale);
            }
        }
    }
}

fn fill_triangle(frame: &mut [u8], p: [(f32, f32); 3], color: u32, texture_scale: usize) {
    let min_x = p
        .iter()
        .map(|(x, _)| *x)
        .fold(f32::INFINITY, f32::min)
        .floor()
        .max(0.0) as usize;
    let max_x = p
        .iter()
        .map(|(x, _)| *x)
        .fold(f32::NEG_INFINITY, f32::max)
        .ceil()
        .min((texture_width(texture_scale) - 1) as f32) as usize;
    let min_y = p
        .iter()
        .map(|(_, y)| *y)
        .fold(f32::INFINITY, f32::min)
        .floor()
        .max(0.0) as usize;
    let max_y = p
        .iter()
        .map(|(_, y)| *y)
        .fold(f32::NEG_INFINITY, f32::max)
        .ceil()
        .min((texture_height(texture_scale) - 1) as f32) as usize;
    let area = edge(p[0], p[1], p[2]);
    if area == 0.0 {
        return;
    }
    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let mut hits = 0;
            for sy in 0..3 {
                for sx in 0..3 {
                    let point = (
                        x as f32 + (sx as f32 + 0.5) / 3.0,
                        y as f32 + (sy as f32 + 0.5) / 3.0,
                    );
                    let w0 = edge(p[1], p[2], point);
                    let w1 = edge(p[2], p[0], point);
                    let w2 = edge(p[0], p[1], point);
                    if (w0 >= 0.0 && w1 >= 0.0 && w2 >= 0.0)
                        || (w0 <= 0.0 && w1 <= 0.0 && w2 <= 0.0)
                    {
                        hits += 1;
                    }
                }
            }
            if hits > 0 {
                blend_pixel(frame, x, y, color, hits as f32 / 9.0, texture_scale);
            }
        }
    }
}

fn edge(a: (f32, f32), b: (f32, f32), c: (f32, f32)) -> f32 {
    (c.0 - a.0) * (b.1 - a.1) - (c.1 - a.1) * (b.0 - a.0)
}

/// Draw a transient overlay message near the bottom-left of the display
/// region: a translucent panel with the text (plus a 1px drop shadow for
/// legibility over arbitrary video). Operates on the presentation
/// texture, so it is never captured in screenshots.
/// Persistent "(*) REC" badge in the display's top-right corner while a
/// video recording runs. Like the OSD it is drawn into the presentation
/// texture after the frame is captured, so it is never recorded.
fn draw_record_badge(frame: &mut [u8], texture_scale: usize) {
    let s = texture_scale;
    let px = 2 * s;
    let pad = 4 * s;
    let margin = 8 * s;
    let dot_d = 8 * s;
    let gap = 4 * s;

    let text = "REC";
    let text_w = font::text_width(text, px);
    let text_h = font::text_height(px);
    let box_w = dot_d + gap + text_w + 2 * pad;
    let box_h = text_h + 2 * pad;
    let box_x = (FB_WIDTH * s).saturating_sub(margin + box_w);
    let box_y = margin;

    fill_rect_blend(
        frame,
        Rect {
            x: box_x,
            y: box_y,
            w: box_w,
            h: box_h,
        },
        OSD_BG,
        0.68,
        s,
    );
    // Red record dot, centred on the text line.
    let cx = (box_x + pad + dot_d / 2) as f32;
    let cy = (box_y + box_h / 2) as f32;
    let radius = dot_d as f32 / 2.0;
    for y in box_y..box_y + box_h {
        for x in box_x + pad..box_x + pad + dot_d {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            if dx * dx + dy * dy <= radius * radius {
                put_pixel(frame, x, y, RECORD_DOT, s);
            }
        }
    }
    let text_x = box_x + pad + dot_d + gap;
    let text_y = box_y + pad;
    font::draw_text(
        frame,
        texture_width(s),
        texture_height(s),
        text_x + s,
        text_y + s,
        text,
        OSD_SHADOW,
        px,
    );
    font::draw_text(
        frame,
        texture_width(s),
        texture_height(s),
        text_x,
        text_y,
        text,
        OSD_TEXT,
        px,
    );
}

fn draw_osd(frame: &mut [u8], text: &str, texture_scale: usize) {
    let s = texture_scale;
    let px = 2 * s; // font pixel -> device pixels
    let pad = 4 * s;
    let margin = 8 * s;
    let fw = texture_width(s);
    let display_h = PRESENT_HEIGHT * s;

    let text_w = font::text_width(text, px).min(fw.saturating_sub(2 * (margin + pad)));
    let text_h = font::text_height(px);
    let box_w = (text_w + 2 * pad).min(fw.saturating_sub(2 * margin));
    let box_h = text_h + 2 * pad;
    let box_x = margin;
    let box_y = display_h.saturating_sub(margin + box_h);

    fill_rect_blend(
        frame,
        Rect {
            x: box_x,
            y: box_y,
            w: box_w,
            h: box_h,
        },
        OSD_BG,
        0.68,
        s,
    );
    let text_x = box_x + pad;
    let text_y = box_y + pad;
    font::draw_text(
        frame,
        fw,
        texture_height(s),
        text_x + s,
        text_y + s,
        text,
        OSD_SHADOW,
        px,
    );
    font::draw_text(
        frame,
        fw,
        texture_height(s),
        text_x,
        text_y,
        text,
        OSD_TEXT,
        px,
    );
}

/// Fill `rect` by alpha-blending `color` over the existing texture
/// contents. Used for the semi-transparent overlay panel.
pub(super) fn fill_rect_blend(
    frame: &mut [u8],
    rect: Rect,
    color: u32,
    alpha: f32,
    texture_scale: usize,
) {
    let x1 = (rect.x + rect.w).min(texture_width(texture_scale));
    let y1 = (rect.y + rect.h).min(texture_height(texture_scale));
    for y in rect.y.min(texture_height(texture_scale))..y1 {
        for x in rect.x.min(texture_width(texture_scale))..x1 {
            blend_pixel(frame, x, y, color, alpha, texture_scale);
        }
    }
}

fn draw_text(frame: &mut [u8], x: usize, y: usize, text: &str, color: u32, texture_scale: usize) {
    let mut cursor = x;
    for ch in text.chars() {
        if let Some(rows) = glyph(ch) {
            draw_glyph(frame, cursor, y, rows, color, texture_scale);
            cursor += 12 * texture_scale;
        } else {
            cursor += 6 * texture_scale;
        }
    }
}

fn draw_glyph(
    frame: &mut [u8],
    x: usize,
    y: usize,
    rows: [u8; 5],
    color: u32,
    texture_scale: usize,
) {
    let block = 2 * texture_scale;
    for (row_idx, row) in rows.iter().enumerate() {
        for col in 0..5 {
            if row & (1 << (4 - col)) == 0 {
                continue;
            }
            let px = x + col * block;
            let py = y + row_idx * block;
            fill_rect(
                frame,
                Rect {
                    x: px,
                    y: py,
                    w: block,
                    h: block,
                },
                color,
                texture_scale,
            );
        }
    }
}

fn glyph(ch: char) -> Option<[u8; 5]> {
    match ch {
        'C' => Some([0b01110, 0b10000, 0b10000, 0b10000, 0b01110]),
        'D' => Some([0b11100, 0b10010, 0b10010, 0b10010, 0b11100]),
        'F' => Some([0b11110, 0b10000, 0b11100, 0b10000, 0b10000]),
        'H' => Some([0b10010, 0b10010, 0b11110, 0b10010, 0b10010]),
        'L' => Some([0b10000, 0b10000, 0b10000, 0b10000, 0b11110]),
        'O' => Some([0b01110, 0b10001, 0b10001, 0b10001, 0b01110]),
        'P' => Some([0b11110, 0b10010, 0b11110, 0b10000, 0b10000]),
        'R' => Some([0b11110, 0b10010, 0b11110, 0b10100, 0b10010]),
        'V' => Some([0b10001, 0b10001, 0b01010, 0b01010, 0b00100]),
        'W' => Some([0b10001, 0b10001, 0b10101, 0b10101, 0b01010]),
        _ => None,
    }
}

pub(super) fn draw_rect_bevel(
    frame: &mut [u8],
    rect: Rect,
    light: u32,
    dark: u32,
    texture_scale: usize,
) {
    for inset in 0..texture_scale {
        draw_hline_span(
            frame,
            rect.y + inset,
            rect.x,
            rect.x + rect.w,
            light,
            texture_scale,
        );
        draw_vline_span(
            frame,
            rect.x + inset,
            rect.y,
            rect.y + rect.h,
            light,
            texture_scale,
        );
        draw_hline_span(
            frame,
            rect.y + rect.h - 1 - inset,
            rect.x,
            rect.x + rect.w,
            dark,
            texture_scale,
        );
        draw_vline_span(
            frame,
            rect.x + rect.w - 1 - inset,
            rect.y,
            rect.y + rect.h,
            dark,
            texture_scale,
        );
    }
}

fn draw_hline(frame: &mut [u8], y: usize, color: u32, texture_scale: usize) {
    draw_hline_span(
        frame,
        y,
        0,
        texture_width(texture_scale),
        color,
        texture_scale,
    );
}

fn draw_hline_span(
    frame: &mut [u8],
    y: usize,
    x0: usize,
    x1: usize,
    color: u32,
    texture_scale: usize,
) {
    if y >= texture_height(texture_scale) {
        return;
    }
    for x in x0.min(texture_width(texture_scale))..x1.min(texture_width(texture_scale)) {
        put_pixel(frame, x, y, color, texture_scale);
    }
}

fn draw_vline_span(
    frame: &mut [u8],
    x: usize,
    y0: usize,
    y1: usize,
    color: u32,
    texture_scale: usize,
) {
    if x >= texture_width(texture_scale) {
        return;
    }
    for y in y0.min(texture_height(texture_scale))..y1.min(texture_height(texture_scale)) {
        put_pixel(frame, x, y, color, texture_scale);
    }
}

pub(super) fn fill_rect(frame: &mut [u8], rect: Rect, color: u32, texture_scale: usize) {
    let x1 = (rect.x + rect.w).min(texture_width(texture_scale));
    let y1 = (rect.y + rect.h).min(texture_height(texture_scale));
    for y in rect.y.min(texture_height(texture_scale))..y1 {
        for x in rect.x.min(texture_width(texture_scale))..x1 {
            put_pixel(frame, x, y, color, texture_scale);
        }
    }
}

fn put_pixel(frame: &mut [u8], x: usize, y: usize, color: u32, texture_scale: usize) {
    if x >= texture_width(texture_scale) || y >= texture_height(texture_scale) {
        return;
    }
    let off = (y * texture_width(texture_scale) + x) * 4;
    frame[off..off + 4].copy_from_slice(&color.to_le_bytes());
}

fn blend_pixel(frame: &mut [u8], x: usize, y: usize, color: u32, alpha: f32, texture_scale: usize) {
    if alpha >= 1.0 {
        put_pixel(frame, x, y, color, texture_scale);
        return;
    }
    if x >= texture_width(texture_scale) || y >= texture_height(texture_scale) {
        return;
    }
    let alpha = alpha.clamp(0.0, 1.0);
    let off = (y * texture_width(texture_scale) + x) * 4;
    let src = color.to_le_bytes();
    for chan in 0..3 {
        let dst = frame[off + chan] as f32;
        let src = src[chan] as f32;
        frame[off + chan] = (dst + (src - dst) * alpha).round() as u8;
    }
    frame[off + 3] = 0xFF;
}

impl App {
    fn toggle_mouse_capture(&mut self) {
        self.set_mouse_captured(!self.mouse_captured);
    }

    fn set_mouse_captured(&mut self, captured: bool) {
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
            window.set_title(WINDOW_TITLE);
            info!("mouse released");
        }
    }

    fn release_mouse_buttons(&mut self) {
        let input = &mut self.emu.bus_mut().input;
        input.lmb_port1 = false;
        input.rmb_port1 = false;
        input.mmb_port1 = false;
    }

    fn track_uncaptured_cursor_motion(&mut self, pos: Option<(i32, i32)>) {
        let Some(pos) = pos.filter(|p| cursor_in_display(*p)) else {
            self.last_display_cursor_pos = None;
            return;
        };
        if let Some(prev) = self.last_display_cursor_pos {
            let dx = pos.0 - prev.0;
            let dy = pos.1 - prev.1;
            if dx != 0 || dy != 0 {
                self.add_mouse_delta_i32(dx, dy);
            }
        }
        self.last_display_cursor_pos = Some(pos);
    }

    fn add_host_mouse_delta(&mut self, dx: f64, dy: f64) {
        if !dx.is_finite() || !dy.is_finite() {
            return;
        }
        self.mouse_delta_remainder.0 += dx * MOUSE_MOTION_SCALE;
        self.mouse_delta_remainder.1 += dy * MOUSE_MOTION_SCALE;
        let ix = take_integral_mouse_delta(&mut self.mouse_delta_remainder.0);
        let iy = take_integral_mouse_delta(&mut self.mouse_delta_remainder.1);
        if ix != 0 || iy != 0 {
            self.add_mouse_delta_i32(ix, iy);
        }
    }

    fn add_mouse_delta_i32(&mut self, dx: i32, dy: i32) {
        self.emu.bus_mut().input.add_mouse_delta_port1(dx, dy);
        // Reverse-debug: note the motion so replay can reproduce it.
        self.emu
            .tt_note_input(crate::inputsched::ReplayAction::MouseMove { dx, dy });
    }

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

    /// Removable-media status for the bar controls: which drives exist,
    /// what is inserted, and whether a CD drive is fitted this session.
    fn media_bar(&self) -> MediaBar {
        let bus = self.emu.bus();
        let drives = std::array::from_fn(|idx| DriveBar {
            connected: bus.floppy.drive_connected(idx),
            inserted: bus.floppy.disk_inserted(idx),
            multi: self.disk_playlists[idx].len() > 1,
        });
        let cd = bus.cd_drive_present().then(|| bus.cd_disc_inserted());
        MediaBar { drives, cd }
    }

    fn main_ui_control_at(&self, pos: (i32, i32)) -> Option<UiControl> {
        if self.ui.panel.is_none() && self.tool_panel_open() && !self.ui.menu_open {
            return None;
        }
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

    fn tool_panel_open(&self) -> bool {
        self.debugger_panel.is_some() || self.frame_analyzer_panel.is_some()
    }

    fn modal_ui_active(&self) -> bool {
        self.ui.active() || self.tool_panel_open()
    }

    fn tool_window(&self, kind: ToolPanelKind) -> Option<&ToolWindow> {
        match kind {
            ToolPanelKind::Debugger => self.debugger_tool_window.as_ref(),
            ToolPanelKind::FrameAnalyzer => self.frame_analyzer_tool_window.as_ref(),
        }
    }

    fn tool_window_mut(&mut self, kind: ToolPanelKind) -> Option<&mut ToolWindow> {
        match kind {
            ToolPanelKind::Debugger => self.debugger_tool_window.as_mut(),
            ToolPanelKind::FrameAnalyzer => self.frame_analyzer_tool_window.as_mut(),
        }
    }

    fn tool_window_slot(&mut self, kind: ToolPanelKind) -> &mut Option<ToolWindow> {
        match kind {
            ToolPanelKind::Debugger => &mut self.debugger_tool_window,
            ToolPanelKind::FrameAnalyzer => &mut self.frame_analyzer_tool_window,
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
        [
            (ToolPanelKind::Debugger, self.debugger_tool_window.as_ref()),
            (
                ToolPanelKind::FrameAnalyzer,
                self.frame_analyzer_tool_window.as_ref(),
            ),
        ]
        .into_iter()
        .find_map(|(kind, tool)| {
            tool.is_some_and(|tool| tool.window.id() == window_id)
                .then_some(kind)
        })
    }

    fn ui_key_accepts_repeat(&self, kind: Option<ToolPanelKind>, code: KeyCode) -> bool {
        kind == Some(ToolPanelKind::FrameAnalyzer)
            && matches!(
                code,
                KeyCode::ArrowLeft | KeyCode::ArrowRight | KeyCode::ArrowUp | KeyCode::ArrowDown
            )
    }

    fn activate_analyzer_pick_at(&mut self, kind: ToolPanelKind, pos: (i32, i32)) -> bool {
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

    fn handle_tool_window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        kind: ToolPanelKind,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => self.close_tool_panel(kind),
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        state,
                        physical_key: PhysicalKey::Code(code),
                        repeat,
                        ..
                    },
                ..
            } => {
                if state != ElementState::Pressed
                    || (repeat && !self.ui_key_accepts_repeat(Some(kind), code))
                {
                    return;
                }
                if code == KeyCode::KeyQ && host_shortcut_modifier_pressed(self.modifiers) {
                    event_loop.exit();
                } else if !self.ui_handle_tool_key(kind, code) {
                    self.request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
            }
            WindowEvent::CursorMoved { position, .. } => {
                let previous = self.tool_window(kind).and_then(|tool| tool.cursor_pos);
                let pos = self.tool_window(kind).and_then(|tool| {
                    cursor_texture_position(&tool.pixels, position, tool.texture_scale)
                });
                if let Some(tool) = self.tool_window_mut(kind) {
                    tool.cursor_pos = pos;
                }
                if kind == ToolPanelKind::FrameAnalyzer && self.analyzer_dragging {
                    if let Some(pos) = pos {
                        self.activate_analyzer_pick_at(kind, pos);
                    }
                }
                if self.tool_hover_changed(kind, previous, pos) {
                    self.request_redraw();
                }
            }
            WindowEvent::CursorLeft { .. } => {
                let previous = self.tool_window(kind).and_then(|tool| tool.cursor_pos);
                if let Some(tool) = self.tool_window_mut(kind) {
                    tool.cursor_pos = None;
                }
                if kind == ToolPanelKind::FrameAnalyzer {
                    self.analyzer_dragging = false;
                }
                if self.tool_hover_changed(kind, previous, None) {
                    self.request_redraw();
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if button != MouseButton::Left {
                    return;
                }
                if state != ElementState::Pressed {
                    if kind == ToolPanelKind::FrameAnalyzer {
                        self.analyzer_dragging = false;
                    }
                    return;
                }
                if kind == ToolPanelKind::FrameAnalyzer {
                    self.analyzer_dragging = false;
                }
                let control = self
                    .tool_window(kind)
                    .and_then(|tool| tool.cursor_pos)
                    .and_then(|pos| self.tool_panel_control_at(kind, pos));
                if let Some(control) = control {
                    if kind == ToolPanelKind::FrameAnalyzer {
                        self.analyzer_dragging = matches!(control, UiControl::AnalyzerPick { .. });
                    }
                    self.activate_tool_control(kind, control);
                    self.ensure_tool_windows_for_open_panels(event_loop);
                }
            }
            WindowEvent::Resized(size) => {
                if let Some(tool) = self.tool_window_mut(kind) {
                    let _ = tool
                        .pixels
                        .resize_surface(size.width.max(1), size.height.max(1));
                }
                self.request_redraw();
            }
            WindowEvent::RedrawRequested => self.draw_tool_window(kind),
            _ => {}
        }
    }

    fn draw_tool_window(&mut self, kind: ToolPanelKind) {
        let Some(panel) = self.tool_panel_for_kind(kind) else {
            *self.tool_window_slot(kind) = None;
            return;
        };
        let ui_data = self.build_tool_panel_view_data(kind);
        let hover = self
            .tool_window(kind)
            .and_then(|tool| tool.cursor_pos)
            .and_then(|pos| ui::panel_control_at(&panel, pos));
        if let Some(tool) = self.tool_window_mut(kind) {
            let frame = tool.pixels.frame_mut();
            frame.fill(0);
            ui::draw_panel_layer(frame, tool.texture_scale, &panel, hover, ui_data.as_ref());
            if let Err(e) = tool.pixels.render() {
                error!("tool pixels.render: {e}");
            }
        }
    }

    /// Run the action behind a clicked status-bar control (volume is
    /// handled separately because it starts a drag).
    fn activate_bar_control(&mut self, control: BarControl) {
        match control {
            BarControl::Power => self.toggle_power(),
            BarControl::Pause => self.toggle_pause(),
            BarControl::Reboot => self.reset_emulator(true),
            BarControl::Screenshot => self.take_screenshot(),
            BarControl::Menu => {
                self.ui.menu_open = !self.ui.menu_open;
                self.request_redraw();
            }
            BarControl::DriveLoad(idx) => self.load_drive_disks_from_dialog(idx),
            BarControl::DriveSwap(idx) => self.swap_drive_disk(idx),
            BarControl::DriveEject(idx) => self.eject_drive_disk(idx),
            BarControl::CdLoad => self.load_cd_from_dialog(),
            BarControl::CdEject => self.eject_cd(),
            BarControl::Joystick => {
                self.cycle_joystick_input_mode();
                self.request_redraw();
            }
            BarControl::Volume => {}
        }
    }

    /// Run the action behind a clicked menu item or panel control.
    #[cfg(test)]
    fn activate_ui_control(&mut self, control: UiControl) {
        self.activate_ui_control_with_event_loop(control, None);
    }

    fn activate_ui_control_with_event_loop(
        &mut self,
        control: UiControl,
        event_loop: Option<&ActiveEventLoop>,
    ) {
        match control {
            UiControl::MenuItem(item) => {
                self.ui.menu_open = false;
                match item {
                    ui::MenuItem::FrameAnalyzer => self.open_frame_analyzer(),
                    ui::MenuItem::About => self.ui.panel = Some(Panel::About),
                    ui::MenuItem::Shortcuts => self.ui.panel = Some(Panel::Shortcuts),
                    ui::MenuItem::Calibration => {
                        self.ui.panel =
                            Some(Panel::Calibration(crate::gamepad::CalibrationSession::new()));
                    }
                    ui::MenuItem::Debugger => self.open_debugger(),
                    ui::MenuItem::JoystickInput => self.cycle_joystick_input_mode(),
                    ui::MenuItem::Warp => self.toggle_warp(),
                    ui::MenuItem::WarpLimit => self.cycle_warp_speed(),
                    ui::MenuItem::Record => self.toggle_recording(),
                    ui::MenuItem::RecordInput => self.toggle_input_recording(),
                    ui::MenuItem::SaveState => self.save_state_interactive(),
                    ui::MenuItem::LoadState => self.load_state_from_dialog(event_loop),
                    ui::MenuItem::LoadRom => self.load_rom_from_dialog(),
                    ui::MenuItem::MachineConfig => self.open_launcher(),
                }
            }
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
            UiControl::DebugBreaksClear => {
                self.activate_tool_control(ToolPanelKind::Debugger, control)
            }
            UiControl::AnalyzerRun => {
                self.activate_tool_control(ToolPanelKind::FrameAnalyzer, control)
            }
            UiControl::AnalyzerFrame => {
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
            UiControl::LauncherTab(tab) => {
                if let Some(state) = self.launcher_state_mut() {
                    state.tab = tab;
                }
            }
            UiControl::LauncherCycle { field, forward } => {
                if let Some(state) = self.launcher_state_mut() {
                    state.setup.cycle(field, forward);
                    state.status = None;
                }
            }
            UiControl::LauncherToggle(field) => {
                if let Some(state) = self.launcher_state_mut() {
                    state.setup.toggle(field);
                    state.status = None;
                }
            }
            UiControl::LauncherClear(field) => {
                if let Some(state) = self.launcher_state_mut() {
                    state.edit_cancel();
                    state.setup.clear_path(field);
                    state.status = None;
                }
            }
            UiControl::LauncherDriveNameEdit(field) => {
                if let Some(state) = self.launcher_state_mut() {
                    state.begin_edit_drive_name(field);
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
                    state.status = Some(StatusMessage::ok("Reset to defaults"));
                }
            }
            UiControl::LauncherBrowse(field) => self.launcher_browse(field),
            UiControl::LauncherZorroAdd => self.launcher_add_zorro(),
            UiControl::LauncherLoad => self.launcher_load(),
            UiControl::LauncherSave => self.launcher_save(),
            UiControl::LauncherRun => self.launcher_run(),
        }
        self.request_redraw();
    }

    fn activate_tool_control(&mut self, kind: ToolPanelKind, control: UiControl) {
        match (kind, control) {
            (ToolPanelKind::Debugger, UiControl::PanelClose) => self.close_tool_panel(kind),
            (ToolPanelKind::FrameAnalyzer, UiControl::PanelClose) => self.close_tool_panel(kind),
            (ToolPanelKind::Debugger, UiControl::PanelBody)
            | (ToolPanelKind::FrameAnalyzer, UiControl::PanelBody) => {}
            (ToolPanelKind::Debugger, UiControl::DebugTab(tab)) => {
                if let Some(panel) = self.debugger_panel.as_mut() {
                    panel.tab = tab;
                }
            }
            (ToolPanelKind::Debugger, UiControl::DebugRun) => self.debugger_toggle_run(),
            (ToolPanelKind::Debugger, UiControl::DebugStep) => self.debugger_step(),
            (ToolPanelKind::Debugger, UiControl::DebugStepOver) => self.debugger_step_over(),
            (ToolPanelKind::Debugger, UiControl::DebugStepOut) => self.debugger_step_out(),
            (ToolPanelKind::Debugger, UiControl::DebugStepFrame) => self.debugger_step_frame(),
            (ToolPanelKind::Debugger, UiControl::DebugRunTo) => self.debugger_run_to(),
            (ToolPanelKind::Debugger, UiControl::DebugReverseStep) => self.debugger_reverse_step(),
            (ToolPanelKind::Debugger, UiControl::DebugReverseFrame) => {
                self.debugger_reverse_frame()
            }
            (ToolPanelKind::Debugger, UiControl::DebugReverseRun) => {
                self.debugger_reverse_continue()
            }
            (ToolPanelKind::Debugger, UiControl::DebugMemPrev) => self.debugger_mem_page(-1),
            (ToolPanelKind::Debugger, UiControl::DebugMemNext) => self.debugger_mem_page(1),
            (ToolPanelKind::Debugger, UiControl::DebugPoke) => self.debugger_poke(),
            (ToolPanelKind::Debugger, UiControl::DebugEntry) => {
                if let Some(panel) = self.debugger_panel.as_mut() {
                    panel.entry_active = true;
                }
            }
            (ToolPanelKind::Debugger, UiControl::DebugBreakToggle) => {
                self.debugger_toggle_breakpoint()
            }
            (ToolPanelKind::Debugger, UiControl::DebugWatchToggle) => {
                self.debugger_toggle_watchpoint()
            }
            (ToolPanelKind::Debugger, UiControl::DebugRegToggle) => {
                self.debugger_toggle_reg_watch()
            }
            (ToolPanelKind::Debugger, UiControl::DebugBreaksClear) => {
                self.emu.machine.ui_breaks_clear();
                self.last_debug_stop = None;
                self.show_osd("Cleared all breakpoints and watchpoints");
            }
            (ToolPanelKind::FrameAnalyzer, UiControl::AnalyzerRun) => {
                self.frame_analyzer_toggle_run()
            }
            (ToolPanelKind::FrameAnalyzer, UiControl::AnalyzerFrame) => {
                self.frame_analyzer_step_frame()
            }
            (ToolPanelKind::FrameAnalyzer, UiControl::AnalyzerPick { x, y, scanline }) => {
                self.frame_analyzer_select(x, y, scanline)
            }
            _ => {}
        }
        self.request_redraw();
    }

    /// Keys consumed by the open menu/panel (Escape, debugger hex entry).
    /// Returns true when the key was handled and must not reach the Amiga.
    fn ui_handle_key(&mut self, code: KeyCode) -> bool {
        if self.ui.active() {
            if code == KeyCode::Escape {
                // While typing into a plugin option, Escape cancels the edit
                // rather than closing the panel.
                if self.launcher_cancel_edit_if_active() {
                    return true;
                }
                if self.ui.menu_open {
                    self.ui.menu_open = false;
                    self.request_redraw();
                } else {
                    self.close_panel();
                }
                return true;
            }
            // Route keys to a focused plugin-option text field, if any.
            if self.launcher_handle_edit_key(code) {
                return true;
            }
            return false;
        }
        self.default_tool_key_kind()
            .is_some_and(|kind| self.ui_handle_tool_key(kind, code))
    }

    /// Cancel an in-progress plugin-option text edit, if one is focused.
    fn launcher_cancel_edit_if_active(&mut self) -> bool {
        let cancelled = matches!(
            self.launcher_state_mut(),
            Some(state) if state.editing().is_some()
        );
        if cancelled {
            if let Some(state) = self.launcher_state_mut() {
                state.edit_cancel();
            }
            self.request_redraw();
        }
        cancelled
    }

    /// Feed a key to a focused plugin-option text field. Returns false (so the
    /// key falls through) when no field is being edited.
    fn launcher_handle_edit_key(&mut self, code: KeyCode) -> bool {
        let handled = {
            let Some(state) = self.launcher_state_mut() else {
                return false;
            };
            if state.editing().is_none() {
                return false;
            }
            if let Some(ch) = entry_char_for_key(code) {
                state.edit_push(ch);
            } else {
                match code {
                    KeyCode::Backspace => state.edit_backspace(),
                    KeyCode::Enter | KeyCode::NumpadEnter => state.edit_commit(),
                    // Swallow other keys while a field has focus.
                    _ => {}
                }
            }
            true
        };
        if handled {
            self.request_redraw();
        }
        handled
    }

    fn default_tool_key_kind(&self) -> Option<ToolPanelKind> {
        if self.debugger_panel.is_some() {
            Some(ToolPanelKind::Debugger)
        } else if self.frame_analyzer_panel.is_some() {
            Some(ToolPanelKind::FrameAnalyzer)
        } else {
            None
        }
    }

    fn ui_handle_tool_key(&mut self, kind: ToolPanelKind, code: KeyCode) -> bool {
        if code == KeyCode::Escape {
            self.close_tool_panel(kind);
            return true;
        }
        match kind {
            ToolPanelKind::Debugger => self.ui_handle_debugger_key(code),
            ToolPanelKind::FrameAnalyzer => self.ui_handle_frame_analyzer_key(code),
        }
    }

    fn ui_handle_debugger_key(&mut self, code: KeyCode) -> bool {
        let Some(panel) = self.debugger_panel.as_mut() else {
            return false;
        };
        if panel.entry_active {
            if let Some(ch) = entry_char_for_key(code) {
                panel.push_entry_char(ch);
                self.request_redraw();
                return true;
            }
            match code {
                KeyCode::Backspace => {
                    panel.backspace_entry();
                    self.request_redraw();
                    return true;
                }
                KeyCode::Enter | KeyCode::NumpadEnter => {
                    match panel.tab {
                        ui::DebugTab::Memory => {
                            if let Some(addr) = panel.entry_addr() {
                                panel.mem_addr = addr & !0xF;
                            }
                        }
                        // On the CPU tab, Enter pins the disassembly to
                        // the typed address; an empty box follows the PC again.
                        ui::DebugTab::Cpu => panel.disasm_addr = panel.entry_addr(),
                        _ => {}
                    }
                    panel.entry_active = false;
                    self.request_redraw();
                    return true;
                }
                _ => {}
            }
        }
        if panel.entry_active {
            return false;
        }
        let control = match code {
            KeyCode::KeyS => Some(UiControl::DebugStep),
            KeyCode::KeyO => Some(UiControl::DebugStepOver),
            KeyCode::KeyU => Some(UiControl::DebugStepOut),
            KeyCode::KeyF => Some(UiControl::DebugStepFrame),
            KeyCode::KeyR => Some(UiControl::DebugRun),
            _ => None,
        };
        if let Some(control) = control {
            self.activate_tool_control(ToolPanelKind::Debugger, control);
            return true;
        }
        false
    }

    fn ui_handle_frame_analyzer_key(&mut self, code: KeyCode) -> bool {
        if self.frame_analyzer_panel.is_none() {
            return false;
        }
        let control = match code {
            KeyCode::KeyF => Some(UiControl::AnalyzerFrame),
            KeyCode::KeyR => Some(UiControl::AnalyzerRun),
            _ => None,
        };
        if let Some(control) = control {
            self.activate_tool_control(ToolPanelKind::FrameAnalyzer, control);
            return true;
        }
        let delta = match code {
            KeyCode::ArrowLeft => Some((-1, 0)),
            KeyCode::ArrowRight => Some((1, 0)),
            KeyCode::ArrowUp => Some((0, -1)),
            KeyCode::ArrowDown => Some((0, 1)),
            _ => None,
        };
        if let Some((dhpos, dvpos)) = delta {
            self.frame_analyzer_move_selection(dhpos, dvpos);
            return true;
        }
        false
    }

    /// Open the debugger window (pausing the machine), or close it again
    /// if it is already open (the host shortcut toggle).
    fn toggle_debugger(&mut self) {
        if self.debugger_panel.is_some() {
            self.close_tool_panel(ToolPanelKind::Debugger);
        } else {
            self.ui.menu_open = false;
            self.open_debugger();
            self.request_redraw();
        }
    }

    fn open_debugger(&mut self) {
        if self.debugger_panel.is_none() {
            // The debugger shortcut can arrive while the mouse is captured;
            // release it so the window's controls are reachable.
            self.set_mouse_captured(false);
            self.ui.panel = None;
            self.paused_before_debugger = self.paused;
            self.paused = true;
            self.sync_live_audio_suspension();
            let mut panel = ui::DebuggerPanel::new();
            // Start the memory view at the current program counter's
            // neighbourhood; it is usually what you came to look at.
            panel.mem_addr = self.emu.machine.pc() & 0x00FF_FFF0;
            self.debugger_panel = Some(panel);
            // Arm reverse debugging so the < Step / < Run controls work. A
            // conservative interval keeps the per-snapshot serialize off the
            // critical path; captures only accrue while the machine advances
            // (Run / Step Frame inside the debugger), not while paused.
            if !self.emu.time_travel_enabled() {
                self.emu.enable_time_travel(
                    crate::debugger::RR_DEFAULT_BUDGET_MB,
                    DEBUGGER_REVERSE_INTERVAL_FRAMES,
                );
            }
        }
    }

    fn tool_window_title(kind: ToolPanelKind) -> &'static str {
        match kind {
            ToolPanelKind::Debugger => "Copperline Debugger",
            ToolPanelKind::FrameAnalyzer => "Copperline Frame Analyzer",
        }
    }

    fn tool_panel_is_open(&self, kind: ToolPanelKind) -> bool {
        match kind {
            ToolPanelKind::Debugger => self.debugger_panel.is_some(),
            ToolPanelKind::FrameAnalyzer => self.frame_analyzer_panel.is_some(),
        }
    }

    fn ensure_tool_windows_for_open_panels(&mut self, event_loop: &ActiveEventLoop) {
        self.ensure_tool_window_for_kind(event_loop, ToolPanelKind::Debugger);
        self.ensure_tool_window_for_kind(event_loop, ToolPanelKind::FrameAnalyzer);
    }

    fn ensure_tool_window_for_kind(&mut self, event_loop: &ActiveEventLoop, kind: ToolPanelKind) {
        if !self.tool_panel_is_open(kind) {
            *self.tool_window_slot(kind) = None;
            return;
        }
        let title = Self::tool_window_title(kind);
        if let Some(tool) = self.tool_window(kind) {
            tool.window.set_title(title);
            tool.window.request_redraw();
            return;
        }

        let size = LogicalSize::new(FB_WIDTH as f64, WINDOW_PRESENT_HEIGHT as f64);
        let attrs = WindowAttributes::default()
            .with_title(title)
            .with_window_icon(copperline_window_icon())
            .with_inner_size(size)
            .with_min_inner_size(LogicalSize::new(
                FB_WIDTH as f64 / 2.0,
                WINDOW_PRESENT_HEIGHT as f64 / 2.0,
            ));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                warn!("create tool window failed: {e}");
                return;
            }
        };
        let texture_scale = texture_scale_for_window(&window);
        let pixels = match build_pixels_for_window(window.clone(), texture_scale) {
            Ok(p) => p,
            Err(e) => {
                warn!("tool window pixels init failed: {e}");
                return;
            }
        };
        info!(
            "tool window ready: {title} (texture {}x{})",
            texture_width(texture_scale),
            texture_height(texture_scale)
        );
        *self.tool_window_slot(kind) = Some(ToolWindow {
            window,
            pixels,
            texture_scale,
            cursor_pos: None,
        });
        self.request_redraw();
    }

    fn open_frame_analyzer(&mut self) {
        if self.frame_analyzer_panel.is_none() {
            self.set_mouse_captured(false);
            self.ui.panel = None;
            self.paused_before_analyzer = self.paused;
            self.paused = true;
            self.sync_live_audio_suspension();
            self.emu.bus_mut().set_frame_analyzer_enabled(true);
            self.frame_analyzer_panel = Some(ui::FrameAnalyzerPanel::new());
        }
    }

    fn frame_analyzer_toggle_run(&mut self) {
        self.paused = !self.paused;
        self.paused_before_analyzer = self.paused;
        self.sync_live_audio_suspension();
        if !self.paused {
            self.emu.bus_mut().set_frame_analyzer_enabled(true);
        }
    }

    fn frame_analyzer_step_frame(&mut self) {
        self.emu.bus_mut().set_frame_analyzer_enabled(true);
        self.debugger_step_frame();
    }

    fn frame_analyzer_select(&mut self, x: u16, y: u16, scanline: bool) {
        let Some(trace) = self.emu.bus().frame_bus_trace() else {
            return;
        };
        let hpos = (usize::from(x) * trace.cols / 1024).min(trace.cols.saturating_sub(1));
        let vpos = if scanline {
            self.frame_analyzer_panel
                .as_ref()
                .map(|panel| panel.selected_vpos as usize)
                .unwrap_or(trace.visible_start_vpos as usize)
        } else {
            (usize::from(y) * trace.rows / 1024).min(trace.rows.saturating_sub(1))
        };
        if let Some(panel) = self.frame_analyzer_panel.as_mut() {
            panel.selected_hpos = hpos.min(u16::MAX as usize) as u16;
            panel.selected_vpos = vpos.min(u16::MAX as usize) as u16;
        }
    }

    fn frame_analyzer_move_selection(&mut self, dhpos: i16, dvpos: i16) {
        let Some((max_hpos, max_vpos)) = self.emu.bus().frame_bus_trace().map(|trace| {
            (
                trace.cols.saturating_sub(1).min(u16::MAX as usize) as i32,
                trace.rows.saturating_sub(1).min(u16::MAX as usize) as i32,
            )
        }) else {
            return;
        };
        if let Some(panel) = self.frame_analyzer_panel.as_mut() {
            let hpos =
                (i32::from(panel.selected_hpos) + i32::from(dhpos)).clamp(0, max_hpos) as u16;
            let vpos =
                (i32::from(panel.selected_vpos) + i32::from(dvpos)).clamp(0, max_vpos) as u16;
            if panel.selected_hpos != hpos || panel.selected_vpos != vpos {
                panel.selected_hpos = hpos;
                panel.selected_vpos = vpos;
                self.request_redraw();
            }
        }
    }

    /// Close the open main-window overlay panel.
    fn close_panel(&mut self) {
        self.analyzer_dragging = false;
        self.ui.panel = None;
        self.resize_for_active_panel();
        self.request_redraw();
    }

    /// Open the machine-configuration screen, seeded from the running (or
    /// last-applied) machine so it reflects the current settings.
    pub(crate) fn open_launcher(&mut self) {
        self.ui.menu_open = false;
        self.ui.panel = Some(Panel::Launcher(Box::new(LauncherState::from_raw(
            &self.machine_config,
        ))));
        self.resize_for_active_panel();
        self.request_redraw();
    }

    fn launcher_state(&self) -> Option<&LauncherState> {
        match self.ui.panel.as_ref() {
            Some(Panel::Launcher(state)) => Some(state.as_ref()),
            _ => None,
        }
    }

    fn launcher_state_mut(&mut self) -> Option<&mut LauncherState> {
        match self.ui.panel.as_mut() {
            Some(Panel::Launcher(state)) => Some(state.as_mut()),
            _ => None,
        }
    }

    fn set_launcher_status(&mut self, status: StatusMessage) {
        if let Some(state) = self.launcher_state_mut() {
            state.status = Some(status);
        }
    }

    /// Open a native file dialog for a configuration-screen path field, seeded
    /// at the field's current directory, and store the picked path.
    fn launcher_browse(&mut self, field: LauncherField) {
        let start_dir = self
            .launcher_state()
            .and_then(|s| s.setup.path(field))
            .and_then(|p| p.parent().map(|d| d.to_path_buf()));
        self.suspend_live_audio_for_host_io();
        let mut dialog = rfd::FileDialog::new().set_title("Select file");
        dialog = match field {
            LauncherField::Rom
            | LauncherField::ExtendedRom
            | LauncherField::ScsiRom
            | LauncherField::ScsiRomOdd => dialog.add_filter("ROM images", &["rom", "bin"]),
            LauncherField::Df0Image
            | LauncherField::Df1Image
            | LauncherField::Df2Image
            | LauncherField::Df3Image => {
                dialog.add_filter("Floppy images", &["adf", "adz", "dms", "scp", "gz", "ipf"])
            }
            LauncherField::CdImage => dialog.add_filter("CD images", &["cue", "iso", "bin"]),
            LauncherField::Cd32Nvram => dialog.add_filter("NVRAM images", &["bin", "nv", "sav"]),
            _ => dialog.add_filter("Hard disk images", &["hdf", "img", "bin"]),
        };
        if let Some(dir) = start_dir {
            dialog = dialog.set_directory(dir);
        }
        let picked = dialog.pick_file();
        if let Some(path) = picked {
            if let Some(state) = self.launcher_state_mut() {
                // A pending volume-name edit (on this or another drive row)
                // would otherwise be left visually focused after the dialog.
                state.edit_cancel();
                state.setup.set_path(field, path);
                state.status = None;
            }
        }
        self.finish_host_io_pause();
    }

    fn launcher_add_zorro(&mut self) {
        self.suspend_live_audio_for_host_io();
        let picked = rfd::FileDialog::new()
            .set_title("Add Zorro board metadata")
            .add_filter("Board metadata", &["toml"])
            .pick_file();
        if let Some(path) = picked {
            if let Some(state) = self.launcher_state_mut() {
                state.setup.add_zorro(path);
                state.status = None;
            }
        }
        self.finish_host_io_pause();
    }

    /// Pick a file for a plugin board's file-typed config option.
    fn launcher_board_browse(&mut self, board: usize, opt: usize) {
        self.suspend_live_audio_for_host_io();
        let picked = rfd::FileDialog::new()
            .set_title("Choose plugin file")
            .pick_file();
        if let Some(path) = picked {
            if let Some(state) = self.launcher_state_mut() {
                state.edit_cancel();
                state
                    .setup
                    .zorro_option_set(board, opt, path.to_string_lossy().into_owned());
                state.status = None;
            }
        }
        self.finish_host_io_pause();
    }

    fn launcher_load(&mut self) {
        self.suspend_live_audio_for_host_io();
        let picked = rfd::FileDialog::new()
            .set_title("Load configuration")
            .add_filter("Copperline config", &["toml"])
            .pick_file();
        if let Some(path) = picked {
            match MachineSetup::load_from(&path) {
                Ok(setup) => {
                    if let Some(state) = self.launcher_state_mut() {
                        state.setup = setup;
                        state.status = Some(StatusMessage::ok(format!(
                            "Loaded {}",
                            display_file_name(&path)
                        )));
                    }
                }
                Err(e) => {
                    warn!("config load failed ({}): {e:#}", path.display());
                    self.set_launcher_status(StatusMessage::err(format!(
                        "Load failed: {}",
                        short_status_error(&e)
                    )));
                }
            }
        }
        self.finish_host_io_pause();
    }

    fn launcher_save(&mut self) {
        // Capture a name/option typed but not yet committed with Enter.
        if let Some(state) = self.launcher_state_mut() {
            state.edit_commit();
        }
        let toml = match self.launcher_state().map(|s| s.setup.to_toml()) {
            Some(Ok(text)) => text,
            Some(Err(e)) => {
                self.set_launcher_status(StatusMessage::err(format!(
                    "Save failed: {}",
                    short_status_error(&e)
                )));
                return;
            }
            None => return,
        };
        self.suspend_live_audio_for_host_io();
        let picked = rfd::FileDialog::new()
            .set_title("Save configuration")
            .add_filter("Copperline config", &["toml"])
            .set_file_name("machine.toml")
            .save_file();
        if let Some(path) = picked {
            match std::fs::write(&path, toml) {
                Ok(()) => self.set_launcher_status(StatusMessage::ok(format!(
                    "Saved {}",
                    display_file_name(&path)
                ))),
                Err(e) => {
                    warn!("config save failed ({}): {e}", path.display());
                    self.set_launcher_status(StatusMessage::err("Save failed (see log)"));
                }
            }
        }
        self.finish_host_io_pause();
    }

    /// Build and start the configured machine (the Run button). Validation,
    /// AROS resolution, audio-device and machine-construction errors all stay
    /// in the panel as a status line; only success swaps the live machine.
    fn launcher_run(&mut self) {
        // Capture a name/option typed but not yet committed with Enter.
        if let Some(state) = self.launcher_state_mut() {
            state.edit_commit();
        }
        let mut cfg = match self.launcher_state().map(|s| s.setup.build_config()) {
            Some(Ok(cfg)) => cfg,
            Some(Err(e)) => {
                self.set_launcher_status(StatusMessage::err(short_status_error(&e)));
                return;
            }
            None => return,
        };
        if let Err(e) = crate::resolve_bundled_rom(&mut cfg) {
            self.set_launcher_status(StatusMessage::err(short_status_error(&e)));
            return;
        }
        let realtime = crate::priority::requested(cfg.emulation.realtime_priority);
        let audio: Box<dyn AudioSink> = match CpalSink::new(realtime) {
            Ok(sink) => Box::new(sink),
            Err(e) => {
                self.set_launcher_status(StatusMessage::err(format!(
                    "Audio init failed: {}",
                    short_status_error(&e)
                )));
                return;
            }
        };
        // The launcher boots a fresh machine, never a save state, so a real
        // ROM is required here.
        let emu = match crate::build_machine(&cfg, audio, true, false) {
            Ok(emu) => emu,
            Err(e) => {
                self.set_launcher_status(StatusMessage::err(short_status_error(&e)));
                return;
            }
        };
        let raw = self
            .launcher_state()
            .map(|s| s.setup.to_raw())
            .unwrap_or_default();
        self.run_machine(emu, &cfg, raw);
    }

    /// Replace the live machine with a freshly built one (configuration screen
    /// Run), refreshing the host-side presentation/runtime state to match and
    /// powering it on. The previous (placeholder or running) machine, and its
    /// audio sink, are dropped here.
    fn run_machine(&mut self, emu: Emulator, cfg: &Config, raw: RawConfig) {
        self.emu = emu;
        self.machine_config = raw;
        self.disk_playlists = cfg.floppy_playlists.clone();
        self.disk_write_protected = std::array::from_fn(|i| {
            cfg.floppy.drives[i]
                .as_ref()
                .map(|d| d.write_protected)
                .unwrap_or(true)
        });
        self.disk_playlist_index = [0; 4];
        self.overscan = crate::resolve_overscan(cfg.overscan);
        self.warp_speed = cfg.emulation.warp_speed;
        // Reset the host joystick source to the new machine's configured
        // start-up mode (a previous live Cmd+J toggle does not carry over).
        self.joystick_input_mode = cfg.joystick_input_mode;
        self.keyboard_joy_held = KeyboardJoystickHeld::default();
        self.about_machine_lines = crate::about_machine_lines(cfg);
        self.deinterlacer = Deinterlacer::with_phosphor(crate::resolve_phosphor(cfg.phosphor));
        self.ui.menu_open = false;
        self.ui.panel = None;
        self.powered_on = true;
        self.cpu_halted = false;
        self.paused = false;
        self.reset_render_pipeline();
        self.resize_for_active_panel();
        self.show_osd("Machine started");
        self.request_redraw();
    }

    fn close_tool_panel(&mut self, kind: ToolPanelKind) {
        match kind {
            ToolPanelKind::Debugger => {
                if self.debugger_panel.is_some() {
                    self.paused = self.paused_before_debugger;
                    self.last_debug_stop = None;
                    self.sync_live_audio_suspension();
                }
                self.debugger_panel = None;
                self.debugger_tool_window = None;
            }
            ToolPanelKind::FrameAnalyzer => {
                if self.frame_analyzer_panel.is_some() {
                    self.paused = self.paused_before_analyzer;
                    self.emu.bus_mut().set_frame_analyzer_enabled(false);
                    self.sync_live_audio_suspension();
                }
                self.analyzer_dragging = false;
                self.frame_analyzer_panel = None;
                self.frame_analyzer_tool_window = None;
            }
        }
        self.resize_for_active_panel();
        self.request_redraw();
    }

    /// Persist a completed calibration session and close the panel.
    fn save_calibration(&mut self) {
        let Some(Panel::Calibration(session)) = self.ui.panel.as_ref() else {
            return;
        };
        match self.gamepad.save_calibration(session) {
            Ok(()) => {
                self.ui.panel = None;
                self.show_osd("Gamepad calibration saved");
            }
            Err(e) => {
                warn!("gamepad calibration save failed: {e:#}");
                self.show_osd("Calibration save failed (see log)");
            }
        }
    }

    /// Toggle warp speed: emulation runs unpaced (as fast as the host
    /// allows) until switched back, when pacing re-anchors to "now".
    fn toggle_warp(&mut self) {
        let warp = self.emu.paced();
        self.emu.set_paced(!warp);
        if warp {
            let limit = self.warp_speed.label();
            info!("warp speed on (emulation unpaced, limit {limit})");
            self.show_osd(format!("Warp speed on ({limit})"));
        } else {
            info!("warp speed off (real-time pacing)");
            self.show_osd("Warp speed off");
        }
    }

    /// How many emulated frames to retire before presenting the next frame, and
    /// an optional wall-clock budget that bounds that burst. Warp's output frame
    /// skip applies only while warp is engaged and not doing headless capture;
    /// real-time pacing and headless capture both run one frame per presented
    /// frame. The `Max` level returns a budget so the burst presents at vsync
    /// rather than spinning to its frame cap.
    fn warp_burst_plan(&self, headless_capture: bool) -> (usize, Option<std::time::Duration>) {
        if self.emu.paced() || headless_capture {
            return (1, None);
        }
        (
            self.warp_speed.frame_cap(),
            self.warp_speed
                .time_budget_ms()
                .map(std::time::Duration::from_millis),
        )
    }

    /// Cycle the warp/turbo output frame-skip level (2x -> 4x -> 8x -> 16x ->
    /// Max). Takes effect immediately when warp is engaged; otherwise it just
    /// arms the level the next warp toggle will use.
    fn cycle_warp_speed(&mut self) {
        self.warp_speed = self.warp_speed.next();
        let limit = self.warp_speed.label();
        info!("warp limit: {limit}");
        let active = !self.emu.paced();
        if active {
            self.show_osd(format!("Warp limit: {limit}"));
        } else {
            self.show_osd(format!("Warp limit: {limit} (warp off)"));
        }
        self.request_redraw();
    }

    /// Interactive shortcut / menu state save: write the whole
    /// emulated machine to an auto-named file in the working directory and
    /// flash the filename on screen. Runs between frames by construction
    /// (the event loop only dispatches input/menu events outside step_frame).
    fn save_state_interactive(&mut self) {
        self.suspend_live_audio_for_host_io();
        let path = crate::savestate::auto_filename();
        match self.emu.save_state(&path) {
            Ok(()) => {
                info!("save state written: {}", path.display());
                self.show_osd(format!("Saved {}", display_file_name(&path)));
            }
            Err(e) => {
                warn!("save state failed ({}): {e:#}", path.display());
                self.show_osd("State save failed (see log)");
            }
        }
        self.finish_host_io_pause();
    }

    /// Pick a save-state file and restore it (shortcut / menu). On
    /// success the machine continues from the state's timeline: power is
    /// forced on, any CPU halt is cleared, and the display re-renders from
    /// the restored Bus. On failure the running machine is untouched.
    fn load_state_from_dialog(&mut self, event_loop: Option<&ActiveEventLoop>) {
        self.suspend_live_audio_for_host_io();
        let picked = rfd::FileDialog::new()
            .set_title("Load save state")
            .add_filter("Copperline save states", &["clstate"])
            .pick_file();

        // Re-baseline pacing after the modal dialog, as for floppies; a
        // successful load re-anchors again to the restored timeline inside
        // Emulator::load_state.
        if let Some(path) = picked {
            if self.load_state_from_path(&path) {
                if let Some(event_loop) = event_loop {
                    event_loop.set_control_flow(ControlFlow::Poll);
                }
            }
        }
        self.finish_host_io_pause();
    }

    fn load_state_from_path(&mut self, path: &std::path::Path) -> bool {
        match self.emu.load_state(path) {
            Ok(outcome) => {
                info!(
                    "save state loaded: {} ({})",
                    path.display(),
                    outcome.summary
                );
                // The pre-boot configuration screen runs on a placeholder
                // machine with a silent NullSink (see build_placeholder_machine);
                // a state loaded over it would keep that null sink and play no
                // audio. Detect that case before powering on and give the
                // restored machine a live host output below, mirroring the
                // launcher's Run path. A machine that already has a real sink
                // (any normal running session) is left untouched.
                let restoring_over_placeholder = self.restoring_over_placeholder();
                self.powered_on = true;
                self.cpu_halted = false;
                // Force a fresh presentation: the restored frame counter
                // may equal (or precede) the last rendered one.
                self.reset_render_pipeline();
                if matches!(self.ui.panel, Some(Panel::Launcher(_))) {
                    self.ui.panel = None;
                    self.resize_for_active_panel();
                }
                if restoring_over_placeholder {
                    self.install_live_audio_after_placeholder_load();
                }
                if outcome.reconfigured {
                    // The state was built on a different machine; the host
                    // has been reconfigured to match it (see log for the
                    // specifics). The disk-swap playlists are host-side and
                    // describe the previous machine's drives, so drop them
                    // rather than let stale swap affordances show in the
                    // status bar; the restored drives keep whatever disks
                    // the state embedded.
                    self.disk_playlists = std::array::from_fn(|_| Vec::new());
                    self.show_osd(format!(
                        "Loaded {} (reconfigured to {})",
                        display_file_name(path),
                        outcome.summary
                    ));
                } else {
                    self.show_osd(format!("Loaded {}", display_file_name(path)));
                }
                self.request_redraw();
                true
            }
            Err(e) => {
                warn!("save state load failed ({}): {e:#}", path.display());
                self.show_osd("State load failed (see log)");
                false
            }
        }
    }

    /// Pick a Kickstart ROM (and an optional extended ROM) and fit it,
    /// cold-resetting the machine as if the chip had been swapped and the
    /// power cycled (menu "Load Kickstart ROM..."). The main ROM is 512 KiB,
    /// or 256 KiB for a Kickstart 1.x part (mirrored up to the full window);
    /// an extended ROM is 512 KiB ($E00000) or 256 KiB ($F00000).
    /// On any error the running machine keeps its current ROM.
    fn load_rom_from_dialog(&mut self) {
        self.suspend_live_audio_for_host_io();
        let picked = rfd::FileDialog::new()
            .set_title("Load Kickstart ROM (512 or 256 KiB)")
            .add_filter("Amiga ROM images", &["rom", "bin"])
            .pick_file();
        if let Some(main_path) = picked {
            // Offer an optional extended ROM (AROS/CDTV/CD32). Cancelling skips it
            // and removes any extended ROM currently fitted.
            let ext_path = rfd::FileDialog::new()
                .set_title("Load extended ROM (optional; Cancel to skip)")
                .add_filter("Amiga ROM images", &["rom", "bin"])
                .pick_file();

            let result = (|| -> anyhow::Result<()> {
                let rom = std::fs::read(&main_path)
                    .map_err(|e| anyhow::anyhow!("reading ROM {}: {e}", main_path.display()))?;
                let ext = match &ext_path {
                    Some(p) => Some(std::fs::read(p).map_err(|e| {
                        anyhow::anyhow!("reading extended ROM {}: {e}", p.display())
                    })?),
                    None => None,
                };
                self.emu.reload_rom(rom, ext)
            })();

            match result {
                Ok(()) => {
                    info!("boot ROM loaded: {}", main_path.display());
                    self.powered_on = true;
                    self.cpu_halted = false;
                    // The cold reset restarts the frame timeline; force a repaint.
                    self.reset_render_pipeline();
                    self.show_osd(format!("ROM: {}", display_file_name(&main_path)));
                    self.request_redraw();
                }
                Err(e) => {
                    warn!("ROM load failed ({}): {e:#}", main_path.display());
                    self.show_osd("ROM load failed (see log)");
                }
            }
        }
        self.finish_host_io_pause();
    }

    /// Start or stop the video+audio capture (shortcut / menu item).
    fn toggle_recording(&mut self) {
        if self.recorder.is_some() {
            self.stop_recording();
        } else {
            self.start_recording();
        }
    }

    fn start_recording(&mut self) {
        self.start_recording_to(crate::recorder::auto_filename());
    }

    fn start_recording_to(&mut self, path: PathBuf) {
        match crate::recorder::VideoRecorder::create(&path, FB_WIDTH, PRESENT_HEIGHT) {
            Ok(rec) => {
                // The Paula tap collects the mixed stereo output from this
                // point on; capture_recorder_output drains it every frame.
                self.emu.bus_mut().paula.set_audio_capture_enabled(true);
                info!("recording video+audio to {}", path.display());
                self.show_osd(format!("Recording {}", display_file_name(&path)));
                self.recorder = Some(rec);
            }
            Err(e) => {
                warn!("recording start failed: {e:#}");
                self.show_osd("Recording start failed (see log)");
            }
        }
        self.request_redraw();
    }

    fn stop_recording(&mut self) {
        let Some(mut rec) = self.recorder.take() else {
            return;
        };
        let samples = self.emu.bus_mut().paula.take_captured_audio();
        self.emu.bus_mut().paula.set_audio_capture_enabled(false);
        rec.push_audio(&samples);
        let seconds = rec.recorded_seconds();
        let path = rec.path().to_path_buf();
        match rec.finish() {
            Ok(()) => {
                info!(
                    "recording saved: {} ({seconds:.1}s of emulated time)",
                    path.display()
                );
                self.show_osd(format!(
                    "Saved {} ({seconds:.1}s)",
                    display_file_name(&path)
                ));
            }
            Err(e) => {
                warn!("recording save failed ({}): {e:#}", path.display());
                self.show_osd("Recording save failed (see log)");
            }
        }
        self.request_redraw();
    }

    /// Feed the active recording: drain the audio captured during the
    /// quantum just stepped and, when a new emulated frame was rendered,
    /// append it with the presentation-scaled picture.
    fn capture_recorder_output(&mut self, rendered: bool) {
        if self.recorder.is_none() {
            return;
        }
        let samples = self.emu.bus_mut().paula.take_captured_audio();
        let mut failure = None;
        if let Some(rec) = self.recorder.as_mut() {
            rec.push_audio(&samples);
            if rendered {
                screenshot::scale_y_into(
                    &self.present_fb,
                    FB_WIDTH,
                    self.present_rows,
                    PRESENT_HEIGHT,
                    &mut self.record_fb,
                );
                if let Err(e) = rec.push_frame(&self.record_fb) {
                    failure = Some(e);
                }
            }
        }
        if let Some(e) = failure {
            warn!("recording frame write failed, stopping capture: {e:#}");
            self.stop_recording();
        }
    }

    fn debugger_toggle_run(&mut self) {
        self.paused = !self.paused;
        self.last_debug_stop = None;
        // Run/Pause inside the debugger is an explicit choice; closing the
        // window must not revert it.
        self.paused_before_debugger = self.paused;
        self.sync_live_audio_suspension();
    }

    /// Execute a single instruction while paused in the debugger.
    fn debugger_step(&mut self) {
        self.paused = true;
        self.sync_live_audio_suspension();
        self.last_debug_stop = None;
        if let Err(e) = self.emu.debug_step_instructions(1) {
            error!("debugger step halted: {e:?}");
            self.cpu_halted = true;
            self.sync_live_audio_suspension();
        }
        self.surface_debug_stop();
    }

    /// Step over a subroutine call while paused: run a BSR/JSR/TRAP callee to
    /// completion and stop at the following instruction (a plain single step
    /// otherwise). Bounded so a call that never returns cannot wedge the UI.
    fn debugger_step_over(&mut self) {
        const STEP_OVER_BUDGET: usize = 5_000_000;
        self.paused = true;
        self.sync_live_audio_suspension();
        self.last_debug_stop = None;
        if let Err(e) = self.emu.debug_step_over(STEP_OVER_BUDGET) {
            error!("debugger step-over halted: {e:?}");
            self.cpu_halted = true;
            self.sync_live_audio_suspension();
        }
        self.surface_debug_stop();
        self.finish_render_for_current_frame();
    }

    /// Step out of the current subroutine while paused: run until it returns to
    /// its caller. Bounded so a routine that never returns cannot wedge the UI.
    fn debugger_step_out(&mut self) {
        const STEP_OUT_BUDGET: usize = 5_000_000;
        self.paused = true;
        self.sync_live_audio_suspension();
        self.last_debug_stop = None;
        if let Err(e) = self.emu.debug_step_out(STEP_OUT_BUDGET) {
            error!("debugger step-out halted: {e:?}");
            self.cpu_halted = true;
            self.sync_live_audio_suspension();
        }
        self.surface_debug_stop();
        self.finish_render_for_current_frame();
    }

    /// Run one whole video frame while paused, refreshing the display so
    /// mid-frame raster effects can be inspected frame by frame. A
    /// scheduler quantum is shorter than a PAL frame, so step until the
    /// frame counter advances (bounded for safety).
    fn debugger_step_frame(&mut self) {
        self.paused = true;
        self.sync_live_audio_suspension();
        self.last_debug_stop = None;
        let target = self.emu.bus().emulated_frames() + 1;
        for _ in 0..8 {
            if let Err(e) = self.emu.step_frame() {
                error!("debugger frame step halted: {e:?}");
                self.cpu_halted = true;
                self.sync_live_audio_suspension();
                break;
            }
            if self.surface_debug_stop() {
                break;
            }
            if self.emu.bus().emulated_frames() >= target {
                break;
            }
        }
        self.finish_render_for_current_frame();
    }

    /// Run until the PC reaches the address typed in the entry box,
    /// bounded so a never-hit address cannot wedge the UI.
    fn debugger_run_to(&mut self) {
        const RUN_TO_BUDGET: usize = 2_000_000;
        let Some(panel) = self.debugger_panel.as_ref() else {
            return;
        };
        let Some(addr) = panel.entry_addr() else {
            self.show_osd("Run to: type a hex address first");
            return;
        };
        self.paused = true;
        self.sync_live_audio_suspension();
        self.last_debug_stop = None;
        match self.emu.debug_run_to_pc(addr, RUN_TO_BUDGET) {
            Ok(true) => {}
            // A breakpoint/watch hit on the way is reported instead of
            // the budget message.
            Ok(false) => {
                if !self.surface_debug_stop() {
                    self.show_osd(format!("PC ${addr:06X} not reached (budget)"));
                }
            }
            Err(e) => {
                error!("debugger run-to halted: {e:?}");
                self.cpu_halted = true;
                self.sync_live_audio_suspension();
            }
        }
        self.finish_render_for_current_frame();
    }

    /// Step one instruction backward, reconstructed from the snapshot ring.
    fn debugger_reverse_step(&mut self) {
        use crate::timetravel::ReverseOutcome;
        self.paused = true;
        self.sync_live_audio_suspension();
        self.last_debug_stop = None;
        match self.emu.tt_reverse_step(1) {
            Ok(ReverseOutcome::Found(_)) => {}
            Ok(ReverseOutcome::BeyondHistory) => self.show_osd("Reverse: beyond recorded history"),
            Ok(ReverseOutcome::NotFound) => self.show_osd("Reverse: nothing earlier to step to"),
            Err(e) => error!("reverse step halted: {e:?}"),
        }
        self.finish_render_for_current_frame();
    }

    /// Step one emulated video frame backward, reconstructed from the
    /// snapshot ring.
    fn debugger_reverse_frame(&mut self) {
        use crate::timetravel::ReverseOutcome;
        self.paused = true;
        self.sync_live_audio_suspension();
        self.last_debug_stop = None;
        match self.emu.tt_reverse_frame() {
            Ok(ReverseOutcome::Found(_)) => {}
            Ok(ReverseOutcome::BeyondHistory) => {
                self.show_osd("Reverse frame: beyond recorded history")
            }
            Ok(ReverseOutcome::NotFound) => {
                self.show_osd("Reverse frame: no earlier frame to step to")
            }
            Err(e) => error!("reverse frame step halted: {e:?}"),
        }
        self.finish_render_for_current_frame();
    }

    /// Run backward to the previous breakpoint hit (reconstructed from the
    /// snapshot ring).
    fn debugger_reverse_continue(&mut self) {
        use crate::timetravel::ReverseOutcome;
        self.paused = true;
        self.sync_live_audio_suspension();
        self.last_debug_stop = None;
        match self.emu.tt_reverse_continue() {
            Ok(ReverseOutcome::Found(_)) => {
                self.surface_debug_stop();
            }
            Ok(ReverseOutcome::NotFound) => self.show_osd("Reverse run: no earlier breakpoint hit"),
            Ok(ReverseOutcome::BeyondHistory) => {
                self.show_osd("Reverse run: beyond recorded history")
            }
            Err(e) => error!("reverse continue halted: {e:?}"),
        }
        self.finish_render_for_current_frame();
    }

    /// Surface a pending breakpoint/watchpoint hit: pause the machine,
    /// bring up the debugger window, and report the reason. Returns true
    /// when a stop was pending.
    fn surface_debug_stop(&mut self) -> bool {
        let Some(stop) = self.emu.machine.take_ui_debug_stop() else {
            return false;
        };
        let message = stop.describe();
        info!("debugger stop: {message}");
        self.paused = true;
        self.paused_before_debugger = true;
        self.sync_live_audio_suspension();
        self.open_debugger();
        self.last_debug_stop = Some(message.clone());
        self.show_osd(message);
        self.request_redraw();
        true
    }

    /// Toggle a PC breakpoint from the entry box. The entry may carry an
    /// optional condition and ignore count: "ADDR [LHS OP RHS] [IGN N]".
    fn debugger_toggle_breakpoint(&mut self) {
        let spec = self
            .debugger_panel
            .as_ref()
            .and_then(|panel| ui::parse_break_spec(&panel.entry));
        let Some((addr, cond, ignore)) = spec else {
            self.show_osd("Break: ADDR [LHS OP RHS] [IGN N] e.g. C033C2 D0 EQ 5");
            return;
        };
        let set = self.emu.machine.ui_set_breakpoint(addr, cond, ignore);
        let mut msg = format!(
            "Breakpoint ${:06X} {}",
            addr & 0x00FF_FFFF,
            if set { "set" } else { "removed" }
        );
        if set {
            if let Some(cond) = &cond {
                msg.push_str(&format!(" when {}", cond.describe()));
            }
            if ignore > 0 {
                msg.push_str(&format!(" ign {ignore}"));
            }
        }
        self.show_osd(msg);
    }

    /// Toggle a memory word watchpoint at the entry-box address.
    fn debugger_toggle_watchpoint(&mut self) {
        let Some(addr) = self.debugger_entry_addr("Watch") else {
            return;
        };
        let addr = addr & 0x00FF_FFFE;
        let set = self.emu.machine.ui_toggle_watch(addr);
        self.show_osd(format!(
            "Watchpoint ${addr:06X} {}",
            if set { "set" } else { "removed" }
        ));
    }

    /// Toggle a chipset-register write watch. The entry accepts either a
    /// bare register offset (96) or a full address (DFF096).
    fn debugger_toggle_reg_watch(&mut self) {
        let Some(addr) = self.debugger_entry_addr("Reg") else {
            return;
        };
        let off = (addr & 0x1FE) as u16;
        let set = self.emu.machine.ui_toggle_reg_watch(off);
        self.show_osd(format!(
            "{} (${off:03X}) write watch {}",
            crate::debugger::custom_reg_name(off),
            if set { "set" } else { "removed" }
        ));
    }

    /// The debugger entry-box address, or an OSD prompt when empty.
    fn debugger_entry_addr(&mut self, what: &str) -> Option<u32> {
        let panel = self.debugger_panel.as_ref()?;
        let addr = panel.entry_addr();
        if addr.is_none() {
            self.show_osd(format!("{what}: type a hex address first"));
        }
        addr
    }

    /// Page the Memory tab's hex dump up or down.
    fn debugger_mem_page(&mut self, direction: i32) {
        if let Some(panel) = self.debugger_panel.as_mut() {
            if panel.tab == ui::DebugTab::Memory {
                let delta = ui::MEM_PAGE_BYTES;
                panel.mem_addr = if direction < 0 {
                    panel.mem_addr.wrapping_sub(delta)
                } else {
                    panel.mem_addr.wrapping_add(delta)
                } & 0x00FF_FFF0;
            }
        }
    }

    /// Write the entry box's value live while paused: a memory word from
    /// "ADDR VALUE" on the Memory tab, or a register from "REG VALUE" on the
    /// CPU tab. The panel borrow is resolved into a plain action first so the
    /// emulator can then be borrowed mutably to perform the write.
    fn debugger_poke(&mut self) {
        enum Poke {
            Mem(u32, u16),
            Reg(usize, u32),
            MemHelp,
            RegHelp,
            None,
        }
        let action = match self.debugger_panel.as_ref() {
            Some(panel) => match panel.tab {
                ui::DebugTab::Memory => match panel.poke_target() {
                    Some((addr, value)) => Poke::Mem(addr, value),
                    None => Poke::MemHelp,
                },
                ui::DebugTab::Cpu => match panel.reg_poke() {
                    Some((reg, value)) => Poke::Reg(reg, value),
                    None => Poke::RegHelp,
                },
                _ => Poke::None,
            },
            None => Poke::None,
        };
        match action {
            Poke::Mem(addr, value) => {
                let written = self
                    .emu
                    .machine
                    .debug_write_memory(addr, &value.to_be_bytes());
                if written == 2 {
                    self.show_osd(format!("Poked ${value:04X} -> ${addr:06X}"));
                } else {
                    self.show_osd(format!("${addr:06X} is not writable RAM"));
                }
            }
            Poke::Reg(reg, value) => {
                self.emu.machine.debug_set_register(reg, value);
                self.show_osd(format!("{} <- ${value:X}", gdb_reg_label(reg)));
            }
            Poke::MemHelp => self.show_osd("Poke: type \"ADDR VALUE\" (hex) first"),
            Poke::RegHelp => self.show_osd("Set Reg: type \"REG VALUE\" e.g. D0 1234"),
            Poke::None => {}
        }
    }

    /// Build the per-redraw view data for the open panel, if any.
    fn build_panel_view_data(&self) -> Option<ui::PanelViewData> {
        match self.ui.panel.as_ref()? {
            Panel::About => Some(ui::PanelViewData::About(ui::AboutView {
                machine_lines: self.about_machine_lines.clone(),
            })),
            Panel::Shortcuts => Some(ui::PanelViewData::Shortcuts),
            Panel::Calibration(session) => Some(ui::PanelViewData::Calibration(
                build_calibration_view(session),
            )),
            Panel::Debugger(panel) => {
                Some(ui::PanelViewData::Debugger(self.build_debugger_view(panel)))
            }
            Panel::FrameAnalyzer(panel) => Some(ui::PanelViewData::FrameAnalyzer(Box::new(
                self.build_frame_analyzer_view(panel),
            ))),
            // The configuration panel renders straight from its own state.
            Panel::Launcher(_) => None,
        }
    }

    fn build_tool_panel_view_data(&self, kind: ToolPanelKind) -> Option<ui::PanelViewData> {
        match kind {
            ToolPanelKind::Debugger => self
                .debugger_panel
                .as_ref()
                .map(|panel| ui::PanelViewData::Debugger(self.build_debugger_view(panel))),
            ToolPanelKind::FrameAnalyzer => self.frame_analyzer_panel.as_ref().map(|panel| {
                ui::PanelViewData::FrameAnalyzer(Box::new(self.build_frame_analyzer_view(panel)))
            }),
        }
    }

    fn build_frame_analyzer_view(&self, panel: &ui::FrameAnalyzerPanel) -> ui::FrameAnalyzerView {
        let bus = self.emu.bus();
        let status = format!(
            "{} frame {} {:.2}s",
            if self.paused { "paused" } else { "running" },
            bus.emulated_frames(),
            bus.emulated_seconds()
        );
        let Some(trace) = bus.frame_bus_trace() else {
            return ui::FrameAnalyzerView {
                running: !self.paused,
                status,
                trace: None,
            };
        };
        let selected_vpos = usize::from(panel.selected_vpos).min(trace.rows.saturating_sub(1));
        let selected_hpos = usize::from(panel.selected_hpos).min(trace.cols.saturating_sub(1));
        let selected_owner_code = trace.owner_code_at(selected_vpos, selected_hpos);
        let selected_owner = owner_name_from_code(selected_owner_code);
        let mut owners = Vec::with_capacity(trace.rows * trace.cols);
        for vpos in 0..trace.rows {
            if let Some(row) = trace.owner_row(vpos) {
                owners.extend_from_slice(row);
            }
        }
        let markers = bus
            .frame_render_events()
            .iter()
            .take(240)
            .map(|event| {
                let off = event.offset & 0x01FE;
                ui::AnalyzerMarker {
                    vpos: event.vpos.min(u32::from(u16::MAX)) as u16,
                    hpos: event.hpos.min(u32::from(u16::MAX)) as u16,
                    label: format!(
                        "{} v={:03} h={:03} ${off:03X}={:04X}",
                        match event.source {
                            BeamWriteSource::Cpu => "cpu",
                            BeamWriteSource::CpuCopperIrq => "irq",
                            BeamWriteSource::Copper => "copper",
                        },
                        event.vpos,
                        event.hpos,
                        event.value,
                    ),
                }
            })
            .collect();
        ui::FrameAnalyzerView {
            running: !self.paused,
            status,
            trace: Some(ui::AnalyzerTraceView {
                frame: trace.frame,
                seconds: trace.seconds,
                rows: trace.rows,
                cols: trace.cols,
                line_cck: trace.line_cck,
                visible_start_vpos: trace.visible_start_vpos,
                visible_lines: trace.visible_lines,
                display_hpos_start: trace.display_hpos_start,
                display_hpos_end: trace.display_hpos_end,
                owner_cck: trace.owner_cck,
                blitter_busy_cck: trace.blitter_busy_cck,
                blitter_starve_cck: trace.blitter_starve_cck,
                partial: trace.partial,
                selected_vpos,
                selected_hpos,
                selected_owner,
                selected_owner_code,
                owners,
                markers,
            }),
        }
    }

    /// Snapshot the machine into the debugger panel's formatted lines.
    /// Everything reads through side-effect-free peeks, so inspecting
    /// state never perturbs the emulation.
    fn build_debugger_view(&self, panel: &ui::DebuggerPanel) -> ui::DebuggerView {
        let machine = &self.emu.machine;
        let bus = self.emu.bus();
        let mut status = format!(
            "{} frame {} {:.2}s",
            if self.paused { "paused" } else { "running" },
            bus.emulated_frames(),
            bus.emulated_seconds()
        );
        // Reverse-debug position and history depth, when the ring is armed.
        if let Some(ring) = self.emu.time_travel_ring() {
            if !ring.is_empty() {
                status.push_str(&format!(
                    "  | pos {} rev {} snaps, {} MB",
                    self.emu.retired_instructions(),
                    ring.len(),
                    ring.used_bytes() / (1024 * 1024),
                ));
            }
        }
        let read = |addr: u32| bus.peek_word_any(addr);
        let mut lines: Vec<ui::DbgLine> = Vec::new();
        match panel.tab {
            ui::DebugTab::Cpu => {
                let pc = machine.pc();
                let sr = machine.sr();
                lines.push(ui::DbgLine::plain(format!(
                    "PC {pc:08X}   SR {sr:04X} [{}]{}",
                    ui::sr_flags(sr),
                    if machine.stopped() { "   STOPPED" } else { "" }
                )));
                lines.push(ui::DbgLine::plain(""));
                for (name, regs) in [("D", 0usize), ("A", 1)] {
                    for half in 0..2 {
                        let row: Vec<String> = (0..4)
                            .map(|i| {
                                let reg = half * 4 + i;
                                let value = if regs == 0 {
                                    machine.d(reg)
                                } else {
                                    machine.a(reg)
                                };
                                format!("{name}{reg} {value:08X}")
                            })
                            .collect();
                        lines.push(ui::DbgLine::plain(row.join("   ")));
                    }
                }
                lines.push(ui::DbgLine::plain(""));
                if let Some(origin) = panel.disasm_addr {
                    lines.push(ui::DbgLine::plain(format!(
                        "Disassembly pinned at ${origin:06X} (empty box + Enter follows PC)"
                    )));
                }
                let breaks = machine.ui_breaks();
                let mut addr = panel.disasm_addr.unwrap_or(pc) & !1;
                for _ in 0..24 {
                    let (text, len) = crate::disasm::disassemble(read, addr, machine.cpu_type());
                    // A leading bullet marks a line that carries a breakpoint.
                    let marker = if breaks.is_breakpoint(addr) { "*" } else { " " };
                    let line = format!("{marker}{addr:08X}  {text}");
                    lines.push(if addr == pc {
                        ui::DbgLine::hilit(line)
                    } else {
                        ui::DbgLine::plain(line)
                    });
                    addr = addr.wrapping_add(len);
                }
            }
            ui::DebugTab::Chipset => {
                let agnus = &bus.agnus;
                let base = bus.current_render_base();
                let intreq = bus.cpu_visible_intreq();
                let intena = bus.paula.intena;
                lines.push(ui::DbgLine::hilit(format!(
                    "Beam vpos {:>3} hpos {:>3}   frame {}",
                    agnus.vpos,
                    agnus.hpos,
                    bus.emulated_frames()
                )));
                lines.push(ui::DbgLine::plain(""));
                lines.push(ui::DbgLine::plain(format!(
                    "DMACON {:04X}  {}",
                    agnus.dmacon,
                    ui::dmacon_flags(agnus.dmacon)
                )));
                lines.push(ui::DbgLine::plain(format!(
                    "INTENA {:04X}  {}",
                    intena,
                    ui::int_flags(intena)
                )));
                lines.push(ui::DbgLine::plain(format!(
                    "INTREQ {:04X}  {}",
                    intreq,
                    ui::int_flags(intreq)
                )));
                lines.push(ui::DbgLine::plain(""));
                lines.push(ui::DbgLine::plain(format!(
                    "COP1LC {:06X}   COP2LC {:06X}   COPPC {:06X} ({})",
                    agnus.cop1lc,
                    agnus.cop2lc,
                    bus.copper.pc(),
                    if bus.copper.is_running() {
                        "running"
                    } else {
                        "stopped"
                    }
                )));
                lines.push(ui::DbgLine::plain(""));
                lines.push(ui::DbgLine::plain(format!(
                    "BPLCON0 {:04X}  BPLCON1 {:04X}  BPLCON2 {:04X}  FMODE {:04X}",
                    base.bplcon0, base.bplcon1, base.bplcon2, base.fmode
                )));
                lines.push(ui::DbgLine::plain(format!(
                    "DIWSTRT {:04X}  DIWSTOP {:04X}  DDFSTRT {:04X}  DDFSTOP {:04X}",
                    base.diwstrt, base.diwstop, base.ddfstrt, base.ddfstop
                )));
                lines.push(ui::DbgLine::plain(format!(
                    "BPL1MOD {}  BPL2MOD {}",
                    base.bpl1mod, base.bpl2mod
                )));
                lines.push(ui::DbgLine::plain(""));
                for (label, ptrs) in [("BPLPT", &base.bplpt), ("SPRPT", &base.sprpt)] {
                    let row: Vec<String> = ptrs.iter().map(|p| format!("{p:06X}")).collect();
                    lines.push(ui::DbgLine::plain(format!("{label} {}", row.join(" "))));
                }
                lines.push(ui::DbgLine::plain(""));
                let colors = base.palette.hi_words();
                for half in 0..2 {
                    let row: Vec<String> = (0..16)
                        .map(|i| format!("{:03X}", colors[half * 16 + i] & 0x0FFF))
                        .collect();
                    lines.push(ui::DbgLine::plain(format!(
                        "COLOR{:02} {}",
                        half * 16,
                        row.join(" ")
                    )));
                }
            }
            ui::DebugTab::Copper => {
                let agnus = &bus.agnus;
                let copper_pc = bus.copper.pc();
                lines.push(ui::DbgLine::plain(format!(
                    "COP1LC {:06X}   COP2LC {:06X}   COPPC {:06X} ({})",
                    agnus.cop1lc,
                    agnus.cop2lc,
                    copper_pc,
                    if bus.copper.is_running() {
                        "running"
                    } else {
                        "stopped"
                    }
                )));
                lines.push(ui::DbgLine::plain(""));
                for (addr, text) in crate::disasm::dump_copper_list(read, agnus.cop1lc, 36) {
                    let line = format!("{addr:06X}  {text}");
                    lines.push(if addr == copper_pc {
                        ui::DbgLine::hilit(line)
                    } else {
                        ui::DbgLine::plain(line)
                    });
                }
            }
            ui::DebugTab::Memory => {
                lines.push(ui::DbgLine::plain(
                    "Click the $ box, type a hex address, Enter to jump; </> to page",
                ));
                lines.push(ui::DbgLine::plain(""));
                let base = panel.mem_addr & 0x00FF_FFF0;
                for row in 0..16u32 {
                    let addr = base.wrapping_add(row * 16) & 0x00FF_FFFF;
                    let mut bytes = [0u8; 16];
                    for word in 0..8u32 {
                        let value = bus.peek_word_any(addr.wrapping_add(word * 2));
                        bytes[word as usize * 2] = (value >> 8) as u8;
                        bytes[word as usize * 2 + 1] = value as u8;
                    }
                    lines.push(ui::DbgLine::plain(ui::hex_dump_row(addr, &bytes)));
                }
            }
            ui::DebugTab::Break => {
                // Leave room for the toggle buttons drawn at the top of
                // the content area.
                for _ in 0..ui::BREAK_TAB_HEADER_LINES {
                    lines.push(ui::DbgLine::plain(""));
                }
                if let Some(stop) = &self.last_debug_stop {
                    lines.push(ui::DbgLine::hilit(format!("Stopped: {stop}")));
                    lines.push(ui::DbgLine::plain(""));
                }
                lines.push(ui::DbgLine::plain(
                    "Type a hex address in the $ box, then a toggle button.",
                ));
                lines.push(ui::DbgLine::plain(
                    "Reg takes a custom-register offset (96) or address (DFF096).",
                ));
                lines.push(ui::DbgLine::plain(
                    "Break cond: ADDR [LHS OP RHS] [IGN N]  e.g. C033C2 D0 EQ 5",
                ));
                lines.push(ui::DbgLine::plain(
                    "  ops EQ NE LT GT LE GE AND; operand Dn An PC SR Mhex hex",
                ));
                lines.push(ui::DbgLine::plain(""));
                let breaks = self.emu.machine.ui_breaks();
                lines.push(ui::DbgLine::plain("Breakpoints:"));
                if breaks.breakpoints.is_empty() {
                    lines.push(ui::DbgLine::plain("  (none)"));
                }
                for bp in &breaks.breakpoints {
                    let mut text = format!("  ${:06X}", bp.addr);
                    if let Some(cond) = &bp.cond {
                        text.push_str(&format!("  {}", cond.describe()));
                    }
                    if bp.ignore > 0 {
                        text.push_str(&format!("  ign {}/{}", bp.hits, bp.ignore));
                    }
                    lines.push(ui::DbgLine::plain(text));
                }
                lines.push(ui::DbgLine::plain(""));
                lines.push(ui::DbgLine::plain("Watchpoints (word, stop on change):"));
                if breaks.watches.is_empty() {
                    lines.push(ui::DbgLine::plain("  (none)"));
                }
                for watch in &breaks.watches {
                    lines.push(ui::DbgLine::plain(format!(
                        "  ${:06X}  now {:04X}",
                        watch.addr,
                        bus.peek_word_any(watch.addr)
                    )));
                }
                lines.push(ui::DbgLine::plain(""));
                lines.push(ui::DbgLine::plain("Register watches (stop on write):"));
                if breaks.reg_watches.is_empty() {
                    lines.push(ui::DbgLine::plain("  (none)"));
                }
                for off in &breaks.reg_watches {
                    lines.push(ui::DbgLine::plain(format!(
                        "  {} (${off:03X})",
                        crate::debugger::custom_reg_name(*off)
                    )));
                }
            }
        }
        // Keep lines inside the panel; the blitter clips at the texture
        // edge, not the panel edge.
        for line in &mut lines {
            if line.text.len() > 82 {
                line.text.truncate(82);
            }
        }
        ui::DebuggerView {
            running: !self.paused,
            reverse_available: self.emu.time_travel_enabled(),
            status,
            lines,
        }
    }

    /// Pick one or more disk images for a drive. The selection replaces
    /// the drive's swap playlist; the first image is inserted right away
    /// and the rest are queued for the swap button / shortcut.
    fn load_drive_disks_from_dialog(&mut self, drive_idx: usize) {
        self.suspend_live_audio_for_host_io();
        let picked = rfd::FileDialog::new()
            .set_title(format!("Load DF{drive_idx} disk image(s)"))
            .add_filter("Amiga disk images", &["adf", "adz", "dms", "scp", "gz"])
            .pick_files();

        // The modal file dialog blocks this (the main/emulation) thread, so
        // wall-clock time advanced while emulated time stood still. Re-baseline
        // the pacing anchor whether or not a file was chosen, otherwise the
        // pacer would fast-forward to catch up and corrupt pacing for the
        // freshly inserted disk. insert_disk_image -> bus floppy
        // insert_disk_image already asserts the disk-change/eject signal.
        if let Some(paths) = picked.filter(|paths| !paths.is_empty()) {
            let count = paths.len();
            let path = paths[0].clone();
            self.disk_playlists[drive_idx] = paths;
            self.disk_playlist_index[drive_idx] = 0;
            let name = display_file_name(&path);
            if self.insert_disk_image(drive_idx, path, self.disk_write_protected[drive_idx]) {
                if count > 1 {
                    self.show_osd(format!("DF{drive_idx}: {name} (1/{count})"));
                } else {
                    self.show_osd(format!("DF{drive_idx}: {name}"));
                }
            } else {
                self.show_osd(format!("DF{drive_idx}: load failed (see log)"));
            }
        }
        self.finish_host_io_pause();
    }

    /// Advance the disk-swap playlist of the first drive that has more
    /// than one image queued (the disk-swap shortcut). With no multi-disk
    /// drive, just shows a notice.
    fn cycle_disk(&mut self) {
        let Some(drive) =
            (0..self.disk_playlists.len()).find(|&idx| self.disk_playlists[idx].len() > 1)
        else {
            self.show_osd("No alternate disk configured");
            return;
        };
        self.swap_drive_disk(drive);
    }

    /// Insert the next disk in a drive's swap playlist, wrapping around,
    /// and flash the new filename on screen.
    fn swap_drive_disk(&mut self, drive_idx: usize) {
        let count = self.disk_playlists[drive_idx].len();
        if count < 2 {
            self.show_osd(format!("DF{drive_idx}: no other disk queued"));
            return;
        }
        let next = (self.disk_playlist_index[drive_idx] + 1) % count;
        let path = self.disk_playlists[drive_idx][next].clone();
        let write_protected = self.disk_write_protected[drive_idx];
        self.disk_playlist_index[drive_idx] = next;
        let name = display_file_name(&path);
        if self.insert_disk_image(drive_idx, path, write_protected) {
            self.show_osd(format!("DF{drive_idx}: {name} ({}/{count})", next + 1));
        } else {
            self.show_osd(format!("DF{drive_idx}: swap failed (see log)"));
        }
    }

    fn eject_drive_disk(&mut self, drive_idx: usize) {
        if !self.emu.bus().floppy.disk_inserted(drive_idx) {
            self.show_osd(format!("DF{drive_idx}: no disk"));
            return;
        }
        match self.emu.bus_mut().floppy.eject_disk_image(drive_idx) {
            Ok(()) => {
                info!("floppy.df{drive_idx} ejected");
                self.show_osd(format!("DF{drive_idx}: ejected"));
                self.request_redraw();
            }
            Err(e) => warn!("floppy.df{drive_idx} eject failed: {e:#}"),
        }
    }

    /// Pick a CD image and mount it with the media-change notification,
    /// ejecting any current disc first.
    fn load_cd_from_dialog(&mut self) {
        self.suspend_live_audio_for_host_io();
        let picked = rfd::FileDialog::new()
            .set_title("Load CD image (cue sheet)")
            .add_filter("CD cue sheets", &["cue"])
            .pick_file();

        // Re-baseline pacing after the modal dialog, as for floppies.
        if let Some(path) = picked {
            match crate::cdrom::CdImage::load(&path) {
                Ok(image) => {
                    info!("cd image: {} ({})", path.display(), image.describe());
                    self.emu.bus_mut().cd_insert_disc(image);
                    self.show_osd(format!("CD: {}", display_file_name(&path)));
                    self.request_redraw();
                }
                Err(e) => {
                    warn!("cd image load failed ({}): {e:#}", path.display());
                    self.show_osd("CD: load failed (see log)");
                }
            }
        }
        self.finish_host_io_pause();
    }

    fn eject_cd(&mut self) {
        if !self.emu.bus().cd_disc_inserted() {
            self.show_osd("CD: no disc");
            return;
        }
        self.emu.bus_mut().cd_eject_disc();
        self.show_osd("CD: ejected");
        self.request_redraw();
    }

    fn insert_disk_image(
        &mut self,
        drive_idx: usize,
        path: PathBuf,
        write_protected: bool,
    ) -> bool {
        self.suspend_live_audio_for_host_io();
        let result = match self.emu.bus_mut().floppy.insert_disk_image(
            drive_idx,
            path.clone(),
            write_protected,
        ) {
            Ok(()) => {
                self.last_fdd_track = None;
                info!("floppy.df{} inserted {}", drive_idx, path.display());
                if let Some(rec) = self.input_recorder.as_mut() {
                    rec.record_disk_insert(drive_idx, &path, self.emu.bus().emulated_seconds());
                }
                // Reverse-debug: mark the media change so replay across it warns
                // (the inserted image is host-file state, not in the log).
                self.emu
                    .tt_note_input(crate::inputsched::ReplayAction::DiskChange);
                self.request_redraw();
                true
            }
            Err(e) => {
                warn!(
                    "floppy.df{} insert failed ({}): {e:#}",
                    drive_idx,
                    path.display()
                );
                false
            }
        };
        self.finish_host_io_pause();
        result
    }

    fn suspend_live_audio_for_host_io(&mut self) {
        self.emu.set_live_audio_suspended(true);
    }

    /// Whether a state is being loaded over the pre-boot placeholder machine
    /// that hosts the configuration screen: powered off, the launcher panel
    /// open, and the silent NullSink still installed. Only then does a load need
    /// to install a real audio output; every normal running session already has
    /// one. Evaluate this before powering on / dismissing the launcher.
    fn restoring_over_placeholder(&self) -> bool {
        !self.powered_on
            && matches!(self.ui.panel, Some(Panel::Launcher(_)))
            && self.emu.bus().paula.audio.is_null_sink()
    }

    /// Replace the placeholder machine's silent NullSink with a live host audio
    /// output after a save state is loaded over the configuration screen. This
    /// mirrors the launcher Run path (`launcher_run`): the configuration screen
    /// itself stays silent, but a machine started from it -- by Run or by a state
    /// load -- gets real sound. On audio-init failure the state stays loaded and
    /// the machine simply runs without sound, exactly as a failed Run does.
    fn install_live_audio_after_placeholder_load(&mut self) {
        match CpalSink::new(crate::priority::requested(false)) {
            Ok(sink) => {
                self.emu.bus_mut().paula.audio = Box::new(sink);
                // Apply the current suspension state to the freshly installed
                // stream (it should be live now: powered on and not paused).
                self.sync_live_audio_suspension();
            }
            Err(e) => {
                warn!("audio init after state load failed; continuing without sound: {e:#}");
            }
        }
    }

    fn finish_host_io_pause(&mut self) {
        self.emu.reanchor_realtime_clock();
        self.sync_live_audio_suspension();
    }

    fn sync_live_audio_suspension(&mut self) {
        let suspended = !self.powered_on || self.cpu_halted || self.paused;
        self.emu.set_live_audio_suspended(suspended);
    }

    fn resize_for_active_panel(&self) {
        let Some(window) = self.render.as_ref().map(|r| r.window.clone()) else {
            return;
        };
        let size = LogicalSize::new(FB_WIDTH as f64, WINDOW_PRESENT_HEIGHT as f64);
        let _ = window.request_inner_size(size);
    }

    fn request_redraw(&self) {
        if let Some(render) = self.render.as_ref() {
            render.window.request_redraw();
        }
        if let Some(tool) = self.debugger_tool_window.as_ref() {
            tool.window.request_redraw();
        }
        if let Some(tool) = self.frame_analyzer_tool_window.as_ref() {
            tool.window.request_redraw();
        }
    }

    fn handle_amiga_key_event(&mut self, rawkey: u8, pressed: bool) {
        let idx = (rawkey & 0x7F) as usize;
        self.held_rawkeys[idx] = pressed;

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
    fn toggle_input_recording(&mut self) {
        match self.input_recorder.take() {
            Some(rec) => {
                let events = rec.events_recorded();
                let script = rec.finish();
                let path = crate::inputrec::auto_filename();
                match std::fs::write(&path, script) {
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

    fn save_screenshot(&self, path: &std::path::Path) {
        // COPPERLINE_SHOT_RAW saves the raw woven framebuffer (716x570
        // for standard fields, the native scan height for programmable
        // modes): the presentation resampler blends adjacent lines, so
        // per-scanline forensics need the unscaled field.
        let src_rows = self.present_rows;
        let result = if crate::envcfg::flag("COPPERLINE_SHOT_RAW") {
            screenshot::save(
                path,
                &self.present_fb[..src_rows * FB_WIDTH],
                FB_WIDTH as u32,
                src_rows as u32,
            )
        } else {
            screenshot::save_scaled_y(
                path,
                &self.present_fb,
                FB_WIDTH as u32,
                src_rows as u32,
                PRESENT_HEIGHT as u32,
            )
        };
        match result {
            Ok(()) => info!("screenshot saved: {}", path.display()),
            Err(e) => warn!("screenshot save failed ({}): {e:#}", path.display()),
        }
    }

    /// Interactive screenshot grab: save to an auto-named PNG and
    /// flash the filename on screen. The overlay is painted into the
    /// presentation texture after the frame is captured, so it never
    /// appears in the saved image.
    fn take_screenshot(&mut self) {
        self.finish_render_for_current_frame();
        let path = screenshot::auto_filename();
        self.save_screenshot(&path);
        self.show_osd(format!("Saved {}", display_file_name(&path)));
    }

    /// Show a transient overlay message over the display for
    /// [`OSD_DURATION`]. The message is cleared automatically; while it is
    /// visible the event loop keeps redrawing even when paused/idle so it
    /// fades on time.
    fn show_osd(&mut self, text: impl Into<String>) {
        self.osd = Some(Osd {
            text: text.into(),
            expires_at: Instant::now() + OSD_DURATION,
        });
        self.request_redraw();
    }

    /// The overlay text to draw this frame, or None when nothing is
    /// active. Expired overlays are dropped as a side effect.
    fn active_osd_text(&mut self) -> Option<String> {
        match &self.osd {
            Some(osd) if Instant::now() < osd.expires_at => Some(osd.text.clone()),
            Some(_) => {
                self.osd = None;
                None
            }
            None => None,
        }
    }

    fn dump_frame_if_due(&mut self, _now: Instant, event_loop: &ActiveEventLoop) -> bool {
        let Some(state) = self.frame_dump.as_ref() else {
            return false;
        };
        if self.emu.bus().emulated_seconds() < state.start_secs as f64 {
            return false;
        }
        let emulated_frame = self.emu.bus().emulated_frames();
        if state.last_saved_emulated_frame == Some(emulated_frame) {
            return false;
        }
        self.finish_render_for_current_frame();
        if self.last_rendered_emulated_frame != Some(emulated_frame) {
            return false;
        }

        let Some(state) = self.frame_dump.as_mut() else {
            return false;
        };
        let path = state.dir.join(format!("frame-{:06}.png", state.dumped));
        if crate::envcfg::flag("COPPERLINE_DUMP_RENDER_META") {
            log_frame_dump_metadata(state.dumped, &self.emu);
        }
        match screenshot::save_scaled_y(
            &path,
            &self.present_fb,
            FB_WIDTH as u32,
            self.present_rows as u32,
            PRESENT_HEIGHT as u32,
        ) {
            Ok(()) => {
                state.last_saved_emulated_frame = Some(emulated_frame);
                state.dumped += 1;
                if state.dumped == 1 || state.dumped == state.count || state.dumped % 25 == 0 {
                    info!(
                        "frame dump: saved {}/{} ({})",
                        state.dumped,
                        state.count,
                        path.display()
                    );
                }
            }
            Err(e) => {
                warn!("frame dump failed ({}): {e:#}", path.display());
                self.frame_dump = None;
                event_loop.exit();
                return true;
            }
        }

        if state.dumped >= state.count {
            info!(
                "frame dump complete: saved {} frames to {}",
                state.count,
                state.dir.display()
            );
            self.emu.report_stats();
            self.emu.bus().poll_stats.dump_top("at frame dump");
            self.frame_dump = None;
            event_loop.exit();
            true
        } else {
            false
        }
    }

    /// Toggle host power. Powering off cold-resets the machine (clearing
    /// RAM) and parks a test screen on the display; powering on boots the
    /// freshly cold machine. The redraw keeps the status-bar button and
    /// display current.
    fn toggle_power(&mut self) {
        if self.powered_on {
            self.power_off();
        } else {
            self.powered_on = true;
            self.sync_live_audio_suspension();
            info!("power button: machine powered on (cold boot)");
        }
        self.request_redraw();
    }

    /// Toggle host-level pause. Pausing freezes the emulator in place
    /// (it stops stepping but stays powered on), so the current frame is
    /// held and emulation resumes from the same point when unpaused.
    fn toggle_pause(&mut self) {
        self.paused = !self.paused;
        self.sync_live_audio_suspension();
        if self.paused {
            info!("pause button: emulation paused");
        } else {
            info!("pause button: emulation resumed");
        }
        self.request_redraw();
    }

    /// Power off: drop into a cold-boot state (RAM cleared) and park the
    /// test screen, so a later power-on comes up as a clean power cycle.
    fn power_off(&mut self) {
        self.powered_on = false;
        self.paused = false;
        self.sync_live_audio_suspension();
        info!("power button: machine powered off (cold boot state)");
        if let Err(e) = self.emu.power_on_reset() {
            error!("cold power-on reset failed: {e:#}");
            self.cpu_halted = true;
            self.sync_live_audio_suspension();
        } else {
            self.cpu_halted = false;
            self.sync_live_audio_suspension();
        }
        self.held_rawkeys = [false; 128];
        self.reset_render_pipeline();
        self.last_fdd_track = None;
        paint_test_screen(&mut self.fb);
        self.deinterlacer
            .push_field(&self.fb, FB_HEIGHT, false, true, true);
        self.refresh_present_from_deinterlacer();
    }

    fn reset_emulator(&mut self, clear_host_keys: bool) {
        if let Err(e) = self.emu.keyboard_reset() {
            error!("keyboard reset failed: {e:#}");
            self.cpu_halted = true;
            self.sync_live_audio_suspension();
        } else {
            self.cpu_halted = false;
            self.sync_live_audio_suspension();
            self.reset_render_pipeline();
            self.last_fdd_track = None;
            if clear_host_keys {
                self.held_rawkeys = [false; 128];
            }
        }
    }

    fn refresh_present_from_deinterlacer(&mut self) {
        let rows = self.deinterlacer.output_rows();
        let active = rows * FB_WIDTH;
        self.present_fb.resize(active, 0);
        self.present_fb
            .copy_from_slice(&self.deinterlacer.output()[..active]);
        self.present_rows = rows;
    }

    fn reset_render_pipeline(&mut self) {
        self.render_generation = self.render_generation.wrapping_add(1);
        self.last_rendered_emulated_frame = None;
        self.last_submitted_render_frame = None;
        let _ = self.collect_threaded_render_results(false);
    }

    fn apply_threaded_render_result(&mut self, result: RenderWorkerResult) -> bool {
        if result.generation != self.render_generation {
            if self.render_recycle_fb.is_empty() {
                self.render_recycle_fb = result.presentation_fb;
            }
            return false;
        }

        self.emu.bus_mut().record_video_render_frame(result.timing);
        let old = std::mem::replace(&mut self.present_fb, result.presentation_fb);
        self.render_recycle_fb = old;
        self.present_rows = result.present_rows;
        self.last_rendered_emulated_frame = Some(result.emulated_frame);
        true
    }

    fn collect_threaded_render_results(&mut self, wait: bool) -> bool {
        let mut rendered = false;
        loop {
            let result = match self.render_worker.as_ref() {
                Some(worker) if wait => match worker.recv() {
                    Ok(result) => result,
                    Err(_) => {
                        self.render_worker = None;
                        return rendered;
                    }
                },
                Some(worker) => match worker.try_recv() {
                    Ok(result) => result,
                    Err(TryRecvError::Empty) => return rendered,
                    Err(TryRecvError::Disconnected) => {
                        self.render_worker = None;
                        return rendered;
                    }
                },
                None => return rendered,
            };
            rendered |= self.apply_threaded_render_result(result);
            if wait {
                return rendered;
            }
        }
    }

    fn render_emulated_frame_threaded(&mut self) -> bool {
        let mut rendered = self.collect_threaded_render_results(false);
        let emulated_frame = self.emu.bus().emulated_frames();
        if !should_render_emulated_frame(self.last_submitted_render_frame, emulated_frame) {
            return rendered;
        }

        let input = bitplane::RenderInput::from_bus(self.emu.bus());
        let h_shift = if self.hcenter {
            presentation_h_shift_for(&input.render_base(), self.overscan)
        } else {
            0
        };
        let job = RenderJob {
            generation: self.render_generation,
            input,
            h_shift,
            overscan: self.overscan,
            presentation_fb: std::mem::take(&mut self.render_recycle_fb),
        };
        let send_result = self
            .render_worker
            .as_ref()
            .expect("threaded render path without worker")
            .send(job);
        match send_result {
            Ok(()) => {
                self.last_submitted_render_frame = Some(emulated_frame);
            }
            Err(err) => {
                warn!("render worker stopped; falling back to synchronous rendering");
                self.render_recycle_fb = err;
                self.render_worker = None;
                rendered |= self.render_emulated_frame_sync();
            }
        }
        rendered | self.collect_threaded_render_results(false)
    }

    fn finish_render_for_current_frame(&mut self) -> bool {
        if !self.powered_on {
            return false;
        }
        if !self.emu.bus().frame_render_available() {
            return false;
        }
        let target = self.emu.bus().emulated_frames();
        let mut rendered = self.render_emulated_frame_if_needed();
        while self.render_worker.is_some() && self.last_rendered_emulated_frame != Some(target) {
            rendered |= self.collect_threaded_render_results(true);
        }
        rendered
    }

    fn render_emulated_frame_if_needed(&mut self) -> bool {
        if !self.emu.bus().frame_render_available() {
            return false;
        }
        if self.render_worker.is_some() {
            return self.render_emulated_frame_threaded();
        }
        self.render_emulated_frame_sync()
    }

    fn render_emulated_frame_sync(&mut self) -> bool {
        let emulated_frame = self.emu.bus().emulated_frames();
        if !should_render_emulated_frame(self.last_rendered_emulated_frame, emulated_frame) {
            return false;
        }

        let visible_start_vpos = self.emu.bus().frame_visible_start_vpos();
        let h_shift = if self.hcenter {
            presentation_h_shift_for(&self.emu.bus().frame_render_base(), self.overscan)
        } else {
            0
        };
        bitplane::render(self.emu.bus_mut(), &mut self.fb);
        let geometry = self.emu.bus().frame_geometry();
        let field_rows = post_process_rendered_field(
            &mut self.fb,
            geometry,
            visible_start_vpos,
            h_shift,
            self.overscan,
        );
        let base = self.emu.bus().frame_render_base();
        // Standard 15 kHz fields line-double / weave to 2x rows; a
        // programmable progressive scan already carries every line.
        self.deinterlacer.push_field(
            &self.fb,
            field_rows,
            base.bplcon0 & 0x0004 != 0,
            base.long_field,
            !geometry.programmable,
        );
        self.refresh_present_from_deinterlacer();
        self.last_rendered_emulated_frame = Some(emulated_frame);
        self.last_submitted_render_frame = Some(emulated_frame);
        true
    }
}

/// `[display] overscan = "tv"`: black out the deep-overscan margins like the
/// bezel of a CRT. Demos routinely leave junk in the deep overscan (e.g. HAM
/// streams that converge off-screen, as on the 9 Fingers title); a TV hides
/// it and so does this mask. The emulated framebuffer itself always carries
/// the full field; this runs on the presentation copy only.
///
/// The window is a realistic PAL TV visible area rather than the bare
/// standard window: real sets show a margin of overscan, which intentional
/// overscan displays rely on, while the deep-overscan junk the mask exists
/// to hide sits further out. Default TV presentation keeps 24 lo-res pixels
/// of horizontal overscan beside the standard PAL window; full overscan
/// remains available through `Overscan::Full`. Vertically the default TV
/// view keeps top overscan but uses a tight lower bezel: software can leave
/// active border sprites or unfinished effects below the standard window, and
/// a consumer crop normally hides that. `h_shift` is the horizontal centring
/// shift already applied to the frame, so the bezel tracks the shifted picture
/// instead of clipping its left edge; `standard_top_row` is the framebuffer row
/// where the standard window's first line sits after vertical centring, so the
/// bezel tracks the centred picture vertically too (a fixed line count from
/// row 0 clipped the bottom of every standard 256-line screen by the centring
/// margin).
fn mask_present_frame_to_tv(fb: &mut [u32], h_shift: usize, standard_top_row: usize) {
    debug_assert!(fb.len() >= FB_PIXELS);
    let black = rgba(0, 0, 0);
    let (source_left, source_right) = tv_source_h_bounds();
    let left = source_left.saturating_sub(h_shift);
    let right = source_right.saturating_sub(h_shift).min(FB_WIDTH).max(left);
    let top = standard_top_row.saturating_sub(TV_TOP_OVERSCAN_MARGIN);
    let bottom = (standard_top_row + STANDARD_PAL_VISIBLE_LINES)
        .saturating_sub(TV_BOTTOM_CROP_ROWS)
        .min(FB_HEIGHT);
    for (y, row) in fb.chunks_mut(FB_WIDTH).enumerate() {
        if y < top || y >= bottom {
            row.fill(black);
            continue;
        }
        row[..left].fill(black);
        if right < FB_WIDTH {
            row[right..].fill(black);
        }
    }
}

/// The framebuffer row carrying the standard window's first line after
/// `center_present_frame_for_visible_start` has run: the centring shift
/// plus however many overscan rows were already visible above it.
fn standard_window_top_row(visible_start_vpos: u32) -> usize {
    let overscan_rows_already_visible =
        STANDARD_PAL_VISIBLE_START_VPOS.saturating_sub(visible_start_vpos) as usize;
    presentation_source_y_offset(visible_start_vpos) + overscan_rows_already_visible
}

/// Whether horizontal recentring is enabled. On unless COPPERLINE_HCENTER is
/// set to a falsey value (0/false/off/no), so the standard display can be
/// presented exactly as rendered when debugging alignment.
fn hcenter_enabled() -> bool {
    match crate::envcfg::var("COPPERLINE_HCENTER") {
        Some(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        None => true,
    }
}

fn threaded_render_enabled() -> bool {
    match crate::envcfg::var("COPPERLINE_THREADED_RENDER") {
        Some(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "off" | "no"
        ),
        None => true,
    }
}

/// Shift the rendered frame left by `shift` framebuffer pixels, filling the
/// vacated right columns with black. Used to recentre a standard display
/// whose deep left overscan would otherwise push the picture right of
/// centre. A no-op when `shift` is 0 (overscan frames).
pub(crate) fn center_present_frame_horizontally(fb: &mut [u32], shift: usize) {
    debug_assert!(fb.len() >= FB_PIXELS);
    if shift == 0 {
        return;
    }
    let shift = shift.min(FB_WIDTH);
    let black = rgba(0, 0, 0);
    for y in 0..FB_HEIGHT {
        let row = &mut fb[y * FB_WIDTH..(y + 1) * FB_WIDTH];
        row.copy_within(shift.., 0);
        row[FB_WIDTH - shift..].fill(black);
    }
}

pub(crate) fn center_present_frame_for_visible_start(fb: &mut [u32], visible_start_vpos: u32) {
    debug_assert!(fb.len() >= FB_PIXELS);
    let offset = presentation_source_y_offset(visible_start_vpos);
    if offset == 0 {
        return;
    }

    for y in (0..FB_HEIGHT - offset).rev() {
        let src = y * FB_WIDTH;
        let dst = (y + offset) * FB_WIDTH;
        fb.copy_within(src..src + FB_WIDTH, dst);
    }
    fb[..offset * FB_WIDTH].fill(rgba(0, 0, 0));
}

fn presentation_source_y_offset(visible_start_vpos: u32) -> usize {
    let standard_offset = FB_HEIGHT.saturating_sub(STANDARD_PAL_VISIBLE_LINES) / 2;
    let overscan_rows_already_visible =
        STANDARD_PAL_VISIBLE_START_VPOS.saturating_sub(visible_start_vpos) as usize;
    standard_offset.saturating_sub(overscan_rows_already_visible)
}

fn presentation_h_shift_for(snapshot: &RenderRegisterSnapshot, overscan: Overscan) -> usize {
    match overscan {
        // TV mode masks to a consumer-visible aperture, so center that aperture
        // in the presentation texture even if the raw frame fetches into deeper
        // horizontal overscan.
        Overscan::Tv => tv_standard_h_shift(),
        Overscan::Full => bitplane::present_h_shift_for(snapshot),
    }
}

fn tv_standard_h_shift() -> usize {
    let (source_left, source_right) = tv_source_h_bounds();
    let source_width = source_right.saturating_sub(source_left);
    let centered_left = FB_WIDTH.saturating_sub(source_width) / 2;
    source_left.saturating_sub(centered_left)
}

fn tv_source_h_bounds() -> (usize, usize) {
    let left = bitplane::STANDARD_VISIBLE_X0.saturating_sub(TV_HORIZONTAL_OVERSCAN_MARGIN);
    let right = bitplane::STANDARD_VISIBLE_X0
        .saturating_add(STANDARD_PAL_VISIBLE_WIDTH)
        .saturating_add(TV_HORIZONTAL_OVERSCAN_MARGIN)
        .min(FB_WIDTH)
        .max(left);
    (left, right)
}

fn should_render_emulated_frame(last_rendered: Option<u64>, current: u64) -> bool {
    last_rendered != Some(current)
}

fn status_with_latched_fdd_track(
    status: FrontPanelStatus,
    last_fdd_track: &mut Option<u8>,
) -> FrontPanelStatus {
    let fdd_track = match status.fdd_track {
        Some(track) => {
            *last_fdd_track = Some(track);
            Some(track)
        }
        None => *last_fdd_track,
    };
    FrontPanelStatus {
        fdd_track,
        ..status
    }
}

fn take_integral_mouse_delta(value: &mut f64) -> i32 {
    let whole = value.trunc();
    if whole > i32::MAX as f64 {
        *value = 0.0;
        i32::MAX
    } else if whole < i32::MIN as f64 {
        *value = 0.0;
        i32::MIN
    } else {
        *value -= whole;
        whole as i32
    }
}

/// Hex digit for a key, for the debugger's address entry box.
/// Map a key to the character it types into the debugger entry box: digits, all
/// letters (for register names and the EQ/NE/.../AND/IGN condition mnemonics and
/// M<hex> memory operands), and Space. `push_entry_char` filters and uppercases.
fn entry_char_for_key(code: KeyCode) -> Option<char> {
    use KeyCode::*;
    Some(match code {
        Digit0 | Numpad0 => '0',
        Digit1 | Numpad1 => '1',
        Digit2 | Numpad2 => '2',
        Digit3 | Numpad3 => '3',
        Digit4 | Numpad4 => '4',
        Digit5 | Numpad5 => '5',
        Digit6 | Numpad6 => '6',
        Digit7 | Numpad7 => '7',
        Digit8 | Numpad8 => '8',
        Digit9 | Numpad9 => '9',
        KeyA => 'A',
        KeyB => 'B',
        KeyC => 'C',
        KeyD => 'D',
        KeyE => 'E',
        KeyF => 'F',
        KeyG => 'G',
        KeyH => 'H',
        KeyI => 'I',
        KeyJ => 'J',
        KeyK => 'K',
        KeyL => 'L',
        KeyM => 'M',
        KeyN => 'N',
        KeyO => 'O',
        KeyP => 'P',
        KeyQ => 'Q',
        KeyR => 'R',
        KeyS => 'S',
        KeyT => 'T',
        KeyU => 'U',
        KeyV => 'V',
        KeyW => 'W',
        KeyX => 'X',
        KeyY => 'Y',
        KeyZ => 'Z',
        Space => ' ',
        _ => return None,
    })
}

/// Format a calibration session for the panel: pad identity, one row per
/// step with its captured binding, and a prompt for what to do next.
fn build_calibration_view(session: &crate::gamepad::CalibrationSession) -> ui::CalibrationView {
    use crate::gamepad::CalibrationSession;
    let pad_line = if session.backend_missing() {
        "No gamepad backend available on this host".to_string()
    } else {
        match (session.pad_name(), session.connected()) {
            (Some(name), true) => format!("Controller: {name}"),
            (Some(name), false) => format!("Reconnect {name}..."),
            (None, _) => "Connect a controller...".to_string(),
        }
    };
    let rows = (0..CalibrationSession::step_count())
        .map(|index| ui::CalRow {
            label: CalibrationSession::step_label(index),
            binding: session.binding_text(index),
            current: session.current_step() == Some(index),
        })
        .collect();
    let status = if session.backend_missing() {
        "Calibration needs a gamepad backend (not available headless).".to_string()
    } else if session.done() {
        if session.live_test().is_empty() {
            "All steps captured. Push controls to test, then Save.".to_string()
        } else {
            format!("Testing: {}", session.live_test())
        }
    } else if !session.connected() {
        "Waiting for a controller to be connected.".to_string()
    } else if session.can_skip() {
        "Push and hold the control, or Skip if the pad lacks it.".to_string()
    } else {
        "Push and hold the control on the pad.".to_string()
    };
    ui::CalibrationView {
        pad_line,
        rows,
        status,
    }
}

/// Translate a winit `KeyCode` to an Amiga raw scan code.
/// Covers the alphanumeric block, common modifiers we treat as their
/// nearest Amiga equivalents, function keys, and arrows. Anything not
/// in the table returns None (the keypress is silently dropped).
fn host_to_amiga_rawkey(code: KeyCode) -> Option<u8> {
    use KeyCode::*;
    Some(match code {
        // Letters (row-by-row, Amiga's funny layout)
        KeyA => 0x20,
        KeyB => 0x35,
        KeyC => 0x33,
        KeyD => 0x22,
        KeyE => 0x12,
        KeyF => 0x23,
        KeyG => 0x24,
        KeyH => 0x25,
        KeyI => 0x17,
        KeyJ => 0x26,
        KeyK => 0x27,
        KeyL => 0x28,
        KeyM => 0x37,
        KeyN => 0x36,
        KeyO => 0x18,
        KeyP => 0x19,
        KeyQ => 0x10,
        KeyR => 0x13,
        KeyS => 0x21,
        KeyT => 0x14,
        KeyU => 0x16,
        KeyV => 0x34,
        KeyW => 0x11,
        KeyX => 0x32,
        KeyY => 0x15,
        KeyZ => 0x31,
        // Top-row digits
        Digit1 => 0x01,
        Digit2 => 0x02,
        Digit3 => 0x03,
        Digit4 => 0x04,
        Digit5 => 0x05,
        Digit6 => 0x06,
        Digit7 => 0x07,
        Digit8 => 0x08,
        Digit9 => 0x09,
        Digit0 => 0x0A,
        // Punctuation
        Backquote => 0x00,
        Minus => 0x0B,
        Equal => 0x0C,
        Backslash => 0x0D,
        BracketLeft => 0x1A,
        BracketRight => 0x1B,
        Semicolon => 0x29,
        Quote => 0x2A,
        Comma => 0x38,
        Period => 0x39,
        Slash => 0x3A,
        // International keys: the ISO 102nd key between left Shift and
        // Z is Amiga rawkey $30; the Japanese Ro key sits in the same
        // matrix position on layouts that have it.
        IntlBackslash | IntlRo => 0x30,
        // Control
        Space => 0x40,
        Enter => 0x44,
        Backspace => 0x41,
        Tab => 0x42,
        Escape => 0x45,
        Delete => 0x46,
        // Amiga Help: F11 host-side (no dedicated host key exists).
        F11 => 0x5F,
        ShiftLeft => 0x60,
        ShiftRight => 0x61,
        CapsLock => 0x62,
        ControlLeft | ControlRight => 0x63,
        AltLeft => 0x64,
        AltRight => 0x65,
        SuperLeft => 0x66,
        SuperRight => 0x67,
        // Arrows
        ArrowUp => 0x4C,
        ArrowDown => 0x4D,
        ArrowRight => 0x4E,
        ArrowLeft => 0x4F,
        // Function keys
        F1 => 0x50,
        F2 => 0x51,
        F3 => 0x52,
        F4 => 0x53,
        F5 => 0x54,
        F6 => 0x55,
        F7 => 0x56,
        F8 => 0x57,
        F9 => 0x58,
        F10 => 0x59,
        // Numpad
        Numpad0 => 0x0F,
        Numpad1 => 0x1D,
        Numpad2 => 0x1E,
        Numpad3 => 0x1F,
        Numpad4 => 0x2D,
        Numpad5 => 0x2E,
        Numpad6 => 0x2F,
        Numpad7 => 0x3D,
        Numpad8 => 0x3E,
        Numpad9 => 0x3F,
        NumpadDecimal => 0x3C,
        NumpadEnter => 0x43,
        NumpadSubtract => 0x4A,
        NumpadAdd => 0x5E,
        NumpadMultiply => 0x5D,
        NumpadDivide => 0x5C,
        NumpadParenLeft => 0x5A,
        NumpadParenRight => 0x5B,
        _ => return None,
    })
}

/// Parse an Amiga raw key as either a decimal/hex code or a common
/// key name for scripted input.
pub fn parse_amiga_key(s: &str) -> Option<u8> {
    let trimmed = s.trim();
    if let Some(raw) = parse_u8(trimmed) {
        return Some(raw);
    }
    let name = trimmed.to_ascii_lowercase().replace(['-', '_', '+'], "");
    Some(match name.as_str() {
        "a" => 0x20,
        "b" => 0x35,
        "c" => 0x33,
        "d" => 0x22,
        "e" => 0x12,
        "f" => 0x23,
        "g" => 0x24,
        "h" => 0x25,
        "i" => 0x17,
        "j" => 0x26,
        "k" => 0x27,
        "l" => 0x28,
        "m" => 0x37,
        "n" => 0x36,
        "o" => 0x18,
        "p" => 0x19,
        "q" => 0x10,
        "r" => 0x13,
        "s" => 0x21,
        "t" => 0x14,
        "u" => 0x16,
        "v" => 0x34,
        "w" => 0x11,
        "x" => 0x32,
        "y" => 0x15,
        "z" => 0x31,
        "1" => 0x01,
        "2" => 0x02,
        "3" => 0x03,
        "4" => 0x04,
        "5" => 0x05,
        "6" => 0x06,
        "7" => 0x07,
        "8" => 0x08,
        "9" => 0x09,
        "0" => 0x0A,
        "space" => 0x40,
        "backspace" | "bs" => 0x41,
        "tab" => 0x42,
        "enter" | "return" => 0x44,
        "escape" | "esc" => 0x45,
        "delete" | "del" => 0x46,
        "up" | "arrowup" => 0x4C,
        "down" | "arrowdown" => 0x4D,
        "right" | "arrowright" => 0x4E,
        "left" | "arrowleft" => 0x4F,
        "f1" => 0x50,
        "f2" => 0x51,
        "f3" => 0x52,
        "f4" => 0x53,
        "f5" => 0x54,
        "f6" => 0x55,
        "f7" => 0x56,
        "f8" => 0x57,
        "f9" => 0x58,
        "f10" => 0x59,
        "lshift" | "leftshift" | "shift" => 0x60,
        "rshift" | "rightshift" => 0x61,
        "caps" | "capslock" => 0x62,
        "ctrl" | "control" | "lctrl" | "leftctrl" | "leftcontrol" | "rctrl" | "rightctrl"
        | "rightcontrol" => 0x63,
        "lalt" | "leftalt" | "alt" => 0x64,
        "ralt" | "rightalt" => 0x65,
        "lami" | "leftami" | "lamiga" | "leftamiga" | "ami" | "amiga" | "cmd" | "command"
        | "super" | "meta" => 0x66,
        "rami" | "rightami" | "ramiga" | "rightamiga" => 0x67,
        _ => return None,
    })
}

fn parse_u8(s: &str) -> Option<u8> {
    if let Some(rest) = s.strip_prefix("0x").or_else(|| s.strip_prefix("$")) {
        u8::from_str_radix(rest, 16).ok()
    } else {
        s.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::ui::{Panel, UiControl};
    use super::{
        bar_layout, center_present_frame_for_visible_start, center_present_frame_horizontally,
        control_at, copperline_icon_image, copperline_logo_image, copy_present_frame,
        draw_status_bar, fdd_track_counter_rect, fdd_track_digit_rect,
        host_shortcut_modifier_pressed, host_to_amiga_rawkey, joystick_mode_uses_keyboard,
        joystick_toggle_rect, keyboard_joystick_key_for, led_row_rect, mask_present_frame_to_tv,
        paint_test_screen, parse_amiga_key, pause_button_rect, power_button_rect,
        present_row_sample, presentation_source_y_offset, reboot_button_rect, rgba,
        shot_button_rect, should_render_emulated_frame, standard_window_top_row,
        status_with_latched_fdd_track, take_integral_mouse_delta, texture_height, texture_width,
        tv_source_h_bounds, tv_standard_h_shift, volume_percent_from_pos, volume_slider_track_rect,
        BarControl, DriveBar, JoystickInputMode, KeyboardJoystickHeld, KeyboardJoystickKey,
        MediaBar, StatusBarView, ToolPanelKind, BUTTON_GLYPH, BUTTON_GLYPH_DISABLED, CD_BODY,
        CD_LED_OFF, CD_LED_ON, DISK_BODY, DISK_BODY_SHADOW, DISK_LABEL, FDD_LED_OFF, FDD_LED_ON,
        HDD_LED_OFF, HDD_LED_ON, POWER_GLYPH_OFF, POWER_GLYPH_ON, POWER_LED_OFF, POWER_LED_ON,
        PRESENT_HEIGHT, STANDARD_PAL_VISIBLE_LINES, STANDARD_PAL_VISIBLE_START_VPOS, STATUS_BG,
        TRACK_SEGMENT_OFF, TRACK_SEGMENT_ON, VOLUME_FILL, VOLUME_GLYPH_X,
    };
    use crate::audio::{AudioSink, NullSink};
    use crate::bus::FrontPanelStatus;
    use crate::config::WarpSpeed;
    use crate::video::{FB_HEIGHT, FB_PIXELS, FB_WIDTH};
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;
    use winit::keyboard::{KeyCode, ModifiersState};

    /// A typical session: DF0 connected with a disk in, no CD drive.
    fn single_drive_media() -> MediaBar {
        let mut drives = [DriveBar::default(); 4];
        drives[0] = DriveBar {
            connected: true,
            inserted: true,
            multi: false,
        };
        MediaBar { drives, cd: None }
    }

    fn media(connected: usize, cd: Option<bool>) -> MediaBar {
        let mut drives = [DriveBar::default(); 4];
        for drive in drives.iter_mut().take(connected) {
            *drive = DriveBar {
                connected: true,
                inserted: true,
                multi: false,
            };
        }
        MediaBar { drives, cd }
    }

    fn view(status: FrontPanelStatus, powered_on: bool, paused: bool) -> StatusBarView {
        StatusBarView {
            status,
            powered_on,
            paused,
            media: single_drive_media(),
            joystick_input_mode: JoystickInputMode::Gamepad,
            hover: None,
        }
    }

    #[test]
    fn host_mapping_includes_amiga_modifiers() {
        assert_eq!(host_to_amiga_rawkey(KeyCode::ControlLeft), Some(0x63));
        assert_eq!(host_to_amiga_rawkey(KeyCode::ControlRight), Some(0x63));
        assert_eq!(host_to_amiga_rawkey(KeyCode::AltLeft), Some(0x64));
        assert_eq!(host_to_amiga_rawkey(KeyCode::AltRight), Some(0x65));
        assert_eq!(host_to_amiga_rawkey(KeyCode::SuperLeft), Some(0x66));
        assert_eq!(host_to_amiga_rawkey(KeyCode::SuperRight), Some(0x67));
    }

    #[test]
    fn keyboard_joystick_mapping_matches_fsuae_controls() {
        assert_eq!(
            keyboard_joystick_key_for(KeyCode::ArrowUp),
            Some(KeyboardJoystickKey::Up)
        );
        assert_eq!(
            keyboard_joystick_key_for(KeyCode::ArrowDown),
            Some(KeyboardJoystickKey::Down)
        );
        assert_eq!(
            keyboard_joystick_key_for(KeyCode::ArrowLeft),
            Some(KeyboardJoystickKey::Left)
        );
        assert_eq!(
            keyboard_joystick_key_for(KeyCode::ArrowRight),
            Some(KeyboardJoystickKey::Right)
        );
        assert_eq!(
            keyboard_joystick_key_for(KeyCode::ControlRight),
            Some(KeyboardJoystickKey::FireRightCtrl)
        );
        assert_eq!(
            keyboard_joystick_key_for(KeyCode::AltRight),
            Some(KeyboardJoystickKey::FireRightAlt)
        );
        assert_eq!(
            keyboard_joystick_key_for(KeyCode::KeyC),
            Some(KeyboardJoystickKey::Red)
        );
        assert_eq!(
            keyboard_joystick_key_for(KeyCode::KeyX),
            Some(KeyboardJoystickKey::Blue)
        );
        assert_eq!(
            keyboard_joystick_key_for(KeyCode::KeyD),
            Some(KeyboardJoystickKey::Green)
        );
        assert_eq!(
            keyboard_joystick_key_for(KeyCode::KeyS),
            Some(KeyboardJoystickKey::Yellow)
        );
        assert_eq!(
            keyboard_joystick_key_for(KeyCode::Enter),
            Some(KeyboardJoystickKey::Play)
        );
        assert_eq!(
            keyboard_joystick_key_for(KeyCode::KeyZ),
            Some(KeyboardJoystickKey::Rewind)
        );
        assert_eq!(
            keyboard_joystick_key_for(KeyCode::KeyA),
            Some(KeyboardJoystickKey::Forward)
        );
        assert_eq!(keyboard_joystick_key_for(KeyCode::ControlLeft), None);
    }

    #[test]
    fn keyboard_joystick_fire_aliases_release_independently() {
        let mut held = KeyboardJoystickHeld::default();
        held.set(KeyboardJoystickKey::FireRightCtrl, true);
        held.set(KeyboardJoystickKey::Red, true);
        assert!(held.joystick_state().fire);

        held.set(KeyboardJoystickKey::FireRightCtrl, false);
        assert!(held.joystick_state().fire);

        held.set(KeyboardJoystickKey::Red, false);
        assert!(!held.joystick_state().fire);
    }

    #[test]
    fn joystick_input_mode_toggles_between_two_explicit_modes() {
        // The toggle flips directly between the two modes; there is no hidden
        // auto-detect state, so the keyboard mapping is engaged exactly when
        // (and only when) the mode is Keyboard.
        assert_eq!(
            JoystickInputMode::Gamepad.next(),
            JoystickInputMode::Keyboard
        );
        assert_eq!(
            JoystickInputMode::Keyboard.next(),
            JoystickInputMode::Gamepad
        );

        assert!(joystick_mode_uses_keyboard(JoystickInputMode::Keyboard));
        assert!(!joystick_mode_uses_keyboard(JoystickInputMode::Gamepad));
    }

    #[test]
    fn joystick_toggle_clears_worst_case_media() {
        // The toggle sits at a fixed x just left of the volume glyph. The
        // widest media layout (four floppies plus a CD) must not reach it, and
        // it must stay left of the volume control's hit area.
        let toggle = joystick_toggle_rect();
        let layout = bar_layout(&media(4, Some(true)));
        let media_right = layout
            .cd_eject
            .into_iter()
            .chain(layout.drive_eject.into_iter().flatten())
            .map(|r| r.x + r.w)
            .max()
            .unwrap();
        assert!(
            media_right <= toggle.x,
            "media right edge {media_right} overlaps joystick toggle at {}",
            toggle.x
        );
        assert!(toggle.x + toggle.w <= VOLUME_GLYPH_X);
    }

    #[test]
    fn joystick_toggle_is_hit_tested_and_draws_each_mode() {
        let layout = bar_layout(&single_drive_media());
        let toggle = joystick_toggle_rect();
        let center = (
            (toggle.x + toggle.w / 2) as i32,
            (toggle.y + toggle.h / 2) as i32,
        );
        assert_eq!(control_at(center, &layout), Some(BarControl::Joystick));

        // Each mode lights the green glyph somewhere in the button (gamepad
        // body vs. keyboard keys), so the two states are visually distinct.
        let scale = 1;
        for mode in [JoystickInputMode::Gamepad, JoystickInputMode::Keyboard] {
            let mut frame = vec![0u8; texture_width(scale) * texture_height(scale) * 4];
            let mut v = view(FrontPanelStatus::default(), true, false);
            v.joystick_input_mode = mode;
            draw_status_bar(&mut frame, &v, scale);
            let lit = (toggle.y..toggle.y + toggle.h).any(|y| {
                (toggle.x..toggle.x + toggle.w)
                    .any(|x| pixel(&frame, x, y, scale) == BUTTON_GLYPH.to_le_bytes())
            });
            assert!(lit, "joystick toggle drew no glyph for {mode:?}");
        }
    }

    #[test]
    fn host_shortcut_modifier_uses_platform_convention() {
        #[cfg(target_os = "macos")]
        {
            assert!(host_shortcut_modifier_pressed(ModifiersState::SUPER));
            assert!(!host_shortcut_modifier_pressed(ModifiersState::ALT));
        }

        #[cfg(not(target_os = "macos"))]
        {
            assert!(host_shortcut_modifier_pressed(ModifiersState::ALT));
            assert!(!host_shortcut_modifier_pressed(ModifiersState::SUPER));
        }
    }

    #[test]
    fn named_key_parser_accepts_modifiers_and_raw_codes() {
        assert_eq!(parse_amiga_key("ctrl"), Some(0x63));
        assert_eq!(parse_amiga_key("left-alt"), Some(0x64));
        assert_eq!(parse_amiga_key("ralt"), Some(0x65));
        assert_eq!(parse_amiga_key("lami"), Some(0x66));
        assert_eq!(parse_amiga_key("right-amiga"), Some(0x67));
        assert_eq!(parse_amiga_key("0x04"), Some(0x04));
        assert_eq!(parse_amiga_key("$04"), Some(0x04));
        assert_eq!(parse_amiga_key("unknown-key"), None);
    }

    #[test]
    fn renderer_runs_once_per_emulated_frame() {
        assert!(should_render_emulated_frame(None, 0));
        assert!(!should_render_emulated_frame(Some(12), 12));
        assert!(should_render_emulated_frame(Some(12), 13));
    }

    #[test]
    fn warp_burst_decouples_emulation_from_the_vsync_present() {
        let mut app = test_app();
        // test_app builds an unpaced (warp) emulator. Default warp level is
        // Max: retire many frames per presented frame, bounded by a wall-clock
        // budget so the loop still presents at vsync.
        app.warp_speed = WarpSpeed::Max;
        let (cap, budget) = app.warp_burst_plan(false);
        assert!(cap > 1, "warp Max must skip output frames, got cap {cap}");
        assert!(budget.is_some(), "Max bounds the burst by wall-clock time");

        // A fixed level retires exactly its multiplier in frames, with no time
        // bound -- predictable speed = level x refresh rate.
        app.warp_speed = WarpSpeed::X4;
        assert_eq!(app.warp_burst_plan(false), (4, None));

        // Headless capture renders every frame, so the burst must not engage
        // even though the core is unpaced.
        assert_eq!(app.warp_burst_plan(true), (1, None));

        // Real-time pacing presents one frame per loop regardless of level.
        app.emu.set_paced(true);
        assert_eq!(app.warp_burst_plan(false), (1, None));
    }

    #[test]
    fn cycle_warp_speed_walks_the_levels() {
        let mut app = test_app();
        app.warp_speed = WarpSpeed::X8;
        app.cycle_warp_speed();
        assert_eq!(app.warp_speed, WarpSpeed::X16);
        app.cycle_warp_speed();
        assert_eq!(app.warp_speed, WarpSpeed::Max);
        app.cycle_warp_speed();
        assert_eq!(app.warp_speed, WarpSpeed::X2);
    }

    #[test]
    fn mouse_delta_integrator_keeps_fractional_remainder() {
        let mut delta = 0.75;
        assert_eq!(take_integral_mouse_delta(&mut delta), 0);
        assert_eq!(delta, 0.75);

        delta += 0.5;
        assert_eq!(take_integral_mouse_delta(&mut delta), 1);
        assert_eq!(delta, 0.25);

        delta -= 1.5;
        assert_eq!(take_integral_mouse_delta(&mut delta), -1);
        assert_eq!(delta, -0.25);
    }

    #[test]
    fn status_bar_draws_hdd_led_only_on_ide_machines() {
        let scale = 1;
        let mut frame = vec![0u8; texture_width(scale) * texture_height(scale) * 4];
        // Row 2 of 3 is where the HDD LED sits on an IDE machine.
        let hdd = led_row_rect(2, 3);

        // No IDE port: the HDD row stays status-bar background.
        draw_status_bar(
            &mut frame,
            &view(FrontPanelStatus::default(), true, false),
            scale,
        );
        assert_eq!(
            pixel(&frame, hdd.x + hdd.w / 2, hdd.y + hdd.h / 2, scale),
            STATUS_BG.to_le_bytes()
        );

        draw_status_bar(
            &mut frame,
            &view(
                FrontPanelStatus {
                    hdd_led: Some(false),
                    ..FrontPanelStatus::default()
                },
                true,
                false,
            ),
            scale,
        );
        assert_eq!(
            pixel(&frame, hdd.x + hdd.w / 2, hdd.y + hdd.h / 2, scale),
            HDD_LED_OFF.to_le_bytes()
        );

        draw_status_bar(
            &mut frame,
            &view(
                FrontPanelStatus {
                    hdd_led: Some(true),
                    ..FrontPanelStatus::default()
                },
                true,
                false,
            ),
            scale,
        );
        assert_eq!(
            pixel(&frame, hdd.x + hdd.w / 2, hdd.y + hdd.h / 2, scale),
            HDD_LED_ON.to_le_bytes()
        );
    }

    #[test]
    fn status_bar_draws_power_and_fdd_led_states() {
        let scale = 1;
        let mut frame = vec![0u8; texture_width(scale) * texture_height(scale) * 4];
        draw_status_bar(
            &mut frame,
            &view(
                FrontPanelStatus {
                    power_led_on: true,
                    fdd_led_on: false,
                    fdd_track: Some(5),
                    hdd_led: None,
                    cd_led: None,
                    output_volume_percent: 100,
                },
                true,
                false,
            ),
            scale,
        );

        let power = led_row_rect(0, 2);
        let fdd = led_row_rect(1, 2);
        assert_eq!(
            pixel(&frame, power.x + power.w / 2, power.y + power.h / 2, scale),
            POWER_LED_ON.to_le_bytes()
        );
        assert_eq!(
            pixel(&frame, fdd.x + fdd.w / 2, fdd.y + fdd.h / 2, scale),
            FDD_LED_OFF.to_le_bytes()
        );
        let hundreds = fdd_track_digit_rect(0);
        let ones = fdd_track_digit_rect(2);
        assert_eq!(
            pixel(
                &frame,
                hundreds.x + hundreds.w / 2,
                hundreds.y + hundreds.h / 2,
                scale
            ),
            TRACK_SEGMENT_OFF.to_le_bytes()
        );
        assert_eq!(
            pixel(&frame, ones.x + ones.w / 2, ones.y + ones.h / 2, scale),
            TRACK_SEGMENT_ON.to_le_bytes()
        );

        draw_status_bar(
            &mut frame,
            &view(
                FrontPanelStatus {
                    power_led_on: false,
                    fdd_led_on: true,
                    fdd_track: Some(42),
                    hdd_led: None,
                    cd_led: None,
                    output_volume_percent: 100,
                },
                true,
                false,
            ),
            scale,
        );
        assert_eq!(
            pixel(&frame, fdd.x + fdd.w / 2, fdd.y + fdd.h / 2, scale),
            FDD_LED_ON.to_le_bytes()
        );
    }

    #[test]
    fn status_bar_extinguishes_power_led_when_host_power_is_off() {
        let scale = 1;
        let mut frame = vec![0u8; texture_width(scale) * texture_height(scale) * 4];
        draw_status_bar(
            &mut frame,
            &view(
                FrontPanelStatus {
                    power_led_on: true,
                    fdd_led_on: false,
                    fdd_track: Some(0),
                    hdd_led: None,
                    cd_led: None,
                    output_volume_percent: 100,
                },
                false,
                false,
            ),
            scale,
        );

        let power = led_row_rect(0, 2);
        assert_eq!(
            pixel(&frame, power.x + power.w / 2, power.y + power.h / 2, scale),
            POWER_LED_OFF.to_le_bytes()
        );
    }

    #[test]
    fn status_bar_stacks_fdd_led_under_power_led() {
        let power = led_row_rect(0, 2);
        let fdd = led_row_rect(1, 2);
        let track = fdd_track_counter_rect();
        let layout = bar_layout(&single_drive_media());

        assert_eq!(fdd.x, power.x);
        assert_eq!(fdd.w, power.w);
        assert!(fdd.y >= power.y + power.h);
        assert!(track.x >= power.x + power.w);
        assert!(layout.drive_load[0].unwrap().x >= track.x + track.w);
    }

    #[test]
    fn status_bar_power_button_glyph_tracks_power_state() {
        let scale = 1;
        let button = power_button_rect();
        // A pixel squarely on the glyph's vertical bar (button centre
        // column, a few rows above centre), where line coverage is full.
        let gx = button.x + button.w / 2;
        let gy = button.y + button.h / 2 - 3;

        for (powered_on, expected) in [(true, POWER_GLYPH_ON), (false, POWER_GLYPH_OFF)] {
            let mut frame = vec![0u8; texture_width(scale) * texture_height(scale) * 4];
            draw_status_bar(
                &mut frame,
                &view(
                    FrontPanelStatus {
                        power_led_on: powered_on,
                        fdd_led_on: false,
                        fdd_track: Some(0),
                        hdd_led: None,
                        cd_led: None,
                        output_volume_percent: 100,
                    },
                    powered_on,
                    false,
                ),
                scale,
            );
            assert_eq!(pixel(&frame, gx, gy, scale), expected.to_le_bytes());
        }
    }

    #[test]
    fn test_screen_paints_colour_bars_over_a_grey_wedge() {
        use crate::video::{FB_HEIGHT, FB_PIXELS, FB_WIDTH};
        let mut fb = vec![0u32; FB_PIXELS];
        paint_test_screen(&mut fb);

        // Top region: leftmost bar is grey, rightmost is blue.
        assert_eq!(fb[0], rgba(192, 192, 192));
        assert_eq!(fb[FB_WIDTH - 1], rgba(0, 0, 192));

        // Bottom region: grey wedge runs from black at the left to white
        // at the right.
        let bottom = (FB_HEIGHT - 1) * FB_WIDTH;
        assert_eq!(fb[bottom], rgba(0, 0, 0));
        assert_eq!(fb[bottom + FB_WIDTH - 1], rgba(255, 255, 255));
    }

    #[test]
    fn embedded_brand_assets_decode_with_transparent_edges() {
        let logo = copperline_logo_image().expect("embedded logo PNG");
        assert_eq!((logo.width, logo.height), (620, 128));
        assert_eq!(logo.rgba[3], 0);
        assert!(logo.rgba.chunks_exact(4).any(|px| px[3] == 0xFF));

        let icon = copperline_icon_image().expect("embedded icon PNG");
        assert_eq!((icon.width, icon.height), (256, 256));
        assert_eq!(icon.rgba[3], 0);
        assert!(icon.rgba.chunks_exact(4).any(|px| px[3] == 0xFF));
    }

    #[test]
    fn test_screen_blits_copperline_logo_over_colour_bars() {
        let mut fb = vec![0u32; FB_PIXELS];
        paint_test_screen(&mut fb);

        let logo = copperline_logo_image().expect("embedded logo PNG");
        let (idx, px) = logo
            .rgba
            .chunks_exact(4)
            .enumerate()
            .find(|(_, px)| px[3] == 0xFF)
            .expect("opaque logo pixel");
        let x = FB_WIDTH.saturating_sub(logo.width) / 2 + idx % logo.width;
        let y = (FB_HEIGHT * 4 / 5).saturating_sub(logo.height) / 2 + idx / logo.width;

        assert_eq!(
            fb[y * FB_WIDTH + x],
            rgba(px[0] as u32, px[1] as u32, px[2] as u32)
        );
    }

    #[test]
    fn power_button_sits_left_of_reboot_without_overlap() {
        let power = power_button_rect();
        let reboot = reboot_button_rect();
        let volume = volume_slider_track_rect();
        assert!(power.x + power.w <= reboot.x);
        assert!(power.x >= volume.x + volume.w);
        assert_eq!(power.y, reboot.y);
        assert_eq!(power.h, reboot.h);
    }

    #[test]
    fn pause_button_sits_left_of_power_without_overlap() {
        let pause = pause_button_rect();
        let power = power_button_rect();
        let volume = volume_slider_track_rect();
        assert!(pause.x + pause.w <= power.x);
        assert!(pause.x >= volume.x + volume.w);
        assert_eq!(pause.y, power.y);
        assert_eq!(pause.h, power.h);
    }

    #[test]
    fn status_bar_pause_button_glyph_tracks_pause_state() {
        let scale = 1;
        let button = pause_button_rect();
        // Centre column, a few rows above centre: on the play triangle's
        // body when paused and between the pause bars when running.
        let cx = button.x + button.w / 2;
        let cy = button.y + button.h / 2;

        // Paused: a play triangle fills the centre column.
        let mut frame = vec![0u8; texture_width(scale) * texture_height(scale) * 4];
        draw_status_bar(
            &mut frame,
            &view(
                FrontPanelStatus {
                    power_led_on: true,
                    fdd_led_on: false,
                    fdd_track: Some(0),
                    hdd_led: None,
                    cd_led: None,
                    output_volume_percent: 100,
                },
                true,
                true,
            ),
            scale,
        );
        assert_eq!(pixel(&frame, cx, cy, scale), BUTTON_GLYPH.to_le_bytes());

        // Running: the gap between the two pause bars leaves the centre
        // column on the button face.
        let mut frame = vec![0u8; texture_width(scale) * texture_height(scale) * 4];
        draw_status_bar(
            &mut frame,
            &view(
                FrontPanelStatus {
                    power_led_on: true,
                    fdd_led_on: false,
                    fdd_track: Some(0),
                    hdd_led: None,
                    cd_led: None,
                    output_volume_percent: 100,
                },
                true,
                false,
            ),
            scale,
        );
        assert_ne!(pixel(&frame, cx, cy, scale), BUTTON_GLYPH.to_le_bytes());
    }

    #[test]
    fn status_bar_draws_disk_image_button_next_to_track_counter() {
        let scale = 1;
        let mut frame = vec![0u8; texture_width(scale) * texture_height(scale) * 4];
        draw_status_bar(
            &mut frame,
            &view(
                FrontPanelStatus {
                    power_led_on: true,
                    fdd_led_on: true,
                    fdd_track: Some(5),
                    hdd_led: None,
                    cd_led: None,
                    output_volume_percent: 50,
                },
                true,
                false,
            ),
            scale,
        );

        let layout = bar_layout(&single_drive_media());
        let button = layout.drive_load[0].unwrap();
        let track = fdd_track_counter_rect();
        assert!(button.x >= track.x + track.w);
        assert_eq!(
            pixel(&frame, button.x + 5, button.y + 11, scale),
            DISK_BODY.to_le_bytes()
        );
        assert_eq!(
            pixel(&frame, button.x + button.w / 2, button.y + 15, scale),
            DISK_LABEL.to_le_bytes()
        );
        // The drive number 0 is written on the disk label (top-left of
        // the 3x5 digit).
        assert_eq!(
            pixel(&frame, button.x + 12, button.y + 12, scale),
            DISK_BODY_SHADOW.to_le_bytes()
        );
    }

    #[test]
    fn status_bar_draws_swap_and_eject_buttons_with_enable_states() {
        let scale = 1;
        let mut bar = single_drive_media();
        let layout = bar_layout(&bar);
        let swap = layout.drive_swap[0].unwrap();
        let eject = layout.drive_eject[0].unwrap();
        assert!(swap.x >= layout.drive_load[0].unwrap().x + 22);
        assert!(eject.x >= swap.x + swap.w);

        // Single disk, inserted: swap is dim, eject is live.
        let mut frame = vec![0u8; texture_width(scale) * texture_height(scale) * 4];
        let mut v = view(FrontPanelStatus::default(), true, false);
        draw_status_bar(&mut frame, &v, scale);
        assert_eq!(
            pixel(&frame, swap.x + 5, swap.y + 8, scale),
            BUTTON_GLYPH_DISABLED.to_le_bytes()
        );
        assert_eq!(
            pixel(&frame, eject.x + 5, eject.y + 15, scale),
            BUTTON_GLYPH.to_le_bytes()
        );

        // Playlist queued, no disk in: swap is live, eject is dim.
        bar.drives[0].multi = true;
        bar.drives[0].inserted = false;
        v.media = bar;
        let mut frame = vec![0u8; texture_width(scale) * texture_height(scale) * 4];
        draw_status_bar(&mut frame, &v, scale);
        assert_eq!(
            pixel(&frame, swap.x + 5, swap.y + 8, scale),
            BUTTON_GLYPH.to_le_bytes()
        );
        assert_eq!(
            pixel(&frame, eject.x + 5, eject.y + 15, scale),
            BUTTON_GLYPH_DISABLED.to_le_bytes()
        );
    }

    #[test]
    fn status_bar_draws_cd_buttons_only_on_cd_machines() {
        assert!(bar_layout(&media(1, None)).cd_load.is_none());
        assert!(bar_layout(&media(1, None)).cd_eject.is_none());

        let bar = media(1, Some(true));
        let layout = bar_layout(&bar);
        let cd_load = layout.cd_load.unwrap();
        let cd_eject = layout.cd_eject.unwrap();
        assert!(cd_load.x >= layout.drive_eject[0].unwrap().x + 16);
        assert!(cd_eject.x >= cd_load.x + cd_load.w);

        let scale = 1;
        let mut frame = vec![0u8; texture_width(scale) * texture_height(scale) * 4];
        let v = StatusBarView {
            status: FrontPanelStatus::default(),
            powered_on: true,
            paused: false,
            media: bar,
            joystick_input_mode: JoystickInputMode::Gamepad,
            hover: None,
        };
        draw_status_bar(&mut frame, &v, scale);
        // The disc body below the hub.
        assert_eq!(
            pixel(&frame, cd_load.x + 11, cd_load.y + 17, scale),
            CD_BODY.to_le_bytes()
        );
        assert_eq!(
            pixel(&frame, cd_eject.x + 5, cd_eject.y + 15, scale),
            BUTTON_GLYPH.to_le_bytes()
        );
    }

    #[test]
    fn status_bar_draws_cd_led_on_cd_machines() {
        let scale = 1;

        // CDTV/CD32 without IDE: rows are PWR, FDD, CD at the classic
        // three-row spacing.
        let mut frame = vec![0u8; texture_width(scale) * texture_height(scale) * 4];
        let mut v = view(
            FrontPanelStatus {
                cd_led: Some(true),
                ..FrontPanelStatus::default()
            },
            true,
            false,
        );
        v.media = media(1, Some(true));
        draw_status_bar(&mut frame, &v, scale);
        let cd = led_row_rect(2, 3);
        assert_eq!(
            pixel(&frame, cd.x + cd.w / 2, cd.y + cd.h / 2, scale),
            CD_LED_ON.to_le_bytes()
        );

        // With IDE as well, all four rows pack tighter and the CD LED is
        // the last row, still inside the bar.
        let mut frame = vec![0u8; texture_width(scale) * texture_height(scale) * 4];
        let mut v = view(
            FrontPanelStatus {
                hdd_led: Some(false),
                cd_led: Some(false),
                ..FrontPanelStatus::default()
            },
            true,
            false,
        );
        v.media = media(1, Some(true));
        draw_status_bar(&mut frame, &v, scale);
        let cd = led_row_rect(3, 4);
        assert!(cd.y + cd.h <= PRESENT_HEIGHT + super::STATUS_BAR_HEIGHT);
        assert_eq!(
            pixel(&frame, cd.x + cd.w / 2, cd.y + cd.h / 2, scale),
            CD_LED_OFF.to_le_bytes()
        );

        // No CD drive: the CD row is absent and that area stays bar
        // background.
        let mut frame = vec![0u8; texture_width(scale) * texture_height(scale) * 4];
        draw_status_bar(
            &mut frame,
            &view(FrontPanelStatus::default(), true, false),
            scale,
        );
        let cd = led_row_rect(2, 3);
        assert_eq!(
            pixel(&frame, cd.x + cd.w / 2, cd.y + cd.h / 2, scale),
            STATUS_BG.to_le_bytes()
        );
    }

    #[test]
    fn bar_layout_stacks_three_or_more_drives_two_up() {
        // One or two drives sit in a single full-height row.
        let flat = bar_layout(&media(2, Some(true)));
        let df0 = flat.drive_load[0].unwrap();
        let df1 = flat.drive_load[1].unwrap();
        assert_eq!(df0.y, df1.y);
        assert_eq!(df0.h, super::STATUS_CONTROL_H);

        // Three or four drives stack in a two-column grid: DF2 sits
        // below DF0 in shorter buttons.
        let stacked = bar_layout(&media(4, Some(true)));
        let df0 = stacked.drive_load[0].unwrap();
        let df1 = stacked.drive_load[1].unwrap();
        let df2 = stacked.drive_load[2].unwrap();
        let df3 = stacked.drive_load[3].unwrap();
        assert_eq!(df0.y, df1.y);
        assert_eq!(df2.y, df3.y);
        assert_eq!(df0.x, df2.x);
        assert_eq!(df1.x, df3.x);
        assert!(df2.y >= df0.y + df0.h);
        assert!(df0.h < super::STATUS_CONTROL_H);
        // The grid clears the track counter on the left and the volume
        // control on the right, CD cluster included.
        assert!(df0.x >= fdd_track_counter_rect().x + fdd_track_counter_rect().w);
        let cd_eject = stacked.cd_eject.unwrap();
        assert!(cd_eject.x + cd_eject.w <= VOLUME_GLYPH_X);
        // Stacked buttons stay inside the status bar.
        assert!(df2.y + df2.h <= PRESENT_HEIGHT + super::STATUS_BAR_HEIGHT);
    }

    #[test]
    fn control_at_maps_media_and_screenshot_buttons() {
        let layout = bar_layout(&media(2, Some(false)));
        let centre = |r: super::Rect| ((r.x + r.w / 2) as i32, (r.y + r.h / 2) as i32);

        assert_eq!(
            control_at(centre(layout.drive_load[0].unwrap()), &layout),
            Some(BarControl::DriveLoad(0))
        );
        assert_eq!(
            control_at(centre(layout.drive_swap[1].unwrap()), &layout),
            Some(BarControl::DriveSwap(1))
        );
        assert_eq!(
            control_at(centre(layout.drive_eject[1].unwrap()), &layout),
            Some(BarControl::DriveEject(1))
        );
        assert_eq!(
            control_at(centre(layout.cd_load.unwrap()), &layout),
            Some(BarControl::CdLoad)
        );
        assert_eq!(
            control_at(centre(layout.cd_eject.unwrap()), &layout),
            Some(BarControl::CdEject)
        );
        assert_eq!(
            control_at(centre(shot_button_rect()), &layout),
            Some(BarControl::Screenshot)
        );
        assert_eq!(
            control_at(centre(pause_button_rect()), &layout),
            Some(BarControl::Pause)
        );
        // No drive 2 connected: the space right of its would-be cluster
        // is empty bar.
        assert_eq!(control_at((2, 2), &layout), None);
    }

    #[test]
    fn status_bar_draws_volume_control_and_maps_pointer_position() {
        let scale = 1;
        let mut frame = vec![0u8; texture_width(scale) * texture_height(scale) * 4];
        draw_status_bar(
            &mut frame,
            &view(
                FrontPanelStatus {
                    power_led_on: true,
                    fdd_led_on: true,
                    fdd_track: Some(5),
                    hdd_led: None,
                    cd_led: None,
                    output_volume_percent: 50,
                },
                true,
                false,
            ),
            scale,
        );

        let track = volume_slider_track_rect();
        assert_eq!(volume_percent_from_pos((track.x as i32, track.y as i32)), 0);
        assert_eq!(
            volume_percent_from_pos(((track.x + track.w - 1) as i32, track.y as i32)),
            100
        );
        assert_eq!(
            pixel(&frame, track.x + track.w / 4, track.y + track.h / 2, scale),
            VOLUME_FILL.to_le_bytes()
        );
    }

    #[test]
    fn status_bar_latches_fdd_track_when_no_drive_is_selected() {
        let mut last_fdd_track = None;
        let status = status_with_latched_fdd_track(
            FrontPanelStatus {
                power_led_on: true,
                fdd_led_on: true,
                fdd_track: Some(42),
                hdd_led: None,
                cd_led: None,
                output_volume_percent: 100,
            },
            &mut last_fdd_track,
        );
        assert_eq!(status.fdd_track, Some(42));
        assert_eq!(last_fdd_track, Some(42));

        let status = status_with_latched_fdd_track(
            FrontPanelStatus {
                power_led_on: true,
                fdd_led_on: false,
                fdd_track: None,
                hdd_led: None,
                cd_led: None,
                output_volume_percent: 100,
            },
            &mut last_fdd_track,
        );
        assert_eq!(status.fdd_track, Some(42));
    }

    #[test]
    fn status_bar_draws_at_hidpi_texture_scale() {
        let scale = 2;
        let mut frame = vec![0u8; texture_width(scale) * texture_height(scale) * 4];

        draw_status_bar(
            &mut frame,
            &view(
                FrontPanelStatus {
                    power_led_on: true,
                    fdd_led_on: true,
                    fdd_track: Some(159),
                    hdd_led: None,
                    cd_led: None,
                    output_volume_percent: 100,
                },
                true,
                true,
            ),
            scale,
        );

        let power = led_row_rect(0, 2);
        assert_eq!(
            pixel(
                &frame,
                (power.x + power.w / 2) * scale,
                (power.y + power.h / 2) * scale,
                scale
            ),
            POWER_LED_ON.to_le_bytes()
        );
        let ones = fdd_track_digit_rect(2);
        assert_eq!(
            pixel(
                &frame,
                (ones.x + ones.w / 2) * scale,
                (ones.y + ones.h / 2) * scale,
                scale
            ),
            TRACK_SEGMENT_ON.to_le_bytes()
        );
    }

    #[test]
    fn present_frame_copy_scales_texture_rows_at_hidpi() {
        use crate::video::deinterlace::{OUT_HEIGHT, OUT_PIXELS};
        let scale = 2;
        let mut src = vec![0u32; OUT_PIXELS];
        src[0] = 0x1122_3344;
        src[1] = 0x5566_7788;
        src[(OUT_HEIGHT - 1) * FB_WIDTH] = 0xAABB_CCDD;
        let mut frame = vec![0u8; texture_width(scale) * texture_height(scale) * 4];

        copy_present_frame(&src, OUT_HEIGHT, &mut frame, scale);

        // The top output row samples the top source row exactly (the
        // centre-aligned position clamps at the edge), and horizontal
        // duplication carries each source pixel across the HiDPI pair.
        assert_eq!(pixel(&frame, 0, 0, scale), src[0].to_le_bytes());
        assert_eq!(pixel(&frame, 1, 0, scale), src[0].to_le_bytes());
        assert_eq!(pixel(&frame, 2, 0, scale), src[1].to_le_bytes());
        // The bottom output row resolves to the last woven source line.
        assert_eq!(
            pixel(&frame, 0, PRESENT_HEIGHT * scale - 1, scale),
            src[(OUT_HEIGHT - 1) * FB_WIDTH].to_le_bytes()
        );
    }

    #[test]
    fn present_row_sampling_gives_every_source_line_equal_coverage() {
        use crate::video::deinterlace::OUT_HEIGHT;
        // With nearest-neighbour subsampling, 33 of the 570 woven source
        // rows received zero presentation coverage and the rest uneven
        // amounts (visibly uneven crosshatch/dot test patterns). The
        // bilinear sampling must spread coverage evenly: accumulate each
        // source row's blend weight over all HiDPI output rows.
        let out_rows = PRESENT_HEIGHT * 2;
        let mut weight = vec![0f64; OUT_HEIGHT];
        for y in 0..out_rows {
            let (src_y0, frac) = present_row_sample(y, OUT_HEIGHT, out_rows);
            assert!(src_y0 < OUT_HEIGHT);
            assert!(frac < 256);
            weight[src_y0] += (256 - frac) as f64 / 256.0;
            if frac > 0 {
                weight[(src_y0 + 1).min(OUT_HEIGHT - 1)] += frac as f64 / 256.0;
            }
        }
        let expected = out_rows as f64 / OUT_HEIGHT as f64;
        for (y, w) in weight.iter().enumerate() {
            assert!(
                *w > 0.0,
                "source row {y} dropped from the presentation entirely"
            );
            // Edge rows are clamped toward the border; interior rows must
            // sit close to the uniform coverage expectation.
            if y > 0 && y + 1 < OUT_HEIGHT {
                assert!(
                    (*w - expected).abs() < 0.51,
                    "source row {y} coverage {w} deviates from uniform {expected}"
                );
            }
        }
    }

    #[test]
    fn standard_pal_frames_get_vertical_presentation_margin() {
        let standard_offset = presentation_source_y_offset(STANDARD_PAL_VISIBLE_START_VPOS);

        assert!(standard_offset > 0);
        assert_eq!(
            presentation_source_y_offset(STANDARD_PAL_VISIBLE_START_VPOS - standard_offset as u32),
            0
        );
    }

    #[test]
    fn horizontal_centering_shifts_left_and_blacks_the_right() {
        let mut fb = vec![rgba(0, 0, 0); FB_PIXELS];
        let marker = rgba(0x12, 0x34, 0x56);
        // A marker 30px in on the first row, and one at the right edge.
        fb[30] = marker;
        fb[FB_WIDTH - 1] = rgba(0x99, 0x88, 0x77);

        center_present_frame_horizontally(&mut fb, 26);

        // Content moved left by 26: x=30 -> x=4.
        assert_eq!(fb[4], marker);
        assert_eq!(fb[30], rgba(0, 0, 0));
        // The right 26 columns are blacked out.
        for x in (FB_WIDTH - 26)..FB_WIDTH {
            assert_eq!(fb[x], rgba(0, 0, 0));
        }
    }

    #[test]
    fn horizontal_centering_is_a_noop_for_zero_shift() {
        let mut fb = vec![rgba(0, 0, 0); FB_PIXELS];
        let marker = rgba(1, 2, 3);
        fb[100] = marker;
        center_present_frame_horizontally(&mut fb, 0);
        assert_eq!(fb[100], marker);
    }

    #[test]
    fn tv_horizontal_centering_centers_the_tv_aperture() {
        let shift = tv_standard_h_shift();
        let (source_left, source_right) = tv_source_h_bounds();
        let source_width = source_right - source_left;
        let centered_left = (FB_WIDTH - source_width) / 2;
        let centered_right = centered_left + source_width;

        assert_eq!(source_left - shift, centered_left);
        assert_eq!(source_right - shift, centered_right);
    }

    #[test]
    fn standard_pal_frame_centering_preserves_horizontal_margin() {
        let offset = presentation_source_y_offset(STANDARD_PAL_VISIBLE_START_VPOS);
        let mut fb = vec![rgba(0, 0, 0); FB_PIXELS];
        let marker = rgba(0x12, 0x34, 0x56);
        fb[32] = marker;

        center_present_frame_for_visible_start(&mut fb, STANDARD_PAL_VISIBLE_START_VPOS);

        assert_eq!(fb[0], rgba(0, 0, 0));
        assert_eq!(fb[(offset * FB_WIDTH) + 31], rgba(0, 0, 0));
        assert_eq!(fb[(offset * FB_WIDTH) + 32], marker);
    }

    #[test]
    fn tv_overscan_mask_blacks_margins_and_keeps_the_tv_window() {
        let marker = rgba(0x12, 0x34, 0x56);
        let mut fb = vec![marker; FB_PIXELS];

        // A standard screen centred by the presentation: window top at
        // framebuffer row 14 (the standard centring offset), with the TV
        // aperture centred horizontally.
        let std_top = standard_window_top_row(STANDARD_PAL_VISIBLE_START_VPOS);
        let shift = tv_standard_h_shift();
        mask_present_frame_to_tv(&mut fb, shift, std_top);

        let (source_left, source_right) = tv_source_h_bounds();
        let left = source_left - shift;
        let right = source_right - shift;
        let mid_row = std_top + 100;
        assert_eq!(fb[mid_row * FB_WIDTH + left - 1], rgba(0, 0, 0));
        assert_eq!(fb[mid_row * FB_WIDTH + left], marker);
        assert_eq!(fb[mid_row * FB_WIDTH + right - 1], marker);
        assert_eq!(fb[mid_row * FB_WIDTH + right], rgba(0, 0, 0));
        assert_eq!(fb[mid_row * FB_WIDTH + FB_WIDTH - 1], rgba(0, 0, 0));
        // The deep left overscan margin stays hidden.
        assert_eq!(fb[mid_row * FB_WIDTH], rgba(0, 0, 0));
        // The vertical TV window tracks the centred standard window: 8
        // lines of top overscan remain visible, while the bottom is cropped
        // one source row inside the standard window like a tight lower bezel.
        let top = std_top - 8;
        assert_eq!(fb[(top - 1) * FB_WIDTH + left], rgba(0, 0, 0));
        assert_eq!(fb[top * FB_WIDTH + left], marker);
        let bottom = std_top + STANDARD_PAL_VISIBLE_LINES - 1;
        assert_eq!(fb[(bottom - 1) * FB_WIDTH + left], marker);
        assert_eq!(fb[bottom * FB_WIDTH + left], rgba(0, 0, 0));
    }

    #[test]
    fn tv_horizontal_centering_preserves_visible_left_overscan_margin() {
        let std_top = standard_window_top_row(STANDARD_PAL_VISIBLE_START_VPOS);
        let mid_row = std_top + 100;
        let (source_left, _) = tv_source_h_bounds();
        let shift = tv_standard_h_shift();
        let marker = rgba(0x12, 0x34, 0x56);
        let hidden_marker = rgba(0x98, 0x76, 0x54);
        let mut fb = vec![rgba(0, 0, 0); FB_PIXELS];

        fb[mid_row * FB_WIDTH + source_left - 1] = hidden_marker;
        fb[mid_row * FB_WIDTH + source_left] = marker;

        center_present_frame_horizontally(&mut fb, shift);
        mask_present_frame_to_tv(&mut fb, shift, std_top);

        assert_eq!(
            fb[mid_row * FB_WIDTH + source_left - shift - 1],
            rgba(0, 0, 0)
        );
        assert_eq!(fb[mid_row * FB_WIDTH + source_left - shift], marker);
    }

    #[test]
    fn tv_overscan_mask_tracks_the_centering_shift() {
        let marker = rgba(0x12, 0x34, 0x56);
        let mut fb = vec![marker; FB_PIXELS];

        // A standard display shifted left 16px for centring: the bezel
        // moves with it, so the window's left edge is not clipped.
        let std_top = standard_window_top_row(STANDARD_PAL_VISIBLE_START_VPOS);
        mask_present_frame_to_tv(&mut fb, 16, std_top);

        let (source_left, source_right) = tv_source_h_bounds();
        let left = source_left - 16;
        let right = source_right - 16;
        let mid_row = std_top + 100;
        assert_eq!(fb[mid_row * FB_WIDTH + left - 1], rgba(0, 0, 0));
        assert_eq!(fb[mid_row * FB_WIDTH + left], marker);
        assert_eq!(fb[mid_row * FB_WIDTH + right - 1], marker);
        assert_eq!(fb[mid_row * FB_WIDTH + right], rgba(0, 0, 0));
        assert_eq!(fb[mid_row * FB_WIDTH + FB_WIDTH - 1], rgba(0, 0, 0));
    }

    #[test]
    fn tv_overscan_mask_tracks_overscan_visible_starts() {
        // A deep-overscan frame (visible start above the standard window):
        // the centring shift is consumed, the standard window sits lower in
        // the framebuffer, and the TV window follows it down.
        let visible_start = STANDARD_PAL_VISIBLE_START_VPOS - 16;
        let std_top = standard_window_top_row(visible_start);
        assert_eq!(std_top, 16);

        let marker = rgba(0x12, 0x34, 0x56);
        let mut fb = vec![marker; FB_PIXELS];
        mask_present_frame_to_tv(&mut fb, 0, std_top);

        let (left, _) = tv_source_h_bounds();
        assert_eq!(fb[(std_top - 8 - 1) * FB_WIDTH + left], rgba(0, 0, 0));
        assert_eq!(fb[(std_top - 8) * FB_WIDTH + left], marker);
        let bottom = std_top + STANDARD_PAL_VISIBLE_LINES - 1;
        assert_eq!(fb[(bottom - 1) * FB_WIDTH + left], marker);
        if bottom < FB_HEIGHT {
            assert_eq!(fb[bottom * FB_WIDTH + left], rgba(0, 0, 0));
        }
    }

    #[test]
    fn reboot_button_hit_rect_is_bounded_to_status_bar_button() {
        let button = reboot_button_rect();

        assert!(button.contains(((button.x + 1) as i32, (button.y + 1) as i32)));
        assert!(button.contains((
            (button.x + button.w - 1) as i32,
            (button.y + button.h - 1) as i32
        )));
        assert!(!button.contains(((button.x - 1) as i32, button.y as i32)));
        assert!(!button.contains((button.x as i32, (button.y - 1) as i32)));
    }

    fn pixel(frame: &[u8], x: usize, y: usize, scale: usize) -> [u8; 4] {
        frame[(y * texture_width(scale) + x) * 4..(y * texture_width(scale) + x) * 4 + 4]
            .try_into()
            .unwrap()
    }

    /// An interactive App around a minimal machine: a NOP-sled ROM with
    /// reset vectors pointing into it, no audio, unpaced. Lets the
    /// debugger window's actions and view builders run against the real
    /// emulator without a host window.
    fn test_app() -> super::App {
        test_app_with_audio(Box::new(NullSink))
    }

    fn test_app_with_audio(audio: Box<dyn AudioSink>) -> super::App {
        use crate::chipset::paula::Paula;
        use crate::config::{CpuModel, PacingBudget};
        use crate::emulator::Emulator;
        use crate::floppy::FloppyController;
        use crate::memory::{Memory, ROM_BASE, ROM_SIZE};
        use crate::serial::StdoutSink;

        let mut rom = vec![0u8; ROM_SIZE];
        let pc = ROM_BASE as u32 + 8;
        rom[0..4].copy_from_slice(&0x0007_FFFEu32.to_be_bytes());
        rom[4..8].copy_from_slice(&pc.to_be_bytes());
        // NOP sled for the rest of the test program.
        for word in rom[8..4096].chunks_exact_mut(2) {
            word.copy_from_slice(&0x4E71u16.to_be_bytes());
        }
        let mem = Memory {
            chip_ram: vec![0u8; 512 * 1024],
            slow_ram: Vec::new(),
            rom,
            overlay: true,
            zorro: crate::zorro::ZorroChain::default(),
            extended_rom: Vec::new(),
            extended_rom_base: 0,
            wcs: Vec::new(),
            wcs_write_protected: false,
        };
        let bus = crate::bus::Bus::new(
            mem,
            Paula::new(Box::new(StdoutSink::new()), audio),
            FloppyController::default(),
        );
        let emu = Emulator::new(bus, CpuModel::M68000, false, PacingBudget::Cycles, 2, false)
            .expect("test emulator");
        super::App::new(
            emu,
            true,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            None,
            std::array::from_fn(|_| Vec::new()),
            [true; 4],
            crate::config::Overscan::Full,
            0.0,
            crate::config::WarpSpeed::Max,
            crate::config::JoystickInputMode::Gamepad,
            vec!["Machine: test".to_string()],
            crate::config::RawConfig::default(),
        )
    }

    #[test]
    fn launcher_panel_edits_machine_setup() {
        use crate::config::MachineModel;
        use crate::video::launcher::{LauncherField, LauncherTab};

        let mut app = test_app();
        app.open_launcher();
        assert!(matches!(app.ui.panel, Some(Panel::Launcher(_))));

        // Pick a machine, switch tabs, and flip a toggle through the same
        // control dispatch the mouse uses.
        app.activate_ui_control(UiControl::LauncherModel(MachineModel::A1200));
        app.activate_ui_control(UiControl::LauncherTab(LauncherTab::Cpu));
        app.activate_ui_control(UiControl::LauncherToggle(LauncherField::Fpu));

        let state = match &app.ui.panel {
            Some(Panel::Launcher(state)) => state,
            _ => panic!("launcher closed unexpectedly"),
        };
        assert_eq!(state.setup.model(), Some(MachineModel::A1200));
        assert_eq!(state.tab, LauncherTab::Cpu);
        // The A1200's profile defaults (AGA, EC020, 2M chip) plus the FPU we
        // toggled on are what a save would emit.
        let raw = state.setup.to_raw();
        assert_eq!(raw.machine.profile.as_deref(), Some("A1200"));
        assert_eq!(raw.cpu.fpu, Some(true));
        assert!(state.setup.build_config().is_ok());
    }

    #[test]
    fn launcher_run_keeps_panel_open_on_error() {
        use crate::video::launcher::LauncherField;

        let mut app = test_app();
        app.powered_on = false;
        app.open_launcher();
        // A floppy image that does not exist fails config validation.
        if let Some(Panel::Launcher(state)) = app.ui.panel.as_mut() {
            state
                .setup
                .set_path(LauncherField::Df0Image, PathBuf::from("/no/such/disk.adf"));
        }
        app.launcher_run();
        match &app.ui.panel {
            Some(Panel::Launcher(state)) => assert!(
                state.status.as_ref().is_some_and(|s| s.error),
                "expected an error status to keep the user in the launcher"
            ),
            _ => panic!("launcher should stay open on a validation error"),
        }
        assert!(
            !app.powered_on,
            "a failed Run must not power the machine on"
        );
    }

    #[test]
    fn state_load_closes_launcher_and_powers_restored_machine() {
        let path = std::env::temp_dir().join(format!(
            "copperline-launcher-state-load-{}.clstate",
            std::process::id()
        ));
        let mut app = test_app();
        app.emu.save_state(&path).expect("save test state");

        app.power_off();
        let parked_present = app.present_fb.clone();
        app.open_launcher();
        assert!(matches!(app.ui.panel, Some(Panel::Launcher(_))));

        assert!(app.load_state_from_path(&path));
        assert!(app.ui.panel.is_none(), "state load should dismiss launcher");
        assert!(app.powered_on);
        assert!(!app.cpu_halted);
        assert!(
            app.present_fb == parked_present,
            "load itself should not invent a rendered frame"
        );

        for _ in 0..3 {
            app.emu.step_frame().expect("step restored frame");
            if app.finish_render_for_current_frame() {
                break;
            }
        }
        assert_ne!(
            app.present_fb, parked_present,
            "restored machine should render over the parked test screen"
        );

        let _ = std::fs::remove_file(&path);
    }

    struct SuspensionSink {
        states: Rc<RefCell<Vec<bool>>>,
    }

    impl AudioSink for SuspensionSink {
        fn push(&mut self, _left: f32, _right: f32) {}

        fn flush(&mut self) {}

        fn set_live_output_suspended(&mut self, suspended: bool) {
            self.states.borrow_mut().push(suspended);
        }
    }

    #[test]
    fn host_pause_states_suspend_live_audio_output() {
        let states = Rc::new(RefCell::new(Vec::new()));
        let mut app = test_app_with_audio(Box::new(SuspensionSink {
            states: Rc::clone(&states),
        }));

        app.toggle_pause();
        assert_eq!(states.borrow().last(), Some(&true));
        app.toggle_pause();
        assert_eq!(states.borrow().last(), Some(&false));

        app.power_off();
        assert_eq!(states.borrow().last(), Some(&true));
        app.toggle_power();
        assert_eq!(states.borrow().last(), Some(&false));

        app.open_debugger();
        assert_eq!(states.borrow().last(), Some(&true));
        app.debugger_toggle_run();
        assert_eq!(states.borrow().last(), Some(&false));
    }

    #[test]
    fn host_io_audio_suspension_restores_current_run_state() {
        let states = Rc::new(RefCell::new(Vec::new()));
        let mut app = test_app_with_audio(Box::new(SuspensionSink {
            states: Rc::clone(&states),
        }));

        app.suspend_live_audio_for_host_io();
        app.finish_host_io_pause();
        assert_eq!(states.borrow().as_slice(), &[true, false]);

        app.toggle_pause();
        app.suspend_live_audio_for_host_io();
        app.finish_host_io_pause();
        assert_eq!(states.borrow().last(), Some(&true));
    }

    #[test]
    fn restoring_over_placeholder_detected_only_for_the_silent_config_screen() {
        // The exact configuration-screen placeholder: powered off, launcher
        // open, NullSink installed (as build_placeholder_machine produces).
        let mut app = test_app();
        app.power_off();
        app.open_launcher();
        assert!(
            app.restoring_over_placeholder(),
            "powered-off launcher with a null sink is the placeholder"
        );

        // A live (non-null) sink behind the launcher is a real running session
        // re-opening the config screen: its audio must not be torn out.
        let states = Rc::new(RefCell::new(Vec::new()));
        let mut live = test_app_with_audio(Box::new(SuspensionSink {
            states: Rc::clone(&states),
        }));
        live.power_off();
        live.open_launcher();
        assert!(
            !live.restoring_over_placeholder(),
            "a real audio sink behind the launcher is not the placeholder"
        );

        // A null sink but powered on, or with no launcher open, is not the
        // pre-boot placeholder either.
        let mut powered = test_app();
        powered.open_launcher();
        assert!(powered.powered_on);
        assert!(!powered.restoring_over_placeholder());

        let mut no_launcher = test_app();
        no_launcher.power_off();
        assert!(no_launcher.ui.panel.is_none());
        assert!(!no_launcher.restoring_over_placeholder());
    }

    #[test]
    fn state_load_over_running_session_keeps_its_live_audio_sink() {
        // Re-opening the config screen over a running machine and loading a
        // state must not replace the live audio sink (the regression guard for
        // the placeholder-upgrade path). Uses a probe sink so no real audio
        // device is touched.
        let path = std::env::temp_dir().join(format!(
            "copperline-running-state-load-{}.clstate",
            std::process::id()
        ));
        let states = Rc::new(RefCell::new(Vec::new()));
        let mut app = test_app_with_audio(Box::new(SuspensionSink {
            states: Rc::clone(&states),
        }));
        app.emu.save_state(&path).expect("save test state");
        app.open_launcher();
        assert!(app.powered_on);
        assert!(!app.restoring_over_placeholder());

        assert!(app.load_state_from_path(&path));

        // The probe sink is still the installed one: a suspension toggle still
        // reaches it. (A replacement CpalSink would have dropped the probe.)
        states.borrow_mut().clear();
        app.suspend_live_audio_for_host_io();
        app.finish_host_io_pause();
        assert_eq!(
            states.borrow().as_slice(),
            &[true, false],
            "live audio sink should survive a state load over a running session"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// End-to-end recording through the app: start, run emulated frames
    /// through the same render/capture path as the event loop, stop, and
    /// check the resulting AVI carries the frames and matching audio.
    /// COPPERLINE_RECORDER_KEEP=1 keeps the file for playback checks.
    #[test]
    fn recording_captures_emulated_frames_with_audio() {
        let mut app = test_app();
        let path = std::env::temp_dir().join(format!(
            "copperline-app-recording-{}.avi",
            std::process::id()
        ));
        for warmup_step in 0..4 {
            app.emu.step_frame().expect("step frame");
            let rendered = if app.render_worker.is_some() {
                app.finish_render_for_current_frame()
            } else {
                app.render_emulated_frame_if_needed()
            };
            if rendered {
                break;
            }
            assert!(
                warmup_step < 3,
                "fixture should produce an initial renderable frame"
            );
        }

        app.start_recording_to(path.clone());
        assert!(app.recorder.is_some(), "recorder should be active");

        let frames_to_record = 5;
        let mut rendered_frames = 0;
        let mut step_quanta = 0;
        while rendered_frames < frames_to_record {
            app.emu.step_frame().expect("step frame");
            let rendered = if app.render_worker.is_some() {
                app.finish_render_for_current_frame()
            } else {
                app.render_emulated_frame_if_needed()
            };
            app.capture_recorder_output(rendered);
            if rendered {
                rendered_frames += 1;
            }
            step_quanta += 1;
            assert!(
                step_quanta <= frames_to_record * 2,
                "fixture should keep producing renderable frames"
            );
        }
        app.stop_recording();
        assert!(app.recorder.is_none());
        // Stopping again is a no-op, and Paula's tap is off.
        app.stop_recording();
        assert!(app.emu.bus_mut().paula.take_captured_audio().is_empty());

        let data = std::fs::read(&path).expect("recording file exists");
        if crate::envcfg::flag("COPPERLINE_RECORDER_KEEP") {
            eprintln!("kept {}", path.display());
        } else {
            std::fs::remove_file(&path).unwrap();
        }
        assert_eq!(&data[0..4], b"RIFF");
        assert_eq!(&data[8..12], b"AVI ");
        assert_eq!(&data[112..116], b"ZMBV");
        // avih dwTotalFrames at offset 48 (see recorder::build_header).
        let frames = u32::from_le_bytes(data[48..52].try_into().unwrap());
        assert_eq!(frames, frames_to_record);
        // Audio stream length (samples) at offset 264 should cover the
        // same emulated interval: ~882 mixer samples per PAL frame or
        // ~735 per NTSC frame (the fixture machine's standard).
        let audio_len = u32::from_le_bytes(data[264..268].try_into().unwrap());
        let per_frame = audio_len as f64 / frames_to_record as f64;
        assert!(
            (700.0..=920.0).contains(&per_frame),
            "audio samples per frame {per_frame}"
        );
    }

    #[test]
    fn debugger_window_pauses_steps_and_restores_run_state() {
        let mut app = test_app();
        assert!(!app.paused);

        // Opening pauses; the memory view starts at the PC's page.
        app.toggle_debugger();
        assert!(app.paused);
        let pc_before = app.emu.machine.pc();
        match app.debugger_panel.as_ref() {
            Some(panel) => {
                assert_eq!(panel.mem_addr, pc_before & 0x00FF_FFF0);
            }
            _ => panic!("debugger panel should be open"),
        }

        // Step executes exactly one instruction (a 2-byte NOP).
        app.debugger_step();
        assert_eq!(app.emu.machine.pc(), pc_before.wrapping_add(2));

        // Run to a nearby address lands exactly there.
        let target = pc_before.wrapping_add(10);
        if let Some(panel) = app.debugger_panel.as_mut() {
            panel.entry = format!("{target:X}");
        }
        app.debugger_run_to();
        assert_eq!(app.emu.machine.pc() & 0x00FF_FFFF, target & 0x00FF_FFFF);

        // Step Frame advances emulated time by one whole frame.
        let frame_before = app.emu.bus().emulated_frames();
        app.debugger_step_frame();
        assert!(app.emu.bus().emulated_frames() > frame_before);

        // Closing restores the pre-debugger (running) state.
        app.toggle_debugger();
        assert!(app.debugger_panel.is_none());
        assert!(!app.paused);

        // Run pressed inside the debugger survives closing it.
        app.toggle_debugger();
        assert!(app.paused);
        app.debugger_toggle_run();
        assert!(!app.paused);
        app.close_tool_panel(ToolPanelKind::Debugger);
        assert!(!app.paused);
    }

    #[test]
    fn debugger_and_frame_analyzer_can_stay_open_together() {
        let mut app = test_app();
        assert!(!app.paused);

        app.open_debugger();
        assert!(app.paused);
        assert!(app.debugger_panel.is_some());
        assert!(app.frame_analyzer_panel.is_none());

        app.open_frame_analyzer();
        assert!(app.paused);
        assert!(app.debugger_panel.is_some());
        assert!(app.frame_analyzer_panel.is_some());

        app.close_tool_panel(ToolPanelKind::FrameAnalyzer);
        assert!(app.paused, "debugger should keep the machine paused");
        assert!(app.debugger_panel.is_some());
        assert!(app.frame_analyzer_panel.is_none());

        app.close_tool_panel(ToolPanelKind::Debugger);
        assert!(!app.paused);
        assert!(app.debugger_panel.is_none());

        let mut app = test_app();
        app.open_frame_analyzer();
        assert!(app.paused);
        app.open_debugger();
        assert!(app.debugger_panel.is_some());
        assert!(app.frame_analyzer_panel.is_some());

        app.close_tool_panel(ToolPanelKind::Debugger);
        assert!(app.paused, "analyzer should keep the machine paused");
        assert!(app.debugger_panel.is_none());
        assert!(app.frame_analyzer_panel.is_some());

        app.close_tool_panel(ToolPanelKind::FrameAnalyzer);
        assert!(!app.paused);
        assert!(app.frame_analyzer_panel.is_none());
    }

    #[test]
    fn debugger_views_reflect_machine_state() {
        let mut app = test_app();
        app.open_debugger();

        let pc = app.emu.machine.pc();
        for tab in super::ui::DEBUG_TABS {
            if let Some(panel) = app.debugger_panel.as_mut() {
                panel.tab = tab;
            }
            let Some(panel) = app.debugger_panel.as_ref() else {
                unreachable!()
            };
            let view = app.build_debugger_view(panel);
            assert!(!view.lines.is_empty());
            match tab {
                super::ui::DebugTab::Cpu => {
                    assert!(view.lines[0].text.contains(&format!("PC {pc:08X}")));
                    // The disassembly cursor line is highlighted and decodes
                    // the NOP sled.
                    let cursor = view
                        .lines
                        .iter()
                        .find(|line| line.highlight)
                        .expect("a highlighted PC line");
                    assert!(cursor.text.contains("NOP"), "{}", cursor.text);
                }
                super::ui::DebugTab::Chipset => {
                    assert!(view.lines.iter().any(|l| l.text.starts_with("DMACON")));
                    assert!(view.lines.iter().any(|l| l.text.starts_with("INTENA")));
                    assert!(view.lines.iter().any(|l| l.text.contains("COLOR00")));
                }
                super::ui::DebugTab::Copper => {
                    assert!(view.lines[0].text.contains("COP1LC"));
                }
                super::ui::DebugTab::Memory => {
                    // The hex dump shows the NOP sled at the PC's ROM page.
                    assert!(view.lines.iter().any(|l| l.text.contains("4E 71")));
                }
                super::ui::DebugTab::Break => {
                    assert!(view.lines.iter().any(|l| l.text == "Breakpoints:"));
                    assert!(view.lines.iter().any(|l| l.text == "  (none)"));
                }
            }
        }
    }

    #[test]
    fn interactive_breakpoint_pauses_and_reopens_the_debugger() {
        let mut app = test_app();
        app.open_debugger();

        // Toggle a breakpoint a few instructions ahead via the entry box.
        let target = app.emu.machine.pc().wrapping_add(8) & 0x00FF_FFFF;
        if let Some(panel) = app.debugger_panel.as_mut() {
            panel.tab = super::ui::DebugTab::Break;
            panel.entry = format!("{target:X}");
        }
        app.activate_ui_control(super::ui::UiControl::DebugBreakToggle);
        assert!(app.emu.machine.ui_breaks().is_breakpoint(target));

        // The Break tab lists it.
        if let Some(panel) = app.debugger_panel.as_ref() {
            let view = app.build_debugger_view(panel);
            assert!(view
                .lines
                .iter()
                .any(|l| l.text.contains(&format!("${target:06X}"))));
        }

        // Close the panel (machine resumes) and run a frame: the hit
        // pauses the machine at the breakpoint, before it executes.
        app.close_tool_panel(ToolPanelKind::Debugger);
        assert!(!app.paused);
        app.emu.step_frame().expect("frame");
        assert!(app.surface_debug_stop());
        assert!(app.paused);
        assert_eq!(app.emu.machine.pc() & 0x00FF_FFFF, target);
        assert!(app.debugger_panel.is_some());
        assert!(app
            .last_debug_stop
            .as_deref()
            .is_some_and(|s| s.contains("Breakpoint")));

        // Resuming does not immediately re-trip the same breakpoint.
        app.debugger_toggle_run();
        assert!(app.last_debug_stop.is_none());
        app.emu.step_frame().expect("frame");
        assert_ne!(app.emu.machine.pc() & 0x00FF_FFFF, target);

        // Toggling the same address again removes the breakpoint.
        if let Some(panel) = app.debugger_panel.as_mut() {
            panel.entry = format!("{target:X}");
        }
        app.activate_ui_control(super::ui::UiControl::DebugBreakToggle);
        assert!(!app.emu.machine.ui_breaks().is_breakpoint(target));
    }

    #[test]
    fn opening_the_debugger_arms_reverse_and_step_reconstructs() {
        let mut app = test_app();
        // Opening the debugger auto-arms the reverse snapshot ring.
        app.open_debugger();
        assert!(app.emu.time_travel_enabled());
        if let Some(panel) = app.debugger_panel.as_ref() {
            assert!(
                app.build_debugger_view(panel).reverse_available,
                "reverse controls should be enabled once armed"
            );
        }

        // Advance enough frames that several snapshots accrue.
        for _ in 0..16 {
            app.debugger_step_frame();
        }
        let pos_before = app.emu.retired_instructions();
        let pc_before = app.emu.machine.pc();
        assert!(pos_before > 0, "the NOP sled retired instructions");

        // Reverse Step moves the position strictly backward.
        app.activate_ui_control(super::ui::UiControl::DebugReverseStep);
        let pos_after = app.emu.retired_instructions();
        assert_eq!(pos_after, pos_before - 1, "stepped back exactly one");

        // Replaying forward to the original position reconstructs the PC.
        for _ in 0..16 {
            app.debugger_step_frame();
        }
        assert!(app.emu.retired_instructions() >= pos_before);
        let frame_before = app.emu.bus().emulated_frames();
        let pos_before_frame = app.emu.retired_instructions();
        assert!(frame_before > 0, "frame history should have advanced");

        // Reverse Frame moves to the previous Agnus frame counter value.
        app.activate_ui_control(super::ui::UiControl::DebugReverseFrame);
        assert_eq!(
            app.emu.bus().emulated_frames(),
            frame_before - 1,
            "stepped back exactly one emulated video frame"
        );
        assert!(
            app.emu.retired_instructions() < pos_before_frame,
            "reverse frame should move to an earlier instruction boundary"
        );

        // And reverse-continue with no breakpoints is a no-op (reports, does
        // not move): position is unchanged afterward.
        let pos = app.emu.retired_instructions();
        app.activate_ui_control(super::ui::UiControl::DebugReverseRun);
        assert_eq!(app.emu.retired_instructions(), pos);
        let _ = pc_before;
    }

    #[test]
    fn interactive_watchpoint_stops_when_the_word_changes() {
        let mut app = test_app();
        // Map chip RAM at $0 so the watched word is real memory.
        app.emu.machine.disable_overlay();
        let addr = 0x0000_1000u32;
        assert!(app.emu.machine.ui_toggle_watch(addr));

        // Unchanged memory: a full frame runs without stopping.
        app.emu.step_frame().expect("frame");
        assert!(!app.surface_debug_stop());

        // Change the watched word (as any non-CPU bus master would); the
        // next executed instruction notices and stops the machine.
        app.emu.bus_mut().mem.chip_ram[addr as usize] = 0xAB;
        app.emu.step_frame().expect("frame");
        assert!(app.surface_debug_stop());
        assert!(app.paused);
        assert!(app
            .last_debug_stop
            .as_deref()
            .is_some_and(|s| s.contains("Watch $001000")));
    }

    #[test]
    fn chipset_register_watch_stops_on_a_cpu_write() {
        let mut app = test_app();
        // Replace part of the NOP sled with MOVE.W #$8020,$DFF096
        // (DMACON), a few instructions ahead of the PC so the already
        // prefetched words are not affected.
        let pc = app.emu.machine.pc();
        let off = (pc as usize & 0x7FFFF) + 8;
        let mov: [u16; 4] = [0x33FC, 0x8020, 0x00DF, 0xF096];
        for (k, word) in mov.iter().enumerate() {
            app.emu.bus_mut().mem.rom[off + k * 2..off + k * 2 + 2]
                .copy_from_slice(&word.to_be_bytes());
        }

        // Watch DMACON via the entry box, accepting the full address form.
        app.open_debugger();
        if let Some(panel) = app.debugger_panel.as_mut() {
            panel.tab = super::ui::DebugTab::Break;
            panel.entry = "DFF096".to_string();
        }
        app.activate_ui_control(super::ui::UiControl::DebugRegToggle);
        assert_eq!(app.emu.machine.ui_breaks().reg_watches, [0x096]);
        app.debugger_toggle_run();
        app.close_tool_panel(ToolPanelKind::Debugger);

        app.emu.step_frame().expect("frame");
        assert!(app.surface_debug_stop());
        assert!(app.paused);
        let stop = app.last_debug_stop.as_deref().unwrap();
        assert!(stop.contains("DMACON"), "{stop}");
        assert!(stop.contains("8020"), "{stop}");
        assert!(stop.contains("cpu write"), "{stop}");
    }

    #[test]
    fn modal_panel_swallows_amiga_key_presses() {
        let mut app = test_app();
        app.ui.panel = Some(Panel::About);

        // Escape closes the panel.
        assert!(app.ui_handle_key(KeyCode::Escape));
        assert!(app.ui.panel.is_none());

        // Hex entry: digits accumulate, Enter commits to the memory view.
        app.open_debugger();
        if let Some(panel) = app.debugger_panel.as_mut() {
            panel.entry_active = true;
            panel.tab = super::ui::DebugTab::Memory;
        }
        for key in [
            KeyCode::KeyC,
            KeyCode::Digit0,
            KeyCode::Digit0,
            KeyCode::Digit1,
        ] {
            assert!(app.ui_handle_key(key));
        }
        assert!(app.ui_handle_key(KeyCode::Enter));
        match app.debugger_panel.as_ref() {
            Some(panel) => {
                assert_eq!(panel.entry, "C001");
                assert_eq!(panel.mem_addr, 0xC000);
                assert!(!panel.entry_active);
            }
            _ => panic!("debugger panel should be open"),
        }
    }

    #[test]
    fn frame_analyzer_cursor_keys_move_selected_slot() {
        let mut app = test_app();
        app.open_frame_analyzer();
        app.frame_analyzer_step_frame();
        assert!(app.emu.bus().frame_bus_trace().is_some());
        assert!(app.ui_key_accepts_repeat(Some(ToolPanelKind::FrameAnalyzer), KeyCode::ArrowRight));
        assert!(!app.ui_key_accepts_repeat(Some(ToolPanelKind::FrameAnalyzer), KeyCode::KeyR));

        let (start_hpos, start_vpos) = match app.frame_analyzer_panel.as_ref() {
            Some(panel) => (panel.selected_hpos, panel.selected_vpos),
            _ => panic!("frame analyzer panel should be open"),
        };

        assert!(app.ui_handle_key(KeyCode::ArrowRight));
        assert!(app.ui_handle_key(KeyCode::ArrowDown));
        match app.frame_analyzer_panel.as_ref() {
            Some(panel) => {
                assert_eq!(panel.selected_hpos, start_hpos + 1);
                assert_eq!(panel.selected_vpos, start_vpos + 1);
            }
            _ => panic!("frame analyzer panel should be open"),
        }

        assert!(app.ui_handle_key(KeyCode::ArrowLeft));
        assert!(app.ui_handle_key(KeyCode::ArrowUp));
        match app.frame_analyzer_panel.as_ref() {
            Some(panel) => {
                assert_eq!(panel.selected_hpos, start_hpos);
                assert_eq!(panel.selected_vpos, start_vpos);
            }
            _ => panic!("frame analyzer panel should be open"),
        }

        if let Some(panel) = app.frame_analyzer_panel.as_mut() {
            panel.selected_hpos = 0;
            panel.selected_vpos = 0;
        }
        assert!(app.ui_handle_key(KeyCode::ArrowLeft));
        assert!(app.ui_handle_key(KeyCode::ArrowUp));
        match app.frame_analyzer_panel.as_ref() {
            Some(panel) => {
                assert_eq!(panel.selected_hpos, 0);
                assert_eq!(panel.selected_vpos, 0);
            }
            _ => panic!("frame analyzer panel should be open"),
        }

        let (max_hpos, max_vpos) = app
            .emu
            .bus()
            .frame_bus_trace()
            .map(|trace| {
                (
                    trace.cols.saturating_sub(1).min(u16::MAX as usize) as u16,
                    trace.rows.saturating_sub(1).min(u16::MAX as usize) as u16,
                )
            })
            .unwrap();
        if let Some(panel) = app.frame_analyzer_panel.as_mut() {
            panel.selected_hpos = max_hpos;
            panel.selected_vpos = max_vpos;
        }
        assert!(app.ui_handle_key(KeyCode::ArrowRight));
        assert!(app.ui_handle_key(KeyCode::ArrowDown));
        match app.frame_analyzer_panel.as_ref() {
            Some(panel) => {
                assert_eq!(panel.selected_hpos, max_hpos);
                assert_eq!(panel.selected_vpos, max_vpos);
            }
            _ => panic!("frame analyzer panel should be open"),
        }
    }

    #[test]
    fn debugger_keys_step_and_pin_disassembly() {
        let mut app = test_app();
        app.open_debugger();

        // S steps one instruction while the entry box is unfocused.
        let pc_before = app.emu.machine.pc();
        assert!(app.ui_handle_key(KeyCode::KeyS));
        assert_eq!(app.emu.machine.pc(), pc_before.wrapping_add(2));

        // R toggles run; the explicit choice survives closing the panel.
        assert!(app.paused);
        assert!(app.ui_handle_key(KeyCode::KeyR));
        assert!(!app.paused);
        assert!(app.ui_handle_key(KeyCode::KeyR));
        assert!(app.paused);

        // On the CPU tab, Enter pins the disassembly origin to the typed
        // address; an empty box follows the PC again.
        if let Some(panel) = app.debugger_panel.as_mut() {
            panel.entry_active = true;
            panel.entry = "FC0010".to_string();
        }
        assert!(app.ui_handle_key(KeyCode::Enter));
        match app.debugger_panel.as_ref() {
            Some(panel) => {
                assert_eq!(panel.disasm_addr, Some(0xFC0010));
                let view = app.build_debugger_view(panel);
                // Each disasm line carries a one-char breakpoint marker prefix.
                assert!(view.lines.iter().any(|l| l.text.contains("00FC0010")));
            }
            _ => panic!("debugger panel should be open"),
        }
        if let Some(panel) = app.debugger_panel.as_mut() {
            panel.entry_active = true;
            panel.entry.clear();
        }
        assert!(app.ui_handle_key(KeyCode::Enter));
        match app.debugger_panel.as_ref() {
            Some(panel) => assert_eq!(panel.disasm_addr, None),
            _ => panic!("debugger panel should be open"),
        }

        // While the entry box is focused, S types the register-name letter
        // 'S' (for SR/SP) into the box instead of stepping.
        if let Some(panel) = app.debugger_panel.as_mut() {
            panel.entry_active = true;
            panel.entry.clear();
        }
        let pc_before = app.emu.machine.pc();
        assert!(app.ui_handle_key(KeyCode::KeyS));
        assert_eq!(app.emu.machine.pc(), pc_before);
        assert_eq!(
            app.debugger_panel.as_ref().map(|p| p.entry.as_str()),
            Some("S")
        );
    }

    #[test]
    fn debugger_poke_writes_memory_and_registers() {
        let mut app = test_app();
        app.open_debugger();
        // Map chip RAM at $0 so the low test address is writable RAM, not the
        // boot ROM overlay.
        app.emu.machine.disable_overlay();

        // Memory tab: "ADDR VALUE" writes a word into chip RAM.
        if let Some(panel) = app.debugger_panel.as_mut() {
            panel.tab = super::ui::DebugTab::Memory;
            panel.entry = "2000 BEEF".to_string();
        }
        app.debugger_poke();
        assert_eq!(
            app.emu.machine.debug_read_memory(0x2000, 2),
            vec![0xBE, 0xEF]
        );

        // CPU tab: "REG VALUE" sets a register.
        if let Some(panel) = app.debugger_panel.as_mut() {
            panel.tab = super::ui::DebugTab::Cpu;
            panel.entry = "D3 12345678".to_string();
        }
        app.debugger_poke();
        assert_eq!(app.emu.machine.d(3), 0x1234_5678);
    }
}
