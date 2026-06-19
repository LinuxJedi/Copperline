// SPDX-License-Identifier: GPL-3.0-or-later

//! In-window menu and overlay sub-windows (about, keyboard shortcuts,
//! gamepad calibration, debugger). Everything is drawn into the
//! presentation texture over the emulated display, styled after the
//! classic Amiga look: white menus with inverted highlights and blue
//! window title bars. This module owns layout, hit-testing and drawing;
//! `window.rs` routes events to it and builds the per-frame view data
//! (register snapshots, disassembly text) the panels render.

use super::window::{
    draw_rect_bevel, fill_rect, fill_rect_blend, rgba, scale_rect, JoystickInputMode, Rect,
    BUTTON_EDGE_DARK, BUTTON_EDGE_LIGHT, BUTTON_FACE, BUTTON_FACE_HOVER,
};
use super::{font, FB_WIDTH, HOST_SHORTCUT_MODIFIER_LABEL, PRESENT_HEIGHT};

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------

const MENU_BG: u32 = rgba(238, 238, 232);
const MENU_TEXT: u32 = rgba(12, 12, 14);
const MENU_HILIGHT_BG: u32 = rgba(0, 85, 170);
const MENU_HILIGHT_TEXT: u32 = rgba(255, 255, 255);
const MENU_EDGE: u32 = rgba(12, 12, 14);
const PANEL_BG: u32 = rgba(30, 32, 36);
const PANEL_TITLE_BG: u32 = rgba(0, 85, 170);
const PANEL_TITLE_TEXT: u32 = rgba(255, 255, 255);
const PANEL_TEXT: u32 = rgba(214, 216, 208);
const PANEL_TEXT_DIM: u32 = rgba(136, 138, 130);
const PANEL_TEXT_HILIGHT: u32 = rgba(120, 255, 150);
const PANEL_TEXT_ACCENT: u32 = rgba(255, 184, 80);
const BUTTON_TEXT: u32 = rgba(220, 222, 214);
const BUTTON_TEXT_DISABLED: u32 = rgba(120, 120, 112);
const ENTRY_BG: u32 = rgba(8, 10, 8);
const ENTRY_TEXT: u32 = rgba(27, 220, 71);
const SCRIM: u32 = rgba(0, 0, 0);
const SCRIM_ALPHA: f32 = 0.45;

// ---------------------------------------------------------------------------
// Menu
// ---------------------------------------------------------------------------

/// Status-bar anchor for the menu button; the pop-up opens above it.
pub const MENU_BUTTON_X: usize = FB_WIDTH - 220;
pub const MENU_BUTTON_W: usize = 22;

const MENU_W: usize = 360;
const MENU_ITEM_H: usize = 20;
const MENU_PAD: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuItem {
    About,
    Shortcuts,
    Calibration,
    Debugger,
    JoystickInput,
    Warp,
    Record,
    RecordInput,
    SaveState,
    LoadState,
    LoadRom,
}

pub const MENU_ITEMS: [MenuItem; 11] = [
    MenuItem::Debugger,
    MenuItem::Calibration,
    MenuItem::JoystickInput,
    MenuItem::Warp,
    MenuItem::Record,
    MenuItem::RecordInput,
    MenuItem::SaveState,
    MenuItem::LoadState,
    MenuItem::LoadRom,
    MenuItem::Shortcuts,
    MenuItem::About,
];

fn menu_item_label(
    item: MenuItem,
    warp: bool,
    recording: bool,
    input_recording: bool,
    joystick_input_mode: JoystickInputMode,
) -> String {
    match item {
        MenuItem::About => "About...".to_string(),
        MenuItem::Shortcuts => "Keyboard Shortcuts...".to_string(),
        MenuItem::Calibration => "Calibrate Gamepad...".to_string(),
        MenuItem::Debugger => "Debugger...".to_string(),
        MenuItem::JoystickInput => format!("Joystick Input  [{}]", joystick_input_mode.label()),
        MenuItem::Warp if warp => "Warp Speed      [on]".to_string(),
        MenuItem::Warp => "Warp Speed     [off]".to_string(),
        MenuItem::Record if recording => "Stop Video Recording".to_string(),
        MenuItem::Record => "Record Video".to_string(),
        MenuItem::RecordInput if input_recording => "Stop Input Recording".to_string(),
        MenuItem::RecordInput => "Record Input".to_string(),
        MenuItem::SaveState => "Save State".to_string(),
        MenuItem::LoadState => "Load State...".to_string(),
        MenuItem::LoadRom => "Load Kickstart ROM...".to_string(),
    }
}

fn menu_rect() -> Rect {
    let h = MENU_ITEMS.len() * MENU_ITEM_H + 2 * MENU_PAD;
    let right = MENU_BUTTON_X + MENU_BUTTON_W;
    Rect {
        x: right.saturating_sub(MENU_W),
        y: PRESENT_HEIGHT.saturating_sub(h + 2),
        w: MENU_W,
        h,
    }
}

fn menu_item_rect(index: usize) -> Rect {
    let menu = menu_rect();
    Rect {
        x: menu.x + 1,
        y: menu.y + MENU_PAD + index * MENU_ITEM_H,
        w: menu.w - 2,
        h: MENU_ITEM_H,
    }
}

// ---------------------------------------------------------------------------
// Panels (overlay sub-windows)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugTab {
    Cpu,
    Chipset,
    Copper,
    Memory,
    Break,
}

pub const DEBUG_TABS: [DebugTab; 5] = [
    DebugTab::Cpu,
    DebugTab::Chipset,
    DebugTab::Copper,
    DebugTab::Memory,
    DebugTab::Break,
];

fn debug_tab_label(tab: DebugTab) -> &'static str {
    match tab {
        DebugTab::Cpu => "CPU",
        DebugTab::Chipset => "Chipset",
        DebugTab::Copper => "Copper",
        DebugTab::Memory => "Memory",
        DebugTab::Break => "Break",
    }
}

/// Interactive state of the debugger sub-window.
pub struct DebuggerPanel {
    pub tab: DebugTab,
    /// Base address of the Memory tab's hex dump.
    pub mem_addr: u32,
    /// Pinned disassembly origin for the CPU tab; None follows the PC.
    pub disasm_addr: Option<u32>,
    /// The hex address being typed into the entry box.
    pub entry: String,
    /// Whether the entry box has keyboard focus.
    pub entry_active: bool,
}

impl DebuggerPanel {
    pub fn new() -> Self {
        Self {
            tab: DebugTab::Cpu,
            mem_addr: 0,
            disasm_addr: None,
            entry: String::new(),
            entry_active: false,
        }
    }

    /// The typed address, when the entry holds valid hex.
    pub fn entry_addr(&self) -> Option<u32> {
        parse_hex_u32(&self.entry)
    }

    pub fn push_entry_char(&mut self, ch: char) {
        if self.entry.len() < 8 && ch.is_ascii_hexdigit() {
            self.entry.push(ch.to_ascii_uppercase());
        }
    }

    pub fn backspace_entry(&mut self) {
        self.entry.pop();
    }
}

impl Default for DebuggerPanel {
    fn default() -> Self {
        Self::new()
    }
}

/// An open overlay sub-window.
pub enum Panel {
    About,
    Shortcuts,
    Calibration(crate::gamepad::CalibrationSession),
    Debugger(DebuggerPanel),
}

/// Menu/panel state owned by the window.
#[derive(Default)]
pub struct UiState {
    pub menu_open: bool,
    pub panel: Option<Panel>,
}

impl UiState {
    /// Whether the UI is consuming pointer/keyboard input.
    pub fn active(&self) -> bool {
        self.menu_open || self.panel.is_some()
    }

    /// The UI control under `pos`, if any. `PanelBody` swallows clicks on
    /// a panel's background so they never reach the emulated display.
    pub fn control_at(&self, pos: (i32, i32)) -> Option<UiControl> {
        if self.menu_open {
            for (index, item) in MENU_ITEMS.iter().enumerate() {
                if menu_item_rect(index).contains(pos) {
                    return Some(UiControl::MenuItem(*item));
                }
            }
            return menu_rect().contains(pos).then_some(UiControl::PanelBody);
        }
        let panel = self.panel.as_ref()?;
        let rect = panel_rect(panel);
        if close_button_rect(rect).contains(pos) {
            return Some(UiControl::PanelClose);
        }
        match panel {
            Panel::Calibration(session) => {
                for (control, button_rect) in cal_button_rects(rect) {
                    if button_rect.contains(pos) && cal_button_enabled(control, session) {
                        return Some(control);
                    }
                }
            }
            Panel::Debugger(panel) => {
                for (index, tab) in DEBUG_TABS.iter().enumerate() {
                    if debug_tab_rect(rect, index).contains(pos) {
                        return Some(UiControl::DebugTab(*tab));
                    }
                }
                for (control, button_rect) in debug_button_rects(rect) {
                    if button_rect.contains(pos) {
                        return Some(control);
                    }
                }
                if panel.tab == DebugTab::Break {
                    for (control, button_rect) in break_tab_button_rects(rect) {
                        if button_rect.contains(pos) {
                            return Some(control);
                        }
                    }
                }
            }
            Panel::About | Panel::Shortcuts => {}
        }
        rect.contains(pos).then_some(UiControl::PanelBody)
    }
}

/// A clickable UI control, used for hit-testing and hover highlights.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiControl {
    MenuItem(MenuItem),
    PanelClose,
    /// Anywhere on a panel that is not a specific control (swallows the
    /// click so it does not fall through to the display).
    PanelBody,
    CalSkip,
    CalCancel,
    CalSave,
    DebugTab(DebugTab),
    DebugRun,
    DebugStep,
    DebugStepFrame,
    DebugRunTo,
    /// Reverse-debug: step one instruction backward (reconstructed from the
    /// snapshot ring).
    DebugReverseStep,
    /// Reverse-debug: run backward to the previous breakpoint/watch hit.
    DebugReverseRun,
    DebugMemPrev,
    DebugMemNext,
    DebugEntry,
    /// Break tab: toggle a PC breakpoint at the entry address.
    DebugBreakToggle,
    /// Break tab: toggle a memory word watchpoint at the entry address.
    DebugWatchToggle,
    /// Break tab: toggle a chipset-register write watch at the entry
    /// address (an offset or a full $DFFxxx address).
    DebugRegToggle,
    /// Break tab: remove all breakpoints and watchpoints.
    DebugBreaksClear,
}

fn panel_dims(panel: &Panel) -> (usize, usize) {
    match panel {
        Panel::About => (560, 380),
        Panel::Shortcuts => (600, 352),
        Panel::Calibration(_) => (620, 372),
        Panel::Debugger(_) => (684, 520),
    }
}

fn panel_title(panel: &Panel) -> &'static str {
    match panel {
        Panel::About => "About Copperline",
        Panel::Shortcuts => "Keyboard Shortcuts",
        Panel::Calibration(_) => "Gamepad Calibration",
        Panel::Debugger(_) => "Debugger",
    }
}

fn panel_rect(panel: &Panel) -> Rect {
    let (w, h) = panel_dims(panel);
    Rect {
        x: (FB_WIDTH.saturating_sub(w)) / 2,
        y: (PRESENT_HEIGHT.saturating_sub(h)) / 2,
        w,
        h,
    }
}

const TITLE_H: usize = 22;

fn close_button_rect(rect: Rect) -> Rect {
    Rect {
        x: rect.x + rect.w - TITLE_H,
        y: rect.y,
        w: TITLE_H,
        h: TITLE_H,
    }
}

// Calibration buttons along the panel's bottom edge.
const CAL_BUTTON_W: usize = 96;
const CAL_BUTTON_H: usize = 22;

fn cal_button_rects(rect: Rect) -> [(UiControl, Rect); 3] {
    let y = rect.y + rect.h - CAL_BUTTON_H - 8;
    let button = |i: usize| Rect {
        x: rect.x + rect.w - (3 - i) * (CAL_BUTTON_W + 8),
        y,
        w: CAL_BUTTON_W,
        h: CAL_BUTTON_H,
    };
    [
        (UiControl::CalSkip, button(0)),
        (UiControl::CalCancel, button(1)),
        (UiControl::CalSave, button(2)),
    ]
}

fn cal_button_enabled(control: UiControl, session: &crate::gamepad::CalibrationSession) -> bool {
    match control {
        UiControl::CalSkip => session.can_skip(),
        UiControl::CalSave => session.done(),
        _ => true,
    }
}

// Debugger chrome: a tab row under the title and a control row at the
// bottom with the transport buttons and the shared hex-entry box.
const DEBUG_TAB_W: usize = 90;
const DEBUG_TAB_H: usize = 18;
const DEBUG_BUTTON_H: usize = 20;

fn debug_tab_rect(rect: Rect, index: usize) -> Rect {
    Rect {
        x: rect.x + 8 + index * (DEBUG_TAB_W + 4),
        y: rect.y + TITLE_H + 4,
        w: DEBUG_TAB_W,
        h: DEBUG_TAB_H,
    }
}

fn debug_button_rects(rect: Rect) -> [(UiControl, Rect); 9] {
    let y = rect.y + rect.h - DEBUG_BUTTON_H - 6;
    let button = |x: usize, w: usize| Rect {
        x: rect.x + x,
        y,
        w,
        h: DEBUG_BUTTON_H,
    };
    [
        (UiControl::DebugRun, button(8, 64)),
        (UiControl::DebugStep, button(76, 56)),
        (UiControl::DebugStepFrame, button(136, 64)),
        (UiControl::DebugRunTo, button(204, 76)),
        (UiControl::DebugEntry, button(284, 110)),
        (UiControl::DebugMemPrev, button(398, 28)),
        (UiControl::DebugMemNext, button(430, 28)),
        // Reverse-debug transport, in the free space at the row's right end.
        (UiControl::DebugReverseStep, button(466, 86)),
        (UiControl::DebugReverseRun, button(556, 92)),
    ]
}

/// Top of a debugger tab's content area (under the tab row).
fn debug_content_top(rect: Rect) -> usize {
    rect.y + TITLE_H + 4 + DEBUG_TAB_H + 6
}

/// Content lines the Break tab's view must leave blank so the toggle
/// buttons drawn at the top of the content area do not overlap text.
pub const BREAK_TAB_HEADER_LINES: usize = 3;

/// The Break tab's toggle buttons, drawn at the top of the content area.
fn break_tab_button_rects(rect: Rect) -> [(UiControl, Rect); 4] {
    let y = debug_content_top(rect);
    let button = |i: usize| Rect {
        x: rect.x + 10 + i * 98,
        y,
        w: 90,
        h: DEBUG_BUTTON_H,
    };
    [
        (UiControl::DebugBreakToggle, button(0)),
        (UiControl::DebugWatchToggle, button(1)),
        (UiControl::DebugRegToggle, button(2)),
        (UiControl::DebugBreaksClear, button(3)),
    ]
}

/// Bytes shown per Memory-tab page (16 rows of 16).
pub const MEM_PAGE_BYTES: u32 = 256;

// ---------------------------------------------------------------------------
// View data built by window.rs each redraw
// ---------------------------------------------------------------------------

pub struct AboutView {
    /// Emulated-machine summary lines (built once at startup).
    pub machine_lines: Vec<String>,
}

pub struct CalRow {
    pub label: &'static str,
    pub binding: String,
    pub current: bool,
}

pub struct CalibrationView {
    pub pad_line: String,
    pub rows: Vec<CalRow>,
    pub status: String,
}

pub struct DbgLine {
    pub text: String,
    pub highlight: bool,
}

impl DbgLine {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            highlight: false,
        }
    }

    pub fn hilit(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            highlight: true,
        }
    }
}

pub struct DebuggerView {
    /// False while the machine is paused (the debugger's usual state).
    pub running: bool,
    /// Whether reverse debugging is armed (snapshot ring present), gating the
    /// reverse transport buttons.
    pub reverse_available: bool,
    /// Status summary drawn in the title bar (frame count, emulated time).
    pub status: String,
    /// Pre-formatted content lines of the active tab.
    pub lines: Vec<DbgLine>,
}

pub enum PanelViewData {
    About(AboutView),
    Shortcuts,
    Calibration(CalibrationView),
    Debugger(DebuggerView),
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

fn draw_panel_text(
    frame: &mut [u8],
    x: usize,
    y: usize,
    text: &str,
    color: u32,
    px: usize,
    texture_scale: usize,
) {
    font::draw_text(
        frame,
        super::window::texture_width(texture_scale),
        super::window::texture_height(texture_scale),
        x * texture_scale,
        y * texture_scale,
        text,
        color,
        px * texture_scale,
    );
}

fn draw_text_button(
    frame: &mut [u8],
    rect: Rect,
    label: &str,
    enabled: bool,
    hover: bool,
    texture_scale: usize,
) {
    let face = if hover && enabled {
        BUTTON_FACE_HOVER
    } else {
        BUTTON_FACE
    };
    let scaled = scale_rect(rect, texture_scale);
    fill_rect(frame, scaled, face, texture_scale);
    draw_rect_bevel(
        frame,
        scaled,
        BUTTON_EDGE_LIGHT,
        BUTTON_EDGE_DARK,
        texture_scale,
    );
    let color = if enabled {
        BUTTON_TEXT
    } else {
        BUTTON_TEXT_DISABLED
    };
    let text_w = label.chars().count() * font::GLYPH_W;
    let x = rect.x + rect.w.saturating_sub(text_w) / 2;
    let y = rect.y + rect.h.saturating_sub(font::GLYPH_H) / 2;
    draw_panel_text(frame, x, y, label, color, 1, texture_scale);
}

fn draw_panel_chrome(frame: &mut [u8], panel: &Panel, hover: Option<UiControl>, scale: usize) {
    let rect = panel_rect(panel);
    // Dim the display behind the window so the panel reads as modal.
    fill_rect_blend(
        frame,
        scale_rect(
            Rect {
                x: 0,
                y: 0,
                w: FB_WIDTH,
                h: PRESENT_HEIGHT,
            },
            scale,
        ),
        SCRIM,
        SCRIM_ALPHA,
        scale,
    );
    let scaled = scale_rect(rect, scale);
    fill_rect(frame, scaled, PANEL_BG, scale);
    draw_rect_bevel(frame, scaled, BUTTON_EDGE_LIGHT, BUTTON_EDGE_DARK, scale);
    // Title bar.
    let title = Rect {
        x: rect.x + 1,
        y: rect.y + 1,
        w: rect.w - 2,
        h: TITLE_H - 1,
    };
    fill_rect(frame, scale_rect(title, scale), PANEL_TITLE_BG, scale);
    draw_panel_text(
        frame,
        rect.x + 10,
        rect.y + (TITLE_H - 16) / 2,
        panel_title(panel),
        PANEL_TITLE_TEXT,
        2,
        scale,
    );
    // Close gadget: classic square with an inner square.
    let close = close_button_rect(rect);
    let close_hover = hover == Some(UiControl::PanelClose);
    let face = if close_hover {
        BUTTON_FACE_HOVER
    } else {
        PANEL_TITLE_BG
    };
    let close_scaled = scale_rect(
        Rect {
            x: close.x + 1,
            y: close.y + 1,
            w: close.w - 2,
            h: close.h - 1,
        },
        scale,
    );
    fill_rect(frame, close_scaled, face, scale);
    draw_rect_bevel(
        frame,
        close_scaled,
        BUTTON_EDGE_LIGHT,
        BUTTON_EDGE_DARK,
        scale,
    );
    let inner = Rect {
        x: close.x + close.w / 2 - 4,
        y: close.y + close.h / 2 - 4,
        w: 8,
        h: 8,
    };
    fill_rect(frame, scale_rect(inner, scale), PANEL_TITLE_TEXT, scale);
    let hole = Rect {
        x: inner.x + 2,
        y: inner.y + 2,
        w: 4,
        h: 4,
    };
    fill_rect(frame, scale_rect(hole, scale), face, scale);
}

fn draw_menu(
    frame: &mut [u8],
    hover: Option<UiControl>,
    warp: bool,
    recording: bool,
    input_recording: bool,
    joystick_input_mode: JoystickInputMode,
    scale: usize,
) {
    let rect = menu_rect();
    let scaled = scale_rect(rect, scale);
    fill_rect(frame, scaled, MENU_BG, scale);
    draw_rect_bevel(frame, scaled, MENU_EDGE, MENU_EDGE, scale);
    for (index, item) in MENU_ITEMS.iter().enumerate() {
        let item_rect = menu_item_rect(index);
        let hovered = hover == Some(UiControl::MenuItem(*item));
        let (bg, fg) = if hovered {
            (MENU_HILIGHT_BG, MENU_HILIGHT_TEXT)
        } else {
            (MENU_BG, MENU_TEXT)
        };
        if hovered {
            fill_rect(frame, scale_rect(item_rect, scale), bg, scale);
        }
        draw_panel_text(
            frame,
            item_rect.x + 8,
            item_rect.y + (MENU_ITEM_H - 16) / 2,
            &menu_item_label(*item, warp, recording, input_recording, joystick_input_mode),
            fg,
            2,
            scale,
        );
    }
}

/// Word-wrap `text` so no panel line is cropped: the first line holds up to
/// `first_width` characters, continuations up to `rest_width` (they are drawn
/// indented). Words longer than a whole line are hard-split.
fn wrap_text(text: &str, first_width: usize, rest_width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let mut word: Vec<char> = word.chars().collect();
        while !word.is_empty() {
            let width = if lines.is_empty() {
                first_width
            } else {
                rest_width
            }
            .max(1);
            let cur_len = cur.chars().count();
            let sep = usize::from(!cur.is_empty());
            if cur_len + sep + word.len() <= width {
                if sep == 1 {
                    cur.push(' ');
                }
                cur.extend(word.drain(..));
            } else if cur.is_empty() {
                let take = width.min(word.len());
                cur.extend(word.drain(..take));
                lines.push(std::mem::take(&mut cur));
            } else {
                lines.push(std::mem::take(&mut cur));
            }
        }
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(cur);
    }
    lines
}

fn draw_about(frame: &mut [u8], rect: Rect, view: &AboutView, scale: usize) {
    let cx = |text: &str, px: usize| rect.x + rect.w.saturating_sub(text.len() * 8 * px) / 2;
    let title = "Copperline";
    let mut y = rect.y + TITLE_H + 14;
    draw_panel_text(frame, cx(title, 3), y, title, PANEL_TEXT_HILIGHT, 3, scale);
    y += 30;
    let version = concat!("version ", env!("CARGO_PKG_VERSION"));
    draw_panel_text(frame, cx(version, 1), y, version, PANEL_TEXT_DIM, 1, scale);
    y += 14;
    let tagline = "A cycle-stepped Amiga emulator";
    draw_panel_text(frame, cx(tagline, 2), y, tagline, PANEL_TEXT, 2, scale);
    y += 22;
    let author = "by Andrew \"LinuxJedi\" Hutchings";
    draw_panel_text(frame, cx(author, 1), y, author, PANEL_TEXT_DIM, 1, scale);
    y += 24;
    let max_chars = (rect.w - 48) / 16;
    for line in &view.machine_lines {
        for (i, part) in wrap_text(line, max_chars, max_chars.saturating_sub(1))
            .iter()
            .enumerate()
        {
            // Continuation lines are indented by one glyph cell.
            let x = rect.x + 24 + if i == 0 { 0 } else { 16 };
            draw_panel_text(frame, x, y, part, PANEL_TEXT, 2, scale);
            y += 18;
        }
    }
    y += 10;
    for line in [
        "m68k CPU core (MIT)",
        "font8x8 by Daniel Hepper / Marcel Sondaar",
        "winit + pixels + cpal + gilrs",
    ] {
        draw_panel_text(frame, rect.x + 24, y, line, PANEL_TEXT_DIM, 1, scale);
        y += 12;
    }
}

const SHORTCUT_ROWS: [(&str, &str, bool); 12] = [
    ("Q", "Quit", true),
    ("S", "Save screenshot", true),
    ("R", "Record video on/off", true),
    ("Shift+R", "Record input on/off", true),
    ("Shift+S", "Save state", true),
    ("Shift+L", "Load state", true),
    ("D", "Swap queued disk", true),
    ("G", "Capture mouse", true),
    ("B", "Debugger", true),
    ("J", "Joystick input mode", true),
    ("Esc", "Close menu/window", false),
    ("Ctrl+Ami+Ami", "Keyboard reset", false),
];

fn draw_shortcuts(frame: &mut [u8], rect: Rect, scale: usize) {
    let mut y = rect.y + TITLE_H + 14;
    for (key, action, host_shortcut) in SHORTCUT_ROWS {
        let key_label = if host_shortcut {
            format!("{HOST_SHORTCUT_MODIFIER_LABEL}+{key}")
        } else {
            key.to_string()
        };
        draw_panel_text(
            frame,
            rect.x + 24,
            y,
            &key_label,
            PANEL_TEXT_ACCENT,
            2,
            scale,
        );
        draw_panel_text(frame, rect.x + 248, y, action, PANEL_TEXT, 2, scale);
        y += 22;
    }
    y += 8;
    for line in [
        "Shortcuts: Cmd on macOS, Alt on Linux/Windows",
        "Amiga modifiers: Alt, Cmd/Super=Amiga, Ctrl",
        "In the debugger: S step, F frame, R run/pause",
    ] {
        draw_panel_text(frame, rect.x + 24, y, line, PANEL_TEXT_DIM, 1, scale);
        y += 12;
    }
}

fn draw_calibration(
    frame: &mut [u8],
    rect: Rect,
    view: &CalibrationView,
    hover: Option<UiControl>,
    session: &crate::gamepad::CalibrationSession,
    scale: usize,
) {
    let mut y = rect.y + TITLE_H + 10;
    draw_panel_text(frame, rect.x + 16, y, &view.pad_line, PANEL_TEXT, 2, scale);
    y += 24;
    for row in &view.rows {
        let (marker, color) = if row.current {
            (">", PANEL_TEXT_HILIGHT)
        } else if row.binding.is_empty() {
            (" ", PANEL_TEXT_DIM)
        } else {
            (" ", PANEL_TEXT)
        };
        draw_panel_text(frame, rect.x + 16, y, marker, PANEL_TEXT_HILIGHT, 2, scale);
        draw_panel_text(frame, rect.x + 36, y, row.label, color, 2, scale);
        draw_panel_text(frame, rect.x + 388, y, &row.binding, color, 2, scale);
        y += 18;
    }
    y += 6;
    draw_panel_text(
        frame,
        rect.x + 16,
        y,
        &view.status,
        PANEL_TEXT_ACCENT,
        1,
        scale,
    );
    for (control, button_rect) in cal_button_rects(rect) {
        let label = match control {
            UiControl::CalSkip => "Skip",
            UiControl::CalCancel => "Cancel",
            _ => "Save",
        };
        draw_text_button(
            frame,
            button_rect,
            label,
            cal_button_enabled(control, session),
            hover == Some(control),
            scale,
        );
    }
}

fn draw_debugger(
    frame: &mut [u8],
    rect: Rect,
    panel: &DebuggerPanel,
    view: &DebuggerView,
    hover: Option<UiControl>,
    scale: usize,
) {
    // Status summary on the right of the title bar.
    let status_w = view.status.chars().count() * font::GLYPH_W;
    draw_panel_text(
        frame,
        rect.x + rect.w - TITLE_H - 8 - status_w.min(rect.w.saturating_sub(TITLE_H + 16)),
        rect.y + (TITLE_H - 8) / 2,
        &view.status,
        PANEL_TITLE_TEXT,
        1,
        scale,
    );
    // Tabs.
    for (index, tab) in DEBUG_TABS.iter().enumerate() {
        let tab_rect = debug_tab_rect(rect, index);
        let selected = panel.tab == *tab;
        let hovered = hover == Some(UiControl::DebugTab(*tab));
        let face = if selected {
            ENTRY_BG
        } else if hovered {
            BUTTON_FACE_HOVER
        } else {
            BUTTON_FACE
        };
        let scaled = scale_rect(tab_rect, scale);
        fill_rect(frame, scaled, face, scale);
        draw_rect_bevel(frame, scaled, BUTTON_EDGE_LIGHT, BUTTON_EDGE_DARK, scale);
        let label = debug_tab_label(*tab);
        let text_w = label.chars().count() * font::GLYPH_W;
        draw_panel_text(
            frame,
            tab_rect.x + tab_rect.w.saturating_sub(text_w) / 2,
            tab_rect.y + (DEBUG_TAB_H - 8) / 2,
            label,
            if selected { ENTRY_TEXT } else { BUTTON_TEXT },
            1,
            scale,
        );
    }
    // Break-tab toggle buttons at the top of the content area (the view
    // leaves BREAK_TAB_HEADER_LINES blank so text starts below them).
    if panel.tab == DebugTab::Break {
        for (control, button_rect) in break_tab_button_rects(rect) {
            let label = match control {
                UiControl::DebugBreakToggle => "Break +/-",
                UiControl::DebugWatchToggle => "Watch +/-",
                UiControl::DebugRegToggle => "Reg +/-",
                _ => "Clear all",
            };
            let enabled = control == UiControl::DebugBreaksClear || panel.entry_addr().is_some();
            draw_text_button(
                frame,
                button_rect,
                label,
                enabled,
                hover == Some(control),
                scale,
            );
        }
    }
    // Content lines.
    let content_top = debug_content_top(rect);
    let content_bottom = rect.y + rect.h - DEBUG_BUTTON_H - 12;
    let pitch = 10;
    let max_lines = content_bottom.saturating_sub(content_top) / pitch;
    for (index, line) in view.lines.iter().take(max_lines).enumerate() {
        let color = if line.highlight {
            PANEL_TEXT_HILIGHT
        } else {
            PANEL_TEXT
        };
        draw_panel_text(
            frame,
            rect.x + 10,
            content_top + index * pitch,
            &line.text,
            color,
            1,
            scale,
        );
    }
    // Transport buttons and the hex-entry box.
    for (control, button_rect) in debug_button_rects(rect) {
        match control {
            UiControl::DebugEntry => {
                let scaled = scale_rect(button_rect, scale);
                fill_rect(frame, scaled, ENTRY_BG, scale);
                draw_rect_bevel(frame, scaled, BUTTON_EDGE_DARK, BUTTON_EDGE_LIGHT, scale);
                let caret = if panel.entry_active { "_" } else { "" };
                let text = format!("${}{}", panel.entry, caret);
                draw_panel_text(
                    frame,
                    button_rect.x + 6,
                    button_rect.y + (DEBUG_BUTTON_H - 8) / 2,
                    &text,
                    ENTRY_TEXT,
                    1,
                    scale,
                );
            }
            _ => {
                let label = match control {
                    UiControl::DebugRun => {
                        if view.running {
                            "Pause"
                        } else {
                            "Run"
                        }
                    }
                    UiControl::DebugStep => "Step",
                    UiControl::DebugStepFrame => "Frame",
                    UiControl::DebugRunTo => "Run to $",
                    UiControl::DebugReverseStep => "< Step",
                    UiControl::DebugReverseRun => "< Run",
                    UiControl::DebugMemPrev => "<",
                    UiControl::DebugMemNext => ">",
                    _ => "",
                };
                let enabled = match control {
                    UiControl::DebugMemPrev | UiControl::DebugMemNext => {
                        panel.tab == DebugTab::Memory
                    }
                    UiControl::DebugRunTo => panel.entry_addr().is_some(),
                    UiControl::DebugReverseStep | UiControl::DebugReverseRun => {
                        view.reverse_available
                    }
                    _ => true,
                };
                draw_text_button(
                    frame,
                    button_rect,
                    label,
                    enabled,
                    hover == Some(control),
                    scale,
                );
            }
        }
    }
}

/// Draw the whole UI layer: pop-up menu and/or the open panel. Drawn after
/// the status bar and OSD so it sits on top of everything.
pub fn draw(
    frame: &mut [u8],
    texture_scale: usize,
    ui: &UiState,
    hover: Option<UiControl>,
    data: Option<&PanelViewData>,
    warp: bool,
    recording: bool,
    input_recording: bool,
    joystick_input_mode: JoystickInputMode,
) {
    if let Some(panel) = &ui.panel {
        draw_panel_chrome(frame, panel, hover, texture_scale);
        let rect = panel_rect(panel);
        match (panel, data) {
            (Panel::About, Some(PanelViewData::About(view))) => {
                draw_about(frame, rect, view, texture_scale)
            }
            (Panel::Shortcuts, _) => draw_shortcuts(frame, rect, texture_scale),
            (Panel::Calibration(session), Some(PanelViewData::Calibration(view))) => {
                draw_calibration(frame, rect, view, hover, session, texture_scale)
            }
            (Panel::Debugger(panel_state), Some(PanelViewData::Debugger(view))) => {
                draw_debugger(frame, rect, panel_state, view, hover, texture_scale)
            }
            _ => {}
        }
    }
    if ui.menu_open {
        draw_menu(
            frame,
            hover,
            warp,
            recording,
            input_recording,
            joystick_input_mode,
            texture_scale,
        );
    }
}

// ---------------------------------------------------------------------------
// Pure formatting helpers (shared with window.rs view builders)
// ---------------------------------------------------------------------------

pub fn parse_hex_u32(s: &str) -> Option<u32> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    u32::from_str_radix(s, 16).ok()
}

const DMACON_BITS: [(u16, &str); 15] = [
    (1 << 14, "BBUSY"),
    (1 << 13, "BZERO"),
    (1 << 10, "BLTPRI"),
    (1 << 9, "DMAEN"),
    (1 << 8, "BPLEN"),
    (1 << 7, "COPEN"),
    (1 << 6, "BLTEN"),
    (1 << 5, "SPREN"),
    (1 << 4, "DSKEN"),
    (1 << 3, "AUD3"),
    (1 << 2, "AUD2"),
    (1 << 1, "AUD1"),
    (1 << 0, "AUD0"),
    (1 << 12, "B12"),
    (1 << 11, "B11"),
];

const INT_BITS: [(u16, &str); 15] = [
    (1 << 14, "INTEN"),
    (1 << 13, "EXTER"),
    (1 << 12, "DSKSYN"),
    (1 << 11, "RBF"),
    (1 << 10, "AUD3"),
    (1 << 9, "AUD2"),
    (1 << 8, "AUD1"),
    (1 << 7, "AUD0"),
    (1 << 6, "BLIT"),
    (1 << 5, "VERTB"),
    (1 << 4, "COPER"),
    (1 << 3, "PORTS"),
    (1 << 2, "SOFT"),
    (1 << 1, "DSKBLK"),
    (1 << 0, "TBE"),
];

fn decode_bits(value: u16, names: &[(u16, &str)]) -> String {
    let set: Vec<&str> = names
        .iter()
        .filter(|(bit, _)| value & bit != 0)
        .map(|(_, name)| *name)
        .collect();
    if set.is_empty() {
        "-".to_string()
    } else {
        set.join(" ")
    }
}

/// The set DMACON bit names, most significant first.
pub fn dmacon_flags(value: u16) -> String {
    decode_bits(value, &DMACON_BITS)
}

/// The set INTENA/INTREQ bit names, most significant first.
pub fn int_flags(value: u16) -> String {
    decode_bits(value, &INT_BITS)
}

/// A compact status-register summary: supervisor/user, interrupt mask,
/// trace, and the CCR flags (uppercase = set).
pub fn sr_flags(sr: u16) -> String {
    let mode = if sr & 0x2000 != 0 { 'S' } else { 'U' };
    let trace = if sr & 0x8000 != 0 { "T " } else { "" };
    let ipl = (sr >> 8) & 7;
    let ccr: String = [(4, 'X'), (3, 'N'), (2, 'Z'), (1, 'V'), (0, 'C')]
        .iter()
        .map(|&(bit, ch)| {
            if sr & (1 << bit) != 0 {
                ch
            } else {
                ch.to_ascii_lowercase()
            }
        })
        .collect();
    format!("{trace}{mode} IPL{ipl} {ccr}")
}

/// One hex-dump row: address, 16 bytes as hex, then printable ASCII.
pub fn hex_dump_row(addr: u32, bytes: &[u8]) -> String {
    let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02X}")).collect();
    let ascii: String = bytes
        .iter()
        .map(|&b| {
            if (0x20..0x7F).contains(&b) {
                b as char
            } else {
                '.'
            }
        })
        .collect();
    format!("{addr:06X}: {}  {ascii}", hex.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_sits_above_the_status_bar_and_hit_tests_items() {
        let rect = menu_rect();
        assert!(rect.y + rect.h <= PRESENT_HEIGHT);
        assert!(rect.x + rect.w <= FB_WIDTH);

        let ui = UiState {
            menu_open: true,
            panel: None,
        };
        let first = menu_item_rect(0);
        let pos = (first.x as i32 + 4, first.y as i32 + 4);
        assert_eq!(ui.control_at(pos), Some(UiControl::MenuItem(MENU_ITEMS[0])));
        let joystick = menu_item_rect(2);
        let pos = (joystick.x as i32 + 4, joystick.y as i32 + 4);
        assert_eq!(
            ui.control_at(pos),
            Some(UiControl::MenuItem(MenuItem::JoystickInput))
        );
        // Outside the menu: nothing (the click closes the menu).
        assert_eq!(ui.control_at((0, 0)), None);
    }

    #[test]
    fn panel_close_button_hit_tests() {
        let ui = UiState {
            menu_open: false,
            panel: Some(Panel::About),
        };
        let rect = panel_rect(ui.panel.as_ref().unwrap());
        let close = close_button_rect(rect);
        let pos = (close.x as i32 + 2, close.y as i32 + 2);
        assert_eq!(ui.control_at(pos), Some(UiControl::PanelClose));
        // Panel body swallows clicks.
        let body = (rect.x as i32 + 5, (rect.y + TITLE_H + 5) as i32);
        assert_eq!(ui.control_at(body), Some(UiControl::PanelBody));
        // Outside the panel: nothing.
        assert_eq!(ui.control_at((0, 0)), None);
    }

    #[test]
    fn debugger_controls_hit_test_and_entry_edits() {
        let ui = UiState {
            menu_open: false,
            panel: Some(Panel::Debugger(DebuggerPanel::new())),
        };
        let rect = panel_rect(ui.panel.as_ref().unwrap());
        let tab = debug_tab_rect(rect, 3);
        assert_eq!(
            ui.control_at((tab.x as i32 + 2, tab.y as i32 + 2)),
            Some(UiControl::DebugTab(DebugTab::Memory))
        );
        let (control, step) = debug_button_rects(rect)[1];
        assert_eq!(control, UiControl::DebugStep);
        assert_eq!(
            ui.control_at((step.x as i32 + 2, step.y as i32 + 2)),
            Some(UiControl::DebugStep)
        );

        // Break-tab toggle buttons hit-test only while that tab is active.
        let mut panel = DebuggerPanel::new();
        panel.tab = DebugTab::Break;
        let ui_break = UiState {
            menu_open: false,
            panel: Some(Panel::Debugger(panel)),
        };
        let (control, toggle) = break_tab_button_rects(rect)[0];
        assert_eq!(control, UiControl::DebugBreakToggle);
        let pos = (toggle.x as i32 + 2, toggle.y as i32 + 2);
        assert_eq!(ui_break.control_at(pos), Some(UiControl::DebugBreakToggle));
        // On another tab the same position is just panel body.
        assert_eq!(ui.control_at(pos), Some(UiControl::PanelBody));

        let mut panel = DebuggerPanel::new();
        for ch in ['c', '0', '0', 'z', '3', 'C'] {
            panel.push_entry_char(ch);
        }
        assert_eq!(panel.entry, "C003C");
        assert_eq!(panel.entry_addr(), Some(0xC003C));
        panel.backspace_entry();
        assert_eq!(panel.entry, "C003");
        // Capped at 8 hex digits.
        for _ in 0..12 {
            panel.push_entry_char('F');
        }
        assert_eq!(panel.entry.len(), 8);
    }

    #[test]
    fn flag_decoders_name_set_bits() {
        assert_eq!(dmacon_flags(0), "-");
        let flags = dmacon_flags(0x8390 & 0x7FFF);
        assert!(flags.contains("DMAEN"));
        assert!(flags.contains("BPLEN"));
        assert!(flags.contains("COPEN"));
        assert!(flags.contains("DSKEN"));
        assert!(!flags.contains("BLTEN"));

        let ints = int_flags((1 << 5) | (1 << 6) | (1 << 14));
        assert_eq!(ints, "INTEN BLIT VERTB");

        assert_eq!(sr_flags(0x2700), "S IPL7 xnzvc");
        assert_eq!(sr_flags(0x0015), "U IPL0 XnZvC");
        assert_eq!(sr_flags(0xA01F), "T S IPL0 XNZVC");
    }

    #[test]
    fn hex_dump_row_formats_address_hex_and_ascii() {
        let bytes: Vec<u8> = (0x41..0x51).collect();
        let row = hex_dump_row(0xC00000, &bytes);
        assert!(row.starts_with("C00000: 41 42 43"));
        assert!(row.ends_with("ABCDEFGHIJKLMNOP"));
    }

    #[test]
    fn parse_hex_entry() {
        assert_eq!(parse_hex_u32("C00000"), Some(0xC00000));
        assert_eq!(parse_hex_u32(""), None);
        assert_eq!(parse_hex_u32("xyz"), None);
    }

    /// Render each panel and the menu into a presentation-sized frame.
    /// Always asserts the drawing landed inside the right region; with
    /// COPPERLINE_UI_PREVIEW=1 also saves PNGs for eyeballing layout.
    #[test]
    fn wrap_text_keeps_long_lines_whole() {
        // Short lines pass through untouched.
        assert_eq!(wrap_text("Machine: A1200", 32, 31), vec!["Machine: A1200"]);
        // Long lines wrap at word boundaries with nothing dropped.
        let rom = "ROM: system v3.1 a1200 release image path rom";
        let wrapped = wrap_text(rom, 32, 31);
        assert!(wrapped.len() > 1);
        assert!(wrapped.iter().all(|l| l.chars().count() <= 32));
        assert_eq!(wrapped.join(" "), rom);
        // Words longer than a whole line are hard-split, not dropped.
        let long_word = "a".repeat(70);
        let wrapped = wrap_text(&long_word, 32, 31);
        assert_eq!(wrapped.concat(), long_word);
        // Empty input still yields one (blank) line.
        assert_eq!(wrap_text("", 32, 31), vec![String::new()]);
    }

    #[test]
    fn panels_render_into_their_rects() {
        use super::super::window::{texture_height, texture_width};

        let scale = 1;
        let (w, h) = (texture_width(scale), texture_height(scale));
        let save = |frame: &[u8], name: &str| {
            if !crate::envcfg::flag("COPPERLINE_UI_PREVIEW") {
                return;
            }
            let path = format!("target/ui-preview-{name}.png");
            let file = std::fs::File::create(&path).unwrap();
            let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), w as u32, h as u32);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(frame).unwrap();
            eprintln!("saved {path}");
        };
        let panel_has_title_bar = |frame: &[u8], panel: &Panel| {
            let rect = panel_rect(panel);
            let probe = ((rect.y + 10) * w + rect.x + 4) * 4;
            let pixel = &frame[probe..probe + 4];
            pixel == PANEL_TITLE_BG.to_le_bytes()
        };

        let mut frame = vec![0u8; w * h * 4];
        let ui = UiState {
            menu_open: true,
            panel: None,
        };
        draw(
            &mut frame,
            scale,
            &ui,
            None,
            None,
            true,
            false,
            false,
            JoystickInputMode::Auto,
        );
        let menu = menu_rect();
        let probe = ((menu.y + MENU_PAD + 2) * w + menu.x + 4) * 4;
        assert_eq!(&frame[probe..probe + 4], &MENU_BG.to_le_bytes());
        save(&frame, "menu");

        let mut frame = vec![0u8; w * h * 4];
        let ui = UiState {
            menu_open: false,
            panel: Some(Panel::About),
        };
        let data = PanelViewData::About(AboutView {
            machine_lines: vec![
                "Machine: A1200".to_string(),
                "CPU: M68EC020 @ 14 MHz".to_string(),
                "Chipset: AGA (Alice/Lisa, PAL)".to_string(),
                "RAM: 2048K chip, 4096K fast".to_string(),
                "ROM: system v3.1 a1200 release image path rom".to_string(),
                "Floppy drives: 1".to_string(),
            ],
        });
        draw(
            &mut frame,
            scale,
            &ui,
            None,
            Some(&data),
            false,
            false,
            false,
            JoystickInputMode::Auto,
        );
        assert!(panel_has_title_bar(&frame, ui.panel.as_ref().unwrap()));
        save(&frame, "about");

        let mut frame = vec![0u8; w * h * 4];
        let ui = UiState {
            menu_open: false,
            panel: Some(Panel::Shortcuts),
        };
        draw(
            &mut frame,
            scale,
            &ui,
            None,
            Some(&PanelViewData::Shortcuts),
            false,
            false,
            false,
            JoystickInputMode::Auto,
        );
        assert!(panel_has_title_bar(&frame, ui.panel.as_ref().unwrap()));
        save(&frame, "shortcuts");

        let mut frame = vec![0u8; w * h * 4];
        let session = crate::gamepad::CalibrationSession::new();
        let rows = (0..crate::gamepad::CalibrationSession::step_count())
            .map(|index| CalRow {
                label: crate::gamepad::CalibrationSession::step_label(index),
                binding: if index == 0 {
                    "axis 10031-".to_string()
                } else {
                    String::new()
                },
                current: index == 1,
            })
            .collect();
        let data = PanelViewData::Calibration(CalibrationView {
            pad_line: "Controller: USB Retro Pad".to_string(),
            rows,
            status: "Push and hold the control on the pad.".to_string(),
        });
        let ui = UiState {
            menu_open: false,
            panel: Some(Panel::Calibration(session)),
        };
        draw(
            &mut frame,
            scale,
            &ui,
            Some(UiControl::CalCancel),
            Some(&data),
            false,
            false,
            false,
            JoystickInputMode::Auto,
        );
        assert!(panel_has_title_bar(&frame, ui.panel.as_ref().unwrap()));
        save(&frame, "calibration");

        let mut frame = vec![0u8; w * h * 4];
        let mut lines = vec![
            DbgLine::plain("PC 00FC0E44   SR 2700 [S IPL7 xnzvc]"),
            DbgLine::plain(""),
            DbgLine::plain("D0 00000000   D1 00000001   D2 00C00FFC   D3 DEADBEEF"),
            DbgLine::plain("A0 00DFF000   A1 00C00000   A2 00000000   A3 00FC0000"),
            DbgLine::plain(""),
        ];
        for i in 0..20 {
            let line = format!("00FC{:04X}  MOVE.W #$4000,(A0)", 0x0E44 + i * 4);
            lines.push(if i == 0 {
                DbgLine::hilit(line)
            } else {
                DbgLine::plain(line)
            });
        }
        let data = PanelViewData::Debugger(DebuggerView {
            running: false,
            reverse_available: true,
            status: "paused frame 1234 24.68s".to_string(),
            lines,
        });
        let mut panel = DebuggerPanel::new();
        panel.entry = "C00000".to_string();
        panel.entry_active = true;
        let ui = UiState {
            menu_open: false,
            panel: Some(Panel::Debugger(panel)),
        };
        draw(
            &mut frame,
            scale,
            &ui,
            Some(UiControl::DebugStep),
            Some(&data),
            false,
            false,
            false,
            JoystickInputMode::Auto,
        );
        assert!(panel_has_title_bar(&frame, ui.panel.as_ref().unwrap()));
        save(&frame, "debugger");

        // Break tab: toggle buttons plus the breakpoint/watch listing.
        let mut frame = vec![0u8; w * h * 4];
        let mut lines: Vec<DbgLine> = (0..BREAK_TAB_HEADER_LINES)
            .map(|_| DbgLine::plain(""))
            .collect();
        lines.push(DbgLine::hilit("Breakpoint at $C033C2"));
        lines.push(DbgLine::plain(""));
        lines.push(DbgLine::plain("Breakpoints:"));
        lines.push(DbgLine::plain("  $C033C2"));
        lines.push(DbgLine::plain("Watchpoints (word):"));
        lines.push(DbgLine::plain("  $C09580  now 0012"));
        lines.push(DbgLine::plain("Register watches (stop on write):"));
        lines.push(DbgLine::plain("  DMACON ($096)"));
        let data = PanelViewData::Debugger(DebuggerView {
            running: false,
            reverse_available: true,
            status: "paused frame 1234 24.68s".to_string(),
            lines,
        });
        let mut panel = DebuggerPanel::new();
        panel.tab = DebugTab::Break;
        panel.entry = "DFF096".to_string();
        let ui = UiState {
            menu_open: false,
            panel: Some(Panel::Debugger(panel)),
        };
        draw(
            &mut frame,
            scale,
            &ui,
            Some(UiControl::DebugRegToggle),
            Some(&data),
            false,
            false,
            false,
            JoystickInputMode::Auto,
        );
        assert!(panel_has_title_bar(&frame, ui.panel.as_ref().unwrap()));
        save(&frame, "debugger-break");
    }
}
