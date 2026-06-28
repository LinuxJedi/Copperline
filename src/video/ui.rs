// SPDX-License-Identifier: GPL-3.0-or-later

//! In-window menu and overlay sub-windows (about, keyboard shortcuts,
//! gamepad calibration, debugger). Everything is drawn into the
//! presentation texture over the emulated display, styled after the
//! classic Amiga look: white menus with inverted highlights and blue
//! window title bars. This module owns layout, hit-testing and drawing;
//! `window.rs` routes events to it and builds the per-frame view data
//! (register snapshots, disassembly text) the panels render.

use super::launcher::{self, LauncherField, LauncherState, LauncherTab, RowKind};
use super::window::{
    draw_rect_bevel, fill_rect, fill_rect_blend, rgba, scale_rect, JoystickInputMode, Rect,
    BUTTON_EDGE_DARK, BUTTON_EDGE_LIGHT, BUTTON_FACE, BUTTON_FACE_HOVER,
};
use super::{font, FB_WIDTH, HOST_SHORTCUT_MODIFIER_LABEL, PRESENT_HEIGHT};
use crate::config::{MachineModel, WarpSpeed};
use crate::debugger::{BreakCond, CondOp, CondOperand};

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

const MENU_ITEM_H: usize = 20;
const MENU_PAD: usize = 3;
/// Font pixel scale labels are drawn at, and the text inset from each side of
/// the popup. The widest label is "Joystick Input  [keyboard]" (26 chars);
/// sizing the popup to it keeps every item's text (and its trailing "...")
/// inside the menu background instead of spilling past the right edge.
const MENU_TEXT_PX: usize = 2;
const MENU_TEXT_INSET: usize = 8;
const MENU_MAX_LABEL_CHARS: usize = 26;
const MENU_W: usize = 2 * MENU_TEXT_INSET + MENU_MAX_LABEL_CHARS * font::GLYPH_W * MENU_TEXT_PX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuItem {
    FrameAnalyzer,
    About,
    Shortcuts,
    Calibration,
    Debugger,
    JoystickInput,
    Warp,
    WarpLimit,
    Record,
    RecordInput,
    SaveState,
    LoadState,
    LoadRom,
    MachineConfig,
}

pub const MENU_ITEMS: [MenuItem; 14] = [
    MenuItem::MachineConfig,
    MenuItem::FrameAnalyzer,
    MenuItem::Debugger,
    MenuItem::Calibration,
    MenuItem::JoystickInput,
    MenuItem::Warp,
    MenuItem::WarpLimit,
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
    warp_speed: WarpSpeed,
    recording: bool,
    input_recording: bool,
    joystick_input_mode: JoystickInputMode,
) -> String {
    match item {
        MenuItem::FrameAnalyzer => "Frame Analyzer...".to_string(),
        MenuItem::About => "About...".to_string(),
        MenuItem::Shortcuts => "Keyboard Shortcuts...".to_string(),
        MenuItem::Calibration => "Calibrate Gamepad...".to_string(),
        MenuItem::Debugger => "Debugger...".to_string(),
        MenuItem::JoystickInput => format!("Joystick Input  [{}]", joystick_input_mode.label()),
        MenuItem::Warp if warp => "Warp Speed      [on]".to_string(),
        MenuItem::Warp => "Warp Speed     [off]".to_string(),
        // Right-pad so the closing bracket stays put as the value width
        // changes (2x/8x vs 16x/Max), aligning with the Warp Speed row above.
        MenuItem::WarpLimit => format!("Warp Limit     {:>5}", format!("[{}]", warp_speed.label())),
        MenuItem::Record if recording => "Stop Video Recording".to_string(),
        MenuItem::Record => "Record Video".to_string(),
        MenuItem::RecordInput if input_recording => "Stop Input Recording".to_string(),
        MenuItem::RecordInput => "Record Input".to_string(),
        MenuItem::SaveState => "Save State".to_string(),
        MenuItem::LoadState => "Load State...".to_string(),
        MenuItem::LoadRom => "Load Kickstart ROM...".to_string(),
        MenuItem::MachineConfig => "Machine Configuration...".to_string(),
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
#[derive(Clone)]
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

    /// The typed address: the first whitespace-separated token parsed as hex.
    /// (Poke uses a second token; the address consumers only need the first.)
    pub fn entry_addr(&self) -> Option<u32> {
        parse_hex_u32(self.entry.split_whitespace().next()?)
    }

    /// Memory poke target: two hex tokens "ADDR VALUE", as an even address and
    /// the 16-bit word to write there.
    pub fn poke_target(&self) -> Option<(u32, u16)> {
        let mut tokens = self.entry.split_whitespace();
        let addr = parse_hex_u32(tokens.next()?)?;
        let value = parse_hex_u32(tokens.next()?)?;
        Some((addr & !1, value as u16))
    }

    /// Register poke target: a register name then a hex value, e.g. "D0 1234"
    /// or "PC F80000". Returns the GDB-style register index and the value.
    pub fn reg_poke(&self) -> Option<(usize, u32)> {
        let mut tokens = self.entry.split_whitespace();
        let reg = parse_reg_name(tokens.next()?)?;
        let value = parse_hex_u32(tokens.next()?)?;
        Some((reg, value))
    }

    pub fn push_entry_char(&mut self, ch: char) {
        // Alphanumerics and spaces: hex for addresses/values, letters for
        // register names (Dn/An/PC/SR), memory operands (M<hex>), and the
        // breakpoint-condition mnemonics (EQ/NE/LT/GT/LE/GE/AND/IGN). A leading
        // or doubled space is dropped so the tokens stay clean.
        if (!ch.is_ascii_alphanumeric() && ch != ' ') || self.entry.len() >= 40 {
            return;
        }
        if ch == ' ' && (self.entry.is_empty() || self.entry.ends_with(' ')) {
            return;
        }
        self.entry.push(ch.to_ascii_uppercase());
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

/// Interactive state of the frame analyzer pane.
#[derive(Clone)]
pub struct FrameAnalyzerPanel {
    pub selected_vpos: u16,
    pub selected_hpos: u16,
}

impl FrameAnalyzerPanel {
    pub fn new() -> Self {
        Self {
            selected_vpos: 0x2C,
            selected_hpos: 0x28,
        }
    }
}

impl Default for FrameAnalyzerPanel {
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
    FrameAnalyzer(FrameAnalyzerPanel),
    /// The pre-boot machine-configuration screen. Boxed: its state is far
    /// larger than the other variants.
    Launcher(Box<LauncherState>),
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
        self.panel
            .as_ref()
            .and_then(|panel| panel_control_at(panel, pos))
    }
}

pub fn panel_control_at(panel: &Panel, pos: (i32, i32)) -> Option<UiControl> {
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
        Panel::FrameAnalyzer(_) => {
            if let Some(control) = analyzer_pick_control(rect, pos) {
                return Some(control);
            }
            for (control, button_rect) in analyzer_button_rects(rect) {
                if button_rect.contains(pos) {
                    return Some(control);
                }
            }
        }
        Panel::Launcher(state) => {
            if let Some(control) = launcher_control_at(rect, state, pos) {
                return Some(control);
            }
        }
        Panel::About | Panel::Shortcuts => {}
    }
    rect.contains(pos).then_some(UiControl::PanelBody)
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
    /// Step over a call: run the callee to completion, stopping at the
    /// instruction after a BSR/JSR/TRAP (a plain single step otherwise).
    DebugStepOver,
    /// Step out: run until the current subroutine returns to its caller.
    DebugStepOut,
    DebugStepFrame,
    DebugRunTo,
    /// Reverse-debug: step one instruction backward (reconstructed from the
    /// snapshot ring).
    DebugReverseStep,
    /// Reverse-debug: step to the previous Agnus frame counter crossing.
    DebugReverseFrame,
    /// Reverse-debug: run backward to the previous breakpoint/watch hit.
    DebugReverseRun,
    DebugMemPrev,
    DebugMemNext,
    DebugEntry,
    /// Poke: on the Memory tab write a word from the entry box's "ADDR VALUE";
    /// on the CPU tab set a register from "REG VALUE".
    DebugPoke,
    /// Break tab: toggle a PC breakpoint at the entry address.
    DebugBreakToggle,
    /// Break tab: toggle a memory word watchpoint at the entry address.
    DebugWatchToggle,
    /// Break tab: toggle a chipset-register write watch at the entry
    /// address (an offset or a full $DFFxxx address).
    DebugRegToggle,
    /// Break tab: remove all breakpoints and watchpoints.
    DebugBreaksClear,
    /// Frame analyzer: run/pause the machine while keeping the pane open.
    AnalyzerRun,
    /// Frame analyzer: step/capture one complete Agnus frame.
    AnalyzerFrame,
    /// Frame analyzer: select a slot. Coordinates are normalized to 0..1023
    /// so window.rs can map them through the current trace dimensions.
    AnalyzerPick {
        x: u16,
        y: u16,
        scanline: bool,
    },
    /// Configuration screen: pick a machine model.
    LauncherModel(MachineModel),
    /// Configuration screen: switch the category tab.
    LauncherTab(LauncherTab),
    /// Configuration screen: step a cycle/stepper field one value.
    LauncherCycle {
        field: LauncherField,
        forward: bool,
    },
    /// Configuration screen: flip a toggle field.
    LauncherToggle(LauncherField),
    /// Configuration screen: open a file dialog for a path field.
    LauncherBrowse(LauncherField),
    /// Configuration screen: clear a path field.
    LauncherClear(LauncherField),
    /// Configuration screen: add a Zorro metadata board file.
    LauncherZorroAdd,
    /// Configuration screen: remove the Zorro board at this index.
    LauncherZorroRemove(usize),
    /// Plugin config: step an enum/int option of a Zorro board.
    LauncherBoardCycle {
        board: usize,
        opt: usize,
        forward: bool,
    },
    /// Plugin config: flip a bool option of a Zorro board.
    LauncherBoardToggle {
        board: usize,
        opt: usize,
    },
    /// Plugin config: pick a file for a file-typed board option.
    LauncherBoardBrowse {
        board: usize,
        opt: usize,
    },
    /// Plugin config: revert a board option to its manifest default.
    LauncherBoardClear {
        board: usize,
        opt: usize,
    },
    /// Plugin config: focus a string/int board option for text entry.
    LauncherBoardEdit {
        board: usize,
        opt: usize,
    },
    /// Configuration screen: load a .toml configuration.
    LauncherLoad,
    /// Configuration screen: save the configuration to a .toml file.
    LauncherSave,
    /// Configuration screen: reset to the selected profile's defaults.
    LauncherDefaults,
    /// Configuration screen: build and run the configured machine.
    LauncherRun,
}

fn panel_dims(panel: &Panel) -> (usize, usize) {
    match panel {
        Panel::About => (560, 380),
        Panel::Shortcuts => (600, 396),
        Panel::Calibration(_) => (620, 372),
        Panel::Debugger(_) => (684, 520),
        Panel::FrameAnalyzer(_) => (700, 526),
        Panel::Launcher(_) => (LAUNCHER_W, LAUNCHER_H),
    }
}

fn panel_title(panel: &Panel) -> &'static str {
    match panel {
        Panel::About => "About Copperline",
        Panel::Shortcuts => "Keyboard Shortcuts",
        Panel::Calibration(_) => "Gamepad Calibration",
        Panel::Debugger(_) => "Debugger",
        Panel::FrameAnalyzer(_) => "Frame Analyzer",
        Panel::Launcher(_) => "Machine Configuration",
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

fn debug_button_rects(rect: Rect) -> [(UiControl, Rect); 13] {
    let y = rect.y + rect.h - DEBUG_BUTTON_H - 6;
    // Step Over / Step Out share a second transport row just above the main
    // one; the main row is already full edge to edge.
    let y2 = rect.y + rect.h - 2 * DEBUG_BUTTON_H - 10;
    let button = |x: usize, w: usize| Rect {
        x: rect.x + x,
        y,
        w,
        h: DEBUG_BUTTON_H,
    };
    let button2 = |x: usize, w: usize| Rect {
        x: rect.x + x,
        y: y2,
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
        (UiControl::DebugReverseFrame, button(466, 76)),
        (UiControl::DebugReverseStep, button(546, 66)),
        (UiControl::DebugReverseRun, button(616, 60)),
        // Forward step-over / step-out on the second row.
        (UiControl::DebugStepOver, button2(8, 90)),
        (UiControl::DebugStepOut, button2(102, 84)),
        // Poke (Memory tab) / Set Reg (CPU tab), on the second row.
        (UiControl::DebugPoke, button2(200, 90)),
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

fn analyzer_raster_rect(rect: Rect) -> Rect {
    Rect {
        x: rect.x + 10,
        y: rect.y + TITLE_H + 44,
        w: 448,
        h: 246,
    }
}

fn analyzer_scanline_rect(rect: Rect) -> Rect {
    Rect {
        x: rect.x + 10,
        y: rect.y + TITLE_H + 336,
        w: 512,
        h: 34,
    }
}

fn analyzer_button_rects(rect: Rect) -> [(UiControl, Rect); 2] {
    let y = rect.y + rect.h - DEBUG_BUTTON_H - 6;
    [
        (
            UiControl::AnalyzerRun,
            Rect {
                x: rect.x + 8,
                y,
                w: 70,
                h: DEBUG_BUTTON_H,
            },
        ),
        (
            UiControl::AnalyzerFrame,
            Rect {
                x: rect.x + 84,
                y,
                w: 76,
                h: DEBUG_BUTTON_H,
            },
        ),
    ]
}

fn analyzer_pick_control(rect: Rect, pos: (i32, i32)) -> Option<UiControl> {
    for (pick_rect, scanline) in [
        (analyzer_raster_rect(rect), false),
        (analyzer_scanline_rect(rect), true),
    ] {
        if !pick_rect.contains(pos) {
            continue;
        }
        let x = (pos.0 - pick_rect.x as i32).max(0) as usize;
        let y = (pos.1 - pick_rect.y as i32).max(0) as usize;
        let nx = ((x * 1023) / pick_rect.w.max(1)).min(1023) as u16;
        let ny = ((y * 1023) / pick_rect.h.max(1)).min(1023) as u16;
        return Some(UiControl::AnalyzerPick {
            x: nx,
            y: ny,
            scanline,
        });
    }
    None
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

pub struct AnalyzerMarker {
    pub vpos: u16,
    pub hpos: u16,
    pub label: String,
}

pub struct AnalyzerTraceView {
    pub frame: u64,
    pub seconds: f64,
    pub rows: usize,
    pub cols: usize,
    pub line_cck: u32,
    pub visible_start_vpos: u32,
    pub visible_lines: usize,
    pub display_hpos_start: u32,
    pub display_hpos_end: u32,
    pub owner_cck: [u64; 9],
    pub blitter_busy_cck: u64,
    pub blitter_starve_cck: [u64; 9],
    pub partial: bool,
    pub selected_vpos: usize,
    pub selected_hpos: usize,
    pub selected_owner: &'static str,
    pub selected_owner_code: u8,
    pub owners: Vec<u8>,
    pub markers: Vec<AnalyzerMarker>,
}

impl AnalyzerTraceView {
    fn owner_code_at(&self, vpos: usize, hpos: usize) -> u8 {
        if vpos >= self.rows || hpos >= self.cols {
            return b'.';
        }
        self.owners[vpos * self.cols + hpos]
    }

    fn owner_row(&self, vpos: usize) -> Option<&[u8]> {
        if vpos >= self.rows || self.cols == 0 {
            return None;
        }
        let start = vpos * self.cols;
        Some(&self.owners[start..start + self.cols])
    }
}

pub struct FrameAnalyzerView {
    pub running: bool,
    pub status: String,
    pub trace: Option<AnalyzerTraceView>,
}

pub enum PanelViewData {
    About(AboutView),
    Shortcuts,
    Calibration(CalibrationView),
    Debugger(DebuggerView),
    FrameAnalyzer(Box<FrameAnalyzerView>),
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
    warp_speed: WarpSpeed,
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
            item_rect.x + MENU_TEXT_INSET,
            item_rect.y + (MENU_ITEM_H - 16) / 2,
            &menu_item_label(
                *item,
                warp,
                warp_speed,
                recording,
                input_recording,
                joystick_input_mode,
            ),
            fg,
            MENU_TEXT_PX,
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
    let version = concat!("version ", env!("COPPERLINE_DISPLAY_VERSION"));
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

const SHORTCUT_ROWS: [(&str, &str, bool); 14] = [
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
    ("W", "Warp speed on/off", true),
    ("Shift+W", "Warp limit (2x..Max)", true),
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
        "In the debugger: S step, O over, U out, F frame, R run/pause",
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
    // Content lines. Two transport rows sit at the bottom now (the main row
    // plus the Step Over/Out row), so the text area ends above both.
    let content_top = debug_content_top(rect);
    let content_bottom = rect.y + rect.h - 2 * DEBUG_BUTTON_H - 16;
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
                    UiControl::DebugStepOver => "Step Over",
                    UiControl::DebugStepOut => "Step Out",
                    UiControl::DebugStepFrame => "Frame",
                    UiControl::DebugRunTo => "Run to $",
                    UiControl::DebugReverseStep => "< Step",
                    UiControl::DebugReverseFrame => "< Frame",
                    UiControl::DebugReverseRun => "< Run",
                    UiControl::DebugMemPrev => "<",
                    UiControl::DebugMemNext => ">",
                    UiControl::DebugPoke => {
                        if panel.tab == DebugTab::Cpu {
                            "Set Reg"
                        } else {
                            "Poke"
                        }
                    }
                    _ => "",
                };
                let enabled = match control {
                    UiControl::DebugMemPrev | UiControl::DebugMemNext => {
                        panel.tab == DebugTab::Memory
                    }
                    UiControl::DebugRunTo => panel.entry_addr().is_some(),
                    UiControl::DebugPoke => match panel.tab {
                        DebugTab::Memory => panel.poke_target().is_some(),
                        DebugTab::Cpu => panel.reg_poke().is_some(),
                        _ => false,
                    },
                    UiControl::DebugReverseStep
                    | UiControl::DebugReverseFrame
                    | UiControl::DebugReverseRun => view.reverse_available,
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

fn owner_color(code: u8) -> u32 {
    match code {
        b'R' => rgba(68, 180, 190),
        b'B' => rgba(64, 118, 230),
        b'S' => rgba(212, 84, 220),
        b'D' => rgba(190, 122, 54),
        b'A' => rgba(72, 190, 96),
        b'C' => rgba(238, 206, 72),
        b'L' => rgba(222, 78, 76),
        b'P' => rgba(230, 232, 224),
        _ => rgba(20, 22, 26),
    }
}

fn owner_name_for_code(code: u8) -> &'static str {
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

fn draw_outline(frame: &mut [u8], rect: Rect, color: u32, scale: usize) {
    if rect.w == 0 || rect.h == 0 {
        return;
    }
    fill_rect(
        frame,
        scale_rect(
            Rect {
                x: rect.x,
                y: rect.y,
                w: rect.w,
                h: 1,
            },
            scale,
        ),
        color,
        scale,
    );
    fill_rect(
        frame,
        scale_rect(
            Rect {
                x: rect.x,
                y: rect.y + rect.h.saturating_sub(1),
                w: rect.w,
                h: 1,
            },
            scale,
        ),
        color,
        scale,
    );
    fill_rect(
        frame,
        scale_rect(
            Rect {
                x: rect.x,
                y: rect.y,
                w: 1,
                h: rect.h,
            },
            scale,
        ),
        color,
        scale,
    );
    fill_rect(
        frame,
        scale_rect(
            Rect {
                x: rect.x + rect.w.saturating_sub(1),
                y: rect.y,
                w: 1,
                h: rect.h,
            },
            scale,
        ),
        color,
        scale,
    );
}

fn clipped_rect(rect: Rect, clip: Rect) -> Option<Rect> {
    let x0 = rect.x.max(clip.x);
    let y0 = rect.y.max(clip.y);
    let x1 = rect
        .x
        .saturating_add(rect.w)
        .min(clip.x.saturating_add(clip.w));
    let y1 = rect
        .y
        .saturating_add(rect.h)
        .min(clip.y.saturating_add(clip.h));
    (x1 > x0 && y1 > y0).then(|| Rect {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    })
}

fn fill_rect_clipped(frame: &mut [u8], rect: Rect, clip: Rect, color: u32, scale: usize) {
    if let Some(rect) = clipped_rect(rect, clip) {
        fill_rect(frame, scale_rect(rect, scale), color, scale);
    }
}

fn draw_outline_clipped(frame: &mut [u8], rect: Rect, clip: Rect, color: u32, scale: usize) {
    if rect.w == 0 || rect.h == 0 {
        return;
    }
    fill_rect_clipped(
        frame,
        Rect {
            x: rect.x,
            y: rect.y,
            w: rect.w,
            h: 1,
        },
        clip,
        color,
        scale,
    );
    fill_rect_clipped(
        frame,
        Rect {
            x: rect.x,
            y: rect.y + rect.h.saturating_sub(1),
            w: rect.w,
            h: 1,
        },
        clip,
        color,
        scale,
    );
    fill_rect_clipped(
        frame,
        Rect {
            x: rect.x,
            y: rect.y,
            w: 1,
            h: rect.h,
        },
        clip,
        color,
        scale,
    );
    fill_rect_clipped(
        frame,
        Rect {
            x: rect.x + rect.w.saturating_sub(1),
            y: rect.y,
            w: 1,
            h: rect.h,
        },
        clip,
        color,
        scale,
    );
}

fn trace_x(rect: Rect, hpos: usize, cols: usize) -> usize {
    rect.x + (hpos.min(cols.saturating_sub(1)) * rect.w / cols.max(1))
}

fn trace_y(rect: Rect, vpos: usize, rows: usize) -> usize {
    rect.y + (vpos.min(rows.saturating_sub(1)) * rect.h / rows.max(1))
}

fn draw_owner_heatmap(frame: &mut [u8], rect: Rect, trace: &AnalyzerTraceView, scale: usize) {
    fill_rect(frame, scale_rect(rect, scale), rgba(10, 12, 14), scale);
    for y in 0..rect.h {
        let vpos = y * trace.rows / rect.h.max(1);
        for x in 0..rect.w {
            let hpos = x * trace.cols / rect.w.max(1);
            let color = owner_color(trace.owner_code_at(vpos, hpos));
            fill_rect(
                frame,
                scale_rect(
                    Rect {
                        x: rect.x + x,
                        y: rect.y + y,
                        w: 1,
                        h: 1,
                    },
                    scale,
                ),
                color,
                scale,
            );
        }
    }

    let visible_top = trace_y(rect, trace.visible_start_vpos as usize, trace.rows);
    let visible_bottom = trace_y(
        rect,
        (trace.visible_start_vpos as usize)
            .saturating_add(trace.visible_lines)
            .min(trace.rows.saturating_sub(1)),
        trace.rows,
    )
    .max(visible_top + 1);
    let display_left = trace_x(rect, trace.display_hpos_start as usize, trace.cols);
    let display_right =
        trace_x(rect, trace.display_hpos_end as usize, trace.cols).max(display_left + 1);
    draw_outline(
        frame,
        Rect {
            x: display_left,
            y: visible_top,
            w: display_right.saturating_sub(display_left).max(1),
            h: visible_bottom.saturating_sub(visible_top).max(1),
        },
        rgba(238, 238, 232),
        scale,
    );

    for marker in trace.markers.iter().take(80) {
        let x = trace_x(rect, marker.hpos as usize, trace.cols);
        let y = trace_y(rect, marker.vpos as usize, trace.rows);
        fill_rect_clipped(
            frame,
            Rect {
                x: x.saturating_sub(1),
                y,
                w: 3,
                h: 1,
            },
            rect,
            PANEL_TEXT_ACCENT,
            scale,
        );
        fill_rect_clipped(
            frame,
            Rect {
                x,
                y: y.saturating_sub(1),
                w: 1,
                h: 3,
            },
            rect,
            PANEL_TEXT_ACCENT,
            scale,
        );
    }

    let sx = trace_x(rect, trace.selected_hpos, trace.cols);
    let sy = trace_y(rect, trace.selected_vpos, trace.rows);
    draw_outline_clipped(
        frame,
        Rect {
            x: sx.saturating_sub(3),
            y: sy.saturating_sub(3),
            w: 7,
            h: 7,
        },
        rect,
        PANEL_TEXT_HILIGHT,
        scale,
    );
    draw_outline(frame, rect, BUTTON_EDGE_LIGHT, scale);
}

fn draw_scanline_strip(frame: &mut [u8], rect: Rect, trace: &AnalyzerTraceView, scale: usize) {
    fill_rect(frame, scale_rect(rect, scale), rgba(10, 12, 14), scale);
    if let Some(row) = trace.owner_row(trace.selected_vpos) {
        for x in 0..rect.w {
            let hpos = x * trace.cols / rect.w.max(1);
            let color = owner_color(row[hpos.min(row.len().saturating_sub(1))]);
            fill_rect(
                frame,
                scale_rect(
                    Rect {
                        x: rect.x + x,
                        y: rect.y + 8,
                        w: 1,
                        h: rect.h.saturating_sub(14),
                    },
                    scale,
                ),
                color,
                scale,
            );
        }
    }
    let sx = trace_x(rect, trace.selected_hpos, trace.cols);
    fill_rect(
        frame,
        scale_rect(
            Rect {
                x: sx,
                y: rect.y,
                w: 1,
                h: rect.h,
            },
            scale,
        ),
        PANEL_TEXT_HILIGHT,
        scale,
    );
    draw_outline(frame, rect, BUTTON_EDGE_LIGHT, scale);
}

fn draw_owner_counters(
    frame: &mut [u8],
    x: usize,
    mut y: usize,
    trace: &AnalyzerTraceView,
    scale: usize,
) {
    let total: u64 = trace.owner_cck.iter().sum();
    draw_panel_text(frame, x, y, "Owner cck", PANEL_TEXT_HILIGHT, 1, scale);
    y += 12;
    for (idx, name) in crate::bus::CHIP_BUS_OWNER_NAMES.iter().enumerate() {
        let cck = trace.owner_cck[idx];
        if cck == 0 {
            continue;
        }
        let pct = if total == 0 {
            0.0
        } else {
            cck as f64 * 100.0 / total as f64
        };
        let code = match idx {
            0 => b'R',
            1 => b'B',
            2 => b'S',
            3 => b'D',
            4 => b'A',
            5 => b'C',
            6 => b'L',
            7 => b'P',
            _ => b'.',
        };
        fill_rect(
            frame,
            scale_rect(
                Rect {
                    x,
                    y: y + 2,
                    w: 8,
                    h: 8,
                },
                scale,
            ),
            owner_color(code),
            scale,
        );
        draw_panel_text(
            frame,
            x + 14,
            y,
            &format!("{name:<8} {cck:>5} {pct:>4.1}%"),
            PANEL_TEXT,
            1,
            scale,
        );
        y += 12;
    }
    if trace.blitter_busy_cck != 0 {
        y += 4;
        let blit_grant = trace.owner_cck[6];
        let pct = blit_grant as f64 * 100.0 / trace.blitter_busy_cck as f64;
        draw_panel_text(
            frame,
            x,
            y,
            &format!("blitter grant {pct:>4.1}%"),
            PANEL_TEXT_ACCENT,
            1,
            scale,
        );
        y += 12;
        let total_starve: u64 = trace.blitter_starve_cck.iter().sum();
        draw_panel_text(
            frame,
            x,
            y,
            &format!("blitter wait {total_starve:>5}"),
            PANEL_TEXT_ACCENT,
            1,
            scale,
        );
        y += 12;
        for (idx, name) in crate::bus::CHIP_BUS_OWNER_NAMES.iter().enumerate() {
            let cck = trace.blitter_starve_cck[idx];
            if cck == 0 {
                continue;
            }
            draw_panel_text(
                frame,
                x,
                y,
                &format!("{name:<8} {cck:>5}"),
                PANEL_TEXT_DIM,
                1,
                scale,
            );
            y += 12;
        }
    }
}

fn draw_frame_analyzer(
    frame: &mut [u8],
    rect: Rect,
    _panel: &FrameAnalyzerPanel,
    view: &FrameAnalyzerView,
    hover: Option<UiControl>,
    scale: usize,
) {
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

    let Some(trace) = &view.trace else {
        let mut y = rect.y + TITLE_H + 36;
        for line in [
            "No chip-bus trace captured yet.",
            "Press Frame to record one full Agnus frame, or Run to collect live frames.",
            "The analyzer records hpos/vpos ownership, including overscan and blanking.",
        ] {
            draw_panel_text(frame, rect.x + 24, y, line, PANEL_TEXT, 1, scale);
            y += 16;
        }
        for (control, button_rect) in analyzer_button_rects(rect) {
            let label = match control {
                UiControl::AnalyzerRun if view.running => "Pause",
                UiControl::AnalyzerRun => "Run",
                _ => "Frame",
            };
            draw_text_button(
                frame,
                button_rect,
                label,
                true,
                hover == Some(control),
                scale,
            );
        }
        return;
    };

    let header = format!(
        "frame {}  {:.3}s  {} lines x {} cck{}{}",
        trace.frame,
        trace.seconds,
        trace.rows,
        trace.line_cck,
        if trace.cols as u32 != trace.line_cck {
            " sampled"
        } else {
            ""
        },
        if trace.partial { "  partial" } else { "" }
    );
    draw_panel_text(
        frame,
        rect.x + 10,
        rect.y + TITLE_H + 10,
        &header,
        PANEL_TEXT,
        1,
        scale,
    );
    draw_panel_text(
        frame,
        rect.x + 10,
        rect.y + TITLE_H + 24,
        "full raster: x=hpos colour clocks, y=vpos beam lines; white box is captured display",
        PANEL_TEXT_DIM,
        1,
        scale,
    );

    let raster = analyzer_raster_rect(rect);
    draw_owner_heatmap(frame, raster, trace, scale);
    let counters_x = raster.x + raster.w + 16;
    draw_owner_counters(frame, counters_x, raster.y, trace, scale);

    let selected = format!(
        "selected v={:03} h={:03}  owner={} ({})",
        trace.selected_vpos,
        trace.selected_hpos,
        trace.selected_owner,
        trace.selected_owner_code as char
    );
    draw_panel_text(
        frame,
        rect.x + 10,
        raster.y + raster.h + 10,
        &selected,
        PANEL_TEXT_HILIGHT,
        1,
        scale,
    );
    if let Some(marker) = trace.markers.iter().find(|m| {
        usize::from(m.vpos) == trace.selected_vpos && usize::from(m.hpos) == trace.selected_hpos
    }) {
        draw_panel_text(
            frame,
            rect.x + 10,
            raster.y + raster.h + 22,
            &marker.label,
            PANEL_TEXT_ACCENT,
            1,
            scale,
        );
    }

    let scanline = analyzer_scanline_rect(rect);
    draw_panel_text(
        frame,
        scanline.x,
        scanline.y - 14,
        "selected scanline",
        PANEL_TEXT_DIM,
        1,
        scale,
    );
    draw_scanline_strip(frame, scanline, trace, scale);

    let mut y = scanline.y + scanline.h + 14;
    draw_panel_text(frame, rect.x + 10, y, "Legend", PANEL_TEXT_DIM, 1, scale);
    let mut x = rect.x + 66;
    for code in [b'R', b'B', b'S', b'D', b'A', b'C', b'L', b'P', b'.'] {
        fill_rect(
            frame,
            scale_rect(
                Rect {
                    x,
                    y: y + 2,
                    w: 8,
                    h: 8,
                },
                scale,
            ),
            owner_color(code),
            scale,
        );
        draw_panel_text(
            frame,
            x + 12,
            y,
            owner_name_for_code(code),
            PANEL_TEXT,
            1,
            scale,
        );
        x += if code == b'.' { 54 } else { 82 };
    }
    y += 18;
    let marker_text = format!("register markers shown: {}", trace.markers.len().min(80));
    draw_panel_text(
        frame,
        rect.x + 10,
        y,
        &marker_text,
        PANEL_TEXT_DIM,
        1,
        scale,
    );

    for (control, button_rect) in analyzer_button_rects(rect) {
        let label = match control {
            UiControl::AnalyzerRun if view.running => "Pause",
            UiControl::AnalyzerRun => "Run",
            _ => "Frame",
        };
        draw_text_button(
            frame,
            button_rect,
            label,
            true,
            hover == Some(control),
            scale,
        );
    }
}

// ---------------------------------------------------------------------------
// Machine-configuration (launcher) panel
// ---------------------------------------------------------------------------

const LAUNCHER_W: usize = 700;
const LAUNCHER_H: usize = 520;
const LAUNCH_MARGIN: usize = 8;
const LAUNCH_MODEL_H: usize = 22;
const LAUNCH_MODEL_GAP: usize = 4;
/// Machines per row in the selector grid before it wraps; the grid rebalances
/// so the buttons fill the width (eight models fit one row today, more wrap to
/// two balanced rows -- room for the A3000/A4000 and beyond).
const LAUNCH_MODEL_MAX_PER_ROW: usize = 8;
/// Width of the left-hand vertical category-tab column.
const LAUNCH_SIDEBAR_W: usize = 116;
const LAUNCH_TAB_H: usize = 26;
const LAUNCH_TAB_GAP: usize = 2;
const LAUNCH_ROW_H: usize = 26;
/// Label column width inside the settings pane (before a row's control).
const LAUNCH_LABEL_W: usize = 150;
const LAUNCH_ARROW_W: usize = 24;
const LAUNCH_VALUE_W: usize = 132;
const LAUNCH_TOGGLE_W: usize = 64;
const LAUNCH_ACTION_W: usize = 84;
const LAUNCH_ACTION_H: usize = 22;
const LAUNCH_BROWSE_W: usize = 66;
const LAUNCH_CLEAR_W: usize = 54;
const LAUNCH_REMOVE_W: usize = 70;
const LAUNCH_CONTROL_H: usize = 20;

fn launcher_model_top(rect: Rect) -> usize {
    rect.y + TITLE_H + 8
}

/// (rows, columns) of the machine-selector grid, balanced so the buttons fill
/// the width evenly however many models there are.
fn launcher_model_grid() -> (usize, usize) {
    let count = launcher::MODELS.len();
    let rows = count.div_ceil(LAUNCH_MODEL_MAX_PER_ROW).max(1);
    (rows, count.div_ceil(rows))
}

fn launcher_model_rect(rect: Rect, i: usize) -> Rect {
    let (_, per_row) = launcher_model_grid();
    let avail = rect.w - 2 * LAUNCH_MARGIN;
    let w = (avail - (per_row - 1) * LAUNCH_MODEL_GAP) / per_row;
    let (row, col) = (i / per_row, i % per_row);
    Rect {
        x: rect.x + LAUNCH_MARGIN + col * (w + LAUNCH_MODEL_GAP),
        y: launcher_model_top(rect) + row * (LAUNCH_MODEL_H + LAUNCH_MODEL_GAP),
        w,
        h: LAUNCH_MODEL_H,
    }
}

fn launcher_model_strip_height() -> usize {
    let (rows, _) = launcher_model_grid();
    rows * (LAUNCH_MODEL_H + LAUNCH_MODEL_GAP)
}

/// Top of the configuration area (the vertical tab column and the settings
/// pane both start here), below the machine grid and its divider.
fn launcher_content_top(rect: Rect) -> usize {
    launcher_model_top(rect) + launcher_model_strip_height() + 12
}

/// A category tab in the left sidebar.
fn launcher_tab_rect(rect: Rect, i: usize) -> Rect {
    Rect {
        x: rect.x + LAUNCH_MARGIN,
        y: launcher_content_top(rect) + i * (LAUNCH_TAB_H + LAUNCH_TAB_GAP),
        w: LAUNCH_SIDEBAR_W,
        h: LAUNCH_TAB_H,
    }
}

/// Left edge of the settings pane (right of the tab column).
fn launcher_pane_x(rect: Rect) -> usize {
    rect.x + LAUNCH_MARGIN + LAUNCH_SIDEBAR_W + 12
}

/// X of a settings row's control column (after its label).
fn launcher_control_x(rect: Rect) -> usize {
    launcher_pane_x(rect) + LAUNCH_LABEL_W
}

fn launcher_row_y(rect: Rect, i: usize) -> usize {
    launcher_content_top(rect) + i * LAUNCH_ROW_H
}

fn launcher_action_y(rect: Rect) -> usize {
    rect.y + rect.h - LAUNCH_ACTION_H - 8
}

fn launcher_status_y(rect: Rect) -> usize {
    launcher_action_y(rect).saturating_sub(16)
}

/// (prev arrow, value field, next arrow) for a cycle row.
fn launcher_cycle_rects(rect: Rect, row_y: usize) -> (Rect, Rect, Rect) {
    let y = row_y + 2;
    let cx = launcher_control_x(rect);
    let prev = Rect {
        x: cx,
        y,
        w: LAUNCH_ARROW_W,
        h: LAUNCH_CONTROL_H,
    };
    let value = Rect {
        x: prev.x + LAUNCH_ARROW_W,
        y,
        w: LAUNCH_VALUE_W,
        h: LAUNCH_CONTROL_H,
    };
    let next = Rect {
        x: value.x + LAUNCH_VALUE_W,
        y,
        w: LAUNCH_ARROW_W,
        h: LAUNCH_CONTROL_H,
    };
    (prev, value, next)
}

fn launcher_toggle_rect(rect: Rect, row_y: usize) -> Rect {
    Rect {
        x: launcher_control_x(rect),
        y: row_y + 2,
        w: LAUNCH_TOGGLE_W,
        h: LAUNCH_CONTROL_H,
    }
}

/// (Browse, Clear) buttons for a path row, right-aligned in the settings pane.
fn launcher_path_rects(rect: Rect, row_y: usize) -> (Rect, Rect) {
    let y = row_y + 2;
    let clear = Rect {
        x: rect.x + rect.w - LAUNCH_MARGIN - LAUNCH_CLEAR_W,
        y,
        w: LAUNCH_CLEAR_W,
        h: LAUNCH_CONTROL_H,
    };
    let browse = Rect {
        x: clear.x - 4 - LAUNCH_BROWSE_W,
        y,
        w: LAUNCH_BROWSE_W,
        h: LAUNCH_CONTROL_H,
    };
    (browse, clear)
}

fn launcher_action_rects(rect: Rect) -> [(UiControl, Rect); 4] {
    let y = launcher_action_y(rect);
    let load = Rect {
        x: rect.x + LAUNCH_MARGIN,
        y,
        w: LAUNCH_ACTION_W,
        h: LAUNCH_ACTION_H,
    };
    let save = Rect {
        x: load.x + LAUNCH_ACTION_W + 6,
        y,
        w: LAUNCH_ACTION_W,
        h: LAUNCH_ACTION_H,
    };
    let run = Rect {
        x: rect.x + rect.w - LAUNCH_MARGIN - LAUNCH_ACTION_W,
        y,
        w: LAUNCH_ACTION_W,
        h: LAUNCH_ACTION_H,
    };
    let defaults = Rect {
        x: run.x - 6 - LAUNCH_ACTION_W,
        y,
        w: LAUNCH_ACTION_W,
        h: LAUNCH_ACTION_H,
    };
    [
        (UiControl::LauncherLoad, load),
        (UiControl::LauncherSave, save),
        (UiControl::LauncherDefaults, defaults),
        (UiControl::LauncherRun, run),
    ]
}

/// One drawable/clickable item in the Zorro tab. The flat layout list keeps
/// drawing and hit-testing in exact sync (immediate-mode UI).
#[derive(Clone, Copy)]
enum ZorroItem {
    Header(usize),
    Option { board: usize, opt: usize },
}

/// Flatten the Zorro boards into (content-row, item) pairs. Row 0 is the Add
/// button, pinned to the top; each board header and its option rows follow.
fn launcher_zorro_layout(setup: &launcher::MachineSetup) -> Vec<(usize, ZorroItem)> {
    let mut items = Vec::new();
    let mut row = 1;
    for (i, board) in setup.zorro_boards().iter().enumerate() {
        items.push((row, ZorroItem::Header(i)));
        row += 1;
        for opt in 0..board.options().len() {
            items.push((row, ZorroItem::Option { board: i, opt }));
            row += 1;
        }
    }
    items
}

/// The Remove button for a board header drawn at content `row`.
fn launcher_zorro_remove_rect(rect: Rect, row: usize) -> Rect {
    Rect {
        x: rect.x + rect.w - LAUNCH_MARGIN - LAUNCH_REMOVE_W,
        y: launcher_row_y(rect, row) + 2,
        w: LAUNCH_REMOVE_W,
        h: LAUNCH_CONTROL_H,
    }
}

/// The clickable value box for a string option at `row_y` (control column to
/// the right margin).
fn launcher_board_value_rect(rect: Rect, row_y: usize) -> Rect {
    let x = launcher_control_x(rect);
    let right = rect.x + rect.w - LAUNCH_MARGIN;
    Rect {
        x,
        y: row_y + 2,
        w: right.saturating_sub(x),
        h: LAUNCH_CONTROL_H,
    }
}

fn launcher_zorro_add_rect(rect: Rect) -> Rect {
    Rect {
        x: launcher_pane_x(rect),
        y: launcher_row_y(rect, 0) + 2,
        w: 130,
        h: LAUNCH_ACTION_H,
    }
}

fn launcher_action_label(control: UiControl) -> &'static str {
    match control {
        UiControl::LauncherLoad => "Load...",
        UiControl::LauncherSave => "Save...",
        UiControl::LauncherDefaults => "Defaults",
        UiControl::LauncherRun => "Run",
        _ => "",
    }
}

/// Hit-test the configuration panel. Returns the control under `pos`, or `None`
/// to let the caller swallow the click on the panel body.
fn launcher_control_at(rect: Rect, state: &LauncherState, pos: (i32, i32)) -> Option<UiControl> {
    for (i, &model) in launcher::MODELS.iter().enumerate() {
        if launcher_model_rect(rect, i).contains(pos) {
            return Some(UiControl::LauncherModel(model));
        }
    }
    for (i, &tab) in launcher::TABS.iter().enumerate() {
        if launcher_tab_rect(rect, i).contains(pos) {
            return Some(UiControl::LauncherTab(tab));
        }
    }
    if state.tab == LauncherTab::Zorro {
        use crate::zorro::ConfigOptionKind as K;
        for (row, item) in launcher_zorro_layout(&state.setup) {
            let row_y = launcher_row_y(rect, row);
            match item {
                ZorroItem::Header(i) => {
                    if launcher_zorro_remove_rect(rect, row).contains(pos) {
                        return Some(UiControl::LauncherZorroRemove(i));
                    }
                }
                ZorroItem::Option { board, opt } => {
                    match &state.setup.zorro_boards()[board].options()[opt].kind {
                        K::Bool => {
                            if launcher_toggle_rect(rect, row_y).contains(pos) {
                                return Some(UiControl::LauncherBoardToggle { board, opt });
                            }
                        }
                        K::Enum(_) | K::Int => {
                            let (prev, _v, next) = launcher_cycle_rects(rect, row_y);
                            if prev.contains(pos) {
                                return Some(UiControl::LauncherBoardCycle {
                                    board,
                                    opt,
                                    forward: false,
                                });
                            }
                            if next.contains(pos) {
                                return Some(UiControl::LauncherBoardCycle {
                                    board,
                                    opt,
                                    forward: true,
                                });
                            }
                        }
                        K::File => {
                            let (browse, clear) = launcher_path_rects(rect, row_y);
                            if browse.contains(pos) {
                                return Some(UiControl::LauncherBoardBrowse { board, opt });
                            }
                            if clear.contains(pos) {
                                return Some(UiControl::LauncherBoardClear { board, opt });
                            }
                        }
                        K::String => {
                            if launcher_board_value_rect(rect, row_y).contains(pos) {
                                return Some(UiControl::LauncherBoardEdit { board, opt });
                            }
                        }
                    }
                }
            }
        }
        if launcher_zorro_add_rect(rect).contains(pos) {
            return Some(UiControl::LauncherZorroAdd);
        }
    } else {
        for (i, r) in launcher::rows(state.tab).iter().enumerate() {
            if !state.setup.applies(r.field) {
                continue;
            }
            let row_y = launcher_row_y(rect, i);
            match r.kind {
                RowKind::Cycle => {
                    let (prev, _value, next) = launcher_cycle_rects(rect, row_y);
                    if prev.contains(pos) {
                        return Some(UiControl::LauncherCycle {
                            field: r.field,
                            forward: false,
                        });
                    }
                    if next.contains(pos) {
                        return Some(UiControl::LauncherCycle {
                            field: r.field,
                            forward: true,
                        });
                    }
                }
                RowKind::Toggle => {
                    if launcher_toggle_rect(rect, row_y).contains(pos) {
                        return Some(UiControl::LauncherToggle(r.field));
                    }
                }
                RowKind::Path => {
                    let (browse, clear) = launcher_path_rects(rect, row_y);
                    if browse.contains(pos) {
                        return Some(UiControl::LauncherBrowse(r.field));
                    }
                    if clear.contains(pos) {
                        return Some(UiControl::LauncherClear(r.field));
                    }
                }
            }
        }
    }
    for (control, button_rect) in launcher_action_rects(rect) {
        if button_rect.contains(pos) {
            return Some(control);
        }
    }
    None
}

/// Truncate `text` (already a short file name) to fit `avail_px`, appending a
/// `~` marker when clipped.
fn truncate_to_width(text: &str, avail_px: usize) -> String {
    let max_chars = avail_px / font::GLYPH_W;
    let len = text.chars().count();
    if len <= max_chars {
        return text.to_string();
    }
    if max_chars <= 1 {
        return String::new();
    }
    let kept: String = text.chars().take(max_chars - 1).collect();
    format!("{kept}~")
}

/// A model-selector / tab button: a flat bevel that fills with the title-bar
/// blue when active/selected. Tabs label left, model buttons centred.
fn draw_launcher_chip(
    frame: &mut [u8],
    rect: Rect,
    label: &str,
    active: bool,
    hover: bool,
    align_left: bool,
    scale: usize,
) {
    let face = if active {
        PANEL_TITLE_BG
    } else if hover {
        BUTTON_FACE_HOVER
    } else {
        BUTTON_FACE
    };
    let scaled = scale_rect(rect, scale);
    fill_rect(frame, scaled, face, scale);
    draw_rect_bevel(frame, scaled, BUTTON_EDGE_LIGHT, BUTTON_EDGE_DARK, scale);
    let color = if active {
        PANEL_TITLE_TEXT
    } else {
        BUTTON_TEXT
    };
    let text_w = label.chars().count() * font::GLYPH_W;
    let x = if align_left {
        rect.x + 8
    } else {
        rect.x + rect.w.saturating_sub(text_w) / 2
    };
    let y = rect.y + rect.h.saturating_sub(font::GLYPH_H) / 2;
    draw_panel_text(frame, x, y, label, color, 1, scale);
}

fn draw_launcher_row(
    frame: &mut [u8],
    rect: Rect,
    setup: &launcher::MachineSetup,
    r: &launcher::Row,
    i: usize,
    hover: Option<UiControl>,
    scale: usize,
) {
    let row_y = launcher_row_y(rect, i);
    let reason = setup.disabled_reason(r.field);
    let label_color = if reason.is_none() {
        PANEL_TEXT
    } else {
        PANEL_TEXT_DIM
    };
    draw_panel_text(
        frame,
        launcher_pane_x(rect),
        row_y + 8,
        r.label,
        label_color,
        1,
        scale,
    );
    // Greyed: explain why instead of drawing controls (e.g. "needs 32-bit CPU").
    if let Some(reason) = reason {
        draw_panel_text(
            frame,
            launcher_control_x(rect),
            row_y + 8,
            reason,
            PANEL_TEXT_DIM,
            1,
            scale,
        );
        return;
    }
    match r.kind {
        RowKind::Cycle => {
            let (prev, value, next) = launcher_cycle_rects(rect, row_y);
            draw_text_button(
                frame,
                prev,
                "<",
                true,
                hover
                    == Some(UiControl::LauncherCycle {
                        field: r.field,
                        forward: false,
                    }),
                scale,
            );
            draw_text_button(
                frame,
                next,
                ">",
                true,
                hover
                    == Some(UiControl::LauncherCycle {
                        field: r.field,
                        forward: true,
                    }),
                scale,
            );
            let text = setup.value_label(r.field);
            let tw = text.chars().count() * font::GLYPH_W;
            let tx = value.x + value.w.saturating_sub(tw) / 2;
            draw_panel_text(frame, tx, value.y + 6, &text, PANEL_TEXT_HILIGHT, 1, scale);
        }
        RowKind::Toggle => {
            let button = launcher_toggle_rect(rect, row_y);
            let label = if setup.toggle_value(r.field) {
                "On"
            } else {
                "Off"
            };
            draw_text_button(
                frame,
                button,
                label,
                true,
                hover == Some(UiControl::LauncherToggle(r.field)),
                scale,
            );
        }
        RowKind::Path => {
            let (browse, clear) = launcher_path_rects(rect, row_y);
            let value_x = launcher_control_x(rect);
            let avail = browse.x.saturating_sub(value_x + 8);
            let text = truncate_to_width(&setup.value_label(r.field), avail);
            draw_panel_text(frame, value_x, browse.y + 6, &text, PANEL_TEXT, 1, scale);
            draw_text_button(
                frame,
                browse,
                "Browse",
                true,
                hover == Some(UiControl::LauncherBrowse(r.field)),
                scale,
            );
            draw_text_button(
                frame,
                clear,
                "Clear",
                true,
                hover == Some(UiControl::LauncherClear(r.field)),
                scale,
            );
        }
    }
}

fn draw_launcher_zorro(
    frame: &mut [u8],
    rect: Rect,
    state: &LauncherState,
    hover: Option<UiControl>,
    scale: usize,
) {
    let setup = &state.setup;
    let pane_x = launcher_pane_x(rect);
    // Add button pinned to the top of the pane; the board list (or the empty
    // note) sits below it.
    draw_text_button(
        frame,
        launcher_zorro_add_rect(rect),
        "Add board...",
        true,
        hover == Some(UiControl::LauncherZorroAdd),
        scale,
    );
    if setup.zorro_boards().is_empty() {
        draw_panel_text(
            frame,
            pane_x,
            launcher_row_y(rect, 1) + 8,
            "No extra Zorro boards configured.",
            PANEL_TEXT_DIM,
            1,
            scale,
        );
    }
    for (row, item) in launcher_zorro_layout(setup) {
        let row_y = launcher_row_y(rect, row);
        match item {
            ZorroItem::Header(i) => {
                let board = &setup.zorro_boards()[i];
                let remove = launcher_zorro_remove_rect(rect, row);
                let name = truncate_to_width(&board.name(), remove.x.saturating_sub(pane_x + 8));
                draw_panel_text(frame, pane_x, row_y + 8, &name, PANEL_TEXT, 1, scale);
                draw_text_button(
                    frame,
                    remove,
                    "Remove",
                    true,
                    hover == Some(UiControl::LauncherZorroRemove(i)),
                    scale,
                );
            }
            ZorroItem::Option { board, opt } => {
                draw_launcher_board_option(frame, rect, state, board, opt, row_y, hover, scale);
            }
        }
    }
}

/// Draw one plugin config-option row (indented under its board): a label plus
/// the widget its kind calls for.
fn draw_launcher_board_option(
    frame: &mut [u8],
    rect: Rect,
    state: &LauncherState,
    board: usize,
    opt: usize,
    row_y: usize,
    hover: Option<UiControl>,
    scale: usize,
) {
    use crate::zorro::ConfigOptionKind as K;
    let setup = &state.setup;
    let option = &setup.zorro_boards()[board].options()[opt];
    // Indented label.
    let label_x = launcher_pane_x(rect) + 12;
    let label = truncate_to_width(
        &option.label,
        launcher_control_x(rect).saturating_sub(label_x + 6),
    );
    draw_panel_text(frame, label_x, row_y + 8, &label, PANEL_TEXT, 1, scale);

    let value = setup.zorro_boards()[board].value(opt);
    match &option.kind {
        K::Bool => {
            let on = value.trim().eq_ignore_ascii_case("true");
            draw_text_button(
                frame,
                launcher_toggle_rect(rect, row_y),
                if on { "On" } else { "Off" },
                true,
                hover == Some(UiControl::LauncherBoardToggle { board, opt }),
                scale,
            );
        }
        K::Enum(_) | K::Int => {
            let (prev, val, next) = launcher_cycle_rects(rect, row_y);
            draw_text_button(
                frame,
                prev,
                "<",
                true,
                hover
                    == Some(UiControl::LauncherBoardCycle {
                        board,
                        opt,
                        forward: false,
                    }),
                scale,
            );
            let shown = truncate_to_width(&value, val.w.saturating_sub(8));
            draw_panel_text(
                frame,
                val.x + 6,
                row_y + 8,
                &shown,
                PANEL_TEXT_HILIGHT,
                1,
                scale,
            );
            draw_text_button(
                frame,
                next,
                ">",
                true,
                hover
                    == Some(UiControl::LauncherBoardCycle {
                        board,
                        opt,
                        forward: true,
                    }),
                scale,
            );
        }
        K::File => {
            let (browse, clear) = launcher_path_rects(rect, row_y);
            let shown = if value.is_empty() {
                "(none)".to_string()
            } else {
                std::path::Path::new(&value)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or(value.clone())
            };
            let avail = browse.x.saturating_sub(launcher_control_x(rect) + 6);
            let shown = truncate_to_width(&shown, avail);
            draw_panel_text(
                frame,
                launcher_control_x(rect),
                row_y + 8,
                &shown,
                PANEL_TEXT,
                1,
                scale,
            );
            draw_text_button(
                frame,
                browse,
                "Browse",
                true,
                hover == Some(UiControl::LauncherBoardBrowse { board, opt }),
                scale,
            );
            draw_text_button(
                frame,
                clear,
                "Clear",
                true,
                hover == Some(UiControl::LauncherBoardClear { board, opt }),
                scale,
            );
        }
        K::String => {
            let editing = state.editing() == Some((board, opt));
            let vbox = launcher_board_value_rect(rect, row_y);
            draw_rect_bevel(
                frame,
                scale_rect(vbox, scale),
                BUTTON_EDGE_DARK,
                BUTTON_EDGE_LIGHT,
                scale,
            );
            let text = if editing {
                format!("{}_", state.edit_buffer())
            } else {
                value.clone()
            };
            let color = if editing {
                PANEL_TEXT_HILIGHT
            } else {
                PANEL_TEXT
            };
            let shown = truncate_to_width(&text, vbox.w.saturating_sub(8));
            draw_panel_text(frame, vbox.x + 4, row_y + 8, &shown, color, 1, scale);
        }
    }
}

/// A thin divider line.
fn draw_launcher_divider(frame: &mut [u8], rect: Rect, scale: usize) {
    fill_rect(frame, scale_rect(rect, scale), BUTTON_EDGE_DARK, scale);
}

fn draw_launcher(
    frame: &mut [u8],
    rect: Rect,
    state: &LauncherState,
    hover: Option<UiControl>,
    scale: usize,
) {
    let setup = &state.setup;
    // Machine selector grid. The A500 highlights when no profile is chosen
    // (a no-profile machine is the A500 defaults).
    let selected_model = setup.selected_model();
    for (i, &model) in launcher::MODELS.iter().enumerate() {
        draw_launcher_chip(
            frame,
            launcher_model_rect(rect, i),
            launcher::model_label(model),
            selected_model == model,
            hover == Some(UiControl::LauncherModel(model)),
            false,
            scale,
        );
    }
    // Divider under the machine grid; vertical divider between the tab column
    // and the settings pane.
    let content_top = launcher_content_top(rect);
    draw_launcher_divider(
        frame,
        Rect {
            x: rect.x + LAUNCH_MARGIN,
            y: content_top - 6,
            w: rect.w - 2 * LAUNCH_MARGIN,
            h: 1,
        },
        scale,
    );
    draw_launcher_divider(
        frame,
        Rect {
            x: rect.x + LAUNCH_MARGIN + LAUNCH_SIDEBAR_W + 5,
            y: content_top,
            w: 1,
            h: launcher_status_y(rect).saturating_sub(content_top + 4),
        },
        scale,
    );
    // Vertical category-tab column.
    for (i, &tab) in launcher::TABS.iter().enumerate() {
        draw_launcher_chip(
            frame,
            launcher_tab_rect(rect, i),
            tab.label(),
            state.tab == tab,
            hover == Some(UiControl::LauncherTab(tab)),
            true,
            scale,
        );
    }
    // Active tab content in the settings pane.
    if state.tab == LauncherTab::Zorro {
        draw_launcher_zorro(frame, rect, state, hover, scale);
    } else {
        for (i, r) in launcher::rows(state.tab).iter().enumerate() {
            draw_launcher_row(frame, rect, setup, r, i, hover, scale);
        }
    }
    // Status / error line.
    if let Some(status) = &state.status {
        let color = if status.error {
            PANEL_TEXT_ACCENT
        } else {
            PANEL_TEXT_HILIGHT
        };
        draw_panel_text(
            frame,
            rect.x + 10,
            launcher_status_y(rect),
            &status.text,
            color,
            1,
            scale,
        );
    }
    // Action bar.
    for (control, button_rect) in launcher_action_rects(rect) {
        draw_text_button(
            frame,
            button_rect,
            launcher_action_label(control),
            true,
            hover == Some(control),
            scale,
        );
    }
}

pub fn draw_panel_layer(
    frame: &mut [u8],
    texture_scale: usize,
    panel: &Panel,
    hover: Option<UiControl>,
    data: Option<&PanelViewData>,
) {
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
        (Panel::FrameAnalyzer(panel_state), Some(PanelViewData::FrameAnalyzer(view))) => {
            draw_frame_analyzer(frame, rect, panel_state, view, hover, texture_scale)
        }
        // The configuration panel is self-contained (its state holds everything
        // it renders), so it needs no per-frame view-data snapshot.
        (Panel::Launcher(state), _) => draw_launcher(frame, rect, state, hover, texture_scale),
        _ => {}
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
    warp_speed: WarpSpeed,
    recording: bool,
    input_recording: bool,
    joystick_input_mode: JoystickInputMode,
) {
    if let Some(panel) = &ui.panel {
        draw_panel_layer(frame, texture_scale, panel, hover, data);
    }
    if ui.menu_open {
        draw_menu(
            frame,
            hover,
            warp,
            warp_speed,
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

/// Parse a 68000 register name into the GDB-style index used by
/// `debug_set_register`: D0-D7 -> 0-7, A0-A7 -> 8-15, SR -> 16, PC -> 17,
/// with SP an alias for A7.
fn parse_reg_name(token: &str) -> Option<usize> {
    let token = token.to_ascii_uppercase();
    match token.as_str() {
        "PC" => return Some(17),
        "SR" => return Some(16),
        "SP" => return Some(15),
        _ => {}
    }
    if token.len() < 2 {
        return None;
    }
    let (kind, idx) = token.split_at(1);
    let n: usize = idx.parse().ok()?;
    match kind {
        "D" if n <= 7 => Some(n),
        "A" if n <= 7 => Some(8 + n),
        _ => None,
    }
}

/// Parse a breakpoint spec from the entry box: "ADDR [LHS OP RHS] [IGN N]".
/// Returns the address, an optional condition, and an ignore count. The
/// condition is three whitespace tokens (operand, mnemonic, operand); the
/// optional trailing "IGN N" gives a hex ignore count.
pub fn parse_break_spec(entry: &str) -> Option<(u32, Option<BreakCond>, u32)> {
    let mut tokens = entry.split_whitespace();
    let addr = parse_hex_u32(tokens.next()?)?;
    let rest: Vec<&str> = tokens.collect();
    // Split off a trailing "IGN N" clause if present.
    let (cond_tokens, ignore) = match rest.iter().position(|t| t.eq_ignore_ascii_case("IGN")) {
        Some(i) => {
            let count = parse_hex_u32(rest.get(i + 1)?)?;
            (&rest[..i], count)
        }
        None => (&rest[..], 0),
    };
    let cond = match cond_tokens {
        [] => None,
        [lhs, op, rhs] => Some(BreakCond {
            lhs: parse_cond_operand(lhs)?,
            op: parse_cond_op(op)?,
            rhs: parse_cond_operand(rhs)?,
        }),
        _ => return None,
    };
    Some((addr, cond, ignore))
}

/// Parse a condition operand: a register name, `M<hex>` for a memory word, or a
/// bare hex immediate. Register names win over hex (so `D0` is the register,
/// not `$D0`); write an immediate with a leading zero (`0D0`) to disambiguate.
fn parse_cond_operand(token: &str) -> Option<CondOperand> {
    if let Some(reg) = parse_reg_name(token) {
        return Some(match reg {
            0..=7 => CondOperand::Data(reg),
            8..=15 => CondOperand::Addr(reg - 8),
            16 => CondOperand::Sr,
            _ => CondOperand::Pc,
        });
    }
    if let Some(hex) = token.strip_prefix('M').or_else(|| token.strip_prefix('m')) {
        return Some(CondOperand::Mem(parse_hex_u32(hex)?));
    }
    Some(CondOperand::Imm(parse_hex_u32(token)?))
}

fn parse_cond_op(token: &str) -> Option<CondOp> {
    Some(match token.to_ascii_uppercase().as_str() {
        "EQ" => CondOp::Eq,
        "NE" => CondOp::Ne,
        "LT" => CondOp::Lt,
        "GT" => CondOp::Gt,
        "LE" => CondOp::Le,
        "GE" => CondOp::Ge,
        "AND" => CondOp::And,
        _ => return None,
    })
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
        assert_eq!(
            ui.control_at(pos),
            Some(UiControl::MenuItem(MenuItem::MachineConfig))
        );
        let joystick = menu_item_rect(4);
        let pos = (joystick.x as i32 + 4, joystick.y as i32 + 4);
        assert_eq!(
            ui.control_at(pos),
            Some(UiControl::MenuItem(MenuItem::JoystickInput))
        );
        // Outside the menu: nothing (the click closes the menu).
        assert_eq!(ui.control_at((0, 0)), None);
    }

    #[test]
    fn every_menu_label_fits_inside_the_popup() {
        // The label is drawn at `item_rect.x + MENU_TEXT_INSET`; its glyphs
        // must end before the popup's right edge or the trailing "..." clips.
        let menu = menu_rect();
        let limit = menu.x + menu.w;
        let modes = [
            JoystickInputMode::Auto,
            JoystickInputMode::Keyboard,
            JoystickInputMode::Gamepad,
        ];
        let speeds = [WarpSpeed::X2, WarpSpeed::X8, WarpSpeed::X16, WarpSpeed::Max];
        for item in MENU_ITEMS {
            for warp in [false, true] {
                for recording in [false, true] {
                    for input_recording in [false, true] {
                        for &mode in &modes {
                            for &speed in &speeds {
                                let label = menu_item_label(
                                    item,
                                    warp,
                                    speed,
                                    recording,
                                    input_recording,
                                    mode,
                                );
                                let text_w = label.chars().count() * font::GLYPH_W * MENU_TEXT_PX;
                                let right = menu_item_rect(0).x + MENU_TEXT_INSET + text_w;
                                assert!(
                                    right <= limit,
                                    "label {label:?} ({text_w}px) overflows the menu by {}px",
                                    right.saturating_sub(limit)
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn frame_analyzer_controls_hit_test() {
        let ui = UiState {
            menu_open: false,
            panel: Some(Panel::FrameAnalyzer(FrameAnalyzerPanel::new())),
        };
        let rect = panel_rect(ui.panel.as_ref().unwrap());
        let raster = analyzer_raster_rect(rect);
        assert_eq!(
            ui.control_at((raster.x as i32 + raster.w as i32 / 2, raster.y as i32 + 2)),
            Some(UiControl::AnalyzerPick {
                x: 511,
                y: 8,
                scanline: false,
            })
        );
        let scanline = analyzer_scanline_rect(rect);
        assert_eq!(
            ui.control_at((
                scanline.x as i32 + scanline.w as i32 / 2,
                scanline.y as i32 + 2
            )),
            Some(UiControl::AnalyzerPick {
                x: 511,
                y: 60,
                scanline: true,
            })
        );
        let (control, button) = analyzer_button_rects(rect)[1];
        assert_eq!(control, UiControl::AnalyzerFrame);
        assert_eq!(
            ui.control_at((button.x as i32 + 2, button.y as i32 + 2)),
            Some(UiControl::AnalyzerFrame)
        );
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
        for ch in ['c', '0', '0', '3', 'C'] {
            panel.push_entry_char(ch);
        }
        assert_eq!(panel.entry, "C003C");
        assert_eq!(panel.entry_addr(), Some(0xC003C));
        // Punctuation is rejected (letters/digits/space are kept for spec
        // mnemonics).
        panel.push_entry_char('!');
        assert_eq!(panel.entry, "C003C");
        panel.backspace_entry();
        assert_eq!(panel.entry, "C003");
        // Capped at the entry length (room for a conditional breakpoint spec).
        for _ in 0..50 {
            panel.push_entry_char('F');
        }
        assert_eq!(panel.entry.len(), 40);
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

    #[test]
    fn entry_box_parses_address_and_poke_tokens() {
        let mut panel = DebuggerPanel::new();
        // The entry only accepts hex, space, and the P/S/R register letters.
        for ch in "C00000 DEAD".chars() {
            panel.push_entry_char(ch);
        }
        assert_eq!(panel.entry, "C00000 DEAD");
        // The address consumers see just the first token.
        assert_eq!(panel.entry_addr(), Some(0xC00000));
        // Memory poke takes both tokens; the address is forced even.
        assert_eq!(panel.poke_target(), Some((0xC00000, 0xDEAD)));

        // Leading/doubled spaces are collapsed, and punctuation never makes it
        // in (letters are allowed now, for register names and condition
        // mnemonics).
        let mut panel = DebuggerPanel::new();
        for ch in "  D0  1234!".chars() {
            panel.push_entry_char(ch);
        }
        assert_eq!(panel.entry, "D0 1234");
        assert_eq!(panel.reg_poke(), Some((0, 0x1234)));
    }

    #[test]
    fn break_spec_parses_address_condition_and_ignore() {
        // Bare address: plain breakpoint.
        assert_eq!(parse_break_spec("C033C2"), Some((0xC033C2, None, 0)));

        // Address plus a register/immediate condition.
        let (addr, cond, ignore) = parse_break_spec("C033C2 D0 EQ 5").unwrap();
        assert_eq!(addr, 0xC033C2);
        assert_eq!(ignore, 0);
        assert_eq!(
            cond,
            Some(BreakCond {
                lhs: CondOperand::Data(0),
                op: CondOp::Eq,
                rhs: CondOperand::Imm(5),
            })
        );

        // Memory operand, bit-test op, and a trailing ignore count.
        let (_, cond, ignore) = parse_break_spec("40 MC00002 AND 4000 IGN A").unwrap();
        assert_eq!(ignore, 0xA);
        assert_eq!(
            cond,
            Some(BreakCond {
                lhs: CondOperand::Mem(0xC00002),
                op: CondOp::And,
                rhs: CondOperand::Imm(0x4000),
            })
        );

        // Ignore count with no condition.
        assert_eq!(parse_break_spec("1234 IGN 3"), Some((0x1234, None, 3)));

        // Malformed condition and bad address are rejected.
        assert!(parse_break_spec("C033C2 D0 EQ").is_none());
        assert!(parse_break_spec("C033C2 D0 XX 5").is_none());
        assert!(parse_break_spec("xyz").is_none());
    }

    #[test]
    fn register_names_map_to_gdb_indices() {
        assert_eq!(parse_reg_name("D0"), Some(0));
        assert_eq!(parse_reg_name("d7"), Some(7));
        assert_eq!(parse_reg_name("A0"), Some(8));
        assert_eq!(parse_reg_name("A7"), Some(15));
        assert_eq!(parse_reg_name("SP"), Some(15));
        assert_eq!(parse_reg_name("SR"), Some(16));
        assert_eq!(parse_reg_name("PC"), Some(17));
        assert_eq!(parse_reg_name("D8"), None);
        assert_eq!(parse_reg_name("A8"), None);
        assert_eq!(parse_reg_name("Z0"), None);
        assert_eq!(parse_reg_name(""), None);
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
    fn frame_analyzer_top_edge_overlays_clip_to_raster() {
        use super::super::window::{texture_height, texture_width};

        let scale = 1;
        let (w, h) = (texture_width(scale), texture_height(scale));
        let mut frame = vec![0u8; w * h * 4];
        let raster = Rect {
            x: 20,
            y: 20,
            w: 40,
            h: 20,
        };
        let trace = AnalyzerTraceView {
            frame: 1,
            seconds: 0.0,
            rows: 4,
            cols: 4,
            line_cck: 4,
            visible_start_vpos: 0,
            visible_lines: 2,
            display_hpos_start: 0,
            display_hpos_end: 4,
            owner_cck: [0; 9],
            blitter_busy_cck: 0,
            blitter_starve_cck: [0; 9],
            partial: false,
            selected_vpos: 0,
            selected_hpos: 0,
            selected_owner: "idle",
            selected_owner_code: b'.',
            owners: vec![b'.'; 16],
            markers: vec![AnalyzerMarker {
                vpos: 0,
                hpos: 1,
                label: "copper v=000 h=001 $096=0000".to_string(),
            }],
        };

        draw_owner_heatmap(&mut frame, raster, &trace, scale);

        let pixel = |frame: &[u8], x: usize, y: usize| -> [u8; 4] {
            frame[(y * w + x) * 4..(y * w + x) * 4 + 4]
                .try_into()
                .unwrap()
        };
        for x in raster.x - 4..raster.x + raster.w + 4 {
            assert_eq!(pixel(&frame, x, raster.y - 1), [0, 0, 0, 0]);
        }
        for y in raster.y..raster.y + raster.h {
            assert_eq!(pixel(&frame, raster.x - 1, y), [0, 0, 0, 0]);
        }
        assert_eq!(
            pixel(&frame, raster.x, raster.y),
            BUTTON_EDGE_LIGHT.to_le_bytes()
        );
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
            WarpSpeed::Max,
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
            WarpSpeed::Max,
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
            WarpSpeed::Max,
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
            WarpSpeed::Max,
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
            WarpSpeed::Max,
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
            WarpSpeed::Max,
            false,
            false,
            JoystickInputMode::Auto,
        );
        assert!(panel_has_title_bar(&frame, ui.panel.as_ref().unwrap()));
        save(&frame, "debugger-break");

        // Configuration screen: an A1200 on the Memory tab.
        let mut frame = vec![0u8; w * h * 4];
        let mut state = LauncherState::new(launcher::MachineSetup::default());
        state.setup.select_model(Some(MachineModel::A1200));
        state
            .setup
            .set_path(LauncherField::Rom, std::path::PathBuf::from("kick31.rom"));
        state.tab = LauncherTab::Memory;
        let ui = UiState {
            menu_open: false,
            panel: Some(Panel::Launcher(Box::new(state))),
        };
        draw(
            &mut frame,
            scale,
            &ui,
            Some(UiControl::LauncherRun),
            None,
            false,
            WarpSpeed::Max,
            false,
            false,
            JoystickInputMode::Auto,
        );
        assert!(panel_has_title_bar(&frame, ui.panel.as_ref().unwrap()));
        save(&frame, "launcher");

        // Configuration screen: the Zorro tab with a WASM plugin board whose
        // config-option schema renders an editable field per option.
        let manifest_path = std::env::temp_dir().join(format!(
            "copperline-ui-preview-board-{}.toml",
            std::process::id()
        ));
        std::fs::write(
            &manifest_path,
            r#"
            name = "Demo NIC"
            zorro = 2
            type = "wasm"
            size = "64K"
            manufacturer = 5192
            product = 16
            wasm = "demo.wasm"
            [config]
            mode = "bridged"
            [[option]]
            key = "mode"
            label = "Mode"
            type = "enum"
            choices = ["bridged", "nat"]
            [[option]]
            key = "verbose"
            label = "Verbose"
            type = "bool"
            [[option]]
            key = "mtu"
            label = "MTU"
            type = "int"
            default = 1500
            [[option]]
            key = "rom"
            label = "Boot ROM"
            type = "file"
            [[option]]
            key = "mac"
            label = "MAC address"
            type = "string"
            default = "02:00:10:00:00:01"
        "#,
        )
        .unwrap();
        let mut frame = vec![0u8; w * h * 4];
        let mut state = LauncherState::new(launcher::MachineSetup::default());
        state.setup.add_zorro(manifest_path.clone());
        state.tab = LauncherTab::Zorro;
        let ui = UiState {
            menu_open: false,
            panel: Some(Panel::Launcher(Box::new(state))),
        };
        draw(
            &mut frame,
            scale,
            &ui,
            None,
            None,
            false,
            WarpSpeed::Max,
            false,
            false,
            JoystickInputMode::Auto,
        );
        assert!(panel_has_title_bar(&frame, ui.panel.as_ref().unwrap()));
        save(&frame, "launcher-zorro");
        let _ = std::fs::remove_file(&manifest_path);
    }
}
