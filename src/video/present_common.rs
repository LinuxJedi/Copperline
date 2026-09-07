// SPDX-License-Identifier: GPL-3.0-or-later

//! Frontend-independent presentation helpers: the post-render pass that
//! turns a rendered field into a presentable frame (vertical/horizontal
//! recentring, the TV overscan bezel mask, programmable-scan stretch) and
//! the aperture/geometry predicates around it. Moved out of
//! `video/window/present.rs` so headless consumers (`cpu.rs`'s debug
//! screenshots, the wasm browser frontend) can present frames without the
//! winit frontend; `window/present.rs` re-exports everything here, so the
//! desktop path is unchanged.

use super::{bitplane, deinterlace::OUT_HEIGHT, FrameGeometry, FB_HEIGHT, FB_PIXELS, FB_WIDTH};
use crate::bus::RenderRegisterSnapshot;
use crate::config::{Overscan, TvCentre};
use crate::screenshot;

pub const fn rgba(r: u32, g: u32, b: u32) -> u32 {
    0xFF00_0000 | (b << 16) | (g << 8) | r
}

/// Compose the active RTG board frame (Z3660 scanout). The board frame lands
/// in `scratch` at its native resolution; `out` gets an FB_WIDTH-stride copy
/// (nearest, one output row per board row) for the screenshot path, which
/// reads the shared presentation buffer. Returns
/// `(present_rows, native_w, native_h)`, or `None` while no RTG board drives
/// the display. The window presents the native `scratch` frame directly (see
/// [`crate::video::window`]); `out` exists only for the screenshot path.
pub fn compose_rtg_present(
    bus: &crate::bus::Bus,
    scratch: &mut Vec<u32>,
    out: &mut Vec<u32>,
) -> Option<(usize, u32, u32)> {
    let (w, h) = bus.rtg_frame(scratch)?;
    let (w, h) = (w as usize, h as usize);
    out.clear();
    out.resize(h * FB_WIDTH, 0xFF00_0000);
    for y in 0..h {
        let src = &scratch[y * w..(y + 1) * w];
        let dst = &mut out[y * FB_WIDTH..(y + 1) * FB_WIDTH];
        for (x, px) in dst.iter_mut().enumerate() {
            // Sample the centre of each output pixel's source span, not its
            // left edge: `x * w / FB_WIDTH` tops out below `w - 1` whenever
            // the board frame is wider than FB_WIDTH, so the rightmost
            // source columns never reach a downscaled screenshot.
            *px = src[(2 * x + 1) * w / (2 * FB_WIDTH)];
        }
    }
    Some((h, w as u32, h as u32))
}

pub const STANDARD_PAL_VISIBLE_WIDTH: usize = 320 * 2;
pub const STANDARD_PAL_VISIBLE_LINES: usize = 256;
pub const STANDARD_NTSC_VISIBLE_LINES: usize = 200;
pub const STANDARD_PAL_VISIBLE_START_VPOS: u32 = 0x2C;
// Default TV presentation keeps a small consumer-visible overscan margin while
// still hiding the deep edge columns that often contain unfinished effects.
pub const TV_HORIZONTAL_OVERSCAN_MARGIN: usize = 24 * 2;
// Overscan field lines the TV aperture keeps above and below the standard
// window.
pub const TV_VERTICAL_OVERSCAN_MARGIN: usize = 7;

// The TV aperture for standard 15 kHz displays, cropped from the woven
// presentation buffer. Horizontally both presentation paths (live window
// and PNG) show the captured aperture (`TV_CAPTURED_*` below) resampled
// onto the framebuffer-wide 4:3 glass by `tv_glass_sample`; the two video
// standards agree -- both place the standard 640-wide window at the same
// colour clocks -- so one cutout serves 50 Hz and 60 Hz scans alike.
// Vertically the aperture follows the field line count: a 312/313-line
// (50 Hz) scan carries the 256-line standard window, a 262/263-line
// (60 Hz) scan the 200-line one, each cropped with the same symmetric
// overscan margin.
pub const TV_PRESENT_SOURCE_Y: usize = 18;
pub const TV_PAL_PRESENT_HEIGHT: usize =
    (STANDARD_PAL_VISIBLE_LINES + 2 * TV_VERTICAL_OVERSCAN_MARGIN) * 2;
pub const TV_NTSC_PRESENT_HEIGHT: usize =
    (STANDARD_NTSC_VISIBLE_LINES + 2 * TV_VERTICAL_OVERSCAN_MARGIN) * 2;

// Output rows a TV-aperture crop presents: both standards' apertures fill
// the same 4:3 glass, so the crop always presents at the 50 Hz aperture's
// native row count. A 50 Hz crop maps 1:1; a 60 Hz crop's 428 rows stretch
// onto it, the taller picture lines of a 200-line display on the same
// screen.
pub const TV_GLASS_PRESENT_ROWS: usize = TV_PAL_PRESENT_HEIGHT;

// The tube aperture, for the live window while a monitor bezel is drawn:
// a real 1084's glass shows more of the raster than the TV aperture
// keeps -- roughly 52 us of each line and ~288 of a 50 Hz scan's lines,
// which exceeds even the whole captured field -- so the drawn tube
// presents every captured row of the scan, resampled onto the same
// glass. The raster stays flush with the glass, as a real set's
// overscanned raster is: the border colour reaches the glass edges, and
// the tube's rounded corners crop into that border instead of into the
// standard window, which the extra rows keep clear of the arcs. One
// height per standard, like the TV aperture's pair: the whole 285-line
// 50 Hz field (PAL_LINES beam lines less the 28 the capture starts at),
// and the 235 lines a 263-line 60 Hz field scans -- both sized for the
// standard's full frame, so a short interlaced field's missing last
// line shows as the black row `blank_rows_past_frame_end` already
// leaves past its frame wrap, exactly like the vertical blank it is.
// Captures (screenshots, frame dumps, recordings, headless runs) are
// presentation-independent and keep the TV aperture.
pub const TUBE_PAL_PRESENT_HEIGHT: usize = OUT_HEIGHT;
pub const TUBE_NTSC_PRESENT_HEIGHT: usize = OUT_HEIGHT
    - 2 * (crate::chipset::agnus::PAL_LINES - crate::chipset::agnus::NTSC_LINES) as usize;

/// The tube-glass crop for a scan the TV aperture already classified:
/// the whole rendered field of the same standard, taken from woven row 0.
pub const fn tube_aperture_rows(tv_aperture_rows: usize) -> usize {
    if tv_aperture_rows >= TV_PAL_PRESENT_HEIGHT {
        TUBE_PAL_PRESENT_HEIGHT
    } else {
        TUBE_NTSC_PRESENT_HEIGHT
    }
}

// The TV aperture clipped to columns the framebuffer actually captures, for
// frontends whose frame should end on real pixels instead of black bezel
// (the browser canvas hugs its border on every side). The margin is the
// captured right-overscan width, mirrored to the left so the standard
// window stays exactly centred; the right edge lands on the framebuffer's
// edge by construction.
pub const TV_CAPTURED_MARGIN_X: usize =
    FB_WIDTH - bitplane::STANDARD_VISIBLE_X0 - STANDARD_PAL_VISIBLE_WIDTH;
pub const TV_CAPTURED_SOURCE_X: usize = bitplane::STANDARD_VISIBLE_X0 - TV_CAPTURED_MARGIN_X;
pub const TV_CAPTURED_WIDTH: usize = STANDARD_PAL_VISIBLE_WIDTH + 2 * TV_CAPTURED_MARGIN_X;

// The captured aperture's invariants: it ends exactly on the framebuffer
// edge (no bezel columns), keeps symmetric margins around the standard
// window, starts inside the reference aperture, and both standards'
// vertical crops fit the woven field.
const _: () = {
    assert!(TV_CAPTURED_SOURCE_X + TV_CAPTURED_WIDTH == FB_WIDTH);
    assert!(bitplane::STANDARD_VISIBLE_X0 - TV_CAPTURED_SOURCE_X == TV_CAPTURED_MARGIN_X);
    assert!(
        TV_CAPTURED_SOURCE_X + TV_CAPTURED_WIDTH
            - (bitplane::STANDARD_VISIBLE_X0 + STANDARD_PAL_VISIBLE_WIDTH)
            == TV_CAPTURED_MARGIN_X
    );
    assert!(TV_PRESENT_SOURCE_Y + TV_PAL_PRESENT_HEIGHT <= OUT_HEIGHT);
    assert!(TV_NTSC_PRESENT_HEIGHT < TV_PAL_PRESENT_HEIGHT);
    // Each standard's TV aperture sits inside its tube aperture, so the
    // tube view only ever adds raster around the TV view.
    assert!(TV_PRESENT_SOURCE_Y + TV_PAL_PRESENT_HEIGHT <= TUBE_PAL_PRESENT_HEIGHT);
    assert!(TV_PRESENT_SOURCE_Y + TV_NTSC_PRESENT_HEIGHT <= TUBE_NTSC_PRESENT_HEIGHT);
};

/// The framebuffer offsets a [`TvCentre`] setting moves the TV aperture's
/// source window by: the picture moves right/down, so the window moves
/// left/up. `.0` in framebuffer pixels, `.1` in woven rows.
pub const fn tv_centre_source_offset(centre: TvCentre) -> (i32, i32) {
    (-2 * centre.h, -2 * centre.v)
}

/// One glass pixel of the 4:3 TV presentation: the captured aperture's
/// `TV_CAPTURED_WIDTH` real columns fill the `FB_WIDTH`-wide glass through
/// a fractional resample (`blend_rgba`, the programmable-scan idiom), the
/// way a real set's raster fills its glass. Every glass pixel derives from
/// real captured pixels -- nothing is padded black -- and the standard
/// window stays exactly centred because the captured margins are symmetric
/// by construction. 8.8 fixed point, sampling between texel centres.
///
/// `source_x_offset` slides the sampled window across the framebuffer (the
/// H-centre control, via [`tv_centre_source_offset`]); glass pushed past
/// the captured raster is unscanned and comes back black.
#[inline]
pub fn tv_glass_sample(row: &[u32], out_x: usize, source_x_offset: i32) -> u32 {
    debug_assert!(row.len() >= FB_WIDTH);
    let s = (TV_CAPTURED_SOURCE_X as i64 + source_x_offset as i64) * 256
        + ((2 * out_x as i64 + 1) * (TV_CAPTURED_WIDTH as i64) * 256) / (2 * FB_WIDTH as i64)
        - 128;
    // Half a texel of slack on each side: the default aperture's own edge
    // samples land there and clamp, exactly as before the offset existed.
    if !(-128..=(FB_WIDTH as i64 - 1) * 256 + 128).contains(&s) {
        return rgba(0, 0, 0);
    }
    let s = s.clamp(0, (FB_WIDTH as i64 - 1) * 256);
    let i = (s >> 8) as usize;
    let frac = (s & 255) as u32;
    crate::video::blend_rgba(row[i], row[(i + 1).min(FB_WIDTH - 1)], frac)
}

/// Turn a rendered field into a presentable frame in place, and report
/// how it was placed ([`FieldPlacement`]). Returns the placement rather
/// than only its row count so a coordinate stated in field space -- the
/// autocrop content envelope -- can follow the pixels through the same
/// maps, and the two cannot disagree.
pub fn post_process_rendered_field(
    fb: &mut [u32],
    geometry: FrameGeometry,
    canvas_scale: usize,
    presentation_h_window: Option<(i32, u32)>,
    presentation_v_window: Option<(i32, u32)>,
    visible_start_vpos: u32,
    h_shift: usize,
    overscan: Overscan,
) -> FieldPlacement {
    let canvas_width = FB_WIDTH * canvas_scale;
    let field_rows = geometry.visible_lines.min(fb.len() / canvas_width);
    // Vertical centring, optional full-overscan horizontal recentring, and the
    // TV bezel mask are 15 kHz CRT concepts anchored to the standard PAL/NTSC
    // window; a programmable scan defines its own window and presents in full,
    // like a multisync monitor.
    if !geometry.programmable {
        // Standard scans always render the classic canvas pitch
        // (`bitplane::canvas_scale_for`).
        debug_assert_eq!(canvas_scale, 1);
        center_present_frame_for_visible_start(fb, visible_start_vpos);
        center_present_frame_horizontally(fb, h_shift);
        if overscan == Overscan::Tv {
            mask_present_frame_to_tv(fb, h_shift, standard_window_top_row(visible_start_vpos));
        }
        return FieldPlacement {
            rows: field_rows,
            canvas_scale,
            map: PlacementMap::Standard {
                y_offset: presentation_source_y_offset(visible_start_vpos),
                h_shift,
            },
        };
    }
    let columns = if let Some((src_x0, src_w)) = presentation_h_window {
        // A multisync monitor locks its horizontal deflection to the
        // programmed sync pulse: the glass shows the line from the sync
        // trailing edge to the next pulse, centring the picture the way
        // the mode's own porches place it. The window is computed in
        // classic-canvas pixels; scale it to this frame's pitch.
        let (src_x0, src_w) = (src_x0 * canvas_scale as i32, src_w * canvas_scale as u32);
        screenshot::stretch_rows_x_window(fb, canvas_width, field_rows, src_x0, src_w);
        ColumnMap::Window { src_x0, src_w }
    } else if geometry.line_cck != 227 {
        // No programmed sync to anchor on: fall back to the time-linear
        // whole-line map (each colour clock of this scan's shorter/longer
        // line covers 227/line_cck of the glass a standard line's clock
        // would).
        screenshot::stretch_rows_x(fb, canvas_width, field_rows, geometry.line_cck, 227);
        ColumnMap::Linear {
            src_num: geometry.line_cck,
            src_den: 227,
        }
    } else {
        ColumnMap::Identity
    };
    let rows =
        presentation_v_window_placement(field_rows, fb.len() / canvas_width, presentation_v_window);
    place_presentation_v_window(fb, canvas_width, field_rows, rows);
    FieldPlacement {
        rows: rows.rows,
        canvas_scale,
        map: PlacementMap::Programmable { columns, rows },
    }
}

/// How [`post_process_rendered_field`] placed a rendered field on the
/// presentation buffer: the rows it left, and the maps it moved the
/// pixels through. Built by the function from what it did, so a
/// field-space coordinate mapped through it ([`Self::content_rect`])
/// lands exactly where the pixels went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldPlacement {
    /// Rows of the placed field.
    pub rows: usize,
    /// The canvas pitch the field was rendered at
    /// (`bitplane::canvas_scale_for`): 1, or 2 for a 35 ns canvas.
    canvas_scale: usize,
    map: PlacementMap,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlacementMap {
    /// A standard scan: rows moved down by the vertical centring, columns
    /// left by the full-overscan recentring shift.
    Standard { y_offset: usize, h_shift: usize },
    /// A programmable scan: columns resampled onto the glass width, rows
    /// placed inside the sync-anchored vertical window.
    Programmable {
        columns: ColumnMap,
        rows: VerticalPlacement,
    },
}

/// The horizontal resample a programmable scan's line takes onto the
/// glass width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ColumnMap {
    /// A standard-length line with no programmed sync: columns stay put.
    Identity,
    /// [`screenshot::stretch_rows_x_window`], the sync-anchored window.
    Window { src_x0: i32, src_w: u32 },
    /// [`screenshot::stretch_rows_x`], the time-linear whole-line map.
    Linear { src_num: u32, src_den: u32 },
}

impl ColumnMap {
    /// The source taps output column `x` of a `width`-column row reads.
    fn source(self, x: usize, width: usize) -> (usize, u32) {
        match self {
            Self::Identity => (x, 0),
            Self::Window { src_x0, src_w } => {
                screenshot::stretch_rows_x_window_source(x, width, src_x0, src_w)
            }
            Self::Linear { src_num, src_den } => {
                screenshot::stretch_rows_x_source(x, width, src_num, src_den)
            }
        }
    }
}

impl FieldPlacement {
    /// A standard placement, for callers that present a field the
    /// classic way without the post-process pass.
    pub fn standard(rows: usize, visible_start_vpos: u32, h_shift: usize) -> Self {
        Self {
            rows,
            canvas_scale: 1,
            map: PlacementMap::Standard {
                y_offset: presentation_source_y_offset(visible_start_vpos),
                h_shift,
            },
        }
    }

    /// Follow a content envelope the renderer stated in field canvas
    /// space (`x` in hi-res-pitch pixels, `y` in rendered field rows)
    /// through this placement and onto the `present_rows`-row buffer the
    /// deinterlacer built from the placed field: the buffer rows and
    /// columns that show any of the envelope. A standard field takes the
    /// centring and recentring shifts, then two woven rows per field row;
    /// a programmable field's columns are scanned against the resample's
    /// own taps (so a partially blended edge column counts, and the
    /// sync tail left of the capture, which clamps to the edge column,
    /// counts when that column is picture), its rows are the captured
    /// rows the vertical window kept, and the deinterlacer's row count
    /// says whether the placed rows were woven (LACE) or passed through
    /// at native height. `None` when nothing of the envelope reaches the
    /// buffer -- a picture entirely off the glass.
    pub fn content_rect(
        &self,
        rect: bitplane::ContentRect,
        present_rows: usize,
    ) -> Option<bitplane::ContentRect> {
        let width = FB_WIDTH * self.canvas_scale;
        // The renderer's x is the hi-res pitch; a 35 ns canvas fans each
        // logical pixel out to `canvas_scale` columns.
        let (x0, x1) = (rect.x0 * self.canvas_scale, rect.x1 * self.canvas_scale);
        let (x0, x1, y0, y1) = match self.map {
            PlacementMap::Standard { y_offset, h_shift } => {
                let x0 = x0.saturating_sub(h_shift).min(width);
                let x1 = x1.saturating_sub(h_shift).clamp(x0, width);
                let y0 = (rect.y0 + y_offset).min(self.rows);
                let y1 = (rect.y1 + y_offset).clamp(y0, self.rows);
                (x0, x1, y0, y1)
            }
            PlacementMap::Programmable { columns, rows } => {
                let mut shown = (0..width).filter(|&x| {
                    screenshot::resampled_column_reads(columns.source(x, width), width, x0, x1)
                });
                let first = shown.next()?;
                let last = shown.next_back().unwrap_or(first);
                let kept = rows.skip_top..rows.skip_top + rows.content_rows;
                let y0 = rect.y0.max(kept.start);
                let y1 = rect.y1.min(kept.end);
                if y1 <= y0 {
                    return None;
                }
                let placed = |y: usize| y - rows.skip_top + rows.pad_top;
                (first, last + 1, placed(y0), placed(y1))
            }
        };
        // The deinterlacer wove or line-doubled the placed rows onto the
        // buffer (two per row), or passed a programmable progressive field
        // through at native height.
        let row_scale = if present_rows >= 2 * self.rows { 2 } else { 1 };
        let y0 = (row_scale * y0).min(present_rows);
        let y1 = (row_scale * y1).clamp(y0, present_rows);
        (x1 > x0 && y1 > y0).then_some(bitplane::ContentRect { x0, x1, y0, y1 })
    }
}

/// Where [`apply_presentation_v_window`] puts the captured rows of a
/// `field_rows`-row field: the rows it skips off the glass top, the
/// blanked rows it pads above them, how many captured rows it keeps, and
/// the rows of the result. Without a programmed vertical sync, or with a
/// glass no taller than the field, the field is left as it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VerticalPlacement {
    pub skip_top: usize,
    pub pad_top: usize,
    pub content_rows: usize,
    pub rows: usize,
}

pub fn presentation_v_window_placement(
    field_rows: usize,
    buf_rows: usize,
    presentation_v_window: Option<(i32, u32)>,
) -> VerticalPlacement {
    let unplaced = VerticalPlacement {
        skip_top: 0,
        pad_top: 0,
        content_rows: field_rows,
        rows: field_rows,
    };
    let Some((v_offset, glass_rows)) = presentation_v_window else {
        return unplaced;
    };
    let glass_rows = (glass_rows as usize).min(buf_rows).max(1);
    if glass_rows <= field_rows {
        return unplaced;
    }
    // Rows the sync-anchored glass top cuts off the capture (offset < 0)
    // vs. blanked glass rows above the captured window (offset > 0).
    let skip_top = (-v_offset).max(0) as usize;
    let pad_top = (v_offset).max(0) as usize;
    if skip_top >= field_rows || pad_top >= glass_rows {
        return VerticalPlacement {
            skip_top,
            pad_top,
            content_rows: 0,
            rows: glass_rows,
        };
    }
    VerticalPlacement {
        skip_top,
        pad_top,
        content_rows: (field_rows - skip_top).min(glass_rows - pad_top),
        rows: glass_rows,
    }
}

/// Vertical counterpart of the sync-anchored horizontal window: place the
/// captured rows inside the glass's full vertical span (the frame minus the
/// programmed sync pulse), bordered with blanked rows where the mode's own
/// porches put them, and return the new row count. Without a programmed
/// vertical sync the captured rows keep covering the whole glass height.
pub fn apply_presentation_v_window(
    fb: &mut [u32],
    canvas_width: usize,
    field_rows: usize,
    presentation_v_window: Option<(i32, u32)>,
) -> usize {
    let placement =
        presentation_v_window_placement(field_rows, fb.len() / canvas_width, presentation_v_window);
    place_presentation_v_window(fb, canvas_width, field_rows, placement);
    placement.rows
}

/// Move a `field_rows`-row field's pixels where
/// [`presentation_v_window_placement`] decided.
fn place_presentation_v_window(
    fb: &mut [u32],
    canvas_width: usize,
    field_rows: usize,
    placement: VerticalPlacement,
) {
    let VerticalPlacement {
        skip_top,
        pad_top,
        content_rows,
        rows: glass_rows,
    } = placement;
    if glass_rows <= field_rows {
        return;
    }
    let black = rgba(0, 0, 0);
    if content_rows == 0 {
        fb[..glass_rows * canvas_width].fill(black);
        return;
    }
    if pad_top > skip_top {
        for row in (0..content_rows).rev() {
            let src = (skip_top + row) * canvas_width;
            let dst = (pad_top + row) * canvas_width;
            fb.copy_within(src..src + canvas_width, dst);
        }
    } else if pad_top < skip_top {
        for row in 0..content_rows {
            let src = (skip_top + row) * canvas_width;
            let dst = (pad_top + row) * canvas_width;
            fb.copy_within(src..src + canvas_width, dst);
        }
    }
    fb[..pad_top * canvas_width].fill(black);
    fb[(pad_top + content_rows) * canvas_width..glass_rows * canvas_width].fill(black);
}

/// `[display] overscan = "tv"`: black out the deep-overscan margins like the
/// bezel of a CRT. Demos routinely leave junk in the deep overscan (e.g. HAM
/// streams that converge off-screen, as on the 9 Fingers title); a TV hides
/// it and so does this mask. The emulated framebuffer itself always carries
/// the full field; this runs on the presentation copy only.
///
/// The window is a realistic PAL TV visible area rather than the bare standard
/// window: real sets show a margin of overscan, which intentional overscan
/// displays rely on, while the deep-overscan junk the mask exists to hide sits
/// further out. Default TV presentation keeps 24 lo-res pixels of horizontal
/// overscan beside the standard PAL window; full overscan remains available
/// through `Overscan::Full`. The mask is horizontal only: vertical border
/// colour changes are part of the Denise output and can be intentional
/// effects, so rows above or below the standard display remain as rendered.
/// `h_shift` is any horizontal presentation shift already applied to the
/// frame, so the bezel tracks the shifted picture instead of clipping its left
/// edge.
pub fn mask_present_frame_to_tv(fb: &mut [u32], h_shift: usize, _standard_top_row: usize) {
    debug_assert!(fb.len() >= FB_PIXELS);
    let black = rgba(0, 0, 0);
    let (source_left, source_right) = tv_source_h_bounds();
    let left = source_left.saturating_sub(h_shift);
    let right = source_right.saturating_sub(h_shift).min(FB_WIDTH).max(left);
    for row in fb.chunks_mut(FB_WIDTH) {
        row[..left].fill(black);
        if right < FB_WIDTH {
            row[right..].fill(black);
        }
    }
}

/// The framebuffer row carrying the standard window's first line after
/// `center_present_frame_for_visible_start` has run: the centring shift
/// plus however many overscan rows were already visible above it.
pub fn standard_window_top_row(visible_start_vpos: u32) -> usize {
    let overscan_rows_already_visible =
        STANDARD_PAL_VISIBLE_START_VPOS.saturating_sub(visible_start_vpos) as usize;
    presentation_source_y_offset(visible_start_vpos) + overscan_rows_already_visible
}

/// Shift the rendered frame left by `shift` framebuffer pixels, filling the
/// vacated right columns with black. Used to recentre a standard display
/// whose deep left overscan would otherwise push the picture right of
/// centre. A no-op when `shift` is 0 (overscan frames).
pub fn center_present_frame_horizontally(fb: &mut [u32], shift: usize) {
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

pub fn center_present_frame_for_visible_start(fb: &mut [u32], visible_start_vpos: u32) {
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

pub fn presentation_source_y_offset(visible_start_vpos: u32) -> usize {
    let standard_offset = FB_HEIGHT.saturating_sub(STANDARD_PAL_VISIBLE_LINES) / 2;
    let overscan_rows_already_visible =
        STANDARD_PAL_VISIBLE_START_VPOS.saturating_sub(visible_start_vpos) as usize;
    standard_offset.saturating_sub(overscan_rows_already_visible)
}

/// Presentation-geometry decisions carried across border-only frames.
///
/// The TV aperture crop and the full-overscan recentring shift are judged
/// from each frame's playfield, but a border-only frame has no playfield to
/// judge -- and screen changes blank the display for a frame or two while
/// Intuition rebuilds the copper list. Deciding those frames "full
/// framebuffer" made the whole picture lurch sideways (and rescale) at
/// every Kickstart screen change, conspicuous inside the monitor bezel's
/// fixed frame. A border-only frame instead keeps the previous decision --
/// the monitor does not move between screens -- with the stock standard
/// display as the power-on default. The two decisions latch independently:
/// the shift resolves when a frame is submitted for rendering, the
/// aperture when its presentation comes back, and each is only consumed by
/// its own overscan mode.
#[derive(Clone, Debug)]
pub struct PresentationLatch {
    standard_aperture: bool,
    h_shift: usize,
}

impl Default for PresentationLatch {
    fn default() -> Self {
        Self {
            standard_aperture: true,
            h_shift: bitplane::standard_present_h_shift(),
        }
    }
}

impl PresentationLatch {
    /// Whether neutral fields retain the standard TV aperture. Frontends
    /// that save presentation state can restore this classification by
    /// resolving a `Standard` or `Full` frame before the next field.
    pub fn uses_standard_aperture(&self) -> bool {
        self.standard_aperture
    }

    /// Back to the power-on default, for presentation discontinuities
    /// (machine swap, reset, state load).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// The horizontal recentring shift for one frame under `overscan`,
    /// latching the decision of any frame that carries content.
    pub fn presentation_h_shift(
        &mut self,
        snapshot: &RenderRegisterSnapshot,
        overscan: Overscan,
    ) -> usize {
        match overscan {
            // TV mode is an aperture over the emulated framebuffer, matching
            // the fixed source cutout used by reference emulators. Do not
            // copy pixels sideways here: a standard hi-res screen already
            // occupies the right edge of Copperline's 716-pixel cutout.
            Overscan::Tv => 0,
            Overscan::Full => match bitplane::horizontal_content_class(snapshot) {
                bitplane::HorizontalContentClass::Standard { shift } => {
                    self.h_shift = shift;
                    shift
                }
                bitplane::HorizontalContentClass::Overscan => {
                    self.h_shift = 0;
                    0
                }
                bitplane::HorizontalContentClass::Neutral => self.h_shift,
            },
        }
    }

    /// Resolve one frame's aperture classification into the crop to apply,
    /// latching the decision of any frame that carries content.
    pub fn resolve_tv_aperture(&mut self, frame: TvApertureFrame) -> Option<usize> {
        match frame {
            TvApertureFrame::Standard(rows) => {
                self.standard_aperture = true;
                Some(rows)
            }
            TvApertureFrame::Full => {
                self.standard_aperture = false;
                None
            }
            TvApertureFrame::Neutral(rows) => self.standard_aperture.then_some(rows),
        }
    }
}

pub fn tv_source_h_bounds() -> (usize, usize) {
    let left = bitplane::STANDARD_VISIBLE_X0.saturating_sub(TV_HORIZONTAL_OVERSCAN_MARGIN);
    let right = bitplane::STANDARD_VISIBLE_X0
        .saturating_add(STANDARD_PAL_VISIBLE_WIDTH)
        .saturating_add(TV_HORIZONTAL_OVERSCAN_MARGIN)
        .min(FB_WIDTH)
        .max(left);
    (left, right)
}

pub fn should_render_emulated_frame(last_rendered: Option<u64>, current: u64) -> bool {
    last_rendered != Some(current)
}

pub fn is_standard_presentation(geometry: FrameGeometry, src_rows: usize) -> bool {
    !geometry.programmable && src_rows == OUT_HEIGHT
}

/// One frame's TV-aperture classification, resolved into a crop by
/// [`PresentationLatch::resolve_tv_aperture`]: crop to the standard
/// aperture (`Standard`), present the full framebuffer (`Full`), or keep
/// the previous geometry (`Neutral`, a border-only frame with no content
/// to judge). `Standard` and `Neutral` carry the crop height in woven
/// rows, which follows the scan the frame actually ran rather than the
/// configured standard: a 312/313-line (50 Hz) field carries the 256-line
/// standard window, a 262/263-line (60 Hz) field the 200-line one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TvApertureFrame {
    Standard(usize),
    Full,
    Neutral(usize),
}

/// Classify one frame for the TV aperture. Programmable scans, frames not
/// rendered at the standard woven height, and true horizontal-overscan
/// fetches present on the full framebuffer.
pub fn standard_tv_aperture_frame(
    geometry: FrameGeometry,
    src_rows: usize,
    snapshot: &RenderRegisterSnapshot,
) -> TvApertureFrame {
    if !is_standard_presentation(geometry, src_rows) {
        return TvApertureFrame::Full;
    }
    let rows = if geometry.frame_lines >= 312 {
        TV_PAL_PRESENT_HEIGHT
    } else {
        TV_NTSC_PRESENT_HEIGHT
    };
    match bitplane::horizontal_content_class(snapshot) {
        // The glass does not move for the content: a display fetching
        // into the overscan extends past the aperture and the monitor
        // crops it, exactly as a real set does (overscan = "full" is the
        // mode for seeing all of it). Only the scan's shape -- the
        // programmable geometry and native-height fields handled above --
        // changes what the presentation shows.
        bitplane::HorizontalContentClass::Standard { .. }
        | bitplane::HorizontalContentClass::Overscan => TvApertureFrame::Standard(rows),
        bitplane::HorizontalContentClass::Neutral => TvApertureFrame::Neutral(rows),
    }
}

/// Presentation smoothing for the autocrop rect. Per-frame content
/// envelopes jitter -- loaders blank the screen, screen transitions
/// rebuild the copper list over a frame or two, games flip between
/// differently-sized displays -- and the presented crop must not pump
/// with them. The latch grows immediately (content must never be cut)
/// to the union of the old and new envelopes, holds through border-only
/// frames like the aperture latch does, and shrinks only after the
/// smaller envelope has held steady for [`Self::SHRINK_STABLE_FRAMES`]
/// consecutive rendered frames. Shared by the desktop window (in woven
/// presentation space) and the browser wrapper (in its presentation
/// buffer's space); the envelope's coordinate space is the caller's.
#[derive(Default, Debug)]
pub struct AutocropLatch {
    active: Option<bitplane::ContentRect>,
    candidate: Option<bitplane::ContentRect>,
    stable_frames: u32,
}

impl AutocropLatch {
    /// Rendered frames a strictly-smaller envelope must persist for
    /// before the presentation tightens onto it: about half a second of
    /// PAL frames, long enough to sit out a screen transition.
    pub const SHRINK_STABLE_FRAMES: u32 = 25;

    pub fn resolve(
        &mut self,
        frame: Option<bitplane::ContentRect>,
    ) -> Option<bitplane::ContentRect> {
        let Some(frame) = frame else {
            // Border-only frame: keep presenting the previous crop, and
            // keep any shrink candidate's clock running from zero again
            // once content returns.
            self.candidate = None;
            self.stable_frames = 0;
            return self.active;
        };
        let Some(active) = self.active else {
            self.active = Some(frame);
            return self.active;
        };
        let contained = frame.x0 >= active.x0
            && frame.x1 <= active.x1
            && frame.y0 >= active.y0
            && frame.y1 <= active.y1;
        if !contained {
            self.active = Some(bitplane::ContentRect {
                x0: active.x0.min(frame.x0),
                x1: active.x1.max(frame.x1),
                y0: active.y0.min(frame.y0),
                y1: active.y1.max(frame.y1),
            });
            self.candidate = None;
            self.stable_frames = 0;
        } else if frame != active {
            if self.candidate == Some(frame) {
                self.stable_frames += 1;
                if self.stable_frames >= Self::SHRINK_STABLE_FRAMES {
                    self.active = Some(frame);
                    self.candidate = None;
                    self.stable_frames = 0;
                }
            } else {
                self.candidate = Some(frame);
                self.stable_frames = 1;
            }
        } else {
            self.candidate = None;
            self.stable_frames = 0;
        }
        self.active
    }

    /// Forget the crop across a presentation discontinuity (power cycle,
    /// RTG entry), like `PresentationLatch::reset`.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// How far a whole-number pixel shape may stray from the glass ratio
/// before a taller fit stops being worth it: max(shape/par, par/shape)
/// at most 11/10. Wide enough that NTSC's 0.854 admits 4:5 (6.8% off)
/// through 6:7, 7:8 and 12:13 -- the family amiga.vision's NTSC guide
/// reaches for, 4:5 being its 1080p and 4K answer -- while excluding
/// 3:4 (14%) and the squat 1:1 (17%); and that PAL's 1.078 admits the
/// square 1:1 (7.2%) and 8:7 (6%) but not 6:5 (11%).
const PAR_TOLERANCE: (u64, u64) = (11, 10);

/// The per-axis whole-number fit of a `w` x `h` canvas rect (hi-res
/// columns by woven rows) into `avail` surface pixels, drawn at the
/// pixel shape `par`: `(columns, lines)` -- surface pixels per canvas
/// column, and per *field line*, the scan's own vertical unit. The
/// woven canvas carries two rows per line so interlaced fields can be
/// woven; a non-interlaced display's two are identical, so a whole
/// number of pixels per line is exact for it whether or not the number
/// is even (an odd count lands as alternate rows of the pair, which is
/// only visible on an interlaced display). Stepping the vertical factor
/// by half a woven row is what makes the NTSC shapes reachable on
/// ordinary displays: 4:5 is two pixels per column and five per line,
/// 1000 rows for a 200-line display -- the 1080p answer.
///
/// The choice is the tallest fit whose shape stays within
/// [`PAR_TOLERANCE`] of `par` (a bigger picture beats a marginally truer
/// one), the closest shape among fits of that height, and the widest
/// among equals; with no fit inside the tolerance, the closest shape at
/// the largest size. `None` when not even one pixel per column and per
/// line fits, where the caller falls back to the smooth fit. Exact
/// integer arithmetic throughout, so every host agrees on the answer.
pub fn per_axis_fit(
    avail: (u32, u32),
    (w, h): (usize, usize),
    par: (u32, u32),
) -> Option<(usize, usize)> {
    if w == 0 || h == 0 || par.0 == 0 || par.1 == 0 {
        return None;
    }
    let max_columns = avail.0 as usize / w;
    // `h` woven rows are `h / 2` field lines: `lines` pixels per line
    // puts `h * lines / 2` rows on the surface.
    let max_lines = 2 * avail.1 as usize / h;
    if max_columns == 0 || max_lines == 0 {
        return None;
    }
    let (par_w, par_h) = (u64::from(par.0), u64::from(par.1));
    // A shape's deviation from `par`, as the two cross products ordered
    // (max, min): their quotient is the symmetric ratio error, and two
    // deviations compare exactly by cross-multiplying.
    let deviation = |columns: usize, lines: usize| -> (u64, u64) {
        let shape = 2 * columns as u64 * par_h;
        let glass = lines as u64 * par_w;
        (shape.max(glass), shape.min(glass))
    };
    let within = |(hi, lo): (u64, u64)| hi * PAR_TOLERANCE.1 <= lo * PAR_TOLERANCE.0;
    let compare = |a: (u64, u64), b: (u64, u64)| (a.0 * b.1).cmp(&(b.0 * a.1));
    // A candidate: its factors, its deviation, and whether that is
    // within the tolerance.
    type Candidate = ((usize, usize), (u64, u64), bool);
    let mut best: Option<Candidate> = None;
    for lines in 1..=max_lines {
        // At a given height the shape grows with the column count, so
        // only the two counts astride the ideal one (lines * par / 2)
        // can be the closest -- and the tolerance band, an interval
        // around the ideal, holds one of them if it holds any. Two
        // candidates per height instead of every column: a one-line
        // autocrop rect on a large surface has thousands of both, and
        // the fit runs on every redraw and cursor move.
        let ideal = (lines as u64 * par_w / (2 * par_h)) as usize;
        let astride = [
            ideal.clamp(1, max_columns),
            (ideal + 1).clamp(1, max_columns),
        ];
        for columns in astride {
            let dev = deviation(columns, lines);
            let ok = within(dev);
            let better = match best {
                None => true,
                Some((_, _, cand_ok)) if ok != cand_ok => ok,
                Some((cand, cand_dev, true)) => lines
                    .cmp(&cand.1)
                    .then(compare(cand_dev, dev))
                    .then(columns.cmp(&cand.0))
                    .is_gt(),
                Some((cand, cand_dev, false)) => compare(cand_dev, dev)
                    .then(lines.cmp(&cand.1))
                    .then(columns.cmp(&cand.0))
                    .is_gt(),
            };
            if better {
                best = Some(((columns, lines), dev, ok));
            }
        }
    }
    best.map(|(factors, _, _)| factors)
}

/// Where a `w` x `h` canvas rect lands in `avail` at whole-number
/// factors `(columns, lines)`: exactly `w * columns` by `h * lines / 2`,
/// centred. The rect's rows are whole field lines (an even count) so the
/// height is exact -- every display rect's are, starting on a woven pair
/// (`display_rects_start_on_field_lines`).
pub fn per_axis_rect(
    avail: (u32, u32),
    (w, h): (usize, usize),
    (columns, lines): (usize, usize),
) -> (u32, u32, u32, u32) {
    let dw = ((w * columns) as u32).min(avail.0);
    let dh = ((h * lines / 2) as u32).min(avail.1);
    ((avail.0 - dw) / 2, (avail.1 - dh) / 2, dw, dh)
}

/// The smooth fit of a `w` x `h` canvas rect drawn at pixel shape
/// `par`: the aspect-preserving letterbox of `w * par` by `h` in
/// `avail`, the classic letterbox (`clip_rect_for`'s smooth fit) with
/// the shape folded into the width (at `(1, 1)` it is that fit). Exact
/// integer arithmetic, so a rect whose shape is the viewport's fills it
/// to the pixel: the browser draws its whole presentation buffer into a
/// 4:3 element this way, where a truncated last column would show as a
/// black seam.
pub fn smooth_fit_rect(
    avail: (u32, u32),
    (w, h): (usize, usize),
    par: (u32, u32),
) -> (u32, u32, u32, u32) {
    if avail.0 == 0 || avail.1 == 0 || w == 0 || h == 0 || par.0 == 0 || par.1 == 0 {
        return (0, 0, 0, 0);
    }
    // The rect's shape on the surface is `w * par.0 : h * par.1`; the
    // fit is width-limited when the viewport is narrower than that
    // shape at the viewport's own height.
    let (avail_w, avail_h) = (u64::from(avail.0), u64::from(avail.1));
    let shaped_w = w as u64 * u64::from(par.0);
    let shaped_h = h as u64 * u64::from(par.1);
    let (dw, dh) = if avail_w * shaped_h <= avail_h * shaped_w {
        (avail_w, avail_w * shaped_h / shaped_w)
    } else {
        (avail_h * shaped_w / shaped_h, avail_h)
    };
    let dw = (dw as u32).clamp(1, avail.0);
    let dh = (dh as u32).clamp(1, avail.1);
    ((avail.0 - dw) / 2, (avail.1 - dh) / 2, dw, dh)
}

/// A sub-rect draw's fit ([`sub_rect_fit`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubRectFit {
    /// The whole-number factors of an integer draw, as surface pixels
    /// `(per column, per field line)` -- a uniform multiple `m` is
    /// `(m, 2 * m)` -- or `None` for a smooth fit.
    pub factors: Option<(usize, usize)>,
    /// Where the rect lands, `(x, y, w, h)` in surface pixels.
    pub dst: (u32, u32, u32, u32),
}

/// The whole-number factors and the destination of a sub-rect draw: the
/// `rect` (`(x, y, w, h)` in canvas or buffer pixels) drawn at the pixel
/// shape `par` into `avail` surface pixels. Under `integer` the fit is
/// retaken against the rect -- a display using fewer lines earns a
/// larger whole multiple, which is the point of autocrop -- as a
/// uniform multiple when the pixels are asked for as square, and as the
/// per-axis fit ([`per_axis_fit`]) at a glass shape; either falls back
/// to the smooth fit of the rect at its shape ([`smooth_fit_rect`])
/// when not even 1x fits. The desktop's `display_src_layout` and the
/// browser's [`buffer_layout`] both draw with this.
pub fn sub_rect_fit(
    avail: (u32, u32),
    integer: bool,
    rect: (usize, usize, usize, usize),
    par: (u32, u32),
) -> SubRectFit {
    let (w, h) = (rect.2.max(1), rect.3.max(1));
    let factors = if !integer {
        None
    } else if par.0 == par.1 {
        // The uniform multiple is the per-axis fit's answer for a square
        // shape, but stated directly: the tolerance that lets a glass
        // shape prefer a taller near-miss must not apply to a canvas
        // asked for as square.
        let fit = (avail.0 as usize / w).min(avail.1 as usize / h);
        (fit >= 1).then_some((fit, 2 * fit))
    } else {
        per_axis_fit(avail, (w, h), par)
    };
    let dst = match factors {
        Some(factors) => per_axis_rect(avail, (w, h), factors),
        None => smooth_fit_rect(avail, (w, h), par),
    };
    SubRectFit { factors, dst }
}

/// The shape of one pixel of a `width` x `rows` presentation buffer
/// drawn to fill a 4:3 glass, as a ratio `(width, height)` in lowest
/// terms. The browser's glass model: the page shows whatever buffer the
/// wrapper presents in a 4:3 element, so the buffer's own dimensions
/// state its pixel shape. It is the desktop's tv canvas read back
/// (`window::present::glass_par`) -- the captured TV aperture,
/// `TV_CAPTURED_WIDTH` columns by its woven rows, lands at PAL 1.078
/// and NTSC 0.854, and the full-overscan field at `rows` /
/// `PRESENT_HEIGHT_TV` (the glass is `FB_WIDTH` by `PRESENT_HEIGHT_TV`,
/// itself 4:3).
pub fn buffer_glass_par(width: usize, rows: usize) -> (u32, u32) {
    let (w, h) = (4 * rows.max(1) as u64, 3 * width.max(1) as u64);
    let g = gcd(w, h);
    ((w / g) as u32, (h / g) as u32)
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// Follow a content envelope stated in woven presentation-field space
/// through the deinterlacer's region copy
/// (`Deinterlacer::present_field_region_into`): the rows and columns of
/// the `destination_width` x `destination_rows` buffer that show any of
/// it. The copy reads woven column `source_x + x` for buffer column `x`,
/// and woven row `source_y + scaled_source_row(y)` for buffer row `y`
/// (`screenshot::scaled_source_row`, the centre-aligned resample of
/// `source_rows` onto `destination_rows`). The rows are scanned against
/// that map rather than derived in closed form, like the desktop's
/// `canvas_content_rect`: a few hundred iterations per rendered frame,
/// and immune to drifting out of agreement with the copy it mirrors.
/// `None` when nothing of the envelope reaches the buffer.
pub fn region_present_content_rect(
    rect: bitplane::ContentRect,
    source_x: i32,
    source_y: i32,
    source_rows: usize,
    destination_width: usize,
    destination_rows: usize,
) -> Option<bitplane::ContentRect> {
    let x0 = (rect.x0 as i64 - i64::from(source_x)).clamp(0, destination_width as i64);
    let x1 = (rect.x1 as i64 - i64::from(source_x)).clamp(x0, destination_width as i64);
    if x1 <= x0 {
        return None;
    }
    let (mut y_min, mut y_max) = (None, None);
    for y in 0..destination_rows {
        let woven = i64::from(source_y)
            + screenshot::scaled_source_row(y, source_rows, destination_rows) as i64;
        if (rect.y0 as i64..rect.y1 as i64).contains(&woven) {
            y_min.get_or_insert(y);
            y_max = Some(y);
        }
    }
    Some(bitplane::ContentRect {
        x0: x0 as usize,
        x1: x1 as usize,
        y0: y_min?,
        y1: y_max? + 1,
    })
}

/// Where a presentation buffer lands on a viewport: the browser
/// wrapper's answer to the desktop's `display_src_layout`, for a page
/// that draws the wrapper's `width` x `rows` buffer itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BufferLayout {
    /// The buffer sub-rect shown, `(x, y, w, h)` in buffer pixels.
    pub src: (usize, usize, usize, usize),
    /// Where it lands, `(x, y, w, h)` in viewport pixels; the viewport
    /// outside it is black.
    pub dst: (u32, u32, u32, u32),
    /// The whole-number factors of an integer draw, as viewport pixels
    /// `(per buffer column, per field line)` -- a uniform multiple `m`
    /// is `(m, 2 * m)` -- or `None` for a smooth fit.
    pub factors: Option<(usize, usize)>,
}

/// The layout of a `width` x `rows` presentation buffer on an `avail`
/// viewport under the browser's display settings: `content` is the
/// autocrop rect to show (the latched envelope in buffer pixels), or
/// `None` to show the whole buffer; `integer` asks for whole-number
/// factors. The buffer's pixel shape is the 4:3 glass's
/// ([`buffer_glass_par`]): drawn smooth, the whole buffer fills a 4:3
/// viewport exactly as the page's plain stretch did, and a crop keeps
/// that shape in a letterbox; drawn integer, a standard scan takes the
/// per-axis fit at that shape, while a `programmable` scan takes the
/// uniform multiple (its rows are not woven pairs for the per-line
/// factor to step by), as on the desktop.
pub fn buffer_layout(
    avail: (u32, u32),
    (width, rows): (usize, usize),
    programmable: bool,
    integer: bool,
    content: Option<bitplane::ContentRect>,
) -> BufferLayout {
    let full = (0, 0, width, rows);
    let src = content
        .map(|rect| (rect.x0, rect.y0, rect.x1 - rect.x0, rect.y1 - rect.y0))
        .filter(|rect| rect.2 > 0 && rect.3 > 0)
        .unwrap_or(full);
    let par = if programmable && integer {
        (1, 1)
    } else {
        buffer_glass_par(width, rows)
    };
    let SubRectFit { factors, dst } = sub_rect_fit(avail, integer, src, par);
    BufferLayout { src, dst, factors }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_aperture_clears_the_tv_bezel_mask() {
        // The const block by the TV_CAPTURED_* definitions pins the
        // aperture geometry at compile time; the mask bounds come from a
        // runtime helper, so check here that a captured-aperture crop never
        // shows masked black columns. The glass resample blends one column
        // left of the aperture start (its first sample centre sits half an
        // output pixel before the first source centre), so that column must
        // clear the mask too: the aperture must start strictly inside it.
        assert!(TV_CAPTURED_SOURCE_X > tv_source_h_bounds().0);
    }

    #[test]
    fn tv_glass_resample_spans_the_captured_aperture() {
        // A row with a marker at each end of the captured aperture: the
        // glass's first and last pixels must derive from them (the edges
        // reach the glass), and a marker just outside the aperture must
        // not appear anywhere.
        let mut row = vec![0xFF00_0000u32; FB_WIDTH];
        let first = 0xFF20_4060;
        let last = 0xFF60_4020;
        for x in TV_CAPTURED_SOURCE_X..FB_WIDTH {
            row[x] = 0xFF7F_7F7F;
        }
        row[TV_CAPTURED_SOURCE_X] = first;
        row[FB_WIDTH - 1] = last;

        let g0 = tv_glass_sample(&row, 0, 0);
        let g_last = tv_glass_sample(&row, FB_WIDTH - 1, 0);
        // The first glass pixel blends the first captured column with its
        // left neighbour; the marker must dominate or match.
        assert_ne!(g0, 0xFF7F_7F7F, "left aperture edge missing from glass");
        assert_eq!(g_last, last, "right aperture edge missing from glass");

        // The standard window's centre column maps to the glass centre.
        let mut centre_row = vec![0xFF00_0000u32; FB_WIDTH];
        let centre_src = TV_CAPTURED_SOURCE_X + TV_CAPTURED_WIDTH / 2;
        centre_row[centre_src - 1] = 0xFFFF_FFFF;
        centre_row[centre_src] = 0xFFFF_FFFF;
        assert_eq!(tv_glass_sample(&centre_row, FB_WIDTH / 2, 0), 0xFFFF_FFFF);
    }

    #[test]
    fn tv_glass_centre_offset_slides_the_window_and_blacks_unscanned_glass() {
        // A right-nudged picture (negative source offset) pulls captured
        // columns left of the default aperture onto the glass. Marker on a
        // neighbouring pair, since a glass pixel blends adjacent texels.
        let mut row = vec![0xFF10_1010u32; FB_WIDTH];
        let marker = 0xFF20_4060;
        row[TV_CAPTURED_SOURCE_X - 13] = marker;
        row[TV_CAPTURED_SOURCE_X - 12] = marker;
        let (x_off, _) = tv_centre_source_offset(TvCentre { h: 6, v: 0 });
        assert_eq!(x_off, -12);
        assert_eq!(tv_glass_sample(&row, 0, x_off), marker);

        // A left-nudged picture slides the window past the framebuffer's
        // right edge: that glass is unscanned and comes back black, never
        // the edge column repeated (the Gen-X drag-line lesson).
        let (x_off, _) = tv_centre_source_offset(TvCentre { h: -6, v: 0 });
        assert_eq!(
            tv_glass_sample(&row, FB_WIDTH - 1, x_off),
            rgba(0, 0, 0),
            "off-capture glass must be black"
        );
    }

    /// A standard full-width display (stock Kickstart screen).
    fn standard_snapshot() -> RenderRegisterSnapshot {
        RenderRegisterSnapshot {
            bplcon0: 0x5200,
            diwstrt: 0x2C81,
            diwstop: 0xF4C1,
            ddfstrt: 0x38,
            ddfstop: 0xD0,
            ..Default::default()
        }
    }

    /// A wide-DIW display whose fetch reaches into the overscan border.
    fn overscan_snapshot() -> RenderRegisterSnapshot {
        RenderRegisterSnapshot {
            bplcon0: 0x5200,
            diwstrt: 0x5702,
            diwstop: 0xFFFF,
            ddfstrt: 0x30,
            ddfstop: 0xD8,
            ..Default::default()
        }
    }

    /// A border-only frame: registers cleared, no fetch (the state a screen
    /// change or the Kickstart boot blanks present for a few frames).
    fn blank_snapshot() -> RenderRegisterSnapshot {
        RenderRegisterSnapshot::default()
    }

    #[test]
    fn tv_aperture_rows_follow_the_scans_field_line_count() {
        // A standard scan's TV aperture is picked by the lines the frame
        // actually ran (BEAMCON0 can retune Agnus mid-session), holding the
        // 256-line standard window of a 50 Hz field and the 200-line window
        // of a 60 Hz field with the same overscan margin.
        let snapshot = standard_snapshot();
        let pal = FrameGeometry::standard(0x1C, 313, false);
        let ntsc = FrameGeometry::standard(0x1C, 262, false);
        assert_eq!(
            standard_tv_aperture_frame(pal, OUT_HEIGHT, &snapshot),
            TvApertureFrame::Standard(TV_PAL_PRESENT_HEIGHT)
        );
        assert_eq!(
            standard_tv_aperture_frame(ntsc, OUT_HEIGHT, &snapshot),
            TvApertureFrame::Standard(TV_NTSC_PRESENT_HEIGHT)
        );
        // A programmable scan or a native-height field presents in full.
        let mut programmable = ntsc;
        programmable.programmable = true;
        assert_eq!(
            standard_tv_aperture_frame(programmable, OUT_HEIGHT, &snapshot),
            TvApertureFrame::Full
        );
        assert_eq!(
            standard_tv_aperture_frame(ntsc, 400, &snapshot),
            TvApertureFrame::Full
        );
        // A border-only frame on a standard scan is neutral: it carries the
        // scan's crop height but no crop decision of its own.
        assert_eq!(
            standard_tv_aperture_frame(pal, OUT_HEIGHT, &blank_snapshot()),
            TvApertureFrame::Neutral(TV_PAL_PRESENT_HEIGHT)
        );
    }

    #[test]
    fn overscan_content_keeps_the_glass_aperture() {
        // A display fetching into the overscan (the CD32 boot screen, demo
        // loaders) extends past the aperture; the monitor's glass crops it
        // rather than zooming out to show it -- the glass does not move
        // for the content. Falling back to the full framebuffer here used
        // to expose the unpainted capture margins as black bands beside
        // the picture.
        let overscan = RenderRegisterSnapshot {
            agnus_revision: crate::chipset::agnus::AgnusRevision::AgaAlice,
            bplcon0: 0x8214,
            diwstrt: 0x1D61,
            diwstop: 0x37C7,
            ddfstrt: 0x0028,
            ddfstop: 0x00D8,
            ..RenderRegisterSnapshot::default()
        };
        assert_eq!(
            bitplane::horizontal_content_class(&overscan),
            bitplane::HorizontalContentClass::Overscan,
            "snapshot must classify as overscan for the assertion to bite"
        );
        let pal = FrameGeometry::standard(0x1C, 313, false);
        assert_eq!(
            standard_tv_aperture_frame(pal, OUT_HEIGHT, &overscan),
            TvApertureFrame::Standard(TV_PAL_PRESENT_HEIGHT)
        );
    }

    #[test]
    fn border_only_frames_keep_the_previous_aperture_decision() {
        // The Kickstart 2.05 boot regression: screen changes emit a frame
        // or two of border-only display, and deciding those "full
        // framebuffer" flipped the whole presentation geometry there and
        // back, lurching the picture sideways at every mode change.
        let mut latch = PresentationLatch::default();
        // Power-on default: crop like the stock display the blanks precede.
        assert_eq!(
            latch.resolve_tv_aperture(TvApertureFrame::Neutral(TV_PAL_PRESENT_HEIGHT)),
            Some(TV_PAL_PRESENT_HEIGHT)
        );
        assert_eq!(
            latch.resolve_tv_aperture(TvApertureFrame::Standard(TV_PAL_PRESENT_HEIGHT)),
            Some(TV_PAL_PRESENT_HEIGHT)
        );
        // The blank between two standard screens stays cropped...
        assert_eq!(
            latch.resolve_tv_aperture(TvApertureFrame::Neutral(TV_PAL_PRESENT_HEIGHT)),
            Some(TV_PAL_PRESENT_HEIGHT)
        );
        // ...and a neutral frame still follows the current scan's height
        // (the crop decision is latched, not the row count).
        assert_eq!(
            latch.resolve_tv_aperture(TvApertureFrame::Neutral(TV_NTSC_PRESENT_HEIGHT)),
            Some(TV_NTSC_PRESENT_HEIGHT)
        );
        // An overscan demo blanking between parts must NOT snap to the
        // aperture: full presentation latches across its blanks too.
        assert_eq!(latch.resolve_tv_aperture(TvApertureFrame::Full), None);
        assert_eq!(
            latch.resolve_tv_aperture(TvApertureFrame::Neutral(TV_PAL_PRESENT_HEIGHT)),
            None
        );
        // A presentation discontinuity restores the power-on default.
        latch.reset();
        assert_eq!(
            latch.resolve_tv_aperture(TvApertureFrame::Neutral(TV_PAL_PRESENT_HEIGHT)),
            Some(TV_PAL_PRESENT_HEIGHT)
        );
    }

    #[test]
    fn border_only_frames_keep_the_previous_recentring_shift() {
        let mut latch = PresentationLatch::default();
        let standard_shift = bitplane::standard_present_h_shift();
        // Power-on default: the stock display's shift, so the first real
        // screen does not jump against the blanks before it.
        assert_eq!(
            latch.presentation_h_shift(&blank_snapshot(), Overscan::Full),
            standard_shift
        );
        assert_eq!(
            latch.presentation_h_shift(&standard_snapshot(), Overscan::Full),
            standard_shift
        );
        // A true overscan display presents unshifted, and its blanks keep
        // that.
        assert_eq!(
            latch.presentation_h_shift(&overscan_snapshot(), Overscan::Full),
            0
        );
        assert_eq!(
            latch.presentation_h_shift(&blank_snapshot(), Overscan::Full),
            0
        );
        // TV mode is a fixed aperture: never shifted, and not latched.
        assert_eq!(
            latch.presentation_h_shift(&standard_snapshot(), Overscan::Tv),
            0
        );
        assert_eq!(
            latch.presentation_h_shift(&blank_snapshot(), Overscan::Full),
            0
        );
    }

    #[test]
    fn ntsc_aperture_stays_inside_the_rendered_field() {
        // A 60 Hz field renders (frame_lines - visible_start) field rows;
        // the aperture crop must end inside them so it never shows
        // unscanned buffer rows as picture.
        let rendered_woven_rows = 2 * (262 - 0x1C);
        assert!(TV_PRESENT_SOURCE_Y + TV_NTSC_PRESENT_HEIGHT <= rendered_woven_rows);
    }

    #[test]
    fn tube_aperture_spans_exactly_the_rendered_field() {
        // The tube glass of a drawn bezel shows every rendered row of the
        // scan the TV aperture classified -- no more (stale buffer rows
        // below a 60 Hz field must never show) and no less: the full
        // 263-line NTSC frame, not the short interlaced field's 262
        // (whose missing last line is blanked black past its frame wrap).
        assert_eq!(tube_aperture_rows(TV_PAL_PRESENT_HEIGHT), OUT_HEIGHT);
        assert_eq!(tube_aperture_rows(TV_NTSC_PRESENT_HEIGHT), 2 * (263 - 0x1C));
    }

    fn v_window_fixture(field_rows: usize, total_rows: usize) -> Vec<u32> {
        // Each captured row is tagged with its index + 1 in every column so
        // relocation is visible; rows past field_rows start as garbage the
        // applier must overwrite or ignore.
        let mut fb = vec![0xDEAD_BEEF; total_rows * FB_WIDTH];
        for row in 0..field_rows {
            fb[row * FB_WIDTH..(row + 1) * FB_WIDTH].fill(row as u32 + 1);
        }
        fb
    }

    #[test]
    fn presentation_v_window_places_rows_by_the_modes_porches() {
        let mut fb = v_window_fixture(4, 12);
        let rows = apply_presentation_v_window(&mut fb, FB_WIDTH, 4, Some((3, 10)));
        assert_eq!(rows, 10);
        let black = rgba(0, 0, 0);
        for row in 0..10 {
            let expected = match row {
                3..=6 => (row - 2) as u32,
                _ => black,
            };
            assert_eq!(fb[row * FB_WIDTH], expected, "row {row}");
        }
    }

    #[test]
    fn presentation_v_window_clips_rows_above_the_glass_top() {
        let mut fb = v_window_fixture(4, 12);
        let rows = apply_presentation_v_window(&mut fb, FB_WIDTH, 4, Some((-2, 10)));
        assert_eq!(rows, 10);
        let black = rgba(0, 0, 0);
        // Captured rows 0-1 fall above the sync-anchored glass; rows 2-3
        // land at the top.
        assert_eq!(fb[0], 3);
        assert_eq!(fb[FB_WIDTH], 4);
        for row in 2..10 {
            assert_eq!(fb[row * FB_WIDTH], black, "row {row}");
        }
    }

    /// The placement a programmable field reports carries its content
    /// envelope exactly where the pixels went: columns through the
    /// sync-anchored resample's own taps (a window starting left of the
    /// capture pushes the picture right and widens it onto the glass),
    /// rows through the vertical window's skip and pad, and the
    /// deinterlacer's row count decides between native rows and woven
    /// pairs.
    #[test]
    fn programmable_placement_maps_the_content_envelope_through_its_windows() {
        use crate::video::bitplane::ContentRect;
        let geometry = FrameGeometry {
            programmable: true,
            visible_start_vpos: 20,
            visible_lines: 100,
            line_cck: 130,
            frame_lines: 140,
            lace: false,
        };
        let mut fb = vec![0u32; FB_WIDTH * 120];
        let lit = 0xFFFF_FFFF;
        let content = ContentRect {
            x0: 100,
            x1: 300,
            y0: 10,
            y1: 60,
        };
        for y in content.y0..content.y1 {
            fb[y * FB_WIDTH + content.x0..y * FB_WIDTH + content.x1].fill(lit);
        }
        // A sync-anchored window from 40 columns left of the capture, 400
        // wide, and a glass of 120 rows whose top sits 5 captured rows
        // down.
        let placement = post_process_rendered_field(
            &mut fb,
            geometry,
            1,
            Some((-40, 400)),
            Some((-5, 120)),
            geometry.visible_start_vpos,
            0,
            Overscan::Tv,
        );
        assert_eq!(placement.rows, 120);
        let mapped = placement
            .content_rect(content, placement.rows)
            .expect("content reaches the glass");
        // Every lit output pixel (any colour in it: a blended edge counts,
        // the opaque black padding does not) lies inside the mapped rect,
        // and the rect's edge columns and rows carry lit pixels (no
        // slack).
        let is_lit = |px: u32| px & 0x00FF_FFFF != 0;
        for y in 0..placement.rows {
            for x in 0..FB_WIDTH {
                if is_lit(fb[y * FB_WIDTH + x]) {
                    assert!(
                        (mapped.x0..mapped.x1).contains(&x) && (mapped.y0..mapped.y1).contains(&y),
                        "lit pixel ({x}, {y}) outside {mapped:?}"
                    );
                }
            }
        }
        let lit_in_column = |x: usize| (0..placement.rows).any(|y| is_lit(fb[y * FB_WIDTH + x]));
        let lit_in_row = |y: usize| (0..FB_WIDTH).any(|x| is_lit(fb[y * FB_WIDTH + x]));
        assert!(lit_in_column(mapped.x0) && lit_in_column(mapped.x1 - 1));
        assert!(lit_in_row(mapped.y0) && lit_in_row(mapped.y1 - 1));
        // Rows: captured rows 10..60 minus the 5 skipped, at the glass top.
        assert_eq!((mapped.y0, mapped.y1), (5, 55));
        // Columns: source 100..300 of a 400-wide window at -40 spans
        // (140..340) * 716 / 400 of the glass, give or take the blend.
        assert!((248..=252).contains(&mapped.x0), "{mapped:?}");
        assert!((606..=612).contains(&mapped.x1), "{mapped:?}");

        // Woven (LACE) presentation doubles the rows.
        let woven = placement.content_rect(content, 2 * placement.rows).unwrap();
        assert_eq!((woven.y0, woven.y1), (10, 110));

        // Content entirely above the glass top maps to nothing.
        let above = ContentRect {
            x0: 100,
            x1: 300,
            y0: 0,
            y1: 5,
        };
        assert_eq!(placement.content_rect(above, placement.rows), None);
        // Content entirely left of the sync-anchored window (the sync
        // tail clamps to the edge column, which is border here) maps
        // to nothing either.
        let mut fb = vec![0u32; FB_WIDTH * 120];
        let placement = post_process_rendered_field(
            &mut fb,
            geometry,
            1,
            Some((300, 400)),
            None,
            geometry.visible_start_vpos,
            0,
            Overscan::Tv,
        );
        let left = ContentRect {
            x0: 100,
            x1: 200,
            y0: 10,
            y1: 60,
        };
        assert_eq!(placement.content_rect(left, placement.rows), None);
        // Without a programmed vertical sync the rows stay as captured,
        // and a standard-length line without programmed horizontal sync
        // keeps its columns.
        let native = FrameGeometry {
            line_cck: 227,
            ..geometry
        };
        let placement = post_process_rendered_field(
            &mut fb,
            native,
            1,
            None,
            None,
            geometry.visible_start_vpos,
            0,
            Overscan::Tv,
        );
        assert_eq!(placement.rows, 100);
        assert_eq!(placement.content_rect(left, 100), Some(left));
    }

    /// A 35 ns canvas fans each hi-res-pitch column of the envelope out
    /// to two buffer columns before the window resample.
    #[test]
    fn programmable_placement_scales_the_envelope_to_the_canvas_pitch() {
        use crate::video::bitplane::ContentRect;
        let geometry = FrameGeometry {
            programmable: true,
            visible_start_vpos: 20,
            visible_lines: 50,
            line_cck: 227,
            frame_lines: 80,
            lace: false,
        };
        let mut fb = vec![0u32; 2 * FB_WIDTH * 50];
        let placement = post_process_rendered_field(
            &mut fb,
            geometry,
            2,
            None,
            None,
            geometry.visible_start_vpos,
            0,
            Overscan::Tv,
        );
        let content = ContentRect {
            x0: 100,
            x1: 300,
            y0: 10,
            y1: 40,
        };
        assert_eq!(
            placement.content_rect(content, 50),
            Some(ContentRect {
                x0: 200,
                x1: 600,
                y0: 10,
                y1: 40
            })
        );
    }

    #[test]
    fn presentation_v_window_absent_or_degenerate_keeps_the_field() {
        let mut fb = v_window_fixture(4, 12);
        assert_eq!(apply_presentation_v_window(&mut fb, FB_WIDTH, 4, None), 4);
        assert_eq!(fb[0], 1);
        // A glass no taller than the captured field cannot add borders.
        assert_eq!(
            apply_presentation_v_window(&mut fb, FB_WIDTH, 4, Some((1, 4))),
            4
        );
        assert_eq!(fb[0], 1);
    }

    #[test]
    fn smooth_fit_rect_fills_a_viewport_of_the_rects_own_shape_exactly() {
        // The browser's whole-buffer draw: every buffer the wrapper presents
        // -- the 50 Hz aperture, the 60 Hz aperture at its own rows, a full
        // overscan field, the square canvas -- fills a 4:3 viewport to the
        // pixel at the glass shape read off the buffer, at any size. The
        // f32 fit this replaces could truncate the last column away.
        for (w, h) in [(668, 540), (668, 428), (716, 626), (716, 570)] {
            let par = buffer_glass_par(w, h);
            for avail in [
                (1440, 1080),
                (2880, 2160),
                (1024, 768),
                (716, 537),
                (3840, 2880),
            ] {
                assert_eq!(
                    smooth_fit_rect(avail, (w, h), par),
                    (0, 0, avail.0, avail.1),
                    "{w}x{h} in {avail:?}"
                );
            }
        }
        // A rect narrower than the viewport at its shape is height-limited
        // and centred: 640 x 400 of the native NTSC aperture (0.854 pixels)
        // on a 16:9 screen.
        let ntsc = buffer_glass_par(668, 428);
        assert_eq!(
            smooth_fit_rect((1920, 1080), (640, 400), ntsc),
            (222, 0, 1476, 1080)
        );
        assert_eq!(smooth_fit_rect((0, 10), (640, 400), ntsc), (0, 0, 0, 0));
    }

    #[test]
    fn buffer_glass_par_is_the_4_3_glass_read_off_the_buffer() {
        // The PAL and NTSC apertures land at the desktop's glass shapes
        // (`glass_par`: 1.078 and 0.854), in lowest terms.
        let ratio = |(w, h): (u32, u32)| f64::from(w) / f64::from(h);
        let pal = buffer_glass_par(TV_CAPTURED_WIDTH, TV_PAL_PRESENT_HEIGHT);
        let ntsc = buffer_glass_par(TV_CAPTURED_WIDTH, TV_NTSC_PRESENT_HEIGHT);
        assert_eq!(pal, (180, 167));
        assert!((ratio(pal) - 1.0778).abs() < 5e-4, "PAL {}", ratio(pal));
        assert!((ratio(ntsc) - 0.8543).abs() < 5e-4, "NTSC {}", ratio(ntsc));
        // The full-overscan field: rows over PRESENT_HEIGHT_TV, the glass
        // being FB_WIDTH x PRESENT_HEIGHT_TV (itself 4:3).
        assert_eq!(
            buffer_glass_par(FB_WIDTH, OUT_HEIGHT),
            (
                OUT_HEIGHT as u32 / 3,
                crate::video::PRESENT_HEIGHT_TV as u32 / 3
            )
        );
        assert_eq!(buffer_glass_par(0, 0), (4, 3));
    }

    #[test]
    fn region_present_content_rect_inverts_the_aperture_copy() {
        use crate::video::bitplane::ContentRect;
        // A 640-wide standard window, 200 lines woven to rows 64..464 of
        // the field, presented through the 50 Hz aperture copy: the
        // capture starts TV_CAPTURED_MARGIN_X left of the window and its
        // rows are the identity from woven row TV_PRESENT_SOURCE_Y.
        let x0 = bitplane::STANDARD_VISIBLE_X0;
        let rect = ContentRect {
            x0,
            x1: x0 + 640,
            y0: 64,
            y1: 464,
        };
        let (source_x, source_y) = (TV_CAPTURED_SOURCE_X as i32, TV_PRESENT_SOURCE_Y as i32);
        assert_eq!(
            region_present_content_rect(rect, source_x, source_y, 540, TV_CAPTURED_WIDTH, 540),
            Some(ContentRect {
                x0: TV_CAPTURED_MARGIN_X,
                x1: TV_CAPTURED_MARGIN_X + 640,
                y0: 46,
                y1: 446,
            })
        );
        // A tv-centre nudge moves the capture, so the buffer rect moves the
        // other way.
        let nudged = region_present_content_rect(
            rect,
            source_x - 8,
            source_y + 4,
            540,
            TV_CAPTURED_WIDTH,
            540,
        )
        .unwrap();
        assert_eq!(
            (nudged.x0, nudged.x1),
            (TV_CAPTURED_MARGIN_X + 8, TV_CAPTURED_MARGIN_X + 648)
        );
        assert_eq!((nudged.y0, nudged.y1), (42, 442));
        // The 60 Hz aperture resampled onto the 50 Hz row count (428 -> 540,
        // the smooth presentation): rows 64..364 land on the buffer rows
        // whose centre-aligned source falls inside them, 58..437; at its
        // own rows (the integer presentation) the map is the identity.
        let ntsc = ContentRect { y1: 364, ..rect };
        let resampled =
            region_present_content_rect(ntsc, source_x, source_y, 428, TV_CAPTURED_WIDTH, 540)
                .unwrap();
        assert_eq!((resampled.y0, resampled.y1), (58, 437));
        let native =
            region_present_content_rect(ntsc, source_x, source_y, 428, TV_CAPTURED_WIDTH, 428)
                .unwrap();
        assert_eq!((native.y0, native.y1), (46, 346));
        // Off the buffer entirely: above the aperture, or left of the capture.
        let above = ContentRect {
            y0: 0,
            y1: 10,
            ..rect
        };
        assert_eq!(
            region_present_content_rect(above, source_x, source_y, 540, TV_CAPTURED_WIDTH, 540),
            None
        );
        let left = ContentRect {
            x0: 0,
            x1: 4,
            ..rect
        };
        assert_eq!(
            region_present_content_rect(left, source_x, source_y, 540, TV_CAPTURED_WIDTH, 540),
            None
        );
        // Partly off: the shown part, clipped.
        let wide = ContentRect {
            x0: 0,
            x1: FB_WIDTH,
            ..rect
        };
        let shown =
            region_present_content_rect(wide, source_x, source_y, 540, TV_CAPTURED_WIDTH, 540)
                .unwrap();
        assert_eq!((shown.x0, shown.x1), (0, TV_CAPTURED_WIDTH));
    }

    #[test]
    fn buffer_layout_places_the_browser_buffer() {
        use crate::video::bitplane::ContentRect;
        // Smooth, whole buffer: the 4:3 element, exactly -- the page's
        // plain stretch, so the modes' draw path shows the same picture.
        let pal = (TV_CAPTURED_WIDTH, TV_PAL_PRESENT_HEIGHT);
        assert_eq!(
            buffer_layout((1440, 1080), pal, false, false, None),
            BufferLayout {
                src: (0, 0, 668, 540),
                dst: (0, 0, 1440, 1080),
                factors: None,
            }
        );
        // Integer, the PAL aperture on a 1080p screen: two pixels per
        // column and four per line (square, within a tenth of PAL's
        // 1.078), 1336 x 1080 centred.
        let layout = buffer_layout((1440, 1080), pal, false, true, None);
        assert_eq!(layout.factors, Some((2, 4)));
        assert_eq!(layout.dst, (52, 0, 1336, 1080));
        // Integer with autocrop, a 640 x 400 NTSC display of the native-row
        // aperture on a 1080p screen: amiga.vision's 4:5 pixel, 1280 x 1000.
        let ntsc = (TV_CAPTURED_WIDTH, TV_NTSC_PRESENT_HEIGHT);
        let content = ContentRect {
            x0: 14,
            x1: 654,
            y0: 14,
            y1: 414,
        };
        let layout = buffer_layout((1920, 1080), ntsc, false, true, Some(content));
        assert_eq!(layout.src, (14, 14, 640, 400));
        assert_eq!(layout.factors, Some((2, 5)));
        assert_eq!(layout.dst, (320, 40, 1280, 1000));
        // Smooth with autocrop: the crop at the glass shape, letterboxed.
        let layout = buffer_layout((1920, 1080), ntsc, false, false, Some(content));
        assert_eq!(layout.factors, None);
        assert_eq!(layout.dst, (222, 0, 1476, 1080));
        // A programmable scan (a 716 x 552 DblPAL field) under integer
        // scaling takes the uniform multiple, its rows not being woven
        // pairs; drawn smooth it fills the glass like any buffer.
        let layout = buffer_layout((1440, 1080), (716, 552), true, true, None);
        assert_eq!(layout.factors, Some((1, 2)));
        assert_eq!(layout.dst, (362, 264, 716, 552));
        assert_eq!(
            buffer_layout((1440, 1080), (716, 552), true, false, None).dst,
            (0, 0, 1440, 1080)
        );
        // Too small for one pixel per column: the smooth fit of the same shape.
        let layout = buffer_layout((600, 500), pal, false, true, None);
        assert_eq!(layout.factors, None);
        assert_eq!(layout.dst, (0, 25, 600, 450));
        // An empty envelope shows the whole buffer.
        let empty = ContentRect {
            x0: 10,
            x1: 10,
            y0: 0,
            y1: 5,
        };
        assert_eq!(
            buffer_layout((1440, 1080), pal, false, false, Some(empty)).src,
            (0, 0, 668, 540)
        );
    }
}
