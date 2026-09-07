// SPDX-License-Identifier: GPL-3.0-or-later

//! Machine configuration panel layout and navigation.

use super::*;
mod dialogs;
#[cfg(feature = "game-library")]
mod library;
mod rows;
pub(in crate::video::ui) use dialogs::*;
#[cfg(feature = "game-library")]
pub(in crate::video::ui) use library::*;
pub(in crate::video::ui) use rows::*;

// ---------------------------------------------------------------------------
// Machine-configuration (launcher) panel
// ---------------------------------------------------------------------------

// Full canvas width: the panel's edges line up with the status bar
// below it rather than leaving gutters of display either side.
pub(in crate::video::ui) const LAUNCHER_W: usize = FB_WIDTH;
pub(in crate::video::ui) const LAUNCHER_H: usize = 520;
pub(in crate::video::ui) const LAUNCH_MARGIN: usize = 8;
pub(in crate::video::ui) const LAUNCH_MODEL_H: usize = 22;
pub(in crate::video::ui) const LAUNCH_MODEL_GAP: usize = 4;
/// Machines per row in the selector grid before it wraps; the grid rebalances
/// so the buttons fill the width (eight fit one row; the current ten models
/// wrap to two balanced rows).
pub(in crate::video::ui) const LAUNCH_MODEL_MAX_PER_ROW: usize = 8;
/// Width of the left-hand vertical category-tab column.
pub(in crate::video::ui) const LAUNCH_SIDEBAR_W: usize = 116;
pub(in crate::video::ui) const LAUNCH_TAB_H: usize = 26;
pub(in crate::video::ui) const LAUNCH_TAB_GAP: usize = 2;
pub(in crate::video::ui) const LAUNCH_ROW_H: usize = 26;
/// Label column width inside the settings pane (before a row's control).
pub(in crate::video::ui) const LAUNCH_LABEL_W: usize = 150;
pub(in crate::video::ui) const LAUNCH_ARROW_W: usize = 24;
pub(in crate::video::ui) const LAUNCH_VALUE_W: usize = 132;
/// The priority column's value box. Narrower than the general one, which is
/// sized for device names: the widest thing here is "No drive", against a
/// priority otherwise (down to the "-128" that a cleared Bootable box
/// stores), and this leaves a clear margin either side.
pub(in crate::video::ui) const LAUNCH_BOOTPRI_VALUE_W: usize = 96;
pub(in crate::video::ui) const LAUNCH_TOGGLE_W: usize = 64;
pub(in crate::video::ui) const LAUNCH_ACTION_W: usize = 84;
pub(in crate::video::ui) const LAUNCH_ACTION_H: usize = 22;
pub(in crate::video::ui) const LAUNCH_BROWSE_W: usize = 66;
pub(in crate::video::ui) const LAUNCH_CLEAR_W: usize = LAUNCH_BROWSE_W;
/// Width of the path-preview text column before a path row's Browse/Clear
/// buttons. The buttons sit just after it (near the other control widgets)
/// rather than out at the panel's right edge; a long value is clipped to fit.
pub(in crate::video::ui) const LAUNCH_PATH_VALUE_W: usize = 216;
/// Width of the editable volume-name box on a drive row.
pub(in crate::video::ui) const LAUNCH_NAME_W: usize = 96;
/// Width of the FFS/OFS toggle button on a drive row (just "FFS"/"OFS").
pub(in crate::video::ui) const LAUNCH_FS_W: usize = 40;
pub(in crate::video::ui) const LAUNCH_REMOVE_W: usize = 70;
pub(in crate::video::ui) const LAUNCH_CONTROL_H: usize = 20;

pub(in crate::video::ui) fn launcher_model_top(rect: Rect) -> usize {
    rect.y + TITLE_H + 8
}

/// (rows, columns) of the machine-selector grid, balanced so the buttons fill
/// the width evenly however many models there are.
pub(in crate::video::ui) fn launcher_model_grid() -> (usize, usize) {
    let count = launcher::MODELS.len();
    let rows = count.div_ceil(LAUNCH_MODEL_MAX_PER_ROW).max(1);
    (rows, count.div_ceil(rows))
}

pub(in crate::video::ui) fn launcher_model_rect(rect: Rect, i: usize) -> Rect {
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

pub(in crate::video::ui) fn launcher_model_strip_height() -> usize {
    let (rows, _) = launcher_model_grid();
    rows * (LAUNCH_MODEL_H + LAUNCH_MODEL_GAP)
}

/// Top of the configuration area (the vertical tab column and the settings
/// pane both start here), below the machine grid and its divider.
pub(in crate::video::ui) fn launcher_content_top(rect: Rect) -> usize {
    launcher_model_top(rect) + launcher_model_strip_height() + 12
}

/// A category tab in the left sidebar.
pub(in crate::video::ui) fn launcher_tab_rect(rect: Rect, i: usize) -> Rect {
    Rect {
        x: rect.x + LAUNCH_MARGIN,
        y: launcher_content_top(rect) + i * (LAUNCH_TAB_H + LAUNCH_TAB_GAP),
        w: LAUNCH_SIDEBAR_W,
        h: LAUNCH_TAB_H,
    }
}

/// Left edge of the settings pane (right of the tab column).
pub(in crate::video::ui) fn launcher_pane_x(rect: Rect) -> usize {
    rect.x + LAUNCH_MARGIN + LAUNCH_SIDEBAR_W + 12
}

/// X of a settings row's control column (after its label).
pub(in crate::video::ui) fn launcher_control_x(rect: Rect) -> usize {
    launcher_pane_x(rect) + LAUNCH_LABEL_W
}

pub(in crate::video::ui) fn launcher_row_y(rect: Rect, i: usize) -> usize {
    launcher_content_top(rect) + i * LAUNCH_ROW_H
}

pub(in crate::video::ui) fn launcher_action_y(rect: Rect) -> usize {
    rect.y + rect.h - LAUNCH_ACTION_H - 8
}

pub(in crate::video::ui) fn launcher_status_y(rect: Rect) -> usize {
    launcher_action_y(rect).saturating_sub(16)
}

/// (prev arrow, value field, next arrow) for a cycle row.
pub(in crate::video::ui) fn launcher_cycle_rects(rect: Rect, row_y: usize) -> (Rect, Rect, Rect) {
    launcher_stepper_rects(rect, row_y, LAUNCH_VALUE_W)
}

/// The geometry figures' `< value >`, on the same run as every other
/// stepper in the launcher.
pub(in crate::video::ui) fn launcher_geometry_stepper_rects(
    rect: Rect,
    row_y: usize,
) -> (Rect, Rect, Rect) {
    launcher_stepper_rects(rect, row_y, 64)
}

/// The priority column's `< value >`, on its own narrower value box.
pub(in crate::video::ui) fn launcher_bootpri_rects(rect: Rect, row_y: usize) -> (Rect, Rect, Rect) {
    launcher_stepper_rects(rect, row_y, LAUNCH_BOOTPRI_VALUE_W)
}

pub(in crate::video::ui) fn launcher_stepper_rects(
    rect: Rect,
    row_y: usize,
    value_w: usize,
) -> (Rect, Rect, Rect) {
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
        w: value_w,
        h: LAUNCH_CONTROL_H,
    };
    let next = Rect {
        x: value.x + value_w,
        y,
        w: LAUNCH_ARROW_W,
        h: LAUNCH_CONTROL_H,
    };
    (prev, value, next)
}

pub(in crate::video::ui) fn launcher_toggle_rect(rect: Rect, row_y: usize) -> Rect {
    Rect {
        x: launcher_control_x(rect),
        y: row_y + 2,
        w: LAUNCH_TOGGLE_W,
        h: LAUNCH_CONTROL_H,
    }
}

/// Nav-row buttons per row before they wrap.
pub(in crate::video::ui) const LAUNCH_NAV_PER_ROW: usize = 4;

/// A nav-row button, in the machine selector's own size and rhythm: same
/// width, same height, same gap, and the first one sits in the machine
/// grid's second column so it lines up with the button above it. Four to a
/// row, wrapping after that.
pub(in crate::video::ui) fn launcher_nav_button_rect(rect: Rect, slot: usize) -> Rect {
    // Column 1 of the machine grid, which is where the pane's own left
    // edge very nearly falls: taking the grid's column exactly is what
    // makes the two rows read as one column of buttons.
    let above = launcher_model_rect(rect, 1);
    let (row, col) = (slot / LAUNCH_NAV_PER_ROW, slot % LAUNCH_NAV_PER_ROW);
    Rect {
        x: above.x + col * (above.w + LAUNCH_MODEL_GAP),
        y: launcher_nav_y(rect) + row * (LAUNCH_MODEL_H + LAUNCH_MODEL_GAP),
        w: above.w,
        h: LAUNCH_MODEL_H,
    }
}

/// How tall the nav row block is for a given number of buttons, so the
/// settings below it start clear of a wrapped second row.
pub(in crate::video::ui) fn launcher_nav_rows(slots: usize) -> usize {
    slots.max(1).div_ceil(LAUNCH_NAV_PER_ROW)
}

/// A free-text value box: where a value would sit, at the width its content
/// needs -- a volume or device name on a Create Image row. (The serial
/// addresses draw their own host/port pair: [`launcher_serial_addr_rects`].)
pub(in crate::video::ui) fn launcher_text_rect(
    rect: Rect,
    row_y: usize,
    field: LauncherField,
) -> Rect {
    Rect {
        x: launcher_pane_x(rect) + LAUNCH_LABEL_W,
        y: row_y + (LAUNCH_ROW_H - LAUNCH_CONTROL_H) / 2,
        w: if field.is_netplay() {
            300
        } else {
            LAUNCH_NAME_W
        },
        h: LAUNCH_CONTROL_H,
    }
}

/// The button on a Create Image action row: the page's one commitment,
/// rather than another little control.
pub(in crate::video::ui) fn launcher_action_rect(rect: Rect, row_y: usize) -> Rect {
    Rect {
        // At the pane's own left edge, under the labels rather than out in
        // the value column: it acts on the page, not on a row.
        x: launcher_pane_x(rect),
        // Pushed down a little, so it is not mistaken for another setting.
        y: row_y + (LAUNCH_ROW_H - LAUNCH_TAB_H) / 2 + 10,
        // Sized like the category buttons down the left: the same shape the
        // launcher uses everywhere for "go and do this".
        w: LAUNCH_SIDEBAR_W,
        h: LAUNCH_TAB_H,
    }
}

/// The geometry editor's second button, beside its Save.
pub(in crate::video::ui) fn launcher_action2_rect(rect: Rect, row_y: usize) -> Rect {
    let first = launcher_action_rect(rect, row_y);
    Rect {
        x: first.x + first.w + LAUNCH_TAB_GAP,
        ..first
    }
}

/// The typed number on the hard-drive size row. Lines up with the value
/// boxes on the rows below it, so the column reads straight down.
pub(in crate::video::ui) fn launcher_size_box_rect(rect: Rect, row_y: usize) -> Rect {
    Rect {
        x: launcher_pane_x(rect) + LAUNCH_LABEL_W,
        y: row_y + (LAUNCH_ROW_H - LAUNCH_CONTROL_H) / 2,
        w: 64,
        h: LAUNCH_CONTROL_H,
    }
}

/// The Auto / Custom pair on the geometry row, and the Configure button
/// that joins them once the geometry is set by hand. Sized like the
/// Browse/Clear buttons the path rows use.
pub(in crate::video::ui) fn launcher_geometry_rects(
    rect: Rect,
    row_y: usize,
) -> (Rect, Rect, Rect) {
    let y = row_y + (LAUNCH_ROW_H - LAUNCH_CONTROL_H) / 2;
    let auto = Rect {
        x: launcher_pane_x(rect) + LAUNCH_LABEL_W,
        y,
        w: LAUNCH_CLEAR_W,
        h: LAUNCH_CONTROL_H,
    };
    let custom = Rect {
        x: auto.x + auto.w + 4,
        w: LAUNCH_BROWSE_W,
        ..auto
    };
    let configure = Rect {
        x: custom.x + custom.w + 4,
        w: LAUNCH_ACTION_W,
        ..auto
    };
    (auto, custom, configure)
}

/// "a" or "an" for a size like `64M`, which is read aloud as a number:
/// anything beginning with an eight takes "an", as do eleven and eighteen
/// themselves. (Eighteen *thousand* would too, but no size box reaches it.)
pub(in crate::video::ui) fn indefinite_article(size: &str) -> &'static str {
    let leading: String = size.chars().take_while(char::is_ascii_digit).collect();
    let vowel = leading.starts_with('8') || leading == "11" || leading == "18";
    if vowel {
        "an"
    } else {
        "a"
    }
}

/// Which control a free-text value box is. The same widget serves two
/// stores -- a Create Image word and a serial address on the machine --
/// so both the hit-test and the drawing ask here rather than each
/// keeping its own copy of the rule.
pub(in crate::video::ui) fn value_box_control(field: LauncherField) -> UiControl {
    if field.is_netplay() {
        UiControl::LauncherNetplayEdit(field)
    } else if field == LauncherField::RamPattern {
        UiControl::LauncherRamPatternEdit
    } else {
        // The serial addresses draw their own pair of boxes and never come
        // through here.
        UiControl::LauncherNewImageEdit(field)
    }
}

pub(in crate::video::ui) fn launcher_row_action(field: LauncherField) -> UiControl {
    if field.is_netplay() {
        UiControl::LauncherNetplayAction(field)
    } else {
        UiControl::LauncherNewImageCreate(field)
    }
}

pub(in crate::video::ui) fn launcher_second_action(field: LauncherField) -> Option<LauncherField> {
    match field {
        LauncherField::NewGeomSave => Some(LauncherField::NewGeomAuto),
        LauncherField::NetplayNewCode => Some(LauncherField::NetplayCopyCode),
        _ => None,
    }
}

/// The two boxes of a serial address row -- `[host] : [port]` -- spanning
/// exactly the run of the steppers above them: from the left edge of a `<`
/// to the right edge of a `>`, so the pair reads as one column with them.
pub(in crate::video::ui) fn launcher_serial_addr_rects(rect: Rect, row_y: usize) -> (Rect, Rect) {
    let y = row_y + (LAUNCH_ROW_H - LAUNCH_CONTROL_H) / 2;
    let x = launcher_control_x(rect);
    let total = LAUNCH_ARROW_W + LAUNCH_VALUE_W + LAUNCH_ARROW_W;
    // Five digits, a cell for the caret while typing, and padding; the
    // host takes what the colon leaves.
    let port_w = (SERIAL_PORT_DIGITS + 1) * font::GLYPH_W + 8;
    let host = Rect {
        x,
        y,
        w: total - port_w - font::GLYPH_W - 8,
        h: LAUNCH_CONTROL_H,
    };
    let port = Rect {
        x: x + total - port_w,
        y,
        w: port_w,
        h: LAUNCH_CONTROL_H,
    };
    (host, port)
}

/// The widest port is five digits.
pub(in crate::video::ui) const SERIAL_PORT_DIGITS: usize = 5;

/// Draw one half of a serial address pair: an edit box that shows its
/// greyed default while untouched.
#[allow(clippy::too_many_arguments)]
pub(in crate::video::ui) fn draw_serial_half_box(
    frame: &mut [u8],
    box_rect: Rect,
    state: &LauncherState,
    control: UiControl,
    typing: bool,
    value: Option<String>,
    placeholder: &str,
    scale: usize,
) {
    draw_rect_bevel(
        frame,
        scale_rect(box_rect, scale),
        BUTTON_EDGE_DARK,
        BUTTON_EDGE_LIGHT,
        scale,
    );
    light_edit_box(frame, box_rect, control, typing, scale);
    let avail = box_rect.w.saturating_sub(8);
    if typing {
        draw_edit_line(
            frame,
            box_rect.x + 4,
            box_rect.y + 6,
            state.edit_buffer(),
            state.edit_caret().at(),
            PANEL_TEXT_HILIGHT,
            PANEL_BG,
            avail,
            scale,
        );
        return;
    }
    let (text, color) = match value {
        Some(v) => (v, PANEL_TEXT),
        None => (placeholder.to_string(), PANEL_TEXT_DIM),
    };
    let shown = truncate_to_width(&text, avail);
    draw_panel_text(
        frame,
        box_rect.x + 4,
        box_rect.y + 6,
        &shown,
        color,
        1,
        scale,
    );
}

/// Light a text box the focus is standing on.
///
/// A box takes the same blue as every other control, so walking onto
/// one looks like walking onto anything else. Opening it to type hands
/// the box back its own colours: what to watch then is the caret
/// blinking in the value, and a lit box behind it would fight that.
/// Only the focus lights these -- the pointer has never coloured them,
/// and a box that changed under the mouse would read as a button.
pub(in crate::video::ui) fn light_edit_box(
    frame: &mut [u8],
    box_rect: Rect,
    control: UiControl,
    editing: bool,
    scale: usize,
) {
    let light = lit(None, control);
    if light == 0.0 || editing {
        return;
    }
    let inner = Rect {
        x: box_rect.x + 1,
        y: box_rect.y + 1,
        w: box_rect.w.saturating_sub(2),
        h: box_rect.h.saturating_sub(2),
    };
    fill_rect(
        frame,
        scale_rect(inner, scale),
        light_face(PANEL_BG, NAV_FACE, light),
        scale,
    );
}

/// Draw a free-text/number value box: what the setting holds, or what is
/// being typed into it, with a caret while it has the focus. Used by the
/// Create Image pages and by the Serial section's TCP address boxes, so it
/// reads the value through `row_value` rather than from either store.
pub(in crate::video::ui) fn draw_launcher_value_box(
    frame: &mut [u8],
    box_rect: Rect,
    state: &LauncherState,
    field: LauncherField,
    disabled: bool,
    centred: bool,
    scale: usize,
) {
    draw_rect_bevel(
        frame,
        scale_rect(box_rect, scale),
        BUTTON_EDGE_DARK,
        BUTTON_EDGE_LIGHT,
        scale,
    );
    light_edit_box(
        frame,
        box_rect,
        value_box_control(field),
        state.typing_in_value_box(field),
        scale,
    );
    let avail = box_rect.w.saturating_sub(8);
    if state.typing_in_value_box(field) {
        draw_edit_line(
            frame,
            box_rect.x + 4,
            box_rect.y + 6,
            state.edit_buffer(),
            state.edit_caret().at(),
            PANEL_TEXT_HILIGHT,
            PANEL_BG,
            avail,
            scale,
        );
        return;
    }
    let (text, color) = match disabled {
        true => (state.row_value(field), PANEL_TEXT_DIM),
        false => (state.row_value(field), PANEL_TEXT),
    };
    // A value too long for the box loses its tail: the head is the part
    // that says which of several it is.
    let shown = truncate_to_width(&text, avail);
    // A short figure between two arrows reads as belonging to them when it
    // is centred, and as a stray left-aligned word when it is not.
    let x = if centred {
        let text_w = shown.chars().count() * font::GLYPH_W;
        box_rect.x + box_rect.w.saturating_sub(text_w) / 2
    } else {
        box_rect.x + 4
    };
    draw_panel_text(frame, x, box_rect.y + 6, &shown, color, 1, scale);
}

/// Gap between one tick box's label and the next box along.
pub(in crate::video::ui) const LAUNCH_TICK_GAP: usize = 14;
/// A tick box's own side, and the gap between it and its label.
pub(in crate::video::ui) const LAUNCH_TICK_BOX: usize = 10;
pub(in crate::video::ui) const LAUNCH_TICK_LABEL_GAP: usize = 5;

/// Lay a row of labelled tick boxes across the value column, left to right,
/// and hand back each one's clickable rect (box and label together, so the
/// word is as easy to hit as the square).
pub(in crate::video::ui) fn launcher_tick_strip(
    rect: Rect,
    row_y: usize,
    labels: &[&str],
) -> Vec<Rect> {
    let mut x = launcher_pane_x(rect) + LAUNCH_LABEL_W;
    let y = row_y + (LAUNCH_ROW_H - LAUNCH_TICK_BOX) / 2;
    labels
        .iter()
        .map(|label| {
            let w = LAUNCH_TICK_BOX + LAUNCH_TICK_LABEL_GAP + label.len() * font::GLYPH_W;
            let at = Rect {
                x,
                y,
                w,
                h: LAUNCH_TICK_BOX,
            };
            x += w + LAUNCH_TICK_GAP;
            at
        })
        .collect()
}

/// Draw one entry of a tick strip: the box, then its word.
pub(in crate::video::ui) fn draw_launcher_tick_choice(
    frame: &mut [u8],
    at: Rect,
    label: &str,
    set: bool,
    disabled: bool,
    hot: f32,
    scale: usize,
) {
    let colour = if disabled { PANEL_TEXT_DIM } else { TICK_GREEN };
    draw_tick_box(frame, at.x, at.y, set, colour, scale);
    if !disabled {
        if let Some(edge) = tick_outline(hot) {
            draw_outline(
                frame,
                Rect {
                    w: LAUNCH_TICK_BOX,
                    ..at
                },
                edge,
                scale,
            );
        }
    }
    draw_panel_text(
        frame,
        at.x + LAUNCH_TICK_BOX + LAUNCH_TICK_LABEL_GAP,
        at.y + 1,
        label,
        if disabled { PANEL_TEXT_DIM } else { PANEL_TEXT },
        1,
        scale,
    );
}

/// A typed whole number, lined up with the value column beside it.
pub(in crate::video::ui) fn launcher_number_rect(rect: Rect, row_y: usize) -> Rect {
    Rect {
        x: launcher_pane_x(rect) + LAUNCH_LABEL_W,
        y: row_y + (LAUNCH_ROW_H - LAUNCH_CONTROL_H) / 2,
        w: 64,
        h: LAUNCH_CONTROL_H,
    }
}

/// The unit written beside that number. Text, not a button -- but clicking
/// it swaps MB and GB, so it is a control all the same.
pub(in crate::video::ui) fn launcher_size_unit_rect(rect: Rect, row_y: usize) -> Rect {
    let box_rect = launcher_size_box_rect(rect, row_y);
    Rect {
        x: box_rect.x + box_rect.w + 8,
        y: box_rect.y,
        w: 2 * font::GLYPH_W,
        h: LAUNCH_CONTROL_H,
    }
}

/// Geometry of the Host Disk table: the framed box listing what the host has.
pub(in crate::video::ui) const HOST_DISK_ROW_H: usize = 14;
pub(in crate::video::ui) const HOST_DISK_HEADER_H: usize = 16;
/// Rows drawn inside the box at once. A longer list scrolls.
pub(crate) const HOST_DISK_VISIBLE_ROWS: usize = 8;
/// Column starts, as offsets from the inside edge of the box. Volume gets
/// the widest cell -- model strings are the longest text on the page -- and
/// every cell clips at the next column, so a Windows `PhysicalDrive11` reads
/// truncated in Disk rather than running into its neighbour.
pub(in crate::video::ui) const HOST_DISK_COL_DISK: usize = 8;
pub(in crate::video::ui) const HOST_DISK_COL_VOLUME: usize = 112;
pub(in crate::video::ui) const HOST_DISK_COL_SIZE: usize = 272;
pub(in crate::video::ui) const HOST_DISK_COL_ATTACH: usize = 344;
pub(in crate::video::ui) const HOST_DISK_COL_WRITABLE: usize = 440;
/// The last column ends before the scroll arrows, which sit inside the frame.
pub(in crate::video::ui) const HOST_DISK_COL_TICK: usize = 472;

pub(in crate::video::ui) fn host_disk_table_rect(rect: Rect) -> Rect {
    let x = launcher_pane_x(rect);
    Rect {
        x,
        y: launcher_content_top(rect) + LAUNCH_NAV_BLOCK_H + 18,
        w: rect.w.saturating_sub(x - rect.x + 16),
        h: HOST_DISK_HEADER_H + HOST_DISK_VISIBLE_ROWS * HOST_DISK_ROW_H + 4,
    }
}

/// One row inside the table, by index.
pub(in crate::video::ui) fn host_disk_row_rect(rect: Rect, index: usize) -> Rect {
    let table = host_disk_table_rect(rect);
    Rect {
        x: table.x + 2,
        y: table.y + HOST_DISK_HEADER_H + index * HOST_DISK_ROW_H,
        w: table.w.saturating_sub(4),
        h: HOST_DISK_ROW_H,
    }
}

// --- the WHDLoad Library page ---------------------------------------------

/// The games list starts level with the top of the Memory tab and is as
/// tall as the art frame beside it; the favourites list fills what is left
/// below, down to the status line. Both are worked out from the panel
/// rather than from a row count, so these are what that comes to -- and
/// what the scrolling and hit-testing count in.
///
/// `whdload_entry` is whether the strip carries the WHDLoad entry -- see
/// [`launcher::tabs`] -- since the strip is a row longer when it does, and
/// every rect on this page is measured against it. Every layout function
/// here takes it for the same reason.
#[cfg(feature = "game-library")]
pub(in crate::video) fn library_visible_rows(rect: Rect, whdload_entry: bool) -> usize {
    library_table_rect(rect, whdload_entry)
        .h
        .saturating_sub(LIBRARY_HEADER_H + 4)
        / LIBRARY_ROW_H
}

#[cfg(feature = "game-library")]
pub(in crate::video) fn library_favourite_rows(rect: Rect, whdload_entry: bool) -> usize {
    library_favourites_rect(rect, whdload_entry)
        .h
        .saturating_sub(LIBRARY_HEADER_H + 4)
        / LIBRARY_ROW_H
}
#[cfg(feature = "game-library")]
pub(in crate::video::ui) const LIBRARY_ROW_H: usize = 14;
#[cfg(feature = "game-library")]
pub(in crate::video::ui) const LIBRARY_HEADER_H: usize = 16;
/// The widest a cover is drawn. The gap either side of its frame is
/// [`LIBRARY_COVER_GAP`], and the frame around it [`LIBRARY_COVER_BEZEL`].
#[cfg(feature = "game-library")]
pub(in crate::video::ui) const LIBRARY_COVER: usize = 128;
/// How much taller than wide the art frame is. Amiga box art is portrait:
/// measured across the catalogue it runs between 0.75 and 0.82 wide-to-tall
/// with the odd square compilation, so 4:5 sits in the middle of what is
/// really there and a picture of any of those shapes only has to give up a
/// thin margin to fit.
#[cfg(feature = "game-library")]
pub(in crate::video::ui) const LIBRARY_COVER_TALL: (usize, usize) = (5, 4);
/// The frame around the art: thicker than the list's hairline outline and
/// bevelled, so the picture reads as mounted in the panel rather than
/// pasted onto it.
#[cfg(feature = "game-library")]
pub(in crate::video::ui) const LIBRARY_COVER_BEZEL: usize = 5;
/// Between the game list and the row of buttons under it.
#[cfg(feature = "game-library")]
pub(in crate::video::ui) const LIBRARY_BUTTON_GAP: usize = 8;
/// How many lines the version under the cover runs to.
#[cfg(feature = "game-library")]
pub(in crate::video::ui) const LIBRARY_VERSION_LINES: usize = 2;
/// How many lines each catalogue field under the cover runs to. A
/// developer is sometimes credited to nine people, and without a limit
/// that one field pushes everything under it off the panel.
#[cfg(feature = "game-library")]
pub(in crate::video::ui) const LIBRARY_FIELD_LINES: usize = 2;

/// The most a version may be, in characters: what fits the column it is
/// drawn in, over [`LIBRARY_VERSION_LINES`] lines. The editor stops there
/// too, since there is no use in typing what the page cannot show.
#[cfg(feature = "game-library")]
pub(in crate::video) fn library_version_max() -> usize {
    LIBRARY_VERSION_LINES * (LIBRARY_COVER + 2 * LIBRARY_COVER_BEZEL) / font::GLYPH_W
}
#[cfg(feature = "game-library")]
pub(in crate::video::ui) const LIBRARY_COVER_GAP: usize = 12;
/// Where each column starts, from the inside edge of the box. Two columns:
/// the game, and whether it is a favourite. Year and publisher moved under
/// the cover art, where there is room to read them.
#[cfg(feature = "game-library")]
pub(in crate::video::ui) const LIBRARY_COL_NAME: usize = 6;
/// The Favourite column, far enough right that a long title clips before
/// it rather than running into it.
/// Where the tick column starts, as an offset into the box.
///
/// Measured back from the right-hand edge rather than fixed, so the
/// heading and the ticks under it stay clear of the scroll arrows inside
/// the frame -- which they did not when the art column beside them grew.
#[cfg(feature = "game-library")]
pub(in crate::video::ui) fn library_col_favourite(rect: Rect, whdload_entry: bool) -> usize {
    let table = library_table_rect(rect, whdload_entry);
    let heading = "Favourite".len() * font::GLYPH_W;
    table
        .w
        .saturating_sub(HOST_DISK_ARROW + 12 + heading + 6)
        .max(LIBRARY_COL_NAME + 40)
}

/// Where a tab sits in the strip, whichever strip is showing.
#[cfg(feature = "game-library")]
pub(in crate::video::ui) fn strip_rect(
    rect: Rect,
    tab: launcher::LauncherTab,
    whdload_entry: bool,
) -> Rect {
    let at = launcher::tabs(whdload_entry)
        .iter()
        .position(|&t| t == tab)
        .unwrap_or(0);
    launcher_tab_rect(rect, at)
}

/// The games list, squared off against the strip beside it: its top level
/// with the top of Memory, its bottom with the bottom of I/O Ports. Tying
/// it to the strip rather than to a row count keeps the page looking
/// deliberate when the strip changes -- which it does, since WHDLoad can
/// join it.
#[cfg(feature = "game-library")]
pub(in crate::video::ui) fn library_table_rect(rect: Rect, whdload_entry: bool) -> Rect {
    let top = strip_rect(rect, launcher::LauncherTab::Memory, whdload_entry);
    let x = launcher_pane_x(rect);
    let right = rect.x + rect.w - 16;
    Rect {
        x,
        y: top.y,
        w: right
            .saturating_sub(x)
            .saturating_sub(library_cover_column()),
        // The art frame's height, so the two boxes end on one line. Its
        // top stays level with the top of Memory in the strip; whatever it
        // no longer reaches down to, the favourites list below it takes.
        h: library_cover_size().1,
    }
}

/// The favourites list, under the games with the button row between them.
/// It stops short of the bottom so the panel's own status line, which
/// reports what just happened, is never drawn over.
#[cfg(feature = "game-library")]
pub(in crate::video::ui) fn library_favourites_rect(rect: Rect, whdload_entry: bool) -> Rect {
    let games = library_table_rect(rect, whdload_entry);
    // The gap: the button row, then the "Favourites:" label above the box.
    let y = games.y + games.h + LIBRARY_BUTTON_GAP + LAUNCH_MODEL_H + 10 + 14;
    let bottom = launcher_status_y(rect).saturating_sub(10);
    Rect {
        y,
        h: bottom.saturating_sub(y),
        ..games
    }
}

/// One row of the favourites list.
#[cfg(feature = "game-library")]
pub(in crate::video::ui) fn library_favourite_row_rect(
    rect: Rect,
    whdload_entry: bool,
    drawn: usize,
) -> Rect {
    let table = library_favourites_rect(rect, whdload_entry);
    Rect {
        x: table.x + 2,
        y: table.y + LIBRARY_HEADER_H + drawn * LIBRARY_ROW_H,
        w: table.w.saturating_sub(4),
        h: LIBRARY_ROW_H,
    }
}

/// The Favourite tick on one drawn row: centred under its heading rather
/// than tucked against the left of the column.
#[cfg(feature = "game-library")]
pub(in crate::video::ui) fn library_favourite_box(
    rect: Rect,
    whdload_entry: bool,
    drawn: usize,
) -> Rect {
    centred_tick(
        library_row_rect(rect, whdload_entry, drawn),
        library_col_favourite(rect, whdload_entry),
        "Favourite",
    )
}

/// The Remove tick on one row of the favourites list.
#[cfg(feature = "game-library")]
pub(in crate::video::ui) fn library_remove_box(
    rect: Rect,
    whdload_entry: bool,
    drawn: usize,
) -> Rect {
    // On the same line as the Favourite tick in the list above it, not
    // centred under its own shorter heading: two columns of the same tick
    // that do not line up read as a mistake.
    centred_tick(
        library_favourite_row_rect(rect, whdload_entry, drawn),
        library_col_favourite(rect, whdload_entry),
        "Favourite",
    )
}

/// Where the "Remove" heading goes: centred over its own ticks.
#[cfg(feature = "game-library")]
pub(in crate::video::ui) fn library_remove_heading_x(rect: Rect, whdload_entry: bool) -> usize {
    let tick = centred_tick(
        library_favourites_rect(rect, whdload_entry),
        library_col_favourite(rect, whdload_entry),
        "Favourite",
    );
    (tick.x + 5).saturating_sub("Remove".len() * font::GLYPH_W / 2)
}

/// A tick in the second column, centred under a heading of that width
/// rather than tucked against the left of the column.
#[cfg(feature = "game-library")]
pub(in crate::video::ui) fn centred_tick(row: Rect, column: usize, heading: &str) -> Rect {
    let width = heading.len() * font::GLYPH_W;
    Rect {
        x: row.x + 4 + column + width.saturating_sub(10) / 2,
        y: row.y + (row.h - 10) / 2,
        w: 10,
        h: 10,
    }
}

/// One row of the list, by drawn position rather than by index into the
/// library: the list scrolls, so the two differ.
#[cfg(feature = "game-library")]
pub(in crate::video::ui) fn library_row_rect(
    rect: Rect,
    whdload_entry: bool,
    drawn: usize,
) -> Rect {
    let table = library_table_rect(rect, whdload_entry);
    Rect {
        x: table.x + 2,
        y: table.y + LIBRARY_HEADER_H + drawn * LIBRARY_ROW_H,
        w: table.w.saturating_sub(4),
        h: LIBRARY_ROW_H,
    }
}

/// How much of the panel's width the art column takes: the widest frame,
/// with a gap either side of it, so the frame is centred in a space rather
/// than pressed against the list.
#[cfg(feature = "game-library")]
pub(in crate::video::ui) fn library_cover_column() -> usize {
    library_cover_size().0 + 2 * LIBRARY_COVER_GAP
}

/// The art frame at its widest, which is also the game list's height: the
/// two boxes end on the same line, and it is the frame -- sized from the
/// shape of a cover -- that decides where that line is. The frame is never
/// stretched to reach anything.
#[cfg(feature = "game-library")]
pub(in crate::video::ui) fn library_cover_size() -> (usize, usize) {
    (
        LIBRARY_COVER + 2 * LIBRARY_COVER_BEZEL,
        LIBRARY_COVER * LIBRARY_COVER_TALL.0 / LIBRARY_COVER_TALL.1 + 2 * LIBRARY_COVER_BEZEL,
    )
}

/// The art frame: the size the layout reserves, whatever is in it. A
/// picture that is not this shape is fitted inside and letterboxed, rather
/// than the frame being cut to the picture -- a frame that changed shape
/// per game would drag the metadata under it up and down the page.
#[cfg(feature = "game-library")]
pub(in crate::video::ui) fn library_cover_rect(rect: Rect, whdload_entry: bool) -> Rect {
    let table = library_table_rect(rect, whdload_entry);
    let column = table.x + table.w;
    let right = rect.x + rect.w - 16;
    let (w, h) = library_cover_size();
    Rect {
        x: column + right.saturating_sub(column).saturating_sub(w) / 2,
        y: table.y,
        w,
        h,
    }
}

/// The most a picture may be, inside the widest frame.
#[cfg(feature = "game-library")]
pub(in crate::video::ui) fn library_art_rect(rect: Rect, whdload_entry: bool) -> Rect {
    let frame = library_cover_rect(rect, whdload_entry);
    Rect {
        x: frame.x + LIBRARY_COVER_BEZEL,
        y: frame.y + LIBRARY_COVER_BEZEL,
        w: frame.w - 2 * LIBRARY_COVER_BEZEL,
        h: frame.h - 2 * LIBRARY_COVER_BEZEL,
    }
}

/// The three buttons under the game list: as thin as the ones along the
/// top, and sized so a third fits beside the two there are.
#[cfg(feature = "game-library")]
pub(in crate::video::ui) fn library_button_rects(rect: Rect, whdload_entry: bool) -> [Rect; 3] {
    let table = library_table_rect(rect, whdload_entry);
    let gap = 6;
    let w = (table.w + gap) / 3 - gap;
    std::array::from_fn(|i| Rect {
        x: table.x + i * (w + gap),
        y: table.y + table.h + LIBRARY_BUTTON_GAP,
        w,
        h: LAUNCH_MODEL_H,
    })
}

/// The art box inside it, at the same 4:5 the Library page uses.
#[cfg(feature = "game-library")]
pub(in crate::video::ui) const META_ART: (usize, usize) = (112, 140);

/// One A-Z shortcut button.
///
/// Its own drawer rather than [`draw_text_button`], for the hover: the lift
/// a button's face gets is a couple of shades across seven visible pixels,
/// which at this size is no answer to the pointer at all. Hovered, the
/// whole face goes to the blue the chosen list row uses, so there is no
/// mistaking which letter is under it. A letter with nothing behind it
/// does not answer.
#[cfg(feature = "game-library")]
pub(in crate::video::ui) fn draw_az_button(
    frame: &mut [u8],
    rect: Rect,
    label: &str,
    live: bool,
    hovered: f32,
    scale: usize,
) {
    let scaled = scale_rect(rect, scale);
    fill_rect(
        frame,
        scaled,
        light_face(BUTTON_FACE, MENU_HILIGHT_BG, hovered),
        scale,
    );
    draw_rect_bevel(frame, scaled, BUTTON_EDGE_LIGHT, BUTTON_EDGE_DARK, scale);
    let resting = if live {
        BUTTON_TEXT
    } else {
        BUTTON_TEXT_DISABLED
    };
    let colour = light_face(resting, MENU_HILIGHT_TEXT, hovered);
    let text_w = label.chars().count() * font::GLYPH_W;
    let x = rect.x + rect.w.saturating_sub(text_w) / 2;
    let y = rect.y + rect.h.saturating_sub(font::GLYPH_H) / 2;
    draw_panel_text(frame, x, y, label, colour, 1, scale);
}

/// The A-Z shortcut buttons, from just after the "Games:" label to the
/// right edge of the list below them.
///
/// Each is barely wider than the character on it -- the row has to hold
/// twenty-eight of them across the width of the list -- except the digits
/// bucket, which carries three characters and is given the room for them.
/// The leftover pixels are spread one apiece from the left, so the last
/// button ends exactly on the list's right edge rather than a few pixels
/// short of it.
#[cfg(feature = "game-library")]
pub(in crate::video::ui) fn library_az_rects(rect: Rect, whdload_entry: bool) -> Vec<Rect> {
    use launcher::AZ_BUCKETS;
    let table = library_table_rect(rect, whdload_entry);
    let label = "Games:".len() * font::GLYPH_W;
    let x = table.x + label + LIBRARY_AZ_GAP;
    let width = (table.x + table.w).saturating_sub(x);
    let wide = 3 * font::GLYPH_W + 2;
    let narrow_count = AZ_BUCKETS - 1;
    let narrow = width.saturating_sub(wide) / narrow_count;
    // What the division left over, one pixel to each of the first buttons.
    let mut spare = width.saturating_sub(wide + narrow * narrow_count);
    let mut at = x;
    (0..AZ_BUCKETS)
        .map(|bucket| {
            let mut w = if bucket == 0 { wide } else { narrow };
            if spare > 0 {
                w += 1;
                spare -= 1;
            }
            let r = Rect {
                x: at,
                y: table.y.saturating_sub(15),
                w: w.saturating_sub(1),
                h: LIBRARY_AZ_H,
            };
            at += w;
            r
        })
        .collect()
}

/// How many games a list needs before the A-Z row appears.
///
/// A short list is read rather than navigated: with a screenful or so in
/// front of you, twenty-eight buttons to reach one of them is in the way.
#[cfg(feature = "game-library")]
pub(in crate::video::ui) const LIBRARY_AZ_MIN_GAMES: usize = 20;

/// How far the shortcut row starts after the "Games:" label, and how tall
/// its buttons are: the label's own line, so the row costs no height.
#[cfg(feature = "game-library")]
pub(in crate::video::ui) const LIBRARY_AZ_GAP: usize = 6;
#[cfg(feature = "game-library")]
pub(in crate::video::ui) const LIBRARY_AZ_H: usize = 11;

/// The scroll arrows for a list, inside its own frame: up in the top right
/// corner, down in the bottom right. Both Library lists use it, each with
/// its own pair of controls.
#[cfg(feature = "game-library")]
pub(in crate::video::ui) fn library_arrows_in(
    table: Rect,
    control: fn(isize) -> UiControl,
) -> [(UiControl, Rect); 2] {
    let x = table.x + table.w - HOST_DISK_ARROW - 3;
    let arrow = |y| Rect {
        x,
        y,
        w: HOST_DISK_ARROW,
        h: HOST_DISK_ARROW,
    };
    [
        (control(-1), arrow(table.y + 2)),
        (control(1), arrow(table.y + table.h - HOST_DISK_ARROW - 2)),
    ]
}

#[cfg(feature = "game-library")]
pub(in crate::video::ui) fn library_arrow_rects(
    rect: Rect,
    whdload_entry: bool,
) -> [(UiControl, Rect); 2] {
    library_arrows_in(
        library_table_rect(rect, whdload_entry),
        UiControl::LauncherLibraryScroll,
    )
}

#[cfg(feature = "game-library")]
pub(in crate::video::ui) fn library_favourite_arrow_rects(
    rect: Rect,
    whdload_entry: bool,
) -> [(UiControl, Rect); 2] {
    library_arrows_in(
        library_favourites_rect(rect, whdload_entry),
        UiControl::LauncherLibraryFavouriteScroll,
    )
}

/// The scroll arrows, up at the top right of the box and down at the bottom
/// right. Inside the frame rather than beside it, so the box keeps its shape
/// whether or not the list overflows.
pub(in crate::video::ui) const HOST_DISK_ARROW: usize = 12;

pub(in crate::video::ui) fn host_disk_arrow_rects(rect: Rect) -> [(UiControl, Rect); 2] {
    let table = host_disk_table_rect(rect);
    let x = table.x + table.w - HOST_DISK_ARROW - 3;
    [
        (
            UiControl::LauncherHostDiskScroll(-1),
            Rect {
                x,
                y: table.y + 2,
                w: HOST_DISK_ARROW,
                h: HOST_DISK_ARROW,
            },
        ),
        (
            UiControl::LauncherHostDiskScroll(1),
            Rect {
                x,
                y: table.y + table.h - HOST_DISK_ARROW - 2,
                w: HOST_DISK_ARROW,
                h: HOST_DISK_ARROW,
            },
        ),
    ]
}

/// One scroll arrow: a bevelled button with a triangle on it.
///
/// Every scrolling list in the launcher draws its pair with this, so they
/// look and behave alike -- lit while there is somewhere to go that way and
/// greyed at the end of the list, brightened under the pointer.
///
/// The triangle is stacked runs rather than a glyph: the 8x8 font has no
/// arrow in it, and a "^" is a caret, which reads as punctuation next to a
/// list rather than as a direction.
pub(in crate::video::ui) fn draw_scroll_arrow(
    frame: &mut [u8],
    arrow: Rect,
    up: bool,
    live: bool,
    hovered: f32,
    scale: usize,
) {
    let scaled = scale_rect(arrow, scale);
    fill_rect(frame, scaled, BUTTON_FACE, scale);
    draw_rect_bevel(frame, scaled, BUTTON_EDGE_LIGHT, BUTTON_EDGE_DARK, scale);
    let colour = if live {
        light_face(BUTTON_TEXT, PANEL_TEXT_HILIGHT, hovered)
    } else {
        BUTTON_TEXT_DISABLED
    };
    // Three rows is enough to read as an arrow at this size. Widening
    // downwards is an up arrow (narrow tip at the top); widening upwards is
    // a down arrow.
    for step in 0..3usize {
        let width = 1 + step * 2;
        let y = match up {
            true => arrow.y + 4 + step,
            false => arrow.y + 4 + (2 - step),
        };
        fill_rect(
            frame,
            scale_rect(
                Rect {
                    x: arrow.x + HOST_DISK_ARROW / 2 - width / 2 - 1,
                    y,
                    w: width,
                    h: 1,
                },
                scale,
            ),
            colour,
            scale,
        );
    }
}

/// The Attach cell of one row: clicked to step through where the machine
/// would see the disk.
pub(in crate::video::ui) fn host_disk_attach_cell(rect: Rect, index: usize) -> Rect {
    let row = host_disk_row_rect(rect, index);
    Rect {
        x: row.x + HOST_DISK_COL_ATTACH,
        y: row.y,
        // Up to the next column, so a click near the edge cannot land on
        // both this cell and the one beside it.
        w: HOST_DISK_COL_WRITABLE - HOST_DISK_COL_ATTACH,
        h: row.h,
    }
}

/// The R/W cell of one row.
pub(in crate::video::ui) fn host_disk_writable_cell(rect: Rect, index: usize) -> Rect {
    let row = host_disk_row_rect(rect, index);
    Rect {
        x: row.x + HOST_DISK_COL_WRITABLE,
        y: row.y,
        w: HOST_DISK_COL_TICK - HOST_DISK_COL_WRITABLE,
        h: row.h,
    }
}

/// The Enable cell: the last column, and the rest of the row with it.
pub(in crate::video::ui) fn host_disk_enable_cell(rect: Rect, index: usize) -> Rect {
    let row = host_disk_row_rect(rect, index);
    Rect {
        x: row.x + HOST_DISK_COL_TICK,
        y: row.y,
        w: row.w.saturating_sub(HOST_DISK_COL_TICK),
        h: row.h,
    }
}

/// The buttons under the table, left to right: the two acts on the ticked
/// disks first, then Refresh, which only looks.
/// The setting a control belongs to, where it belongs to one.
///
/// A greyed row greys everything on it -- its arrows, its box, its
/// Browse -- so this is how the focus knows to step over the lot
/// rather than standing on a control that cannot light or answer.
pub(in crate::video::ui) fn control_field(control: UiControl) -> Option<LauncherField> {
    Some(match control {
        UiControl::LauncherCycle { field, .. }
        | UiControl::LauncherFsFamily { field, .. }
        | UiControl::LauncherFsVariant { field, .. }
        | UiControl::LauncherToggle(field)
        | UiControl::LauncherBrowse(field)
        | UiControl::LauncherClear(field)
        | UiControl::LauncherDriveNameEdit(field)
        | UiControl::LauncherDriveFilesystemToggle(field)
        | UiControl::LauncherNewImageEdit(field)
        | UiControl::LauncherNetplayEdit(field)
        | UiControl::LauncherNetplayAction(field)
        | UiControl::LauncherSerialHostEdit(field)
        | UiControl::LauncherSerialPortEdit(field)
        | UiControl::LauncherNewImageCreate(field)
        | UiControl::LauncherDriveBootpriEdit(field)
        | UiControl::LauncherDriveBootToggle(field) => field,
        #[cfg(feature = "game-library")]
        UiControl::LauncherWhdloadDownload(field) => field,
        _ => return None,
    })
}

/// Whether a control can be worked at all.
///
/// The drawing greys what cannot be answered and, having greyed it,
/// refuses it any light: a marker standing on one is a marker that has
/// disappeared, which reads as the arrow key having done nothing. So
/// the focus is not offered them. The pointer still is -- clicking a
/// dead button has always been harmless, and taking the hit away would
/// change what the mouse does.
pub(in crate::video) fn control_live(ui: &UiState, control: UiControl) -> bool {
    // The calibration panel greys Skip until a step may be skipped, and
    // Save until every step is captured, by the same rule it draws them
    // with: a marker on either while it is dead would disappear.
    if let Some(Panel::Calibration(session)) = ui.panel.as_ref() {
        return cal_button_enabled(control, session);
    }
    let Some(Panel::Launcher(state)) = ui.panel.as_ref() else {
        return true;
    };
    if let UiControl::LauncherHostDiskAttach(at) = control {
        // Blank until the disk is ticked, and a blank cell is nothing
        // to stand on: ticking is what gives a disk a place to go.
        return state
            .setup
            .host_disks()
            .get(at)
            .is_some_and(|disk| state.setup.host_disk_is_selected(&disk.id));
    }
    // A dialog answers for the whole panel while it is up, and what it
    // answers with everywhere else is "put me away". That is a click
    // anywhere, not a place on the screen, so it is nowhere for the
    // marker to stand -- and standing on it, covering the panel, there
    // was nothing beyond it to step to.
    if state.save_dialog && control == UiControl::LauncherSave {
        return false;
    }
    if state.confirm_reset && control == UiControl::LauncherCancelReset {
        return false;
    }
    let Some(field) = control_field(control) else {
        return true;
    };
    // A workshop row greys on its own terms -- there is no machine
    // setting behind it to explain itself -- so it is asked directly,
    // as the drawing asks it.
    if field.is_netplay() {
        return state.row_applies(field);
    }
    if LauncherState::is_workshop(field) {
        return state.workshop_applies(field);
    }
    state.setup.disabled_reason(field).is_none()
}

/// Whether one of the buttons under the host-disk list can be pressed.
///
/// Mount needs a disk to mount; Unmount a ticked disk the machine
/// actually has; Refresh only ever looks, so it stays live. Asked by
/// the hit-test as well as the drawing, so a dead button is no more a
/// place for the focus to stand than it is a thing to click.
pub(in crate::video::ui) fn host_disk_button_live(
    setup: &launcher::MachineSetup,
    control: UiControl,
) -> bool {
    match control {
        UiControl::LauncherHostDiskMount => !setup.host_disks_selected().is_empty(),
        UiControl::LauncherHostDiskUnmountSelected => setup
            .host_disks_selected()
            .iter()
            .any(|id| setup.host_disk_is_attached(id)),
        _ => true,
    }
}

pub(in crate::video::ui) fn host_disk_button_rects(rect: Rect) -> [(UiControl, Rect); 3] {
    let table = host_disk_table_rect(rect);
    let y = table.y + table.h + 10;
    let button = |slot: usize| Rect {
        x: table.x + slot * 96,
        y,
        w: 88,
        h: LAUNCH_TAB_H,
    };
    [
        (UiControl::LauncherHostDiskMount, button(0)),
        (UiControl::LauncherHostDiskUnmountSelected, button(1)),
        (UiControl::LauncherHostDiskRefresh, button(2)),
    ]
}

/// A sub-page's Back button: the nav row's first slot, always.
pub(in crate::video::ui) fn launcher_back_button_rect(rect: Rect) -> Rect {
    launcher_nav_button_rect(rect, 0)
}

/// How many buttons the nav row holds for a tab: its sibling links, plus a
/// Back button when it is a sub-page.
pub(in crate::video::ui) fn launcher_nav_slots(tab: launcher::LauncherTab) -> usize {
    usize::from(tab.parent_tab().is_some()) + tab.nav_options().len()
}

/// Y of the nav row (the sibling-page buttons and any Back button) at the top of
/// the settings pane, in line with the first category tab. The setting rows
/// below it are shifted down by [`LAUNCH_NAV_BLOCK_H`] to make room.
pub(in crate::video::ui) fn launcher_nav_y(rect: Rect) -> usize {
    launcher_content_top(rect)
}

/// Vertical space reserved at the top of the pane for the nav button row plus a
/// gap below it, before the settings begin, on tabs that have a nav.
pub(in crate::video::ui) const LAUNCH_NAV_BLOCK_H: usize = LAUNCH_MODEL_H + 14;

/// The same, for a tab whose nav wraps onto more than one row.
pub(in crate::video::ui) fn launcher_nav_block_h(tab: launcher::LauncherTab) -> usize {
    let rows = launcher_nav_rows(launcher_nav_slots(tab));
    LAUNCH_NAV_BLOCK_H + (rows - 1) * (LAUNCH_MODEL_H + LAUNCH_MODEL_GAP)
}

/// The Boot Priority page's paging button, when it has one: any page short
/// of the last offers the next, and only while there is a next to offer.
/// Every page's Back button already returns to the first, so paging only
/// ever needs to go forward.
pub(in crate::video::ui) fn boot_page_button(
    state: &LauncherState,
) -> Option<(&'static str, launcher::LauncherTab)> {
    state
        .setup
        .boot_priority_next_page(state.tab)
        .map(|next| ("Next Page >", next))
}

/// The Status column's clickable area (the "Bootable" label plus its tick box),
/// sitting to the right of the priority stepper on a Boot Priority row.
pub(in crate::video::ui) fn launcher_bootable_rect(rect: Rect, row_y: usize) -> Rect {
    let (_, _, next) = launcher_bootpri_rects(rect, row_y);
    Rect {
        x: next.x + next.w + 24,
        y: row_y + 2,
        w: BOOTABLE_LABEL.len() * font::GLYPH_W + 8 + 12,
        h: LAUNCH_CONTROL_H,
    }
}

/// The tick box within a Bootable cell, after its label.
pub(in crate::video::ui) fn launcher_bootable_box(cell: Rect) -> Rect {
    Rect {
        x: cell.x + BOOTABLE_LABEL.len() * font::GLYPH_W + 8,
        y: cell.y + (cell.h.saturating_sub(12)) / 2,
        w: 12,
        h: 12,
    }
}

pub(in crate::video::ui) const BOOTABLE_LABEL: &str = "Bootable";

/// The heading above the FluxBridge settings: upstream's own name for the
/// library, and which version of it is installed. Nothing else in the launcher
/// says which build is in use, and it is the first thing worth knowing when a
/// drive misbehaves.
pub(in crate::video::ui) fn bridge_library_heading() -> String {
    #[cfg(feature = "fluxbridge")]
    return format!("FluxBridge v{}:", crate::fluxbridge::version());
    #[cfg(not(feature = "fluxbridge"))]
    "FluxBridge:".to_string()
}

pub(in crate::video::ui) const WRITE_PROTECT_LABEL: &str = "Write protect:";
pub(in crate::video::ui) const PHYSICAL_DRIVE_LABEL: &str = "Physical drive:";

/// The two tick-box cells under a floppy drive: write protect on the left,
/// the real-drive switch level with the value column so the eye can run down
/// the tab.
pub(in crate::video::ui) fn launcher_floppy_flag_rects(rect: Rect, row_y: usize) -> (Rect, Rect) {
    let y = row_y + 2;
    let protect = Rect {
        // Indented to sit under the media row's label, which carries its own
        // two leading spaces, so the drive's two lines start together.
        x: launcher_pane_x(rect) + 2 * font::GLYPH_W,
        y,
        w: WRITE_PROTECT_LABEL.len() * font::GLYPH_W + 8 + 12,
        h: LAUNCH_CONTROL_H,
    };
    let bridge = Rect {
        x: launcher_control_x(rect) + LAUNCH_ARROW_W,
        y,
        w: PHYSICAL_DRIVE_LABEL.len() * font::GLYPH_W + 8 + 12,
        h: LAUNCH_CONTROL_H,
    };
    (protect, bridge)
}

/// The tick box inside one of those cells, after its label.
pub(in crate::video::ui) fn launcher_flag_box(cell: Rect, label: &str) -> Rect {
    Rect {
        x: cell.x + label.len() * font::GLYPH_W + 8,
        y: cell.y + (cell.h.saturating_sub(12)) / 2,
        w: 12,
        h: 12,
    }
}

/// The Configure button on a bridged drive's media row, where Browse sits on
/// an image-backed one.
pub(in crate::video::ui) fn launcher_bridge_configure_rect(rect: Rect, row_y: usize) -> Rect {
    let (browse, clear) = launcher_path_rects(rect, row_y);
    Rect {
        x: browse.x,
        y: browse.y,
        w: browse.w + 4 + clear.w,
        h: browse.h,
    }
}

/// (Browse, Clear) buttons for a path row, just after the fixed-width value
/// column ([`LAUNCH_PATH_VALUE_W`]) rather than out at the panel's right edge.
/// Which of a path row's two buttons are there, as (browse, reset).
///
/// Every path row outside the Paths page has both, always. On the Paths
/// page a row that is inheriting has nothing to reset, so it offers only
/// Browse -- and the base swaps the two rather than showing both, because
/// it is the root the others hang off and moving it is a different act
/// from picking a folder for one of them.
///
/// One function, so what is drawn and what can be clicked cannot disagree:
/// a Reset that is not there must not still answer, and a Browse that is
/// not there must not still open a dialog.
pub(in crate::video::ui) fn launcher_path_buttons(
    setup: &launcher::MachineSetup,
    field: LauncherField,
) -> (bool, bool) {
    // The soundfont row keeps both buttons on show; Reset greys out
    // while the bundled GeneralUser GS is already the bank in force.
    #[cfg(feature = "coppersynth")]
    if field == LauncherField::CsynthSoundfont {
        return (true, true);
    }
    if !field.is_paths_field() {
        return (true, true);
    }
    let set = setup.paths_is_set(field);
    if field == LauncherField::PathsBase {
        (!set, set)
    } else {
        (true, set)
    }
}

/// Whether a row is a Paths row that has not been given a directory of its
/// own. Its label and value are dimmed to say so: the row is showing
/// Copperline's answer rather than the person's.
///
/// Not the base. It names a real directory either way, it is the one row
/// on the page that always says something, and dimming the only line that
/// tells you where everything is would be the wrong thing to play down.
pub(in crate::video::ui) fn launcher_path_inherits(
    setup: &launcher::MachineSetup,
    field: LauncherField,
) -> bool {
    // The soundfont row reads the same way: unset means the bundled
    // bank, dimmed as Copperline's answer and reading from the left
    // like the bundled ROM defaults do.
    #[cfg(feature = "coppersynth")]
    if field == LauncherField::CsynthSoundfont {
        return setup.path(field).is_none();
    }
    // The ROMs with bundled defaults read the same way: unset means the
    // bundled image, dimmed as Copperline's answer.
    if matches!(field, LauncherField::Rom | LauncherField::FmvRom) {
        return setup.path(field).is_none()
            && (field != LauncherField::FmvRom || !setup.fmv_rom_disabled());
    }
    if field == LauncherField::ScsiRom {
        return setup.scsi_bundled_rom_label().is_some() && setup.path(field).is_none();
    }
    field.is_paths_field() && field != LauncherField::PathsBase && !setup.paths_is_set(field)
}

/// Whether the row's second button has anything to do: a Clear with
/// nothing behind it is shown but greyed, so the pair of buttons keeps
/// its shape while saying there is nothing to take away. The Paths page
/// keeps its own arrangement -- its Reset only appears once something
/// is set, so it is always live.
pub(in crate::video::ui) fn launcher_clear_enabled(
    setup: &launcher::MachineSetup,
    field: LauncherField,
) -> bool {
    if field.is_paths_field() {
        return true;
    }
    if field == LauncherField::FmvRom {
        return true;
    }
    setup.path(field).is_some()
}

/// The FMV path row's second button controls the physical module, not just a
/// pathname: it must be usable from the bundled-default state so the launcher
/// can write `fmv_rom = ""`, and usable again to restore that default.
pub(in crate::video::ui) fn launcher_clear_label(
    setup: &launcher::MachineSetup,
    field: LauncherField,
) -> &'static str {
    if field == LauncherField::FmvRom {
        if setup.fmv_rom_disabled() {
            "Default"
        } else {
            "Remove"
        }
    } else if field.is_paths_field() {
        "Reset"
    } else {
        "Clear"
    }
}

pub(in crate::video::ui) fn launcher_path_rects(rect: Rect, row_y: usize) -> (Rect, Rect) {
    let y = row_y + 2;
    let browse = Rect {
        x: launcher_control_x(rect) + LAUNCH_PATH_VALUE_W,
        y,
        w: LAUNCH_BROWSE_W,
        h: LAUNCH_CONTROL_H,
    };
    let clear = Rect {
        x: browse.x + LAUNCH_BROWSE_W + 4,
        y,
        w: LAUNCH_CLEAR_W,
        h: LAUNCH_CONTROL_H,
    };
    (browse, clear)
}

/// The Download button on a support-archive row.
///
/// To the *left* of Browse rather than after Clear, where the row's value
/// would be. There is room because the button and the value are never both
/// there: it is only offered while nothing has been chosen, and the value
/// then reads "(none)".
#[cfg(feature = "game-library")]
pub(in crate::video::ui) const LAUNCH_DOWNLOAD_W: usize = 78;

#[cfg(feature = "game-library")]
pub(in crate::video::ui) fn launcher_download_rect(rect: Rect, row_y: usize) -> Rect {
    let (browse, _) = launcher_path_rects(rect, row_y);
    Rect {
        x: browse.x.saturating_sub(LAUNCH_DOWNLOAD_W + 6),
        w: LAUNCH_DOWNLOAD_W,
        ..browse
    }
}

/// Which archive a row is for, if it is one of the two.
#[cfg(feature = "game-library")]
pub(in crate::video::ui) fn row_archive(
    field: LauncherField,
) -> Option<crate::gamelib::support::Archive> {
    use crate::gamelib::support::Archive;
    match field {
        LauncherField::WhdloadWhdPackage => Some(Archive::Whdload),
        LauncherField::WhdloadSkickPackage => Some(Archive::Skick),
        _ => None,
    }
}

/// The editable volume-name box on a drive row: it sits just left of the
/// Browse button, with the path text filling the space before it.
/// Whether a drive row's FFS/OFS toggle applies: only a directory mount on
/// one of the disk-backed drive fields (IDE/SCSI/lide) has a filesystem
/// choice to make -- an HDF/gzip image already carries its own, and a
/// `Filesys*Dir` row is a live HOSTFS mount, not a disk snapshot, so it has
/// no filesystem to choose either. `drive_is_directory` restricts to
/// exactly that field set on its own (returning `false` for anything else,
/// same as `drive_filesystem`'s fallback) and reads a cached flag rather
/// than statting the path here on every frame the row is drawn.
pub(in crate::video::ui) fn launcher_drive_fs_applies(
    setup: &launcher::MachineSetup,
    field: LauncherField,
) -> bool {
    setup.drive_is_directory(field)
}

pub(in crate::video::ui) fn launcher_drive_name_rect(rect: Rect, row_y: usize) -> Rect {
    let (browse, _clear) = launcher_path_rects(rect, row_y);
    Rect {
        x: browse.x.saturating_sub(6 + LAUNCH_NAME_W),
        y: browse.y,
        w: LAUNCH_NAME_W,
        h: LAUNCH_CONTROL_H,
    }
}

/// The FFS/OFS toggle button on a drive row: just left of the volume-name
/// box, shown under the same condition as `launcher_drive_fs_applies`
/// above (a directory mount on a disk-backed drive field).
pub(in crate::video::ui) fn launcher_drive_fs_rect(rect: Rect, row_y: usize) -> Rect {
    let name_box = launcher_drive_name_rect(rect, row_y);
    Rect {
        x: name_box.x.saturating_sub(6 + LAUNCH_FS_W),
        y: name_box.y,
        w: LAUNCH_FS_W,
        h: LAUNCH_CONTROL_H,
    }
}

pub(in crate::video::ui) fn launcher_action_rects(rect: Rect) -> [(UiControl, Rect); 4] {
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
pub(in crate::video::ui) enum ZorroItem {
    Header(usize),
    Option { board: usize, opt: usize },
}

/// Flatten the Zorro boards into (content-row, item) pairs: each board header
/// and its option rows, with row 0 the first board header. The Add button is
/// drawn above the list, outside these rows.
pub(in crate::video::ui) fn launcher_zorro_layout(
    setup: &launcher::MachineSetup,
) -> Vec<(usize, ZorroItem)> {
    let mut items = Vec::new();
    // Row 0 is the first list row; the board list is shifted below the Add button
    // by LAUNCH_NAV_BLOCK_H at draw/hit-test time.
    let mut row = 0;
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
pub(in crate::video::ui) fn launcher_zorro_remove_rect(rect: Rect, row: usize) -> Rect {
    Rect {
        x: rect.x + rect.w - LAUNCH_MARGIN - LAUNCH_REMOVE_W,
        y: launcher_row_y(rect, row) + LAUNCH_NAV_BLOCK_H + 2,
        w: LAUNCH_REMOVE_W,
        h: LAUNCH_CONTROL_H,
    }
}

/// The clickable value box for a string option at `row_y` (control column to
/// the right margin).
pub(in crate::video::ui) fn launcher_board_value_rect(rect: Rect, row_y: usize) -> Rect {
    let x = launcher_control_x(rect);
    let right = rect.x + rect.w - LAUNCH_MARGIN;
    Rect {
        x,
        y: row_y + 2,
        w: right.saturating_sub(x),
        h: LAUNCH_CONTROL_H,
    }
}

/// The "Add board..." button. It stands where every other tab's nav row
/// stands and takes that row's first slot, so the top of the pane keeps one
/// shape whichever tab is open; the board list follows below it after the
/// same gap.
pub(in crate::video::ui) fn launcher_zorro_add_rect(rect: Rect) -> Rect {
    launcher_nav_button_rect(rect, 0)
}

pub(in crate::video::ui) fn launcher_action_label(control: UiControl) -> &'static str {
    match control {
        UiControl::LauncherLoad => "Load...",
        UiControl::LauncherSave => "Save...",
        UiControl::LauncherDefaults => "Defaults",
        UiControl::LauncherRun => "Run",
        UiControl::LauncherSaveAs => "Save As",
        UiControl::LauncherSaveDefault => "Save default",
        UiControl::LauncherResetDefault => "Reset default",
        _ => "",
    }
}

/// What the Save dialog offers, left to right. The one that deletes
/// something sits furthest from where the pointer comes in.
pub(in crate::video) const SAVE_ACTIONS: [UiControl; 3] = [
    UiControl::LauncherSaveAs,
    UiControl::LauncherSaveDefault,
    UiControl::LauncherResetDefault,
];

/// One button's size, and the space around them. Every button is as wide
/// as the longest label so the row is even, and the dialog is then sized
/// to the row rather than the row fitted into a dialog.
pub(in crate::video::ui) const SAVE_DIALOG_BUTTON: (usize, usize) = (116, 20);
pub(in crate::video::ui) const SAVE_DIALOG_MARGIN: usize = 12;
pub(in crate::video::ui) const SAVE_DIALOG_GAP: usize = 6;
/// Lines kept for the description above the buttons, what one costs, and
/// the space between the last of them and the row.
///
/// Always reserved, whether or not anything is being pointed at: a dialog
/// that changed size as the pointer crossed it would move the buttons out
/// from under the pointer that was crossing them.
pub(in crate::video::ui) const SAVE_DIALOG_HELP_LINES: usize = 2;
pub(in crate::video::ui) const SAVE_DIALOG_LINE_H: usize = 12;
pub(in crate::video::ui) const SAVE_DIALOG_HELP_GAP: usize = 16;

/// What each button does, said while the pointer is on it.
///
/// Anything that is not one of the three gets the Save line. This is a
/// Save dialog opened from a Save button, so with the pointer resting
/// nowhere in particular it should say what saving means rather than go
/// blank and leave a hole where a sentence was a moment ago.
pub(in crate::video::ui) fn save_dialog_help(control: UiControl) -> &'static str {
    match control {
        UiControl::LauncherSaveDefault => {
            "Sets the running configuration as the default when you launch Copperline."
        }
        UiControl::LauncherResetDefault => "Resets the current default config to factory settings.",
        _ => "Save the running configuration to a file.",
    }
}

/// Hit-test the configuration panel. Returns the control under `pos`, or `None`
/// to let the caller swallow the click on the panel body.
pub(in crate::video::ui) fn launcher_control_at(
    rect: Rect,
    state: &LauncherState,
    pos: (i32, i32),
) -> Option<UiControl> {
    // The dialog answers for the whole panel while it is up: nothing
    // behind it can be clicked, which is what makes it a dialog.
    #[cfg(feature = "game-library")]
    if state.meta.is_some() {
        for (at, control) in [
            UiControl::MetaSave,
            UiControl::MetaClear,
            UiControl::MetaCancel,
        ]
        .into_iter()
        .enumerate()
        {
            if meta_button_rects(rect)[at].contains(pos) {
                return Some(control);
            }
        }
        if close_button_rect(meta_rect(rect)).contains(pos) {
            return Some(UiControl::MetaCancel);
        }
        if meta_art_rect(rect).contains(pos) {
            return Some(UiControl::MetaArt);
        }
        for field in launcher::MetaField::ALL {
            if meta_field_rect(rect, field).contains(pos) {
                return Some(UiControl::MetaField(field));
            }
        }
        return Some(UiControl::PanelBody);
    }
    #[cfg(feature = "game-library")]
    if state.login.is_some() {
        let (ok, cancel) = login_button_rects(rect);
        if ok.contains(pos) {
            return Some(UiControl::LoginOk);
        }
        // Its own close gadget, which is Cancel by another name. Checked
        // before the panel's, which sits behind it.
        if cancel.contains(pos) || close_button_rect(login_rect(rect)).contains(pos) {
            return Some(UiControl::LoginCancel);
        }
        for field in [launcher::LoginField::User, launcher::LoginField::Pass] {
            if login_field_rect(rect, field).contains(pos) {
                return Some(UiControl::LoginField(field));
            }
        }
        return Some(UiControl::PanelBody);
    }
    for (i, &model) in launcher::MODELS.iter().enumerate() {
        if launcher_model_rect(rect, i).contains(pos) {
            return Some(UiControl::LauncherModel(model));
        }
    }
    for (i, &tab) in launcher::tabs(state.setup.whdload_enabled())
        .iter()
        .enumerate()
    {
        if launcher_tab_rect(rect, i).contains(pos) {
            return Some(UiControl::LauncherTab(tab));
        }
    }
    if state.tab == LauncherTab::Zorro {
        use crate::zorro::ConfigOptionKind as K;
        for (row, item) in launcher_zorro_layout(&state.setup) {
            let row_y = launcher_row_y(rect, row) + LAUNCH_NAV_BLOCK_H;
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
                            if !state.setup.zorro_boards()[board].value(opt).is_empty()
                                && clear.contains(pos)
                            {
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
        let row_offset = if state.tab.has_top_nav() {
            launcher_nav_block_h(state.tab)
        } else {
            0
        };
        for (i, r) in state
            .rows()
            .iter()
            .filter(|r| state.setup.row_on_page(state.tab, r.field))
            .enumerate()
        {
            if !state.row_applies(r.field)
                && !launcher_second_action(r.field).is_some_and(|second| state.row_applies(second))
            {
                continue;
            }
            let row_y = launcher_row_y(rect, i) + row_offset;
            if let Some(control) = launcher_row_control_at(rect, state, r, row_y, pos) {
                return Some(control);
            }
        }
    }
    #[cfg(feature = "game-library")]
    if state.tab == LauncherTab::WhdloadLibrary {
        let whdload_entry = state.setup.whdload_enabled();
        if state.library.games.len() > library_visible_rows(rect, whdload_entry) {
            for (control, arrow) in library_arrow_rects(rect, whdload_entry) {
                if arrow.contains(pos) {
                    return Some(control);
                }
            }
        }
        for drawn in 0..library_visible_rows(rect, whdload_entry) {
            if state.library.scroll + drawn >= state.library.games.len() {
                break;
            }
            // The tick first: it sits inside the row, and marking a
            // favourite is not the same as choosing the game.
            if library_favourite_box(rect, whdload_entry, drawn).contains(pos) {
                return Some(UiControl::LauncherLibraryFavourite(drawn));
            }
            if library_row_rect(rect, whdload_entry, drawn).contains(pos) {
                return Some(UiControl::LauncherLibraryPick(drawn));
            }
        }
        for (at, control) in [
            UiControl::LauncherLibraryRefresh,
            UiControl::LauncherLibraryUpdate,
            UiControl::LauncherLibraryEdit,
        ]
        .into_iter()
        .enumerate()
        {
            if library_button_rects(rect, whdload_entry)[at].contains(pos)
                && (at == 0 || !state.library.games.is_empty())
            {
                return Some(control);
            }
        }
        if state.library.games.len() >= LIBRARY_AZ_MIN_GAMES {
            for (bucket, at) in library_az_rects(rect, whdload_entry)
                .into_iter()
                .enumerate()
            {
                if at.contains(pos) {
                    return Some(UiControl::LauncherLibraryJump(bucket));
                }
            }
        }
        let starred = state.library.db.favourite_count();
        let rows = library_favourite_rows(rect, whdload_entry);
        if starred > rows {
            for (control, arrow) in library_favourite_arrow_rects(rect, whdload_entry) {
                if arrow.contains(pos) {
                    return Some(control);
                }
            }
        }
        for drawn in 0..starred
            .saturating_sub(state.library.favourite_scroll)
            .min(rows)
        {
            if library_remove_box(rect, whdload_entry, drawn).contains(pos) {
                return Some(UiControl::LauncherLibraryFavouriteRemove(drawn));
            }
            if library_favourite_row_rect(rect, whdload_entry, drawn).contains(pos) {
                return Some(UiControl::LauncherLibraryFavouritePick(drawn));
            }
        }
    }
    // The top nav row: a page's sibling links (the Storage and A/V sub-pages),
    // or a Back button.
    if state.tab == LauncherTab::HostDisk {
        let disks = state.setup.host_disks().len();
        if disks > HOST_DISK_VISIBLE_ROWS {
            for (control, arrow) in host_disk_arrow_rects(rect) {
                if arrow.contains(pos) {
                    return Some(control);
                }
            }
        }
        {
            let scroll = state.setup.host_disk_scroll().min(disks);
            for slot in 0..disks.saturating_sub(scroll).min(HOST_DISK_VISIBLE_ROWS) {
                let i = scroll + slot;
                // The cells that are their own answer come first: clicking
                // Attach or R/O sets that, rather than picking the row.
                if host_disk_attach_cell(rect, slot).contains(pos) {
                    return Some(UiControl::LauncherHostDiskAttach(i));
                }
                if host_disk_writable_cell(rect, slot).contains(pos) {
                    return Some(UiControl::LauncherHostDiskWritable(i));
                }
                if host_disk_enable_cell(rect, slot).contains(pos) {
                    return Some(UiControl::LauncherHostDiskEnable(i));
                }
                if host_disk_row_rect(rect, slot).contains(pos) {
                    return Some(UiControl::LauncherHostDiskSelect(i));
                }
            }
        }
        for (control, button) in host_disk_button_rects(rect) {
            if button.contains(pos) && host_disk_button_live(&state.setup, control) {
                return Some(control);
            }
        }
    }
    // The nav row: a Back button when this is a sub-page, then whatever
    // sibling pages it offers. A page can have both -- the Create Image pages
    // say where they came from and which of the two they are.
    let mut slot = 0;
    if let Some(parent) = state.tab.parent_tab() {
        if launcher_back_button_rect(rect).contains(pos) {
            return Some(UiControl::LauncherNavTab(parent));
        }
        slot = 1;
    }
    for (i, &(_, target)) in state.tab.nav_options().iter().enumerate() {
        if launcher_nav_button_rect(rect, slot + i).contains(pos) {
            return Some(UiControl::LauncherNavTab(target));
        }
    }
    // The boot order's paging button, drawn in the slot after Back.
    if let Some((_, target)) = boot_page_button(state) {
        if launcher_nav_button_rect(rect, slot).contains(pos) {
            return Some(UiControl::LauncherNavTab(target));
        }
    }
    for (control, button_rect) in launcher_action_rects(rect) {
        if button_rect.contains(pos) {
            return Some(control);
        }
    }
    None
}

/// The Host Disk page: what the host has, and which of it to attach.
/// The Library page: the games found, which are favourites, and what the
/// database says about the one picked.
pub(in crate::video::ui) fn draw_host_disk_page(
    frame: &mut [u8],
    rect: Rect,
    state: &LauncherState,
    hover: Option<UiControl>,
    scale: usize,
) {
    let setup = &state.setup;
    let table = host_disk_table_rect(rect);

    // The box, sunk into the panel like an entry field so it reads as
    // something to look into rather than a raised control. The outline comes
    // first and goes all the way round: the inset shading alone is nearly the
    // panel's own colour, so on its own only the lit edges show and the box
    // looks bevelled on two sides rather than recessed.
    let scaled = scale_rect(table, scale);
    fill_rect(frame, scaled, ENTRY_BG, scale);
    draw_outline(frame, table, BUTTON_EDGE_LIGHT, scale);
    draw_rect_bevel(
        frame,
        scale_rect(
            Rect {
                x: table.x + 1,
                y: table.y + 1,
                w: table.w.saturating_sub(2),
                h: table.h.saturating_sub(2),
            },
            scale,
        ),
        BUTTON_EDGE_DARK,
        ENTRY_BG,
        scale,
    );

    // Column headings, then a rule under them.
    let head_y = table.y + 4;
    for (offset, title) in [
        (HOST_DISK_COL_DISK, "Disk"),
        (HOST_DISK_COL_VOLUME, "Volume"),
        (HOST_DISK_COL_SIZE, "Size"),
        (HOST_DISK_COL_ATTACH, "Attach"),
        (HOST_DISK_COL_WRITABLE, "R/W"),
        (HOST_DISK_COL_TICK, "Enable"),
    ] {
        draw_panel_text(
            frame,
            table.x + offset,
            head_y,
            title,
            PANEL_TEXT_DIM,
            1,
            scale,
        );
    }
    fill_rect(
        frame,
        scale_rect(
            Rect {
                x: table.x + 2,
                y: table.y + HOST_DISK_HEADER_H - 2,
                w: table.w.saturating_sub(4),
                h: 1,
            },
            scale,
        ),
        BUTTON_EDGE_DARK,
        scale,
    );

    let disks = setup.host_disks();
    let scroll = setup.host_disk_scroll().min(disks.len());
    if disks.is_empty() {
        draw_panel_text(
            frame,
            table.x + HOST_DISK_COL_DISK,
            table.y + HOST_DISK_HEADER_H + 4,
            "No supported disks found on the host system.",
            PANEL_TEXT_DIM,
            1,
            scale,
        );
    }
    for (slot, disk) in disks
        .iter()
        .skip(scroll)
        .take(HOST_DISK_VISIBLE_ROWS)
        .enumerate()
    {
        // The list index, not the row on screen: everything that acts on a
        // disk names the disk, so a scrolled list still ticks the right one.
        let i = scroll + slot;
        let row = host_disk_row_rect(rect, slot);
        let ticked = setup.host_disk_is_selected(&disk.id);
        // A disk the machine has keeps the highlight whether or not it is
        // ticked right now: in a long list, what is in use should be
        // findable at a glance.
        let light = lit(hover, UiControl::LauncherHostDiskSelect(i));
        if ticked || setup.host_disk_is_attached(&disk.id) || light != 0.0 {
            // The pointer's own highlight here is the same face a disk
            // in use keeps, so only the focus changes the colour.
            fill_rect(
                frame,
                scale_rect(row, scale),
                light_face(BUTTON_FACE, BUTTON_FACE, light),
                scale,
            );
        }
        let text_y = row.y + (HOST_DISK_ROW_H - font::GLYPH_H) / 2;
        // A disk the host has mounted is not dimmed: mounting takes it from
        // the host first, so being in use is not a reason it cannot be had.
        // Every column is clipped to the space before the next one: a long
        // device name or volume must not run into its neighbour.
        for (offset, next, text) in [
            (HOST_DISK_COL_DISK, HOST_DISK_COL_VOLUME, disk.id.clone()),
            (
                HOST_DISK_COL_VOLUME,
                HOST_DISK_COL_SIZE,
                disk.volume.clone(),
            ),
            (HOST_DISK_COL_SIZE, HOST_DISK_COL_ATTACH, disk.size.clone()),
            (
                HOST_DISK_COL_ATTACH,
                HOST_DISK_COL_WRITABLE,
                // Blank until the disk is ticked: an unticked disk is going
                // nowhere, and ticking is what gives it a place.
                disk.attach.map(|attach| attach.label()).unwrap_or_default(),
            ),
        ] {
            let text = truncate_to_width(&text, next - offset - 8);
            draw_panel_text(frame, row.x + offset, text_y, &text, PANEL_TEXT, 1, scale);
        }
        // Two ticks, the same kind of answer either way: may the guest write
        // to this disk, and is it going to the machine at all. Writing is on
        // by default -- a disk given to a machine is normally meant to be
        // used -- so unticking R/W is what protects it.
        for (x, set, colour, control) in [
            (
                HOST_DISK_COL_WRITABLE + 6,
                disk.writable,
                PANEL_TEXT,
                UiControl::LauncherHostDiskWritable(i),
            ),
            (
                HOST_DISK_COL_TICK + 12,
                ticked,
                PANEL_TEXT_HILIGHT,
                UiControl::LauncherHostDiskEnable(i),
            ),
        ] {
            let at = Rect {
                x: row.x + x,
                y: row.y + 2,
                w: TICK_BOX,
                h: TICK_BOX,
            };
            draw_tick_box(frame, at.x, at.y, set, colour, scale);
            if let Some(edge) = tick_outline(lit(hover, control)) {
                draw_outline(frame, at, edge, scale);
            }
        }
        // The attach column is blank until the disk is ticked, so the
        // focus standing on it has nothing of its own to light: it
        // takes the face a button under the pointer would.
        let attach_light = lit(hover, UiControl::LauncherHostDiskAttach(i));
        if attach_light != 0.0 {
            let cell = host_disk_attach_cell(rect, slot);
            fill_rect(
                frame,
                scale_rect(cell, scale),
                light_face(BUTTON_FACE, BUTTON_FACE_HOVER, attach_light),
                scale,
            );
            draw_panel_text(
                frame,
                cell.x,
                text_y,
                &disk.attach.map(|a| a.label()).unwrap_or_default(),
                PANEL_TEXT,
                1,
                scale,
            );
        }
    }

    // Arrows only when there is somewhere to go, and each greys at its end
    // of the list so the box says where the window is.
    if disks.len() > HOST_DISK_VISIBLE_ROWS {
        for (control, arrow) in host_disk_arrow_rects(rect) {
            let up = control == UiControl::LauncherHostDiskScroll(-1);
            let live = if up {
                scroll > 0
            } else {
                scroll + HOST_DISK_VISIBLE_ROWS < disks.len()
            };
            draw_scroll_arrow(frame, arrow, up, live, lit(hover, control), scale);
        }
    }

    for (control, button) in host_disk_button_rects(rect) {
        let label = match control {
            UiControl::LauncherHostDiskMount => "Mount",
            UiControl::LauncherHostDiskUnmountSelected => "Unmount",
            _ => "Refresh",
        };
        let enabled = host_disk_button_live(setup, control);
        draw_text_button(
            frame,
            button,
            label,
            enabled,
            if enabled { lit(hover, control) } else { 0.0 },
            scale,
        );
    }

    // What Mount will do, one line per ticked disk, under the buttons so the
    // greyed Mount button is never a mystery and two ticks are never a
    // surprise about where the second disk went. Same shape as the Input
    // page's summary: a dimmed heading over the lines it introduces. With
    // nothing ticked the block instead says the one thing worth knowing
    // before ticking anything -- on hosts where attaching will raise the
    // system's privilege prompt, that it will; elsewhere, what to do next.
    let summary_top = host_disk_button_rects(rect)[0].1.y + LAUNCH_TAB_H + 10;
    let chosen: Vec<&crate::video::launcher::HostDiskRow> = setup
        .host_disks()
        .iter()
        .filter(|d| setup.host_disk_is_selected(&d.id))
        .collect();
    let warn_privilege = chosen.is_empty() && crate::blockdev::attaching_needs_privilege();
    draw_panel_text(
        frame,
        table.x,
        summary_top,
        if warn_privilege {
            "Warning:"
        } else {
            "With these settings:"
        },
        if warn_privilege {
            PANEL_TEXT_ACCENT
        } else {
            PANEL_TEXT_DIM
        },
        1,
        scale,
    );
    if chosen.is_empty() {
        if warn_privilege {
            draw_panel_text(
                frame,
                table.x + 8,
                summary_top + 16,
                "Attaching a host drive requires elevated privileges.",
                PANEL_TEXT_ACCENT,
                1,
                scale,
            );
        } else {
            draw_panel_text(
                frame,
                table.x + 8,
                summary_top + 16,
                "Select a disk to attach it to the machine",
                PANEL_TEXT,
                1,
                scale,
            );
        }
    }
    // A disk gets two lines if it needs them. The sentence ends with where the
    // disk is going, and on a long model name that is exactly the half a
    // single clipped line loses -- leaving a summary that says everything
    // except the thing it was written to say. Past two lines it is truncated,
    // because the summary cannot keep growing without reaching the panel edge.
    // Both lines start at the same edge: the wrap is one sentence running on,
    // not a list with sub-items, and stepping the second line in makes it read
    // as something subordinate to the first.
    let width_px = table.w.saturating_sub(8);
    let bottom = rect.y + rect.h;
    let mut y = summary_top + 16;
    for disk in &chosen {
        let access = if disk.writable {
            "read/write"
        } else {
            "read only"
        };
        let place = disk
            .attach
            .expect("a ticked disk has an attachment point")
            .label();
        let text = format!(
            "{} ({}): attached {access} to {place}",
            disk.id, disk.volume
        );
        let chars = width_px / font::GLYPH_W;
        let mut lines = wrap_text(&text, chars, chars);
        if lines.len() > 2 {
            let overflow = lines[1..].join(" ");
            lines.truncate(1);
            lines.push(truncate_to_width(&overflow, width_px));
        }
        for line in &lines {
            // Out of panel is not somewhere to draw: the rest of the page is
            // below this and would be written over.
            if y + HOST_DISK_ROW_H > bottom {
                return;
            }
            draw_panel_text(frame, table.x + 8, y, line, PANEL_TEXT, 1, scale);
            y += HOST_DISK_ROW_H;
        }
    }
}

/// A small square box, filled when set. The fill colour distinguishes what
/// is being answered: one page can carry more than one kind of tick.
/// A tick box is this square, wherever one is drawn.
pub(in crate::video::ui) const TICK_BOX: usize = 10;

pub(in crate::video::ui) fn draw_tick_box(
    frame: &mut [u8],
    x: usize,
    y: usize,
    set: bool,
    colour: u32,
    scale: usize,
) {
    let outer = Rect {
        x,
        y,
        w: TICK_BOX,
        h: TICK_BOX,
    };
    fill_rect(frame, scale_rect(outer, scale), ENTRY_BG, scale);
    draw_outline(frame, outer, BUTTON_EDGE_LIGHT, scale);
    if set {
        let inner = Rect {
            x: x + 2,
            y: y + 2,
            w: 6,
            h: 6,
        };
        fill_rect(frame, scale_rect(inner, scale), colour, scale);
    }
}

/// Truncate `text` (already a short file name) to fit `avail_px`, appending a
/// `~` marker when clipped.
pub(in crate::video::ui) fn truncate_to_width(text: &str, avail_px: usize) -> String {
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

/// Which slice of a line a box shows, and where in it the caret lands.
///
/// Answers `(first character shown, cell the caret is on)` for a line of
/// `len` characters in a box `cells` wide. The caret is kept off the edges
/// where there is text either side of it -- half a box of lead means
/// stepping through a long value moves the block, and only shifts the text
/// once the block reaches an end.
pub(in crate::video::ui) fn edit_window(len: usize, caret: usize, cells: usize) -> (usize, usize) {
    let first = caret
        .saturating_sub(cells / 2)
        .min(len.saturating_sub(cells));
    (first, caret - first)
}

/// Draw a line that is being typed into, with a block over the caret.
///
/// Every editable box in the launcher goes through here -- the value boxes
/// on the configuration pages and both WHDLoad dialogs -- so a caret means
/// the same thing wherever it is seen. A block rather than a bar: the font
/// is an 8x8 cell grid with no sub-pixel anywhere in it, and a one-pixel
/// line between two cells is easy to miss on a scaled-up panel.
///
/// The window on the text slides to keep the caret in view, so typing at
/// the end of something longer than the box pushes the head off the left
/// and stepping back to the front brings it home. An "..." marks a head
/// that has been scrolled past, and the caret cell is left free at the
/// right so a caret past the last character has somewhere to sit.
#[allow(clippy::too_many_arguments)]
pub(in crate::video::ui) fn draw_edit_line(
    frame: &mut [u8],
    x: usize,
    y: usize,
    text: &str,
    caret: usize,
    color: u32,
    bg: u32,
    avail_px: usize,
    scale: usize,
) {
    let chars: Vec<char> = text.chars().collect();
    // One cell is the caret's, so a full box still shows where typing goes.
    let cells = (avail_px / font::GLYPH_W).saturating_sub(1).max(1);
    let caret = caret.min(chars.len());
    let (first, cell) = edit_window(chars.len(), caret, cells);
    let mut shown: Vec<char> = chars.iter().skip(first).take(cells + 1).copied().collect();
    // Say the head was scrolled past, where the three cells it takes do not
    // land under the block: a caret sitting on a dot would be a lie about
    // what deleting there would remove.
    if first > 0 && cell >= 3 {
        shown[..3].fill('.');
    }
    let shown: String = shown.into_iter().collect();
    draw_panel_text(frame, x, y, &shown, color, 1, scale);
    // Half a cell wide: enough to be seen against the text at any scale,
    // narrow enough to leave most of the character it stands on legible.
    // It blinks, so it is also read as a caret rather than as a mark in
    // the value; out of phase, nothing is drawn and the character shows
    // whole.
    if !crate::video::caret_lit() {
        return;
    }
    let block = Rect {
        x: x + cell * font::GLYPH_W,
        y,
        w: (font::GLYPH_W / 2).max(1),
        h: font::GLYPH_H,
    };
    fill_rect(frame, scale_rect(block, scale), color, scale);
    let _ = bg;
}

/// Clip `text` to `avail_px`, keeping the TAIL and prefixing an ASCII "..."
/// when it does not fit, so a host directory's meaningful end (the leaf
/// dir) stays visible. The bitmap font is ASCII-only, so a real ellipsis
/// glyph cannot be drawn; "..." is the closest it can render. Mirrors
/// [`truncate_to_width`], which keeps the head instead, and
/// [`draw_edit_line`], which shows a window around the caret.
pub(in crate::video::ui) fn clip_text_tail(text: &str, avail_px: usize) -> String {
    let max_chars = avail_px / font::GLYPH_W;
    let len = text.chars().count();
    if len <= max_chars {
        return text.to_string();
    }
    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }
    let tail: String = text.chars().skip(len - (max_chars - 3)).collect();
    format!("...{tail}")
}

/// Clip a host path to `avail_px`, always keeping the final component (the file
/// name) whole: leading directories are dropped and replaced by a "..." prefix,
/// rather than cutting into the name. Splits on both `/` and `\` so Windows and
/// Unix paths work. If even the name alone is too wide, its tail is shown.
pub(in crate::video::ui) fn clip_path_keep_name(text: &str, avail_px: usize) -> String {
    clip_path_to_chars(text, avail_px / font::GLYPH_W)
}

/// [`clip_path_keep_name`] in characters rather than pixels, shared with the
/// status line (see `window::shorten_status_paths`).
pub(in crate::video) fn clip_path_to_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut comps: Vec<&str> = text.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    let name = comps.pop().unwrap_or(text);
    let sep = if text.contains('\\') { '\\' } else { '/' };
    // Grow from the name, prepending whole parent components while the result
    // (with its "..." prefix) still fits.
    let mut shown = name.to_string();
    for comp in comps.into_iter().rev() {
        let candidate = format!("{comp}{sep}{shown}");
        if 3 + 1 + candidate.chars().count() <= max_chars {
            shown = candidate;
        } else {
            break;
        }
    }
    let prefixed = format!("...{sep}{shown}");
    if prefixed.chars().count() <= max_chars {
        prefixed
    } else {
        // The file name alone does not fit; fall back to a plain tail clip.
        clip_text_tail(name, max_chars * font::GLYPH_W)
    }
}

/// A model-selector / tab button: a flat bevel that fills with the title-bar
/// blue when active/selected. Tabs label left, model buttons centred.
pub(in crate::video::ui) fn draw_launcher_chip(
    frame: &mut [u8],
    rect: Rect,
    label: &str,
    active: bool,
    hover: f32,
    align_left: bool,
    scale: usize,
) {
    let face = if active {
        light_face_to(PANEL_TITLE_BG, PANEL_TITLE_BG, NAV_FACE_ON, hover)
    } else {
        light_face(BUTTON_FACE, BUTTON_FACE_HOVER, hover)
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

pub(in crate::video::ui) fn draw_launcher_zorro(
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
        lit(hover, UiControl::LauncherZorroAdd),
        scale,
    );
    if setup.zorro_boards().is_empty() {
        draw_panel_text(
            frame,
            pane_x,
            launcher_row_y(rect, 0) + LAUNCH_NAV_BLOCK_H + 8,
            "No extra Zorro boards configured.",
            PANEL_TEXT_DIM,
            1,
            scale,
        );
    }
    for (row, item) in launcher_zorro_layout(setup) {
        let row_y = launcher_row_y(rect, row) + LAUNCH_NAV_BLOCK_H;
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
                    lit(hover, UiControl::LauncherZorroRemove(i)),
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
pub(in crate::video::ui) fn draw_launcher_board_option(
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
                lit(hover, UiControl::LauncherBoardToggle { board, opt }),
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
                lit(
                    hover,
                    UiControl::LauncherBoardCycle {
                        board,
                        opt,
                        forward: false,
                    },
                ),
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
                lit(
                    hover,
                    UiControl::LauncherBoardCycle {
                        board,
                        opt,
                        forward: true,
                    },
                ),
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
                lit(hover, UiControl::LauncherBoardBrowse { board, opt }),
                scale,
            );
            draw_text_button(
                frame,
                clear,
                "Clear",
                !value.is_empty(),
                lit(hover, UiControl::LauncherBoardClear { board, opt }),
                scale,
            );
        }
        K::String => {
            let editing = state.editing() == Some(EditTarget::BoardOption { board, opt });
            let vbox = launcher_board_value_rect(rect, row_y);
            draw_rect_bevel(
                frame,
                scale_rect(vbox, scale),
                BUTTON_EDGE_DARK,
                BUTTON_EDGE_LIGHT,
                scale,
            );
            light_edit_box(
                frame,
                vbox,
                UiControl::LauncherBoardEdit { board, opt },
                editing,
                scale,
            );
            if editing {
                draw_edit_line(
                    frame,
                    vbox.x + 4,
                    row_y + 8,
                    state.edit_buffer(),
                    state.edit_caret().at(),
                    PANEL_TEXT_HILIGHT,
                    PANEL_BG,
                    vbox.w.saturating_sub(8),
                    scale,
                );
            } else {
                let shown = truncate_to_width(&value, vbox.w.saturating_sub(8));
                draw_panel_text(frame, vbox.x + 4, row_y + 8, &shown, PANEL_TEXT, 1, scale);
            }
        }
    }
}

/// A thin divider line.
pub(in crate::video::ui) fn draw_launcher_divider(frame: &mut [u8], rect: Rect, scale: usize) {
    fill_rect(frame, scale_rect(rect, scale), BUTTON_EDGE_DARK, scale);
}

pub(in crate::video::ui) fn draw_launcher(
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
            lit(hover, UiControl::LauncherModel(model)),
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
    let whdload_entry = state.setup.whdload_enabled();
    let strip = launcher::tabs(whdload_entry);
    for (i, &tab) in strip.iter().enumerate() {
        draw_launcher_chip(
            frame,
            launcher_tab_rect(rect, i),
            tab.label(),
            state.tab.strip_tab() == tab,
            lit(hover, UiControl::LauncherTab(tab)),
            true,
            scale,
        );
    }
    // Active tab content in the settings pane, shifted down past the top nav
    // when the tab has one.
    let row_offset = if state.tab.has_top_nav() {
        launcher_nav_block_h(state.tab)
    } else {
        0
    };
    if state.tab == LauncherTab::Zorro {
        draw_launcher_zorro(frame, rect, state, hover, scale);
    } else {
        for (i, r) in state
            .rows()
            .iter()
            .filter(|r| state.setup.row_on_page(state.tab, r.field))
            .enumerate()
        {
            draw_launcher_row(frame, rect, state, r, i, row_offset, hover, scale);
        }
    }
    // Nav row at the top of the pane: a Back button when this is a sub-page,
    // then its sibling links, with the current one highlighted. A page can
    // show both -- the Create Image pages say where they came from and which
    // of the two they are.
    let back_parent = state.tab.parent_tab();
    let options = state.tab.nav_options();
    let mut slot = 0;
    if let Some(parent) = back_parent {
        draw_text_button(
            frame,
            launcher_back_button_rect(rect),
            "< Back",
            true,
            lit(hover, UiControl::LauncherNavTab(parent)),
            scale,
        );
        slot = 1;
    }
    for (i, &(label, target)) in options.iter().enumerate() {
        draw_launcher_chip(
            frame,
            launcher_nav_button_rect(rect, slot + i),
            label,
            target == state.tab,
            lit(hover, UiControl::LauncherNavTab(target)),
            false,
            scale,
        );
    }
    // The rest of the boot order, when there is more of it than one page
    // holds. Next to Back, in the slot a sibling link would have taken.
    if let Some((label, target)) = boot_page_button(state) {
        draw_text_button(
            frame,
            launcher_nav_button_rect(rect, slot),
            label,
            true,
            lit(hover, UiControl::LauncherNavTab(target)),
            scale,
        );
    }
    #[cfg(feature = "game-library")]
    if state.tab == LauncherTab::WhdloadLibrary {
        draw_library_page(frame, rect, state, hover, scale);
    }
    if state.tab == LauncherTab::HostDisk {
        draw_host_disk_page(frame, rect, state, hover, scale);
    }
    if state.tab == LauncherTab::Netplay {
        let top = launcher_row_y(rect, 10) + row_offset;
        for (i, line) in if state.netplay.internet {
            [
                "Use the same build, machine, ROM and floppy contents.",
                "Host: new invitation, copy code, then Run. Join: paste, Run.",
                "Blank relay uses n0's public service; custom URL optional.",
                "Run connects. F11 disconnects. Guest uses host timing.",
            ]
        } else {
            [
                "Use the same machine, ROM and floppy contents.",
                "Share one session code; choose opposite players.",
                "Netplay sets digital ports, serial off and interpreter.",
                "Run connects. F11 returns here. Settings last this session.",
            ]
        }
        .iter()
        .enumerate()
        {
            draw_panel_text(
                frame,
                launcher_pane_x(rect),
                top + i * 14,
                line,
                PANEL_TEXT_DIM,
                1,
                scale,
            );
        }
    }
    // The Input tab spells out what the chosen wiring means: which host
    // input source ends up driving each port, live as the values cycle.
    if state.tab == LauncherTab::Input {
        let summary_top = launcher_row_y(
            rect,
            launcher::rows(
                LauncherTab::Input,
                state.setup.parallel_device(),
                state.setup.serial_mode(),
                state.setup.midi_out_is_mt32(),
                state.setup.midi_out_is_csynth(),
            )
            .len()
                + 1,
        ) + row_offset;
        draw_panel_text(
            frame,
            launcher_pane_x(rect),
            summary_top,
            "With these settings:",
            PANEL_TEXT_DIM,
            1,
            scale,
        );
        for (i, line) in setup.input_routing_summary().iter().enumerate() {
            draw_panel_text(
                frame,
                launcher_pane_x(rect) + 8,
                summary_top + 16 + i * 14,
                line,
                PANEL_TEXT,
                1,
                scale,
            );
        }
    }
    // The geometry editor says what the figures come to, because on this
    // page the geometry -- not the Size box -- decides how big the image is.
    if state.tab == LauncherTab::CreateGeometry {
        let g = state.workshop.custom_geometry;
        let note_top = launcher_row_y(
            rect,
            launcher::rows(
                LauncherTab::CreateGeometry,
                state.setup.parallel_device(),
                state.setup.serial_mode(),
                state.setup.midi_out_is_mt32(),
                state.setup.midi_out_is_csynth(),
            )
            .len()
                + 1,
        ) + row_offset
            + 14;
        draw_panel_text(
            frame,
            launcher_pane_x(rect),
            note_top,
            "Info:",
            PANEL_TEXT_DIM,
            1,
            scale,
        );
        draw_panel_text(
            frame,
            launcher_pane_x(rect) + 2 * font::GLYPH_W,
            note_top + 16,
            &{
                let size = crate::config::format_size(g.bytes() as usize);
                format!(
                    "These values will create {} {size} disk image.",
                    indefinite_article(&size)
                )
            },
            PANEL_TEXT,
            1,
            scale,
        );
    }
    // The Boot Priority page spells out the valid priority range below the
    // rows, under a dimmed "Info:" heading.
    // The first page only: a page holds nine drives at most, so the note
    // always has its room under them, and the second page needs no second
    // copy of it.
    if state.tab == LauncherTab::BootPriority && state.setup.has_boot_priority_rows() {
        // Below a full page of drives, whether or not this machine has one:
        // the note keeps the same place on the page however many rows are
        // drawn above it, rather than riding up and down with the count.
        let below_a_full_page = launcher::BOOTPRI_PAGE_ROWS + 2;
        let help_top = (launcher_row_y(rect, below_a_full_page) + row_offset).saturating_sub(10);
        draw_panel_text(
            frame,
            launcher_pane_x(rect),
            help_top,
            "Info:",
            PANEL_TEXT_DIM,
            1,
            scale,
        );
        for (i, line) in [
            "Valid boot priorities are any value between 127 (highest) and",
            "-128 (disabled).",
        ]
        .iter()
        .enumerate()
        {
            draw_panel_text(
                frame,
                launcher_pane_x(rect) + 8,
                help_top + 16 + i * 14,
                line,
                PANEL_TEXT,
                1,
                scale,
            );
        }
    }
    // NAT and bridged backends deliver inbound traffic on the host's schedule,
    // so warn that runs stop being reproducible the moment packets flow
    // (loopback and an isolated NIC stay deterministic).
    if state.tab == LauncherTab::IoNetworking && setup.ethernet_breaks_determinism() {
        let note_top = launcher_row_y(
            rect,
            launcher::rows(
                LauncherTab::IoNetworking,
                state.setup.parallel_device(),
                state.setup.serial_mode(),
                state.setup.midi_out_is_mt32(),
                state.setup.midi_out_is_csynth(),
            )
            .len()
                + 1,
        ) + row_offset;
        draw_panel_text(
            frame,
            launcher_pane_x(rect),
            note_top,
            "Warning: host networking is non-deterministic.",
            PANEL_TEXT_ACCENT,
            1,
            scale,
        );
        for (i, line) in [
            "Inbound traffic follows the host clock, so input recordings",
            "and save-state replays are not byte-identical while it flows.",
        ]
        .iter()
        .enumerate()
        {
            draw_panel_text(
                frame,
                launcher_pane_x(rect) + 8,
                note_top + 16 + i * 14,
                line,
                PANEL_TEXT,
                1,
                scale,
            );
        }
    }
    // Status / error line.
    if let Some(status) = &state.status {
        let color = match status.kind {
            launcher::StatusKind::Ok => PANEL_TEXT_HILIGHT,
            // Work in progress and a failure share the warning colour:
            // both are "not finished, and worth your attention", and only
            // a line that says something worked has earned the green.
            launcher::StatusKind::Busy | launcher::StatusKind::Error => PANEL_TEXT_ACCENT,
        };
        // Kept inside the panel. A failure explains itself at whatever length
        // it needs to, and one long enough to run past the edge is drawn over
        // the window either side of it -- so it is clipped here, with the log
        // holding the whole of what went wrong.
        let text = truncate_to_width(&status.text, rect.w.saturating_sub(20));
        draw_panel_text(
            frame,
            rect.x + 10,
            launcher_status_y(rect),
            &text,
            color,
            1,
            scale,
        );
    }
    // Action bar. While the Save dialog is up, every position outside
    // its three buttons answers as the Save control (a stray click puts
    // the dialog away), so pointer-lighting the button under it would
    // flash on every hover in the dialog -- the button stays unlit until
    // the dialog is gone.
    for (control, button_rect) in launcher_action_rects(rect) {
        let light = if control == UiControl::LauncherSave && state.save_dialog {
            0.0
        } else {
            lit(hover, control)
        };
        draw_text_button(
            frame,
            button_rect,
            launcher_action_label(control),
            true,
            light,
            scale,
        );
    }
    draw_launcher_save_dialog(frame, rect, state, hover, scale);
    draw_launcher_confirm(frame, rect, state, hover, scale);
    // Over everything, because it is the only thing being answered while
    // it is up.
    #[cfg(feature = "game-library")]
    if state.login.is_some() {
        draw_login_dialog(frame, rect, state, hover, scale);
    }
    #[cfg(feature = "game-library")]
    if state.meta.is_some() {
        draw_meta_dialog(frame, rect, state, hover, scale);
    }
}
