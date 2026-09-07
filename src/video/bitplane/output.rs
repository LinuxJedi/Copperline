// SPDX-License-Identifier: GPL-3.0-or-later

//! Denise pixel output: palette/control segment sampling, background
//! and border colour, RGB conversion, HAM6/HAM8, EHB, dual playfield
//! and SHRES composition. Split out of `bitplane.rs` for size; same
//! module family, full access to the parent's private items.

use super::*;

const INDEXED_OUTPUT_CACHE_LEN: usize = 8;

/// Only these controls feed `denise_playfield_output`. Scroll, fetch, DMA,
/// window and collision controls select samples or compose their output;
/// they do not change the colour table for a given sample index. Keep whole
/// register values here so mode, priority, EHB, PF2OF and BPLAM changes all
/// invalidate the table, including undocumented combinations.
fn indexed_color_key(control: ControlState) -> (bool, u16, u16, u16, u16) {
    (
        control.aga(),
        control.bplcon0,
        control.bplcon2,
        control.bplcon3,
        control.bplcon4,
    )
}

struct IndexedOutputCacheEntry {
    control: ControlState,
    palette: Palette,
    outputs: Box<[DenisePlayfieldOutput; 256]>,
}

/// Small frame-local cache for the indexed (non-HAM) Denise/Lisa colour
/// resolver. A normal screen reuses one control/palette pair for hundreds of
/// rows; resolving all possible indices once turns the per-pixel mode,
/// priority, EHB, BPLAM, and palette branches into one indexed load. Copper
/// palette/control changes naturally produce another entry, while HAM stays
/// on the history-dependent resolver.
#[derive(Default)]
pub(super) struct IndexedOutputCache {
    entries: Vec<IndexedOutputCacheEntry>,
}

impl IndexedOutputCache {
    pub(super) fn outputs(
        &mut self,
        control: ControlState,
        palette: &Palette,
    ) -> &[DenisePlayfieldOutput; 256] {
        debug_assert!(!control.ham());
        let color_key = indexed_color_key(control);
        if let Some(idx) = self.entries.iter().position(|entry| {
            indexed_color_key(entry.control) == color_key && entry.palette == *palette
        }) {
            return &self.entries[idx].outputs;
        }

        let outputs = Box::new(std::array::from_fn(|idx| {
            let mut ignored_history = 0;
            denise_playfield_output(control, palette, idx as u8, &mut ignored_history)
        }));
        if self.entries.len() == INDEXED_OUTPUT_CACHE_LEN {
            self.entries.remove(0);
        }
        self.entries.push(IndexedOutputCacheEntry {
            control,
            palette: *palette,
            outputs,
        });
        &self.entries.last().expect("just inserted").outputs
    }
}

#[cfg(test)]
#[path = "output_cache_tests.rs"]
mod output_cache_tests;

#[inline(always)]
pub(super) fn cached_indexed_output(
    outputs: &[DenisePlayfieldOutput; 256],
    idx: u8,
    ham_color: &mut u32,
) -> DenisePlayfieldOutput {
    let output = outputs[idx as usize];
    // The indexed resolver seeds the held colour for a later mid-line switch
    // into HAM, even though its own output is history-independent.
    *ham_color = output.color;
    output
}

pub(super) fn palette_at_x(mut palette: Palette, segments: &[PaletteSegment], x: usize) -> Palette {
    for seg in segments {
        if seg.x > x {
            break;
        }
        seg.apply(&mut palette);
    }
    palette
}

/// One palette entry at horizontal position `x`: the base palette's entry
/// with the row's segment writes to that entry up to `x` applied. The sprite
/// pixel loops resolve exactly one colour per pixel, so sampling that entry
/// alone avoids copying the whole 1KB palette per pixel (a measured render
/// hot spot on sprite-multiplexing games).
pub(super) fn palette_entry_at_x(
    palette: &Palette,
    segments: &[PaletteSegment],
    x: usize,
    entry: usize,
) -> crate::chipset::denise::PaletteEntry {
    let mut out = palette.entry(entry);
    for seg in segments {
        if seg.x > x {
            break;
        }
        if usize::from(seg.entry) == entry & (crate::chipset::denise::PALETTE_ENTRIES - 1) {
            out.write(seg.loct, seg.value);
        }
    }
    out
}

pub(super) fn control_at_x(
    mut control: ControlState,
    segments: &[ControlSegment],
    x: usize,
) -> ControlState {
    for seg in segments {
        if seg.x > x {
            break;
        }
        control = seg.control;
    }
    control
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn fill_background(
    fb: &mut [u32],
    base_palettes: &[Palette],
    palette_segments: &[Vec<PaletteSegment>],
    base_controls: &[ControlState],
    control_segments: &[Vec<ControlSegment>],
) {
    let h_window_rows = compute_h_window_rows(base_controls, control_segments, PAL_VISIBLE_LINE0);
    fill_background_with_visible_line0(
        fb,
        base_palettes,
        palette_segments,
        base_controls,
        control_segments,
        &h_window_rows,
        PAL_VISIBLE_LINE0,
    );
}

/// The background colour for one pixel given the latched control/palette
/// state and whether the pixel is in the border. `sample` is always `None`
/// for a background pixel, so the result depends only on these three
/// inputs -- which is what lets [`fill_background_with_visible_line0`] fill
/// constant runs instead of recomputing per pixel.
pub(super) fn background_pixel(control: &ControlState, color0: u16, border: bool) -> u32 {
    let color_latch = if control.border_blank_enabled() && border {
        0
    } else {
        color0
    };
    let transparent = control.genlock_transparent(color_latch, None, border);
    rgb12_to_rgba8_alpha(color_rgb12(color_latch), !transparent)
}

pub(super) fn fill_background_with_visible_line0(
    fb: &mut [u32],
    base_palettes: &[Palette],
    palette_segments: &[Vec<PaletteSegment>],
    base_controls: &[ControlState],
    control_segments: &[Vec<ControlSegment>],
    h_window_rows: &[HWindowRow],
    visible_line0: i32,
) {
    let canvas_scale = super::active_canvas_scale();
    let out_w = FB_WIDTH * canvas_scale;
    for y in 0..base_palettes.len() {
        let row = &mut fb[y * out_w..(y + 1) * out_w];
        let pal_segs = &palette_segments[y];
        let ctl_segs = &control_segments[y];
        let mut palette = base_palettes[y];
        let mut control = base_controls[y];
        let mut palette_idx = 0usize;
        let mut control_idx = 0usize;
        // Walk runs over which `palette[0]` and `control` are constant. Each
        // run is then split by the border-zone boundary (the display
        // window edges), which is also fixed while `control` is. Within
        // each resulting sub-run every pixel is identical, so it is filled
        // in one go. A plain row collapses to left-border/active/right-
        // border, three fills instead of FB_WIDTH per-pixel computations.
        let mut x = 0usize;
        while x < FB_WIDTH {
            while palette_idx < pal_segs.len() && pal_segs[palette_idx].x <= x {
                pal_segs[palette_idx].apply(&mut palette);
                palette_idx += 1;
            }
            while control_idx < ctl_segs.len() && ctl_segs[control_idx].x <= x {
                control = ctl_segs[control_idx].control;
                control_idx += 1;
            }
            let next_pal = pal_segs
                .get(palette_idx)
                .map_or(FB_WIDTH, |seg| seg.x.min(FB_WIDTH));
            let next_ctl = ctl_segs
                .get(control_idx)
                .map_or(FB_WIDTH, |seg| seg.x.min(FB_WIDTH));
            let run_end = next_pal.min(next_ctl).max(x + 1);
            let color0 = palette[0];

            if !control.display_window_contains_line(y, visible_line0) {
                // Whole run is border: a single fill.
                row[x * canvas_scale..run_end * canvas_scale]
                    .fill(background_pixel(&control, color0, true));
                x = run_end;
                continue;
            }
            // In the vertical window: border holds wherever the horizontal
            // window flip-flop is closed (hardware comparator model; the
            // open runs already reflect this row's mid-line DIW writes).
            let open_runs = h_window_rows[y].open_runs();
            let mut sx = x;
            while sx < run_end {
                let open = open_runs.iter().any(|&(s, e)| sx >= s && sx < e);
                let flip = open_runs
                    .iter()
                    .flat_map(|&(s, e)| [s, e])
                    .filter(|&b| b > sx)
                    .min()
                    .unwrap_or(FB_WIDTH);
                let sub_end = flip.min(run_end).max(sx + 1);
                row[sx * canvas_scale..sub_end * canvas_scale]
                    .fill(background_pixel(&control, color0, !open));
                sx = sub_end;
            }
            x = run_end;
        }
    }
}

pub(super) fn rgb12_to_rgba8_alpha(c: u16, opaque: bool) -> u32 {
    let rgba = rgb12_to_rgba8(c);
    if opaque {
        rgba
    } else {
        rgba & 0x00FF_FFFF
    }
}

pub(super) fn rgb24_to_rgba8_alpha(c: u32, opaque: bool) -> u32 {
    let rgba = rgb24_to_rgba8(c);
    if opaque {
        rgba
    } else {
        rgba & 0x00FF_FFFF
    }
}

/// Framebuffer RGBA back to 24-bit 0x00RRGGBB (HAM seeding from an already
/// rendered pixel).
pub(super) fn rgba8_to_rgb24(c: u32) -> u32 {
    let r = c & 0xFF;
    let g = (c >> 8) & 0xFF;
    let b = (c >> 16) & 0xFF;
    (r << 16) | (g << 8) | b
}

/// High nibbles of a 24-bit colour as a 12-bit word. Exact inverse of
/// rgb12_to_rgb24 for nibble-duplicated values, used to keep the OCS HAM6
/// maths in its native 12-bit space while the pipeline carries 24-bit.
pub(super) fn rgb24_to_rgb12_hi(c: u32) -> u16 {
    let r = ((c >> 20) & 0xF) as u16;
    let g = ((c >> 12) & 0xF) as u16;
    let b = ((c >> 4) & 0xF) as u16;
    (r << 8) | (g << 4) | b
}

pub(super) fn color_rgb12(color_latch: u16) -> u16 {
    color_latch & COLOR_RGB_MASK
}

pub(super) fn palette_index_to_rgb12(palette: &Palette, idx: u8, extra_half_brite: bool) -> u16 {
    let color = color_rgb12(palette[(idx as usize) & 0x1F]);
    if extra_half_brite && idx & 0x20 != 0 {
        half_brite_rgb12(color)
    } else {
        color
    }
}

pub(super) fn shres_composite_sample(
    left: DeniseBitplaneSample,
    right: DeniseBitplaneSample,
) -> DeniseBitplaneSample {
    DeniseBitplaneSample {
        idx: left.idx | right.idx,
        nplanes: left.nplanes.max(right.nplanes),
        active: left.active || right.active,
    }
}

/// Per-channel mean of two 24-bit colours without intermediate overflow.
pub(super) fn rgb24_blend_halves(a: u32, b: u32) -> u32 {
    (a & b) + (((a ^ b) & 0x00FE_FEFE) >> 1)
}

/// Per-channel mean of two packed RGBA8 pixels, including genlock alpha.
pub(super) fn rgba8_blend_halves(a: u32, b: u32) -> u32 {
    (a & b) + (((a ^ b) & 0xFEFE_FEFE) >> 1)
}

/// Resolve the two 35 ns samples of one 70 ns framebuffer column, each
/// through the full palette pipeline (ECS Denise carries at most two
/// bitplanes into SHRES; AGA Lisa runs the complete 8-bit index path). On
/// a 35 ns-pitch canvas each half is emitted as its own pixel; on the
/// classic hi-res-pitch canvas the pair is blended by
/// [`denise_shres_playfield_output`].
pub(super) fn denise_shres_playfield_output_pair(
    control: ControlState,
    palette: &Palette,
    left_idx: u8,
    right_idx: u8,
    ham_color: &mut u32,
) -> (DenisePlayfieldOutput, DenisePlayfieldOutput) {
    let left = denise_playfield_output(control, palette, left_idx, ham_color);
    let right = denise_playfield_output(control, palette, right_idx, ham_color);
    (left, right)
}

/// Merge a resolved 35 ns pair into one hi-res-pitch output: the blend is
/// a framebuffer-pitch compromise, not hardware, used when the canvas
/// carries one colour per 70 ns pixel.
pub(super) fn blend_shres_outputs(
    left: DenisePlayfieldOutput,
    right: DenisePlayfieldOutput,
) -> DenisePlayfieldOutput {
    DenisePlayfieldOutput {
        color: rgb24_blend_halves(left.color, right.color),
        color_latch: right.color_latch,
        pf_mask: left.pf_mask | right.pf_mask,
    }
}

pub(super) fn denise_playfield_output(
    control: ControlState,
    palette: &Palette,
    idx: u8,
    ham_color: &mut u32,
) -> DenisePlayfieldOutput {
    if control.aga() {
        return denise_aga_playfield_output(control, palette, idx, ham_color);
    }

    if control.hold_and_modify() {
        if control.dual_playfield() {
            // Invalid HAM + dual-playfield combination. Denise resolves the
            // dual-playfield colour index and then runs it through the HAM
            // logic: the HAM control code still comes from the raw plane-5/6
            // bits, but the value nibble (and the "set" palette index) is the
            // dual-playfield-resolved index, not the raw plane bits (vAmiga
            // translateDPF writes mBuffer with the resolved index, then
            // colorizeHAM takes the control from dBuffer bits 4-5). No real
            // software sets both bits; regression coverage is vAmigaTS
            // Denise/BPLCON0/modes4 and invprio3.
            let (pf_mask, color_idx) = dual_playfield_pixel(idx, control);
            let ham_code = ((idx >> 4) & 0x03) << 4 | ((color_idx as u8) & 0x0F);
            let previous = rgb24_to_rgb12_hi(*ham_color);
            *ham_color = rgb12_to_rgb24(ham6_rgb12(palette, ham_code, previous));
            return DenisePlayfieldOutput {
                color: *ham_color,
                color_latch: palette.get(color_idx).copied().unwrap_or(0),
                pf_mask,
            };
        }
        let previous = rgb24_to_rgb12_hi(*ham_color);
        *ham_color = rgb12_to_rgb24(ham6_rgb12(palette, idx, previous));
        return DenisePlayfieldOutput {
            color: *ham_color,
            color_latch: palette[(idx as usize) & 0x1F],
            pf_mask: u8::from(idx != 0) * 2,
        };
    }

    if control.dual_playfield() {
        let (pf_mask, color_idx) = dual_playfield_pixel(idx, control);
        let color_latch = palette.get(color_idx).copied().unwrap_or(0);
        let color = rgb12_to_rgb24(color_rgb12(color_latch));
        *ham_color = color;
        return DenisePlayfieldOutput {
            color,
            color_latch,
            pf_mask,
        };
    }

    // A single playfield whose BPLCON2 PF2 priority code is programmed out of
    // range (5-7) eliminates the four low bitplanes wherever the fifth
    // bitplane is set, keeping only bitplanes 5-6, and forces the pixel to
    // background sprite priority (vAmiga translateSPF; the quirk does not
    // happen in HAM mode, already returned above). Real software only uses
    // codes 0-4, so valid single-playfield content is unaffected.
    let invalid_pf2_priority = control.playfield_priority_code(2) > 4;
    let (idx, pf_mask) = if invalid_pf2_priority {
        let idx = if idx & 0x10 != 0 { idx & 0x30 } else { idx };
        (idx, 0)
    } else {
        (idx, u8::from(idx != 0) * 2)
    };
    let color_latch = palette[(idx as usize) & 0x1F];
    let color = rgb12_to_rgb24(palette_index_to_rgb12(
        palette,
        idx,
        control.extra_half_brite(),
    ));
    *ham_color = color;
    DenisePlayfieldOutput {
        color,
        color_latch,
        pf_mask,
    }
}

/// Lisa pixel resolution: 24-bit colours from the banked palette, BPLCON4
/// BPLAM XOR applied to the full pixel index, HAM8 with 8 bitplanes (HAM
/// with 5/6 planes keeps the OCS-compatible HAM6 maths on the high
/// nibbles), and EHB halving in 8-bit component space.
pub(super) fn denise_aga_playfield_output(
    control: ControlState,
    palette: &Palette,
    idx: u8,
    ham_color: &mut u32,
) -> DenisePlayfieldOutput {
    let idx = idx ^ control.bplam();
    let color_latch = palette[(idx as usize) & 0xFF];

    if control.bplcon0 & 0x0800 != 0 && control.nplanes() == 8 {
        *ham_color = ham8_rgb24(palette, idx, *ham_color);
        return DenisePlayfieldOutput {
            color: *ham_color,
            color_latch,
            pf_mask: u8::from(idx != 0) * 2,
        };
    }
    if control.hold_and_modify() {
        let previous = rgb24_to_rgb12_hi(*ham_color);
        *ham_color = rgb12_to_rgb24(ham6_rgb12(palette, idx, previous));
        return DenisePlayfieldOutput {
            color: *ham_color,
            color_latch,
            pf_mask: u8::from(idx != 0) * 2,
        };
    }

    if control.dual_playfield() {
        let (pf_mask, color_idx) = dual_playfield_pixel(idx, control);
        let color = palette.rgb24(color_idx) & 0x00FF_FFFF;
        *ham_color = color;
        return DenisePlayfieldOutput {
            color,
            color_latch: palette.get(color_idx).copied().unwrap_or(0),
            pf_mask,
        };
    }

    let mut color = palette.rgb24((idx as usize) & 0xFF) & 0x00FF_FFFF;
    if control.extra_half_brite() && idx & 0x20 != 0 {
        color = palette.rgb24((idx as usize) & 0x1F) & 0x00FF_FFFF;
        color = (color >> 1) & 0x007F_7F7F;
    }
    *ham_color = color;
    DenisePlayfieldOutput {
        color,
        color_latch,
        pf_mask: u8::from(idx != 0) * 2,
    }
}

/// AGA HAM8: unlike HAM6 (whose control bits are the two highest planes),
/// planes 1-2 (pixel bits 0-1) select the operation and planes 3-8 carry a
/// 6-bit value that replaces the top six bits of the modified component
/// (the low two bits hold their previous value). The set operation looks
/// up base palette entry `idx >> 2` (0-63). Hires HAM8 content is the
/// regression example for the bit assignment.
pub(super) fn ham8_rgb24(palette: &Palette, idx: u8, previous: u32) -> u32 {
    let value = u32::from(idx & 0xFC);
    match idx & 0x03 {
        0 => palette.rgb24(usize::from(idx >> 2)) & 0x00FF_FFFF,
        // 01 modifies blue, 10 modifies red, 11 modifies green.
        1 => (previous & 0x00FF_FF00) | (value | (previous & 0x03)),
        2 => (previous & 0x0000_FFFF) | ((value | ((previous >> 16) & 0x03)) << 16),
        _ => (previous & 0x00FF_00FF) | ((value | ((previous >> 8) & 0x03)) << 8),
    }
}

#[cfg(test)]
pub(super) fn dual_playfield_palette_index(idx: u8, control: ControlState) -> usize {
    dual_playfield_pixel(idx, control).1
}

pub(super) fn dual_playfield_pixel(idx: u8, control: ControlState) -> (u8, usize) {
    // OCS/ECS dual playfield splits six bitplanes into two 3-bit fields
    // (PF1 = planes 1/3/5, PF2 = planes 2/4/6). AGA Lisa extends each field
    // to four bits with the 7th and 8th bitplanes (PF1 += plane 7, PF2 +=
    // plane 8), so a 7-8 plane dual playfield addresses palette entries
    // 8..15 per field. Pre-AGA chips never carry bitplanes 7/8, so the
    // extra bits are always clear there and the 3-bit decode is preserved.
    let mut pf1 = (idx & 0x01) | ((idx >> 1) & 0x02) | ((idx >> 2) & 0x04);
    let mut pf2 = ((idx >> 1) & 0x01) | ((idx >> 2) & 0x02) | ((idx >> 3) & 0x04);
    if control.aga() {
        pf1 |= (idx >> 3) & 0x08;
        pf2 |= (idx >> 4) & 0x08;
    }
    let pf2_offset = control.pf2_palette_offset();
    let (winner, pf_mask, color_idx) = match (pf1, pf2) {
        (0, 0) => return (0, 0),
        (pf, 0) => (1u8, 1u8, pf as usize),
        (0, pf) => (2, 2, pf2_offset + pf as usize),
        (_, pf2) if control.pf2_priority() => (2, 2, pf2_offset + pf2 as usize),
        (pf1, _) => (1, 1, pf1 as usize),
    };
    // Denise draws a field transparent when its BPLCON2 priority code is
    // programmed out of range (5-7): the winning field's pixels collapse to
    // the background instead of revealing the field behind it. Photographed
    // on an A500 (vAmigaTS Denise/Registers/BPLCON0/invprio1, PF2 code 7,
    // where the real machine shows background between the bars).
    //
    // Lisa does not inherit the quirk. Alfred Chicken (AGA) programs
    // BPLCON2 = 0x003F -- both codes 7 -- for its whole in-game display and
    // draws an eight-plane dual playfield on real hardware, which the
    // Denise behaviour would blank to the background colour. The quirk
    // reached us from an OCS/ECS-only reference, so it never carried
    // evidence about Lisa; WinUAE, which does model AGA, resolves the
    // playfield colour from the plane bits alone and uses the priority
    // codes only to mask sprites. So the split is drawn where the evidence
    // is: the photographed Denise case keeps the quirk, and Lisa resolves
    // the colour independently of the code.
    // TODO: the Lisa side rests on the game plus WinUAE, with no photograph
    // of its own; shoot invprio1 on an A1200 to confirm Lisa ignores
    // out-of-range codes entirely rather than differing in some other way.
    if !control.aga() && control.playfield_priority_code(winner) > 4 {
        return (0, 0);
    }
    (pf_mask, color_idx)
}

pub(super) fn half_brite_rgb12(color: u16) -> u16 {
    let r = ((color >> 8) & 0x0F) >> 1;
    let g = ((color >> 4) & 0x0F) >> 1;
    let b = (color & 0x0F) >> 1;
    (r << 8) | (g << 4) | b
}

pub(super) fn ham6_rgb12(palette: &Palette, idx: u8, previous: u16) -> u16 {
    let data = (idx & 0x0F) as u16;
    match idx >> 4 {
        0 => color_rgb12(palette[data as usize]),
        1 => (previous & 0x0FF0) | data,
        2 => (previous & 0x00FF) | (data << 8),
        _ => (previous & 0x0F0F) | (data << 4),
    }
}
