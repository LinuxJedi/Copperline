// SPDX-License-Identifier: GPL-3.0-or-later

//! Decode the Amiga's planar bitmap into a packed RGBA8 framebuffer.
//!
//! The Copper is executed by the bus as beam time advances. Rendering
//! replays the recorded custom-register writes for the most recently
//! completed frame so scanline and horizontal palette/control changes
//! are based on scheduled execution rather than reparsing COP1LC.

use super::FrameGeometry;
#[cfg(test)]
use super::FB_PIXELS;
use super::{FB_HEIGHT, FB_WIDTH, MAX_VISIBLE_LINES};
use crate::bus::{
    BeamChipRamWrite, BeamRegisterWrite, BeamWriteSource, Bus, CapturedBitplaneRow,
    CapturedSpriteLine, HeldSpriteLine, RenderRegisterSnapshot, VideoRenderFrameTiming,
};
use crate::chipset::agnus::{ddf_hard_bounds, sprite_dma_disabled_by_bitplane_ddf, AgnusRevision};
#[cfg(test)]
use crate::chipset::denise::BPLCON3_PF2OF_DEFAULT;
use crate::chipset::denise::{
    color_register_value, rgb12_to_rgb24, rgb12_to_rgba8, rgb24_to_rgba8, BitplaneMode, DiwHigh,
    Palette, COLOR_RGB_MASK, COLOR_TRANSPARENCY_BIT,
};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Instant;

// Beam-to-framebuffer conversion anchors for the pragmatic renderer.
// They are derived from the OCS PAL display window/fetch positions
// used by DiagROM and the boot ROM screen, not from a cycle-exact
// Denise/Agnus scheduler.
#[cfg_attr(not(test), allow(dead_code))]
const PAL_VISIBLE_LINE0: i32 = 0x2C;
// Framebuffer x=0 anchor. Held 8 colour clocks (16 lo-res pixels) left of the
// standard display start so the framebuffer captures the deep-left overscan a
// real Denise can display, matching vAmiga's 716-wide regression cutout. The
// matching shift of COPPER_WAIT_HPOS_FB0 (below) keeps the bitplane/register
// pipeline delta (BITPLANE_CONTROL_PIPELINE_FB) invariant.
const DIW_HSTART_FB0: i32 = 0x61;
const STANDARD_DIW_HSTART: i32 = 0x81;
// Standard DIWSTRT $81 is the visible window edge. The first standard
// bitplane sample at DDFSTRT $38 is already one lowres native sample into the
// fetched word, so the fetch/output phase is referenced one color clock earlier.
//
// The lo-res fetch/display phase sits 3 colour clocks later than the hi-res
// phase: a hi-res fetch slot delivers its word to Denise's shifter on a
// different beam edge than a lo-res slot, so the reference the renderer uses to
// place the first fetched pixel differs by resolution. Lo-res references $80;
// hi-res references $83 (verified against vAmiga: low-res HAM playfields align
// with their bezel sprites at $80, while hi-res boot-screen text aligns at $83).
// See `fetch_reference` below.
const DIW_HSTART_FETCH_REFERENCE_LORES: i32 = 0x80;
const DIW_HSTART_FETCH_REFERENCE_HIRES: i32 = 0x83;
// Register/copper-write x=0 anchor, in colour clocks. Moved left by 8 colour
// clocks in lockstep with DIW_HSTART_FB0 (16 lo-res pixels) so register writes
// and bitplane pixels still register against each other after widening.
const COPPER_WAIT_HPOS_FB0: i32 = 0x28;
/// COLORxx writes feed Denise's final colour-selection/output path. Denise
/// applies copper/CPU colour-register changes in the palette/output phase,
/// ahead of register writes that feed the bitplane shifter. This anchor keeps
/// COLORxx changes one lores pixel ahead of the generic beam/write domain,
/// matching vAmiga's model where COLORxx is recorded at the current output
/// pixel while BPLCON/DDF/sprite data paths carry explicit pixel delays. OCS
/// Denise (8362) and ECS Denise (8373) share this timing exactly -- the only
/// OCS/ECS colour-path difference is the OCS 12-bit value mask -- so this
/// anchor is revision-independent across OCS/ECS.
///
/// TODO: AGA Lisa delays colour changes by one hires pixel relative to
/// OCS/ECS (WinUAE: "AGA color changes are 1 hires pixel delayed"). That
/// sub-colour-clock offset is not yet modelled here.
///
/// STOP before retuning this. If a scene's colours or copper-driven picture
/// look horizontally shifted, the cause is usually bitplane fetch/DDF
/// alignment, sprite arming, or a missed write-domain delay, not this final
/// colour-output anchor.
const COLOR_WRITE_HPOS_FB0: i32 = 0x35;
/// AGA BPLCON4's low sprite-palette byte follows Lisa's sprite colour lookup
/// path, which reaches sprite output earlier than ordinary COLORxx palette
/// writes. Keep it separate from COLOR replay so copper palette gradients stay
/// in the Denise palette-output phase on OCS/ECS.
const SPRITE_PALETTE_CONTROL_HPOS_FB0: i32 = 0x36;
/// SPRxPOS and SPRxDATx writes feed Denise's sprite comparator/latches seven
/// CCK ahead of the normal register/output beam domain. Manual sprite replays
/// use this earlier domain so adjacent position writes can abut at their
/// programmed HSTARTs, and data writes that beat the comparator load the same
/// scanline.
const SPRITE_REGISTER_WRITE_PIPELINE_CCK: u32 = 7;
/// Framebuffer-x offset between the copper/register coordinate
/// ([`COPPER_WAIT_HPOS_FB0`], used to place beam-timed register writes) and the
/// bitplane/DIW coordinate ([`DIW_HSTART_FB0`], used to place fetched bitplane
/// pixels, which bakes in the Agnus-fetch -> Denise-display pipeline delay).
///
/// A register write at copper-x maps to bitplane-x `copper_x - this`. Bitplane
/// control writes (BPLCON scroll/mode) feed the bitplane shifter, so the scroll
/// they set must be applied to the pixels they actually control. Without this
/// correction a per-line scroll write lands
/// to the right of the first fetched word it governs, leaving that word's left
/// edge using the previous line's stale scroll -- a duplicate ("E-clone") of the
/// playfield's left edge in the deep left overscan at maximum scroll.
const BITPLANE_CONTROL_PIPELINE_FB: usize =
    ((DIW_HSTART_FB0 - COPPER_WAIT_HPOS_FB0 * 2) * 2) as usize;
/// Framebuffer x of the left edge of the standard (non-overscan) display. The
/// columns to its left are overscan border that a real PAL display crops.
pub(crate) const STANDARD_VISIBLE_X0: usize = ((STANDARD_DIW_HSTART - DIW_HSTART_FB0) * 2) as usize;
/// Standard PAL display right edge (DIWSTOP H), in colour clocks. Columns to
/// its right are overscan a real PAL display crops. OCS forces DIWSTOP bit 8,
/// so the stock $2CC1 DIWSTOP yields $1C1.
const STANDARD_DIW_HSTOP: i32 = 0x1C1;

/// Horizontal presentation shift, in framebuffer pixels, that recentres a
/// standard (non-overscan) display inside the overscan field buffer.
///
/// The framebuffer anchors a deep slab of left overscan ([`DIW_HSTART_FB0`] is
/// 0x20 colour clocks left of the standard display start = 64 hi-res px) but
/// only a few px of right overscan, so a stock display sits right-of-centre
/// compared with vAmiga/FS-UAE, which crop overscan roughly symmetrically.
/// Shifting the picture left by half the border asymmetry centres it.
///
/// Returns 0 (leave the frame untouched) whenever the window uses left *or*
/// right overscan, so overscan demos -- which deliberately fetch into the
/// border -- are presented exactly as rendered and never clipped.
pub fn present_h_shift(diw_h_start: u16, diw_h_stop: u16) -> usize {
    let h_start = diw_h_start as i32;
    let h_stop = diw_h_stop as i32;
    if h_start < STANDARD_DIW_HSTART || h_stop > STANDARD_DIW_HSTOP {
        return 0;
    }
    let left_border = ((h_start - DIW_HSTART_FB0).max(0) * 2) as usize;
    let right_x = ((h_stop - DIW_HSTART_FB0).max(0) * 2) as usize;
    let right_border = FB_WIDTH.saturating_sub(right_x);
    left_border.saturating_sub(right_border) / 2
}

/// `present_h_shift`, but able to recentre a display that opens its DIW window
/// into the overscan around a picture it only fetches at standard width.
///
/// A demo can open DIWSTRT/DIWSTOP much wider than the playfield it draws --
/// Virtual Dreams' "Absolute Inebriation" opens DIW $02..$1FF around a standard
/// 320-px lo-res picture (DDF $38..$D0) -- where the extra window only reveals
/// COLOR0 border the TV crops, not content. The bare `present_h_shift` keys off
/// DIW alone and so declines to recentre any window that reaches into the
/// overscan, leaving such a picture sitting right-of-centre.
///
/// While the DIW window stays inside the standard window, behaviour is
/// identical to `present_h_shift(diw_h_start, diw_h_stop)` -- the window
/// (including any COLOR0 border it legitimately shows) is centred as before.
/// Only when the window reaches into the overscan do we fall back to centring
/// on the *fetched content* (DDF) clamped to the window: a picture whose fetch
/// stays within the standard window is recentred like the standard display it
/// really is, while one that genuinely fetches bitplane data into the border is
/// still left exactly as rendered (`present_h_shift` returns 0 for it).
pub fn present_h_shift_for(snapshot: &RenderRegisterSnapshot) -> usize {
    let control = ControlState::from_render_state(&RenderState::from_snapshot(*snapshot));
    let diw_start = control.diw_h_start() as i32;
    let mut diw_stop = control.diw_h_stop() as i32;
    if diw_stop <= diw_start {
        diw_stop += 0x100;
    }
    // DIW within the standard window: centre on the window itself, exactly as
    // the bare helper does, so stock and sub-standard displays are unchanged.
    if diw_start >= STANDARD_DIW_HSTART && diw_stop <= STANDARD_DIW_HSTOP {
        return present_h_shift(diw_start as u16, diw_stop as u16);
    }
    // DIW reaches into the overscan: recentre on the fetched content if it
    // stays standard, otherwise leave a true overscan display untouched.
    let Some((content_start, content_stop)) = control.bitplane_content_window_h() else {
        return 0;
    };
    let eff_start = diw_start.max(content_start);
    let eff_stop = diw_stop.min(content_stop);
    if eff_stop <= eff_start {
        return 0;
    }
    present_h_shift(eff_start as u16, eff_stop as u16)
}
const BPLCON0_ECSENA: u16 = 1 << 0;
const BPLCON0_SHRES: u16 = 1 << 6;
const BPLCON2_ZDBPSEL_SHIFT: u16 = 12;
const BPLCON2_ZDBPSEL_MASK: u16 = 0x7000;
const BPLCON2_ZDBPEN: u16 = 1 << 11;
const BPLCON2_ZDCTEN: u16 = 1 << 10;
const BPLCON2_KILLEHB: u16 = 1 << 9;
const BPLCON3_BRDSPRT: u16 = 1 << 1;
const BPLCON3_ZDCLKEN: u16 = 1 << 2;
const BPLCON3_BRDNTRAN: u16 = 1 << 4;
const BPLCON3_BRDRBLNK: u16 = 1 << 5;
const BPLCON3_SPRES_MASK: u16 = 0x00C0;
const BPLCON3_SPRES_LORES: u16 = 0x0040;
const BPLCON3_SPRES_HIRES: u16 = 0x0080;
const BPLCON3_SPRES_SHRES: u16 = 0x00C0;
const BPLCON3_PF2OF_MASK: u16 = 0x1C00;
const DMACON_SPREN: u16 = 1 << 5;
const DMACON_BPLEN: u16 = 1 << 8;
const DMACON_DMAEN: u16 = 1 << 9;

#[cfg(test)]
fn sprite_display_enabled_from_line_start() -> [Option<usize>; FB_HEIGHT] {
    [Some(0); FB_HEIGHT]
}
const BITPLANE_DDF_HARD_START: u16 = 0x0018;
const BITPLANE_DDF_HARD_STOP: u16 = 0x00D8;
const BITPLANE_FETCH_HARD_END: u32 = BITPLANE_DDF_HARD_STOP as u32 + 7;
const OCS_LORES_BPL_SEQUENCE: [usize; 8] = [8, 4, 6, 2, 7, 3, 5, 1];
#[cfg_attr(
    not(any(test, debug_assertions, feature = "display-plan-trace")),
    allow(dead_code)
)]
const SPRITE_DMA_PAIR_CAPTURE_HPOS: [u32; 4] = [0x018, 0x020, 0x028, 0x030];

#[derive(Clone, Copy)]
/// One COLORxx write replayed mid-line: at framebuffer x, the absolute
/// palette entry (bank * 32 + index on AGA, plain index otherwise) takes
/// `value` on the high-nibble plane (and low, unless `loct`). Stored as a
/// diff (not a full palette snapshot) so copper-palette-heavy frames stay
/// cheap with the 256-entry store.
struct PaletteSegment {
    x: usize,
    entry: u8,
    loct: bool,
    value: u16,
}

impl PaletteSegment {
    fn apply(&self, palette: &mut Palette) {
        palette.write_entry(usize::from(self.entry), self.loct, self.value);
    }
}

#[derive(Clone, Copy)]
struct PaletteRowDiag {
    first_vpos: u32,
    last_vpos: u32,
}

impl PaletteRowDiag {
    fn contains(self, vpos: u32) -> bool {
        (self.first_vpos..=self.last_vpos).contains(&vpos)
    }
}

fn parse_diag_u32(raw: &str) -> Option<u32> {
    let raw = raw.trim();
    if let Some(hex) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).ok()
    } else {
        raw.parse::<u32>().ok()
    }
}

/// Cached COPPERLINE_DIAG_PALETTE_ROW setting (read once). Accepted forms:
/// presence/`all` logs every COLOR write, `V` logs one beam line, and
/// `START:END` logs an inclusive beam-line range.
fn palette_row_diag() -> Option<PaletteRowDiag> {
    static SPEC: OnceLock<Option<PaletteRowDiag>> = OnceLock::new();
    *SPEC.get_or_init(|| {
        let raw = crate::envcfg::var("COPPERLINE_DIAG_PALETTE_ROW")?;
        let raw = raw.trim();
        if raw.is_empty() || raw == "1" || raw.eq_ignore_ascii_case("all") {
            return Some(PaletteRowDiag {
                first_vpos: 0,
                last_vpos: u32::MAX,
            });
        }
        if let Some((first, last)) = raw.split_once(':') {
            let first_vpos = parse_diag_u32(first).unwrap_or(0);
            let last_vpos = parse_diag_u32(last).unwrap_or(u32::MAX);
            return Some(PaletteRowDiag {
                first_vpos: first_vpos.min(last_vpos),
                last_vpos: first_vpos.max(last_vpos),
            });
        }
        parse_diag_u32(raw).map(|vpos| PaletteRowDiag {
            first_vpos: vpos,
            last_vpos: vpos,
        })
    })
}

fn beam_write_source_label(source: BeamWriteSource) -> &'static str {
    match source {
        BeamWriteSource::Cpu => "cpu",
        BeamWriteSource::CpuCopperIrq => "cpu_copper_irq",
        BeamWriteSource::Copper => "copper",
    }
}

/// Cached COPPERLINE_CLAMP_PLANES setting (read once). `bitplane_mode` runs per
/// pixel in the playfield decode loop, so it must not do a map lookup (hashing
/// the name dominated the whole renderer in profiles).
fn clamp_planes_setting() -> Option<u16> {
    use std::sync::OnceLock;
    static V: OnceLock<Option<u16>> = OnceLock::new();
    *V.get_or_init(|| {
        crate::envcfg::var("COPPERLINE_CLAMP_PLANES").and_then(|v| v.trim().parse::<u16>().ok())
    })
}

/// Resolve where a COLORxx write lands in the palette store: the BPLCON3
/// BANK/LOCT mechanics on AGA, the plain entry otherwise.
fn palette_entry_for_write(bplcon3: u16, aga: bool, idx: usize) -> (u8, bool) {
    if aga {
        (
            (Palette::bank_from_bplcon3(bplcon3) * 32 + (idx & 31)) as u8,
            Palette::loct_from_bplcon3(bplcon3),
        )
    } else {
        ((idx & 31) as u8, false)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ControlState {
    agnus_revision: AgnusRevision,
    harddis: bool,
    dmacon: u16,
    bplcon0: u16,
    bplcon1: u16,
    bplcon2: u16,
    bplcon3: u16,
    bplcon4: u16,
    /// AGA FMODE latch; 0 on OCS/ECS.
    fmode: u16,
    clxcon: u16,
    clxcon2: u16,
    diwstrt: u16,
    diwstop: u16,
    diwhigh: DiwHigh,
    ddfstrt: u16,
    ddfstop: u16,
    bpl1mod: i16,
    bpl2mod: i16,
}

fn display_window_unprogrammed(diwstrt: u16, diwstop: u16) -> bool {
    diwstrt == 0 && diwstop == 0
}

impl ControlState {
    fn from_render_state(state: &RenderState) -> Self {
        Self {
            agnus_revision: state.agnus_revision,
            harddis: state.harddis,
            dmacon: state.dmacon,
            bplcon0: state.bplcon0,
            bplcon1: state.bplcon1,
            bplcon2: state.bplcon2,
            bplcon3: state.bplcon3,
            bplcon4: state.bplcon4,
            fmode: state.fmode,
            clxcon: state.clxcon,
            clxcon2: state.clxcon2,
            diwstrt: state.diwstrt,
            diwstop: state.diwstop,
            diwhigh: state.diwhigh,
            ddfstrt: state.ddfstrt,
            ddfstop: state.ddfstop,
            bpl1mod: state.bpl1mod,
            bpl2mod: state.bpl2mod,
        }
    }

    fn bitplane_mode(&self) -> BitplaneMode {
        // Debug aid: COPPERLINE_CLAMP_PLANES=N clamps the displayed bitplane count
        // to N by masking the BPLCON0 BPU field, for A/B testing which plane is
        // responsible for a rendering artifact.
        if let Some(n) = clamp_planes_setting() {
            let bpu = (self.bplcon0 >> 12) & 0x7;
            if bpu > n {
                let clamped = (self.bplcon0 & !0x7000) | (n << 12);
                return BitplaneMode::from_bplcon0(clamped, self.aga());
            }
        }
        BitplaneMode::from_bplcon0(self.bplcon0, self.aga())
    }

    fn aga(&self) -> bool {
        matches!(self.agnus_revision, AgnusRevision::AgaAlice)
    }

    fn nplanes(&self) -> usize {
        self.bitplane_mode().display_planes()
    }

    fn dma_planes(&self) -> usize {
        self.bitplane_mode().dma_planes()
    }

    fn bitplane_dma_enabled(&self) -> bool {
        self.dmacon & (DMACON_DMAEN | DMACON_BPLEN) == (DMACON_DMAEN | DMACON_BPLEN)
    }

    fn ecsena(&self) -> bool {
        self.bplcon0 & BPLCON0_ECSENA != 0
    }

    fn border_blank_enabled(&self) -> bool {
        self.ecsena() && self.bplcon3 & BPLCON3_BRDRBLNK != 0
    }

    fn border_non_transparent_enabled(&self) -> bool {
        self.ecsena() && self.bplcon3 & BPLCON3_BRDNTRAN != 0
    }

    fn border_sprite_enabled(&self) -> bool {
        self.ecsena() && self.bplcon3 & BPLCON3_BRDSPRT != 0
    }

    fn zd_clock_enabled(&self) -> bool {
        self.ecsena() && self.bplcon3 & BPLCON3_ZDCLKEN != 0
    }

    fn color_key_enabled(&self) -> bool {
        !self.zd_clock_enabled() && self.bplcon2 & BPLCON2_ZDCTEN != 0
    }

    fn bitplane_key_enabled(&self) -> bool {
        !self.zd_clock_enabled() && self.bplcon2 & BPLCON2_ZDBPEN != 0
    }

    fn bitplane_key_plane(&self) -> usize {
        ((self.bplcon2 & BPLCON2_ZDBPSEL_MASK) >> BPLCON2_ZDBPSEL_SHIFT) as usize
    }

    fn genlock_transparent(
        &self,
        color_latch: u16,
        sample: Option<DeniseBitplaneSample>,
        border: bool,
    ) -> bool {
        if self.zd_clock_enabled() || (border && self.border_non_transparent_enabled()) {
            return false;
        }
        let color_key = self.color_key_enabled() && color_latch & COLOR_TRANSPARENCY_BIT != 0;
        let bitplane_key = self.bitplane_key_enabled()
            && sample.is_some_and(|sample| {
                let plane = self.bitplane_key_plane();
                plane < sample.nplanes && sample.idx & (1 << plane) != 0
            });
        (border && self.border_blank_enabled()) || color_key || bitplane_key
    }

    fn sprite_pixel_repeat(&self) -> i32 {
        match self.bplcon3 & BPLCON3_SPRES_MASK {
            0 => {
                if self.bplcon0 & BPLCON0_SHRES != 0 {
                    1
                } else {
                    2
                }
            }
            BPLCON3_SPRES_LORES => 2,
            BPLCON3_SPRES_HIRES | BPLCON3_SPRES_SHRES => {
                // TODO: A true SHRES output path should emit 35 ns sprite
                // samples; the current framebuffer resolves 70 ns.
                1
            }
            _ => unreachable!(),
        }
    }

    fn display_window_contains_line(&self, line: usize, visible_line0: i32) -> bool {
        if display_window_unprogrammed(self.diwstrt, self.diwstop) {
            return true;
        }
        let start = self.diw_v_start() as i32;
        let mut stop = self.diw_v_stop() as i32;
        let mut v = visible_line0 + line as i32;
        if stop <= start {
            stop += 0x100;
            if v < start {
                v += 0x100;
            }
        }
        v >= start && v < stop
    }

    fn display_window_x(&self) -> (usize, usize) {
        if display_window_unprogrammed(self.diwstrt, self.diwstop) {
            return (0, FB_WIDTH);
        }
        let start = self.diw_h_start() as i32;
        let mut stop = self.diw_h_stop() as i32;
        if stop <= start {
            stop += 0x100;
        }
        let left = ((start - DIW_HSTART_FB0).max(0) as usize * 2).min(FB_WIDTH);
        let mut right = ((stop - DIW_HSTART_FB0).max(0) as usize * 2).min(FB_WIDTH);
        if FB_WIDTH.saturating_sub(right) <= 2 {
            right = FB_WIDTH;
        }
        if right > left {
            (left, right)
        } else {
            (left, FB_WIDTH)
        }
    }

    fn clipped_display_rows_before_frame(&self, visible_line0: i32) -> usize {
        if display_window_unprogrammed(self.diwstrt, self.diwstop) {
            return 0;
        }
        (visible_line0 - self.diw_v_start() as i32).max(0) as usize
    }

    fn diw_v_start(&self) -> u16 {
        self.diwhigh.v_start(self.diwstrt)
    }

    fn diw_v_stop(&self) -> u16 {
        self.diwhigh.v_stop(self.diwstop)
    }

    fn diw_h_start(&self) -> u16 {
        self.diwhigh.h_start(self.diwstrt)
    }

    fn diw_h_stop(&self) -> u16 {
        self.diwhigh.h_stop(self.diwstop)
    }

    fn hires(&self) -> bool {
        self.bplcon0 & 0x8000 != 0 && !self.shres()
    }

    fn shres(&self) -> bool {
        self.bplcon0 & BPLCON0_SHRES != 0
    }

    /// Beam hpos the renderer treats as the origin for the first fetched
    /// bitplane pixel. Hi-res delivers fetched words to Denise's shifter 3
    /// colour clocks earlier in phase than lo-res, so the two resolutions
    /// reference different anchors (see DIW_HSTART_FETCH_REFERENCE_* docs).
    /// Super-hi-res (ECS, effectively unused under OCS) keeps the lo-res
    /// anchor.
    fn fetch_reference(&self) -> i32 {
        if self.hires() {
            // Wide-FMODE hi-res fetches deliver their gulp to the shifter
            // one colour clock earlier than the single-word fetch the $83
            // reference was calibrated on: Lisa's bitplane delay is
            // fetch-width-dependent (the classic WinUAE/FS-UAE
            // `delayoffset` is computed from the fetch mode). With the
            // FMODE=0 reference, AGA hi-res screens (system software,
            // FMODE=3, DIW $81, DDFSTRT $38) sat 2 lo-res pixels right of
            // FS-UAE: a colour-0 stripe inside the window's left edge and
            // the bitmap's last pixels clipped at the right.
            if self.fetch_quantum() > 1 {
                DIW_HSTART_FETCH_REFERENCE_HIRES - 2
            } else {
                DIW_HSTART_FETCH_REFERENCE_HIRES
            }
        } else {
            DIW_HSTART_FETCH_REFERENCE_LORES
        }
    }

    fn framebuffer_pixel_repeat(&self) -> usize {
        if self.hires() || self.shres() {
            1
        } else {
            2
        }
    }

    fn native_samples_per_framebuffer_pixel(&self) -> usize {
        if self.shres() {
            2
        } else {
            1
        }
    }

    fn fetch_cck_per_word(&self) -> u32 {
        if self.shres() {
            2
        } else if self.hires() {
            4
        } else {
            8
        }
    }

    /// AGA FMODE: 16-bit words per bitplane fetch slot.
    fn fetch_quantum(&self) -> u32 {
        if !self.aga() {
            return 1;
        }
        match self.fmode & 0x0003 {
            0 => 1,
            3 => 4,
            _ => 2,
        }
    }

    /// Colour clocks between successive fetches of one plane.
    fn fetch_period(&self) -> u32 {
        self.fetch_cck_per_word() * self.fetch_quantum()
    }

    /// DDF block quantum in colour clocks (8 at FMODE=0).
    fn fetch_unit(&self) -> u32 {
        self.fetch_period().max(8)
    }

    /// AGA BPLCON4 BPLAM: XOR mask applied to the bitplane pixel index.
    fn bplam(&self) -> u8 {
        (self.bplcon4 >> 8) as u8
    }

    fn extra_half_brite(&self) -> bool {
        // OCS EHB is selected by six bitplanes with HAM and dual-playfield
        // disabled. Bitplane 6 halves the intensity of colors 0..31.
        self.nplanes() == 6 && self.bplcon0 & 0x0C00 == 0 && self.bplcon2 & BPLCON2_KILLEHB == 0
    }

    fn hold_and_modify(&self) -> bool {
        !self.shres() && matches!(self.nplanes(), 5 | 6) && self.bplcon0 & 0x0800 != 0
    }

    fn dual_playfield(&self) -> bool {
        self.bplcon0 & 0x0400 != 0
    }

    fn pf2_priority(&self) -> bool {
        self.bplcon2 & 0x0040 != 0
    }

    fn pf2_palette_offset(&self) -> usize {
        match (self.bplcon3 & BPLCON3_PF2OF_MASK) >> 10 {
            0 => 0,
            1 => 2,
            2 => 4,
            3 => 8,
            4 => 16,
            5 => 32,
            6 => 64,
            7 => 128,
            _ => unreachable!(),
        }
    }

    fn pf1_scroll(&self) -> usize {
        if self.aga() {
            return self.aga_bplcon1_scroll_samples(false);
        }
        (self.bplcon1 & 0x000F) as usize
    }

    fn pf2_scroll(&self) -> usize {
        if self.aga() {
            return self.aga_bplcon1_scroll_samples(true);
        }
        ((self.bplcon1 >> 4) & 0x000F) as usize
    }

    /// AGA Lisa expands BPLCON1 to two 8-bit scroll counters in 35 ns
    /// super-hires units. The old OCS/ECS nibble bits are renamed to H2..H5,
    /// preserving old lo-res scroll values, while the new bits provide
    /// sub-lores positioning and the extra range needed by wide FMODE fetches.
    fn aga_bplcon1_scroll_samples(&self, pf2: bool) -> usize {
        let shres_scroll = if pf2 {
            ((self.bplcon1 >> 12) & 0x0001)
                | ((self.bplcon1 >> 12) & 0x0002)
                | ((self.bplcon1 >> 2) & 0x0004)
                | ((self.bplcon1 >> 2) & 0x0008)
                | ((self.bplcon1 >> 2) & 0x0010)
                | ((self.bplcon1 >> 2) & 0x0020)
                | ((self.bplcon1 >> 8) & 0x0040)
                | ((self.bplcon1 >> 8) & 0x0080)
        } else {
            ((self.bplcon1 >> 8) & 0x0001)
                | ((self.bplcon1 >> 8) & 0x0002)
                | ((self.bplcon1 << 2) & 0x0004)
                | ((self.bplcon1 << 2) & 0x0008)
                | ((self.bplcon1 << 2) & 0x0010)
                | ((self.bplcon1 << 2) & 0x0020)
                | ((self.bplcon1 >> 4) & 0x0040)
                | ((self.bplcon1 >> 4) & 0x0080)
        } as usize;
        let fetch_mask = match self.fetch_quantum() {
            1 => 0x3F,
            2 => 0x7F,
            _ => 0xFF,
        };
        let shres_scroll = shres_scroll & fetch_mask;
        let samples_per_native = if self.shres() {
            1
        } else if self.hires() {
            2
        } else {
            4
        };
        shres_scroll / samples_per_native
    }

    fn scroll_for_plane(&self, plane: usize) -> usize {
        if plane & 1 != 0 {
            self.pf2_scroll()
        } else {
            self.pf1_scroll()
        }
    }

    fn playfield_priority_code(&self, playfield: u8) -> u8 {
        if playfield == 1 {
            (self.bplcon2 & 0x0007) as u8
        } else {
            ((self.bplcon2 >> 3) & 0x0007) as u8
        }
    }

    /// End-of-line modulo for a plane. FMODE BSCAN2 (bit 14, Alice only)
    /// scan-doubles bitplanes: both plane groups share one modulo, selected
    /// by the line parity relative to DIWSTRT's vertical start - the
    /// matching-parity line adds BPL1MOD, the doubled line BPL2MOD (WinUAE
    /// model). Software doubles each row by rewinding with
    /// BPL1MOD = -(row bytes) and advancing with BPL2MOD.
    fn modulo_for_plane(&self, plane: usize, vpos: i32) -> i32 {
        if self.aga() && self.fmode & 0x4000 != 0 {
            return if (i32::from(self.diwstrt >> 8) ^ vpos) & 1 != 0 {
                self.bpl2mod as i32
            } else {
                self.bpl1mod as i32
            };
        }
        if plane & 1 == 0 {
            self.bpl1mod as i32
        } else {
            self.bpl2mod as i32
        }
    }

    fn words_per_row(&self, native_w: usize) -> usize {
        let fallback = native_w / 16;
        let Some((start, stop)) = effective_ddf_window(
            self.agnus_revision,
            self.hires() || self.shres(),
            self.ddfstrt,
            self.ddfstop,
            self.harddis,
        ) else {
            return fallback;
        };
        let unit = self.fetch_unit();
        let start = crate::chipset::agnus::anchor_bitplane_fetch_start(start, unit);
        let blocks = crate::chipset::agnus::bitplane_fetch_blocks(u32::from(stop - start), unit);
        let words = blocks * (unit / self.fetch_cck_per_word()) as usize;
        words.max(1)
    }

    fn has_valid_ddf_window(&self) -> bool {
        let hires_like = self.hires() || self.shres();
        let start = effective_ddf_start_hpos_raw(self.agnus_revision, hires_like, self.ddfstrt);
        let stop = effective_ddf_stop_hpos(self.agnus_revision, hires_like, self.ddfstop);
        (start == 0 && stop == 0)
            || effective_ddf_window(
                self.agnus_revision,
                hires_like,
                self.ddfstrt,
                self.ddfstop,
                self.harddis,
            )
            .is_some()
    }

    /// Horizontal extent, in DIWSTRT/DIWSTOP H coordinates, of the bitplane
    /// data this control actually fetches: the display-data-fetch window
    /// (DDFSTRT/DDFSTOP, rounded to completed fetch units) widened by the
    /// fetched word count at the current resolution. Calibrated so a standard DDF
    /// window ($38/$D0 lo-res, $3C/$D4 hi-res) yields exactly the standard
    /// DIW edges (`STANDARD_DIW_HSTART`..`STANDARD_DIW_HSTOP`): the picture a
    /// stock display fetches lands on the same beam positions as its DIW
    /// window, so presentation centring of a stock display is unchanged.
    ///
    /// Returns `None` when no valid DDF window is programmed, so the caller
    /// falls back to the raw DIW window.
    fn bitplane_content_window_h(&self) -> Option<(i32, i32)> {
        let hires_like = self.hires() || self.shres();
        // Reuse `words_per_row`'s own validity check (same arguments): a
        // valid window guarantees a non-fallback word count below.
        effective_ddf_window(
            self.agnus_revision,
            hires_like,
            self.ddfstrt,
            self.ddfstop,
            self.harddis,
        )?;
        let words = self.words_per_row(0) as i32;
        if words == 0 {
            return None;
        }
        // The displayed shifter origin moves in whole fetch gulps, matching
        // the renderer's placement (see `fetch_origin_native_shift`). This is
        // separate from the DMA slot positions, which start at the
        // revision-masked DDFSTRT comparator value. Each colour clock of DDF
        // shift moves the picture two lo-res H units.
        let gulp = self.fetch_period() as i32;
        let align = |hpos: i32| -> i32 {
            (hpos.div_euclid(gulp) * gulp).max(BITPLANE_DDF_HARD_START as i32)
        };
        let standard_ddf = if hires_like { 0x003C } else { 0x0038 };
        let aligned_start =
            align(effective_ddf_start_hpos(self.agnus_revision, hires_like, self.ddfstrt) as i32);
        let start_h = STANDARD_DIW_HSTART + (aligned_start - align(standard_ddf)) * 2;
        // Fetched H width: one word spans 16 lo-res, 8 hi-res, or 4 super-hi-res
        // H units, so the standard 20/40/80-word row is 320 H units wide.
        let h_units_per_word = if self.shres() {
            4
        } else if self.hires() {
            8
        } else {
            16
        };
        Some((start_h, start_h + words * h_units_per_word))
    }

    fn fetch_start_native_x(&self, diw_h_start: u16, pixel_repeat: usize) -> usize {
        (-self.fetch_origin_native_shift(diw_h_start, pixel_repeat)).max(0) as usize
    }

    fn native_x_offset(&self, diw_h_start: u16, pixel_repeat: usize) -> usize {
        self.fetch_origin_native_shift(diw_h_start, pixel_repeat)
            .max(0) as usize
    }

    fn ham_history_start_native_x(
        &self,
        diw_h_start: u16,
        pixel_repeat: usize,
        native_x_offset: usize,
    ) -> usize {
        let display_phase_native =
            ((diw_h_start as i32 - self.fetch_reference()) * 2) / pixel_repeat as i32;
        let visible_phase = display_phase_native.max(0) as usize;
        native_x_offset.saturating_sub(visible_phase.min(native_x_offset))
    }

    fn fetch_origin_native_shift(&self, diw_h_start: u16, pixel_repeat: usize) -> i32 {
        let display_native_shift =
            ((diw_h_start as i32 - self.fetch_reference()) * 2) / pixel_repeat as i32;
        let standard_ddf = if self.hires() || self.shres() {
            0x003C
        } else {
            0x0038
        };
        let ddf_native_scale = if self.shres() {
            8
        } else if self.hires() {
            4
        } else {
            2
        };
        // The displayed picture position is quantized to the fetch-period
        // grid (one FMODE gulp per plane). The DMA sequencer itself starts at
        // the revision-masked DDFSTRT comparator value, but the shifter
        // consumes data in whole 1/2/4-word gulps, so a DDFSTRT moved within
        // one gulp changes how much tail data is fetched without necessarily
        // moving the visible picture. With
        // FMODE=0 the gulp equals the DDF granularity and nothing changes
        // (boot-screen insert-disk art is drawn for the continuous placement:
        // its negative modulos overlap rows so the hand/disk's right edge
        // lives in the next row's first bytes - the calibrated FMODE=0 anchors
        // must stay). With wide FMODE fetches system software programs DDFSTRT
        // $38 or $3C interchangeably (same 16-cck gulp slot, BPLCON1=0), and
        // its interleaved-bitmap modulos expect exactly the visible row width
        // in the window - without the placement quantization, the fetch overrun
        // displayed inside the window's right edge as the next plane's row
        // start. The placement grid anchor is the colour-clock origin, not the
        // hard DDF start $18.
        let align = |hpos: i32| -> i32 {
            let gulp = self.fetch_period() as i32;
            // Clamped to the DDF hard start: placement before the first usable
            // fetch position is not visible.
            (hpos.div_euclid(gulp) * gulp).max(BITPLANE_DDF_HARD_START as i32)
        };
        let ddf_native_shift = (align(effective_ddf_start_hpos(
            self.agnus_revision,
            self.hires() || self.shres(),
            self.ddfstrt,
        ) as i32)
            - align(standard_ddf))
            * ddf_native_scale;
        // The render loop measures output pixels from the CLAMPED display
        // window start: the framebuffer cannot show anything left of
        // DIW_HSTART_FB0, so a window programmed further left (extreme
        // left-overscan DIWSTRT) has its off-screen
        // part clipped. The fetched content's position is fixed by DDFSTRT on
        // the beam, not by the window, so the clipped-away window pixels must
        // not push the content to the right. Without this correction the
        // content shifted right by (DIW_HSTART_FB0 - diw_h_start) lores pixels
        // and lost its right edge off the framebuffer.
        let clamped_window_native =
            ((DIW_HSTART_FB0 - diw_h_start as i32).max(0) * 2) / pixel_repeat as i32;
        let mut origin_shift = display_native_shift - ddf_native_shift + clamped_window_native;
        let ddf_start = effective_ddf_start_hpos(
            self.agnus_revision,
            self.hires() || self.shres(),
            self.ddfstrt,
        );
        if !self.hires() && !self.shres() && self.fetch_quantum() == 1 {
            let ddf = i32::from(ddf_start);
            if ddf < standard_ddf && origin_shift > 0 {
                // Single-word lo-res fetches that start before the standard $38
                // slot expose whole 16-pixel groups. The standard one-sample
                // lo-res phase bias must not push a standard-width DIW one sample
                // past that completed early-DDF row at the right edge.
                origin_shift -= 1;
            }
        }
        origin_shift
    }
}

#[derive(Clone, Copy)]
struct ControlSegment {
    x: usize,
    control: ControlState,
}

#[derive(Clone, Copy)]
struct ManualBplSegment {
    line: usize,
    hpos: u32,
    x: i32,
    planes: [u16; 8],
    palette: Palette,
}

#[derive(Clone, Copy)]
struct SpriteLine {
    hstart: i32,
    hsub_70ns: bool,
    beam_y: i32,
    data: u16,
    datb: u16,
    /// AGA FMODE wide-fetch words beyond the first (SPR32/SPAGEM).
    data_ext: [u16; 3],
    datb_ext: [u16; 3],
    /// Words per channel: 1 (16 px), 2 (32 px), or 4 (64 px).
    width_words: u8,
    attached: bool,
    x_start: usize,
    x_stop: usize,
}

impl SpriteLine {
    fn width_words(&self) -> usize {
        (self.width_words as usize).max(1)
    }

    fn word(&self, w: usize) -> (u16, u16) {
        if w == 0 {
            (self.data, self.datb)
        } else {
            (self.data_ext[w - 1], self.datb_ext[w - 1])
        }
    }
}

const SPRITE_LINE_MAX_BITS: usize = 64;

struct SpriteLineSampler<'a> {
    line: &'a SpriteLine,
    bit_stops: [i32; SPRITE_LINE_MAX_BITS + 1],
    bit_values: [u8; SPRITE_LINE_MAX_BITS],
    bit_count: usize,
}

impl<'a> SpriteLineSampler<'a> {
    fn new(
        line: &'a SpriteLine,
        base_control: ControlState,
        control_segments: &[ControlSegment],
    ) -> Self {
        let base_x =
            sprite_base_framebuffer_x(line.hstart, line.hsub_70ns, base_control, control_segments);
        let mut bit_stops = [0i32; SPRITE_LINE_MAX_BITS + 1];
        let mut bit_values = [0u8; SPRITE_LINE_MAX_BITS];
        let mut bit_count = 0usize;
        let mut x_cursor = base_x;
        bit_stops[0] = base_x;

        for w in 0..line.width_words() {
            let (data, datb) = line.word(w);
            for bit in (0..16).rev() {
                let sample_x = x_cursor.clamp(0, FB_WIDTH.saturating_sub(1) as i32) as usize;
                let sprite_pixel_repeat =
                    control_at_x(base_control, control_segments, sample_x).sprite_pixel_repeat();
                let lo = u8::from(data & (1 << bit) != 0);
                let hi = u8::from(datb & (1 << bit) != 0);
                bit_values[bit_count] = lo | (hi << 1);
                x_cursor += sprite_pixel_repeat;
                bit_count += 1;
                bit_stops[bit_count] = x_cursor;
            }
        }

        Self {
            line,
            bit_stops,
            bit_values,
            bit_count,
        }
    }

    fn framebuffer_range(&self) -> Option<(i32, i32)> {
        let start = self.bit_stops[0].max(self.line.x_start as i32).max(0);
        let stop = self.bit_stops[self.bit_count]
            .min(self.line.x_stop as i32)
            .min(FB_WIDTH as i32);
        (start < stop).then_some((start, stop))
    }

    fn pixel_bits_at(&self, x: i32) -> u8 {
        if x < self.line.x_start as i32
            || x >= self.line.x_stop as i32
            || x < self.bit_stops[0]
            || x >= self.bit_stops[self.bit_count]
        {
            return 0;
        }
        let bit_idx = self.bit_stops[1..=self.bit_count].partition_point(|stop| *stop <= x);
        self.bit_values[bit_idx]
    }
}

/// Sprite colour entry in the palette store. AGA bases the lookup on the
/// BPLCON4 ESPRM low nibble (even sprites / attached pairs) or OSPRM high
/// nibble (odd sprites); pre-AGA uses the classic 16..31 block. Attached pairs
/// use the 4-bit pixel index directly; unattached sprites add the pair's
/// 4-colour offset.
fn sprite_color_entry(control: ControlState, sprite: usize, idx: u8, attached: bool) -> usize {
    let offset = if attached {
        idx as usize
    } else {
        (sprite / 2) * 4 + idx as usize
    };
    if control.aga() {
        let nibble = if attached || sprite & 1 == 0 {
            control.bplcon4 & 0x0F
        } else {
            (control.bplcon4 >> 4) & 0x0F
        } as usize;
        (nibble << 4) + offset
    } else {
        16 + offset
    }
}

#[derive(Clone, Copy)]
struct SpriteClip {
    x_start: usize,
    x_stop: usize,
    y_start: usize,
    y_stop: usize,
}

// Scaffolding for the renderer's sprite unit-test helpers. Sprite DMA is now
// sourced from captured DMA lines (see bus.rs), so the renderer no longer reads
// pointer-refresh state; the helpers keep this shape only to drive tests.
#[cfg(test)]
#[derive(Clone, Copy, Default)]
#[allow(dead_code)]
struct SpritePointerRefresh {
    refreshed: bool,
    ptr: u32,
    beam: Option<(u32, u32)>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DeniseBitplaneSample {
    idx: u8,
    nplanes: usize,
    active: bool,
}

struct DenisePlannedPlayfieldLine<'a> {
    y: usize,
    x_start: usize,
    x_stop: usize,
    plane_words: &'a [Vec<u16>],
    fetched_pixels: usize,
    carry_words: [Option<u16>; 8],
    first_word_plane_start_x: [usize; 8],
}

impl<'a> DenisePlannedPlayfieldLine<'a> {
    fn new(
        y: usize,
        x_start: usize,
        x_stop: usize,
        plane_words: &'a [Vec<u16>],
        fetched_pixels: usize,
    ) -> Self {
        Self {
            y,
            x_start,
            x_stop,
            plane_words,
            fetched_pixels,
            carry_words: [None; 8],
            first_word_plane_start_x: [0; 8],
        }
    }

    fn with_carry_words(mut self, carry_words: [Option<u16>; 8]) -> Self {
        self.carry_words = carry_words;
        self
    }

    fn with_first_word_plane_start_x(mut self, start_x: [usize; 8]) -> Self {
        self.first_word_plane_start_x = start_x;
        self
    }

    #[cfg(test)]
    fn sample(&self, control: ControlState, native_x: usize) -> DeniseBitplaneSample {
        let nplanes = control.nplanes().min(self.plane_words.len());
        let delays = std::array::from_fn(|plane| control.scroll_for_plane(plane));
        self.sample_prepared(nplanes, &delays, 0, native_x)
    }

    /// `sample` with the control-derived inputs hoisted out: the playfield
    /// pixel loop runs this per output pixel, so the plane count and the
    /// per-plane scroll delays are computed once per control run instead.
    ///
    /// `min_fetch_x` is the first fetched-pixel index that the display window
    /// actually shows (the renderer's `native_x_offset`). When the display
    /// window opens to the right of the DDF-derived fetch origin
    /// (`native_x_offset > 0`, e.g. a narrow DIWSTRT), the bitplane shifter has
    /// already clocked those leading pixels out into the left border by the
    /// time the window opens. BPLCON1 scroll must not pull that shifted-out
    /// pre-fetch back into view: the scrolled-in region at the window's left
    /// edge is background, matching the standard `native_x_offset == 0` case
    /// where `native_x < delay` already yields background. (Kickstart 3.1's
    /// insert-disk screen leaves an uninitialised word at the bitplane base,
    /// which would otherwise scroll a stray fleck into the top-left corner.)
    fn sample_prepared(
        &self,
        nplanes: usize,
        delays: &[usize; 8],
        min_fetch_x: usize,
        native_x: usize,
    ) -> DeniseBitplaneSample {
        let mut idx = 0u8;
        let mut active = false;
        for (plane, words) in self.plane_words.iter().enumerate().take(nplanes) {
            let delay = delays[plane];
            if native_x < delay {
                active = true;
                if delay <= 16 {
                    let carry_offset = delay - native_x;
                    if let Some(word) = self.carry_words.get(plane).copied().flatten() {
                        let bit = carry_offset - 1;
                        if word & (1 << bit) != 0 {
                            idx |= 1 << plane;
                        }
                    }
                }
                continue;
            }
            let fetch_x = native_x - delay;
            if fetch_x < min_fetch_x {
                active = true;
                continue;
            }
            if fetch_x < 16 && fetch_x < self.first_word_plane_start_x[plane] {
                active = true;
                let carry_offset = self.first_word_plane_start_x[plane] - fetch_x;
                if carry_offset <= 16 {
                    let bit = carry_offset - 1;
                    if self.carry_words[plane].is_some_and(|word| word & (1 << bit) != 0) {
                        idx |= 1 << plane;
                    }
                }
                continue;
            }
            if fetch_x >= self.fetched_pixels {
                continue;
            }
            active = true;
            let word = words[fetch_x / 16];
            let bit = 15 - (fetch_x & 0x0F);
            if word & (1 << bit) != 0 {
                idx |= 1 << plane;
            }
        }
        DeniseBitplaneSample {
            idx,
            nplanes,
            active,
        }
    }
}

#[derive(Clone, Copy)]
struct DeniseManualBitplaneShifter {
    planes: [u16; 8],
    word_bits: usize,
}

impl DeniseManualBitplaneShifter {
    fn new(planes: [u16; 8], word_bits: usize) -> Self {
        Self { planes, word_bits }
    }

    fn sample(&self, control: ControlState, native_idx: usize) -> Option<DeniseBitplaneSample> {
        let nplanes = control.nplanes().min(self.planes.len());
        let mut idx = 0u8;
        let mut word_active = false;
        for plane in 0..nplanes {
            let delay = control.scroll_for_plane(plane);
            if native_idx < delay {
                word_active = true;
                continue;
            }
            let source_bit = native_idx - delay;
            if source_bit >= self.word_bits {
                continue;
            }
            word_active = true;
            let bit = 15 - source_bit;
            if self.planes[plane] & (1 << bit) != 0 {
                idx |= 1 << plane;
            }
        }
        word_active.then_some(DeniseBitplaneSample {
            idx,
            nplanes,
            active: true,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DenisePlayfieldOutput {
    /// 24-bit 0x00RRGGBB. OCS/ECS resolution keeps its exact 12-bit maths
    /// and expands by nibble duplication at this boundary; the AGA path
    /// resolves natively in 24-bit.
    color: u32,
    color_latch: u16,
    pf_mask: u8,
}

#[cfg_attr(
    not(any(test, debug_assertions, feature = "display-plan-trace")),
    allow(dead_code)
)]
#[derive(Clone, Debug, PartialEq, Eq)]
enum DisplayLinePlanEvent {
    BitplaneDmaFetch {
        hpos: u32,
        word_idx: usize,
        plane: usize,
        word: u16,
    },
    LatchedBitplaneWord {
        hpos: u32,
        word_idx: usize,
        plane: usize,
        word: u16,
    },
    BpldatWrite {
        hpos: u32,
        x: i32,
        plane: usize,
        value: u16,
    },
    ControlChange {
        hpos: u32,
        x: usize,
        control: ControlState,
    },
    PaletteChange {
        hpos: u32,
        x: usize,
        palette: Box<Palette>,
    },
    SpriteSlot {
        hpos: u32,
        sprite: usize,
        hstart: i32,
        data: u16,
        datb: u16,
        attached: bool,
    },
}

#[cfg_attr(
    not(any(test, debug_assertions, feature = "display-plan-trace")),
    allow(dead_code)
)]
impl DisplayLinePlanEvent {
    fn hpos(&self) -> u32 {
        match self {
            Self::BitplaneDmaFetch { hpos, .. }
            | Self::LatchedBitplaneWord { hpos, .. }
            | Self::BpldatWrite { hpos, .. }
            | Self::ControlChange { hpos, .. }
            | Self::PaletteChange { hpos, .. }
            | Self::SpriteSlot { hpos, .. } => *hpos,
        }
    }

    fn beam_order(&self) -> u8 {
        match self {
            Self::SpriteSlot { .. } => 0,
            Self::ControlChange { .. } => 1,
            Self::PaletteChange { .. } => 2,
            Self::BpldatWrite { .. } => 3,
            Self::BitplaneDmaFetch { .. } => 4,
            Self::LatchedBitplaneWord { .. } => 5,
        }
    }
}

// In-progress display-plan trace machinery; not every field has a consumer
// yet.
#[allow(dead_code)]
struct DisplayLinePlan<'a> {
    line: usize,
    beam_y: u32,
    x_start: usize,
    x_stop: usize,
    nplanes: usize,
    dma_planes: usize,
    words_per_row: usize,
    fetch_plans: &'a [LineFetchPlan],
    row_words: &'a [Vec<u16>; 8],
    register_events: &'a [DisplayLinePlanEvent],
    captured_sprite_lines: &'a [CapturedSpriteLine],
    fallback_control: ControlState,
}

#[cfg_attr(
    not(any(test, debug_assertions, feature = "display-plan-trace")),
    allow(dead_code)
)]
impl<'a> DisplayLinePlan<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        line: usize,
        beam_y: u32,
        x_start: usize,
        x_stop: usize,
        nplanes: usize,
        dma_planes: usize,
        words_per_row: usize,
        fetch_plans: &'a [LineFetchPlan],
        row_words: &'a [Vec<u16>; 8],
        register_events: &'a [DisplayLinePlanEvent],
        captured_sprite_lines: &'a [CapturedSpriteLine],
        fallback_control: ControlState,
    ) -> Self {
        Self {
            line,
            beam_y,
            x_start,
            x_stop,
            nplanes,
            dma_planes,
            words_per_row,
            fetch_plans,
            row_words,
            register_events,
            captured_sprite_lines,
            fallback_control,
        }
    }

    fn collect_events(&self) -> Vec<DisplayLinePlanEvent> {
        let mut events = Vec::new();
        for (word_idx, fetch_plan) in self.fetch_plans.iter().enumerate().take(self.words_per_row) {
            let mut recorded_dma_planes = [false; 6];
            for (hpos, plane) in fetch_plan.iter() {
                if plane < self.nplanes
                    && plane < self.row_words.len()
                    && word_idx < self.row_words[plane].len()
                {
                    recorded_dma_planes[plane] = true;
                    events.push(DisplayLinePlanEvent::BitplaneDmaFetch {
                        hpos,
                        word_idx,
                        plane,
                        word: self.row_words[plane][word_idx],
                    });
                }
            }
            for plane in 0..self.dma_planes.min(self.nplanes).min(self.row_words.len()) {
                if !recorded_dma_planes[plane] && word_idx < self.row_words[plane].len() {
                    events.push(DisplayLinePlanEvent::BitplaneDmaFetch {
                        hpos: fetch_plan.word_fetch_hpos.unwrap_or_else(|| {
                            bitplane_fetch_hpos_for_plane(self.fallback_control, word_idx, plane)
                        }),
                        word_idx,
                        plane,
                        word: self.row_words[plane][word_idx],
                    });
                }
            }
            let latched_hpos = fetch_plan
                .latched_plane_sample_hpos()
                .unwrap_or_else(|| bitplane_fetch_hpos(self.fallback_control, word_idx));
            for plane in self.dma_planes..self.nplanes.min(self.row_words.len()) {
                if word_idx < self.row_words[plane].len() {
                    events.push(DisplayLinePlanEvent::LatchedBitplaneWord {
                        hpos: latched_hpos,
                        word_idx,
                        plane,
                        word: self.row_words[plane][word_idx],
                    });
                }
            }
        }
        events.extend_from_slice(self.register_events);
        for line in self.captured_sprite_lines {
            if line.beam_y != self.beam_y as i32 || line.sprite >= 8 {
                continue;
            }
            events.push(DisplayLinePlanEvent::SpriteSlot {
                hpos: SPRITE_DMA_PAIR_CAPTURE_HPOS[line.sprite / 2],
                sprite: line.sprite,
                hstart: line.hstart,
                data: line.data,
                datb: line.datb,
                attached: line.attached,
            });
        }
        events.sort_by_key(|event| (event.hpos(), event.beam_order()));
        events
    }
}

#[cfg_attr(
    not(any(test, debug_assertions, feature = "display-plan-trace")),
    allow(dead_code)
)]
struct DisplayFramePlan {
    register_events_by_line: Vec<Vec<DisplayLinePlanEvent>>,
    line_events: Vec<Vec<DisplayLinePlanEvent>>,
    recorded_lines: [bool; MAX_VISIBLE_LINES],
}

#[cfg_attr(
    not(any(test, debug_assertions, feature = "display-plan-trace")),
    allow(dead_code)
)]
impl DisplayFramePlan {
    fn new() -> Self {
        Self {
            register_events_by_line: vec![Vec::new(); MAX_VISIBLE_LINES],
            line_events: vec![Vec::new(); MAX_VISIBLE_LINES],
            recorded_lines: [false; MAX_VISIBLE_LINES],
        }
    }

    fn register_events_mut(&mut self) -> &mut [Vec<DisplayLinePlanEvent>] {
        &mut self.register_events_by_line
    }

    #[allow(clippy::too_many_arguments)]
    fn record_line(
        &mut self,
        line: usize,
        beam_y: u32,
        x_start: usize,
        x_stop: usize,
        nplanes: usize,
        dma_planes: usize,
        words_per_row: usize,
        fetch_plans: &[LineFetchPlan],
        row_words: &[Vec<u16>; 8],
        captured_sprite_lines: &[CapturedSpriteLine],
        fallback_control: ControlState,
    ) {
        if line >= self.line_events.len() {
            return;
        }
        let events = {
            let plan = DisplayLinePlan::new(
                line,
                beam_y,
                x_start,
                x_stop,
                nplanes,
                dma_planes,
                words_per_row,
                fetch_plans,
                row_words,
                &self.register_events_by_line[line],
                captured_sprite_lines,
                fallback_control,
            );
            plan.collect_events()
        };
        self.line_events[line] = events;
        self.recorded_lines[line] = true;
    }

    fn finish_register_and_sprite_only_lines(
        &mut self,
        captured_sprite_lines: &[CapturedSpriteLine],
        visible_line0: i32,
    ) {
        for line in 0..self.recorded_lines.len() {
            if self.recorded_lines[line] {
                continue;
            }
            self.line_events[line].extend_from_slice(&self.register_events_by_line[line]);
            let beam_y = visible_line0 + line as i32;
            for sprite_line in captured_sprite_lines {
                if sprite_line.beam_y != beam_y || sprite_line.sprite >= 8 {
                    continue;
                }
                self.line_events[line].push(DisplayLinePlanEvent::SpriteSlot {
                    hpos: SPRITE_DMA_PAIR_CAPTURE_HPOS[sprite_line.sprite / 2],
                    sprite: sprite_line.sprite,
                    hstart: sprite_line.hstart,
                    data: sprite_line.data,
                    datb: sprite_line.datb,
                    attached: sprite_line.attached,
                });
            }
            self.line_events[line].sort_by_key(|event| (event.hpos(), event.beam_order()));
        }
    }

    fn log_summary(&self) {
        let mut lines = 0usize;
        let mut dma_fetches = 0usize;
        let mut latched_words = 0usize;
        let mut bpldat_writes = 0usize;
        let mut control_changes = 0usize;
        let mut palette_changes = 0usize;
        let mut sprite_slots = 0usize;
        for events in &self.line_events {
            if !events.is_empty() {
                lines += 1;
            }
            for event in events {
                match event {
                    DisplayLinePlanEvent::BitplaneDmaFetch { .. } => dma_fetches += 1,
                    DisplayLinePlanEvent::LatchedBitplaneWord { .. } => latched_words += 1,
                    DisplayLinePlanEvent::BpldatWrite { .. } => bpldat_writes += 1,
                    DisplayLinePlanEvent::ControlChange { .. } => control_changes += 1,
                    DisplayLinePlanEvent::PaletteChange { .. } => palette_changes += 1,
                    DisplayLinePlanEvent::SpriteSlot { .. } => sprite_slots += 1,
                }
            }
        }
        log::info!(
            "display-plan lines={} dma_fetches={} latched_words={} bpldat_writes={} control_changes={} palette_changes={} sprite_slots={}",
            lines,
            dma_fetches,
            latched_words,
            bpldat_writes,
            control_changes,
            palette_changes,
            sprite_slots
        );
    }
}

#[derive(Clone, Copy)]
struct BeamSpriteState {
    sprpos: [u16; 8],
    sprctl: [u16; 8],
    sprdata: [u16; 8],
    sprdatb: [u16; 8],
    spr_armed: [bool; 8],
    direct_data_armed: [bool; 8],
    /// Lisa only: FMODE SPR32/SPAGEM widen manual sprites too. A CPU/Copper
    /// SPRxDATA/SPRxDATB write loads the same 16-bit value into every word
    /// of the wide holding register, so a manual wide sprite repeats its
    /// 16-pixel pattern across the 32/64-pixel window (WinUAE model).
    aga: bool,
    fmode: u16,
    /// Sprites reused with DMA off (SPREN cleared mid-frame): the bus
    /// established the held pixel data off-screen, and the Copper repositions
    /// them via SPRxPOS. When present the sprite is armed and displays this
    /// held data (with its full wide-fetch words, unlike a manual SPRxDATA
    /// write which only replicates one word) at the current SPRxPOS, clipped
    /// per reposition interval. The held state is captured only after sprite
    /// DMA has already made the channel active; once SPREN is off, the DMA
    /// descriptor's later VSTOP no longer clears that latched display data.
    held: [Option<HeldSpriteLine>; 8],
}

impl BeamSpriteState {
    fn from_render_state(state: &RenderState, held: &[Option<HeldSpriteLine>; 8]) -> Self {
        let mut sprpos = state.sprpos;
        let mut sprctl = state.sprctl;
        let mut spr_armed = state.spr_armed;
        for (i, h) in held.iter().enumerate() {
            if let Some(held) = h {
                let (pos, ctl) = sprite_control_words_from_parts(
                    held.vstart,
                    held.vstop,
                    held.line.hstart,
                    held.line.hsub_70ns,
                    held.line.attached,
                );
                sprpos[i] = pos;
                sprctl[i] = ctl;
                spr_armed[i] = true;
            }
        }
        Self {
            sprpos,
            sprctl,
            sprdata: state.sprdata,
            sprdatb: state.sprdatb,
            spr_armed,
            direct_data_armed: [false; 8],
            aga: matches!(state.agnus_revision, AgnusRevision::AgaAlice),
            fmode: state.fmode,
            held: *held,
        }
    }

    fn apply_write(&mut self, off: u16, val: u16) {
        if off == 0x1FC {
            if self.aga {
                self.fmode = val & 0xC00F;
            }
            return;
        }
        let idx = ((off - 0x140) / 8) as usize;
        if idx >= 8 {
            return;
        }
        match (off - 0x140) & 0x0006 {
            0x0 => self.sprpos[idx] = val,
            0x2 => {
                self.sprctl[idx] = val;
                self.spr_armed[idx] = false;
                self.direct_data_armed[idx] = false;
            }
            0x4 => {
                self.sprdata[idx] = val;
                self.spr_armed[idx] = true;
                self.direct_data_armed[idx] = true;
            }
            0x6 => self.sprdatb[idx] = val,
            _ => {}
        }
    }

    fn line_for_sprite(
        &self,
        sprite: usize,
        beam_y: i32,
        x_start: usize,
        x_stop: usize,
    ) -> Option<SpriteLine> {
        if x_start >= x_stop || !self.spr_armed[sprite] {
            return None;
        }
        let pos = self.sprpos[sprite];
        let ctl = self.sprctl[sprite];
        let held = self.held[sprite];
        let hstart = sprite_hstart(pos, ctl);
        let hsub_70ns = sprite_hsub_70ns(ctl);
        let base_x = sprite_nominal_base_framebuffer_x(pos, ctl);
        // A held sprite was already active when SPREN was cleared. With no
        // sprite DMA slot running, the DMA descriptor's stop comparator cannot
        // retire the latched data; later SPRxPOS writes simply reposition it.
        if let Some(held) = held {
            return Some(SpriteLine {
                hstart,
                hsub_70ns,
                beam_y,
                data: held.line.data,
                datb: held.line.datb,
                data_ext: held.line.data_ext,
                datb_ext: held.line.datb_ext,
                width_words: held.line.width_words,
                attached: ctl & 0x0080 != 0,
                x_start,
                x_stop,
            });
        }
        // SPRxDATA/SPRxDATB writes update Denise's data latches, but the
        // serializer only copies those latches when the horizontal sprite
        // comparator fires. A write after that compare is for a later compare,
        // not the remaining pixels of the current word.
        if x_start as i32 > base_x {
            return None;
        }
        if !self.direct_data_armed[sprite] {
            let vstart = sprite_vstart(pos, ctl);
            let vstop = sprite_vstop(ctl);
            // Normal pair: [vstart, vstop). Equal start/stop is an empty window;
            // only a strictly inverted pair wraps through the frame boundary.
            let in_window = if vstop == vstart {
                false
            } else if vstop > vstart {
                beam_y >= vstart && beam_y < vstop
            } else {
                beam_y >= vstart || beam_y < vstop
            };
            if !in_window {
                return None;
            }
        }
        let width_words = if self.aga {
            sprite_width_words_from_fmode(self.fmode)
        } else {
            1
        };
        let data = self.sprdata[sprite];
        let datb = self.sprdatb[sprite];
        let (data_ext, datb_ext) = if width_words > 1 {
            ([data; 3], [datb; 3])
        } else {
            ([0; 3], [0; 3])
        };
        Some(SpriteLine {
            hstart,
            hsub_70ns,
            beam_y,
            data,
            datb,
            data_ext,
            datb_ext,
            width_words,
            attached: ctl & 0x0080 != 0,
            x_start,
            x_stop,
        })
    }
}

/// FMODE SPR32/SPAGEM (bits 2-3): 16-bit words per sprite channel, i.e. the
/// sprite output width in words (16/32/64 pixels).
fn sprite_width_words_from_fmode(fmode: u16) -> u8 {
    match (fmode >> 2) & 0x0003 {
        0 => 1,
        3 => 4,
        _ => 2,
    }
}

/// Snapshot of all chipset register values relevant to rendering this
/// frame. Initialized from direct register writes (Denise/Agnus state)
/// and then optionally overridden by recorded beam events.
struct RenderState {
    agnus_revision: AgnusRevision,
    harddis: bool,
    dmacon: u16,
    bplcon0: u16,
    bplcon1: u16,
    bplcon2: u16,
    bplcon3: u16,
    bplcon4: u16,
    fmode: u16,
    clxcon: u16,
    clxcon2: u16,
    bplpt: [u32; 8],
    bpldat: [u16; 8],
    sprpt: [u32; 8],
    sprpos: [u16; 8],
    sprctl: [u16; 8],
    sprdata: [u16; 8],
    sprdatb: [u16; 8],
    spr_armed: [bool; 8],
    bpl1mod: i16,
    bpl2mod: i16,
    palette: Palette,
    diwstrt: u16,
    diwstop: u16,
    diwhigh: DiwHigh,
    ddfstrt: u16,
    ddfstop: u16,
}

impl RenderState {
    fn from_snapshot(snapshot: RenderRegisterSnapshot) -> Self {
        Self {
            agnus_revision: snapshot.agnus_revision,
            harddis: snapshot.harddis,
            dmacon: snapshot.dmacon,
            bplcon0: snapshot.bplcon0,
            bplcon1: snapshot.bplcon1,
            bplcon2: snapshot.bplcon2,
            bplcon3: snapshot.bplcon3,
            bplcon4: snapshot.bplcon4,
            fmode: snapshot.fmode,
            clxcon: snapshot.clxcon,
            clxcon2: snapshot.clxcon2,
            bplpt: snapshot.bplpt,
            bpldat: snapshot.bpldat,
            sprpt: snapshot.sprpt,
            sprpos: snapshot.sprpos,
            sprctl: snapshot.sprctl,
            sprdata: snapshot.sprdata,
            sprdatb: snapshot.sprdatb,
            spr_armed: snapshot.spr_armed,
            bpl1mod: snapshot.bpl1mod,
            bpl2mod: snapshot.bpl2mod,
            palette: snapshot.palette,
            diwstrt: snapshot.diwstrt,
            diwstop: snapshot.diwstop,
            diwhigh: snapshot.diwhigh,
            ddfstrt: snapshot.ddfstrt,
            ddfstop: snapshot.ddfstop,
        }
    }

    #[cfg(test)]
    fn display_window_y(&self) -> (usize, usize) {
        if display_window_unprogrammed(self.diwstrt, self.diwstop) {
            return (0, FB_HEIGHT);
        }
        let start = self.diw_v_start() as i32;
        let mut stop = self.diw_v_stop() as i32;
        if stop <= start {
            stop += 0x100;
        }
        let top = (start - PAL_VISIBLE_LINE0).max(0) as usize;
        let bottom = (stop - PAL_VISIBLE_LINE0).max(top as i32) as usize;
        (top.min(FB_HEIGHT), bottom.min(FB_HEIGHT))
    }

    #[cfg(test)]
    fn clipped_display_rows_before_frame(&self) -> usize {
        if display_window_unprogrammed(self.diwstrt, self.diwstop) {
            return 0;
        }
        (PAL_VISIBLE_LINE0 - self.diw_v_start() as i32).max(0) as usize
    }

    #[cfg(test)]
    fn display_window_x(&self) -> (usize, usize) {
        if display_window_unprogrammed(self.diwstrt, self.diwstop) {
            return (0, FB_WIDTH);
        }
        let start = self.diw_h_start() as i32;
        let mut stop = self.diw_h_stop() as i32;
        if stop <= start {
            stop += 0x100;
        }
        let left = ((start - DIW_HSTART_FB0).max(0) as usize * 2).min(FB_WIDTH);
        let mut right = ((stop - DIW_HSTART_FB0).max(0) as usize * 2).min(FB_WIDTH);
        if FB_WIDTH.saturating_sub(right) <= 2 {
            right = FB_WIDTH;
        }
        if right > left {
            (left, right)
        } else {
            (left, FB_WIDTH)
        }
    }

    #[cfg(test)]
    fn clipped_display_pixels_before_frame(&self) -> usize {
        if display_window_unprogrammed(self.diwstrt, self.diwstop) {
            return 0;
        }
        ((DIW_HSTART_FB0 - self.diw_h_start() as i32).max(0) as usize * 2).min(FB_WIDTH)
    }

    #[cfg(test)]
    fn diw_v_start(&self) -> u16 {
        self.diwhigh.v_start(self.diwstrt)
    }

    #[cfg(test)]
    fn diw_v_stop(&self) -> u16 {
        self.diwhigh.v_stop(self.diwstop)
    }

    #[cfg(test)]
    fn diw_h_start(&self) -> u16 {
        self.diwhigh.h_start(self.diwstrt)
    }

    #[cfg(test)]
    fn diw_h_stop(&self) -> u16 {
        self.diwhigh.h_stop(self.diwstop)
    }

    #[cfg(test)]
    fn words_per_row(&self, hires: bool, native_w: usize) -> usize {
        let mut control = ControlState::from_render_state(self);
        if hires {
            control.bplcon0 |= 0x8000;
        } else {
            control.bplcon0 &= !0x8000;
        }
        control.words_per_row(native_w)
    }

    #[cfg(test)]
    fn fetch_origin_native_offset(&self, hires: bool, pixel_repeat: usize) -> usize {
        self.fetch_origin_native_shift(hires, pixel_repeat).max(0) as usize
    }

    #[cfg(test)]
    fn fetch_start_native_x(&self, hires: bool, pixel_repeat: usize) -> usize {
        (-self.fetch_origin_native_shift(hires, pixel_repeat)).max(0) as usize
    }

    #[cfg(test)]
    fn fetch_origin_native_shift(&self, hires: bool, pixel_repeat: usize) -> i32 {
        let mut control = ControlState::from_render_state(self);
        if hires {
            control.bplcon0 |= 0x8000;
        } else {
            control.bplcon0 &= !0x8000;
        }
        control.fetch_origin_native_shift(self.diw_h_start(), pixel_repeat)
    }

    #[cfg(test)]
    fn native_x_offset(&self, hires: bool, pixel_repeat: usize) -> usize {
        self.fetch_origin_native_offset(hires, pixel_repeat)
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn apply_render_events(
    state: &mut RenderState,
    events: &[BeamRegisterWrite],
    base_palettes: &mut [Palette],
    palette_segments: &mut [Vec<PaletteSegment>],
    base_controls: &mut [ControlState],
    control_segments: &mut [Vec<ControlSegment>],
    manual_bpl_segments: &mut Vec<ManualBplSegment>,
) {
    apply_render_events_with_visible_line0(
        state,
        events,
        base_palettes,
        palette_segments,
        base_controls,
        control_segments,
        manual_bpl_segments,
        PAL_VISIBLE_LINE0,
    );
}

// Render-event path for builds without display-plan tracing; trace-capable
// builds call the _and_collect_display_plan_events variant directly.
#[cfg_attr(
    any(test, debug_assertions, feature = "display-plan-trace"),
    allow(dead_code)
)]
fn apply_render_events_with_visible_line0(
    state: &mut RenderState,
    events: &[BeamRegisterWrite],
    base_palettes: &mut [Palette],
    palette_segments: &mut [Vec<PaletteSegment>],
    base_controls: &mut [ControlState],
    control_segments: &mut [Vec<ControlSegment>],
    manual_bpl_segments: &mut Vec<ManualBplSegment>,
    visible_line0: i32,
) {
    apply_render_events_and_collect_display_plan_events_with_visible_line0(
        state,
        events,
        base_palettes,
        palette_segments,
        base_controls,
        control_segments,
        manual_bpl_segments,
        visible_line0,
        None,
    );
}

#[allow(clippy::too_many_arguments)]
#[cfg_attr(not(test), allow(dead_code))]
fn apply_render_events_and_collect_display_plan_events(
    state: &mut RenderState,
    events: &[BeamRegisterWrite],
    base_palettes: &mut [Palette],
    palette_segments: &mut [Vec<PaletteSegment>],
    base_controls: &mut [ControlState],
    control_segments: &mut [Vec<ControlSegment>],
    manual_bpl_segments: &mut Vec<ManualBplSegment>,
    display_line_events: Option<&mut [Vec<DisplayLinePlanEvent>]>,
) {
    apply_render_events_and_collect_display_plan_events_with_visible_line0(
        state,
        events,
        base_palettes,
        palette_segments,
        base_controls,
        control_segments,
        manual_bpl_segments,
        PAL_VISIBLE_LINE0,
        display_line_events,
    );
}

#[allow(clippy::too_many_arguments)]
fn apply_render_events_and_collect_display_plan_events_with_visible_line0(
    state: &mut RenderState,
    events: &[BeamRegisterWrite],
    base_palettes: &mut [Palette],
    palette_segments: &mut [Vec<PaletteSegment>],
    base_controls: &mut [ControlState],
    control_segments: &mut [Vec<ControlSegment>],
    manual_bpl_segments: &mut Vec<ManualBplSegment>,
    visible_line0: i32,
    mut display_line_events: Option<&mut [Vec<DisplayLinePlanEvent>]>,
) {
    let mut palette = state.palette;
    let mut control = ControlState::from_render_state(state);
    let mut next_base_line = 0usize;
    let mut next_control_line = 0usize;
    let cpu_palette_beam_timed = cpu_palette_writes_are_beam_timed(events, visible_line0);

    if !cpu_palette_beam_timed {
        for event in events {
            let off = event.offset & 0x01FE;
            if matches!(event.source, BeamWriteSource::Cpu) && matches!(off, 0x180..=0x1BE) {
                let idx = ((off - 0x180) / 2) as usize;
                if idx < 32 {
                    let value = color_register_value(event.value);
                    let (entry, loct) = palette_entry_for_write(state.bplcon3, control.aga(), idx);
                    palette.write_entry(usize::from(entry), loct, value);
                    state.palette.write_entry(usize::from(entry), loct, value);
                }
            }
        }
    }

    for event in events {
        let off = event.offset & 0x01FE;
        if matches!(
            event.source,
            BeamWriteSource::Cpu | BeamWriteSource::CpuCopperIrq
        ) && matches!(off, 0x180..=0x1BE)
            && (event.vpos as i32) <= visible_line0
        {
            let idx = ((off - 0x180) / 2) as usize;
            if idx < 32 {
                let value = color_register_value(event.value);
                let (entry, loct) = palette_entry_for_write(state.bplcon3, control.aga(), idx);
                palette.write_entry(usize::from(entry), loct, value);
                state.palette.write_entry(usize::from(entry), loct, value);
            }
            continue;
        }

        let (line, mut beam_x) = beam_to_framebuffer_pos_with_visible_line0(
            event.vpos,
            event.hpos,
            visible_line0,
            base_palettes.len(),
        );
        // An event from a line above the visible area happened before the
        // first framebuffer line started: it contributes to line 0's start
        // state, not a mid-line change at its horizontal position (which
        // would, e.g., split the first display line of a screen whose
        // copper list programs the display on the line before the window
        // opens, as the boot ROM does).
        let before_visible_lines = (event.vpos as i32) < visible_line0;
        if before_visible_lines {
            beam_x = 0;
        }
        fill_base_palettes(base_palettes, &mut next_base_line, line, palette);
        fill_base_controls(base_controls, &mut next_control_line, line, control);

        if matches!(event.source, BeamWriteSource::Cpu)
            && matches!(off, 0x180..=0x1BE)
            && !cpu_palette_beam_timed
        {
            continue;
        }

        if let 0x180..=0x1BE = off {
            let idx = ((off - 0x180) / 2) as usize;
            let (entry, loct) = palette_entry_for_write(control.bplcon3, control.aga(), idx);
            if idx < 32 {
                palette.write_entry(usize::from(entry), loct, color_register_value(event.value));
            }
            let x = if before_visible_lines {
                0
            } else {
                color_write_framebuffer_x(event.hpos)
            };
            if palette_row_diag().is_some_and(|spec| spec.contains(event.vpos)) {
                log::info!(
                    "palrow v={} h={} x={} line={} source={} color{:02} entry={} loct={} value={:#06X} bplcon3={:#06X}",
                    event.vpos,
                    event.hpos,
                    x,
                    line,
                    beam_write_source_label(event.source),
                    idx,
                    entry,
                    loct,
                    color_register_value(event.value),
                    control.bplcon3,
                );
            }
            push_palette_segment(
                palette_segments,
                line,
                x,
                entry,
                loct,
                color_register_value(event.value),
            );
            if let Some(events_by_line) = display_line_events.as_deref_mut() {
                if line < events_by_line.len() {
                    events_by_line[line].push(DisplayLinePlanEvent::PaletteChange {
                        hpos: event.hpos,
                        x,
                        palette: Box::new(palette),
                    });
                }
            }
        }

        let previous_control = control;
        apply_move(state, off, event.value);
        if matches!(
            off,
            0x08E
                | 0x090
                | 0x092
                | 0x094
                | 0x096
                | 0x098
                | 0x100
                | 0x102
                | 0x104
                | 0x106
                | 0x108
                | 0x10A
                | 0x10C
                | 0x10E
                | 0x1E4
                | 0x1FC
        ) {
            let next_control = ControlState::from_render_state(state);
            if off == 0x10C && previous_control.aga() && !before_visible_lines {
                // Lisa applies BPLCON4's sprite palette-base byte earlier
                // than its bitplane XOR byte. Keep BPLAM on the normal
                // control timeline while letting sprite colour lookup see the
                // new ESPRM/OSPRM byte in its earlier sprite-palette domain.
                let sprite_x = sprite_palette_control_framebuffer_x(event.hpos);
                if sprite_x < beam_x {
                    let mut sprite_control = previous_control;
                    sprite_control.bplcon4 =
                        (previous_control.bplcon4 & 0xFF00) | (next_control.bplcon4 & 0x00FF);
                    push_control_segment(
                        control_segments,
                        line,
                        sprite_x,
                        base_controls[line],
                        sprite_control,
                    );
                }
            }
            control = next_control;
            push_control_segment(control_segments, line, beam_x, base_controls[line], control);
            if matches!(off, 0x102 | 0x108 | 0x10A) {
                if let Some(events_by_line) = display_line_events.as_deref_mut() {
                    if line < events_by_line.len() {
                        events_by_line[line].push(DisplayLinePlanEvent::ControlChange {
                            hpos: event.hpos,
                            x: beam_x,
                            control,
                        });
                    }
                }
            }
        }
        if let 0x110..=0x11A = off {
            if let Some(events_by_line) = display_line_events.as_deref_mut() {
                if line < events_by_line.len() {
                    events_by_line[line].push(DisplayLinePlanEvent::BpldatWrite {
                        hpos: event.hpos,
                        x: if before_visible_lines {
                            0
                        } else {
                            beam_to_framebuffer_x_unclamped(event.hpos)
                        },
                        plane: ((off - 0x110) / 2) as usize,
                        value: event.value,
                    });
                }
            }
        }
        let visible_line = event.vpos as i32 - visible_line0;
        if off == 0x110 && (0..base_palettes.len() as i32).contains(&visible_line) {
            manual_bpl_segments.push(ManualBplSegment {
                line,
                hpos: event.hpos,
                x: beam_to_framebuffer_x_unclamped(event.hpos),
                planes: state.bpldat,
                palette,
            });
        }
    }

    fill_base_palettes(
        base_palettes,
        &mut next_base_line,
        base_palettes.len().saturating_sub(1),
        palette,
    );
    fill_base_controls(
        base_controls,
        &mut next_control_line,
        base_controls.len().saturating_sub(1),
        control,
    );
}

fn cpu_palette_writes_are_beam_timed(events: &[BeamRegisterWrite], visible_line0: i32) -> bool {
    events.iter().any(|event| {
        matches!(event.source, BeamWriteSource::Cpu)
            && matches!(event.offset & 0x01FE, 0x180..=0x1BE)
            && (event.vpos as i32) > visible_line0
    })
}

#[cfg_attr(not(test), allow(dead_code))]
fn beam_to_framebuffer_pos(vpos: u32, hpos: u32) -> (usize, usize) {
    beam_to_framebuffer_pos_with_visible_line0(vpos, hpos, PAL_VISIBLE_LINE0, FB_HEIGHT)
}

fn beam_to_framebuffer_pos_with_visible_line0(
    vpos: u32,
    hpos: u32,
    visible_line0: i32,
    rows: usize,
) -> (usize, usize) {
    let line = (vpos as i32 - visible_line0).max(0) as usize;
    let x = beam_to_framebuffer_x_unclamped(hpos).clamp(0, FB_WIDTH as i32) as usize;
    (line.min(rows.saturating_sub(1)), x)
}

fn beam_to_framebuffer_x_unclamped(hpos: u32) -> i32 {
    (hpos as i32 - COPPER_WAIT_HPOS_FB0) * 4
}

fn color_write_framebuffer_x(hpos: u32) -> usize {
    ((hpos as i32 - COLOR_WRITE_HPOS_FB0) * 4).clamp(0, FB_WIDTH as i32) as usize
}

fn sprite_palette_control_framebuffer_x(hpos: u32) -> usize {
    ((hpos as i32 - SPRITE_PALETTE_CONTROL_HPOS_FB0) * 4).clamp(0, FB_WIDTH as i32) as usize
}

fn fill_base_palettes(
    base_palettes: &mut [Palette],
    next_line: &mut usize,
    end_inclusive: usize,
    palette: Palette,
) {
    let end = end_inclusive.saturating_add(1).min(base_palettes.len());
    if end <= *next_line {
        return;
    }
    for dst in &mut base_palettes[*next_line..end] {
        *dst = palette;
    }
    *next_line = end;
}

fn push_palette_segment(
    palette_segments: &mut [Vec<PaletteSegment>],
    line: usize,
    x: usize,
    entry: u8,
    loct: bool,
    value: u16,
) {
    if line >= palette_segments.len() {
        return;
    }
    let x = x.min(FB_WIDTH);
    // A rewrite of the same entry at the same x collapses to the last value;
    // writes to other entries at the same x are kept as separate diffs.
    if let Some(last) = palette_segments[line].last_mut() {
        if last.x == x && last.entry == entry && last.loct == loct {
            last.value = value;
            return;
        }
    }
    palette_segments[line].push(PaletteSegment {
        x,
        entry,
        loct,
        value,
    });
}

fn fill_base_controls(
    base_controls: &mut [ControlState],
    next_line: &mut usize,
    end_inclusive: usize,
    control: ControlState,
) {
    let end = end_inclusive.saturating_add(1).min(base_controls.len());
    if end <= *next_line {
        return;
    }
    for dst in &mut base_controls[*next_line..end] {
        *dst = control;
    }
    *next_line = end;
}

fn push_control_segment(
    control_segments: &mut [Vec<ControlSegment>],
    line: usize,
    x: usize,
    base_control: ControlState,
    control: ControlState,
) {
    if line >= control_segments.len() {
        return;
    }
    let x = x.min(FB_WIDTH);
    if let Some(last) = control_segments[line].last_mut() {
        if last.control == control {
            return;
        }
        if last.x == x {
            last.control = control;
            return;
        }
    } else if base_control == control {
        return;
    }
    control_segments[line].push(ControlSegment { x, control });
}

fn bitplane_scroll_effect_x(segment_x: usize, visible_x_stop: usize) -> usize {
    if segment_x >= visible_x_stop {
        segment_x
    } else {
        segment_x.saturating_sub(BITPLANE_CONTROL_PIPELINE_FB)
    }
}

fn line_control_at_x(
    base_controls: &[ControlState],
    control_segments: &[Vec<ControlSegment>],
    line: usize,
    x: usize,
) -> ControlState {
    let mut control = base_controls[line];
    for seg in &control_segments[line] {
        if seg.x <= x {
            control = seg.control;
        }
    }
    control
}

fn line_words_per_row(base_control: ControlState, control_segments: &[ControlSegment]) -> usize {
    let base_native_w = native_frame_width_for_control(base_control);
    let mut words = if base_control.has_valid_ddf_window() {
        base_control.words_per_row(base_native_w)
    } else {
        0
    };
    for segment in control_segments {
        let segment_native_w = native_frame_width_for_control(segment.control);
        if segment.control.has_valid_ddf_window() {
            words = words.max(segment.control.words_per_row(segment_native_w));
        }
    }
    words.max(1)
}

fn line_has_valid_ddf_window(
    base_control: ControlState,
    control_segments: &[ControlSegment],
) -> bool {
    base_control.has_valid_ddf_window()
        || control_segments
            .iter()
            .any(|segment| segment.control.has_valid_ddf_window())
}

fn line_display_window_bounds(
    base_control: ControlState,
    control_segments: &[ControlSegment],
    line: usize,
    visible_line0: i32,
) -> Option<(usize, usize)> {
    let mut bounds = None;
    let mut control = base_control;
    let mut run_start = 0usize;
    for segment in control_segments {
        let run_stop = segment.x.min(FB_WIDTH);
        merge_display_window_run_bounds(
            &mut bounds,
            control,
            line,
            visible_line0,
            run_start,
            run_stop,
        );
        control = segment.control;
        run_start = run_stop;
    }
    merge_display_window_run_bounds(
        &mut bounds,
        control,
        line,
        visible_line0,
        run_start,
        FB_WIDTH,
    );
    bounds.filter(|(x_start, x_stop)| x_start < x_stop)
}

fn merge_display_window_run_bounds(
    bounds: &mut Option<(usize, usize)>,
    control: ControlState,
    line: usize,
    visible_line0: i32,
    run_start: usize,
    run_stop: usize,
) {
    if run_start >= run_stop || !control.display_window_contains_line(line, visible_line0) {
        return;
    }
    let (window_x_start, window_x_stop) = control.display_window_x();
    if window_x_start >= run_start && window_x_start <= run_stop {
        // The horizontal DIW start comparator has fired while this control was
        // active, so it establishes the shifter origin even if a same-line
        // write clips all visible pixels before the next control run.
        match bounds {
            Some((bounds_start, _)) => {
                *bounds_start = (*bounds_start).min(window_x_start);
            }
            None => *bounds = Some((window_x_start, window_x_start)),
        }
    }
    let x_start = window_x_start.max(run_start);
    let x_stop = window_x_stop.min(run_stop);
    if x_start >= x_stop {
        return;
    }
    match bounds {
        Some((bounds_start, bounds_stop)) => {
            *bounds_start = (*bounds_start).min(x_start);
            *bounds_stop = (*bounds_stop).max(x_stop);
        }
        None => *bounds = Some((x_start, x_stop)),
    }
}

fn line_max_display_planes(
    base_control: ControlState,
    control_segments: &[ControlSegment],
) -> usize {
    control_segments
        .iter()
        .map(|segment| segment.control.nplanes())
        .fold(base_control.nplanes(), usize::max)
}

fn line_max_dma_planes(base_control: ControlState, control_segments: &[ControlSegment]) -> usize {
    control_segments
        .iter()
        .map(|segment| segment.control.dma_planes())
        .fold(base_control.dma_planes(), usize::max)
}

fn fetch_word_index_active_at_hpos(control: ControlState, word_idx: usize, hpos: u32) -> bool {
    if !control.bitplane_dma_enabled() || !control.has_valid_ddf_window() {
        return false;
    }
    let native_w = native_frame_width_for_control(control);
    if word_idx >= control.words_per_row(native_w) {
        return false;
    }
    let Some((start, _stop)) = effective_ddf_window(
        control.agnus_revision,
        control.hires() || control.shres(),
        control.ddfstrt,
        control.ddfstop,
        control.harddis,
    ) else {
        return false;
    };
    let start = u32::from(start);
    if hpos < start {
        return false;
    }
    let rel = hpos - start;
    let step = control.fetch_cck_per_word();
    rel.is_multiple_of(step) && (rel / step) == word_idx as u32
}

fn bitplane_fetch_order(control: ControlState, plane: usize) -> u32 {
    if control.hires() || control.shres() {
        return plane as u32;
    }

    let plane_num = plane + 1;
    OCS_LORES_BPL_SEQUENCE
        .iter()
        .position(|&candidate| candidate == plane_num)
        .unwrap_or(7) as u32
}

fn native_frame_width_for_control(control: ControlState) -> usize {
    if control.shres() {
        FB_WIDTH * 2
    } else if control.hires() {
        FB_WIDTH
    } else {
        FB_WIDTH / 2
    }
}

fn bitplane_fetch_hpos_for_plane(control: ControlState, word_idx: usize, plane: usize) -> u32 {
    let start = u32::from(effective_ddf_start_hpos(
        control.agnus_revision,
        control.hires() || control.shres(),
        control.ddfstrt,
    ));
    let group = word_idx as u32 / control.fetch_quantum();
    if control.hires() || control.shres() {
        return start + group.saturating_mul(control.fetch_period());
    }

    let unit = control.fetch_unit();
    start + group.saturating_mul(unit) + bitplane_fetch_order(control, plane)
}

fn fetch_plane_word_active_at_hpos(
    control: ControlState,
    word_idx: usize,
    plane: usize,
    hpos: u32,
) -> bool {
    if plane >= control.dma_planes().min(8) || !control.bitplane_dma_enabled() {
        return false;
    }
    let native_w = native_frame_width_for_control(control);
    if !control.has_valid_ddf_window() || word_idx >= control.words_per_row(native_w) {
        return false;
    }
    bitplane_fetch_hpos_for_plane(control, word_idx, plane) == hpos
}

#[derive(Clone, Copy)]
struct LineFetchPlan {
    word_fetch_hpos: Option<u32>,
    fetches: [(u32, usize); 8],
    len: usize,
}

impl LineFetchPlan {
    fn empty() -> Self {
        Self {
            word_fetch_hpos: None,
            fetches: [(0, 0); 8],
            len: 0,
        }
    }

    fn push(&mut self, hpos: u32, plane: usize) {
        debug_assert!(self.len < self.fetches.len());
        self.fetches[self.len] = (hpos, plane);
        self.len += 1;
    }

    fn sort_fetches(&mut self) {
        self.fetches[..self.len].sort_unstable();
    }

    fn iter(&self) -> impl Iterator<Item = (u32, usize)> + '_ {
        self.fetches[..self.len].iter().copied()
    }

    fn latched_plane_sample_hpos(&self) -> Option<u32> {
        self.fetches[..self.len]
            .iter()
            .map(|(hpos, _)| *hpos)
            .max()
            .or(self.word_fetch_hpos)
    }
}

fn line_fetch_plan_for_word(
    base_control: ControlState,
    control_segments: &[ControlSegment],
    word_idx: usize,
    dma_planes: usize,
) -> LineFetchPlan {
    let mut plan = LineFetchPlan::empty();
    if control_segments.is_empty() {
        let start = u32::from(effective_ddf_start_hpos(
            base_control.agnus_revision,
            base_control.hires() || base_control.shres(),
            base_control.ddfstrt,
        ));
        let word_group = word_idx as u32 / base_control.fetch_quantum();
        let word_hpos = if base_control.hires() || base_control.shres() {
            start + word_group.saturating_mul(base_control.fetch_period())
        } else {
            start + word_group.saturating_mul(base_control.fetch_unit())
        };
        if (u32::from(BITPLANE_DDF_HARD_START)..=BITPLANE_FETCH_HARD_END).contains(&word_hpos)
            && fetch_word_index_active_at_hpos(base_control, word_idx, word_hpos)
        {
            plan.word_fetch_hpos = Some(word_hpos);
        }
        for plane in 0..dma_planes.min(8) {
            let hpos = bitplane_fetch_hpos_for_plane(base_control, word_idx, plane);
            if (u32::from(BITPLANE_DDF_HARD_START)..=BITPLANE_FETCH_HARD_END).contains(&hpos)
                && fetch_plane_word_active_at_hpos(base_control, word_idx, plane, hpos)
            {
                plan.push(hpos, plane);
            }
        }
        plan.sort_fetches();
        return plan;
    }

    let mut control = base_control;
    let mut segment_idx = 0usize;
    for hpos in u32::from(BITPLANE_DDF_HARD_START)..=BITPLANE_FETCH_HARD_END {
        let x = ((hpos as i32 - COPPER_WAIT_HPOS_FB0).max(0) as usize * 4).min(FB_WIDTH);
        while segment_idx < control_segments.len() && control_segments[segment_idx].x <= x {
            control = control_segments[segment_idx].control;
            segment_idx += 1;
        }
        if plan.word_fetch_hpos.is_none()
            && fetch_word_index_active_at_hpos(control, word_idx, hpos)
        {
            plan.word_fetch_hpos = Some(hpos);
        }
        for plane in 0..dma_planes.min(8) {
            if fetch_plane_word_active_at_hpos(control, word_idx, plane, hpos) {
                plan.push(hpos, plane);
            }
        }
    }
    plan
}

fn line_fetch_plans_for_line(
    base_control: ControlState,
    control_segments: &[ControlSegment],
    words_per_row: usize,
    dma_planes: usize,
) -> Vec<LineFetchPlan> {
    let mut plans = vec![LineFetchPlan::empty(); words_per_row];
    if words_per_row == 0 {
        return plans;
    }
    if control_segments.is_empty() {
        for (word_idx, plan) in plans.iter_mut().enumerate() {
            *plan = line_fetch_plan_for_word(base_control, control_segments, word_idx, dma_planes);
        }
        return plans;
    }

    let mut control = base_control;
    let mut segment_idx = 0usize;
    for hpos in u32::from(BITPLANE_DDF_HARD_START)..=BITPLANE_FETCH_HARD_END {
        let x = ((hpos as i32 - COPPER_WAIT_HPOS_FB0).max(0) as usize * 4).min(FB_WIDTH);
        while segment_idx < control_segments.len() && control_segments[segment_idx].x <= x {
            control = control_segments[segment_idx].control;
            segment_idx += 1;
        }
        if !control.bitplane_dma_enabled() || !control.has_valid_ddf_window() {
            continue;
        }
        let Some((start, _stop)) = effective_ddf_window(
            control.agnus_revision,
            control.hires() || control.shres(),
            control.ddfstrt,
            control.ddfstop,
            control.harddis,
        ) else {
            continue;
        };
        let start = u32::from(start);
        if hpos < start {
            continue;
        }
        let rel = hpos - start;
        let Some(word_idx) = (if control.hires() || control.shres() {
            let step = control.fetch_cck_per_word();
            (rel % step == 0).then_some((rel / step) as usize)
        } else {
            Some((rel / 8) as usize)
        }) else {
            continue;
        };
        if word_idx >= words_per_row {
            continue;
        }
        if plans[word_idx].word_fetch_hpos.is_none()
            && fetch_word_index_active_at_hpos(control, word_idx, hpos)
        {
            plans[word_idx].word_fetch_hpos = Some(hpos);
        }
        for plane in 0..dma_planes.min(8) {
            // A plane fetches a given word once; if it reads as active across
            // more than one colorclock (overlapping DDF segments), keep only
            // the first so the per-word fetch plan never exceeds dma_planes.
            if fetch_plane_word_active_at_hpos(control, word_idx, plane, hpos)
                && !plans[word_idx].iter().any(|(_, p)| p == plane)
            {
                plans[word_idx].push(hpos, plane);
            }
        }
    }
    for plan in &mut plans {
        plan.sort_fetches();
    }
    plans
}

#[cfg(test)]
fn bitplane_output_start_x(
    base_control: ControlState,
    control_segments: &[ControlSegment],
    display_start_x: usize,
    words_per_row: usize,
    dma_planes: usize,
) -> usize {
    bitplane_dma_output_start_x(
        base_control,
        control_segments,
        display_start_x,
        words_per_row,
        dma_planes,
    )
    .unwrap_or(0)
}

fn bitplane_dma_output_start_x(
    base_control: ControlState,
    control_segments: &[ControlSegment],
    display_start_x: usize,
    words_per_row: usize,
    dma_planes: usize,
) -> Option<usize> {
    if dma_planes == 0 || words_per_row == 0 {
        return None;
    }
    let mut display_control = base_control;
    for segment in control_segments {
        if segment.x <= display_start_x {
            display_control = segment.control;
        }
    }
    let pixel_repeat = display_control.framebuffer_pixel_repeat();
    if display_control.fetch_start_native_x(display_control.diw_h_start(), pixel_repeat) == 0 {
        return Some(display_start_x);
    }
    let plan = line_fetch_plan_for_word(base_control, control_segments, 0, dma_planes);
    plan.word_fetch_hpos
        .or_else(|| {
            plan.iter()
                .find_map(|(hpos, plane)| (plane == 0).then_some(hpos))
        })
        .map(bitplane_fetch_framebuffer_x)
}

fn first_word_plane_start_x(
    base_control: ControlState,
    control_segments: &[ControlSegment],
    display_start_x: usize,
    words_per_row: usize,
    dma_planes: usize,
) -> [usize; 8] {
    if dma_planes == 0 || words_per_row == 0 {
        return [0; 8];
    }
    let mut display_control = base_control;
    for segment in control_segments {
        if segment.x <= display_start_x {
            display_control = segment.control;
        }
    }
    let pixel_repeat = display_control.framebuffer_pixel_repeat();
    let native_per_pixel = display_control.native_samples_per_framebuffer_pixel();
    let fetch_start_native_x =
        display_control.fetch_start_native_x(display_control.diw_h_start(), pixel_repeat);
    if fetch_start_native_x == 0 {
        return [0; 8];
    }
    let native_x_offset =
        display_control.native_x_offset(display_control.diw_h_start(), pixel_repeat);
    let framebuffer_x_to_fetch_x = |x: usize| -> usize {
        let output_native_x = x
            .saturating_sub(display_start_x)
            .saturating_div(pixel_repeat)
            .saturating_mul(native_per_pixel);
        output_native_x
            .saturating_sub(fetch_start_native_x)
            .saturating_add(native_x_offset)
    };

    let mut start_x = [0; 8];
    let plan = line_fetch_plan_for_word(base_control, control_segments, 0, dma_planes);
    for (hpos, plane) in plan.iter() {
        if plane < start_x.len() {
            start_x[plane] = framebuffer_x_to_fetch_x(bitplane_fetch_framebuffer_x(hpos));
        }
    }
    start_x
}

fn bitplane_carry_words_for_line(
    block_start: bool,
    display_start_x: usize,
    dma_output_start_x: Option<usize>,
    previous_playfield_tail_words: [Option<u16>; 8],
) -> [Option<u16>; 8] {
    if block_start || dma_output_start_x.is_some_and(|start| start > display_start_x) {
        [None; 8]
    } else {
        previous_playfield_tail_words
    }
}

#[cfg(test)]
fn line_fetch_hpos_for_word(
    base_control: ControlState,
    control_segments: &[ControlSegment],
    word_idx: usize,
) -> Option<u32> {
    line_fetch_plan_for_word(base_control, control_segments, word_idx, 0).word_fetch_hpos
}

fn any_control_matching(
    base_controls: &[ControlState],
    control_segments: &[Vec<ControlSegment>],
    mut predicate: impl FnMut(ControlState) -> bool,
) -> bool {
    base_controls.iter().copied().any(&mut predicate)
        || control_segments
            .iter()
            .flat_map(|segments| segments.iter())
            .any(|segment| predicate(segment.control))
}

fn advance_bitplane_ptrs_for_rows(
    ptrs: &mut [u32; 8],
    rows: usize,
    nplanes: usize,
    words_per_row: usize,
    control: &ControlState,
    first_vpos: i32,
    addr_mask: u32,
) {
    let row_data_bytes = (words_per_row * 2) as i64;
    for row in 0..rows {
        let vpos = first_vpos + row as i32;
        for (p, ptr) in ptrs.iter_mut().enumerate().take(nplanes.min(8)) {
            let delta = row_data_bytes + control.modulo_for_plane(p, vpos) as i64;
            *ptr = ((*ptr as i64).wrapping_add(delta) as u32) & addr_mask;
        }
    }
}

fn replay_bitplane_pointer_events_through_beam(
    events: &[BeamRegisterWrite],
    next_event: &mut usize,
    vpos: u32,
    hpos: u32,
    ptrs: &mut [u32; 8],
) {
    while let Some(event) = events.get(*next_event) {
        if !beam_event_at_or_before_beam(event, vpos, hpos) {
            break;
        }
        apply_bitplane_pointer_write(ptrs, event.offset & 0x01FE, event.value);
        *next_event += 1;
    }
}

fn replay_bitplane_data_events_through_beam(
    events: &[BeamRegisterWrite],
    next_event: &mut usize,
    vpos: u32,
    hpos: u32,
    bpldat: &mut [u16; 8],
) {
    while let Some(event) = events.get(*next_event) {
        if !beam_event_at_or_before_beam(event, vpos, hpos) {
            break;
        }
        apply_bitplane_data_write(bpldat, event.offset & 0x01FE, event.value);
        *next_event += 1;
    }
}

fn beam_event_at_or_before_beam(event: &BeamRegisterWrite, vpos: u32, hpos: u32) -> bool {
    event.vpos < vpos || (event.vpos == vpos && event.hpos <= hpos)
}

fn bitplane_fetch_hpos(control: ControlState, word_idx: usize) -> u32 {
    bitplane_fetch_hpos_for_plane(control, word_idx, 0)
}

fn bitplane_fetch_framebuffer_x(hpos: u32) -> usize {
    ((hpos as i32 * 2 - DIW_HSTART_FB0) * 2).clamp(0, FB_WIDTH as i32) as usize
}

fn apply_bitplane_pointer_write(ptrs: &mut [u32; 8], off: u16, val: u16) {
    if !(0x0E0..=0x0FF).contains(&off) {
        return;
    }
    let idx = ((off - 0x0E0) / 4) as usize;
    if idx >= ptrs.len() {
        return;
    }
    if off & 2 == 0 {
        let cur = ptrs[idx];
        ptrs[idx] = (cur & 0x0000_FFFF) | ((val as u32 & 0x001F) << 16);
    } else {
        let cur = ptrs[idx];
        ptrs[idx] = (cur & 0x00FF_0000) | (val as u32 & 0xFFFE);
    }
}

fn apply_bitplane_data_write(bpldat: &mut [u16; 8], off: u16, val: u16) {
    if !(0x110..=0x11E).contains(&off) {
        return;
    }
    let idx = ((off - 0x110) / 2) as usize;
    if idx < bpldat.len() {
        bpldat[idx] = val;
    }
}

fn seed_manual_bpl_segments_from_latches(
    segments: &mut [ManualBplSegment],
    frame_start_bpldat: [u16; 8],
    render_events: &[BeamRegisterWrite],
    base_controls: &[ControlState],
    control_segments: &[Vec<ControlSegment>],
    captured_bitplane_rows: &[Option<CapturedBitplaneRow>],
    visible_line0: i32,
) {
    if segments.is_empty() {
        return;
    }

    let mut segment_indices_by_beam: HashMap<(usize, u32), Vec<usize>> = HashMap::new();
    for (idx, segment) in segments.iter().enumerate() {
        segment_indices_by_beam
            .entry((segment.line, segment.hpos))
            .or_default()
            .push(idx);
    }
    for indices in segment_indices_by_beam.values_mut() {
        indices.reverse();
    }

    let rows = base_controls.len().min(control_segments.len());
    let mut bpldat = frame_start_bpldat;
    let mut event_idx = 0usize;

    for line in 0..rows {
        let beam_y_i = visible_line0 + line as i32;
        if beam_y_i < 0 {
            continue;
        }
        let beam_y = beam_y_i as u32;
        while let Some(event) = render_events.get(event_idx) {
            if event.vpos >= beam_y {
                break;
            }
            apply_bitplane_data_write(&mut bpldat, event.offset & 0x01FE, event.value);
            event_idx += 1;
        }

        let row_control_segments = &control_segments[line];
        let words_per_row = line_words_per_row(base_controls[line], row_control_segments);
        let dma_planes = line_max_dma_planes(base_controls[line], row_control_segments);
        let mut fetches = Vec::new();
        if dma_planes != 0 && line_has_valid_ddf_window(base_controls[line], row_control_segments) {
            if let Some(captured) = captured_bitplane_rows.get(line).and_then(Option::as_ref) {
                let fetch_plans = line_fetch_plans_for_line(
                    base_controls[line],
                    row_control_segments,
                    words_per_row,
                    dma_planes,
                );
                for (word_idx, plan) in fetch_plans
                    .iter()
                    .enumerate()
                    .take(words_per_row.min(captured.words_per_row))
                {
                    for (hpos, plane) in plan.iter() {
                        if plane < dma_planes.min(8)
                            && plane < captured.nplanes
                            && word_idx < captured.planes[plane].len()
                        {
                            fetches.push((hpos, plane, word_idx));
                        }
                    }
                }
                fetches.sort_unstable();
            }
        }

        let mut fetch_idx = 0usize;
        loop {
            let next_event_hpos = render_events
                .get(event_idx)
                .and_then(|event| (event.vpos == beam_y).then_some(event.hpos));
            let next_fetch = fetches.get(fetch_idx).copied();
            match (next_event_hpos, next_fetch) {
                (Some(event_hpos), Some((fetch_hpos, plane, word_idx)))
                    if fetch_hpos < event_hpos =>
                {
                    bpldat[plane] = captured_bitplane_rows[line]
                        .as_ref()
                        .and_then(|row| row.planes[plane].get(word_idx).copied())
                        .unwrap_or(0);
                    fetch_idx += 1;
                }
                (Some(_), _) => {
                    let event = render_events[event_idx];
                    let off = event.offset & 0x01FE;
                    apply_bitplane_data_write(&mut bpldat, off, event.value);
                    if off == 0x110 {
                        if let Some(indices) = segment_indices_by_beam.get_mut(&(line, event.hpos))
                        {
                            if let Some(segment_idx) = indices.pop() {
                                segments[segment_idx].planes = bpldat;
                            }
                        }
                    }
                    event_idx += 1;
                }
                (None, Some((_fetch_hpos, plane, word_idx))) => {
                    bpldat[plane] = captured_bitplane_rows[line]
                        .as_ref()
                        .and_then(|row| row.planes[plane].get(word_idx).copied())
                        .unwrap_or(0);
                    fetch_idx += 1;
                }
                (None, None) => break,
            }
        }
    }
}

struct TimedChipRam<'a> {
    ram: Cow<'a, [u8]>,
    writes: &'a [BeamChipRamWrite],
    next_write: usize,
}

impl<'a> TimedChipRam<'a> {
    fn new(ram: &'a [u8], writes: &'a [BeamChipRamWrite]) -> Self {
        Self {
            ram: Cow::Borrowed(ram),
            writes,
            next_write: 0,
        }
    }

    fn len(&self) -> usize {
        self.ram.len()
    }

    fn replay_through(&mut self, vpos: u32, hpos: u32) {
        while let Some(write) = self.writes.get(self.next_write) {
            if write.vpos > vpos || (write.vpos == vpos && write.hpos > hpos) {
                break;
            }
            let ram = self.ram.to_mut();
            let offset = write.offset as usize;
            for (idx, byte) in write.bytes().iter().copied().enumerate() {
                if let Some(dst) = ram.get_mut(offset + idx) {
                    *dst = byte;
                }
            }
            self.next_write += 1;
        }
    }

    fn read_word_wrapping(&mut self, addr: u32, vpos: u32, hpos: u32) -> u16 {
        self.replay_through(vpos, hpos);
        read_chip_word_wrapping(&self.ram, addr)
    }
}

fn apply_move(state: &mut RenderState, off: u16, val: u16) {
    match off {
        0x100 => state.bplcon0 = val,
        0x102 => state.bplcon1 = val,
        0x104 => state.bplcon2 = val,
        0x106 => state.bplcon3 = val,
        0x10C => state.bplcon4 = val,
        0x1FC => state.fmode = val & 0xC00F,
        0x108 => state.bpl1mod = (val & 0xFFFE) as i16,
        0x10A => state.bpl2mod = (val & 0xFFFE) as i16,
        0x098 => {
            state.clxcon = val;
            // AGA: a CLXCON write resets CLXCON2.
            state.clxcon2 = 0;
        }
        0x10E => state.clxcon2 = val & 0x0FFF,
        // Writing DIWSTRT/DIWSTOP re-arms the OCS-implicit high bits: an ECS
        // DIWHIGH value only applies until the next DIWSTRT/DIWSTOP write (the
        // Agnus side clears its diwhigh_written flag here for the same reason).
        // Without this, a stale DIWHIGH (e.g. $00FF, pushing V-start off-screen)
        // keeps the display window empty even after the window is reprogrammed.
        0x08E => {
            state.diwstrt = val;
            state.diwhigh = DiwHigh::ocs_implicit();
        }
        0x090 => {
            state.diwstop = val;
            state.diwhigh = DiwHigh::ocs_implicit();
        }
        0x1E4 => state.diwhigh = DiwHigh::ecs_explicit(val),
        0x092 => state.ddfstrt = val,
        0x094 => state.ddfstop = val,
        0x096 => {
            let bits = val & 0x07FF;
            if val & 0x8000 != 0 {
                state.dmacon |= bits;
            } else {
                state.dmacon &= !bits;
            }
        }
        0x110..=0x11E => {
            let idx = ((off - 0x110) / 2) as usize;
            if idx < 8 {
                state.bpldat[idx] = val;
            }
        }
        0x120..=0x13F => {
            let idx = ((off - 0x120) / 4) as usize;
            if idx < 8 {
                if off & 2 == 0 {
                    let cur = state.sprpt[idx];
                    state.sprpt[idx] = (cur & 0x0000_FFFF) | ((val as u32 & 0x001F) << 16);
                } else {
                    let cur = state.sprpt[idx];
                    state.sprpt[idx] = (cur & 0x00FF_0000) | (val as u32 & 0xFFFE);
                }
            }
        }
        0x140..=0x17F => {
            let idx = ((off - 0x140) / 8) as usize;
            let reg = (off - 0x140) & 0x0006;
            if idx < 8 {
                match reg {
                    0x0 => state.sprpos[idx] = val,
                    0x2 => {
                        state.sprctl[idx] = val;
                        state.spr_armed[idx] = false;
                    }
                    0x4 => {
                        state.sprdata[idx] = val;
                        state.spr_armed[idx] = true;
                    }
                    0x6 => state.sprdatb[idx] = val,
                    _ => {}
                }
            }
        }
        0x0E0..=0x0FF => {
            let idx = ((off - 0x0E0) / 4) as usize;
            if idx < 8 {
                if off & 2 == 0 {
                    let cur = state.bplpt[idx];
                    state.bplpt[idx] = (cur & 0x0000_FFFF) | ((val as u32 & 0x001F) << 16);
                } else {
                    let cur = state.bplpt[idx];
                    state.bplpt[idx] = (cur & 0x00FF_0000) | (val as u32 & 0xFFFE);
                }
            }
        }
        0x180..=0x1BE => {
            let idx = ((off - 0x180) / 2) as usize;
            if idx < 32 {
                let aga = matches!(state.agnus_revision, AgnusRevision::AgaAlice);
                let (entry, loct) = palette_entry_for_write(state.bplcon3, aga, idx);
                state
                    .palette
                    .write_entry(usize::from(entry), loct, color_register_value(val));
            }
        }
        _ => {}
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn manual_sprite_lines_from_events(
    initial_state: &RenderState,
    events: &[BeamRegisterWrite],
) -> Vec<Vec<SpriteLine>> {
    manual_sprite_lines_from_events_with_visible_line0(
        initial_state,
        events,
        &[None; 8],
        PAL_VISIBLE_LINE0,
        FB_HEIGHT,
        true,
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ManualSpriteFlushMode {
    ClipAtEnd,
    PreserveStartedOutput,
}

fn manual_sprite_lines_from_events_with_visible_line0(
    initial_state: &RenderState,
    events: &[BeamRegisterWrite],
    held: &[Option<HeldSpriteLine>; 8],
    visible_line0: i32,
    rows: usize,
    include_latched_sprite_state: bool,
) -> Vec<Vec<SpriteLine>> {
    let mut regs = BeamSpriteState::from_render_state(initial_state, held);
    let visible_end = visible_line0 + rows as i32;
    let mut next_beam: [(i32, usize); 8] = std::array::from_fn(|sprite| {
        if include_latched_sprite_state || held[sprite].is_some() {
            (visible_line0, 0usize)
        } else {
            (visible_end, 0usize)
        }
    });
    let mut lines = vec![Vec::new(); 8];

    for event in events {
        let off = event.offset & 0x01FE;
        if off == 0x1FC {
            // FMODE changes the manual sprite output width, so flush every
            // sprite's pending span at the old width before applying it.
            let event_beam = manual_sprite_event_beam(event.vpos, event.hpos, visible_line0, rows);
            for sprite in 0..8 {
                flush_manual_sprite_lines(
                    sprite,
                    &regs,
                    next_beam[sprite],
                    event_beam,
                    ManualSpriteFlushMode::ClipAtEnd,
                    &mut lines,
                );
                next_beam[sprite] = event_beam;
            }
            regs.apply_write(off, event.value);
            continue;
        }
        if !(0x140..=0x17F).contains(&off) {
            continue;
        }
        let sprite = ((off - 0x140) / 8) as usize;
        if sprite >= 8 {
            continue;
        }
        let event_beam = manual_sprite_event_beam_for_sprite_write(
            off,
            event.vpos,
            event.hpos,
            visible_line0,
            rows,
        );
        flush_manual_sprite_lines(
            sprite,
            &regs,
            next_beam[sprite],
            event_beam,
            ManualSpriteFlushMode::PreserveStartedOutput,
            &mut lines,
        );
        if !include_latched_sprite_state
            && held[sprite].is_none()
            && (off - 0x140) & 0x0006 == 0
            && event.hpos < SPRITE_DMA_PAIR_CAPTURE_HPOS[sprite / 2]
        {
            if regs.direct_data_armed[sprite] {
                regs.spr_armed[sprite] = false;
            }
            regs.direct_data_armed[sprite] = false;
        }
        regs.apply_write(off, event.value);
        if (event.vpos as i32) < visible_line0 && matches!((off - 0x140) & 0x0006, 0x4 | 0x6) {
            regs.direct_data_armed[sprite] = false;
        }
        next_beam[sprite] = event_beam;
    }

    for sprite in 0..8 {
        flush_manual_sprite_lines(
            sprite,
            &regs,
            next_beam[sprite],
            (visible_end, 0),
            ManualSpriteFlushMode::ClipAtEnd,
            &mut lines,
        );
    }

    lines
}

fn manual_sprite_lines_from_captured_dma_reuse(
    initial_state: &RenderState,
    events: &[BeamRegisterWrite],
    captured_sprite_lines: &[CapturedSpriteLine],
    visible_line0: i32,
    rows: usize,
) -> Vec<Vec<SpriteLine>> {
    let mut lines = vec![Vec::new(); 8];
    if captured_sprite_lines.is_empty() || events.is_empty() {
        return lines;
    }

    let visible_end = visible_line0 + rows as i32;
    let mut events_by_sprite: [Vec<BeamRegisterWrite>; 8] = std::array::from_fn(|_| Vec::new());
    for event in events {
        let off = event.offset & 0x01FE;
        if !(0x140..=0x17F).contains(&off) {
            continue;
        }
        let sprite = ((off - 0x140) / 8) as usize;
        if sprite < events_by_sprite.len() {
            events_by_sprite[sprite].push(*event);
        }
    }

    for captured in captured_sprite_lines {
        let sprite = captured.sprite;
        if sprite >= 8 || events_by_sprite[sprite].is_empty() {
            continue;
        }
        let beam_y = captured.beam_y;
        if beam_y < visible_line0 || beam_y >= visible_end {
            continue;
        }

        let mut held = [None; 8];
        held[sprite] = Some(HeldSpriteLine {
            line: *captured,
            vstart: beam_y,
            vstop: beam_y + 1,
        });
        let mut regs = BeamSpriteState::from_render_state(initial_state, &held);
        let mut next_beam = (visible_end, 0usize);
        let dma_hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[sprite / 2];

        for event in events_by_sprite[sprite]
            .iter()
            .filter(|event| event.vpos as i32 == beam_y && event.hpos >= dma_hpos)
        {
            let off = event.offset & 0x01FE;
            let event_beam = manual_sprite_event_beam_for_sprite_write(
                off,
                event.vpos,
                event.hpos,
                visible_line0,
                rows,
            );

            match (off - 0x140) & 0x0006 {
                // SPRxPOS re-arms the horizontal comparator. If sprite DMA has
                // already loaded this scanline's data, a later POS write can
                // reuse that data without another SPRxDATA write.
                0x0 => {
                    flush_manual_sprite_lines(
                        sprite,
                        &regs,
                        next_beam,
                        event_beam,
                        ManualSpriteFlushMode::PreserveStartedOutput,
                        &mut lines,
                    );
                    regs.apply_write(off, event.value);
                    next_beam = event_beam;
                }
                // DATA/CTL writes leave the DMA-seeded reuse model. The normal
                // beam-timed manual replay handles explicitly written data;
                // CTL disarms output until DATA arms it again.
                _ => {
                    flush_manual_sprite_lines(
                        sprite,
                        &regs,
                        next_beam,
                        event_beam,
                        ManualSpriteFlushMode::ClipAtEnd,
                        &mut lines,
                    );
                    next_beam = (visible_end, 0);
                }
            }
        }

        flush_manual_sprite_lines(
            sprite,
            &regs,
            next_beam,
            (beam_y + 1, 0),
            ManualSpriteFlushMode::ClipAtEnd,
            &mut lines,
        );
    }

    lines
}

fn merge_dma_seeded_manual_sprite_lines(
    manual_lines: &mut [Vec<SpriteLine>],
    mut dma_seeded_lines: Vec<Vec<SpriteLine>>,
) {
    for (sprite, seeded) in dma_seeded_lines.iter_mut().enumerate() {
        if seeded.is_empty() {
            continue;
        }
        let target = &mut manual_lines[sprite];
        clip_sprite_lines_around_register_lines(seeded, target);
        target.append(seeded);
        target.sort_by_key(|line| (line.beam_y, line.x_start, line.x_stop));
    }
}

fn manual_sprite_event_beam_for_sprite_write(
    off: u16,
    vpos: u32,
    hpos: u32,
    visible_line0: i32,
    rows: usize,
) -> (i32, usize) {
    match (off - 0x140) & 0x0006 {
        // SPRxPOS re-arms the sprite horizontal comparator. When the
        // write happens before the newly programmed HSTART, the sprite can
        // still begin at HSTART; clipping in the later colour-output register
        // domain delays attached pairs whose even/odd position writes are
        // staggered by the Copper.
        0x0 => manual_sprite_position_event_beam(vpos, hpos, visible_line0, rows),
        // SPRxDATA/SPRxDATB update the latches copied by Denise's horizontal
        // sprite comparator. If the write reaches that path before the
        // comparator fires, the new data belongs to the current scanline.
        0x4 | 0x6 => manual_sprite_data_event_beam(vpos, hpos, visible_line0, rows),
        _ => manual_sprite_event_beam(vpos, hpos, visible_line0, rows),
    }
}

fn manual_sprite_event_beam(vpos: u32, hpos: u32, visible_line0: i32, rows: usize) -> (i32, usize) {
    let visible_end = visible_line0 + rows as i32;
    let vpos = vpos as i32;
    if vpos < visible_line0 {
        return (visible_line0, 0);
    }
    if vpos >= visible_end {
        return (visible_end, 0);
    }
    let (_, x) = beam_to_framebuffer_pos_with_visible_line0(vpos as u32, hpos, visible_line0, rows);
    (vpos, x)
}

fn manual_sprite_position_event_beam(
    vpos: u32,
    hpos: u32,
    visible_line0: i32,
    rows: usize,
) -> (i32, usize) {
    let visible_end = visible_line0 + rows as i32;
    let vpos = vpos as i32;
    if vpos < visible_line0 {
        return (visible_line0, 0);
    }
    if vpos >= visible_end {
        return (visible_end, 0);
    }
    let x = sprite_position_write_framebuffer_x(hpos);
    (vpos, x)
}

fn manual_sprite_data_event_beam(
    vpos: u32,
    hpos: u32,
    visible_line0: i32,
    rows: usize,
) -> (i32, usize) {
    let visible_end = visible_line0 + rows as i32;
    let vpos = vpos as i32;
    if vpos < visible_line0 {
        return (visible_line0, 0);
    }
    if vpos >= visible_end {
        return (visible_end, 0);
    }
    let x = sprite_data_write_framebuffer_x(hpos);
    (vpos, x)
}

fn sprite_position_write_framebuffer_x(hpos: u32) -> usize {
    let hpos = hpos.saturating_sub(SPRITE_REGISTER_WRITE_PIPELINE_CCK);
    ((hpos as i32 * 2 - DIW_HSTART_FB0) * 2).clamp(0, FB_WIDTH as i32) as usize
}

fn sprite_data_write_framebuffer_x(hpos: u32) -> usize {
    sprite_position_write_framebuffer_x(hpos)
}

fn flush_manual_sprite_lines(
    sprite: usize,
    regs: &BeamSpriteState,
    start_beam: (i32, usize),
    end_beam: (i32, usize),
    mode: ManualSpriteFlushMode,
    lines: &mut [Vec<SpriteLine>],
) {
    let (start_line, start_x) = start_beam;
    let (end_line, end_x) = end_beam;
    if start_line > end_line || (start_line == end_line && start_x >= end_x) {
        return;
    }
    let end_exclusive = if end_x == 0 { end_line } else { end_line + 1 };
    for beam_y in start_line..end_exclusive {
        let x_start = if beam_y == start_line { start_x } else { 0 };
        let mut x_stop = if beam_y == end_line { end_x } else { FB_WIDTH };
        if mode == ManualSpriteFlushMode::PreserveStartedOutput && beam_y == end_line {
            let pos = regs.sprpos[sprite];
            let ctl = regs.sprctl[sprite];
            let base_x = sprite_nominal_base_framebuffer_x(pos, ctl);
            if x_stop as i32 >= base_x {
                x_stop = FB_WIDTH;
            }
        }
        if let Some(line) = regs.line_for_sprite(sprite, beam_y, x_start, x_stop) {
            lines[sprite].push(line);
        }
    }
}

fn projected_primary_bitplane_pointer(mut ptr: u32, events: &[BeamRegisterWrite]) -> u32 {
    for event in events {
        match event.offset & 0x01FE {
            0x0E0 => {
                ptr = (ptr & 0x0000_FFFF) | ((event.value as u32 & 0x001F) << 16);
            }
            0x0E2 => {
                ptr = (ptr & 0x00FF_0000) | (event.value as u32 & 0xFFFE);
            }
            _ => {}
        }
    }
    ptr
}

fn primary_bitplane_buffer_carries_forward(
    completed_bpl0: u32,
    completed_events: &[BeamRegisterWrite],
    current_bpl0: u32,
    current_events: &[BeamRegisterWrite],
) -> bool {
    let completed = projected_primary_bitplane_pointer(completed_bpl0, completed_events);
    let current = projected_primary_bitplane_pointer(current_bpl0, current_events);
    completed != 0 && completed == current
}

fn is_cpu_copper_irq_palette_event(event: &BeamRegisterWrite) -> bool {
    matches!(event.offset & 0x01FE, 0x180..=0x1BE)
        && matches!(event.source, BeamWriteSource::CpuCopperIrq)
}

fn has_non_irq_beam_palette_events(events: &[BeamRegisterWrite], visible_line0: i32) -> bool {
    events.iter().any(|event| {
        if !matches!(event.offset & 0x01FE, 0x180..=0x1BE) {
            return false;
        }
        match event.source {
            BeamWriteSource::Copper => true,
            BeamWriteSource::Cpu => (event.vpos as i32) > visible_line0,
            BeamWriteSource::CpuCopperIrq => false,
        }
    })
}

#[cfg_attr(not(test), allow(dead_code))]
fn should_replay_bottom_palette_events(
    frame_events: &[BeamRegisterWrite],
    frame_cpu_copper_palette_events: &[BeamRegisterWrite],
    bottom_palette_replay_events: &[BeamRegisterWrite],
    beam_bottom_palette_valid: bool,
) -> bool {
    should_replay_bottom_palette_events_with_visible_line0(
        frame_events,
        frame_cpu_copper_palette_events,
        bottom_palette_replay_events,
        beam_bottom_palette_valid,
        PAL_VISIBLE_LINE0,
    )
}

fn should_replay_bottom_palette_events_with_visible_line0(
    frame_events: &[BeamRegisterWrite],
    frame_cpu_copper_palette_events: &[BeamRegisterWrite],
    bottom_palette_replay_events: &[BeamRegisterWrite],
    beam_bottom_palette_valid: bool,
    visible_line0: i32,
) -> bool {
    if bottom_palette_replay_events.is_empty() || !beam_bottom_palette_valid {
        return false;
    }
    if palette_event_sequences_equivalent(
        bottom_palette_replay_events,
        frame_cpu_copper_palette_events,
    ) {
        return true;
    }
    frame_cpu_copper_palette_events.is_empty()
        && !has_non_irq_beam_palette_events(frame_events, visible_line0)
}

/// Decide whether the copper-interrupt-positioned bottom-palette replay events
/// must be injected into this frame's render events.
///
/// They are needed only in the carry-forward case: the bottom palette was
/// established by a copper interrupt in an earlier frame, and this frame carries
/// no raw CpuCopperIrq palette writes of its own to position from. When the
/// frame does contain those raw writes (the same-frame case), they already carry
/// beam-accurate positions from the cycle-stepped CPU. Injecting the replay
/// events as well would apply each write a second time at the copper interrupt's
/// trigger beam position, which precedes the 68000 interrupt latency before the
/// handler's MOVE executes. That double-application recolors the scanline on
/// which the copper raised the interrupt, one line ahead of where the palette
/// truly changes.
fn should_inject_bottom_palette_replay_events_with_visible_line0(
    frame_events: &[BeamRegisterWrite],
    frame_cpu_copper_palette_events: &[BeamRegisterWrite],
    bottom_palette_replay_events: &[BeamRegisterWrite],
    beam_bottom_palette_valid: bool,
    visible_line0: i32,
) -> bool {
    frame_cpu_copper_palette_events.is_empty()
        && should_replay_bottom_palette_events_with_visible_line0(
            frame_events,
            frame_cpu_copper_palette_events,
            bottom_palette_replay_events,
            beam_bottom_palette_valid,
            visible_line0,
        )
}

#[cfg(test)]
fn should_inject_bottom_palette_replay_events(
    frame_events: &[BeamRegisterWrite],
    frame_cpu_copper_palette_events: &[BeamRegisterWrite],
    bottom_palette_replay_events: &[BeamRegisterWrite],
    beam_bottom_palette_valid: bool,
) -> bool {
    should_inject_bottom_palette_replay_events_with_visible_line0(
        frame_events,
        frame_cpu_copper_palette_events,
        bottom_palette_replay_events,
        beam_bottom_palette_valid,
        PAL_VISIBLE_LINE0,
    )
}

fn append_bottom_palette_replay_events(
    out: &mut Vec<BeamRegisterWrite>,
    events: &[BeamRegisterWrite],
    bottom_palette: Palette,
) {
    for event in events {
        let off = event.offset & 0x01FE;
        let idx = ((off - 0x180) / 2) as usize;
        if idx < bottom_palette.len() {
            let mut replay = *event;
            replay.value = bottom_palette[idx];
            out.push(replay);
        }
    }
}

fn palette_event_sequences_equivalent(a: &[BeamRegisterWrite], b: &[BeamRegisterWrite]) -> bool {
    !a.is_empty()
        && a.len() == b.len()
        && a.iter().zip(b).all(|(a, b)| {
            (a.offset & 0x01FE) == (b.offset & 0x01FE)
                && color_register_value(a.value) == color_register_value(b.value)
        })
}

/// Debug helper: when `COPPERLINE_DBG_FRAMESTATE` is set, log the per-frame display
/// snapshot the renderer starts from (DMA enable, scroll, window, modulos,
/// bitplane pointers, the active palette, and a sprite summary) once per
/// rendered frame, optionally bounded by `COPPERLINE_DBG_AFTER` / `COPPERLINE_DBG_UNTIL`
/// emulated seconds. This watches how display state evolves across a frame
/// boundary where content unexpectedly appears or vanishes. All values are read
/// from the renderer's own frame snapshot, so they match what is drawn.
fn maybe_log_frame_state(
    emulated_seconds: f64,
    emulated_frames: u64,
    geometry: FrameGeometry,
    captured_sprite_lines: &[CapturedSpriteLine],
    sprite_dma_observed: bool,
    control: &ControlState,
    state: &RenderState,
    bplpt: &[u32; 8],
    visible_line0: i32,
) {
    if !crate::envcfg::flag("COPPERLINE_DBG_FRAMESTATE") {
        return;
    }
    let secs = emulated_seconds;
    let after = env_f64("COPPERLINE_DBG_AFTER").unwrap_or(0.0);
    let until = env_f64("COPPERLINE_DBG_UNTIL").unwrap_or(f64::INFINITY);
    if secs < after || secs >= until {
        return;
    }
    log::info!(
        "framestate secs={secs:.4} frame={} vline0={visible_line0} dmacon={:#06X} \
         bplcon0={:#06X} bplcon1={:#06X} bplcon2={:#06X} bplcon3={:#06X} \
         bplcon4={:#06X} diwstrt={:#06X} diwstop={:#06X} \
         ddfstrt={:#06X} ddfstop={:#06X} fmode={:#06X} bpl1mod={} bpl2mod={} bplpt={:08X?}",
        emulated_frames,
        control.dmacon,
        control.bplcon0,
        control.bplcon1,
        control.bplcon2,
        control.bplcon3,
        control.bplcon4,
        control.diwstrt,
        control.diwstop,
        control.ddfstrt,
        control.ddfstop,
        control.fmode,
        control.bpl1mod,
        control.bpl2mod,
        bplpt,
    );
    log::info!(
        "  geometry: programmable={} visible_start={} visible_lines={} line_cck={} lace={}",
        geometry.programmable,
        geometry.visible_start_vpos,
        geometry.visible_lines,
        geometry.line_cck,
        geometry.lace,
    );
    let pal: Vec<String> = (0..16)
        .map(|i| format!("{:03x}", state.palette[i]))
        .collect();
    log::info!("  pal0-15=[{}]", pal.join(" "));
    let spr_lines = captured_sprite_lines;
    let mut per_sprite = [0u32; 8];
    let (mut ymin, mut ymax) = (i32::MAX, i32::MIN);
    for l in spr_lines {
        if l.sprite < 8 {
            per_sprite[l.sprite] += 1;
        }
        ymin = ymin.min(l.beam_y);
        ymax = ymax.max(l.beam_y);
    }
    log::info!(
        "  sprites: total={} dma_observed={} per_sprite={per_sprite:?} ybeam=[{},{}]",
        spr_lines.len(),
        sprite_dma_observed,
        ymin,
        ymax,
    );
    log::info!(
        "  sprpos={:04X?} sprctl={:04X?} sprarmed={:?}",
        state.sprpos,
        state.sprctl,
        state.spr_armed,
    );
}

#[derive(Clone, Copy)]
struct ManualSpriteDiagSpec {
    want_all: bool,
    beam_y: Option<i32>,
    after: f64,
    until: f64,
}

#[derive(Clone, Copy)]
struct SpritePixelDiagSpec {
    beam_y: i32,
    step: usize,
    after: f64,
    until: f64,
}

#[derive(Clone, Copy)]
struct PixelDiagSpec {
    beam_y: i32,
    x_start: usize,
    x_stop: usize,
    step: usize,
    after: f64,
    until: f64,
}

fn manual_sprite_diag_spec() -> Option<ManualSpriteDiagSpec> {
    static SPEC: OnceLock<Option<ManualSpriteDiagSpec>> = OnceLock::new();
    *SPEC.get_or_init(|| {
        let raw = crate::envcfg::var("COPPERLINE_DIAG_MANUAL_SPRITES")?;
        let raw = raw.trim();
        let (want_all, beam_y) = if raw.eq_ignore_ascii_case("all") {
            (true, None)
        } else {
            (false, Some(raw.parse::<i32>().ok()?))
        };
        Some(ManualSpriteDiagSpec {
            want_all,
            beam_y,
            after: env_f64("COPPERLINE_DBG_AFTER").unwrap_or(0.0),
            until: env_f64("COPPERLINE_DBG_UNTIL").unwrap_or(f64::INFINITY),
        })
    })
}

fn sprite_pixel_diag_spec() -> Option<SpritePixelDiagSpec> {
    static SPEC: OnceLock<Option<SpritePixelDiagSpec>> = OnceLock::new();
    *SPEC.get_or_init(|| {
        let raw = crate::envcfg::var("COPPERLINE_DIAG_SPRITE_PIXELS")?;
        let raw = raw.trim();
        let (beam_y, step) = if let Some((beam_y, step)) = raw.split_once(',') {
            (
                beam_y.trim().parse::<i32>().ok()?,
                step.trim().parse::<usize>().ok()?.max(1),
            )
        } else {
            (raw.parse::<i32>().ok()?, 32)
        };
        Some(SpritePixelDiagSpec {
            beam_y,
            step,
            after: env_f64("COPPERLINE_DBG_AFTER").unwrap_or(0.0),
            until: env_f64("COPPERLINE_DBG_UNTIL").unwrap_or(f64::INFINITY),
        })
    })
}

fn parse_pixel_diag_spec(raw: &str) -> Option<PixelDiagSpec> {
    let parts: Vec<_> = raw.split(',').map(str::trim).collect();
    if !(3..=4).contains(&parts.len()) {
        return None;
    }
    let beam_y = parts[0].parse::<i32>().ok()?;
    let x_start = parts[1].parse::<usize>().ok()?;
    let x_stop = parts[2].parse::<usize>().ok()?;
    let step = parts
        .get(3)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);
    Some(PixelDiagSpec {
        beam_y,
        x_start: x_start.min(x_stop),
        x_stop: x_start.max(x_stop),
        step,
        after: env_f64("COPPERLINE_DBG_AFTER").unwrap_or(0.0),
        until: env_f64("COPPERLINE_DBG_UNTIL").unwrap_or(f64::INFINITY),
    })
}

fn ham_pixel_diag_spec() -> Option<PixelDiagSpec> {
    static SPEC: OnceLock<Option<PixelDiagSpec>> = OnceLock::new();
    *SPEC.get_or_init(|| {
        let raw = crate::envcfg::var("COPPERLINE_DIAG_HAM_PIXELS")?;
        parse_pixel_diag_spec(&raw)
    })
}

fn manual_bpl_pixel_diag_spec() -> Option<PixelDiagSpec> {
    static SPEC: OnceLock<Option<PixelDiagSpec>> = OnceLock::new();
    *SPEC.get_or_init(|| {
        let raw = crate::envcfg::var("COPPERLINE_DIAG_MANUAL_BPL_PIXELS")?;
        parse_pixel_diag_spec(&raw)
    })
}

fn frame_pixel_diag_spec() -> Option<PixelDiagSpec> {
    static SPEC: OnceLock<Option<PixelDiagSpec>> = OnceLock::new();
    *SPEC.get_or_init(|| {
        let raw = crate::envcfg::var("COPPERLINE_DIAG_FRAME_PIXELS")?;
        parse_pixel_diag_spec(&raw)
    })
}

fn maybe_log_frame_pixel_samples(
    label: &str,
    emulated_seconds: f64,
    emulated_frames: u64,
    fb: &[u32],
    visible_line0: i32,
) {
    let Some(spec) = frame_pixel_diag_spec()
        .filter(|spec| emulated_seconds >= spec.after && emulated_seconds < spec.until)
    else {
        return;
    };
    let row = spec.beam_y - visible_line0;
    if !(0..FB_HEIGHT as i32).contains(&row) {
        return;
    }
    let row = row as usize;
    let start = spec.x_start.min(FB_WIDTH);
    let stop = spec.x_stop.min(FB_WIDTH);
    for x in start..stop {
        if !(x - start).is_multiple_of(spec.step) {
            continue;
        }
        let color = fb[row * FB_WIDTH + x] & 0x00FF_FFFF;
        log::info!(
            "frame-pixel {label} secs={emulated_seconds:.4} frame={emulated_frames} y={} x={x} rgba={:#010X} rgb={:#08X}",
            spec.beam_y,
            fb[row * FB_WIDTH + x],
            color,
        );
    }
}

fn maybe_log_manual_sprite_intervals(
    emulated_seconds: f64,
    emulated_frames: u64,
    state: &RenderState,
    events: &[BeamRegisterWrite],
    held: &[Option<HeldSpriteLine>; 8],
    lines: &[Vec<SpriteLine>],
) {
    let Some(spec) = manual_sprite_diag_spec() else {
        return;
    };
    let secs = emulated_seconds;
    if secs < spec.after || secs >= spec.until {
        return;
    }

    let held_summary: Vec<_> = held
        .iter()
        .map(|line| {
            line.map(|line| {
                (
                    line.vstart,
                    line.vstop,
                    line.line.hstart,
                    line.line.width_words,
                    line.line.attached,
                    line.line.data,
                    line.line.data_ext,
                    line.line.datb,
                    line.line.datb_ext,
                )
            })
        })
        .collect();
    let sprpt_align: Vec<_> = state.sprpt.iter().map(|ptr| ptr & 7).collect();
    let event_count = events
        .iter()
        .filter(|event| {
            matches!(
                event.offset & 0x01FE,
                0x096 | 0x106 | 0x10C | 0x140..=0x17E | 0x180..=0x1BE | 0x1FC
            )
        })
        .count();
    log::info!(
        "manual-sprite intervals secs={secs:.4} frame={} events={} sprpt={:08X?} sprpt_align={sprpt_align:?} held={held_summary:?}",
        emulated_frames,
        event_count,
        state.sprpt,
    );

    for event in events.iter().filter(|event| {
        matches!(
            event.offset & 0x01FE,
            0x096 | 0x106 | 0x10C | 0x140..=0x17E | 0x180..=0x1BE | 0x1FC
        )
    }) {
        if !spec.want_all
            && spec
                .beam_y
                .is_some_and(|beam_y| event.vpos as i32 != beam_y)
        {
            continue;
        }
        let off = event.offset & 0x01FE;
        let beam_x = beam_to_framebuffer_x_unclamped(event.hpos);
        let color_x = color_write_framebuffer_x(event.hpos);
        match off {
            0x096 => log::info!(
                "manual-sprite event y={} h={} beam_x={} DMACON={:#06X}",
                event.vpos,
                event.hpos,
                beam_x,
                event.value
            ),
            0x106 => log::info!(
                "manual-sprite event y={} h={} beam_x={} color_x={} BPLCON3={:#06X}",
                event.vpos,
                event.hpos,
                beam_x,
                color_x,
                event.value
            ),
            0x10C => log::info!(
                "manual-sprite event y={} h={} beam_x={} color_x={} BPLCON4={:#06X}",
                event.vpos,
                event.hpos,
                beam_x,
                color_x,
                event.value
            ),
            0x1FC => log::info!(
                "manual-sprite event y={} h={} beam_x={} FMODE={:#06X}",
                event.vpos,
                event.hpos,
                beam_x,
                event.value
            ),
            0x180..=0x1BE => log::info!(
                "manual-sprite event y={} h={} color_x={} COLOR{}={:#06X}",
                event.vpos,
                event.hpos,
                color_x,
                (off - 0x180) / 2,
                event.value
            ),
            0x140..=0x17E => {
                let sprite = ((off - 0x140) / 8) as usize;
                log::info!(
                    "manual-sprite event y={} h={} beam_x={} s{} reg={:#05X} val={:#06X}",
                    event.vpos,
                    event.hpos,
                    beam_x,
                    sprite,
                    off,
                    event.value
                );
            }
            _ => {}
        }
    }

    for (sprite, sprite_lines) in lines.iter().enumerate() {
        for line in sprite_lines {
            if !spec.want_all && spec.beam_y.is_some_and(|beam_y| line.beam_y != beam_y) {
                continue;
            }
            log::info!(
                "manual-sprite line y={} s{} x={}..{} hstart={} hsub={} words={} att={} A={:04X} {:04X?} B={:04X} {:04X?}",
                line.beam_y,
                sprite,
                line.x_start,
                line.x_stop,
                line.hstart,
                u8::from(line.hsub_70ns),
                line.width_words,
                u8::from(line.attached),
                line.data,
                line.data_ext,
                line.datb,
                line.datb_ext
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn maybe_log_sprite_pixel_samples(
    emulated_seconds: f64,
    emulated_frames: u64,
    state: &RenderState,
    fb: &[u32],
    captured_sprite_lines: &[CapturedSpriteLine],
    sprite_dma_observed: bool,
    manual_sprite_lines: &[Vec<SpriteLine>],
    base_palettes: &[Palette],
    palette_segments: &[Vec<PaletteSegment>],
    base_controls: &[ControlState],
    control_segments: &[Vec<ControlSegment>],
    sprite_display_enable_x_by_y: &[Option<usize>],
    playfield_mask: &[u8],
    visible_line0: i32,
) {
    let Some(spec) = sprite_pixel_diag_spec() else {
        return;
    };
    let secs = emulated_seconds;
    if secs < spec.after || secs >= spec.until {
        return;
    }
    let y = spec.beam_y - visible_line0;
    if y < 0 || y >= base_controls.len() as i32 {
        return;
    }
    let y = y as usize;
    let use_captured_sprite_dma = sprite_dma_observed
        || state.dmacon & (DMACON_DMAEN | DMACON_SPREN) == (DMACON_DMAEN | DMACON_SPREN);
    let sprite_lines: [Vec<SpriteLine>; 8] = std::array::from_fn(|sprite| {
        collect_sprite_lines(
            sprite,
            state,
            captured_sprite_lines,
            use_captured_sprite_dma,
            Some(manual_sprite_lines),
        )
    });
    log::info!(
        "sprite-pixel samples secs={secs:.4} frame={} y={} step={}",
        emulated_frames,
        spec.beam_y,
        spec.step
    );
    for x in (0..FB_WIDTH).step_by(spec.step) {
        let control = control_at_x(base_controls[y], &control_segments[y], x);
        let palette = palette_at_x(base_palettes[y], &palette_segments[y], x);
        let fb_idx = y * FB_WIDTH + x;
        let pf_mask = playfield_mask[fb_idx];
        let final_rgb = rgba8_to_rgb24(fb[fb_idx]);
        let display_enable_x = sprite_display_enable_x_for_y(sprite_display_enable_x_by_y, y);
        for pair in 0..4 {
            let even_sprite = pair * 2;
            let odd_sprite = even_sprite + 1;
            let even_lines = &sprite_lines[even_sprite];
            let odd_lines = &sprite_lines[odd_sprite];
            let attached = sprite_pair_attach_active_for_beam(even_lines, odd_lines, spec.beam_y);
            if attached {
                let even_idx = sprite_lines_pixel_bits_at(
                    even_lines,
                    spec.beam_y,
                    y,
                    x as i32,
                    base_controls,
                    control_segments,
                );
                let odd_idx = sprite_lines_pixel_bits_at(
                    odd_lines,
                    spec.beam_y,
                    y,
                    x as i32,
                    base_controls,
                    control_segments,
                );
                let idx = even_idx | (odd_idx << 2);
                if idx == 0 {
                    continue;
                }
                let color_idx = sprite_color_entry(control, even_sprite, idx, true);
                let priority = sprite_has_priority(even_sprite, pf_mask, control);
                let display = sprite_pixel_inside_display_window(
                    control,
                    y,
                    x,
                    visible_line0,
                    display_enable_x,
                );
                log::info!(
                    "sprite-pixel y={} x={} pair{} att idx={:#04X} color={} rgb={:#08X} final={:#08X} pf_mask={:#04X} priority={} display={} enable_x={:?} DIW={:#06X}/{:#06X} BPLCON2={:#06X} BPLCON3={:#06X} BPLCON4={:#06X}",
                    spec.beam_y,
                    x,
                    pair,
                    idx,
                    color_idx,
                    palette.rgb24(color_idx) & 0x00FF_FFFF,
                    final_rgb,
                    pf_mask,
                    priority,
                    display,
                    display_enable_x,
                    control.diwstrt,
                    control.diwstop,
                    control.bplcon2,
                    control.bplcon3,
                    control.bplcon4
                );
            } else {
                for sprite in [even_sprite, odd_sprite] {
                    let idx = sprite_lines_pixel_bits_at(
                        &sprite_lines[sprite],
                        spec.beam_y,
                        y,
                        x as i32,
                        base_controls,
                        control_segments,
                    );
                    if idx == 0 {
                        continue;
                    }
                    let color_idx = sprite_color_entry(control, sprite, idx, false);
                    let priority = sprite_has_priority(sprite, pf_mask, control);
                    let display = sprite_pixel_inside_display_window(
                        control,
                        y,
                        x,
                        visible_line0,
                        display_enable_x,
                    );
                    log::info!(
                        "sprite-pixel y={} x={} s{} idx={:#04X} color={} rgb={:#08X} final={:#08X} pf_mask={:#04X} priority={} display={} enable_x={:?} DIW={:#06X}/{:#06X} BPLCON2={:#06X} BPLCON3={:#06X} BPLCON4={:#06X}",
                        spec.beam_y,
                        x,
                        sprite,
                        idx,
                        color_idx,
                        palette.rgb24(color_idx) & 0x00FF_FFFF,
                        final_rgb,
                        pf_mask,
                        priority,
                        display,
                        display_enable_x,
                        control.diwstrt,
                        control.diwstop,
                        control.bplcon2,
                        control.bplcon3,
                        control.bplcon4
                    );
                }
            }
        }
    }
}

fn env_f64(var: &str) -> Option<f64> {
    crate::envcfg::var(var).and_then(|s| s.trim().parse::<f64>().ok())
}

/// Everything `render_from_input` needs to paint a completed frame, owned so
/// it can outlive the `Bus` borrow (and, with the render-thread pipeline, be
/// moved to a worker). It is a snapshot of the just-finished frame: the bus
/// already double-buffers chip RAM and the beam-event/capture logs at the
/// end-of-frame swap, so rendering is a pure function of this bundle.
pub struct RenderInput {
    geometry: FrameGeometry,
    visible_start_vpos: u32,
    palette_split: (Palette, Palette, bool),
    render_base: RenderRegisterSnapshot,
    frame_render_events: Vec<BeamRegisterWrite>,
    current_render_base: RenderRegisterSnapshot,
    current_render_events: Vec<BeamRegisterWrite>,
    bottom_palette_events: Vec<BeamRegisterWrite>,
    top_palette_end: Palette,
    chip_ram: Vec<u8>,
    chip_ram_writes: Vec<BeamChipRamWrite>,
    captured_bitplane_rows: Vec<Option<CapturedBitplaneRow>>,
    captured_sprite_lines: Vec<CapturedSpriteLine>,
    held_sprites: [Option<HeldSpriteLine>; 8],
    sprite_display_enable_x_by_y: Vec<Option<usize>>,
    sprite_dma_observed: bool,
    // Agnus-derived blanking windows, sampled once per frame from the live
    // latches (the helpers below take these instead of borrowing the Bus).
    frame_lines: u32,
    programmable_vertical_blank: Option<(u32, u32)>,
    programmable_horizontal_blank: Option<(u32, u32)>,
    // Scalars only the COPPERLINE_DBG_* side-channels read.
    emulated_seconds: f64,
    emulated_frames: u64,
}

impl RenderInput {
    /// Snapshot the just-finished frame from the bus into an owned bundle.
    pub fn from_bus(bus: &Bus) -> Self {
        Self {
            geometry: bus.frame_geometry(),
            visible_start_vpos: bus.frame_visible_start_vpos(),
            palette_split: bus.frame_palette_split(),
            render_base: bus.frame_render_base(),
            frame_render_events: bus.frame_render_events().to_vec(),
            current_render_base: bus.current_render_base(),
            current_render_events: bus.current_render_events().to_vec(),
            bottom_palette_events: bus.frame_bottom_palette_events().to_vec(),
            top_palette_end: bus.frame_top_palette_end(),
            chip_ram: bus.frame_chip_ram().to_vec(),
            chip_ram_writes: bus.frame_chip_ram_writes().to_vec(),
            captured_bitplane_rows: bus.frame_captured_bitplane_rows().to_vec(),
            captured_sprite_lines: bus.frame_captured_sprite_lines().to_vec(),
            held_sprites: bus.frame_held_sprites(),
            sprite_display_enable_x_by_y: bus.frame_sprite_display_enable_x_by_y().to_vec(),
            sprite_dma_observed: bus.frame_sprite_dma_observed(),
            frame_lines: bus.agnus.current_frame_lines(),
            programmable_vertical_blank: bus.agnus.programmable_vertical_blank(),
            programmable_horizontal_blank: bus.agnus.programmable_horizontal_blank(),
            emulated_seconds: bus.emulated_seconds(),
            emulated_frames: bus.emulated_frames(),
        }
    }

    pub fn geometry(&self) -> FrameGeometry {
        self.geometry
    }

    pub fn visible_start_vpos(&self) -> u32 {
        self.visible_start_vpos
    }

    pub fn render_base(&self) -> RenderRegisterSnapshot {
        self.render_base
    }

    pub fn emulated_frames(&self) -> u64 {
        self.emulated_frames
    }
}

/// Outputs of `render_from_input`. Render timing is always recorded back on
/// the main thread. `clxdat` is applied only by the synchronous wrapper; the
/// threaded path completes CPU-visible Denise collision state at frame end
/// before the worker can lag behind.
pub struct RenderResult {
    pub timing: VideoRenderFrameTiming,
    pub clxdat: u16,
}

/// Paint the just-finished frame through the synchronous compatibility path.
/// The render itself is a pure function of the owned snapshot
/// (`render_from_input`); this wrapper owns the remaining bus coupling.
pub fn render(bus: &mut Bus, fb: &mut [u32]) {
    let input = RenderInput::from_bus(bus);
    let result = render_from_input(&input, fb);
    bus.denise.or_clxdat(result.clxdat);
    bus.record_video_render_frame(result.timing);
}

pub fn render_from_input(input: &RenderInput, fb: &mut [u32]) -> RenderResult {
    let render_started = Instant::now();
    let mut render_timing = VideoRenderFrameTiming::default();
    let mut state = RenderState::from_snapshot(input.render_base);
    let geometry = input.geometry;
    // Rows rendered this frame: the frame geometry's scan height, bounded
    // by the caller's buffer (legacy fixed-size callers keep the classic
    // field height).
    let rows = geometry.visible_lines.min(fb.len() / FB_WIDTH);
    debug_assert!(fb.len() >= FB_WIDTH * rows);
    let visible_line0 = input.visible_start_vpos as i32;
    let (beam_top_palette, beam_bottom_palette, beam_bottom_palette_valid) = input.palette_split;
    let frame_render_events = input.frame_render_events.as_slice();
    let current_render_base = input.current_render_base;
    let current_render_events = input.current_render_events.as_slice();
    let primary_buffer_carries_forward = primary_bitplane_buffer_carries_forward(
        state.bplpt[0],
        frame_render_events,
        current_render_base.bplpt[0],
        current_render_events,
    );
    let frame_cpu_copper_palette_events: Vec<_> = frame_render_events
        .iter()
        .copied()
        .filter(is_cpu_copper_irq_palette_event)
        .collect();
    let bottom_palette_replay_events = input.bottom_palette_events.as_slice();
    let mut merged_render_events = Vec::new();
    let render_events = if should_inject_bottom_palette_replay_events_with_visible_line0(
        frame_render_events,
        &frame_cpu_copper_palette_events,
        bottom_palette_replay_events,
        beam_bottom_palette_valid,
        visible_line0,
    ) {
        // Carry-forward case: the bottom palette was established by a copper
        // interrupt in an earlier frame, so this frame contains no raw
        // CpuCopperIrq palette writes of its own. Replay the bottom-palette
        // writes at the copper interrupt beam position to reconstruct the
        // palette for this frame.
        merged_render_events.extend_from_slice(frame_render_events);
        append_bottom_palette_replay_events(
            &mut merged_render_events,
            bottom_palette_replay_events,
            beam_bottom_palette,
        );
        merged_render_events.sort_by_key(|event| (event.vpos, event.hpos));
        merged_render_events.as_slice()
    } else if should_replay_bottom_palette_events_with_visible_line0(
        frame_render_events,
        &frame_cpu_copper_palette_events,
        bottom_palette_replay_events,
        beam_bottom_palette_valid,
        visible_line0,
    ) {
        // Same-frame case: this frame already contains the raw CpuCopperIrq
        // palette writes that produced the bottom palette, and those raw writes
        // now carry beam-accurate positions because the CPU is cycle-stepped.
        // Re-injecting the replay events would apply each write a second time at
        // the copper interrupt's trigger position, which precedes the 68000
        // interrupt latency before the handler's MOVE executes, recoloring the
        // scanline on which the copper raised the interrupt one line ahead of
        // where the palette truly changes. Use the raw beam-accurate writes.
        frame_render_events
    } else if frame_cpu_copper_palette_events.is_empty() && primary_buffer_carries_forward {
        let current_cpu_copper_palette_events: Vec<_> = current_render_events
            .iter()
            .copied()
            .filter(is_cpu_copper_irq_palette_event)
            .collect();
        if !current_cpu_copper_palette_events.is_empty() {
            merged_render_events.extend_from_slice(frame_render_events);
            merged_render_events.extend_from_slice(&current_cpu_copper_palette_events);
            merged_render_events.sort_by_key(|event| (event.vpos, event.hpos));
            merged_render_events.as_slice()
        } else {
            frame_render_events
        }
    } else {
        frame_render_events
    };
    if beam_bottom_palette_valid {
        state.palette = if primary_buffer_carries_forward {
            input.top_palette_end
        } else {
            beam_top_palette
        };
    }
    let frame_start_bplpt = state.bplpt;
    let frame_start_bpldat = state.bpldat;
    let frame_start_control = ControlState::from_render_state(&state);
    maybe_log_frame_state(
        input.emulated_seconds,
        input.emulated_frames,
        input.geometry,
        &input.captured_sprite_lines,
        input.sprite_dma_observed,
        &frame_start_control,
        &state,
        &frame_start_bplpt,
        visible_line0,
    );
    let event_started = Instant::now();
    // Seed replay spans from beam-timed SPRx writes or DMA-established held
    // sprites. SPRxDATA latches remain armed across the frame boundary, but
    // they do not emit by themselves when captured DMA is the primary source;
    // a later SPRxPOS write can still reuse that latch after the DMA slot.
    let mut manual_sprite_lines = manual_sprite_lines_from_events_with_visible_line0(
        &state,
        render_events,
        &input.held_sprites,
        visible_line0,
        rows,
        false,
    );
    if input.sprite_dma_observed {
        let dma_seeded_lines = manual_sprite_lines_from_captured_dma_reuse(
            &state,
            render_events,
            &input.captured_sprite_lines,
            visible_line0,
            rows,
        );
        merge_dma_seeded_manual_sprite_lines(&mut manual_sprite_lines, dma_seeded_lines);
    }
    maybe_log_manual_sprite_intervals(
        input.emulated_seconds,
        input.emulated_frames,
        &state,
        render_events,
        &input.held_sprites,
        &manual_sprite_lines,
    );
    let mut base_palettes = vec![state.palette; rows];
    let mut palette_segments = vec![Vec::new(); rows];
    let mut base_controls = vec![frame_start_control; rows];
    let mut control_segments = vec![Vec::new(); rows];
    let mut manual_bpl_segments = Vec::new();
    #[cfg(any(test, debug_assertions, feature = "display-plan-trace"))]
    let mut display_frame_plan =
        crate::envcfg::var_os("COPPERLINE_TRACE_DISPLAY_PLAN").map(|_| DisplayFramePlan::new());
    #[cfg(any(test, debug_assertions, feature = "display-plan-trace"))]
    let display_line_events = display_frame_plan
        .as_mut()
        .map(DisplayFramePlan::register_events_mut);
    #[cfg(any(test, debug_assertions, feature = "display-plan-trace"))]
    apply_render_events_and_collect_display_plan_events_with_visible_line0(
        &mut state,
        render_events,
        &mut base_palettes,
        &mut palette_segments,
        &mut base_controls,
        &mut control_segments,
        &mut manual_bpl_segments,
        visible_line0,
        display_line_events,
    );
    #[cfg(not(any(test, debug_assertions, feature = "display-plan-trace")))]
    apply_render_events_with_visible_line0(
        &mut state,
        render_events,
        &mut base_palettes,
        &mut palette_segments,
        &mut base_controls,
        &mut control_segments,
        &mut manual_bpl_segments,
        visible_line0,
    );
    render_timing.event_nanos = event_started.elapsed().as_nanos();
    render_timing.events = render_events.len() as u64;
    render_timing.control_segments = control_segments
        .iter()
        .map(|segments| segments.len() as u64)
        .sum();
    let frame_ram = input.chip_ram.as_slice();
    let mut ram = TimedChipRam::new(frame_ram, input.chip_ram_writes.as_slice());
    let captured_bitplane_rows = input.captured_bitplane_rows.as_slice();
    let has_captured_bitplane_rows = captured_bitplane_rows.iter().any(Option::is_some);
    let captured_sprite_lines = input.captured_sprite_lines.as_slice();
    let sprite_display_enable_x_by_y = input.sprite_display_enable_x_by_y.as_slice();
    let sprite_dma_observed = input.sprite_dma_observed;
    render_timing.sprite_lines = captured_sprite_lines.len() as u64
        + manual_sprite_lines
            .iter()
            .map(|lines| lines.len() as u64)
            .sum::<u64>();

    let ram_mask = (ram.len() - 1) as u32;

    let mut ptrs: [u32; 8] = frame_start_bplpt;
    let mut next_bitplane_pointer_event = 0usize;
    let mut bpldat = frame_start_bpldat;
    let mut next_bitplane_data_event = 0usize;
    let mut playfield_mask = vec![0u8; FB_WIDTH * rows];
    let mut collision_pixels = vec![CollisionPixel::default(); FB_WIDTH * rows];
    let mut clxdat = 0u16;
    let mut dma_output_start_x_by_line = vec![None; rows];

    let background_started = Instant::now();
    fill_background_with_visible_line0(
        fb,
        &base_palettes,
        &palette_segments,
        &base_controls,
        &control_segments,
        visible_line0,
    );
    render_timing.background_nanos = background_started.elapsed().as_nanos();

    let any_bitplane_control = any_control_matching(&base_controls, &control_segments, |control| {
        control.nplanes() != 0
    });
    let any_bitplane_dma_control =
        any_control_matching(&base_controls, &control_segments, |control| {
            control.bitplane_dma_enabled() && control.nplanes() != 0
        });

    let playfield_started = Instant::now();
    if (has_captured_bitplane_rows || state.bplpt[0] != 0)
        && any_bitplane_control
        && (has_captured_bitplane_rows || any_bitplane_dma_control)
    {
        let frame_start_x = frame_start_control.display_window_x().0;
        let clipped_rows = frame_start_control.clipped_display_rows_before_frame(visible_line0);
        if clipped_rows != 0 {
            let control = line_control_at_x(&base_controls, &control_segments, 0, frame_start_x);
            replay_bitplane_pointer_events_through_beam(
                render_events,
                &mut next_bitplane_pointer_event,
                visible_line0 as u32,
                bitplane_fetch_hpos(control, 0),
                &mut ptrs,
            );
            // Bitplane DMA only fetched on the clipped lines where it was
            // enabled at the time: replay this frame's BPLCON0/DMACON writes
            // across the span instead of sampling the canvas-row-0 control
            // (mirrors the capture side's advance_display_dma_for_clipped_rows;
            // the CDTV boot screen opens DIW at line 5 but raises BPLCON0 to
            // 6 planes only at line 24).
            let mut line_control = control;
            line_control.bplcon0 = frame_start_control.bplcon0;
            line_control.dmacon = frame_start_control.dmacon;
            let fetch_gate_hpos = u32::from(BITPLANE_DDF_HARD_START);
            let first_line = visible_line0 - clipped_rows as i32;
            let mut event_idx = 0usize;
            for vpos in first_line..visible_line0 {
                while event_idx < render_events.len()
                    && ((render_events[event_idx].vpos as i32) < vpos
                        || (render_events[event_idx].vpos as i32 == vpos
                            && render_events[event_idx].hpos < fetch_gate_hpos))
                {
                    let event = render_events[event_idx];
                    match event.offset {
                        0x096 => {
                            if event.value & 0x8000 != 0 {
                                line_control.dmacon |= event.value & 0x7FFF;
                            } else {
                                line_control.dmacon &= !event.value;
                            }
                        }
                        0x100 => line_control.bplcon0 = event.value,
                        _ => {}
                    }
                    event_idx += 1;
                }
                if !line_control.bitplane_dma_enabled() {
                    continue;
                }
                let nplanes = line_control.dma_planes();
                if nplanes == 0 {
                    continue;
                }
                let native_w = native_frame_width_for_control(line_control);
                let words_per_row = line_control.words_per_row(native_w);
                advance_bitplane_ptrs_for_rows(
                    &mut ptrs,
                    1,
                    nplanes,
                    words_per_row,
                    &line_control,
                    vpos,
                    ram_mask,
                );
            }
        }
        let mut row_words: [Vec<u16>; 8] = std::array::from_fn(|_| Vec::new());
        // COPPERLINE_DBG_EXPORT_PLANES exports each bitplane and a composite
        // color-index image for every rendered frame in the requested
        // emulated-seconds window. It uses the exact per-line plane words the
        // renderer fetches, so it is a ground-truth view of each plane.
        const EXPORT_W: usize = 64 * 16;
        let mut export_planes: Option<Box<[Vec<u8>; 8]>> = None;
        let mut export_index: Option<Vec<u8>> = None;
        if crate::envcfg::flag("COPPERLINE_DBG_EXPORT_PLANES") {
            let after = env_f64("COPPERLINE_DBG_AFTER").unwrap_or(0.0);
            let until = env_f64("COPPERLINE_DBG_UNTIL").unwrap_or(f64::INFINITY);
            let secs = input.emulated_seconds;
            if secs >= after && secs < until {
                export_planes = Some(Box::new(std::array::from_fn(|_| {
                    vec![0u8; EXPORT_W * rows]
                })));
                export_index = Some(vec![0u8; EXPORT_W * rows]);
            }
        }
        // Tracks the last line that actually drew bitplanes, so a line whose
        // predecessor was border (no carried-over shifter data) can suppress
        // the BPLCON1 scroll pulling its leading pre-fetch words into view.
        let mut last_playfield_line: Option<usize> = None;
        let mut previous_playfield_tail_words: [Option<u16>; 8] = [None; 8];
        for y in 0..rows {
            let row_control_segments = &control_segments[y];
            let Some((x_start, x_stop)) = line_display_window_bounds(
                base_controls[y],
                row_control_segments,
                y,
                visible_line0,
            ) else {
                continue;
            };
            render_timing.playfield_pixels = render_timing
                .playfield_pixels
                .saturating_add((x_stop - x_start) as u64);
            let mut palette = base_palettes[y];
            let segments = &palette_segments[y];
            let mut segment_idx = 0usize;
            let control = line_control_at_x(&base_controls, &control_segments, y, x_start);
            let mut control_segment_idx = 0usize;
            let mut pixel_control = base_controls[y];
            let nplanes = line_max_display_planes(control, row_control_segments);
            if nplanes == 0 {
                continue;
            }
            let dma_planes = line_max_dma_planes(control, row_control_segments);
            if !line_has_valid_ddf_window(control, row_control_segments) {
                continue;
            }
            let words_per_row = line_words_per_row(base_controls[y], row_control_segments);
            let beam_y = visible_line0 as u32 + y as u32;
            replay_bitplane_pointer_events_through_beam(
                render_events,
                &mut next_bitplane_pointer_event,
                beam_y,
                bitplane_fetch_hpos(control, 0),
                &mut ptrs,
            );
            let fetched_pixels = words_per_row * 16;
            while segment_idx < segments.len() && segments[segment_idx].x <= x_start {
                segments[segment_idx].apply(&mut palette);
                segment_idx += 1;
            }
            while control_segment_idx < row_control_segments.len()
                && row_control_segments[control_segment_idx].x <= x_start
            {
                pixel_control = row_control_segments[control_segment_idx].control;
                control_segment_idx += 1;
            }
            let captured_row = captured_bitplane_rows
                .get(y)
                .and_then(Option::as_ref)
                .filter(|row| {
                    row.nplanes >= nplanes
                        && row.words_per_row == words_per_row
                        && row.planes[..nplanes]
                            .iter()
                            .all(|plane| plane.len() >= words_per_row)
                });
            if captured_row.is_none() && !control.bitplane_dma_enabled() {
                continue;
            }
            for (plane, words) in row_words.iter_mut().enumerate() {
                words.clear();
                if plane < nplanes {
                    words.resize(words_per_row, 0);
                }
            }
            if let Some(captured) = captured_row {
                for p in 0..nplanes {
                    row_words[p].copy_from_slice(&captured.planes[p][..words_per_row]);
                    if p < dma_planes {
                        ptrs[p] = ptrs[p].wrapping_add((words_per_row * 2) as u32);
                    }
                }
                if nplanes > dma_planes {
                    let line_fetch_plans = line_fetch_plans_for_line(
                        base_controls[y],
                        row_control_segments,
                        words_per_row,
                        dma_planes.min(nplanes),
                    );
                    for word_idx in 0..words_per_row {
                        let fetch_hpos = line_fetch_plans[word_idx]
                            .latched_plane_sample_hpos()
                            .unwrap_or_else(|| bitplane_fetch_hpos(control, word_idx));
                        replay_bitplane_data_events_through_beam(
                            render_events,
                            &mut next_bitplane_data_event,
                            beam_y,
                            fetch_hpos,
                            &mut bpldat,
                        );
                        for p in dma_planes..nplanes {
                            row_words[p][word_idx] = bpldat[p];
                        }
                    }
                    #[cfg(any(test, debug_assertions, feature = "display-plan-trace"))]
                    if let Some(display_frame_plan) = display_frame_plan.as_mut() {
                        display_frame_plan.record_line(
                            y,
                            beam_y,
                            x_start,
                            x_stop,
                            nplanes,
                            dma_planes,
                            words_per_row,
                            &line_fetch_plans,
                            &row_words,
                            captured_sprite_lines,
                            control,
                        );
                    }
                } else {
                    #[cfg(any(test, debug_assertions, feature = "display-plan-trace"))]
                    if let Some(display_frame_plan) = display_frame_plan.as_mut() {
                        let line_fetch_plans = line_fetch_plans_for_line(
                            base_controls[y],
                            row_control_segments,
                            words_per_row,
                            dma_planes.min(nplanes),
                        );
                        display_frame_plan.record_line(
                            y,
                            beam_y,
                            x_start,
                            x_stop,
                            nplanes,
                            dma_planes,
                            words_per_row,
                            &line_fetch_plans,
                            &row_words,
                            captured_sprite_lines,
                            control,
                        );
                    }
                }
            } else {
                let line_fetch_plans = line_fetch_plans_for_line(
                    base_controls[y],
                    row_control_segments,
                    words_per_row,
                    dma_planes.min(nplanes),
                );
                for word_idx in 0..words_per_row {
                    let fetch_plan = &line_fetch_plans[word_idx];
                    let fetch_hpos = fetch_plan.latched_plane_sample_hpos();
                    if nplanes > dma_planes {
                        if let Some(fetch_hpos) = fetch_hpos {
                            replay_bitplane_data_events_through_beam(
                                render_events,
                                &mut next_bitplane_data_event,
                                beam_y,
                                fetch_hpos,
                                &mut bpldat,
                            );
                        }
                    }
                    if fetch_hpos.is_some() {
                        for p in dma_planes..nplanes {
                            row_words[p][word_idx] = bpldat[p];
                        }
                    }
                    for (fetch_hpos, p) in fetch_plan.iter() {
                        replay_bitplane_pointer_events_through_beam(
                            render_events,
                            &mut next_bitplane_pointer_event,
                            beam_y,
                            fetch_hpos,
                            &mut ptrs,
                        );
                        row_words[p][word_idx] =
                            ram.read_word_wrapping(ptrs[p], beam_y, fetch_hpos);
                        ptrs[p] = ptrs[p].wrapping_add(2);
                    }
                }
                #[cfg(any(test, debug_assertions, feature = "display-plan-trace"))]
                if let Some(display_frame_plan) = display_frame_plan.as_mut() {
                    display_frame_plan.record_line(
                        y,
                        beam_y,
                        x_start,
                        x_stop,
                        nplanes,
                        dma_planes,
                        words_per_row,
                        &line_fetch_plans,
                        &row_words,
                        captured_sprite_lines,
                        control,
                    );
                }
            }
            if let (Some(planes), Some(index)) = (export_planes.as_mut(), export_index.as_mut()) {
                for word_i in 0..words_per_row.min(EXPORT_W / 16) {
                    for bit in 0..16 {
                        let col = word_i * 16 + bit;
                        let mask = 1u16 << (15 - bit);
                        let mut idx = 0u8;
                        for p in 0..nplanes.min(8) {
                            if row_words[p][word_i] & mask != 0 {
                                planes[p][y * EXPORT_W + col] = 255;
                                idx |= 1 << p;
                            }
                        }
                        index[y * EXPORT_W + col] = idx;
                    }
                }
            }
            let block_start = last_playfield_line != Some(y.wrapping_sub(1));
            let dma_output_start_x = bitplane_dma_output_start_x(
                base_controls[y],
                row_control_segments,
                x_start,
                words_per_row,
                dma_planes.min(nplanes),
            );
            let carry_words = bitplane_carry_words_for_line(
                block_start,
                x_start,
                dma_output_start_x,
                previous_playfield_tail_words,
            );
            let first_word_plane_start_x = first_word_plane_start_x(
                base_controls[y],
                row_control_segments,
                x_start,
                words_per_row,
                dma_planes.min(nplanes),
            );
            let line_plan = DenisePlannedPlayfieldLine::new(
                y,
                x_start,
                x_stop,
                &row_words[..nplanes],
                fetched_pixels,
            )
            .with_carry_words(carry_words)
            .with_first_word_plane_start_x(first_word_plane_start_x);
            let bpl_output_start_x = dma_output_start_x.unwrap_or(0);
            dma_output_start_x_by_line[y] = dma_output_start_x;
            last_playfield_line = Some(y);
            render_planned_playfield_line(
                &line_plan,
                fb,
                &mut playfield_mask,
                &mut collision_pixels,
                &mut clxdat,
                palette,
                segments,
                segment_idx,
                pixel_control,
                row_control_segments,
                control_segment_idx,
                base_controls[y].bplcon1,
                block_start,
                bpl_output_start_x,
                visible_line0,
                input.emulated_seconds,
                input.emulated_frames,
            );
            for p in 0..dma_planes {
                let m = control.modulo_for_plane(p, beam_y as i32);
                ptrs[p] = ((ptrs[p] as i64).wrapping_add(m as i64) as u32) & ram_mask;
            }
            previous_playfield_tail_words = std::array::from_fn(|plane| {
                (plane < nplanes)
                    .then(|| row_words[plane].last().copied())
                    .flatten()
            });
        }
        if let (Some(planes), Some(index)) = (export_planes.as_ref(), export_index.as_ref()) {
            let frame = input.emulated_frames;
            let dir = crate::envcfg::var("COPPERLINE_DBG_EXPORT_PLANES_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(std::env::temp_dir);
            match std::fs::create_dir_all(&dir) {
                Ok(()) => {
                    let write_pgm = |name: &str, data: &[u8]| {
                        let mut buf = format!("P5\n{EXPORT_W} {rows}\n255\n").into_bytes();
                        buf.extend_from_slice(data);
                        let path = dir.join(format!("{name}_{frame}.pgm"));
                        if let Err(err) = std::fs::write(&path, buf) {
                            log::warn!("writing plane export {}: {err}", path.display());
                        }
                    };
                    for (p, plane) in planes.iter().enumerate() {
                        write_pgm(&format!("plane{p}"), plane);
                    }
                    // Composite: scale color index to full range for visibility.
                    let scaled: Vec<u8> = index.iter().map(|&i| i.wrapping_mul(8)).collect();
                    write_pgm("composite", &scaled);
                    log::info!(
                        "exported planes for frame {frame} (secs={:.4}) to {}",
                        input.emulated_seconds,
                        dir.display(),
                    );
                }
                Err(err) => log::warn!("creating plane export dir {}: {err}", dir.display()),
            }
        }
    }
    render_timing.playfield_nanos = playfield_started.elapsed().as_nanos();
    maybe_log_frame_pixel_samples(
        "after-playfield",
        input.emulated_seconds,
        input.emulated_frames,
        fb,
        visible_line0,
    );

    let manual_bpl_started = Instant::now();
    seed_manual_bpl_segments_from_latches(
        &mut manual_bpl_segments,
        frame_start_bpldat,
        render_events,
        &base_controls,
        &control_segments,
        captured_bitplane_rows,
        visible_line0,
    );
    render_timing.manual_bpl_segments = manual_bpl_segments.len() as u64;
    render_manual_bpl_segments_with_visible_line0(
        &manual_bpl_segments,
        fb,
        &mut playfield_mask,
        &mut collision_pixels,
        &mut clxdat,
        &base_palettes,
        &palette_segments,
        &base_controls,
        &control_segments,
        &dma_output_start_x_by_line,
        visible_line0,
        input.emulated_seconds,
        input.emulated_frames,
    );
    render_timing.manual_bpl_nanos = manual_bpl_started.elapsed().as_nanos();
    maybe_log_frame_pixel_samples(
        "after-manual-bpl",
        input.emulated_seconds,
        input.emulated_frames,
        fb,
        visible_line0,
    );
    let sprite_started = Instant::now();
    clxdat |= render_sprites_with_manual_lines_and_writes(
        &state,
        frame_ram,
        fb,
        SpriteClip {
            x_start: 0,
            x_stop: FB_WIDTH,
            y_start: 0,
            y_stop: rows,
        },
        &base_palettes,
        &palette_segments,
        &base_controls,
        &control_segments,
        sprite_display_enable_x_by_y,
        &playfield_mask,
        &mut collision_pixels,
        captured_sprite_lines,
        sprite_dma_observed,
        Some(&manual_sprite_lines),
        visible_line0,
    );
    render_timing.sprite_nanos = sprite_started.elapsed().as_nanos();
    maybe_log_frame_pixel_samples(
        "after-sprites",
        input.emulated_seconds,
        input.emulated_frames,
        fb,
        visible_line0,
    );
    maybe_log_sprite_pixel_samples(
        input.emulated_seconds,
        input.emulated_frames,
        &state,
        fb,
        input.captured_sprite_lines.as_slice(),
        input.sprite_dma_observed,
        &manual_sprite_lines,
        &base_palettes,
        &palette_segments,
        &base_controls,
        &control_segments,
        sprite_display_enable_x_by_y,
        &playfield_mask,
        visible_line0,
    );
    #[cfg(any(test, debug_assertions, feature = "display-plan-trace"))]
    if let Some(display_frame_plan) = display_frame_plan.as_mut() {
        display_frame_plan
            .finish_register_and_sprite_only_lines(captured_sprite_lines, visible_line0);
        display_frame_plan.log_summary();
    }
    apply_programmable_blanking(
        input.programmable_vertical_blank,
        input.programmable_horizontal_blank,
        fb,
        visible_line0,
        rows,
    );
    blank_rows_past_frame_end(input.frame_lines, fb, visible_line0, rows);
    maybe_log_frame_pixel_samples(
        "final",
        input.emulated_seconds,
        input.emulated_frames,
        fb,
        visible_line0,
    );
    render_timing.total_nanos = render_started.elapsed().as_nanos();
    RenderResult {
        timing: render_timing,
        clxdat,
    }
}

/// Canvas rows whose beam line is at or past the frame wrap do not exist on
/// the scan: the fixed 285-row canvas is taller than a standard PAL/NTSC
/// field actually scans (lines 44..311 on PAL), and a deep-overscan display
/// window otherwise lets the playfield replay keep walking bitplane memory
/// for lines the beam never produced. Regression example: the CDTV
/// extended-ROM boot screen opens DIW to vstop $140 and relies on the frame
/// ending at line 311, which left rows for lines 312..328 showing garbage
/// fetched past the image. Hardware is in vertical blank there; force black.
fn blank_rows_past_frame_end(frame_lines: u32, fb: &mut [u32], visible_line0: i32, rows: usize) {
    const BLANK_RGBA: u32 = 0xFF00_0000;
    let frame_lines = frame_lines as i32;
    let first_blank_row = (frame_lines - visible_line0).clamp(0, rows as i32) as usize;
    for row in fb[first_blank_row * FB_WIDTH..rows * FB_WIDTH].chunks_exact_mut(FB_WIDTH) {
        row.fill(BLANK_RGBA);
    }
}

/// ECS programmable blanking (plan 1.2): force the composite blank windows
/// to black on the finished frame. Vertical blanking follows VBSTRT/VBSTOP
/// under BEAMCON0.VARVBEN; horizontal blanking follows HBSTRT/HBSTOP under
/// BEAMCON0.BLANKEN. The windows are read from the live Agnus latches rather
/// than the beam-ordered replay log: software that runs a programmable scan
/// sets them once at mode switch, so per-frame sampling is sufficient.
///
/// Comparator semantics: blank asserts at the STRT match and clears at the
/// STOP match, so a window with STRT >= STOP wraps through the frame/line
/// origin. The fixed 716x285 canvas shows beam lines visible_line0.. only;
/// blanking outside the canvas is invisible by construction.
fn apply_programmable_blanking(
    programmable_vertical_blank: Option<(u32, u32)>,
    programmable_horizontal_blank: Option<(u32, u32)>,
    fb: &mut [u32],
    visible_line0: i32,
    rows: usize,
) {
    const BLANK_RGBA: u32 = 0xFF00_0000;
    let in_window = |pos: u32, strt: u32, stop: u32| {
        if strt < stop {
            pos >= strt && pos < stop
        } else {
            pos >= strt || pos < stop
        }
    };

    if let Some((strt, stop)) = programmable_vertical_blank {
        for y in 0..rows {
            let vpos = (visible_line0 + y as i32).max(0) as u32;
            if in_window(vpos, strt, stop) {
                fb[y * FB_WIDTH..(y + 1) * FB_WIDTH].fill(BLANK_RGBA);
            }
        }
    }

    if let Some((strt, stop)) = programmable_horizontal_blank {
        // HBSTRT/HBSTOP are in colour clocks; one colour clock spans two
        // lo-res DIW positions, i.e. four hi-res framebuffer pixels.
        let mut blank_cols = [false; FB_WIDTH];
        let mut any = false;
        for (x, blank) in blank_cols.iter_mut().enumerate() {
            let diw_pos = DIW_HSTART_FB0 + (x as i32) / 2;
            let cck = (diw_pos / 2).max(0) as u32;
            if in_window(cck, strt, stop) {
                *blank = true;
                any = true;
            }
        }
        if any {
            for row in fb.chunks_exact_mut(FB_WIDTH) {
                for (x, px) in row.iter_mut().enumerate() {
                    if blank_cols[x] {
                        *px = BLANK_RGBA;
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_planned_playfield_line(
    plan: &DenisePlannedPlayfieldLine<'_>,
    fb: &mut [u32],
    playfield_mask: &mut [u8],
    collision_pixels: &mut [CollisionPixel],
    clxdat: &mut u16,
    mut palette: Palette,
    palette_segments: &[PaletteSegment],
    mut segment_idx: usize,
    mut pixel_control: ControlState,
    control_segments: &[ControlSegment],
    mut control_segment_idx: usize,
    base_scroll_bplcon1: u16,
    suppress_prefetch_scroll_fill: bool,
    bpl_output_start_x: usize,
    visible_line0: i32,
    emulated_seconds: f64,
    emulated_frames: u64,
) {
    let mut ham_color = rgb12_to_rgb24(color_rgb12(palette[0]));
    let mut next_ham_native_x = 0usize;
    let mut x = plan.x_start;
    let beam_y = visible_line0 + plan.y as i32;
    let ham_diag = ham_pixel_diag_spec().filter(|spec| {
        spec.beam_y == beam_y && emulated_seconds >= spec.after && emulated_seconds < spec.until
    });
    // The bitplane scroll (BPLCON1) feeds the bitplane shifter, so a scroll
    // write normally applies on the bitplane coordinate
    // ([`BITPLANE_CONTROL_PIPELINE_FB`] left of the copper-x where the write was
    // recorded). Once the normal output position is at or past the DIW right
    // edge, the current scanline has no visible bitplane samples left to retap;
    // keep that write in the normal register domain so it seeds following
    // scanlines without disturbing the current HAM tail.
    let mut scroll_bplcon1 = base_scroll_bplcon1;
    let mut scroll_segment_idx = 0usize;
    // Playfield-collision classification (clxcon_planes_match) depends only on
    // the bitplane index and the constant control fields (CLXCON/CLXCON2, plane
    // count, dual-playfield), so memoize it per control run as a 256-entry
    // table indexed by the sample index. This lifts a per-plane matching loop
    // out of the per-pixel path; the table is rebuilt only when those control
    // inputs change.
    let mut collision_key: Option<(u16, u16, bool, usize)> = None;
    let mut collision_table = [CollisionPixel::default(); 256];
    // The loop runs in segment-bounded chunks: control, scroll, and palette
    // segments apply at pixel boundaries (x stepping by pixel_repeat), so
    // between two boundaries every control-derived value is constant and is
    // hoisted out of the per-pixel work. The per-pixel decisions are
    // unchanged from the previous pixel-at-a-time loop.
    while x < plan.x_stop {
        while control_segment_idx < control_segments.len()
            && control_segments[control_segment_idx].x <= x
        {
            pixel_control = control_segments[control_segment_idx].control;
            control_segment_idx += 1;
        }
        let scroll_visible_x_stop = pixel_control.display_window_x().1;
        while scroll_segment_idx < control_segments.len()
            && bitplane_scroll_effect_x(
                control_segments[scroll_segment_idx].x,
                scroll_visible_x_stop,
            ) <= x
        {
            scroll_bplcon1 = control_segments[scroll_segment_idx].control.bplcon1;
            scroll_segment_idx += 1;
        }
        let mut sample_control = pixel_control;
        sample_control.bplcon1 = scroll_bplcon1;
        while segment_idx < palette_segments.len() && palette_segments[segment_idx].x <= x {
            palette_segments[segment_idx].apply(&mut palette);
            segment_idx += 1;
        }

        // First x at which a pending segment could take effect. Segments
        // land on the pixel boundary at-or-after their x, exactly as the
        // per-pixel loop applied them.
        let mut run_stop = plan.x_stop;
        if control_segment_idx < control_segments.len() {
            run_stop = run_stop.min(control_segments[control_segment_idx].x);
        }
        if scroll_segment_idx < control_segments.len() {
            run_stop = run_stop.min(bitplane_scroll_effect_x(
                control_segments[scroll_segment_idx].x,
                scroll_visible_x_stop,
            ));
        }
        if segment_idx < palette_segments.len() {
            run_stop = run_stop.min(palette_segments[segment_idx].x);
        }

        let pixel_repeat = pixel_control.framebuffer_pixel_repeat();
        let native_per_pixel = pixel_control.native_samples_per_framebuffer_pixel();
        let pixel_diw_h_start = pixel_control.diw_h_start();
        let pixel_fetch_start_native_x =
            pixel_control.fetch_start_native_x(pixel_diw_h_start, pixel_repeat);
        let native_x_offset = pixel_control.native_x_offset(pixel_diw_h_start, pixel_repeat);
        // BPLCON1 scroll fills the window's left edge from the bitplane
        // shifter. On a line whose predecessor also fetched bitplanes the
        // shifter still holds that line's tail, so the scroll-in is real
        // content. At the first line of a bitplane-DMA block (the previous
        // line was border) the shifter has no carried-over data, so the scroll
        // must not pull the leading pre-fetch words (already clocked into the
        // left border by the time the window opens) back into view. Suppress
        // those by treating fetch positions before `native_x_offset` as
        // background only on a block-start line.
        let min_fetch_x = if suppress_prefetch_scroll_fill {
            native_x_offset
        } else {
            0
        };
        let shres = pixel_control.shres();
        let (win_x_start, win_x_stop) = pixel_control.display_window_x();
        let line_visible = pixel_control.display_window_contains_line(plan.y, visible_line0);
        let background_rgb24 = rgb12_to_rgb24(color_rgb12(palette[0]));
        let nplanes = sample_control.nplanes().min(plan.plane_words.len());
        let delays = std::array::from_fn(|plane| sample_control.scroll_for_plane(plane));
        let ham_mode = sample_control.hold_and_modify();
        let ham_history_start_native_x = if ham_mode {
            pixel_control.ham_history_start_native_x(
                pixel_diw_h_start,
                pixel_repeat,
                native_x_offset,
            )
        } else {
            0
        };

        let collision_dual = pixel_control.dual_playfield();
        let collision_key_now = (
            pixel_control.clxcon,
            pixel_control.clxcon2,
            collision_dual,
            nplanes,
        );
        if collision_key != Some(collision_key_now) {
            collision_table = std::array::from_fn(|idx| {
                collision_pixel(
                    idx as u8,
                    nplanes,
                    pixel_control.clxcon,
                    pixel_control.clxcon2,
                    collision_dual,
                )
            });
            collision_key = Some(collision_key_now);
        }

        loop {
            let output_native_x = ((x - plan.x_start) / pixel_repeat) * native_per_pixel;
            let Some(relative_native_x) = output_native_x.checked_sub(pixel_fetch_start_native_x)
            else {
                x += pixel_repeat;
                if x >= run_stop {
                    break;
                }
                continue;
            };
            let native_x = relative_native_x + native_x_offset;
            let visible_sample = line_visible
                && (0..pixel_repeat).any(|dx| {
                    let pixel_x = x + dx;
                    pixel_x < plan.x_stop
                        && pixel_x >= bpl_output_start_x
                        && pixel_x >= win_x_start
                        && pixel_x < win_x_stop
                });
            if ham_mode {
                next_ham_native_x =
                    next_ham_native_x.max(ham_history_start_native_x.min(plan.fetched_pixels));
                let preroll_stop = native_x.min(plan.fetched_pixels);
                while next_ham_native_x < preroll_stop {
                    let skipped =
                        plan.sample_prepared(nplanes, &delays, min_fetch_x, next_ham_native_x);
                    denise_playfield_output(sample_control, palette, skipped.idx, &mut ham_color);
                    next_ham_native_x += 1;
                }
            }
            if !visible_sample {
                if ham_mode {
                    let sample = plan.sample_prepared(nplanes, &delays, min_fetch_x, native_x);
                    denise_playfield_output(sample_control, palette, sample.idx, &mut ham_color);
                    next_ham_native_x = next_ham_native_x.max(native_x + 1);
                } else if !shres {
                    ham_color = background_rgb24;
                    next_ham_native_x = next_ham_native_x.max(native_x + 1);
                }
                x += pixel_repeat;
                if x >= run_stop {
                    break;
                }
                continue;
            }
            let (sample, output) = if shres {
                let left = plan.sample_prepared(nplanes, &delays, min_fetch_x, native_x);
                let right = plan.sample_prepared(nplanes, &delays, min_fetch_x, native_x + 1);
                (
                    shres_composite_sample(left, right),
                    denise_shres_playfield_output(palette, left.idx, right.idx, &mut ham_color),
                )
            } else {
                let sample = plan.sample_prepared(nplanes, &delays, min_fetch_x, native_x);
                let ham_before = ham_color;
                let output =
                    denise_playfield_output(pixel_control, palette, sample.idx, &mut ham_color);
                if let Some(spec) = ham_diag {
                    if x >= spec.x_start
                        && x < spec.x_stop
                        && (x - spec.x_start).is_multiple_of(spec.step)
                    {
                        log::info!(
                            "ham-pixel secs={emulated_seconds:.4} frame={emulated_frames} y={beam_y} x={x} native={native_x} rel={} idx={:#04X} active={} ham={} before={:#08X} after={:#08X} color={:#08X} latch={:#06X} nplanes={} fetched={} delays={:?} bplcon0={:#06X} bplcon1={:#06X} diw={:#06X}/{:#06X} ddf={:#06X}/{:#06X} win={}..{}",
                            relative_native_x,
                            sample.idx,
                            u8::from(sample.active),
                            u8::from(ham_mode),
                            ham_before & 0x00FF_FFFF,
                            ham_color & 0x00FF_FFFF,
                            output.color & 0x00FF_FFFF,
                            output.color_latch,
                            nplanes,
                            plan.fetched_pixels,
                            delays,
                            pixel_control.bplcon0,
                            sample_control.bplcon1,
                            pixel_control.diwstrt,
                            pixel_control.diwstop,
                            pixel_control.ddfstrt,
                            pixel_control.ddfstop,
                            win_x_start,
                            win_x_stop,
                        );
                    }
                }
                (sample, output)
            };
            if ham_mode || !shres {
                next_ham_native_x = next_ham_native_x.max(native_x + 1);
            }
            // Collision classification is identical for every framebuffer
            // pixel of this native sample, so look it up once. CLXDAT only
            // accumulates set bits, so ORing it here (rather than once per
            // written pixel) is equivalent: a visible sample writes at least
            // one in-window pixel.
            let collision = collision_table[sample.idx as usize];
            let pf_mask = u8::from(collision.pf1) | (u8::from(collision.pf2) << 1);
            *clxdat |= collision.clxdat_bits();
            for dx in 0..pixel_repeat {
                let pixel_x = x + dx;
                if pixel_x >= plan.x_stop || pixel_x < win_x_start || pixel_x >= win_x_stop {
                    continue;
                }
                let fb_idx = plan.y * FB_WIDTH + pixel_x;
                if pf_mask != 0 {
                    playfield_mask[fb_idx] = pf_mask;
                }
                collision_pixels[fb_idx] = collision;
                let transparent =
                    pixel_control.genlock_transparent(output.color_latch, Some(sample), false);
                fb[fb_idx] = rgb24_to_rgba8_alpha(output.color, !transparent);
            }
            x += pixel_repeat;
            if x >= run_stop {
                break;
            }
        }
    }
}

#[cfg(test)]
#[cfg(test)]
fn sprite_pointer_refreshes_from_mask(mask: [bool; 8]) -> [SpritePointerRefresh; 8] {
    std::array::from_fn(|idx| SpritePointerRefresh {
        refreshed: mask[idx],
        ptr: 0,
        beam: None,
    })
}

#[cfg(test)]
fn left_edge_blank_pixels(control: ControlState) -> usize {
    let delay = control.pf1_scroll();
    if control.hires() || control.shres() {
        delay
    } else {
        delay * 2
    }
}

#[derive(Clone, Copy, Default)]
struct CollisionPixel {
    pf1: bool,
    pf2: bool,
    pf1_match: bool,
    pf2_match: bool,
}

impl CollisionPixel {
    fn clxdat_bits(self) -> u16 {
        u16::from(self.pf1_match && self.pf2_match)
    }
}

fn collision_pixel(
    idx: u8,
    nplanes: usize,
    clxcon: u16,
    clxcon2: u16,
    dual_playfield: bool,
) -> CollisionPixel {
    let even_match = clxcon_planes_match(idx, nplanes, clxcon, clxcon2, 1);
    let odd_match_raw = clxcon_planes_match(idx, nplanes, clxcon, clxcon2, 0);
    let odd_match = odd_match_raw && (dual_playfield || even_match);
    CollisionPixel {
        pf1: dual_playfield && idx & 0b010101 != 0,
        pf2: if dual_playfield {
            idx & 0b101010 != 0
        } else {
            idx != 0
        },
        pf1_match: odd_match,
        pf2_match: even_match,
    }
}

fn clxcon_planes_match(
    idx: u8,
    nplanes: usize,
    clxcon: u16,
    clxcon2: u16,
    first_plane: usize,
) -> bool {
    let mut matches = true;
    for plane in (first_plane..nplanes.min(8)).step_by(2) {
        // Planes 1-6 take their enable/match bits from CLXCON; the AGA
        // planes 7-8 from CLXCON2 (ENBP7/ENBP8 in bits 6-7, MVBP7/MVBP8 in
        // bits 0-1).
        let (enabled, desired) = if plane < 6 {
            (clxcon & (1 << (6 + plane)) != 0, clxcon & (1 << plane) != 0)
        } else {
            (
                clxcon2 & (1 << plane) != 0,
                clxcon2 & (1 << (plane - 6)) != 0,
            )
        };
        if !enabled {
            continue;
        }
        let actual = idx & (1 << plane) != 0;
        matches &= desired == actual;
    }
    matches
}

fn record_generated_playfield_collision_pixel(
    playfield_mask: &mut [u8],
    collision_pixels: &mut [CollisionPixel],
    clxdat: &mut u16,
    fb_idx: usize,
    sample: DeniseBitplaneSample,
    control: ControlState,
) {
    let collision = collision_pixel(
        sample.idx,
        sample.nplanes,
        control.clxcon,
        control.clxcon2,
        control.dual_playfield(),
    );
    let pf_mask = u8::from(collision.pf1) | (u8::from(collision.pf2) << 1);
    if pf_mask != 0 {
        playfield_mask[fb_idx] = pf_mask;
    }
    collision_pixels[fb_idx] = collision;
    *clxdat |= collision.clxdat_bits();
}

#[cfg_attr(not(test), allow(dead_code))]
fn render_manual_bpl_segments(
    segments: &[ManualBplSegment],
    fb: &mut [u32],
    playfield_mask: &mut [u8],
    collision_pixels: &mut [CollisionPixel],
    clxdat: &mut u16,
    base_palettes: &[Palette],
    palette_segments: &[Vec<PaletteSegment>],
    base_controls: &[ControlState],
    control_segments: &[Vec<ControlSegment>],
) {
    let dma_output_start_x_by_line = vec![None; base_controls.len()];
    render_manual_bpl_segments_with_visible_line0(
        segments,
        fb,
        playfield_mask,
        collision_pixels,
        clxdat,
        base_palettes,
        palette_segments,
        base_controls,
        control_segments,
        &dma_output_start_x_by_line,
        PAL_VISIBLE_LINE0,
        0.0,
        0,
    );
}

#[allow(clippy::too_many_arguments)]
fn render_manual_bpl_segments_with_visible_line0(
    segments: &[ManualBplSegment],
    fb: &mut [u32],
    playfield_mask: &mut [u8],
    collision_pixels: &mut [CollisionPixel],
    clxdat: &mut u16,
    base_palettes: &[Palette],
    palette_segments: &[Vec<PaletteSegment>],
    base_controls: &[ControlState],
    control_segments: &[Vec<ControlSegment>],
    dma_output_start_x_by_line: &[Option<usize>],
    visible_line0: i32,
    emulated_seconds: f64,
    emulated_frames: u64,
) {
    if segments.is_empty() {
        return;
    }
    let mut ham_select_pixels = vec![0u8; fb.len()];
    for seg in segments {
        if seg.line >= base_controls.len() {
            continue;
        }
        let mut ham_color = manual_bpl_ham_seed_color(
            seg,
            fb,
            base_palettes,
            palette_segments,
            base_controls,
            control_segments,
        );
        let mut ham_select = manual_bpl_ham_seed_select(seg, &ham_select_pixels);
        let beam_y = visible_line0 + seg.line as i32;
        let diag = manual_bpl_pixel_diag_spec().filter(|spec| {
            spec.beam_y == beam_y && emulated_seconds >= spec.after && emulated_seconds < spec.until
        });
        draw_manual_bpl_word(
            seg,
            fb,
            playfield_mask,
            collision_pixels,
            clxdat,
            base_palettes,
            palette_segments,
            base_controls,
            control_segments,
            dma_output_start_x_by_line,
            &mut ham_color,
            &mut ham_select,
            &mut ham_select_pixels,
            visible_line0,
            emulated_seconds,
            emulated_frames,
            diag,
        );
    }
}

fn manual_bpl_ham_seed_color(
    seg: &ManualBplSegment,
    fb: &[u32],
    base_palettes: &[Palette],
    palette_segments: &[Vec<PaletteSegment>],
    base_controls: &[ControlState],
    control_segments: &[Vec<ControlSegment>],
) -> u32 {
    if seg.line >= base_controls.len() {
        return rgb12_to_rgb24(color_rgb12(seg.palette[0]));
    }
    let sample_x = seg.x.clamp(0, FB_WIDTH.saturating_sub(1) as i32) as usize;
    let control = control_at_x(
        base_controls[seg.line],
        &control_segments[seg.line],
        sample_x,
    );
    if !control.hold_and_modify() {
        return rgb12_to_rgb24(color_rgb12(seg.palette[0]));
    }
    if seg.x <= 0 {
        return rgb12_to_rgb24(color_rgb12(
            palette_at_x(base_palettes[seg.line], &palette_segments[seg.line], 0)[0],
        ));
    }
    let previous_x = (seg.x - 1).min(FB_WIDTH.saturating_sub(1) as i32) as usize;
    rgba8_to_rgb24(fb[seg.line * FB_WIDTH + previous_x])
}

fn manual_bpl_ham_seed_select(seg: &ManualBplSegment, ham_select_pixels: &[u8]) -> u8 {
    if (seg.line + 1) * FB_WIDTH > ham_select_pixels.len() || seg.x <= 0 {
        return 0;
    }
    let previous_x = (seg.x - 1).min(FB_WIDTH.saturating_sub(1) as i32) as usize;
    ham_select_pixels[seg.line * FB_WIDTH + previous_x]
}

fn draw_manual_bpl_word(
    seg: &ManualBplSegment,
    fb: &mut [u32],
    playfield_mask: &mut [u8],
    collision_pixels: &mut [CollisionPixel],
    clxdat: &mut u16,
    base_palettes: &[Palette],
    palette_segments: &[Vec<PaletteSegment>],
    base_controls: &[ControlState],
    control_segments: &[Vec<ControlSegment>],
    dma_output_start_x_by_line: &[Option<usize>],
    ham_color: &mut u32,
    ham_select: &mut u8,
    ham_select_pixels: &mut [u8],
    visible_line0: i32,
    emulated_seconds: f64,
    emulated_frames: u64,
    diag: Option<PixelDiagSpec>,
) {
    const MANUAL_BPL_WORD_BITS: usize = 16;
    const MAX_BPLCON1_DELAY: usize = 15;
    const MAX_MANUAL_BPL_NATIVE_SAMPLES: usize = MANUAL_BPL_WORD_BITS + MAX_BPLCON1_DELAY;

    let shifter = DeniseManualBitplaneShifter::new(seg.planes, MANUAL_BPL_WORD_BITS);
    let dma_output_start_x = dma_output_start_x_by_line
        .get(seg.line)
        .copied()
        .flatten()
        .filter(|&x| seg.x < x as i32);
    let mut x_cursor = seg.x;
    let mut native_idx = 0usize;
    while native_idx < MAX_MANUAL_BPL_NATIVE_SAMPLES {
        let source_sample_x = x_cursor.clamp(0, FB_WIDTH.saturating_sub(1) as i32) as usize;
        let source_control = control_at_x(
            base_controls[seg.line],
            &control_segments[seg.line],
            source_sample_x,
        );
        let pixel_repeat = source_control.framebuffer_pixel_repeat();
        let native_step = source_control.native_samples_per_framebuffer_pixel();
        let Some(left_sample) = shifter.sample(source_control, native_idx) else {
            x_cursor += pixel_repeat as i32;
            native_idx += native_step;
            continue;
        };
        let sample = if source_control.shres() {
            let right_sample = shifter
                .sample(source_control, native_idx + 1)
                .unwrap_or_default();
            shres_composite_sample(left_sample, right_sample)
        } else {
            left_sample
        };
        let source_palette = palette_at_x(
            base_palettes[seg.line],
            &palette_segments[seg.line],
            source_sample_x,
        );
        let visible_sample = (0..pixel_repeat).any(|dx| {
            let x = x_cursor + dx as i32;
            if !(0..FB_WIDTH as i32).contains(&x) {
                return false;
            }
            let x = x as usize;
            if dma_output_start_x.is_some_and(|dma_x| x >= dma_x) {
                return false;
            }
            let pixel_control =
                control_at_x(base_controls[seg.line], &control_segments[seg.line], x);
            let (window_x_start, window_x_stop) = pixel_control.display_window_x();
            pixel_control.display_window_contains_line(seg.line, visible_line0)
                && x >= window_x_start
                && x < window_x_stop
        });
        if !visible_sample {
            if !source_control.shres() {
                *ham_color = rgb12_to_rgb24(color_rgb12(source_palette[0]));
                *ham_select = 0;
            }
            x_cursor += pixel_repeat as i32;
            native_idx += native_step;
            continue;
        }
        let ham_before = *ham_color;
        let output_idx = if source_control.hold_and_modify() {
            *ham_select
        } else {
            sample.idx
        };
        let source_output = if source_control.shres() {
            let right_sample = shifter
                .sample(source_control, native_idx + 1)
                .unwrap_or_default();
            denise_shres_playfield_output(
                source_palette,
                left_sample.idx,
                right_sample.idx,
                ham_color,
            )
        } else {
            let output =
                denise_playfield_output(source_control, source_palette, output_idx, ham_color);
            *ham_select = sample.idx;
            output
        };
        for dx in 0..pixel_repeat {
            let x = x_cursor + dx as i32;
            if !(0..FB_WIDTH as i32).contains(&x) {
                continue;
            }
            let x = x as usize;
            if dma_output_start_x.is_some_and(|dma_x| x >= dma_x) {
                continue;
            }
            let pixel_control =
                control_at_x(base_controls[seg.line], &control_segments[seg.line], x);
            let (window_x_start, window_x_stop) = pixel_control.display_window_x();
            if !pixel_control.display_window_contains_line(seg.line, visible_line0)
                || x < window_x_start
                || x >= window_x_stop
            {
                continue;
            }
            let fb_idx = seg.line * FB_WIDTH + x;
            let pixel_palette =
                palette_at_x(base_palettes[seg.line], &palette_segments[seg.line], x);
            record_generated_playfield_collision_pixel(
                playfield_mask,
                collision_pixels,
                clxdat,
                fb_idx,
                sample,
                pixel_control,
            );
            if !source_control.shres() {
                ham_select_pixels[fb_idx] = sample.idx;
            }
            if let Some(spec) = diag {
                if x >= spec.x_start
                    && x < spec.x_stop
                    && (x - spec.x_start).is_multiple_of(spec.step)
                {
                    let beam_y = visible_line0 + seg.line as i32;
                    log::info!(
                        "manual-bpl-pixel secs={emulated_seconds:.4} frame={emulated_frames} y={beam_y} x={x} seg_x={} native={} idx={:#04X} output_idx={:#04X} ham_before={:#08X} ham_after={:#08X} color={:#08X} latch={:#06X} bplcon0={:#06X} bplcon1={:#06X} win={:?}",
                        seg.x,
                        native_idx,
                        sample.idx,
                        output_idx,
                        ham_before & 0x00FF_FFFF,
                        *ham_color & 0x00FF_FFFF,
                        source_output.color & 0x00FF_FFFF,
                        source_output.color_latch,
                        source_control.bplcon0,
                        source_control.bplcon1,
                        source_control.display_window_x(),
                    );
                }
            }
            let (pixel_color, pixel_color_latch) =
                if source_control.shres() || source_control.hold_and_modify() {
                    (source_output.color, source_output.color_latch)
                } else if source_control.aga() {
                    // Re-resolve against the palette at this x so mid-line
                    // palette diffs land, mirroring the pre-AGA arms below.
                    let mut pixel_ham = *ham_color;
                    let output = denise_aga_playfield_output(
                        source_control,
                        pixel_palette,
                        sample.idx,
                        &mut pixel_ham,
                    );
                    (output.color, output.color_latch)
                } else if sample.idx == 0 {
                    (
                        rgb12_to_rgb24(color_rgb12(pixel_palette[0])),
                        pixel_palette[0],
                    )
                } else if source_control.dual_playfield() {
                    let (_, color_idx) = dual_playfield_pixel(sample.idx, source_control);
                    let color_latch = pixel_palette.get(color_idx).copied().unwrap_or(0);
                    (rgb12_to_rgb24(color_rgb12(color_latch)), color_latch)
                } else {
                    (
                        rgb12_to_rgb24(palette_index_to_rgb12(
                            pixel_palette,
                            sample.idx,
                            source_control.extra_half_brite(),
                        )),
                        pixel_palette[(sample.idx as usize) & 0x1F],
                    )
                };
            *ham_color = pixel_color;
            let transparent =
                pixel_control.genlock_transparent(pixel_color_latch, Some(sample), false);
            fb[fb_idx] = rgb24_to_rgba8_alpha(pixel_color, !transparent);
        }
        x_cursor += pixel_repeat as i32;
        native_idx += native_step;
    }
}

#[cfg(test)]
fn render_sprites(
    state: &RenderState,
    ram: &[u8],
    fb: &mut [u32],
    clip: SpriteClip,
    base_palettes: &[Palette],
    palette_segments: &[Vec<PaletteSegment>],
    base_controls: &[ControlState],
    control_segments: &[Vec<ControlSegment>],
    playfield_mask: &[u8],
    collision_pixels: &mut [CollisionPixel],
    sprite_ptr_refreshed: [bool; 8],
    captured_sprite_lines: &[CapturedSpriteLine],
    sprite_dma_observed: bool,
) -> u16 {
    #[cfg(feature = "internal-diagnostics")]
    if crate::envcfg::flag("COPPERLINE_EXP_NO_SPRITE_RENDER") {
        return 0;
    }
    render_sprites_with_manual_lines(
        state,
        ram,
        fb,
        clip,
        base_palettes,
        palette_segments,
        base_controls,
        control_segments,
        playfield_mask,
        collision_pixels,
        sprite_pointer_refreshes_from_mask(sprite_ptr_refreshed),
        captured_sprite_lines,
        sprite_dma_observed,
        None,
    )
}

#[cfg(test)]
fn render_sprites_with_manual_lines(
    state: &RenderState,
    ram: &[u8],
    fb: &mut [u32],
    clip: SpriteClip,
    base_palettes: &[Palette],
    palette_segments: &[Vec<PaletteSegment>],
    base_controls: &[ControlState],
    control_segments: &[Vec<ControlSegment>],
    playfield_mask: &[u8],
    collision_pixels: &mut [CollisionPixel],
    // Sprite pointer refreshes are no longer consumed by the renderer (captured
    // sprite DMA is authoritative); kept so existing renderer tests compile.
    _sprite_ptr_refreshes: [SpritePointerRefresh; 8],
    captured_sprite_lines: &[CapturedSpriteLine],
    sprite_dma_observed: bool,
    manual_sprite_lines: Option<&[Vec<SpriteLine>]>,
) -> u16 {
    let sprite_display_enable_x_by_y = sprite_display_enabled_from_line_start();
    render_sprites_with_manual_lines_and_writes(
        state,
        ram,
        fb,
        clip,
        base_palettes,
        palette_segments,
        base_controls,
        control_segments,
        &sprite_display_enable_x_by_y,
        playfield_mask,
        collision_pixels,
        captured_sprite_lines,
        sprite_dma_observed,
        manual_sprite_lines,
        PAL_VISIBLE_LINE0,
    )
}

fn render_sprites_with_manual_lines_and_writes(
    state: &RenderState,
    ram: &[u8],
    fb: &mut [u32],
    clip: SpriteClip,
    base_palettes: &[Palette],
    palette_segments: &[Vec<PaletteSegment>],
    base_controls: &[ControlState],
    control_segments: &[Vec<ControlSegment>],
    sprite_display_enable_x_by_y: &[Option<usize>],
    playfield_mask: &[u8],
    collision_pixels: &mut [CollisionPixel],
    captured_sprite_lines: &[CapturedSpriteLine],
    sprite_dma_observed: bool,
    manual_sprite_lines: Option<&[Vec<SpriteLine>]>,
    visible_line0: i32,
) -> u16 {
    if ram.is_empty() && !sprite_dma_observed {
        return 0;
    }

    let mut clxdat = 0u16;
    let mut sprite_group_mask = vec![0u8; fb.len()];
    let use_captured_sprite_dma = sprite_dma_observed;
    let sprite_lines: [Vec<SpriteLine>; 8] = std::array::from_fn(|sprite| {
        collect_sprite_lines(
            sprite,
            state,
            captured_sprite_lines,
            use_captured_sprite_dma,
            manual_sprite_lines,
        )
    });

    // Draw low-priority sprite pairs first so lower-numbered pairs
    // overwrite higher-numbered pairs, matching Denise's fixed sprite
    // group priority.
    for pair in (0..4).rev() {
        let even_sprite = pair * 2;
        let odd_sprite = even_sprite + 1;
        let even_lines = &sprite_lines[even_sprite];
        let odd_lines = &sprite_lines[odd_sprite];

        clxdat |= render_attached_sprite_pair_lines(
            even_sprite,
            even_lines,
            odd_lines,
            fb,
            clip,
            base_palettes,
            palette_segments,
            base_controls,
            control_segments,
            sprite_display_enable_x_by_y,
            playfield_mask,
            collision_pixels,
            &mut sprite_group_mask,
            visible_line0,
        );
        clxdat |= render_unattached_sprite_pair_lines(
            even_sprite,
            even_lines,
            odd_lines,
            fb,
            clip,
            base_palettes,
            palette_segments,
            base_controls,
            control_segments,
            sprite_display_enable_x_by_y,
            playfield_mask,
            collision_pixels,
            &mut sprite_group_mask,
            visible_line0,
        );
    }
    clxdat
}

fn render_unattached_sprite_pair_lines(
    even_sprite: usize,
    even_lines: &[SpriteLine],
    odd_lines: &[SpriteLine],
    fb: &mut [u32],
    clip: SpriteClip,
    base_palettes: &[Palette],
    palette_segments: &[Vec<PaletteSegment>],
    base_controls: &[ControlState],
    control_segments: &[Vec<ControlSegment>],
    sprite_display_enable_x_by_y: &[Option<usize>],
    playfield_mask: &[u8],
    collision_pixels: &mut [CollisionPixel],
    sprite_group_mask: &mut [u8],
    visible_line0: i32,
) -> u16 {
    let mut clxdat = 0u16;
    let odd_sprite = even_sprite + 1;
    clxdat |= render_collected_sprite_lines(
        odd_sprite,
        odd_lines,
        |line| !sprite_pair_attach_active_for_beam(even_lines, odd_lines, line.beam_y),
        fb,
        clip,
        base_palettes,
        palette_segments,
        base_controls,
        control_segments,
        sprite_display_enable_x_by_y,
        playfield_mask,
        collision_pixels,
        sprite_group_mask,
        visible_line0,
    );
    for even in even_lines {
        if sprite_pair_attach_active_for_beam(even_lines, odd_lines, even.beam_y) {
            continue;
        }
        clxdat |= draw_sprite_line(
            even_sprite,
            even,
            fb,
            clip,
            base_palettes,
            palette_segments,
            base_controls,
            control_segments,
            sprite_display_enable_x_by_y,
            playfield_mask,
            collision_pixels,
            sprite_group_mask,
            visible_line0,
        );
    }
    clxdat
}

fn render_collected_sprite_lines<F>(
    sprite: usize,
    lines: &[SpriteLine],
    mut include_line: F,
    fb: &mut [u32],
    clip: SpriteClip,
    base_palettes: &[Palette],
    palette_segments: &[Vec<PaletteSegment>],
    base_controls: &[ControlState],
    control_segments: &[Vec<ControlSegment>],
    sprite_display_enable_x_by_y: &[Option<usize>],
    playfield_mask: &[u8],
    collision_pixels: &mut [CollisionPixel],
    sprite_group_mask: &mut [u8],
    visible_line0: i32,
) -> u16
where
    F: FnMut(&SpriteLine) -> bool,
{
    let mut clxdat = 0u16;
    for line in lines {
        if !include_line(line) {
            continue;
        }
        clxdat |= draw_sprite_line(
            sprite,
            line,
            fb,
            clip,
            base_palettes,
            palette_segments,
            base_controls,
            control_segments,
            sprite_display_enable_x_by_y,
            playfield_mask,
            collision_pixels,
            sprite_group_mask,
            visible_line0,
        );
    }
    clxdat
}

fn render_attached_sprite_pair_lines(
    even_sprite: usize,
    even_lines: &[SpriteLine],
    odd_lines: &[SpriteLine],
    fb: &mut [u32],
    clip: SpriteClip,
    base_palettes: &[Palette],
    palette_segments: &[Vec<PaletteSegment>],
    base_controls: &[ControlState],
    control_segments: &[Vec<ControlSegment>],
    sprite_display_enable_x_by_y: &[Option<usize>],
    playfield_mask: &[u8],
    collision_pixels: &mut [CollisionPixel],
    sprite_group_mask: &mut [u8],
    visible_line0: i32,
) -> u16 {
    let mut clxdat = 0u16;
    let mut beams: Vec<i32> = even_lines
        .iter()
        .chain(odd_lines.iter())
        .filter(|line| line.attached)
        .map(|line| line.beam_y)
        .collect();
    beams.sort_unstable();
    beams.dedup();

    for beam_y in beams {
        let y = beam_y - visible_line0;
        if y < 0 || y >= base_controls.len() as i32 {
            continue;
        }
        let y = y as usize;
        if y < clip.y_start || y >= clip.y_stop {
            continue;
        }

        let even_beam_lines: Vec<SpriteLineSampler<'_>> = even_lines
            .iter()
            .filter(|line| line.beam_y == beam_y)
            .map(|line| SpriteLineSampler::new(line, base_controls[y], &control_segments[y]))
            .collect();
        let odd_beam_lines: Vec<SpriteLineSampler<'_>> = odd_lines
            .iter()
            .filter(|line| line.beam_y == beam_y)
            .map(|line| SpriteLineSampler::new(line, base_controls[y], &control_segments[y]))
            .collect();

        let mut x_start = FB_WIDTH as i32;
        let mut x_stop = 0i32;
        for line in even_beam_lines.iter().chain(odd_beam_lines.iter()) {
            if let Some((start, stop)) = line.framebuffer_range() {
                x_start = x_start.min(start);
                x_stop = x_stop.max(stop);
            }
        }
        x_start = x_start.max(clip.x_start as i32);
        x_stop = x_stop.min(clip.x_stop as i32);
        if x_start >= x_stop {
            continue;
        }

        for x in x_start..x_stop {
            let x_usize = x as usize;
            let even_idx = sprite_line_samplers_pixel_bits_at(&even_beam_lines, x);
            let odd_idx = sprite_line_samplers_pixel_bits_at(&odd_beam_lines, x);
            let idx = even_idx | (odd_idx << 2);
            if idx == 0 {
                continue;
            }
            let control = control_at_x(base_controls[y], &control_segments[y], x_usize);
            if !sprite_pixel_inside_display_window(
                control,
                y,
                x_usize,
                visible_line0,
                sprite_display_enable_x_for_y(sprite_display_enable_x_by_y, y),
            ) {
                continue;
            }
            let fb_idx = y * FB_WIDTH + x_usize;
            clxdat |= generated_sprite_pair_collision_bits(
                even_sprite,
                fb_idx,
                control.clxcon,
                even_idx != 0,
                odd_idx != 0,
                sprite_group_mask,
                collision_pixels,
                playfield_mask,
            );
            if !sprite_has_priority(even_sprite, playfield_mask[fb_idx], control) {
                continue;
            }
            let palette = palette_at_x(base_palettes[y], &palette_segments[y], x_usize);
            let color_idx = sprite_color_entry(control, even_sprite, idx, true);
            let color_latch = palette[color_idx];
            let transparent = control.genlock_transparent(color_latch, None, false);
            let color = if control.aga() {
                palette.rgb24(color_idx) & 0x00FF_FFFF
            } else {
                rgb12_to_rgb24(color_rgb12(color_latch))
            };
            fb[fb_idx] = rgb24_to_rgba8_alpha(color, !transparent);
        }
    }
    clxdat
}

fn sprite_pair_attach_active_for_beam(
    even_lines: &[SpriteLine],
    odd_lines: &[SpriteLine],
    beam_y: i32,
) -> bool {
    even_lines
        .iter()
        .chain(odd_lines.iter())
        .any(|line| line.beam_y == beam_y && line.attached)
}

fn sprite_lines_pixel_bits_at(
    lines: &[SpriteLine],
    beam_y: i32,
    y: usize,
    x: i32,
    base_controls: &[ControlState],
    control_segments: &[Vec<ControlSegment>],
) -> u8 {
    lines
        .iter()
        .filter(|line| line.beam_y == beam_y)
        .find_map(|line| {
            let idx = sprite_line_pixel_bits_at(line, x, base_controls[y], &control_segments[y]);
            (idx != 0).then_some(idx)
        })
        .unwrap_or(0)
}

fn sprite_line_samplers_pixel_bits_at(lines: &[SpriteLineSampler<'_>], x: i32) -> u8 {
    lines
        .iter()
        .find_map(|line| {
            let idx = line.pixel_bits_at(x);
            (idx != 0).then_some(idx)
        })
        .unwrap_or(0)
}

fn sprite_line_pixel_bits_at(
    line: &SpriteLine,
    x: i32,
    base_control: ControlState,
    control_segments: &[ControlSegment],
) -> u8 {
    if x < line.x_start as i32 || x >= line.x_stop as i32 {
        return 0;
    }
    let base_x =
        sprite_base_framebuffer_x(line.hstart, line.hsub_70ns, base_control, control_segments);
    let mut x_cursor = base_x;
    for w in 0..line.width_words() {
        let (data, datb) = line.word(w);
        for bit in (0..16).rev() {
            let sample_x = x_cursor.clamp(0, FB_WIDTH.saturating_sub(1) as i32) as usize;
            let sprite_pixel_repeat =
                control_at_x(base_control, control_segments, sample_x).sprite_pixel_repeat();
            let x_stop = x_cursor + sprite_pixel_repeat;
            if x >= x_cursor && x < x_stop {
                let lo = u8::from(data & (1 << bit) != 0);
                let hi = u8::from(datb & (1 << bit) != 0);
                return lo | (hi << 1);
            }
            x_cursor = x_stop;
        }
    }
    0
}

fn collect_captured_sprite_lines(
    sprite: usize,
    captured_sprite_lines: &[CapturedSpriteLine],
) -> Vec<SpriteLine> {
    captured_sprite_lines
        .iter()
        .filter(|line| line.sprite == sprite)
        .map(|line| SpriteLine {
            hstart: line.hstart,
            hsub_70ns: line.hsub_70ns,
            beam_y: line.beam_y,
            data: line.data,
            datb: line.datb,
            data_ext: line.data_ext,
            datb_ext: line.datb_ext,
            width_words: line.width_words,
            attached: line.attached,
            x_start: 0,
            x_stop: FB_WIDTH,
        })
        .collect()
}

fn clip_sprite_lines_around_register_lines(
    lines: &mut Vec<SpriteLine>,
    register_lines: &[SpriteLine],
) {
    if lines.is_empty() || register_lines.is_empty() {
        return;
    }

    let mut clipped = Vec::with_capacity(lines.len());
    for line in lines.drain(..) {
        let mut segments = vec![(line.x_start, line.x_stop)];
        for register_line in register_lines
            .iter()
            .filter(|register_line| register_line.beam_y == line.beam_y)
        {
            let mask_start = register_line.x_start.max(line.x_start);
            let mask_stop = register_line.x_stop.min(line.x_stop);
            if mask_start >= mask_stop {
                continue;
            }
            let mut next_segments = Vec::new();
            for (start, stop) in segments {
                if start < mask_start {
                    next_segments.push((start, mask_start));
                }
                if mask_stop < stop {
                    next_segments.push((mask_stop, stop));
                }
            }
            segments = next_segments;
            if segments.is_empty() {
                break;
            }
        }
        for (x_start, x_stop) in segments {
            let mut segment = line;
            segment.x_start = x_start;
            segment.x_stop = x_stop;
            clipped.push(segment);
        }
    }
    *lines = clipped;
}

#[allow(clippy::too_many_arguments)]
fn draw_sprite_line(
    sprite: usize,
    line: &SpriteLine,
    fb: &mut [u32],
    clip: SpriteClip,
    base_palettes: &[Palette],
    palette_segments: &[Vec<PaletteSegment>],
    base_controls: &[ControlState],
    control_segments: &[Vec<ControlSegment>],
    sprite_display_enable_x_by_y: &[Option<usize>],
    playfield_mask: &[u8],
    collision_pixels: &mut [CollisionPixel],
    sprite_group_mask: &mut [u8],
    visible_line0: i32,
) -> u16 {
    let y = line.beam_y - visible_line0;
    if y < 0 || y >= base_controls.len() as i32 {
        return 0;
    }
    let y = y as usize;
    if y < clip.y_start || y >= clip.y_stop {
        return 0;
    }
    let base_x = sprite_base_framebuffer_x(
        line.hstart,
        line.hsub_70ns,
        base_controls[y],
        &control_segments[y],
    );
    let mut clxdat = 0u16;
    let mut x_cursor = base_x;

    for w in 0..line.width_words() {
        let (data, datb) = line.word(w);
        for bit in (0..16).rev() {
            let sample_x = x_cursor.clamp(0, FB_WIDTH.saturating_sub(1) as i32) as usize;
            let sprite_pixel_repeat =
                control_at_x(base_controls[y], &control_segments[y], sample_x)
                    .sprite_pixel_repeat();
            let lo = u8::from(data & (1 << bit) != 0);
            let hi = u8::from(datb & (1 << bit) != 0);
            let idx = lo | (hi << 1);
            if idx == 0 {
                x_cursor += sprite_pixel_repeat;
                continue;
            }
            for dx in 0..sprite_pixel_repeat {
                let x = x_cursor + dx;
                if x < 0 || x >= FB_WIDTH as i32 {
                    continue;
                }
                let x = x as usize;
                if x < clip.x_start || x >= clip.x_stop || x < line.x_start || x >= line.x_stop {
                    continue;
                }
                let fb_idx = y * FB_WIDTH + x;
                let control = control_at_x(base_controls[y], &control_segments[y], x);
                if !sprite_pixel_inside_display_window(
                    control,
                    y,
                    x,
                    visible_line0,
                    sprite_display_enable_x_for_y(sprite_display_enable_x_by_y, y),
                ) {
                    continue;
                }
                clxdat |= generated_sprite_collision_bits(
                    sprite,
                    fb_idx,
                    control.clxcon,
                    sprite_group_mask,
                    collision_pixels,
                    playfield_mask,
                );
                if !sprite_has_priority(sprite, playfield_mask[fb_idx], control) {
                    continue;
                }
                let color_idx = sprite_color_entry(control, sprite, idx, false);
                let palette = palette_at_x(base_palettes[y], &palette_segments[y], x);
                let color_latch = palette[color_idx];
                let transparent = control.genlock_transparent(color_latch, None, false);
                let color = if control.aga() {
                    palette.rgb24(color_idx) & 0x00FF_FFFF
                } else {
                    rgb12_to_rgb24(color_rgb12(color_latch))
                };
                fb[fb_idx] = rgb24_to_rgba8_alpha(color, !transparent);
            }
            x_cursor += sprite_pixel_repeat;
        }
    }
    clxdat
}

fn collect_sprite_lines(
    sprite: usize,
    state: &RenderState,
    captured_sprite_lines: &[CapturedSpriteLine],
    use_captured_sprite_dma: bool,
    manual_sprite_lines: Option<&[Vec<SpriteLine>]>,
) -> Vec<SpriteLine> {
    let sprite_dma_blocked_by_ddf = sprite_dma_disabled_by_bitplane_ddf(
        sprite,
        state.agnus_revision,
        state.bplcon0,
        state.fmode,
        state.dmacon,
        state.ddfstrt,
        state.ddfstop,
        state.harddis,
    );
    let mut lines = Vec::new();

    if use_captured_sprite_dma && !sprite_dma_blocked_by_ddf {
        lines.extend(collect_captured_sprite_lines(sprite, captured_sprite_lines));
    }

    if let Some(register_lines) = manual_sprite_lines.and_then(|lines| lines.get(sprite)) {
        clip_sprite_lines_around_register_lines(&mut lines, register_lines);
        lines.extend_from_slice(register_lines);
        return lines;
    }

    // With no captured sprite DMA for this frame, render any armed sprites
    // from their latched registers (CPU-driven sprites); captured DMA is the
    // source whenever Agnus actually fetched sprite data.
    if !use_captured_sprite_dma {
        lines.extend(register_latched_sprite_lines(sprite, state));
    }
    lines
}

fn register_latched_sprite_lines(sprite: usize, state: &RenderState) -> Vec<SpriteLine> {
    if !state.spr_armed[sprite] {
        return Vec::new();
    }
    let regs = BeamSpriteState::from_render_state(state, &[None; 8]);
    let pos = regs.sprpos[sprite];
    let ctl = regs.sprctl[sprite];
    (sprite_vstart(pos, ctl)..sprite_vstop(ctl))
        .filter_map(|beam_y| regs.line_for_sprite(sprite, beam_y, 0, FB_WIDTH))
        .collect()
}

fn sprite_has_priority(sprite: usize, playfield: u8, control: ControlState) -> bool {
    if playfield == 0 {
        return true;
    }
    let group = (sprite / 2) as u8;
    let priority = control.playfield_priority_code(playfield);
    group < priority.min(4)
}

fn sprite_base_framebuffer_x(
    hstart: i32,
    hsub_70ns: bool,
    base_control: ControlState,
    control_segments: &[ControlSegment],
) -> i32 {
    let base_x = (hstart - DIW_HSTART_FB0) * 2;
    let sample_x = base_x.clamp(0, FB_WIDTH.saturating_sub(1) as i32) as usize;
    let control = control_at_x(base_control, control_segments, sample_x);
    base_x + i32::from(hsub_70ns && control.shres())
}

fn sprite_nominal_base_framebuffer_x(pos: u16, ctl: u16) -> i32 {
    (sprite_hstart(pos, ctl) - DIW_HSTART_FB0) * 2 + i32::from(sprite_hsub_70ns(ctl))
}

fn sprite_display_enable_x_for_y(
    sprite_display_enable_x_by_y: &[Option<usize>],
    y: usize,
) -> Option<usize> {
    if y < sprite_display_enable_x_by_y.len() {
        sprite_display_enable_x_by_y[y]
    } else {
        Some(0)
    }
}

fn sprite_pixel_inside_display_window(
    control: ControlState,
    _y: usize,
    x: usize,
    _visible_line0: i32,
    display_enable_x: Option<usize>,
) -> bool {
    if control.border_sprite_enabled() {
        return true;
    }
    let Some(enable_x) = display_enable_x else {
        return false;
    };
    if x < enable_x {
        return false;
    }
    // OCS/ECS Denise clips normal sprites to the horizontal display window.
    // Bitplane DMA opens that gate at DIW's left edge even when DDFSTRT delays
    // the first playfield word; a manual BPL1DAT write can still open it on a
    // scanline where the vertical bitplane window is closed.
    let (x_start, x_stop) = control.display_window_x();
    x >= x_start && x < x_stop
}

fn generated_sprite_collision_bits(
    sprite: usize,
    fb_idx: usize,
    clxcon: u16,
    sprite_group_mask: &mut [u8],
    collision_pixels: &mut [CollisionPixel],
    _playfield_mask: &[u8],
) -> u16 {
    let group = sprite / 2;
    if sprite & 1 != 0 && clxcon & (1 << (12 + group)) == 0 {
        return 0;
    }
    let bit = 1u8 << group;
    let mut clxdat = 0u16;
    let prior_sprites = sprite_group_mask[fb_idx];
    if prior_sprites != 0 {
        for other in 0..4 {
            if prior_sprites & (1 << other) != 0 && other != group {
                clxdat |= sprite_sprite_clx_bit(group, other);
            }
        }
    }
    let collision = collision_pixels[fb_idx];
    if collision.pf1_match {
        clxdat |= 1 << (group + 1);
    }
    if collision.pf2_match {
        clxdat |= 1 << (group + 5);
    }
    sprite_group_mask[fb_idx] |= bit;
    clxdat
}

fn generated_sprite_pair_collision_bits(
    even_sprite: usize,
    fb_idx: usize,
    clxcon: u16,
    even_opaque: bool,
    odd_opaque: bool,
    sprite_group_mask: &mut [u8],
    collision_pixels: &mut [CollisionPixel],
    _playfield_mask: &[u8],
) -> u16 {
    let group = even_sprite / 2;
    // CLXCON bits 12..15 (ENSP1/3/5/7) gate the odd sprite of each pair
    // into collision detection.
    let odd_collides = odd_opaque && clxcon & (1 << (12 + group)) != 0;
    if !even_opaque && !odd_collides {
        return 0;
    }
    let bit = 1u8 << group;
    let mut clxdat = 0u16;
    let prior_sprites = sprite_group_mask[fb_idx];
    if prior_sprites != 0 {
        for other in 0..4 {
            if prior_sprites & (1 << other) != 0 && other != group {
                clxdat |= sprite_sprite_clx_bit(group, other);
            }
        }
    }
    let collision = collision_pixels[fb_idx];
    if collision.pf1_match {
        clxdat |= 1 << (group + 1);
    }
    if collision.pf2_match {
        clxdat |= 1 << (group + 5);
    }
    sprite_group_mask[fb_idx] |= bit;
    clxdat
}

fn sprite_sprite_clx_bit(a: usize, b: usize) -> u16 {
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    match (lo, hi) {
        (0, 1) => 1 << 9,
        (0, 2) => 1 << 10,
        (0, 3) => 1 << 11,
        (1, 2) => 1 << 12,
        (1, 3) => 1 << 13,
        (2, 3) => 1 << 14,
        _ => 0,
    }
}

fn sprite_vstart(pos: u16, ctl: u16) -> i32 {
    (((pos >> 8) & 0x00FF) | ((ctl & 0x0004) << 6)) as i32
}

fn sprite_vstop(ctl: u16) -> i32 {
    (((ctl >> 8) & 0x00FF) | ((ctl & 0x0002) << 7)) as i32
}

fn sprite_hstart(pos: u16, ctl: u16) -> i32 {
    (((pos & 0x00FF) << 1) | (ctl & 0x0001)) as i32
}

fn sprite_hsub_70ns(ctl: u16) -> bool {
    ctl & 0x0010 != 0
}

fn sprite_control_words_from_parts(
    vstart: i32,
    vstop: i32,
    hstart: i32,
    hsub_70ns: bool,
    attached: bool,
) -> (u16, u16) {
    let vstart = vstart as u16;
    let vstop = vstop as u16;
    let hstart = hstart as u16;
    let pos = ((vstart & 0x00FF) << 8) | ((hstart >> 1) & 0x00FF);
    let mut ctl = ((vstop & 0x00FF) << 8)
        | ((vstart & 0x0100) >> 6)
        | ((vstop & 0x0100) >> 7)
        | (hstart & 0x0001);
    if hsub_70ns {
        ctl |= 0x0010;
    }
    if attached {
        ctl |= 0x0080;
    }
    (pos, ctl)
}

fn read_chip_word_wrapping(ram: &[u8], addr: u32) -> u16 {
    let mask = ram.len() - 1;
    let a = addr as usize & mask;
    u16::from_be_bytes([ram[a], ram[(a + 1) & mask]])
}

fn ddf_register_mask(revision: AgnusRevision) -> u16 {
    if matches!(revision, AgnusRevision::Ocs) {
        0x00FC
    } else {
        0x00FE
    }
}

fn effective_ddf_start_hpos_raw(revision: AgnusRevision, hires: bool, raw: u16) -> u16 {
    let _ = hires;
    raw & ddf_register_mask(revision)
}

fn effective_ddf_stop_hpos(revision: AgnusRevision, hires: bool, raw: u16) -> u16 {
    let _ = hires;
    raw & ddf_register_mask(revision)
}

fn effective_ddf_start_hpos(revision: AgnusRevision, hires: bool, raw: u16) -> u16 {
    let start = effective_ddf_start_hpos_raw(revision, hires, raw);
    if start == 0 {
        0
    } else {
        start.clamp(BITPLANE_DDF_HARD_START, BITPLANE_DDF_HARD_STOP)
    }
}

fn effective_ddf_window(
    revision: AgnusRevision,
    hires: bool,
    ddfstrt: u16,
    ddfstop: u16,
    harddis: bool,
) -> Option<(u16, u16)> {
    let (hard_start, hard_stop) = ddf_hard_bounds(harddis);
    let start = effective_ddf_start_hpos_raw(revision, hires, ddfstrt);
    let mut stop = effective_ddf_stop_hpos(revision, hires, ddfstop);
    if start == 0 || start > hard_stop {
        return None;
    }
    if matches!(revision, AgnusRevision::Ocs) && stop == start {
        stop = hard_stop;
    }
    let start = start.max(hard_start);
    let stop = stop.min(hard_stop);
    (stop >= start).then_some((start, stop))
}

fn palette_at_x(mut palette: Palette, segments: &[PaletteSegment], x: usize) -> Palette {
    for seg in segments {
        if seg.x > x {
            break;
        }
        seg.apply(&mut palette);
    }
    palette
}

fn control_at_x(mut control: ControlState, segments: &[ControlSegment], x: usize) -> ControlState {
    for seg in segments {
        if seg.x > x {
            break;
        }
        control = seg.control;
    }
    control
}

#[cfg_attr(not(test), allow(dead_code))]
fn fill_background(
    fb: &mut [u32],
    base_palettes: &[Palette],
    palette_segments: &[Vec<PaletteSegment>],
    base_controls: &[ControlState],
    control_segments: &[Vec<ControlSegment>],
) {
    fill_background_with_visible_line0(
        fb,
        base_palettes,
        palette_segments,
        base_controls,
        control_segments,
        PAL_VISIBLE_LINE0,
    );
}

/// The background colour for one pixel given the latched control/palette
/// state and whether the pixel is in the border. `sample` is always `None`
/// for a background pixel, so the result depends only on these three
/// inputs -- which is what lets [`fill_background_with_visible_line0`] fill
/// constant runs instead of recomputing per pixel.
fn background_pixel(control: &ControlState, color0: u16, border: bool) -> u32 {
    let color_latch = if control.border_blank_enabled() && border {
        0
    } else {
        color0
    };
    let transparent = control.genlock_transparent(color_latch, None, border);
    rgb12_to_rgba8_alpha(color_rgb12(color_latch), !transparent)
}

fn fill_background_with_visible_line0(
    fb: &mut [u32],
    base_palettes: &[Palette],
    palette_segments: &[Vec<PaletteSegment>],
    base_controls: &[ControlState],
    control_segments: &[Vec<ControlSegment>],
    visible_line0: i32,
) {
    for y in 0..base_palettes.len() {
        let row = &mut fb[y * FB_WIDTH..(y + 1) * FB_WIDTH];
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
                row[x..run_end].fill(background_pixel(&control, color0, true));
                x = run_end;
                continue;
            }
            // In the vertical window: border holds outside [x_start, x_stop).
            let (x_start, x_stop) = control.display_window_x();
            let mut sx = x;
            while sx < run_end {
                let border = sx < x_start || sx >= x_stop;
                let flip = if sx < x_start {
                    x_start
                } else if sx < x_stop {
                    x_stop
                } else {
                    FB_WIDTH
                };
                let sub_end = flip.min(run_end).max(sx + 1);
                row[sx..sub_end].fill(background_pixel(&control, color0, border));
                sx = sub_end;
            }
            x = run_end;
        }
    }
}

fn rgb12_to_rgba8_alpha(c: u16, opaque: bool) -> u32 {
    let rgba = rgb12_to_rgba8(c);
    if opaque {
        rgba
    } else {
        rgba & 0x00FF_FFFF
    }
}

fn rgb24_to_rgba8_alpha(c: u32, opaque: bool) -> u32 {
    let rgba = rgb24_to_rgba8(c);
    if opaque {
        rgba
    } else {
        rgba & 0x00FF_FFFF
    }
}

/// Framebuffer RGBA back to 24-bit 0x00RRGGBB (HAM seeding from an already
/// rendered pixel).
fn rgba8_to_rgb24(c: u32) -> u32 {
    let r = c & 0xFF;
    let g = (c >> 8) & 0xFF;
    let b = (c >> 16) & 0xFF;
    (r << 16) | (g << 8) | b
}

/// High nibbles of a 24-bit colour as a 12-bit word. Exact inverse of
/// rgb12_to_rgb24 for nibble-duplicated values, used to keep the OCS HAM6
/// maths in its native 12-bit space while the pipeline carries 24-bit.
fn rgb24_to_rgb12_hi(c: u32) -> u16 {
    let r = ((c >> 20) & 0xF) as u16;
    let g = ((c >> 12) & 0xF) as u16;
    let b = ((c >> 4) & 0xF) as u16;
    (r << 8) | (g << 4) | b
}

fn color_rgb12(color_latch: u16) -> u16 {
    color_latch & COLOR_RGB_MASK
}

fn palette_index_to_rgb12(palette: Palette, idx: u8, extra_half_brite: bool) -> u16 {
    let color = color_rgb12(palette[(idx as usize) & 0x1F]);
    if extra_half_brite && idx & 0x20 != 0 {
        half_brite_rgb12(color)
    } else {
        color
    }
}

fn shres_composite_sample(
    left: DeniseBitplaneSample,
    right: DeniseBitplaneSample,
) -> DeniseBitplaneSample {
    DeniseBitplaneSample {
        idx: (left.idx | right.idx) & 0x03,
        nplanes: left.nplanes.max(right.nplanes).min(2),
        active: left.active || right.active,
    }
}

fn shres_palette_index(left_idx: u8, right_idx: u8) -> usize {
    ((left_idx as usize) & 0x03) | (((right_idx as usize) & 0x03) << 2)
}

fn denise_shres_playfield_output(
    palette: Palette,
    left_idx: u8,
    right_idx: u8,
    ham_color: &mut u32,
) -> DenisePlayfieldOutput {
    let color_idx = shres_palette_index(left_idx, right_idx);
    let color_latch = palette[color_idx];
    let color = rgb12_to_rgb24(color_rgb12(color_latch));
    *ham_color = color;
    DenisePlayfieldOutput {
        color,
        color_latch,
        pf_mask: u8::from((left_idx | right_idx) & 0x03 != 0) * 2,
    }
}

fn denise_playfield_output(
    control: ControlState,
    palette: Palette,
    idx: u8,
    ham_color: &mut u32,
) -> DenisePlayfieldOutput {
    if control.aga() {
        return denise_aga_playfield_output(control, palette, idx, ham_color);
    }

    if control.hold_and_modify() {
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
        pf_mask: u8::from(idx != 0) * 2,
    }
}

/// Lisa pixel resolution: 24-bit colours from the banked palette, BPLCON4
/// BPLAM XOR applied to the full pixel index, HAM8 with 8 bitplanes (HAM
/// with 5/6 planes keeps the OCS-compatible HAM6 maths on the high
/// nibbles), and EHB halving in 8-bit component space.
fn denise_aga_playfield_output(
    control: ControlState,
    palette: Palette,
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
fn ham8_rgb24(palette: Palette, idx: u8, previous: u32) -> u32 {
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
fn dual_playfield_palette_index(idx: u8, control: ControlState) -> usize {
    dual_playfield_pixel(idx, control).1
}

fn dual_playfield_pixel(idx: u8, control: ControlState) -> (u8, usize) {
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
    match (pf1, pf2) {
        (0, 0) => (0, 0),
        (pf, 0) => (1, pf as usize),
        (0, pf) => (2, pf2_offset + pf as usize),
        (_, pf2) if control.pf2_priority() => (2, pf2_offset + pf2 as usize),
        (pf1, _) => (1, pf1 as usize),
    }
}

fn half_brite_rgb12(color: u16) -> u16 {
    let r = ((color >> 8) & 0x0F) >> 1;
    let g = ((color >> 4) & 0x0F) >> 1;
    let b = (color & 0x0F) >> 1;
    (r << 8) | (g << 4) | b
}

fn ham6_rgb12(palette: Palette, idx: u8, previous: u16) -> u16 {
    let data = (idx & 0x0F) as u16;
    match idx >> 4 {
        0 => color_rgb12(palette[data as usize]),
        1 => (previous & 0x0FF0) | data,
        2 => (previous & 0x00FF) | (data << 8),
        _ => (previous & 0x0F0F) | (data << 4),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{BeamRegisterWrite, BeamWriteSource};

    #[test]
    fn programmable_blanking_blanks_vbstrt_vbstop_rows_under_varvben() {
        use crate::chipset::agnus::{
            Agnus, AgnusRevision, VideoStandard, BEAMCON0_PAL, BEAMCON0_VARVBEN,
        };

        let mut agnus =
            Agnus::with_video_standard_and_revision(VideoStandard::Pal, AgnusRevision::Ecs8372Rev4);
        agnus.write_beamcon0(BEAMCON0_PAL | BEAMCON0_VARVBEN);
        // Blank beam lines 0x50..0x58 (rows 0x24..0x2C of the canvas).
        agnus.write_vbstrt(0x50);
        agnus.write_vbstop(0x58);

        const FILL: u32 = 0xFFAA_BBCC;
        let mut fb = vec![FILL; FB_PIXELS];
        apply_programmable_blanking(
            agnus.programmable_vertical_blank(),
            agnus.programmable_horizontal_blank(),
            &mut fb,
            PAL_VISIBLE_LINE0,
            FB_HEIGHT,
        );

        let row = |fb: &[u32], y: usize| fb[y * FB_WIDTH];
        assert_eq!(row(&fb, 0x23), FILL, "line before VBSTRT untouched");
        assert_eq!(row(&fb, 0x24), 0xFF00_0000, "VBSTRT line blanked");
        assert_eq!(row(&fb, 0x2B), 0xFF00_0000, "last blanked line");
        assert_eq!(row(&fb, 0x2C), FILL, "VBSTOP line shows again");

        // Without VARVBEN the window is ignored.
        agnus.write_beamcon0(BEAMCON0_PAL);
        let mut fb = vec![FILL; FB_PIXELS];
        apply_programmable_blanking(
            agnus.programmable_vertical_blank(),
            agnus.programmable_horizontal_blank(),
            &mut fb,
            PAL_VISIBLE_LINE0,
            FB_HEIGHT,
        );
        assert_eq!(row(&fb, 0x24), FILL);
    }

    #[test]
    fn programmable_blanking_blanks_hbstrt_hbstop_columns_under_blanken() {
        use crate::chipset::agnus::{
            Agnus, AgnusRevision, VideoStandard, BEAMCON0_BLANKEN, BEAMCON0_PAL,
        };

        let mut agnus =
            Agnus::with_video_standard_and_revision(VideoStandard::Pal, AgnusRevision::Ecs8372Rev4);
        agnus.write_beamcon0(BEAMCON0_PAL | BEAMCON0_BLANKEN);
        // Blank colour clocks 0x40..0x48. CCK 0x40 = DIW position 0x80 =
        // framebuffer x (0x80 - DIW_HSTART_FB0) * 2 = 0x3E.
        agnus.write_hbstrt(0x40);
        agnus.write_hbstop(0x48);

        const FILL: u32 = 0xFFAA_BBCC;
        let mut fb = vec![FILL; FB_PIXELS];
        apply_programmable_blanking(
            agnus.programmable_vertical_blank(),
            agnus.programmable_horizontal_blank(),
            &mut fb,
            PAL_VISIBLE_LINE0,
            FB_HEIGHT,
        );

        let x_first = ((0x80 - DIW_HSTART_FB0) * 2) as usize;
        let x_last = ((0x90 - DIW_HSTART_FB0) * 2) as usize - 1;
        assert_eq!(fb[x_first - 1], FILL, "pixel before HBSTRT untouched");
        assert_eq!(fb[x_first], 0xFF00_0000, "HBSTRT pixel blanked");
        assert_eq!(fb[x_last], 0xFF00_0000, "last blanked pixel");
        assert_eq!(fb[x_last + 1], FILL, "HBSTOP pixel shows again");
        // Applies to every row.
        assert_eq!(fb[100 * FB_WIDTH + x_first], 0xFF00_0000);
    }

    #[test]
    fn programmable_blanking_requires_ecs_and_wraps() {
        use crate::chipset::agnus::{
            Agnus, AgnusRevision, VideoStandard, BEAMCON0_PAL, BEAMCON0_VARVBEN,
        };

        // OCS Agnus: BEAMCON0 writes are dropped, so nothing blanks.
        let mut ocs = Agnus::with_video_standard(VideoStandard::Pal);
        ocs.write_beamcon0(BEAMCON0_PAL | BEAMCON0_VARVBEN);
        ocs.write_vbstrt(0x50);
        ocs.write_vbstop(0x58);
        const FILL: u32 = 0xFFAA_BBCC;
        let mut fb = vec![FILL; FB_PIXELS];
        apply_programmable_blanking(
            ocs.programmable_vertical_blank(),
            ocs.programmable_horizontal_blank(),
            &mut fb,
            PAL_VISIBLE_LINE0,
            FB_HEIGHT,
        );
        assert!(fb.iter().all(|&px| px == FILL));

        // VBSTRT >= VBSTOP wraps through the frame top: everything from
        // VBSTRT down plus the top rows below VBSTOP is blanked.
        let mut ecs =
            Agnus::with_video_standard_and_revision(VideoStandard::Pal, AgnusRevision::Ecs8372Rev4);
        ecs.write_beamcon0(BEAMCON0_PAL | BEAMCON0_VARVBEN);
        ecs.write_vbstrt(0x120);
        ecs.write_vbstop(0x30);
        let mut fb = vec![FILL; FB_PIXELS];
        apply_programmable_blanking(
            ecs.programmable_vertical_blank(),
            ecs.programmable_horizontal_blank(),
            &mut fb,
            PAL_VISIBLE_LINE0,
            FB_HEIGHT,
        );
        let row = |fb: &[u32], y: usize| fb[y * FB_WIDTH];
        // Beam line 0x2C (row 0) is below VBSTOP 0x30: blanked.
        assert_eq!(row(&fb, 0), 0xFF00_0000);
        assert_eq!(row(&fb, 0x30 - 0x2C), FILL, "VBSTOP clears the blank");
        assert_eq!(row(&fb, 0x120 - 0x2C), 0xFF00_0000, "VBSTRT asserts again");
    }

    /// Plan 3.4: AGA sprite colours come from the BPLCON4 ESPRM/OSPRM
    /// banks; pre-AGA stays on the classic 16..31 block.
    #[test]
    fn sprite_color_entry_follows_bplcon4_on_aga() {
        let ocs = ControlState::default();
        assert_eq!(sprite_color_entry(ocs, 0, 1, false), 17);
        assert_eq!(sprite_color_entry(ocs, 6, 3, false), 16 + 12 + 3);
        assert_eq!(sprite_color_entry(ocs, 0, 9, true), 25);

        // AGA with the reset default 0x0011: same 16..31 block.
        let aga_default = ControlState {
            agnus_revision: AgnusRevision::AgaAlice,
            bplcon4: 0x0011,
            ..ControlState::default()
        };
        assert_eq!(sprite_color_entry(aga_default, 0, 1, false), 17);
        assert_eq!(sprite_color_entry(aga_default, 1, 1, false), 17);

        // Distinct even/odd banks: ESPRM=7 (even sprites and attached
        // pairs at 112..), OSPRM=2 (odd sprites at 32..).
        let aga = ControlState {
            agnus_revision: AgnusRevision::AgaAlice,
            bplcon4: 0x0027,
            ..ControlState::default()
        };
        assert_eq!(sprite_color_entry(aga, 0, 1, false), 112 + 1);
        assert_eq!(sprite_color_entry(aga, 1, 1, false), 32 + 1);
        assert_eq!(sprite_color_entry(aga, 4, 2, false), 112 + 8 + 2);
        assert_eq!(sprite_color_entry(aga, 2, 9, true), 112 + 9);
    }

    #[test]
    fn present_h_shift_centres_standard_window() {
        // Stock PAL DIW ($2C81/$2CC1 -> H 0x81/0x1C1). 64px left border vs
        // 12px right border; recentre by half the difference = 26px.
        assert_eq!(present_h_shift(0x81, 0x1C1), 26);
    }

    #[test]
    fn present_h_shift_leaves_overscan_frames_untouched() {
        // Left overscan (DIWSTRT H left of standard).
        assert_eq!(present_h_shift(0x41, 0x1C1), 0);
        // Right overscan (DIWSTOP H past standard).
        assert_eq!(present_h_shift(0x81, 0x1D4), 0);
        // Both.
        assert_eq!(present_h_shift(0x71, 0x1E0), 0);
    }

    #[test]
    fn content_window_h_matches_standard_diw_for_stock_ddf() {
        // A standard lo-res fetch ($38..$D0) covers exactly the standard DIW
        // window, so a stock display's presentation centring is unchanged.
        let lores = ControlState {
            ddfstrt: 0x0038,
            ddfstop: 0x00D0,
            ..ControlState::default()
        };
        assert_eq!(
            lores.bitplane_content_window_h(),
            Some((STANDARD_DIW_HSTART, STANDARD_DIW_HSTOP))
        );
        // The standard hi-res fetch ($3C..$D4) covers the same window.
        let hires = ControlState {
            bplcon0: 0x8000,
            ddfstrt: 0x003C,
            ddfstop: 0x00D4,
            ..ControlState::default()
        };
        assert_eq!(
            hires.bitplane_content_window_h(),
            Some((STANDARD_DIW_HSTART, STANDARD_DIW_HSTOP))
        );
    }

    fn ocs_snapshot(
        diwstrt: u16,
        diwstop: u16,
        ddfstrt: u16,
        ddfstop: u16,
    ) -> RenderRegisterSnapshot {
        RenderRegisterSnapshot {
            // 5 lo-res planes; resolution/plane count is irrelevant to the H
            // window, but keep it a plausible playfield.
            bplcon0: 0x5200,
            diwstrt,
            diwstop,
            ddfstrt,
            ddfstop,
            ..RenderRegisterSnapshot::default()
        }
    }

    #[test]
    fn present_h_shift_for_centres_wide_diw_around_standard_fetch() {
        // Virtual Dreams "Absolute Inebriation": DIW opened wide
        // (DIWSTRT $5702 -> H $02, DIWSTOP $FFFF -> H $1FF) around a standard
        // 320-px lo-res picture (DDF $38..$D0). The open window only reveals
        // COLOR0 border the TV crops, so the picture must still recentre by the
        // stock 26px instead of sitting right-of-centre.
        assert_eq!(
            present_h_shift_for(&ocs_snapshot(0x5702, 0xFFFF, 0x0038, 0x00D0)),
            26
        );
        // A genuinely centred stock display is unchanged.
        assert_eq!(
            present_h_shift_for(&ocs_snapshot(0x2C81, 0x2CC1, 0x0038, 0x00D0)),
            26
        );
    }

    #[test]
    fn present_h_shift_for_leaves_true_overscan_fetch_untouched() {
        // Wide DIW *and* a fetch that reaches into the overscan border
        // (DDFSTRT $30 starts the picture left of the standard window): a real
        // overscan display, presented exactly as rendered.
        assert_eq!(
            present_h_shift_for(&ocs_snapshot(0x5702, 0xFFFF, 0x0030, 0x00D8)),
            0
        );
    }

    #[test]
    fn present_h_shift_for_leaves_narrow_late_fetch_untouched() {
        // A normal DIW can be used around a tiny one-word late-DDF object. The
        // fetched object must stay in beam position; presentation centring must
        // not copy its right edge into the deep-left overscan border.
        assert_eq!(
            present_h_shift_for(&ocs_snapshot(0x3481, 0x24D1, 0x0050, 0x0058)),
            0
        );
    }

    fn put_word(ram: &mut [u8], pc: usize, word: u16) {
        ram[pc..pc + 2].copy_from_slice(&word.to_be_bytes());
    }

    fn beam_event(vpos: u32, hpos: u32, offset: u16, value: u16) -> BeamRegisterWrite {
        BeamRegisterWrite {
            vpos,
            hpos,
            offset,
            value,
            source: BeamWriteSource::Copper,
        }
    }

    fn cpu_event(vpos: u32, hpos: u32, offset: u16, value: u16) -> BeamRegisterWrite {
        BeamRegisterWrite {
            vpos,
            hpos,
            offset,
            value,
            source: BeamWriteSource::Cpu,
        }
    }

    fn cpu_copper_irq_event(vpos: u32, hpos: u32, offset: u16, value: u16) -> BeamRegisterWrite {
        BeamRegisterWrite {
            vpos,
            hpos,
            offset,
            value,
            source: BeamWriteSource::CpuCopperIrq,
        }
    }

    fn visible_lowres_control(bplcon0: u16) -> ControlState {
        ControlState {
            bplcon0,
            diwstrt: ((PAL_VISIBLE_LINE0 as u16) << 8) | DIW_HSTART_FETCH_REFERENCE_LORES as u16,
            diwstop: (((PAL_VISIBLE_LINE0 + 1) as u16) << 8)
                | (DIW_HSTART_FETCH_REFERENCE_LORES as u16 + 4),
            ddfstrt: 0x0038,
            ddfstop: 0x0038,
            ..ControlState::default()
        }
    }

    #[test]
    fn bottom_palette_replay_uses_matching_cpu_copper_irq_events() {
        let frame_events = [cpu_copper_irq_event(0xFA, 0x40, 0x180, 0x0222)];
        let bottom_events = [cpu_copper_irq_event(0xD4, 0x16, 0x180, 0x0222)];

        assert!(should_replay_bottom_palette_events(
            &frame_events,
            &frame_events,
            &bottom_events,
            true
        ));
    }

    #[test]
    fn bottom_palette_replay_not_reinjected_when_frame_has_matching_cpu_copper_irq_events() {
        // A copper interrupt raised on the scanline above the bottom of an
        // index band triggers a CPU palette MOVE. The cycle-stepped CPU records
        // that write as a raw CpuCopperIrq event at the beam position where the
        // MOVE actually executes (one line later, after interrupt latency). The
        // bottom-palette replay carries the same write stamped at the earlier
        // copper-interrupt trigger position. Both should not be applied: the raw
        // beam-accurate write is authoritative, so the replay must not be
        // injected (otherwise it recolors the band's final visible scanline).
        let frame_events = [cpu_copper_irq_event(0xD5, 0x2B, 0x19C, 0x0999)];
        let bottom_events = [cpu_copper_irq_event(0xD4, 0x0B, 0x19C, 0x0999)];

        assert!(should_replay_bottom_palette_events(
            &frame_events,
            &frame_events,
            &bottom_events,
            true
        ));
        assert!(!should_inject_bottom_palette_replay_events(
            &frame_events,
            &frame_events,
            &bottom_events,
            true
        ));
    }

    #[test]
    fn bottom_palette_replay_injected_when_frame_carries_palette_forward() {
        // No raw CpuCopperIrq palette writes this frame: the palette was set by
        // a copper interrupt in an earlier frame and must be replayed at the
        // copper-interrupt beam position to reconstruct it for this frame.
        let frame_events = [beam_event(0x19, 0x2D, 0x092, 0x0038)];
        let bottom_events = [cpu_copper_irq_event(0xD4, 0x16, 0x180, 0x0222)];

        assert!(should_inject_bottom_palette_replay_events(
            &frame_events,
            &[],
            &bottom_events,
            true
        ));
    }

    #[test]
    fn bottom_palette_replay_persists_when_frame_has_no_palette_timing() {
        let frame_events = [beam_event(0x19, 0x2D, 0x092, 0x0038)];
        let bottom_events = [cpu_copper_irq_event(0xD4, 0x16, 0x180, 0x0222)];

        assert!(should_replay_bottom_palette_events(
            &frame_events,
            &[],
            &bottom_events,
            true
        ));
    }

    #[test]
    fn bottom_palette_replay_does_not_override_copper_palette_bands() {
        let frame_events = [beam_event(0x84, 0xDF, 0x180, 0x0111)];
        let bottom_events = [cpu_copper_irq_event(0xD4, 0x16, 0x180, 0x0222)];

        assert!(!should_replay_bottom_palette_events(
            &frame_events,
            &[],
            &bottom_events,
            true
        ));
    }

    #[test]
    fn current_cpu_copper_irq_palette_replay_requires_same_primary_bitplane_buffer() {
        let completed_pointer_events = [
            beam_event(0x20, 0x20, 0x0E0, 0x0004),
            beam_event(0x20, 0x24, 0x0E2, 0x2000),
        ];

        assert!(primary_bitplane_buffer_carries_forward(
            0x001000,
            &completed_pointer_events,
            0x042000,
            &[],
        ));
        assert!(!primary_bitplane_buffer_carries_forward(
            0x001000,
            &completed_pointer_events,
            0x052000,
            &[],
        ));
        assert!(!primary_bitplane_buffer_carries_forward(0, &[], 0, &[],));
    }

    fn sprite_control_words(vstart: u16, vstop: u16, hstart: u16) -> (u16, u16) {
        let pos = ((vstart & 0x00FF) << 8) | ((hstart >> 1) & 0x00FF);
        let ctl = ((vstop & 0x00FF) << 8)
            | ((vstart & 0x0100) >> 6)
            | ((vstop & 0x0100) >> 7)
            | (hstart & 0x0001);
        (pos, ctl)
    }

    fn blank_state() -> RenderState {
        RenderState {
            agnus_revision: AgnusRevision::Ocs,
            harddis: false,
            dmacon: 0,
            bplcon0: 0,
            bplcon1: 0,
            bplcon2: 0,
            bplcon3: BPLCON3_PF2OF_DEFAULT,
            bplcon4: 0x0011,
            fmode: 0,
            clxcon2: 0,
            clxcon: 0,
            bplpt: [0; 8],
            bpldat: [0; 8],
            sprpt: [0; 8],
            sprpos: [0; 8],
            sprctl: [0; 8],
            sprdata: [0; 8],
            sprdatb: [0; 8],
            spr_armed: [false; 8],
            bpl1mod: 0,
            bpl2mod: 0,
            palette: Palette::from_ocs([0x0103; 32]),
            diwstrt: 0,
            diwstop: 0,
            diwhigh: DiwHigh::ocs_implicit(),
            ddfstrt: 0,
            ddfstop: 0,
        }
    }

    #[test]
    fn display_window_converts_pal_beam_bounds_to_framebuffer_bounds() {
        let state = RenderState {
            diwstrt: ((PAL_VISIBLE_LINE0 as u16) << 8) | (DIW_HSTART_FB0 as u16 + 8),
            diwstop: (((PAL_VISIBLE_LINE0 + 128) as u16) << 8) | ((DIW_HSTART_FB0 + 63) as u16),
            ..blank_state()
        };

        assert_eq!(state.display_window_y(), (0, 128));
        assert_eq!(state.display_window_x(), (16, 638));
    }

    #[test]
    fn display_window_counts_rows_clipped_above_framebuffer() {
        let state = RenderState {
            diwstrt: (((PAL_VISIBLE_LINE0 - 16) as u16) << 8) | DIW_HSTART_FB0 as u16,
            diwstop: (((PAL_VISIBLE_LINE0 + 128) as u16) << 8) | DIW_HSTART_FB0 as u16,
            ..blank_state()
        };

        assert_eq!(state.display_window_y(), (0, 128));
        assert_eq!(state.clipped_display_rows_before_frame(), 16);
    }

    #[test]
    fn display_window_counts_pixels_clipped_left_of_framebuffer() {
        let state = RenderState {
            diwstrt: ((PAL_VISIBLE_LINE0 as u16) << 8) | (DIW_HSTART_FB0 as u16 - 18),
            diwstop: (((PAL_VISIBLE_LINE0 + 128) as u16) << 8) | 0x00D1,
            ..blank_state()
        };

        assert_eq!(state.display_window_x().0, 0);
        assert_eq!(state.clipped_display_pixels_before_frame(), 36);
    }

    #[test]
    fn display_window_zero_start_uses_denise_comparator() {
        let state = RenderState {
            diwstrt: 0x0000,
            diwstop: 0x2CC1,
            ..blank_state()
        };

        assert_eq!(state.diw_v_start(), 0);
        assert_eq!(state.diw_h_start(), 0);
        assert_eq!(state.display_window_y(), (0, 256));
        assert_eq!(
            state.display_window_x(),
            (0, ((0x01C1 - DIW_HSTART_FB0) * 2) as usize)
        );
        assert_eq!(
            state.clipped_display_rows_before_frame(),
            PAL_VISIBLE_LINE0 as usize
        );
        assert_eq!(
            state.clipped_display_pixels_before_frame(),
            (DIW_HSTART_FB0 as usize) * 2
        );
    }

    #[test]
    fn display_window_maps_pal_horizontal_overscan_to_full_framebuffer_width() {
        let state = RenderState {
            diwstrt: ((PAL_VISIBLE_LINE0 as u16) << 8) | DIW_HSTART_FB0 as u16,
            diwstop: (((PAL_VISIBLE_LINE0 + 128) as u16) << 8) | 0x00D1,
            ..blank_state()
        };

        assert_eq!(state.display_window_x(), (0, FB_WIDTH));
    }

    #[test]
    fn display_window_uses_stop_as_exclusive_right_edge() {
        let state = RenderState {
            diwstrt: 0x6395,
            diwstop: 0xF4AD,
            diwhigh: DiwHigh::ecs_explicit(0x2000),
            ..blank_state()
        };

        assert_eq!(state.display_window_x(), (104, 664));
    }

    #[test]
    fn display_window_uses_ocs_implicit_high_bits_until_diwhigh_is_written() {
        let state = RenderState {
            diwstrt: 0xFCFC,
            diwstop: 0x7F01,
            ..blank_state()
        };

        assert_eq!(state.diw_v_start(), 0x00FC);
        assert_eq!(state.diw_h_start(), 0x00FC);
        assert_eq!(state.diw_v_stop(), 0x017F);
        assert_eq!(state.diw_h_stop(), 0x0101);
    }

    #[test]
    fn display_window_diwhigh_zero_write_selects_ecs_direct_high_bits() {
        let state = RenderState {
            diwstrt: 0xFCFC,
            diwstop: 0x7F01,
            diwhigh: DiwHigh::ecs_explicit(0),
            ..blank_state()
        };

        assert_eq!(state.diw_v_start(), 0x00FC);
        assert_eq!(state.diw_h_start(), 0x00FC);
        assert_eq!(state.diw_v_stop(), 0x007F);
        assert_eq!(state.diw_h_stop(), 0x0001);
    }

    #[test]
    fn display_window_diwstrt_diwstop_write_reverts_diwhigh_to_implicit() {
        // An ECS DIWHIGH value only applies until the next DIWSTRT/DIWSTOP
        // write, which re-arms the OCS-implicit high bits. A stale DIWHIGH
        // must not keep shrinking the window after it is reprogrammed --
        // TurboTomato's ECS title rendered black because $00FF stayed latched
        // and pushed the vertical window start off-screen.
        let mut state = blank_state();
        apply_move(&mut state, 0x1E4, 0x00FF);
        assert_eq!(state.diwhigh, DiwHigh::ecs_explicit(0x00FF));
        apply_move(&mut state, 0x08E, 0x2C81);
        assert_eq!(state.diwhigh, DiwHigh::ocs_implicit());

        apply_move(&mut state, 0x1E4, 0x00FF);
        apply_move(&mut state, 0x090, 0x2CC1);
        assert_eq!(state.diwhigh, DiwHigh::ocs_implicit());
    }

    #[test]
    fn clipped_overscan_rows_advance_bitplane_pointers() {
        let mut ptrs = [0x0100, 0x1000, 0, 0, 0, 0, 0, 0];
        let control = ControlState::from_render_state(&RenderState {
            bpl1mod: 4,
            bpl2mod: -2,
            ..blank_state()
        });

        advance_bitplane_ptrs_for_rows(&mut ptrs, 3, 2, 22, &control, 0, 0x001F_FFFF);

        assert_eq!(ptrs[0], 0x0100 + 3 * (22 * 2 + 4));
        assert_eq!(ptrs[1], 0x1000 + 3 * (22 * 2 - 2));
    }

    #[test]
    fn bscan2_clipped_rows_alternate_modulos_by_line_parity() {
        let mut ptrs = [0x0100, 0x1000, 0, 0, 0, 0, 0, 0];
        let control = ControlState::from_render_state(&RenderState {
            agnus_revision: AgnusRevision::AgaAlice,
            fmode: 0x4000,
            bpl1mod: -44,
            bpl2mod: 4,
            diwstrt: ((PAL_VISIBLE_LINE0 as u16) << 8) | DIW_HSTART_FB0 as u16,
            ..blank_state()
        });

        // Two rows starting on DIWSTRT's parity: BPL1MOD then BPL2MOD, the
        // same modulo for both plane groups.
        advance_bitplane_ptrs_for_rows(
            &mut ptrs,
            2,
            2,
            22,
            &control,
            PAL_VISIBLE_LINE0,
            0x001F_FFFF,
        );

        let expected = 22 * 2 + 4;
        assert_eq!(ptrs[0], 0x0100 + expected as u32);
        assert_eq!(ptrs[1], 0x1000 + expected as u32);
    }

    #[test]
    fn display_window_uses_diwhigh_upper_vertical_bits() {
        let state = RenderState {
            diwstrt: 0x2C81,
            diwstop: 0x2D82,
            diwhigh: DiwHigh::ecs_explicit(0x0100),
            ..blank_state()
        };

        assert_eq!(state.diw_v_start(), 0x02C);
        assert_eq!(state.diw_v_stop(), 0x12D);
    }

    #[test]
    fn line_start_diw_write_replaces_previous_horizontal_display_bounds() {
        let base = ControlState {
            diwstrt: ((PAL_VISIBLE_LINE0 as u16) << 8) | STANDARD_DIW_HSTART as u16,
            diwstop: (((PAL_VISIBLE_LINE0 + 1) as u16) << 8) | 0x00C1,
            ..ControlState::default()
        };
        let narrowed = ControlState {
            diwstrt: ((PAL_VISIBLE_LINE0 as u16) << 8) | 0x00A0,
            ..base
        };
        let segments = [ControlSegment {
            x: 0,
            control: narrowed,
        }];

        assert_eq!(
            line_display_window_bounds(base, &segments, 0, PAL_VISIBLE_LINE0),
            Some(narrowed.display_window_x())
        );
    }

    #[test]
    fn beam_position_converts_to_line_and_segment_x() {
        let line = PAL_VISIBLE_LINE0 + 25;
        let hpos = COPPER_WAIT_HPOS_FB0 + 16;

        assert_eq!(beam_to_framebuffer_pos(line as u32, hpos as u32), (25, 64));
    }

    #[test]
    fn denise_horizontal_delay_aligns_copper_beam_and_display_fetch_domains() {
        let state = RenderState {
            bplcon0: 0,
            diwstrt: ((PAL_VISIBLE_LINE0 as u16) << 8) | STANDARD_DIW_HSTART as u16,
            diwstop: (((PAL_VISIBLE_LINE0 + 1) as u16) << 8) | 0x00C1,
            ddfstrt: 0x0038,
            ..blank_state()
        };
        let display_left = state.display_window_x().0;
        let copper_hpos = COPPER_WAIT_HPOS_FB0 + (display_left / 4) as i32;

        assert_eq!(display_left, 64);
        assert_eq!(
            beam_to_framebuffer_pos(PAL_VISIBLE_LINE0 as u32, copper_hpos as u32),
            (0, display_left)
        );
        assert_eq!(state.native_x_offset(false, 2), 1);
        assert_eq!(state.fetch_start_native_x(false, 2), 0);
    }

    #[test]
    fn color_register_writes_use_final_output_position() {
        assert_eq!(color_write_framebuffer_x(COLOR_WRITE_HPOS_FB0 as u32), 0);
        assert_eq!(
            color_write_framebuffer_x((COLOR_WRITE_HPOS_FB0 + 4) as u32),
            16
        );
        assert_eq!(
            beam_to_framebuffer_x_unclamped(COLOR_WRITE_HPOS_FB0 as u32),
            52
        );
        assert_eq!(
            sprite_palette_control_framebuffer_x(SPRITE_PALETTE_CONTROL_HPOS_FB0 as u32),
            0
        );
        assert_eq!(
            sprite_palette_control_framebuffer_x((SPRITE_PALETTE_CONTROL_HPOS_FB0 + 4) as u32),
            16
        );
    }

    #[test]
    fn bplcon3_brdrblnk_blanks_border_when_ecsena_set() {
        let mut state = RenderState {
            bplcon0: BPLCON0_ECSENA,
            bplcon3: BPLCON3_BRDRBLNK,
            diwstrt: ((PAL_VISIBLE_LINE0 as u16) << 8) | (DIW_HSTART_FB0 as u16 + 8),
            diwstop: (((PAL_VISIBLE_LINE0 + 128) as u16) << 8) | (DIW_HSTART_FB0 as u16 + 63),
            ..blank_state()
        };
        state.palette.write_ocs(0, 0x0F00);
        let base_palettes = [state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let base_control = ControlState::from_render_state(&state);
        let base_controls = [base_control; FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let mut fb = vec![0; FB_PIXELS];

        fill_background(
            &mut fb,
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
        );

        assert_eq!(fb[0], rgb12_to_rgba8_alpha(0, false));
        assert_eq!(fb[16], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[200 * FB_WIDTH], rgb12_to_rgba8_alpha(0, false));
    }

    #[test]
    fn bplcon3_brdrblnk_requires_ecsena_for_border_blank() {
        let mut state = RenderState {
            bplcon0: 0,
            bplcon3: BPLCON3_BRDRBLNK,
            diwstrt: ((PAL_VISIBLE_LINE0 as u16) << 8) | (DIW_HSTART_FB0 as u16 + 8),
            diwstop: (((PAL_VISIBLE_LINE0 + 128) as u16) << 8) | (DIW_HSTART_FB0 as u16 + 63),
            ..blank_state()
        };
        state.palette.write_ocs(0, 0x0F00);
        let base_palettes = [state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let base_control = ControlState::from_render_state(&state);
        let base_controls = [base_control; FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let mut fb = vec![0; FB_PIXELS];

        fill_background(
            &mut fb,
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
        );

        assert_eq!(fb[0], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[16], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[200 * FB_WIDTH], rgb12_to_rgba8(0x0F00));
    }

    #[test]
    fn bplcon3_brdntran_keeps_border_opaque_for_color_key() {
        let mut state = RenderState {
            bplcon0: BPLCON0_ECSENA,
            bplcon2: BPLCON2_ZDCTEN,
            bplcon3: BPLCON3_BRDNTRAN,
            diwstrt: ((PAL_VISIBLE_LINE0 as u16) << 8) | (DIW_HSTART_FB0 as u16 + 8),
            diwstop: (((PAL_VISIBLE_LINE0 + 128) as u16) << 8) | (DIW_HSTART_FB0 as u16 + 63),
            ..blank_state()
        };
        state.palette.write_ocs(0, COLOR_TRANSPARENCY_BIT | 0x0F00);
        let base_palettes = [state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let base_control = ControlState::from_render_state(&state);
        let base_controls = [base_control; FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let mut fb = vec![0; FB_PIXELS];

        fill_background(
            &mut fb,
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
        );

        assert_eq!(fb[0], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[16], rgb12_to_rgba8_alpha(0x0F00, false));
    }

    #[test]
    fn bplcon3_brdntran_makes_blank_border_opaque_black() {
        let mut state = RenderState {
            bplcon0: BPLCON0_ECSENA,
            bplcon3: BPLCON3_BRDRBLNK | BPLCON3_BRDNTRAN,
            diwstrt: ((PAL_VISIBLE_LINE0 as u16) << 8) | (DIW_HSTART_FB0 as u16 + 8),
            diwstop: (((PAL_VISIBLE_LINE0 + 128) as u16) << 8) | (DIW_HSTART_FB0 as u16 + 63),
            ..blank_state()
        };
        state.palette.write_ocs(0, 0x0F00);
        let base_palettes = [state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let base_control = ControlState::from_render_state(&state);
        let base_controls = [base_control; FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let mut fb = vec![0; FB_PIXELS];

        fill_background(
            &mut fb,
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
        );

        assert_eq!(fb[0], rgb12_to_rgba8(0));
        assert_eq!(fb[16], rgb12_to_rgba8(0x0F00));
    }

    #[test]
    fn beam_timed_bplcon3_brdrblnk_latches_until_ecsena_enables_effect() {
        let mut state = RenderState {
            bplcon0: 0,
            bplcon3: 0,
            diwstrt: ((PAL_VISIBLE_LINE0 as u16) << 8) | (DIW_HSTART_FB0 as u16 + 80),
            diwstop: (((PAL_VISIBLE_LINE0 + 1) as u16) << 8) | (DIW_HSTART_FB0 as u16 + 120),
            ..blank_state()
        };
        state.palette.write_ocs(0, 0x0F00);
        let mut base_palettes = [state.palette; FB_HEIGHT];
        let mut palette_segments = vec![Vec::new(); FB_HEIGHT];
        let mut base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let mut control_segments = vec![Vec::new(); FB_HEIGHT];
        let mut manual_bpl_segments = Vec::new();
        let events = [
            beam_event(
                PAL_VISIBLE_LINE0 as u32,
                (COPPER_WAIT_HPOS_FB0 + 4) as u32,
                0x0106,
                BPLCON3_BRDRBLNK,
            ),
            beam_event(
                PAL_VISIBLE_LINE0 as u32,
                (COPPER_WAIT_HPOS_FB0 + 8) as u32,
                0x0100,
                BPLCON0_ECSENA,
            ),
        ];

        apply_render_events(
            &mut state,
            &events,
            &mut base_palettes,
            &mut palette_segments,
            &mut base_controls,
            &mut control_segments,
            &mut manual_bpl_segments,
        );
        let mut fb = vec![0; FB_PIXELS];
        fill_background(
            &mut fb,
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
        );

        assert_eq!(fb[8], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[24], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[40], rgb12_to_rgba8_alpha(0, false));
    }

    #[test]
    fn native_x_offset_accounts_for_diw_and_ddf_alignment() {
        let standard_hires = RenderState {
            bplcon0: 0x8000,
            diwstrt: ((PAL_VISIBLE_LINE0 as u16) << 8) | DIW_HSTART_FB0 as u16,
            ddfstrt: 0x003C,
            ..blank_state()
        };
        assert_eq!(standard_hires.native_x_offset(true, 1), 0);

        // FMODE=0: the fetch gulp equals the DDF granularity, so the picture
        // follows DDFSTRT continuously. KS 2.05's insert-disk screen (DDF
        // $40, DIW $95) is drawn for exactly this placement: its negative
        // modulos overlap rows so the drive art's right edge lives in the
        // next row's first bytes.
        let kickstart_hires = RenderState {
            bplcon0: 0x8000,
            diwstrt: 0x6395,
            ddfstrt: 0x0040,
            ..blank_state()
        };
        assert_eq!(kickstart_hires.native_x_offset(true, 1), 20);

        // Wide FMODE fetches quantize the displayed shifter origin to the gulp
        // grid: AGA system screens program DDFSTRT $38 or $3C interchangeably
        // (same 16-cck gulp slot) and must display identically; without the
        // quantized placement the $38 screens showed the interleaved bitmap's
        // fetch overrun as a junk column inside the window's right edge.
        let wb_hires_overscan_fetch = RenderState {
            agnus_revision: AgnusRevision::AgaAlice,
            bplcon0: 0x8000,
            fmode: 0x0003,
            diwstrt: 0x2C81,
            diwstop: 0x2CC1,
            ddfstrt: 0x0038,
            ..blank_state()
        };
        let wb_with_standard_ddf = RenderState {
            ddfstrt: 0x003C,
            ..wb_hires_overscan_fetch
        };
        assert_eq!(
            wb_hires_overscan_fetch.native_x_offset(true, 1),
            wb_with_standard_ddf.native_x_offset(true, 1)
        );
        assert_eq!(
            wb_hires_overscan_fetch.fetch_start_native_x(true, 1),
            wb_with_standard_ddf.fetch_start_native_x(true, 1)
        );
        // Wide-FMODE hi-res output sits one colour clock left of the
        // FMODE=0-calibrated reference (Lisa's fetch-width-dependent
        // bitplane delay): the AGA bitmap fills its standard
        // $81 window exactly flush, matching FS-UAE. With the FMODE=0
        // reference it painted 4 pixels right (colour-0 stripe at the
        // window's left edge, last bitmap pixels clipped at the right).
        assert_eq!(wb_hires_overscan_fetch.native_x_offset(true, 1), 0);
        assert_eq!(wb_hires_overscan_fetch.fetch_start_native_x(true, 1), 0);

        // The placement gulp grid runs on absolute colour-clock multiples of
        // the fetch period. Lores FMODE=1 has a 16-cck gulp: DDFSTRT $30 is
        // on-grid and shares its displayed origin with the standard $38, so
        // both must display at the same position. A $18-anchored grid put
        // these modes half a gulp early, shifting the picture left with wrap
        // junk at the window's right edge.
        let pinball_lores_wide_fetch = RenderState {
            agnus_revision: AgnusRevision::AgaAlice,
            bplcon0: 0x0611,
            fmode: 0x0001,
            diwstrt: 0x2C81,
            diwstop: 0x2CC1,
            ddfstrt: 0x0030,
            ..blank_state()
        };
        let pinball_with_standard_ddf = RenderState {
            ddfstrt: 0x0038,
            ..pinball_lores_wide_fetch
        };
        assert_eq!(
            pinball_lores_wide_fetch.native_x_offset(false, 2),
            pinball_with_standard_ddf.native_x_offset(false, 2)
        );
        assert_eq!(
            pinball_lores_wide_fetch.fetch_start_native_x(false, 2),
            pinball_with_standard_ddf.fetch_start_native_x(false, 2)
        );

        let diagrom_hires = RenderState {
            bplcon0: 0x8000,
            diwstrt: 0x2C81,
            diwstop: 0x2CC1,
            ddfstrt: 0x003C,
            ..blank_state()
        };
        assert_eq!(diagrom_hires.display_window_x().0, 64);
        assert_eq!(diagrom_hires.clipped_display_pixels_before_frame(), 0);
        assert_eq!(diagrom_hires.native_x_offset(true, 1), 0);

        let lores_extra_fetch_word = RenderState {
            bplcon0: 0,
            diwstrt: ((PAL_VISIBLE_LINE0 as u16) << 8) | DIW_HSTART_FB0 as u16,
            ddfstrt: 0x0030,
            ..blank_state()
        };
        assert_eq!(lores_extra_fetch_word.native_x_offset(false, 2), 0);
        assert_eq!(lores_extra_fetch_word.fetch_start_native_x(false, 2), 15);

        let lores_late_fetch = RenderState {
            bplcon0: 0,
            diwstrt: ((PAL_VISIBLE_LINE0 as u16) << 8) | DIW_HSTART_FB0 as u16,
            ddfstrt: 0x0050,
            ..blank_state()
        };
        assert_eq!(lores_late_fetch.native_x_offset(false, 2), 0);
        assert_eq!(lores_late_fetch.fetch_start_native_x(false, 2), 79);

        let lores_early_fetch_standard_window = RenderState {
            bplcon0: 0,
            diwstrt: ((PAL_VISIBLE_LINE0 as u16) << 8) | STANDARD_DIW_HSTART as u16,
            ddfstrt: 0x0030,
            ddfstop: 0x00D0,
            ..blank_state()
        };
        assert_eq!(
            lores_early_fetch_standard_window.words_per_row(false, 0),
            21
        );
        assert_eq!(
            lores_early_fetch_standard_window.native_x_offset(false, 2),
            16
        );
    }

    #[test]
    fn ddfstrt_positions_first_lowres_bitplane_word_relative_to_diwstrt() {
        let standard_lowres = RenderState {
            bplcon0: 0,
            diwstrt: ((PAL_VISIBLE_LINE0 as u16) << 8) | 0x0081,
            ddfstrt: 0x0038,
            ..blank_state()
        };
        assert_eq!(standard_lowres.fetch_start_native_x(false, 2), 0);
        assert_eq!(standard_lowres.native_x_offset(false, 2), 1);

        let late_window_aligned_fetch = RenderState {
            bplcon0: 0,
            diwstrt: ((PAL_VISIBLE_LINE0 as u16) << 8) | 0x00A1,
            ddfstrt: 0x0048,
            ..blank_state()
        };
        assert_eq!(late_window_aligned_fetch.fetch_start_native_x(false, 2), 0);
        assert_eq!(late_window_aligned_fetch.native_x_offset(false, 2), 1);

        let inset_fetch = RenderState {
            bplcon0: 0,
            diwstrt: ((PAL_VISIBLE_LINE0 as u16) << 8) | 0x00A2,
            ddfstrt: 0x0050,
            ..blank_state()
        };
        assert_eq!(inset_fetch.fetch_start_native_x(false, 2), 14);
        assert_eq!(inset_fetch.native_x_offset(false, 2), 0);

        let late_fetch_standard_window = RenderState {
            bplcon0: 0,
            diwstrt: ((PAL_VISIBLE_LINE0 as u16) << 8) | 0x0081,
            ddfstrt: 0x0048,
            ..blank_state()
        };
        assert_eq!(
            late_fetch_standard_window.fetch_start_native_x(false, 2),
            31
        );
        assert_eq!(late_fetch_standard_window.native_x_offset(false, 2), 0);
    }

    #[test]
    fn late_ddf_bitplane_output_starts_at_first_word_fetch() {
        let standard_ddf = ControlState {
            dmacon: DMACON_DMAEN | DMACON_BPLEN,
            bplcon0: 0x1000,
            diwstrt: ((PAL_VISIBLE_LINE0 as u16) << 8) | STANDARD_DIW_HSTART as u16,
            diwstop: (((PAL_VISIBLE_LINE0 + 1) as u16) << 8) | STANDARD_DIW_HSTOP as u16,
            ddfstrt: 0x0038,
            ddfstop: 0x00D0,
            ..ControlState::default()
        };
        let standard_words =
            standard_ddf.words_per_row(native_frame_width_for_control(standard_ddf));

        assert_eq!(
            bitplane_output_start_x(
                standard_ddf,
                &[],
                standard_ddf.display_window_x().0,
                standard_words,
                standard_ddf.dma_planes(),
            ),
            standard_ddf.display_window_x().0
        );

        let inset_ddf = ControlState {
            ddfstrt: 0x0050,
            ddfstop: 0x00D0,
            ..standard_ddf
        };
        let inset_words = inset_ddf.words_per_row(native_frame_width_for_control(inset_ddf));
        let first_word_x = bitplane_fetch_framebuffer_x(
            line_fetch_plan_for_word(inset_ddf, &[], 0, inset_ddf.dma_planes())
                .word_fetch_hpos
                .unwrap(),
        );
        let first_bpl1dat_x =
            bitplane_fetch_framebuffer_x(bitplane_fetch_hpos_for_plane(inset_ddf, 0, 0));

        assert_ne!(
            inset_ddf.fetch_start_native_x(inset_ddf.diw_h_start(), 2),
            0
        );
        assert!(first_word_x < first_bpl1dat_x);
        assert_eq!(
            bitplane_output_start_x(
                inset_ddf,
                &[],
                inset_ddf.display_window_x().0,
                inset_words,
                inset_ddf.dma_planes(),
            ),
            first_word_x
        );
    }

    #[test]
    fn late_ddf_waits_for_each_planes_first_fetch_slot() {
        let control = ControlState {
            bplcon0: 0x1000,
            ..ControlState::default()
        };
        let plane_words = [vec![0x0800]];
        let mut carry_words = [None; 8];
        carry_words[0] = Some(0x0001);
        let mut first_word_plane_start_x = [0; 8];
        first_word_plane_start_x[0] = 4;

        let line = DenisePlannedPlayfieldLine::new(0, 0, 64, &plane_words, 16)
            .with_carry_words(carry_words)
            .with_first_word_plane_start_x(first_word_plane_start_x);

        assert_eq!(line.sample(control, 2).idx, 0);
        assert_eq!(line.sample(control, 3).idx, 1);
        assert_eq!(line.sample(control, 4).idx, 1);
    }

    #[test]
    fn bplcon1_delay_blanks_left_edge_without_shifting_row() {
        assert_eq!(
            left_edge_blank_pixels(ControlState {
                bplcon0: 0x8000,
                bplcon1: 4,
                bplcon2: 0,
                ..ControlState::default()
            }),
            4
        );
        assert_eq!(
            left_edge_blank_pixels(ControlState {
                bplcon0: 0,
                bplcon1: 4,
                bplcon2: 0,
                ..ControlState::default()
            }),
            8
        );
    }

    #[test]
    fn bplcon1_delay_uses_previous_line_shifter_tail_when_contiguous() {
        let control = ControlState {
            bplcon0: 0x1000,
            bplcon1: 3,
            ..ControlState::default()
        };
        let plane_words = [vec![0x8000]];
        let no_carry = DenisePlannedPlayfieldLine::new(0, 0, 32, &plane_words, 16);

        assert_eq!(
            no_carry.sample(control, 0),
            DeniseBitplaneSample {
                idx: 0,
                nplanes: 1,
                active: true,
            }
        );

        let mut carry_words = [None; 8];
        carry_words[0] = Some(0x0004);
        let with_carry = DenisePlannedPlayfieldLine::new(0, 0, 32, &plane_words, 16)
            .with_carry_words(carry_words);

        assert_eq!(with_carry.sample(control, 0).idx, 1);
        assert_eq!(with_carry.sample(control, 1).idx, 0);
        assert_eq!(with_carry.sample(control, 3).idx, 1);
    }

    #[test]
    fn aga_extended_bplcon1_delay_does_not_reuse_single_word_line_tail() {
        let control = ControlState {
            agnus_revision: AgnusRevision::AgaAlice,
            bplcon0: BPLCON0_ECSENA | 0x1000,
            bplcon1: 0x0800,
            fmode: 0x0003,
            ..ControlState::default()
        };
        assert_eq!(control.scroll_for_plane(0), 32);

        let plane_words = [vec![0x8000, 0x0000, 0x0000]];
        let mut carry_words = [None; 8];
        carry_words[0] = Some(0xFFFF);
        let line = DenisePlannedPlayfieldLine::new(0, 0, 64, &plane_words, 48)
            .with_carry_words(carry_words);

        assert_eq!(line.sample(control, 15).idx, 0);
        assert_eq!(line.sample(control, 16).idx, 0);
        assert_eq!(line.sample(control, 31).idx, 0);
        assert_eq!(line.sample(control, 32).idx, 1);
    }

    #[test]
    fn bplcon1_delay_drops_carry_when_first_bpl1dat_is_late() {
        let mut previous_tail = [None; 8];
        previous_tail[0] = Some(0x0001);

        assert_eq!(
            bitplane_carry_words_for_line(false, 64, Some(64), previous_tail),
            previous_tail
        );
        assert_eq!(
            bitplane_carry_words_for_line(false, 64, Some(158), previous_tail),
            [None; 8]
        );
        assert_eq!(
            bitplane_carry_words_for_line(true, 64, Some(64), previous_tail),
            [None; 8]
        );
    }

    #[test]
    fn visible_cpu_palette_write_replays_by_beam_position() {
        let mut state = blank_state();
        let mut base_palettes = [state.palette; FB_HEIGHT];
        let mut palette_segments = vec![Vec::new(); FB_HEIGHT];
        let mut base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let mut control_segments = vec![Vec::new(); FB_HEIGHT];
        let mut manual_bpl_segments = Vec::new();
        let events = [cpu_event(0x45, COPPER_WAIT_HPOS_FB0 as u32, 0x0180, 0x0FFF)];

        apply_render_events(
            &mut state,
            &events,
            &mut base_palettes,
            &mut palette_segments,
            &mut base_controls,
            &mut control_segments,
            &mut manual_bpl_segments,
        );

        let line = (0x45 - 0x2C) as usize;
        assert_eq!(base_palettes[0][0], 0x0103);
        assert_eq!(base_palettes[line][0], 0x0103);
        assert_eq!(base_palettes[line + 1][0], 0x0FFF);
        assert_eq!(palette_segments[line].len(), 1);
        assert_eq!(palette_segments[line][0].x, 0);
        assert_eq!(palette_segments[line][0].entry, 0);
        assert_eq!(palette_segments[line][0].value, 0x0FFF);
        assert_eq!(state.palette[0], 0x0FFF);
    }

    #[test]
    fn small_visible_cpu_palette_batch_replays_by_beam_position() {
        let mut state = blank_state();
        let mut base_palettes = [state.palette; FB_HEIGHT];
        let mut palette_segments = vec![Vec::new(); FB_HEIGHT];
        let mut base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let mut control_segments = vec![Vec::new(); FB_HEIGHT];
        let mut manual_bpl_segments = Vec::new();
        let mut events = Vec::new();
        for idx in 0..30 {
            events.push(cpu_event(
                0x45 + idx as u32,
                COPPER_WAIT_HPOS_FB0 as u32,
                0x0180,
                idx as u16,
            ));
        }

        apply_render_events(
            &mut state,
            &events,
            &mut base_palettes,
            &mut palette_segments,
            &mut base_controls,
            &mut control_segments,
            &mut manual_bpl_segments,
        );

        let line = (0x45 - 0x2C) as usize;
        assert_eq!(base_palettes[line][0], 0x0103);
        assert_eq!(palette_segments[line].len(), 1);
        assert_eq!(palette_segments[line][0].x, 0);
        assert_eq!(palette_segments[line][0].entry, 0);
        assert_eq!(palette_segments[line][0].value, 0);
    }

    #[test]
    fn cpu_copper_irq_palette_writes_replay_by_beam_position() {
        let mut state = blank_state();
        let mut base_palettes = [state.palette; FB_HEIGHT];
        let mut palette_segments = vec![Vec::new(); FB_HEIGHT];
        let mut base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let mut control_segments = vec![Vec::new(); FB_HEIGHT];
        let mut manual_bpl_segments = Vec::new();
        let events = [cpu_copper_irq_event(
            0x45,
            COPPER_WAIT_HPOS_FB0 as u32,
            0x0180,
            0x0FFF,
        )];

        apply_render_events(
            &mut state,
            &events,
            &mut base_palettes,
            &mut palette_segments,
            &mut base_controls,
            &mut control_segments,
            &mut manual_bpl_segments,
        );

        let line = (0x45 - 0x2C) as usize;
        assert_eq!(base_palettes[line][0], 0x0103);
        assert_eq!(palette_segments[line].len(), 1);
        assert_eq!(palette_segments[line][0].x, 0);
        assert_eq!(palette_segments[line][0].entry, 0);
        assert_eq!(palette_segments[line][0].value, 0x0FFF);
    }

    #[test]
    fn cpu_palette_writes_before_visible_area_update_frame_base() {
        let mut state = blank_state();
        let mut base_palettes = [state.palette; FB_HEIGHT];
        let mut palette_segments = vec![Vec::new(); FB_HEIGHT];
        let mut base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let mut control_segments = vec![Vec::new(); FB_HEIGHT];
        let mut manual_bpl_segments = Vec::new();
        let events = [cpu_event(0x10, COPPER_WAIT_HPOS_FB0 as u32, 0x0180, 0x0ACE)];

        apply_render_events(
            &mut state,
            &events,
            &mut base_palettes,
            &mut palette_segments,
            &mut base_controls,
            &mut control_segments,
            &mut manual_bpl_segments,
        );

        assert!(palette_segments.iter().all(Vec::is_empty));
        assert_eq!(base_palettes[0][0], 0x0ACE);
        assert_eq!(base_palettes[FB_HEIGHT - 1][0], 0x0ACE);
    }

    #[test]
    fn ddf_overscan_fetches_still_advance_bitplane_pointers() {
        let state = RenderState {
            ddfstrt: 0x0038,
            ddfstop: 0x00D8,
            ..blank_state()
        };
        assert_eq!(state.words_per_row(true, 640), 42);
        assert_eq!(state.words_per_row(false, 320), 21);

        let hard_clipped = RenderState {
            ddfstrt: 0x0010,
            ddfstop: 0x00E0,
            ..blank_state()
        };
        assert_eq!(hard_clipped.words_per_row(false, 320), 25);

        let ocs_equal = RenderState {
            ddfstrt: 0x0038,
            ddfstop: 0x0038,
            ..blank_state()
        };
        assert_eq!(ocs_equal.words_per_row(false, 320), 21);

        let lores_partial_stop = RenderState {
            ddfstrt: 0x004A,
            ddfstop: 0x00B6,
            ..blank_state()
        };
        assert_eq!(lores_partial_stop.words_per_row(false, 320), 15);

        let lores_odd_stop = RenderState {
            ddfstrt: 0x0064,
            ddfstop: 0x00A5,
            ..blank_state()
        };
        assert_eq!(lores_odd_stop.words_per_row(false, 320), 9);

        let lores_four_cck_start = RenderState {
            ddfstrt: 0x0034,
            ddfstop: 0x00D4,
            ..blank_state()
        };
        assert_eq!(lores_four_cck_start.words_per_row(false, 320), 21);

        let lores_second_half_stop = RenderState {
            ddfstrt: 0x0028,
            ddfstop: 0x00D4,
            ..blank_state()
        };
        assert_eq!(lores_second_half_stop.words_per_row(false, 320), 23);

        let ecs_equal = RenderState {
            agnus_revision: AgnusRevision::Ecs8372Rev4,
            ddfstrt: 0x0038,
            ddfstop: 0x0038,
            ..blank_state()
        };
        assert_eq!(ecs_equal.words_per_row(false, 320), 1);
    }

    #[test]
    fn six_plane_non_ham_non_dual_playfield_selects_extra_half_brite() {
        let state = ControlState {
            bplcon0: 0x6000,
            bplcon1: 0,
            bplcon2: 0,
            ..ControlState::default()
        };
        assert!(state.extra_half_brite());

        let ham = ControlState {
            bplcon0: 0x6800,
            bplcon1: 0,
            bplcon2: 0,
            ..ControlState::default()
        };
        assert!(!ham.extra_half_brite());

        let dual_playfield = ControlState {
            bplcon0: 0x6400,
            bplcon1: 0,
            bplcon2: 0,
            ..ControlState::default()
        };
        assert!(!dual_playfield.extra_half_brite());

        let kill_ehb = ControlState {
            bplcon0: 0x6000,
            bplcon1: 0,
            bplcon2: 0x0200,
            ..ControlState::default()
        };
        assert!(!kill_ehb.extra_half_brite());
    }

    #[test]
    fn six_plane_ham_selects_hold_and_modify() {
        let state = ControlState {
            bplcon0: 0x6800,
            bplcon1: 0,
            bplcon2: 0,
            ..ControlState::default()
        };
        assert!(state.hold_and_modify());
        assert!(!state.extra_half_brite());

        let five_plane_ham_bit = ControlState {
            bplcon0: 0x5800,
            bplcon1: 0,
            bplcon2: 0,
            ..ControlState::default()
        };
        assert!(five_plane_ham_bit.hold_and_modify());
    }

    #[test]
    fn shres_limits_bitplane_depth_and_disables_ham() {
        let state = ControlState {
            bplcon0: BPLCON0_SHRES | 0x6800,
            bplcon1: 0,
            bplcon2: 0,
            ..ControlState::default()
        };

        assert_eq!(state.nplanes(), 2);
        assert_eq!(state.dma_planes(), 2);
        assert!(!state.hold_and_modify());
    }

    #[test]
    fn ocs_lowres_bpu7_renders_six_latched_planes_with_four_dma_planes() {
        let state = ControlState {
            bplcon0: 0x7800,
            bplcon1: 0,
            bplcon2: 0,
            ..ControlState::default()
        };
        assert_eq!(state.nplanes(), 6);
        assert_eq!(state.dma_planes(), 4);
        assert!(state.hold_and_modify());
        assert!(!state.extra_half_brite());
    }

    #[test]
    fn shres_bitplane_fetch_uses_four_words_per_fetch_slot() {
        let control = ControlState {
            agnus_revision: AgnusRevision::Ecs8372Rev4,
            bplcon0: BPLCON0_SHRES | 0x2000,
            ddfstrt: 0x0038,
            ddfstop: 0x0038,
            ..ControlState::default()
        };

        assert_eq!(
            control.words_per_row(native_frame_width_for_control(control)),
            4
        );
    }

    #[test]
    fn display_line_fetch_plan_records_lowres_bpu7_four_dma_slots_in_beam_order() {
        let control = ControlState {
            dmacon: DMACON_DMAEN | DMACON_BPLEN,
            bplcon0: 0x7800,
            ddfstrt: 0x0038,
            ddfstop: 0x0040,
            ..ControlState::default()
        };
        let segments = [ControlSegment { x: 0, control }];

        let plans = line_fetch_plans_for_line(control, &segments, 2, control.dma_planes());

        assert_eq!(plans[0].word_fetch_hpos, Some(0x0038));
        assert_eq!(plans[1].word_fetch_hpos, Some(0x0040));
        assert_eq!(
            plans[0].iter().collect::<Vec<_>>(),
            vec![(0x0039, 3), (0x003B, 1), (0x003D, 2), (0x003F, 0)]
        );
        assert_eq!(
            plans[1].iter().collect::<Vec<_>>(),
            vec![(0x0041, 3), (0x0043, 1), (0x0045, 2), (0x0047, 0)]
        );
    }

    #[test]
    fn display_line_plan_records_words_registers_and_sprite_slots_in_beam_order() {
        let control = ControlState {
            dmacon: DMACON_DMAEN | DMACON_BPLEN | DMACON_SPREN,
            bplcon0: 0x7800,
            ddfstrt: 0x0038,
            ddfstop: 0x0040,
            ..ControlState::default()
        };
        let row_words = [
            vec![0x1000, 0x1001],
            vec![0x2000, 0x2001],
            vec![0x3000, 0x3001],
            vec![0x4000, 0x4001],
            vec![0x5000, 0x5001],
            vec![0x6000, 0x6001],
            Vec::new(),
            Vec::new(),
        ];
        let fetch_plans = line_fetch_plans_for_line(control, &[], 2, control.dma_planes());
        let register_events = [DisplayLinePlanEvent::BpldatWrite {
            hpos: 0x003E,
            x: beam_to_framebuffer_x_unclamped(0x003E),
            plane: 5,
            value: 0xAAAA,
        }];
        let sprite_lines = [CapturedSpriteLine {
            sprite: 2,
            hstart: 0x90,
            hsub_70ns: false,
            beam_y: PAL_VISIBLE_LINE0,
            data: 0x8000,
            datb: 0x4000,
            attached: false,
            data_ext: [0; 3],
            datb_ext: [0; 3],
            width_words: 1,
        }];
        let plan = DisplayLinePlan::new(
            0,
            PAL_VISIBLE_LINE0 as u32,
            0,
            64,
            control.nplanes(),
            control.dma_planes(),
            2,
            &fetch_plans,
            &row_words,
            &register_events,
            &sprite_lines,
            control,
        );
        let events = plan.collect_events();

        assert!(events.windows(2).all(|pair| {
            (pair[0].hpos(), pair[0].beam_order()) <= (pair[1].hpos(), pair[1].beam_order())
        }));
        assert!(events.contains(&DisplayLinePlanEvent::SpriteSlot {
            hpos: SPRITE_DMA_PAIR_CAPTURE_HPOS[1],
            sprite: 2,
            hstart: 0x90,
            data: 0x8000,
            datb: 0x4000,
            attached: false,
        }));
        assert!(events.contains(&DisplayLinePlanEvent::BitplaneDmaFetch {
            hpos: 0x003F,
            word_idx: 0,
            plane: 0,
            word: 0x1000,
        }));
        assert!(events.contains(&DisplayLinePlanEvent::LatchedBitplaneWord {
            hpos: 0x003F,
            word_idx: 0,
            plane: 4,
            word: 0x5000,
        }));
        assert!(events.contains(&DisplayLinePlanEvent::BpldatWrite {
            hpos: 0x003E,
            x: beam_to_framebuffer_x_unclamped(0x003E),
            plane: 5,
            value: 0xAAAA,
        }));
    }

    #[test]
    fn ehb_palette_indexes_use_half_bright_base_color() {
        let mut palette = Palette::new();
        palette.write_ocs(3, 0x0E86);

        assert_eq!(half_brite_rgb12(0x0E86), 0x0743);
        assert_eq!(palette_index_to_rgb12(palette, 0x23, true), 0x0743);
        assert_eq!(palette_index_to_rgb12(palette, 0x23, false), 0x0E86);
    }

    fn render_sprite_dma_test_frame(
        state: &RenderState,
        ram: &[u8],
        refreshes: [SpritePointerRefresh; 8],
    ) -> Vec<u32> {
        let base_palettes = [state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let base_controls = [ControlState::from_render_state(state); FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];

        render_sprites_with_manual_lines(
            state,
            ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &playfield_mask,
            &mut collision_pixels,
            refreshes,
            &[],
            false,
            None,
        );

        fb
    }

    #[test]
    fn captured_sprite_dma_blocks_sprite_seven_when_ddfstrt_uses_early_fetch_slot() {
        let mut state = blank_state();
        state.dmacon = DMACON_DMAEN | DMACON_SPREN | DMACON_BPLEN;
        state.bplcon0 = 0x1000;
        state.ddfstrt = 0x0028;
        state.ddfstop = 0x0038;
        state.palette.write_ocs(29, 0x0F00);
        let ram = vec![0; 64];
        let base_palettes = [state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];
        let captured = [CapturedSpriteLine {
            sprite: 7,
            hstart: DIW_HSTART_FB0,
            hsub_70ns: false,
            beam_y: PAL_VISIBLE_LINE0,
            data: 0x8000,
            datb: 0,
            attached: false,
            data_ext: [0; 3],
            datb_ext: [0; 3],
            width_words: 1,
        }];

        render_sprites(
            &state,
            &ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &playfield_mask,
            &mut collision_pixels,
            [false; 8],
            &captured,
            true,
        );
        assert_eq!(fb[0], rgb12_to_rgba8(0x0000));

        state.ddfstrt = 0x0038;
        state.ddfstop = 0x0038;
        let base_palettes = [state.palette; FB_HEIGHT];
        let base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        fb.fill(rgb12_to_rgba8(0));
        render_sprites(
            &state,
            &ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &playfield_mask,
            &mut collision_pixels,
            [false; 8],
            &captured,
            true,
        );
        assert_eq!(fb[0], rgb12_to_rgba8(0x0F00));
    }

    #[test]
    fn bplcon3_spres_default_uses_ecs_sprite_width_for_hires_playfield() {
        let mut state = RenderState {
            dmacon: DMACON_DMAEN | DMACON_SPREN,
            bplcon0: 0x8000,
            ..blank_state()
        };
        state.palette.write_ocs(17, 0x0F00);
        let ram = vec![0; 64];
        let base_palettes = [state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];
        let captured = [CapturedSpriteLine {
            sprite: 0,
            hstart: DIW_HSTART_FB0,
            hsub_70ns: false,
            beam_y: PAL_VISIBLE_LINE0,
            data: 0x8000,
            datb: 0,
            attached: false,
            data_ext: [0; 3],
            datb_ext: [0; 3],
            width_words: 1,
        }];

        render_sprites(
            &state,
            &ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &playfield_mask,
            &mut collision_pixels,
            [false; 8],
            &captured,
            true,
        );

        assert_eq!(fb[0], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[1], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[2], rgb12_to_rgba8(0));
    }

    #[test]
    fn bplcon3_spres_default_upgrades_to_70ns_when_shres_is_set() {
        let mut state = RenderState {
            dmacon: DMACON_DMAEN | DMACON_SPREN,
            bplcon0: BPLCON0_SHRES,
            ..blank_state()
        };
        state.palette.write_ocs(17, 0x0F00);
        let ram = vec![0; 64];
        let base_palettes = [state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];
        let captured = [CapturedSpriteLine {
            sprite: 0,
            hstart: DIW_HSTART_FB0,
            hsub_70ns: false,
            beam_y: PAL_VISIBLE_LINE0,
            data: 0x8000,
            datb: 0,
            attached: false,
            data_ext: [0; 3],
            datb_ext: [0; 3],
            width_words: 1,
        }];

        render_sprites(
            &state,
            &ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &playfield_mask,
            &mut collision_pixels,
            [false; 8],
            &captured,
            true,
        );

        assert_eq!(fb[0], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[1], rgb12_to_rgba8(0));
    }

    #[test]
    fn shres_sprite_control_bit4_adds_70ns_horizontal_offset() {
        let (pos, ctl) =
            sprite_control_words(PAL_VISIBLE_LINE0 as u16, 0x2D, DIW_HSTART_FB0 as u16);
        let mut state = RenderState {
            dmacon: DMACON_DMAEN | DMACON_SPREN,
            bplcon0: BPLCON0_SHRES,
            ..blank_state()
        };
        state.palette.write_ocs(17, 0x0F00);
        state.sprpos[0] = pos;
        state.sprctl[0] = ctl | 0x0010;
        state.sprdata[0] = 0x8000;
        state.spr_armed[0] = true;
        let ram = vec![0; 64];
        let base_palettes = [state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];

        render_sprites(
            &state,
            &ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &playfield_mask,
            &mut collision_pixels,
            [false; 8],
            &[],
            false,
        );

        assert_eq!(fb[0], rgb12_to_rgba8(0));
        assert_eq!(fb[1], rgb12_to_rgba8(0x0F00));
    }

    #[test]
    fn bplcon3_spres_hires_draws_one_framebuffer_pixel_per_sprite_bit() {
        let mut state = RenderState {
            dmacon: DMACON_DMAEN | DMACON_SPREN,
            bplcon3: BPLCON3_SPRES_HIRES,
            ..blank_state()
        };
        state.palette.write_ocs(17, 0x0F00);
        let ram = vec![0; 64];
        let base_palettes = [state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];
        let captured = [CapturedSpriteLine {
            sprite: 0,
            hstart: DIW_HSTART_FB0,
            hsub_70ns: false,
            beam_y: PAL_VISIBLE_LINE0,
            data: 0x8000,
            datb: 0,
            attached: false,
            data_ext: [0; 3],
            datb_ext: [0; 3],
            width_words: 1,
        }];

        render_sprites(
            &state,
            &ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &playfield_mask,
            &mut collision_pixels,
            [false; 8],
            &captured,
            true,
        );

        assert_eq!(fb[0], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[1], rgb12_to_rgba8(0));
    }

    #[test]
    fn bplcon3_spres_hires_applies_to_attached_sprite_pairs() {
        let mut state = RenderState {
            dmacon: DMACON_DMAEN | DMACON_SPREN,
            bplcon3: BPLCON3_SPRES_HIRES,
            ..blank_state()
        };
        state.palette.write_ocs(21, 0x00F0);
        let ram = vec![0; 64];
        let base_palettes = [state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];
        let captured = [
            CapturedSpriteLine {
                sprite: 0,
                hstart: DIW_HSTART_FB0,
                hsub_70ns: false,
                beam_y: PAL_VISIBLE_LINE0,
                data: 0x8000,
                datb: 0,
                attached: false,
                data_ext: [0; 3],
                datb_ext: [0; 3],
                width_words: 1,
            },
            CapturedSpriteLine {
                sprite: 1,
                hstart: DIW_HSTART_FB0,
                hsub_70ns: false,
                beam_y: PAL_VISIBLE_LINE0,
                data: 0x8000,
                datb: 0,
                attached: true,
                data_ext: [0; 3],
                datb_ext: [0; 3],
                width_words: 1,
            },
        ];

        render_sprites(
            &state,
            &ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &playfield_mask,
            &mut collision_pixels,
            [false; 8],
            &captured,
            true,
        );

        assert_eq!(fb[0], rgb12_to_rgba8(0x00F0));
        assert_eq!(fb[1], rgb12_to_rgba8(0));
    }

    #[test]
    fn sprite_dma_ignores_unrefreshed_stale_sprite_pointer() {
        let mut ram = vec![0u8; 512 * 1024];
        let sprite_ptr = 0x0100usize;
        let (pos, ctl) = sprite_control_words(
            PAL_VISIBLE_LINE0 as u16,
            PAL_VISIBLE_LINE0 as u16 + 1,
            DIW_HSTART_FB0 as u16,
        );
        put_word(&mut ram, sprite_ptr, pos);
        put_word(&mut ram, sprite_ptr + 2, ctl);
        put_word(&mut ram, sprite_ptr + 4, 0x8000);
        put_word(&mut ram, sprite_ptr + 6, 0);
        put_word(&mut ram, sprite_ptr + 8, 0);
        put_word(&mut ram, sprite_ptr + 10, 0);

        let mut state = blank_state();
        state.dmacon = DMACON_DMAEN | DMACON_SPREN;
        state.sprpt[0] = sprite_ptr as u32;
        state.palette.write_ocs(17, 0x0F00);
        let base_palettes = [state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];

        render_sprites(
            &state,
            &ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &playfield_mask,
            &mut collision_pixels,
            [false; 8],
            &[],
            false,
        );

        assert_eq!(fb[0], rgb12_to_rgba8(0));
    }

    #[test]
    fn manual_sprite_data_writes_affect_only_later_beam_lines() {
        let mut initial_state = blank_state();
        let (pos, ctl) = sprite_control_words(
            PAL_VISIBLE_LINE0 as u16,
            PAL_VISIBLE_LINE0 as u16 + 8,
            DIW_HSTART_FB0 as u16,
        );
        initial_state.sprpos[0] = pos;
        initial_state.sprctl[0] = ctl;
        initial_state.palette.write_ocs(17, 0x0F00);

        let write_line = PAL_VISIBLE_LINE0 + 4;
        let events = [cpu_event(
            write_line as u32,
            COPPER_WAIT_HPOS_FB0 as u32,
            0x144,
            0x8000,
        )];
        let manual_sprite_lines = manual_sprite_lines_from_events(&initial_state, &events);
        assert!(manual_sprite_lines[0]
            .iter()
            .all(|line| line.beam_y >= write_line));

        let base_controls = [ControlState::from_render_state(&initial_state); FB_HEIGHT];
        let mut render_state = initial_state;
        apply_move(&mut render_state, 0x144, 0x8000);
        let ram = vec![0u8; 512 * 1024];
        let base_palettes = [render_state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];

        render_sprites_with_manual_lines(
            &render_state,
            &ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &playfield_mask,
            &mut collision_pixels,
            sprite_pointer_refreshes_from_mask([false; 8]),
            &[],
            false,
            Some(&manual_sprite_lines),
        );

        assert_eq!(fb[0], rgb12_to_rgba8(0));
        assert_eq!(fb[4 * FB_WIDTH], rgb12_to_rgba8(0x0F00));
    }

    #[test]
    fn latched_sprite_vstart_equal_vstop_is_empty() {
        let mut initial_state = blank_state();
        let beam_y = PAL_VISIBLE_LINE0;
        let (pos, ctl) = sprite_control_words(beam_y as u16, beam_y as u16, DIW_HSTART_FB0 as u16);
        initial_state.sprpos[0] = pos;
        initial_state.sprctl[0] = ctl;
        initial_state.sprdata[0] = 0xFFFF;
        initial_state.spr_armed[0] = true;

        let manual_sprite_lines = manual_sprite_lines_from_events_with_visible_line0(
            &initial_state,
            &[],
            &[None; 8],
            PAL_VISIBLE_LINE0,
            FB_HEIGHT,
            true,
        );

        assert!(manual_sprite_lines[0].is_empty());
    }

    #[test]
    fn direct_sprite_data_write_ignores_dma_vertical_window() {
        let mut initial_state = blank_state();
        let beam_y = PAL_VISIBLE_LINE0 + 12;
        let (pos, ctl) = sprite_control_words(
            PAL_VISIBLE_LINE0 as u16,
            PAL_VISIBLE_LINE0 as u16,
            DIW_HSTART_FB0 as u16,
        );
        initial_state.sprpos[0] = pos;
        initial_state.sprctl[0] = ctl;

        let manual_sprite_lines = manual_sprite_lines_from_events_with_visible_line0(
            &initial_state,
            &[cpu_event(
                beam_y as u32,
                COPPER_WAIT_HPOS_FB0 as u32,
                0x144,
                0xFFFF,
            )],
            &[None; 8],
            PAL_VISIBLE_LINE0,
            FB_HEIGHT,
            false,
        );

        assert!(manual_sprite_lines[0]
            .iter()
            .any(|line| line.beam_y == beam_y));
    }

    #[test]
    fn manual_sprite_data_write_replicates_words_across_fmode_wide_register() {
        let mut initial_state = blank_state();
        initial_state.agnus_revision = AgnusRevision::AgaAlice;
        initial_state.fmode = 0x000C; // SPR32 | SPAGEM: 64-pixel sprites
        let (pos, ctl) = sprite_control_words(
            PAL_VISIBLE_LINE0 as u16,
            PAL_VISIBLE_LINE0 as u16 + 4,
            DIW_HSTART_FB0 as u16,
        );
        initial_state.sprpos[0] = pos;
        initial_state.sprctl[0] = ctl;

        let events = [
            cpu_event(PAL_VISIBLE_LINE0 as u32, 0, 0x146, 0x00FF),
            cpu_event(PAL_VISIBLE_LINE0 as u32, 0, 0x144, 0x8001),
        ];
        let manual_sprite_lines = manual_sprite_lines_from_events(&initial_state, &events);
        let line = manual_sprite_lines[0]
            .iter()
            .find(|line| line.beam_y == PAL_VISIBLE_LINE0 + 1)
            .expect("armed manual sprite line");
        assert_eq!(line.width_words, 4);
        assert_eq!(line.data, 0x8001);
        assert_eq!(line.data_ext, [0x8001; 3]);
        assert_eq!(line.datb, 0x00FF);
        assert_eq!(line.datb_ext, [0x00FF; 3]);
    }

    #[test]
    fn manual_sprite_width_stays_one_word_without_aga_lisa() {
        let mut initial_state = blank_state();
        // FMODE can only be nonzero on Alice, but the manual sprite replay
        // must not widen on a pre-AGA revision even if state carries a value.
        initial_state.fmode = 0x000C;
        let (pos, ctl) = sprite_control_words(
            PAL_VISIBLE_LINE0 as u16,
            PAL_VISIBLE_LINE0 as u16 + 4,
            DIW_HSTART_FB0 as u16,
        );
        initial_state.sprpos[0] = pos;
        initial_state.sprctl[0] = ctl;

        let events = [cpu_event(PAL_VISIBLE_LINE0 as u32, 0, 0x144, 0x8001)];
        let manual_sprite_lines = manual_sprite_lines_from_events(&initial_state, &events);
        let line = manual_sprite_lines[0]
            .iter()
            .find(|line| line.beam_y == PAL_VISIBLE_LINE0 + 1)
            .expect("armed manual sprite line");
        assert_eq!(line.width_words, 1);
        assert_eq!(line.data_ext, [0; 3]);
    }

    #[test]
    fn manual_sprite_fmode_event_changes_width_for_later_lines() {
        let mut initial_state = blank_state();
        initial_state.agnus_revision = AgnusRevision::AgaAlice;
        let (pos, ctl) = sprite_control_words(
            PAL_VISIBLE_LINE0 as u16,
            PAL_VISIBLE_LINE0 as u16 + 8,
            DIW_HSTART_FB0 as u16,
        );
        initial_state.sprpos[0] = pos;
        initial_state.sprctl[0] = ctl;

        let fmode_line = PAL_VISIBLE_LINE0 + 4;
        let events = [
            cpu_event(PAL_VISIBLE_LINE0 as u32, 0, 0x144, 0x8001),
            cpu_event(fmode_line as u32, 0, 0x1FC, 0x0004),
        ];
        let manual_sprite_lines = manual_sprite_lines_from_events(&initial_state, &events);
        for line in &manual_sprite_lines[0] {
            if line.beam_y < fmode_line {
                assert_eq!(line.width_words, 1, "beam_y={}", line.beam_y);
            } else {
                assert_eq!(line.width_words, 2, "beam_y={}", line.beam_y);
                assert_eq!(line.data_ext[0], 0x8001);
            }
        }
        assert!(manual_sprite_lines[0]
            .iter()
            .any(|line| line.beam_y >= fmode_line));
    }

    #[test]
    fn sprite_dma_capture_suppresses_latched_manual_sprite_spans() {
        let mut initial_state = blank_state();
        let (pos, ctl) = sprite_control_words(
            PAL_VISIBLE_LINE0 as u16,
            PAL_VISIBLE_LINE0 as u16 + 8,
            DIW_HSTART_FB0 as u16,
        );
        initial_state.sprpos[0] = pos;
        initial_state.sprctl[0] = ctl;
        initial_state.sprdata[0] = 0xFFFF;
        initial_state.spr_armed[0] = true;

        let manual_sprite_lines = manual_sprite_lines_from_events_with_visible_line0(
            &initial_state,
            &[],
            &[None; 8],
            PAL_VISIBLE_LINE0,
            FB_HEIGHT,
            false,
        );

        assert!(manual_sprite_lines[0].is_empty());
    }

    #[test]
    fn manual_sprite_replay_does_not_seed_from_frame_start_data_latch() {
        let mut initial_state = blank_state();
        let (pos, ctl) = sprite_control_words(PAL_VISIBLE_LINE0 as u16, 0, DIW_HSTART_FB0 as u16);
        initial_state.sprpos[0] = pos;
        initial_state.sprctl[0] = ctl;
        initial_state.sprdata[0] = 0xFFFF;
        initial_state.spr_armed[0] = true;

        let manual_sprite_lines = manual_sprite_lines_from_events_with_visible_line0(
            &initial_state,
            &[],
            &[None; 8],
            PAL_VISIBLE_LINE0,
            FB_HEIGHT,
            false,
        );

        assert!(manual_sprite_lines[0].is_empty());
    }

    #[test]
    fn pre_dma_position_write_preserves_armed_latch_for_later_position_write() {
        let mut initial_state = blank_state();
        let beam_y = PAL_VISIBLE_LINE0;
        let (old_pos, ctl) =
            sprite_control_words(beam_y as u16, beam_y as u16 + 1, DIW_HSTART_FB0 as u16);
        let pre_dma_hstart = (DIW_HSTART_FB0 + 16) as u16;
        let (pre_dma_pos, _) =
            sprite_control_words(beam_y as u16, beam_y as u16 + 1, pre_dma_hstart);
        let post_dma_hstart = (DIW_HSTART_FB0 + 32) as u16;
        let (post_dma_pos, _) =
            sprite_control_words(beam_y as u16, beam_y as u16 + 1, post_dma_hstart);
        initial_state.sprpos[0] = old_pos;
        initial_state.sprctl[0] = ctl;
        initial_state.sprdata[0] = 0x8000;
        initial_state.spr_armed[0] = true;

        let manual_sprite_lines = manual_sprite_lines_from_events_with_visible_line0(
            &initial_state,
            &[
                cpu_event(
                    beam_y as u32,
                    SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1,
                    0x140,
                    pre_dma_pos,
                ),
                cpu_event(
                    beam_y as u32,
                    SPRITE_DMA_PAIR_CAPTURE_HPOS[0] + 1,
                    0x140,
                    post_dma_pos,
                ),
            ],
            &[None; 8],
            PAL_VISIBLE_LINE0,
            FB_HEIGHT,
            false,
        );

        assert!(manual_sprite_lines[0].iter().any(|line| {
            line.beam_y == beam_y
                && line.hstart == i32::from(post_dma_hstart)
                && line.data == 0x8000
        }));
    }

    #[test]
    fn post_dma_position_write_reuses_armed_frame_start_data_latch() {
        let mut initial_state = blank_state();
        let beam_y = PAL_VISIBLE_LINE0;
        let (old_pos, ctl) =
            sprite_control_words(beam_y as u16, beam_y as u16 + 1, DIW_HSTART_FB0 as u16);
        let reused_hstart = (DIW_HSTART_FB0 + 32) as u16;
        let (new_pos, _) = sprite_control_words(beam_y as u16, beam_y as u16 + 1, reused_hstart);
        initial_state.sprpos[0] = old_pos;
        initial_state.sprctl[0] = ctl;
        initial_state.sprdata[0] = 0x8000;
        initial_state.spr_armed[0] = true;

        let manual_sprite_lines = manual_sprite_lines_from_events_with_visible_line0(
            &initial_state,
            &[cpu_event(
                beam_y as u32,
                SPRITE_DMA_PAIR_CAPTURE_HPOS[0] + 1,
                0x140,
                new_pos,
            )],
            &[None; 8],
            PAL_VISIBLE_LINE0,
            FB_HEIGHT,
            false,
        );

        assert!(manual_sprite_lines[0].iter().any(|line| {
            line.beam_y == beam_y && line.hstart == i32::from(reused_hstart) && line.data == 0x8000
        }));
    }

    #[test]
    fn offscreen_pre_dma_position_write_preserves_armed_latch_for_same_line_retime() {
        let mut initial_state = blank_state();
        let beam_y = 99;
        initial_state.sprpos[3] = 0x5020;
        initial_state.sprctl[3] = 0x0602;
        initial_state.sprdata[3] = 0xE92D;
        initial_state.sprdatb[3] = 0x16FF;
        initial_state.spr_armed[3] = true;

        let manual_sprite_lines = manual_sprite_lines_from_events_with_visible_line0(
            &initial_state,
            &[
                cpu_event(beam_y as u32, 8, 0x158, 0x5020),
                cpu_event(beam_y as u32, 64, 0x158, 0x503C),
            ],
            &[None; 8],
            PAL_VISIBLE_LINE0,
            FB_HEIGHT,
            false,
        );

        assert!(manual_sprite_lines[3].iter().any(|line| {
            line.beam_y == beam_y
                && line.hstart == 0x78
                && line.data == 0xE92D
                && line.datb == 0x16FF
        }));
    }

    #[test]
    fn pre_visible_data_write_seeds_latch_without_direct_output_guard() {
        let mut initial_state = blank_state();
        initial_state.sprpos[3] = 0x5020;

        let manual_sprite_lines = manual_sprite_lines_from_events_with_visible_line0(
            &initial_state,
            &[
                cpu_event(0, 78, 0x15A, 0x0602),
                cpu_event(0, 110, 0x15E, 0x16FF),
                cpu_event(0, 142, 0x15C, 0xE92D),
                cpu_event(99, 8, 0x158, 0x5020),
                cpu_event(99, 64, 0x158, 0x503C),
            ],
            &[None; 8],
            PAL_VISIBLE_LINE0,
            FB_HEIGHT,
            false,
        );

        assert!(manual_sprite_lines[3].iter().any(|line| {
            line.beam_y == 99 && line.hstart == 0x78 && line.data == 0xE92D && line.datb == 0x16FF
        }));
    }

    #[test]
    fn manual_sprite_replay_starts_from_beam_timed_data_write() {
        let mut initial_state = blank_state();
        let (pos, ctl) = sprite_control_words(
            PAL_VISIBLE_LINE0 as u16,
            PAL_VISIBLE_LINE0 as u16 + 4,
            DIW_HSTART_FB0 as u16,
        );
        initial_state.sprpos[0] = pos;
        initial_state.sprctl[0] = ctl;

        let manual_sprite_lines = manual_sprite_lines_from_events_with_visible_line0(
            &initial_state,
            &[cpu_event(
                PAL_VISIBLE_LINE0 as u32,
                COPPER_WAIT_HPOS_FB0 as u32,
                0x144,
                0xFFFF,
            )],
            &[None; 8],
            PAL_VISIBLE_LINE0,
            FB_HEIGHT,
            false,
        );

        assert!(manual_sprite_lines[0]
            .iter()
            .any(|line| line.beam_y == PAL_VISIBLE_LINE0));
    }

    #[test]
    fn early_line_position_write_does_not_reuse_previous_manual_sprite_data() {
        let mut initial_state = blank_state();
        let beam_y = PAL_VISIBLE_LINE0;
        let (pos, ctl) =
            sprite_control_words(beam_y as u16, beam_y as u16 + 4, DIW_HSTART_FB0 as u16);
        initial_state.sprpos[0] = pos;
        initial_state.sprctl[0] = ctl;

        let (next_pos, _) = sprite_control_words(
            (beam_y + 1) as u16,
            (beam_y + 2) as u16,
            (DIW_HSTART_FB0 + 32) as u16,
        );
        let manual_sprite_lines = manual_sprite_lines_from_events_with_visible_line0(
            &initial_state,
            &[
                cpu_event(beam_y as u32, COPPER_WAIT_HPOS_FB0 as u32, 0x144, 0xFFFF),
                cpu_event((beam_y + 1) as u32, 4, 0x140, next_pos),
            ],
            &[None; 8],
            PAL_VISIBLE_LINE0,
            FB_HEIGHT,
            false,
        );

        assert!(manual_sprite_lines[0]
            .iter()
            .any(|line| line.beam_y == beam_y && line.data == 0xFFFF));
        assert!(manual_sprite_lines[0]
            .iter()
            .all(|line| line.beam_y != beam_y + 1));
    }

    #[test]
    fn dma_seeded_sprite_reuse_keeps_later_register_data_on_same_line() {
        let beam_y = PAL_VISIBLE_LINE0;
        let mut manual_lines = vec![Vec::new(); 8];
        manual_lines[0].push(SpriteLine {
            hstart: DIW_HSTART_FB0 + 96,
            hsub_70ns: false,
            beam_y,
            data: 0xFFFF,
            datb: 0,
            data_ext: [0; 3],
            datb_ext: [0; 3],
            width_words: 1,
            attached: false,
            x_start: 208,
            x_stop: FB_WIDTH,
        });

        let mut dma_seeded = vec![Vec::new(); 8];
        dma_seeded[0].push(SpriteLine {
            hstart: DIW_HSTART_FB0 + 96,
            hsub_70ns: false,
            beam_y,
            data: 0x8000,
            datb: 0,
            data_ext: [0; 3],
            datb_ext: [0; 3],
            width_words: 1,
            attached: false,
            x_start: 112,
            x_stop: 240,
        });

        merge_dma_seeded_manual_sprite_lines(&mut manual_lines, dma_seeded);

        assert!(manual_lines[0].iter().any(|line| {
            line.beam_y == beam_y
                && line.data == 0xFFFF
                && line.x_start == 208
                && line.x_stop == FB_WIDTH
        }));
        assert!(manual_lines[0].iter().any(|line| {
            line.beam_y == beam_y
                && line.data == 0x8000
                && line.x_start == 112
                && line.x_stop == 208
        }));
        assert!(manual_lines[0]
            .iter()
            .all(|line| line.data != 0x8000 || line.x_stop <= 208));
    }

    #[test]
    fn held_sprite_after_dma_disable_persists_past_descriptor_vstop() {
        let mut initial_state = blank_state();
        let held_vstart = PAL_VISIBLE_LINE0;
        let held_vstop = PAL_VISIBLE_LINE0 + 4;
        let live_vstart = PAL_VISIBLE_LINE0 + 32;
        let live_vstop = PAL_VISIBLE_LINE0 + 40;
        let live_hstart = DIW_HSTART_FB0 + 8;
        let (pos, ctl) =
            sprite_control_words(live_vstart as u16, live_vstop as u16, live_hstart as u16);
        initial_state.sprpos[0] = pos;
        initial_state.sprctl[0] = ctl;

        let mut held = [None; 8];
        held[0] = Some(HeldSpriteLine {
            line: CapturedSpriteLine {
                sprite: 0,
                hstart: DIW_HSTART_FB0,
                hsub_70ns: false,
                beam_y: held_vstart,
                data: 0x8000,
                datb: 0,
                data_ext: [0; 3],
                datb_ext: [0; 3],
                width_words: 1,
                attached: false,
            },
            vstart: held_vstart,
            vstop: held_vstop,
        });

        let manual_sprite_lines = manual_sprite_lines_from_events_with_visible_line0(
            &initial_state,
            &[cpu_event(
                held_vstart as u32,
                COPPER_WAIT_HPOS_FB0 as u32,
                0x140,
                pos,
            )],
            &held,
            PAL_VISIBLE_LINE0,
            FB_HEIGHT,
            true,
        );

        let line = manual_sprite_lines[0]
            .iter()
            .find(|line| line.beam_y == held_vstart)
            .expect("held sprite remains visible in its DMA vertical window");
        assert_eq!(line.hstart, live_hstart);
        assert_eq!(line.x_start, 0);
        assert!(manual_sprite_lines[0]
            .iter()
            .any(|line| line.beam_y == held_vstop && line.hstart == live_hstart));
        assert!(manual_sprite_lines[0]
            .iter()
            .any(|line| line.beam_y > held_vstop && line.hstart == live_hstart));
    }

    #[test]
    fn held_sprite_starts_from_dma_loaded_position_and_control() {
        let initial_state = blank_state();
        let held_vstart = PAL_VISIBLE_LINE0;
        let held_hstart = DIW_HSTART_FB0 + 16;

        let mut held = [None; 8];
        held[1] = Some(HeldSpriteLine {
            line: CapturedSpriteLine {
                sprite: 1,
                hstart: held_hstart,
                hsub_70ns: true,
                beam_y: held_vstart,
                data: 0x8000,
                datb: 0,
                data_ext: [0; 3],
                datb_ext: [0; 3],
                width_words: 1,
                attached: true,
            },
            vstart: held_vstart,
            vstop: held_vstart + 1,
        });

        let manual_sprite_lines = manual_sprite_lines_from_events_with_visible_line0(
            &initial_state,
            &[],
            &held,
            PAL_VISIBLE_LINE0,
            FB_HEIGHT,
            true,
        );

        let line = manual_sprite_lines[1]
            .iter()
            .find(|line| line.beam_y == held_vstart)
            .expect("held sprite keeps DMA-loaded position without a register write");
        assert_eq!(line.hstart, held_hstart);
        assert!(line.hsub_70ns);
        assert!(line.attached);
    }

    #[test]
    fn manual_sprite_position_write_before_hstart_uses_sprite_compare_domain() {
        let mut initial_state = blank_state();
        let beam_y = PAL_VISIBLE_LINE0;
        let old_hstart = 384;
        let new_hstart = 192;
        let (old_pos, old_ctl) = sprite_control_words(beam_y as u16, beam_y as u16 + 1, old_hstart);
        let (new_pos, _) = sprite_control_words(beam_y as u16, beam_y as u16 + 1, new_hstart);
        initial_state.sprpos[2] = old_pos;
        initial_state.sprctl[2] = old_ctl;
        initial_state.sprdata[2] = 0xFFFF;
        initial_state.spr_armed[2] = true;

        // SPRxPOS writes update the Denise sprite comparator before the
        // general colour-output register domain reaches the same beam hpos.
        // The repositioned sprite must not be clipped to that later output x.
        let event_hpos = 96;
        let manual_sprite_lines = manual_sprite_lines_from_events(
            &initial_state,
            &[cpu_event(beam_y as u32, event_hpos, 0x150, new_pos)],
        );

        let old_line = manual_sprite_lines[2]
            .iter()
            .find(|line| line.beam_y == beam_y && line.hstart == old_hstart as i32)
            .expect("old position interval");
        let new_line = manual_sprite_lines[2]
            .iter()
            .find(|line| line.beam_y == beam_y && line.hstart == new_hstart as i32)
            .expect("new position interval");
        let sprite_position_x = sprite_position_write_framebuffer_x(event_hpos);
        let colour_output_x = ((event_hpos as i32 - COPPER_WAIT_HPOS_FB0) * 4) as usize;

        assert_eq!(old_line.x_stop, sprite_position_x);
        assert_eq!(new_line.x_start, sprite_position_x);
        assert_ne!(new_line.x_start, colour_output_x);
    }

    #[test]
    fn manual_sprite_position_writes_use_denise_compare_lag() {
        let mut initial_state = blank_state();
        let beam_y = PAL_VISIBLE_LINE0;
        let initial_hstart = 64;
        let first_hstart = 114;
        let second_hstart = 130;
        let (initial_pos, ctl) =
            sprite_control_words(beam_y as u16, beam_y as u16 + 1, initial_hstart);
        let (first_pos, _) = sprite_control_words(beam_y as u16, beam_y as u16 + 1, first_hstart);
        let (second_pos, _) = sprite_control_words(beam_y as u16, beam_y as u16 + 1, second_hstart);
        initial_state.sprpos[0] = initial_pos;
        initial_state.sprctl[0] = ctl;
        initial_state.sprdata[0] = 0xFFFF;
        initial_state.spr_armed[0] = true;

        let first_hpos = 64;
        let second_hpos = 72;
        let manual_sprite_lines = manual_sprite_lines_from_events(
            &initial_state,
            &[
                cpu_event(beam_y as u32, first_hpos, 0x140, first_pos),
                cpu_event(beam_y as u32, second_hpos, 0x140, second_pos),
            ],
        );

        let first_line = manual_sprite_lines[0]
            .iter()
            .find(|line| line.beam_y == beam_y && line.hstart == first_hstart as i32)
            .expect("first position interval");
        let second_line = manual_sprite_lines[0]
            .iter()
            .find(|line| line.beam_y == beam_y && line.hstart == second_hstart as i32)
            .expect("second position interval");
        let first_base_x = ((first_hstart as i32 - DIW_HSTART_FB0) * 2) as usize;
        let second_base_x = ((second_hstart as i32 - DIW_HSTART_FB0) * 2) as usize;
        let base_control = ControlState::from_render_state(&initial_state);

        assert_eq!(first_line.x_start, first_base_x);
        assert!(first_line.x_stop > second_base_x);
        assert_eq!(second_line.x_start, second_base_x);
        assert_eq!(
            first_line.x_start,
            sprite_position_write_framebuffer_x(first_hpos)
        );
        assert_eq!(
            second_line.x_start,
            sprite_position_write_framebuffer_x(second_hpos)
        );
        assert_eq!(
            sprite_line_pixel_bits_at(first_line, second_base_x as i32 - 1, base_control, &[]),
            1
        );
        assert_eq!(
            sprite_line_pixel_bits_at(second_line, second_base_x as i32, base_control, &[]),
            1
        );
    }

    #[test]
    fn manual_sprite_position_write_does_not_truncate_started_word() {
        let mut initial_state = blank_state();
        let beam_y = PAL_VISIBLE_LINE0;
        let initial_hstart = 64;
        let first_hstart = 126;
        let second_hstart = 142;
        let (initial_pos, ctl) =
            sprite_control_words(beam_y as u16, beam_y as u16 + 1, initial_hstart);
        let (first_pos, _) = sprite_control_words(beam_y as u16, beam_y as u16 + 1, first_hstart);
        let (second_pos, _) = sprite_control_words(beam_y as u16, beam_y as u16 + 1, second_hstart);
        initial_state.sprpos[0] = initial_pos;
        initial_state.sprctl[0] = ctl;
        initial_state.sprdata[0] = 0xFFFF;
        initial_state.spr_armed[0] = true;

        let manual_sprite_lines = manual_sprite_lines_from_events(
            &initial_state,
            &[
                cpu_event(beam_y as u32, 64, 0x140, first_pos),
                cpu_event(beam_y as u32, 72, 0x140, second_pos),
            ],
        );

        let first_line = manual_sprite_lines[0]
            .iter()
            .find(|line| line.beam_y == beam_y && line.hstart == first_hstart as i32)
            .expect("first position interval");
        let second_line = manual_sprite_lines[0]
            .iter()
            .find(|line| line.beam_y == beam_y && line.hstart == second_hstart as i32)
            .expect("second position interval");
        let first_base_x = (first_hstart as i32 - DIW_HSTART_FB0) * 2;
        let second_base_x = (second_hstart as i32 - DIW_HSTART_FB0) * 2;
        let second_write_x = sprite_position_write_framebuffer_x(72) as i32;
        let base_control = ControlState::from_render_state(&initial_state);

        assert!(second_write_x > first_base_x);
        assert!(second_write_x < second_base_x);
        assert!(first_line.x_stop as i32 > second_base_x);
        assert_eq!(
            sprite_line_pixel_bits_at(first_line, second_write_x + 2, base_control, &[]),
            1,
            "a POS write must not cut off a word that has already started"
        );
        assert_eq!(
            sprite_line_pixel_bits_at(first_line, second_base_x - 1, base_control, &[]),
            1
        );
        assert_eq!(
            sprite_line_pixel_bits_at(second_line, second_base_x, base_control, &[]),
            1
        );
    }

    #[test]
    fn manual_sprite_position_write_on_compare_boundary_preserves_started_word() {
        let mut initial_state = blank_state();
        let beam_y = PAL_VISIBLE_LINE0;
        let initial_hstart = 64;
        let first_hstart = 126;
        let second_hstart = 142;
        let (initial_pos, ctl) =
            sprite_control_words(beam_y as u16, beam_y as u16 + 1, initial_hstart);
        let (first_pos, _) = sprite_control_words(beam_y as u16, beam_y as u16 + 1, first_hstart);
        let (second_pos, _) = sprite_control_words(beam_y as u16, beam_y as u16 + 1, second_hstart);
        initial_state.sprpos[0] = initial_pos;
        initial_state.sprctl[0] = ctl;
        initial_state.sprdata[0] = 0xFFFF;
        initial_state.spr_armed[0] = true;

        let boundary_hpos = u32::from(first_hstart / 2) + SPRITE_REGISTER_WRITE_PIPELINE_CCK;
        let manual_sprite_lines = manual_sprite_lines_from_events(
            &initial_state,
            &[
                cpu_event(beam_y as u32, 64, 0x140, first_pos),
                cpu_event(beam_y as u32, boundary_hpos, 0x140, second_pos),
            ],
        );

        let first_line = manual_sprite_lines[0]
            .iter()
            .find(|line| line.beam_y == beam_y && line.hstart == first_hstart as i32)
            .expect("first position interval");
        let base_control = ControlState::from_render_state(&initial_state);
        let first_base_x = (first_hstart as i32 - DIW_HSTART_FB0) * 2;

        assert_eq!(
            sprite_position_write_framebuffer_x(boundary_hpos),
            first_base_x as usize
        );
        assert_eq!(
            sprite_line_pixel_bits_at(first_line, first_base_x, base_control, &[]),
            1,
            "a POS write on the comparator boundary must not cancel the word"
        );
    }

    #[test]
    fn manual_sprite_data_write_before_compare_uses_sprite_compare_domain() {
        let mut initial_state = blank_state();
        let beam_y = PAL_VISIBLE_LINE0;
        let hstart = 240;
        let (pos, ctl) = sprite_control_words(beam_y as u16, beam_y as u16 + 1, hstart);
        initial_state.sprpos[2] = pos;
        initial_state.sprctl[2] = ctl;
        initial_state.palette.write_ocs(25, 0x0F00);

        let event_hpos = 116;
        let data_x = sprite_data_write_framebuffer_x(event_hpos);
        let colour_output_x = beam_to_framebuffer_x_unclamped(event_hpos) as usize;
        let base_x = sprite_nominal_base_framebuffer_x(pos, ctl) as usize;
        assert!(data_x < base_x);
        assert!(colour_output_x > base_x);

        let manual_sprite_lines = manual_sprite_lines_from_events(
            &initial_state,
            &[cpu_event(beam_y as u32, event_hpos, 0x154, 0x8000)],
        );

        let line = manual_sprite_lines[2]
            .iter()
            .find(|line| line.beam_y == beam_y)
            .expect("data write before the comparator affects the current scanline");
        assert_eq!(line.x_start, data_x);
        assert_eq!(line.data, 0x8000);
        assert_eq!(
            sprite_line_pixel_bits_at(
                line,
                base_x as i32,
                ControlState::from_render_state(&initial_state),
                &[],
            ),
            1
        );
    }

    #[test]
    fn attached_manual_sprite_data_write_before_compare_uses_sprite_compare_domain() {
        let mut initial_state = blank_state();
        let beam_y = PAL_VISIBLE_LINE0;
        let hstart = 240;
        let (pos, ctl) = sprite_control_words(beam_y as u16, beam_y as u16 + 1, hstart);
        initial_state.sprpos[0] = pos;
        initial_state.sprctl[0] = ctl;
        initial_state.sprpos[1] = pos;
        initial_state.sprctl[1] = ctl | 0x0080;
        initial_state.palette.write_ocs(20, 0x0F00);

        let event_hpos = 116;
        let data_x = sprite_data_write_framebuffer_x(event_hpos);
        let colour_output_x = beam_to_framebuffer_x_unclamped(event_hpos) as usize;
        let base_x = sprite_nominal_base_framebuffer_x(pos, ctl) as usize;
        assert!(data_x < base_x);
        assert!(colour_output_x > base_x);

        let events = [
            cpu_event(beam_y as u32, event_hpos, 0x144, 0),
            cpu_event(beam_y as u32, event_hpos, 0x14C, 0x8000),
        ];
        let manual_sprite_lines = manual_sprite_lines_from_events(&initial_state, &events);

        let base_controls = [ControlState::from_render_state(&initial_state); FB_HEIGHT];
        let mut render_state = initial_state;
        apply_move(&mut render_state, 0x144, 0);
        apply_move(&mut render_state, 0x14C, 0x8000);
        let ram = vec![0u8; 512 * 1024];
        let base_palettes = [render_state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];

        render_sprites_with_manual_lines(
            &render_state,
            &ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &playfield_mask,
            &mut collision_pixels,
            sprite_pointer_refreshes_from_mask([false; 8]),
            &[],
            false,
            Some(&manual_sprite_lines),
        );

        assert_eq!(fb[base_x], rgb12_to_rgba8(0x0F00));
    }

    #[test]
    fn manual_sprite_data_write_after_compare_waits_for_next_scanline() {
        let mut initial_state = blank_state();
        let (pos, ctl) = sprite_control_words(
            PAL_VISIBLE_LINE0 as u16,
            PAL_VISIBLE_LINE0 as u16 + 1,
            DIW_HSTART_FB0 as u16,
        );
        initial_state.sprpos[0] = pos;
        initial_state.sprctl[0] = ctl;
        initial_state.palette.write_ocs(17, 0x0F00);

        let after_compare_hpos =
            (DIW_HSTART_FB0 as u32 / 2) + SPRITE_REGISTER_WRITE_PIPELINE_CCK + 2;
        assert!(sprite_data_write_framebuffer_x(after_compare_hpos) > 0);
        let events = [cpu_event(
            PAL_VISIBLE_LINE0 as u32,
            after_compare_hpos,
            0x144,
            0xA000,
        )];
        let manual_sprite_lines = manual_sprite_lines_from_events(&initial_state, &events);
        assert!(manual_sprite_lines[0]
            .iter()
            .all(|line| line.beam_y != PAL_VISIBLE_LINE0));
        assert!(manual_sprite_lines[0]
            .iter()
            .any(|line| line.beam_y == PAL_VISIBLE_LINE0 + 1 && line.x_start == 0));

        let base_controls = [ControlState::from_render_state(&initial_state); FB_HEIGHT];
        let mut render_state = initial_state;
        apply_move(&mut render_state, 0x144, 0xA000);
        let ram = vec![0u8; 512 * 1024];
        let base_palettes = [render_state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];

        render_sprites_with_manual_lines(
            &render_state,
            &ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &playfield_mask,
            &mut collision_pixels,
            sprite_pointer_refreshes_from_mask([false; 8]),
            &[],
            false,
            Some(&manual_sprite_lines),
        );

        assert_eq!(fb[0], rgb12_to_rgba8(0));
        assert_eq!(fb[4], rgb12_to_rgba8(0));
        assert_eq!(fb[FB_WIDTH], rgb12_to_rgba8(0x0F00));
    }

    #[test]
    fn manual_sprite_datb_write_after_compare_waits_for_next_scanline() {
        let mut initial_state = blank_state();
        let (pos, ctl) = sprite_control_words(
            PAL_VISIBLE_LINE0 as u16,
            PAL_VISIBLE_LINE0 as u16 + 1,
            DIW_HSTART_FB0 as u16,
        );
        initial_state.sprpos[0] = pos;
        initial_state.sprctl[0] = ctl;
        initial_state.palette.write_ocs(17, 0x0F00);
        initial_state.palette.write_ocs(19, 0x00F0);

        let after_compare_hpos =
            (DIW_HSTART_FB0 as u32 / 2) + SPRITE_REGISTER_WRITE_PIPELINE_CCK + 2;
        assert!(sprite_data_write_framebuffer_x(after_compare_hpos) > 0);
        let events = [
            cpu_event(
                PAL_VISIBLE_LINE0 as u32,
                COPPER_WAIT_HPOS_FB0 as u32,
                0x144,
                0xFFFF,
            ),
            cpu_event(PAL_VISIBLE_LINE0 as u32, after_compare_hpos, 0x146, 0xFFFF),
        ];
        let manual_sprite_lines = manual_sprite_lines_from_events(&initial_state, &events);

        let base_controls = [ControlState::from_render_state(&initial_state); FB_HEIGHT];
        let mut render_state = initial_state;
        apply_move(&mut render_state, 0x144, 0xFFFF);
        apply_move(&mut render_state, 0x146, 0xFFFF);
        let ram = vec![0u8; 512 * 1024];
        let base_palettes = [render_state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];

        render_sprites_with_manual_lines(
            &render_state,
            &ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &playfield_mask,
            &mut collision_pixels,
            sprite_pointer_refreshes_from_mask([false; 8]),
            &[],
            false,
            Some(&manual_sprite_lines),
        );

        assert_eq!(fb[0], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[4], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[FB_WIDTH], rgb12_to_rgba8(0x00F0));
    }

    #[test]
    fn manual_sprite_control_write_after_compare_preserves_loaded_word() {
        let mut initial_state = blank_state();
        let (pos, ctl) = sprite_control_words(
            PAL_VISIBLE_LINE0 as u16,
            PAL_VISIBLE_LINE0 as u16 + 1,
            DIW_HSTART_FB0 as u16,
        );
        initial_state.sprpos[0] = pos;
        initial_state.sprctl[0] = ctl;
        initial_state.sprdata[0] = 0xFFFF;
        initial_state.spr_armed[0] = true;
        initial_state.palette.write_ocs(17, 0x0F00);

        let events = [
            cpu_event(
                PAL_VISIBLE_LINE0 as u32,
                (COPPER_WAIT_HPOS_FB0 + 2) as u32,
                0x142,
                ctl,
            ),
            cpu_event(
                PAL_VISIBLE_LINE0 as u32,
                (COPPER_WAIT_HPOS_FB0 + 4) as u32,
                0x144,
                0xFFFF,
            ),
        ];
        let manual_sprite_lines = manual_sprite_lines_from_events(&initial_state, &events);

        let base_controls = [ControlState::from_render_state(&initial_state); FB_HEIGHT];
        let mut render_state = initial_state;
        apply_move(&mut render_state, 0x142, ctl);
        apply_move(&mut render_state, 0x144, 0xFFFF);
        let ram = vec![0u8; 512 * 1024];
        let base_palettes = [render_state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];

        render_sprites_with_manual_lines(
            &render_state,
            &ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &playfield_mask,
            &mut collision_pixels,
            sprite_pointer_refreshes_from_mask([false; 8]),
            &[],
            false,
            Some(&manual_sprite_lines),
        );

        assert_eq!(fb[0], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[7], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[8], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[15], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[16], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[32], rgb12_to_rgba8(0));
    }

    #[test]
    fn attached_manual_sprite_writes_draw_odd_bits_without_even_data_bits() {
        let mut initial_state = blank_state();
        let (pos, ctl) = sprite_control_words(
            PAL_VISIBLE_LINE0 as u16,
            PAL_VISIBLE_LINE0 as u16 + 1,
            DIW_HSTART_FB0 as u16,
        );
        initial_state.sprpos[0] = pos;
        initial_state.sprctl[0] = ctl;
        initial_state.sprpos[1] = pos;
        initial_state.sprctl[1] = ctl | 0x0080;
        initial_state.palette.write_ocs(20, 0x0F00);

        let events = [
            cpu_event(
                PAL_VISIBLE_LINE0 as u32,
                COPPER_WAIT_HPOS_FB0 as u32,
                0x144,
                0,
            ),
            cpu_event(
                PAL_VISIBLE_LINE0 as u32,
                COPPER_WAIT_HPOS_FB0 as u32,
                0x14C,
                0x8000,
            ),
        ];
        let manual_sprite_lines = manual_sprite_lines_from_events(&initial_state, &events);

        let base_controls = [ControlState::from_render_state(&initial_state); FB_HEIGHT];
        let mut render_state = initial_state;
        apply_move(&mut render_state, 0x144, 0);
        apply_move(&mut render_state, 0x14C, 0x8000);
        let ram = vec![0u8; 512 * 1024];
        let base_palettes = [render_state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];

        render_sprites_with_manual_lines(
            &render_state,
            &ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &playfield_mask,
            &mut collision_pixels,
            sprite_pointer_refreshes_from_mask([false; 8]),
            &[],
            false,
            Some(&manual_sprite_lines),
        );

        assert_eq!(fb[0], rgb12_to_rgba8(0x0F00));
    }

    #[test]
    fn attached_manual_sprite_data_after_compare_waits_for_next_scanline() {
        let mut initial_state = blank_state();
        let (pos, ctl) = sprite_control_words(
            PAL_VISIBLE_LINE0 as u16,
            PAL_VISIBLE_LINE0 as u16 + 1,
            DIW_HSTART_FB0 as u16,
        );
        initial_state.sprpos[0] = pos;
        initial_state.sprctl[0] = ctl;
        initial_state.sprpos[1] = pos;
        initial_state.sprctl[1] = ctl | 0x0080;
        initial_state.palette.write_ocs(20, 0x0F00);

        let after_compare_hpos =
            (DIW_HSTART_FB0 as u32 / 2) + SPRITE_REGISTER_WRITE_PIPELINE_CCK + 2;
        assert!(sprite_data_write_framebuffer_x(after_compare_hpos) > 0);
        let events = [
            cpu_event(
                PAL_VISIBLE_LINE0 as u32,
                COPPER_WAIT_HPOS_FB0 as u32,
                0x144,
                0,
            ),
            cpu_event(
                PAL_VISIBLE_LINE0 as u32,
                COPPER_WAIT_HPOS_FB0 as u32,
                0x14C,
                0x8000,
            ),
            cpu_event(PAL_VISIBLE_LINE0 as u32, after_compare_hpos, 0x14C, 0x2000),
        ];
        let manual_sprite_lines = manual_sprite_lines_from_events(&initial_state, &events);

        let base_controls = [ControlState::from_render_state(&initial_state); FB_HEIGHT];
        let mut render_state = initial_state;
        apply_move(&mut render_state, 0x144, 0);
        apply_move(&mut render_state, 0x14C, 0x2000);
        let ram = vec![0u8; 512 * 1024];
        let base_palettes = [render_state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];

        render_sprites_with_manual_lines(
            &render_state,
            &ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &playfield_mask,
            &mut collision_pixels,
            sprite_pointer_refreshes_from_mask([false; 8]),
            &[],
            false,
            Some(&manual_sprite_lines),
        );

        assert_eq!(fb[0], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[4], rgb12_to_rgba8(0));
    }

    #[test]
    fn attached_manual_sprite_data_after_compare_preserves_loaded_even_pixels() {
        let mut initial_state = blank_state();
        let (pos, ctl) = sprite_control_words(
            PAL_VISIBLE_LINE0 as u16,
            PAL_VISIBLE_LINE0 as u16 + 1,
            DIW_HSTART_FB0 as u16,
        );
        initial_state.sprpos[0] = pos;
        initial_state.sprctl[0] = ctl;
        initial_state.sprdata[0] = 0xA000;
        initial_state.spr_armed[0] = true;
        initial_state.sprpos[1] = pos;
        initial_state.sprctl[1] = ctl | 0x0080;
        initial_state.palette.write_ocs(17, 0x0F00);
        initial_state.palette.write_ocs(21, 0x00F0);

        let after_compare_hpos =
            (DIW_HSTART_FB0 as u32 / 2) + SPRITE_REGISTER_WRITE_PIPELINE_CCK + 2;
        assert!(sprite_data_write_framebuffer_x(after_compare_hpos) > 0);
        let events = [cpu_event(
            PAL_VISIBLE_LINE0 as u32,
            after_compare_hpos,
            0x14C,
            0x2000,
        )];
        let manual_sprite_lines = manual_sprite_lines_from_events(&initial_state, &events);

        let base_controls = [ControlState::from_render_state(&initial_state); FB_HEIGHT];
        let mut render_state = initial_state;
        apply_move(&mut render_state, 0x14C, 0x2000);
        let ram = vec![0u8; 512 * 1024];
        let base_palettes = [render_state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];

        render_sprites_with_manual_lines(
            &render_state,
            &ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &playfield_mask,
            &mut collision_pixels,
            sprite_pointer_refreshes_from_mask([false; 8]),
            &[],
            false,
            Some(&manual_sprite_lines),
        );

        assert_eq!(fb[0], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[4], rgb12_to_rgba8(0x0F00));
    }

    #[test]
    fn disabled_sprite_dma_ignores_stale_sprite_pointers() {
        let mut ram = vec![0u8; 512 * 1024];
        let sprite_ptr = 0x0100usize;
        let (pos, ctl) = sprite_control_words(
            PAL_VISIBLE_LINE0 as u16,
            PAL_VISIBLE_LINE0 as u16 + 1,
            DIW_HSTART_FB0 as u16,
        );
        put_word(&mut ram, sprite_ptr, pos);
        put_word(&mut ram, sprite_ptr + 2, ctl);
        put_word(&mut ram, sprite_ptr + 4, 0x8000);
        put_word(&mut ram, sprite_ptr + 6, 0);
        put_word(&mut ram, sprite_ptr + 8, 0);
        put_word(&mut ram, sprite_ptr + 10, 0);

        let mut state = blank_state();
        state.sprpt[0] = sprite_ptr as u32;
        state.palette.write_ocs(17, 0x0F00);
        let base_palettes = [state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];

        render_sprites(
            &state,
            &ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &playfield_mask,
            &mut collision_pixels,
            [false; 8],
            &[],
            false,
        );

        assert_eq!(fb[0], rgb12_to_rgba8(0));
    }

    #[test]
    fn sprites_wait_for_first_bpl1dat_display_enable_on_scanline() {
        let mut state = blank_state();
        state.palette.write_ocs(17, 0x0F00);
        let ram = vec![0; 64];
        let base_palettes = [state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let captured = [CapturedSpriteLine {
            sprite: 0,
            hstart: DIW_HSTART_FB0,
            hsub_70ns: false,
            beam_y: PAL_VISIBLE_LINE0,
            data: 0xFFFF,
            datb: 0,
            attached: false,
            data_ext: [0; 3],
            datb_ext: [0; 3],
            width_words: 1,
        }];
        let mut display_enable_x = [None; FB_HEIGHT];
        display_enable_x[0] = Some(4);
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];

        render_sprites_with_manual_lines_and_writes(
            &state,
            &ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &display_enable_x,
            &playfield_mask,
            &mut collision_pixels,
            &captured,
            true,
            None,
            PAL_VISIBLE_LINE0,
        );

        assert_eq!(fb[0], rgb12_to_rgba8(0));
        assert_eq!(fb[3], rgb12_to_rgba8(0));
        assert_eq!(fb[4], rgb12_to_rgba8(0x0F00));
    }

    #[test]
    fn manual_bpl1dat_display_enable_allows_sprites_on_vertically_closed_diw_line() {
        let mut state = RenderState {
            dmacon: DMACON_DMAEN | DMACON_SPREN,
            diwstrt: (((PAL_VISIBLE_LINE0 + 10) as u16) << 8) | DIW_HSTART_FB0 as u16,
            diwstop: (((PAL_VISIBLE_LINE0 + 20) as u16) << 8) | 0x00C1,
            ..blank_state()
        };
        state.palette.write_ocs(17, 0x0F00);
        let ram = vec![0; 64];
        let base_palettes = [state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let captured = [CapturedSpriteLine {
            sprite: 0,
            hstart: DIW_HSTART_FB0,
            hsub_70ns: false,
            beam_y: PAL_VISIBLE_LINE0,
            data: 0x8000,
            datb: 0,
            attached: false,
            data_ext: [0; 3],
            datb_ext: [0; 3],
            width_words: 1,
        }];

        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];
        let display_disabled = [None; FB_HEIGHT];
        render_sprites_with_manual_lines_and_writes(
            &state,
            &ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &display_disabled,
            &playfield_mask,
            &mut collision_pixels,
            &captured,
            true,
            None,
            PAL_VISIBLE_LINE0,
        );
        assert_eq!(fb[0], rgb12_to_rgba8(0));

        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];
        let mut display_enabled = [None; FB_HEIGHT];
        display_enabled[0] = Some(0);
        render_sprites_with_manual_lines_and_writes(
            &state,
            &ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &display_enabled,
            &playfield_mask,
            &mut collision_pixels,
            &captured,
            true,
            None,
            PAL_VISIBLE_LINE0,
        );
        assert_eq!(fb[0], rgb12_to_rgba8(0x0F00));
    }

    #[test]
    fn brdsprt_bypasses_first_bpl1dat_display_enable_gate() {
        let mut state = RenderState {
            bplcon0: BPLCON0_ECSENA,
            bplcon3: BPLCON3_BRDSPRT,
            ..blank_state()
        };
        state.palette.write_ocs(17, 0x0F00);
        let ram = vec![0; 64];
        let base_palettes = [state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let captured = [CapturedSpriteLine {
            sprite: 0,
            hstart: DIW_HSTART_FB0,
            hsub_70ns: false,
            beam_y: PAL_VISIBLE_LINE0,
            data: 0x8000,
            datb: 0,
            attached: false,
            data_ext: [0; 3],
            datb_ext: [0; 3],
            width_words: 1,
        }];
        let display_enable_x = [None; FB_HEIGHT];
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];

        render_sprites_with_manual_lines_and_writes(
            &state,
            &ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &display_enable_x,
            &playfield_mask,
            &mut collision_pixels,
            &captured,
            true,
            None,
            PAL_VISIBLE_LINE0,
        );

        assert_eq!(fb[0], rgb12_to_rgba8(0x0F00));
    }

    #[test]
    fn bplcon3_brdsprt_allows_sprites_in_border_when_ecsena_set() {
        let mut state = RenderState {
            dmacon: DMACON_DMAEN | DMACON_SPREN,
            bplcon0: BPLCON0_ECSENA,
            bplcon3: BPLCON3_BRDSPRT,
            diwstrt: ((PAL_VISIBLE_LINE0 as u16) << 8) | (DIW_HSTART_FB0 as u16 + 8),
            diwstop: (((PAL_VISIBLE_LINE0 + 128) as u16) << 8) | (DIW_HSTART_FB0 as u16 + 63),
            ..blank_state()
        };
        state.palette.write_ocs(17, 0x0F00);
        let ram = vec![0; 64];
        let base_palettes = [state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];
        let captured = [CapturedSpriteLine {
            sprite: 0,
            hstart: DIW_HSTART_FB0,
            hsub_70ns: false,
            beam_y: PAL_VISIBLE_LINE0,
            data: 0x8000,
            datb: 0,
            attached: false,
            data_ext: [0; 3],
            datb_ext: [0; 3],
            width_words: 1,
        }];

        render_sprites(
            &state,
            &ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &playfield_mask,
            &mut collision_pixels,
            [false; 8],
            &captured,
            true,
        );

        assert_eq!(fb[0], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[1], rgb12_to_rgba8(0x0F00));
    }

    #[test]
    fn bplcon3_brdsprt_requires_ecsena_for_border_sprites() {
        let mut state = RenderState {
            dmacon: DMACON_DMAEN | DMACON_SPREN,
            bplcon0: 0,
            bplcon3: BPLCON3_BRDSPRT,
            diwstrt: ((PAL_VISIBLE_LINE0 as u16) << 8) | (DIW_HSTART_FB0 as u16 + 8),
            diwstop: (((PAL_VISIBLE_LINE0 + 128) as u16) << 8) | (DIW_HSTART_FB0 as u16 + 63),
            ..blank_state()
        };
        state.palette.write_ocs(17, 0x0F00);
        let ram = vec![0; 64];
        let base_palettes = [state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];
        let captured = [CapturedSpriteLine {
            sprite: 0,
            hstart: DIW_HSTART_FB0,
            hsub_70ns: false,
            beam_y: PAL_VISIBLE_LINE0,
            data: 0x8000,
            datb: 0,
            attached: false,
            data_ext: [0; 3],
            datb_ext: [0; 3],
            width_words: 1,
        }];

        render_sprites(
            &state,
            &ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &playfield_mask,
            &mut collision_pixels,
            [false; 8],
            &captured,
            true,
        );

        assert_eq!(fb[0], rgb12_to_rgba8(0));
        assert_eq!(fb[1], rgb12_to_rgba8(0));
    }

    #[test]
    fn beam_timed_bplcon3_brdsprt_latches_until_ecsena_enables_effect() {
        let mut state = RenderState {
            dmacon: DMACON_DMAEN | DMACON_SPREN,
            bplcon0: 0,
            bplcon3: 0,
            diwstrt: ((PAL_VISIBLE_LINE0 as u16) << 8) | (DIW_HSTART_FB0 as u16 + 80),
            diwstop: (((PAL_VISIBLE_LINE0 + 128) as u16) << 8) | (DIW_HSTART_FB0 as u16 + 120),
            ..blank_state()
        };
        state.palette.write_ocs(17, 0x0F00);
        let mut base_palettes = [state.palette; FB_HEIGHT];
        let mut palette_segments = vec![Vec::new(); FB_HEIGHT];
        let mut base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let mut control_segments = vec![Vec::new(); FB_HEIGHT];
        let mut manual_bpl_segments = Vec::new();
        let events = [
            beam_event(
                PAL_VISIBLE_LINE0 as u32,
                (COPPER_WAIT_HPOS_FB0 + 4) as u32,
                0x0106,
                BPLCON3_BRDSPRT,
            ),
            beam_event(
                PAL_VISIBLE_LINE0 as u32,
                (COPPER_WAIT_HPOS_FB0 + 8) as u32,
                0x0100,
                BPLCON0_ECSENA,
            ),
        ];

        apply_render_events(
            &mut state,
            &events,
            &mut base_palettes,
            &mut palette_segments,
            &mut base_controls,
            &mut control_segments,
            &mut manual_bpl_segments,
        );

        let ram = vec![0; 64];
        let playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];
        let captured = [CapturedSpriteLine {
            sprite: 0,
            hstart: DIW_HSTART_FB0 + 8,
            hsub_70ns: false,
            beam_y: PAL_VISIBLE_LINE0,
            data: 0xFFFF,
            datb: 0,
            attached: false,
            data_ext: [0; 3],
            datb_ext: [0; 3],
            width_words: 1,
        }];

        render_sprites(
            &state,
            &ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &playfield_mask,
            &mut collision_pixels,
            [false; 8],
            &captured,
            true,
        );

        assert_eq!(fb[24], rgb12_to_rgba8(0));
        assert_eq!(fb[40], rgb12_to_rgba8(0x0F00));
    }

    #[test]
    fn sprites_use_beam_timed_display_window_control() {
        let mut state = blank_state();
        state.dmacon = DMACON_DMAEN | DMACON_SPREN;
        state.diwstrt = ((PAL_VISIBLE_LINE0 as u16) << 8) | DIW_HSTART_FB0 as u16;
        state.diwstop = (((PAL_VISIBLE_LINE0 + 1) as u16) << 8) | 0x00C1;
        state.palette.write_ocs(17, 0x0F00);
        let ram = vec![0; 64];
        let base_palettes = [state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let base_control = ControlState::from_render_state(&state);
        let base_controls = [base_control; FB_HEIGHT];
        let mut control_segments = vec![Vec::new(); FB_HEIGHT];
        let mut later_control = base_control;
        later_control.diwstrt = ((PAL_VISIBLE_LINE0 as u16) << 8) | (DIW_HSTART_FB0 as u16 + 2);
        control_segments[0].push(ControlSegment {
            x: 2,
            control: later_control,
        });
        let playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];
        let captured = [CapturedSpriteLine {
            sprite: 0,
            hstart: DIW_HSTART_FB0,
            hsub_70ns: false,
            beam_y: PAL_VISIBLE_LINE0,
            data: 0xC000,
            datb: 0,
            attached: false,
            data_ext: [0; 3],
            datb_ext: [0; 3],
            width_words: 1,
        }];

        render_sprites(
            &state,
            &ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &playfield_mask,
            &mut collision_pixels,
            [false; 8],
            &captured,
            true,
        );

        assert_eq!(fb[0], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[1], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[2], rgb12_to_rgba8(0));
        assert_eq!(fb[3], rgb12_to_rgba8(0));
    }

    #[test]
    fn captured_sprite_dma_lines_render_without_reparsing_frame_ram() {
        let mut state = blank_state();
        state.dmacon = DMACON_DMAEN | DMACON_SPREN;
        state.palette.write_ocs(17, 0x0F00);
        let ram = vec![0; 64];
        let base_palettes = [state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];
        let captured = [CapturedSpriteLine {
            sprite: 0,
            hstart: DIW_HSTART_FB0,
            hsub_70ns: false,
            beam_y: PAL_VISIBLE_LINE0,
            data: 0x8000,
            datb: 0,
            attached: false,
            data_ext: [0; 3],
            datb_ext: [0; 3],
            width_words: 1,
        }];

        render_sprites(
            &state,
            &ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &playfield_mask,
            &mut collision_pixels,
            [false; 8],
            &captured,
            true,
        );

        assert_eq!(fb[0], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[1], rgb12_to_rgba8(0x0F00));
    }

    #[test]
    fn dma_loaded_sprite_data_rearms_on_same_line_position_write() {
        let mut state = blank_state();
        state.dmacon = DMACON_DMAEN | DMACON_SPREN;
        state.palette.write_ocs(17, 0x0F00);
        let ram = vec![0; 64];
        let base_palettes = [state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];
        let beam_y = PAL_VISIBLE_LINE0;
        let initial_hstart = DIW_HSTART_FB0 as u16;
        let reused_hstart = initial_hstart + 64;
        let (reused_pos, _) = sprite_control_words(beam_y as u16, beam_y as u16 + 1, reused_hstart);
        let captured = [CapturedSpriteLine {
            sprite: 0,
            hstart: i32::from(initial_hstart),
            hsub_70ns: false,
            beam_y,
            data: 0x8000,
            datb: 0,
            attached: false,
            data_ext: [0; 3],
            datb_ext: [0; 3],
            width_words: 1,
        }];
        let events = [beam_event(
            beam_y as u32,
            (COPPER_WAIT_HPOS_FB0 + 4) as u32,
            0x0140,
            reused_pos,
        )];
        let mut manual_sprite_lines = manual_sprite_lines_from_events_with_visible_line0(
            &state,
            &events,
            &[None; 8],
            PAL_VISIBLE_LINE0,
            FB_HEIGHT,
            false,
        );

        assert!(manual_sprite_lines[0].is_empty());

        let dma_seeded_lines = manual_sprite_lines_from_captured_dma_reuse(
            &state,
            &events,
            &captured,
            PAL_VISIBLE_LINE0,
            FB_HEIGHT,
        );
        assert!(dma_seeded_lines[0].iter().any(|line| {
            line.beam_y == beam_y && line.hstart == i32::from(reused_hstart) && line.data == 0x8000
        }));
        merge_dma_seeded_manual_sprite_lines(&mut manual_sprite_lines, dma_seeded_lines);

        render_sprites_with_manual_lines(
            &state,
            &ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &playfield_mask,
            &mut collision_pixels,
            sprite_pointer_refreshes_from_mask([false; 8]),
            &captured,
            true,
            Some(&manual_sprite_lines),
        );

        let reused_x =
            sprite_base_framebuffer_x(i32::from(reused_hstart), false, base_controls[0], &[]);
        assert_eq!(fb[reused_x as usize], rgb12_to_rgba8(0x0F00));
    }

    #[test]
    fn sprite_register_data_write_after_compare_preserves_dma_latch_on_same_beam_line() {
        let mut state = blank_state();
        let (pos, ctl) = sprite_control_words(
            PAL_VISIBLE_LINE0 as u16,
            PAL_VISIBLE_LINE0 as u16 + 1,
            DIW_HSTART_FB0 as u16,
        );
        state.dmacon = DMACON_DMAEN | DMACON_SPREN;
        state.sprpos[0] = pos;
        state.sprctl[0] = ctl;
        state.palette.write_ocs(17, 0x0F00);
        let ram = vec![0; 64];
        let base_palettes = [state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];
        let captured = [CapturedSpriteLine {
            sprite: 0,
            hstart: DIW_HSTART_FB0,
            hsub_70ns: false,
            beam_y: PAL_VISIBLE_LINE0,
            data: 0xFFFF,
            datb: 0,
            attached: false,
            data_ext: [0; 3],
            datb_ext: [0; 3],
            width_words: 1,
        }];
        let after_compare_hpos =
            (DIW_HSTART_FB0 as u32 / 2) + SPRITE_REGISTER_WRITE_PIPELINE_CCK + 2;
        assert!(sprite_data_write_framebuffer_x(after_compare_hpos) > 0);
        let manual_sprite_lines = manual_sprite_lines_from_events(
            &state,
            &[beam_event(
                PAL_VISIBLE_LINE0 as u32,
                after_compare_hpos,
                0x0144,
                0x0000,
            )],
        );

        render_sprites_with_manual_lines(
            &state,
            &ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &playfield_mask,
            &mut collision_pixels,
            sprite_pointer_refreshes_from_mask([false; 8]),
            &captured,
            true,
            Some(&manual_sprite_lines),
        );

        assert_eq!(fb[0], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[7], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[8], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[31], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[32], rgb12_to_rgba8(0));
    }

    #[test]
    fn dual_playfield_uses_separate_palette_banks_and_priority() {
        let pf1_priority = ControlState {
            bplcon0: 0x6400,
            bplcon1: 0,
            bplcon2: 0,
            bplcon3: BPLCON3_PF2OF_DEFAULT,
            ..ControlState::default()
        };
        assert!(pf1_priority.dual_playfield());
        assert_eq!(dual_playfield_palette_index(0b010101, pf1_priority), 7);
        assert_eq!(dual_playfield_palette_index(0b101010, pf1_priority), 15);
        assert_eq!(dual_playfield_palette_index(0b000011, pf1_priority), 1);

        let pf2_priority = ControlState {
            bplcon0: 0x6400,
            bplcon1: 0,
            bplcon2: 0x0040,
            bplcon3: BPLCON3_PF2OF_DEFAULT,
            ..ControlState::default()
        };
        assert_eq!(dual_playfield_palette_index(0b000011, pf2_priority), 9);
    }

    #[test]
    fn aga_dual_playfield_decodes_bitplane7_into_pf1_fourth_bit() {
        // AGA Lisa dual playfield gives each field four bits: bitplane 7
        // becomes PF1's high bit (palette entries 8..15) and bitplane 8
        // PF2's. Pre-AGA chips decode only three bits per field. Zool
        // (A1200) draws its sprite-cel character into a 7-plane dual
        // playfield: the black body lives at PF1 index 11, which collapses
        // to index 3 (orange) when bitplane 7 is dropped.
        let aga = ControlState {
            bplcon0: 0x7400,
            bplcon3: BPLCON3_PF2OF_DEFAULT,
            agnus_revision: AgnusRevision::AgaAlice,
            ..ControlState::default()
        };
        assert!(aga.aga() && aga.dual_playfield());
        // Bitplanes 1,3,7 set -> PF1 = 0b1011 = 11, PF2 empty.
        assert_eq!(dual_playfield_palette_index(0b0100_0101, aga), 11);
        // Bitplane 8 set -> PF2 high bit; PF2 = 0b1000 = 8, plus the
        // default PF2OF offset of 8 -> palette entry 16.
        assert_eq!(dual_playfield_palette_index(0b1000_0000, aga), 16);

        // The same indices on OCS keep the three-bit decode (bits 6,7 are
        // never carried by <=6 plane hardware, so they are ignored).
        let ocs = ControlState {
            bplcon0: 0x6400,
            ..ControlState::default()
        };
        assert!(!ocs.aga() && ocs.dual_playfield());
        assert_eq!(dual_playfield_palette_index(0b0100_0101, ocs), 3);
    }

    #[test]
    fn bplcon3_pf2of_selects_dual_playfield_pf2_palette_offset() {
        let control = |bplcon3| ControlState {
            bplcon0: 0x6400,
            bplcon1: 0,
            bplcon2: 0x0040,
            bplcon3,
            ..ControlState::default()
        };

        assert_eq!(dual_playfield_palette_index(0b000011, control(0x0000)), 1);
        assert_eq!(dual_playfield_palette_index(0b000011, control(0x0400)), 3);
        assert_eq!(dual_playfield_palette_index(0b000011, control(0x0800)), 5);
        assert_eq!(dual_playfield_palette_index(0b000011, control(0x1000)), 17);
    }

    #[test]
    fn bplcon1_exposes_separate_playfield_scroll_nibbles() {
        let control = ControlState {
            bplcon0: 0x6400,
            bplcon1: 0x00A3,
            bplcon2: 0,
            ..ControlState::default()
        };

        assert_eq!(control.pf1_scroll(), 3);
        assert_eq!(control.pf2_scroll(), 10);
        assert_eq!(control.scroll_for_plane(0), 3);
        assert_eq!(control.scroll_for_plane(1), 10);
    }

    #[test]
    fn aga_bplcon1_decodes_expanded_scroll_fields() {
        let control = ControlState {
            agnus_revision: AgnusRevision::AgaAlice,
            bplcon0: 0x0010,
            bplcon1: 0xC8C2,
            fmode: 0x0003,
            ..ControlState::default()
        };

        // BPLCON1 bit layout on Lisa:
        // PF1 H0..H7 = bits 8,9,0,1,2,3,10,11.
        // PF2 H0..H7 = bits 12,13,4,5,6,7,14,15.
        // This frame uses 136 and 240 super-hires pixels respectively; in
        // lo-res output those are 34 and 60 native samples.
        assert_eq!(control.pf1_scroll(), 34);
        assert_eq!(control.pf2_scroll(), 60);
        assert_eq!(control.scroll_for_plane(0), 34);
        assert_eq!(control.scroll_for_plane(1), 60);
    }

    #[test]
    fn aga_bplcon1_preserves_classic_lores_scroll_nibbles() {
        let control = ControlState {
            agnus_revision: AgnusRevision::AgaAlice,
            bplcon0: 0x0010,
            bplcon1: 0x00A3,
            fmode: 0,
            ..ControlState::default()
        };

        assert_eq!(control.pf1_scroll(), 3);
        assert_eq!(control.pf2_scroll(), 10);
    }

    #[test]
    fn aga_bplcon1_masks_scroll_range_by_fetch_width() {
        let control = |fmode| ControlState {
            agnus_revision: AgnusRevision::AgaAlice,
            bplcon0: 0x0010,
            bplcon1: 0xCC00,
            fmode,
            ..ControlState::default()
        };

        assert_eq!(control(0x0000).pf1_scroll(), 0);
        assert_eq!(control(0x0001).pf1_scroll(), 16);
        assert_eq!(control(0x0003).pf1_scroll(), 48);
    }

    #[test]
    fn bplcon1_scroll_nibbles_apply_to_odd_even_planes_without_dual_playfield() {
        let control = ControlState {
            bplcon0: 0x2200,
            bplcon1: 0x0020,
            bplcon2: 0,
            ..ControlState::default()
        };

        assert!(!control.dual_playfield());
        assert_eq!(control.scroll_for_plane(0), 0);
        assert_eq!(control.scroll_for_plane(1), 2);
    }

    #[test]
    fn bplcon2_priority_codes_place_sprite_groups_against_playfields() {
        for playfield in 1..=2 {
            for priority_code in 0u16..=7 {
                let bplcon2 = if playfield == 1 {
                    priority_code
                } else {
                    priority_code << 3
                };
                let control = ControlState {
                    bplcon0: 0,
                    bplcon1: 0,
                    bplcon2,
                    ..ControlState::default()
                };
                let visible_groups = priority_code.min(4) as usize;
                for group in 0..4 {
                    let sprite = group * 2;
                    assert_eq!(
                        sprite_has_priority(sprite, playfield, control),
                        group < visible_groups,
                        "playfield={playfield} priority_code={priority_code} sprite_group={group}"
                    );
                }
            }
        }
    }

    #[test]
    fn attached_manual_sprites_use_four_bit_color_indexes() {
        let mut state = blank_state();
        let (pos, ctl) = sprite_control_words(
            PAL_VISIBLE_LINE0 as u16,
            PAL_VISIBLE_LINE0 as u16 + 1,
            DIW_HSTART_FB0 as u16,
        );
        state.sprpos[0] = pos;
        state.sprctl[0] = ctl;
        state.sprdata[0] = 0x8000;
        state.sprdatb[0] = 0;
        state.spr_armed[0] = true;
        state.sprpos[1] = pos;
        state.sprctl[1] = ctl | 0x0080;
        state.sprdata[1] = 0x8000;
        state.sprdatb[1] = 0;
        state.spr_armed[1] = true;
        state.palette.write_ocs(21, 0x00F0);
        let ram = vec![0; 64];
        let base_palettes = [state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];

        render_sprites(
            &state,
            &ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &playfield_mask,
            &mut collision_pixels,
            [false; 8],
            &[],
            false,
        );

        assert_eq!(fb[0], rgb12_to_rgba8(0x00F0));
    }

    #[test]
    fn attached_manual_sprites_draw_odd_bits_without_even_data_bits() {
        let mut state = blank_state();
        let (pos, ctl) = sprite_control_words(
            PAL_VISIBLE_LINE0 as u16,
            PAL_VISIBLE_LINE0 as u16 + 1,
            DIW_HSTART_FB0 as u16,
        );
        state.sprpos[0] = pos;
        state.sprctl[0] = ctl;
        state.sprdata[0] = 0;
        state.sprdatb[0] = 0;
        state.spr_armed[0] = true;
        state.sprpos[1] = pos;
        state.sprctl[1] = ctl | 0x0080;
        state.sprdata[1] = 0x8000;
        state.sprdatb[1] = 0;
        state.spr_armed[1] = true;
        state.palette.write_ocs(20, 0x00F0);
        let ram = vec![0; 64];
        let base_palettes = [state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];

        render_sprites(
            &state,
            &ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &playfield_mask,
            &mut collision_pixels,
            [false; 8],
            &[],
            false,
        );

        assert_eq!(fb[0], rgb12_to_rgba8(0x00F0));
    }

    #[test]
    fn attached_manual_sprite_pair_uses_even_control_attach_bit() {
        let mut state = blank_state();
        let (pos, ctl) = sprite_control_words(
            PAL_VISIBLE_LINE0 as u16,
            PAL_VISIBLE_LINE0 as u16 + 1,
            DIW_HSTART_FB0 as u16,
        );
        state.sprpos[0] = pos;
        state.sprctl[0] = ctl | 0x0080;
        state.sprdata[0] = 0x8000;
        state.sprdatb[0] = 0;
        state.spr_armed[0] = true;
        state.sprpos[1] = pos;
        state.sprctl[1] = ctl;
        state.sprdata[1] = 0x8000;
        state.sprdatb[1] = 0;
        state.spr_armed[1] = true;
        state.palette.write_ocs(21, 0x00F0);
        let ram = vec![0; 64];
        let base_palettes = [state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];

        render_sprites(
            &state,
            &ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &playfield_mask,
            &mut collision_pixels,
            [false; 8],
            &[],
            false,
        );

        assert_eq!(fb[0], rgb12_to_rgba8(0x00F0));
    }

    #[test]
    fn attached_manual_sprite_pair_decodes_odd_pixels_without_even_line() {
        let mut state = blank_state();
        let (pos, ctl) = sprite_control_words(
            PAL_VISIBLE_LINE0 as u16,
            PAL_VISIBLE_LINE0 as u16 + 1,
            DIW_HSTART_FB0 as u16,
        );
        state.sprpos[1] = pos;
        state.sprctl[1] = ctl | 0x0080;
        state.sprdata[1] = 0x8000;
        state.sprdatb[1] = 0;
        state.spr_armed[1] = true;
        state.palette.write_ocs(20, 0x00F0);
        let ram = vec![0; 64];
        let base_palettes = [state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];

        render_sprites(
            &state,
            &ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &playfield_mask,
            &mut collision_pixels,
            [false; 8],
            &[],
            false,
        );

        assert_eq!(fb[0], rgb12_to_rgba8(0x00F0));
    }

    #[test]
    fn sprite_dma_reuse_stops_at_null_control_block() {
        let mut ram = vec![0u8; 512 * 1024];
        let sprite_ptr = 0x0100usize;
        let (pos, ctl) = sprite_control_words(
            PAL_VISIBLE_LINE0 as u16,
            PAL_VISIBLE_LINE0 as u16 + 1,
            DIW_HSTART_FB0 as u16,
        );
        put_word(&mut ram, sprite_ptr, 0);
        put_word(&mut ram, sprite_ptr + 2, 0);
        put_word(&mut ram, sprite_ptr + 4, pos);
        put_word(&mut ram, sprite_ptr + 6, ctl);
        put_word(&mut ram, sprite_ptr + 8, 0x8000);
        put_word(&mut ram, sprite_ptr + 10, 0);
        put_word(&mut ram, sprite_ptr + 12, 0);
        put_word(&mut ram, sprite_ptr + 14, 0);

        let mut state = blank_state();
        state.dmacon = DMACON_DMAEN | DMACON_SPREN;
        state.sprpt[0] = sprite_ptr as u32;
        state.palette.write_ocs(17, 0x0F00);
        let mut sprite_ptr_refreshed = [false; 8];
        sprite_ptr_refreshed[0] = true;

        let fb = render_sprite_dma_test_frame(
            &state,
            &ram,
            sprite_pointer_refreshes_from_mask(sprite_ptr_refreshed),
        );

        assert_eq!(fb[0], rgb12_to_rgba8(0));
    }

    #[test]
    fn collision_pixel_honors_clxcon_match_bits() {
        let collision = collision_pixel(0b000011, 2, 0x00C3, 0, false);
        assert!(collision.pf1_match);
        assert!(collision.pf2_match);

        let mismatch = collision_pixel(0b000010, 2, 0x00C3, 0, false);
        assert!(!mismatch.pf1_match);
        assert!(mismatch.pf2_match);
    }

    #[test]
    fn collision_pixel_single_playfield_odd_match_requires_even_match() {
        let single = collision_pixel(0b000001, 2, 0x0083, 0, false);
        assert!(!single.pf1_match);
        assert!(!single.pf2_match);

        let dual = collision_pixel(0b000001, 2, 0x0083, 0, true);
        assert!(dual.pf1_match);
        assert!(!dual.pf2_match);
    }

    /// Plan 3.4: AGA planes 7-8 collision control comes from CLXCON2
    /// (ENBP7/ENBP8 in bits 6-7, MVBP7/MVBP8 in bits 0-1).
    #[test]
    fn collision_pixel_planes_7_and_8_use_clxcon2() {
        // Plane 7 (bit 6) enabled, must be set: pixel with bit 6 matches.
        let hit = collision_pixel(0b0100_0000, 8, 0, 0x0041, false);
        assert!(hit.pf1_match);
        let miss = collision_pixel(0, 8, 0, 0x0041, false);
        assert!(!miss.pf1_match);

        // Plane 8 (bit 7) enabled, must be clear: a set bit 7 mismatches.
        let even_hit = collision_pixel(0, 8, 0, 0x0080, false);
        assert!(even_hit.pf2_match);
        let even_miss = collision_pixel(0b1000_0000, 8, 0, 0x0080, false);
        assert!(!even_miss.pf2_match);

        // With CLXCON2 clear, planes 7-8 never gate the match (and the
        // sprite-enable bits of CLXCON are not misread for them).
        let ignore = collision_pixel(0b1100_0000, 8, 0xF000, 0, false);
        assert!(ignore.pf1_match && ignore.pf2_match);
    }

    #[test]
    fn collision_pixel_disabled_planes_match_continuously() {
        let collision = collision_pixel(0, 6, 0, 0, false);
        assert!(collision.pf1_match);
        assert!(collision.pf2_match);
        assert_eq!(collision.clxdat_bits(), 1);
    }

    #[test]
    fn generated_playfield_pixels_feed_playfield_and_sprite_clxdat() {
        let control = ControlState {
            bplcon0: 0x2400,
            ..ControlState::default()
        };
        let sample = DeniseBitplaneSample {
            idx: 0b000011,
            nplanes: 2,
            active: true,
        };
        let mut playfield_mask = vec![0u8; 1];
        let mut collision_pixels = vec![CollisionPixel::default(); 1];
        let mut clxdat = 0u16;

        record_generated_playfield_collision_pixel(
            &mut playfield_mask,
            &mut collision_pixels,
            &mut clxdat,
            0,
            sample,
            control,
        );

        assert_eq!(clxdat & 0x0001, 0x0001);
        assert_eq!(playfield_mask[0], 0x03);

        let mut sprite_group_mask = vec![0u8; 1];
        let sprite_clxdat = generated_sprite_collision_bits(
            0,
            0,
            control.clxcon,
            &mut sprite_group_mask,
            &mut collision_pixels,
            &playfield_mask,
        );
        assert_eq!(sprite_clxdat & 0x0022, 0x0022);
    }

    #[test]
    fn denise_planned_playfield_line_applies_word_phase_scroll_and_plane_count() {
        let row_words = vec![vec![0x8000], vec![0x4000], vec![0x2000]];
        let line_plan = DenisePlannedPlayfieldLine::new(0, 0, 16, &row_words, 16);
        let mut control = ControlState {
            bplcon0: 0x3000,
            ..ControlState::default()
        };

        assert_eq!(
            line_plan.sample(control, 0),
            DeniseBitplaneSample {
                idx: 0x01,
                nplanes: 3,
                active: true,
            }
        );
        assert_eq!(
            line_plan.sample(control, 1),
            DeniseBitplaneSample {
                idx: 0x02,
                nplanes: 3,
                active: true,
            }
        );

        control.bplcon1 = 0x0001;
        assert_eq!(
            line_plan.sample(control, 0),
            DeniseBitplaneSample {
                idx: 0x00,
                nplanes: 3,
                active: true,
            }
        );
        assert_eq!(
            line_plan.sample(control, 1),
            DeniseBitplaneSample {
                idx: 0x03,
                nplanes: 3,
                active: true,
            }
        );

        control.bplcon0 = 0x1000;
        assert_eq!(
            line_plan.sample(control, 1),
            DeniseBitplaneSample {
                idx: 0x01,
                nplanes: 1,
                active: true,
            }
        );
    }

    #[test]
    fn planned_ham_dma_uses_current_bitplane_sample_at_fetch_edge() {
        let mut row_words = vec![vec![0; 1]; 6];
        for words in row_words.iter_mut().take(4) {
            words[0] |= 0x8000;
        }
        let line_plan = DenisePlannedPlayfieldLine::new(0, 68, 72, &row_words, 1);
        let control = visible_lowres_control(0x7800);
        let mut palette = Palette::new();
        palette.write_ocs(15, 0x0123);
        let mut fb = vec![0; FB_PIXELS];
        let mut playfield_mask = vec![0; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut clxdat = 0;

        render_planned_playfield_line(
            &line_plan,
            &mut fb,
            &mut playfield_mask,
            &mut collision_pixels,
            &mut clxdat,
            palette,
            &[],
            0,
            control,
            &[],
            0,
            control.bplcon1,
            false,
            0,
            PAL_VISIBLE_LINE0,
            0.0,
            0,
        );

        assert_eq!(fb[68], rgb12_to_rgba8(0x0123));
        assert_eq!(fb[69], rgb12_to_rgba8(0x0123));
        assert_eq!(fb[70], rgb12_to_rgba8(0x0000));
        assert_eq!(fb[71], rgb12_to_rgba8(0x0000));
        assert_eq!(&playfield_mask[68..72], &[0x02, 0x02, 0x00, 0x00]);
    }

    #[test]
    fn planned_ham_dma_advances_hold_through_edge_fetch_phase() {
        let mut row_words = vec![vec![0; 1]; 6];
        row_words[0][0] |= 0x8000; // native x 0: direct palette entry 1
        row_words[1][0] |= 0x4000; // native x 1: HAM blue := 2
        row_words[4][0] |= 0x4000;
        let line_plan = DenisePlannedPlayfieldLine::new(0, 64, 66, &row_words, 2);
        let mut control = visible_lowres_control(0x6800);
        control.diwstrt = ((PAL_VISIBLE_LINE0 as u16) << 8) | STANDARD_DIW_HSTART as u16;
        control.diwstop =
            (((PAL_VISIBLE_LINE0 + 1) as u16) << 8) | (STANDARD_DIW_HSTART as u16 + 1);
        let mut palette = Palette::new();
        palette.write_ocs(1, 0x0123);
        let mut fb = vec![0; FB_PIXELS];
        let mut playfield_mask = vec![0; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut clxdat = 0;

        render_planned_playfield_line(
            &line_plan,
            &mut fb,
            &mut playfield_mask,
            &mut collision_pixels,
            &mut clxdat,
            palette,
            &[],
            0,
            control,
            &[],
            0,
            control.bplcon1,
            false,
            0,
            PAL_VISIBLE_LINE0,
            0.0,
            0,
        );

        assert_eq!(control.native_x_offset(control.diw_h_start(), 2), 1);
        assert_eq!(fb[64], rgb12_to_rgba8(0x0122));
        assert_eq!(fb[65], rgb12_to_rgba8(0x0122));
        assert_eq!(&playfield_mask[62..64], &[0x00, 0x00]);
    }

    #[test]
    fn planned_ham_dma_ignores_extra_early_ddf_history_before_diw() {
        let mut row_words = vec![vec![0; 2]; 6];
        row_words[0][0] |= 0x8000; // native x 0: direct palette entry 1
        row_words[4][0] |= 0x0001; // native x 15: HAM blue := 0
        row_words[4][1] |= 0x8000; // native x 16: HAM blue := 0
        let line_plan = DenisePlannedPlayfieldLine::new(0, 64, 66, &row_words, 32);
        let mut control = visible_lowres_control(0x6800);
        control.diwstrt = ((PAL_VISIBLE_LINE0 as u16) << 8) | STANDARD_DIW_HSTART as u16;
        control.diwstop =
            (((PAL_VISIBLE_LINE0 + 1) as u16) << 8) | (STANDARD_DIW_HSTART as u16 + 1);
        control.ddfstrt = 0x0030;
        control.ddfstop = 0x00D0;
        let mut palette = Palette::new();
        palette.write_ocs(1, 0x0123);
        let mut fb = vec![0; FB_PIXELS];
        let mut playfield_mask = vec![0; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut clxdat = 0;

        render_planned_playfield_line(
            &line_plan,
            &mut fb,
            &mut playfield_mask,
            &mut collision_pixels,
            &mut clxdat,
            palette,
            &[],
            0,
            control,
            &[],
            0,
            control.bplcon1,
            false,
            0,
            PAL_VISIBLE_LINE0,
            0.0,
            0,
        );

        assert_eq!(control.native_x_offset(control.diw_h_start(), 2), 16);
        assert_eq!(fb[64], rgb12_to_rgba8(0x0000));
        assert_eq!(fb[65], rgb12_to_rgba8(0x0000));
        assert_eq!(&playfield_mask[64..66], &[0x02, 0x02]);
    }

    #[test]
    fn bplcon1_write_at_diw_right_edge_does_not_retap_current_ham_line() {
        let mut row_words = vec![vec![0; 1]; 6];
        row_words[0][0] |= 0x4000; // native x 1: direct palette entry 1
        let line_plan = DenisePlannedPlayfieldLine::new(0, 64, 96, &row_words, 16);
        let mut control = visible_lowres_control(0x6800);
        control.diwstrt = ((PAL_VISIBLE_LINE0 as u16) << 8) | STANDARD_DIW_HSTART as u16;
        control.diwstop =
            (((PAL_VISIBLE_LINE0 + 1) as u16) << 8) | (STANDARD_DIW_HSTART as u16 + 16);
        control.diwhigh = DiwHigh::ecs_explicit(0);
        let mut retapped_control = control;
        retapped_control.bplcon1 = 0x0004;
        let control_segments = [ControlSegment {
            x: control.display_window_x().1,
            control: retapped_control,
        }];
        let mut palette = Palette::new();
        palette.write_ocs(1, 0x0123);
        let mut fb = vec![0; FB_PIXELS];
        let mut playfield_mask = vec![0; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut clxdat = 0;

        render_planned_playfield_line(
            &line_plan,
            &mut fb,
            &mut playfield_mask,
            &mut collision_pixels,
            &mut clxdat,
            palette,
            &[],
            0,
            control,
            &control_segments,
            0,
            control.bplcon1,
            false,
            0,
            PAL_VISIBLE_LINE0,
            0.0,
            0,
        );

        assert_eq!(control.display_window_x(), (64, 96));
        assert_eq!(fb[64], rgb12_to_rgba8(0x0123));
        assert_eq!(fb[65], rgb12_to_rgba8(0x0123));
    }

    #[test]
    fn bplcon2_color_key_uses_color_register_transparency_bit() {
        let row_words = vec![vec![0x8000]];
        let line_plan = DenisePlannedPlayfieldLine::new(0, 68, 70, &row_words, 16);
        let mut control = visible_lowres_control(0x1000);
        control.bplcon2 = BPLCON2_ZDCTEN;
        let mut palette = Palette::new();
        palette.write_ocs(1, COLOR_TRANSPARENCY_BIT | 0x0F00);
        let mut fb = vec![0; FB_PIXELS];
        let mut playfield_mask = vec![0; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut clxdat = 0;

        render_planned_playfield_line(
            &line_plan,
            &mut fb,
            &mut playfield_mask,
            &mut collision_pixels,
            &mut clxdat,
            palette,
            &[],
            0,
            control,
            &[],
            0,
            control.bplcon1,
            false,
            0,
            PAL_VISIBLE_LINE0,
            0.0,
            0,
        );

        assert_eq!(fb[68], rgb12_to_rgba8_alpha(0x0F00, false));
        assert_eq!(&playfield_mask[68..70], &[0x02, 0x02]);
    }

    #[test]
    fn bplcon2_bitplane_key_uses_selected_bitplane_sample() {
        let row_words = vec![vec![0x8000], vec![0x4000]];
        let line_plan = DenisePlannedPlayfieldLine::new(0, 68, 72, &row_words, 16);
        let mut control = visible_lowres_control(0x2000);
        control.bplcon2 = BPLCON2_ZDBPEN | (1 << BPLCON2_ZDBPSEL_SHIFT);
        let mut palette = Palette::new();
        palette.write_ocs(1, 0x0F00);
        palette.write_ocs(2, 0x00F0);
        let mut fb = vec![0; FB_PIXELS];
        let mut playfield_mask = vec![0; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut clxdat = 0;

        render_planned_playfield_line(
            &line_plan,
            &mut fb,
            &mut playfield_mask,
            &mut collision_pixels,
            &mut clxdat,
            palette,
            &[],
            0,
            control,
            &[],
            0,
            control.bplcon1,
            false,
            0,
            PAL_VISIBLE_LINE0,
            0.0,
            0,
        );

        assert_eq!(fb[68], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[70], rgb12_to_rgba8_alpha(0x00F0, false));
    }

    #[test]
    fn bplcon3_zdclken_disables_internal_genlock_keys() {
        let row_words = vec![vec![0x8000]];
        let line_plan = DenisePlannedPlayfieldLine::new(0, 68, 70, &row_words, 16);
        let mut control = visible_lowres_control(BPLCON0_ECSENA | 0x1000);
        control.bplcon2 = BPLCON2_ZDCTEN;
        control.bplcon3 = BPLCON3_ZDCLKEN;
        let mut palette = Palette::new();
        palette.write_ocs(1, COLOR_TRANSPARENCY_BIT | 0x0F00);
        let mut fb = vec![0; FB_PIXELS];
        let mut playfield_mask = vec![0; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut clxdat = 0;

        render_planned_playfield_line(
            &line_plan,
            &mut fb,
            &mut playfield_mask,
            &mut collision_pixels,
            &mut clxdat,
            palette,
            &[],
            0,
            control,
            &[],
            0,
            control.bplcon1,
            false,
            0,
            PAL_VISIBLE_LINE0,
            0.0,
            0,
        );

        assert_eq!(fb[68], rgb12_to_rgba8(0x0F00));
        assert_eq!(&playfield_mask[68..70], &[0x02, 0x02]);
    }

    #[test]
    fn planned_playfield_line_feeds_clxdat_from_rendered_dual_playfield_sample() {
        let row_words = vec![vec![0x8000], vec![0x8000]];
        let line_plan = DenisePlannedPlayfieldLine::new(0, 68, 70, &row_words, 16);
        let control = visible_lowres_control(0x2400);
        let mut fb = vec![0; FB_PIXELS];
        let mut playfield_mask = vec![0; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut clxdat = 0;

        render_planned_playfield_line(
            &line_plan,
            &mut fb,
            &mut playfield_mask,
            &mut collision_pixels,
            &mut clxdat,
            Palette::new(),
            &[],
            0,
            control,
            &[],
            0,
            control.bplcon1,
            false,
            0,
            PAL_VISIBLE_LINE0,
            0.0,
            0,
        );

        assert_eq!(clxdat & 0x0001, 0x0001);
        assert_eq!(&playfield_mask[68..70], &[0x03, 0x03]);
        assert!(collision_pixels[68].pf1);
        assert!(collision_pixels[68].pf2);
    }

    #[test]
    fn denise_manual_bitplane_shifter_uses_bpldat_latches_and_delay() {
        let shifter = DeniseManualBitplaneShifter::new([0x8000, 0x4000, 0, 0, 0, 0, 0, 0], 16);
        let mut control = ControlState {
            bplcon0: 0x2000,
            ..ControlState::default()
        };

        assert_eq!(
            shifter.sample(control, 0),
            Some(DeniseBitplaneSample {
                idx: 0x01,
                nplanes: 2,
                active: true,
            })
        );
        assert_eq!(
            shifter.sample(control, 1),
            Some(DeniseBitplaneSample {
                idx: 0x02,
                nplanes: 2,
                active: true,
            })
        );
        assert_eq!(shifter.sample(control, 16), None);

        control.bplcon1 = 0x0001;
        assert_eq!(
            shifter.sample(control, 0),
            Some(DeniseBitplaneSample {
                idx: 0x00,
                nplanes: 2,
                active: true,
            })
        );
        assert_eq!(
            shifter.sample(control, 1),
            Some(DeniseBitplaneSample {
                idx: 0x03,
                nplanes: 2,
                active: true,
            })
        );
    }

    #[test]
    fn ham8_control_bits_are_the_two_lowest_planes() {
        let mut palette = Palette::new();
        palette.write_entry(5, false, 0x0123);
        palette.write_entry(5, true, 0x0456); // 24-bit entry 5 = 0x142536
                                              // Set: control bits (pixel bits 0-1) = 00, palette index in the
                                              // top six bits.
        let set = ham8_rgb24(palette, 5 << 2, 0);
        assert_eq!(set, 0x0014_2536);
        // Modify blue (01): value bits replace the top six bits of the
        // component, the low two bits hold.
        let blue = ham8_rgb24(palette, 0b1010_1001, set);
        assert_eq!(blue, 0x0014_25AA); // 0xA8 | (0x36 & 0x03)
                                       // Modify red (10).
        let red = ham8_rgb24(palette, 0b1111_1110, blue);
        assert_eq!(red, 0x00FC_25AA);
        // Modify green (11).
        let green = ham8_rgb24(palette, 0b0100_0111, red);
        assert_eq!(green, 0x00FC_45AA); // 0x44 | (0x25 & 0x03)
    }

    #[test]
    fn denise_playfield_output_selects_ehb_ham_and_dual_playfield_colors() {
        let mut palette = Palette::new();
        palette.write_ocs(1, 0x0E86);
        palette.write_ocs(2, 0x0123);
        palette.write_ocs(9, 0x0456);
        let mut ham_color = rgb12_to_rgb24(0x0ABC);

        let ehb = ControlState {
            bplcon0: 0x6000,
            ..ControlState::default()
        };
        assert_eq!(
            denise_playfield_output(ehb, palette, 0x21, &mut ham_color),
            DenisePlayfieldOutput {
                color: rgb12_to_rgb24(0x0743),
                color_latch: 0x0E86,
                pf_mask: 2,
            }
        );

        let ham = ControlState {
            bplcon0: 0x6800,
            ..ControlState::default()
        };
        assert_eq!(
            denise_playfield_output(ham, palette, 0x2F, &mut ham_color).color,
            rgb12_to_rgb24(0x0F43)
        );

        let dual = ControlState {
            bplcon0: 0x2400,
            bplcon3: BPLCON3_PF2OF_DEFAULT,
            ..ControlState::default()
        };
        assert_eq!(
            denise_playfield_output(dual, palette, 0x02, &mut ham_color),
            DenisePlayfieldOutput {
                color: rgb12_to_rgb24(0x0456),
                color_latch: 0x0456,
                pf_mask: 2,
            }
        );
    }

    /// Plan 3.3: the Lisa resolution path. BPLAM XORs the pixel index,
    /// HAM8 modifies six bits per component, EHB halves in 8-bit space,
    /// and palette lookups read the full 24-bit banked store.
    #[test]
    fn aga_playfield_output_resolves_ham8_ehb_and_bplam() {
        let mut palette = Palette::new();
        palette.write_banked(0, 1, false, 0x0123);
        palette.write_banked(0, 1, true, 0x0456);
        let aga = ControlState {
            agnus_revision: AgnusRevision::AgaAlice,
            bplcon0: 0x0010, // BPU3: 8 planes
            ..ControlState::default()
        };
        let mut ham = 0u32;

        // Plain palette lookup composes high and low nibbles.
        let out = denise_playfield_output(aga, palette, 0x01, &mut ham);
        assert_eq!(out.color, 0x0014_2536);

        // HAM8 with 8 planes: control bits live in the two lowest planes,
        // the value in the top six. Control 01 modifies blue.
        let ham8 = ControlState {
            bplcon0: 0x0810,
            ..aga
        };
        ham = 0x00AA_BBCC;
        let out = denise_playfield_output(ham8, palette, (0x3F << 2) | 0x01, &mut ham);
        assert_eq!(out.color, 0x00AA_BBFC, "blue := 111111<<2 | old low bits");
        let out = denise_playfield_output(ham8, palette, (0x15 << 2) | 0x02, &mut ham);
        assert_eq!(
            out.color, 0x0056_BBFC,
            "red := 010101<<2 | old low-bit pair"
        );

        // BPLAM XORs the index before lookup: index 0 becomes 1.
        let masked = ControlState {
            bplcon4: 0x0100,
            ..aga
        };
        let mut ham2 = 0u32;
        let out = denise_playfield_output(masked, palette, 0x00, &mut ham2);
        assert_eq!(out.color, 0x0014_2536);

        // AGA EHB: entry halved per 8-bit component.
        let mut ehb_palette = Palette::new();
        ehb_palette.write_banked(0, 1, false, 0x0FFF);
        ehb_palette.write_banked(0, 1, true, 0x0EEE);
        let ehb = ControlState {
            agnus_revision: AgnusRevision::AgaAlice,
            bplcon0: 0x6000, // 6 planes, no HAM
            ..ControlState::default()
        };
        let mut ham3 = 0u32;
        let out = denise_playfield_output(ehb, ehb_palette, 0x21, &mut ham3);
        assert_eq!(out.color, (0x00FE_FEFE >> 1) & 0x007F_7F7F);
    }

    #[test]
    fn shres_playfield_output_selects_encoded_35ns_color_pair() {
        let mut palette = Palette::new();
        palette.write_ocs(1, 0x00F0);
        palette.write_ocs(4, 0x0F00);
        palette.write_ocs(5, 0x000F);
        let mut ham_color = 0;

        assert_eq!(
            denise_shres_playfield_output(palette, 0, 1, &mut ham_color),
            DenisePlayfieldOutput {
                color: rgb12_to_rgb24(0x0F00),
                color_latch: 0x0F00,
                pf_mask: 2,
            }
        );
        assert_eq!(
            denise_shres_playfield_output(palette, 1, 1, &mut ham_color).color,
            rgb12_to_rgb24(0x000F)
        );
    }

    #[test]
    fn clxcon_odd_sprite_enable_bits_or_odd_sprites_with_even_partner() {
        let mut sprite_group_mask = vec![0u8; 1];
        let mut collision_pixels = vec![CollisionPixel::default(); 1];
        let playfield_mask = vec![0u8; 1];

        assert_eq!(
            generated_sprite_collision_bits(
                1,
                0,
                0,
                &mut sprite_group_mask,
                &mut collision_pixels,
                &playfield_mask
            ),
            0
        );
        assert_eq!(sprite_group_mask[0], 0);

        assert_eq!(
            generated_sprite_collision_bits(
                0,
                0,
                0,
                &mut sprite_group_mask,
                &mut collision_pixels,
                &playfield_mask
            ),
            0
        );
        assert_eq!(sprite_group_mask[0], 0b0001);

        assert_eq!(
            generated_sprite_collision_bits(
                3,
                0,
                0,
                &mut sprite_group_mask,
                &mut collision_pixels,
                &playfield_mask
            ),
            0
        );
        assert_eq!(sprite_group_mask[0], 0b0001);

        assert_eq!(
            generated_sprite_collision_bits(
                3,
                0,
                1 << 13,
                &mut sprite_group_mask,
                &mut collision_pixels,
                &playfield_mask
            ),
            1 << 9
        );
        assert_eq!(sprite_group_mask[0], 0b0011);
    }

    #[test]
    fn attached_sprite_pair_collision_groups_or_odd_pixels_through_clxcon() {
        let mut sprite_group_mask = vec![0b0010u8; 1];
        let mut collision_pixels = vec![CollisionPixel::default(); 1];
        let playfield_mask = vec![0u8; 1];

        assert_eq!(
            generated_sprite_pair_collision_bits(
                0,
                0,
                0,
                false,
                true,
                &mut sprite_group_mask,
                &mut collision_pixels,
                &playfield_mask,
            ),
            0
        );
        assert_eq!(sprite_group_mask[0], 0b0010);

        assert_eq!(
            generated_sprite_pair_collision_bits(
                0,
                0,
                1 << 12,
                false,
                true,
                &mut sprite_group_mask,
                &mut collision_pixels,
                &playfield_mask,
            ),
            1 << 9
        );
        assert_eq!(sprite_group_mask[0], 0b0011);
    }

    #[test]
    fn ham6_pixels_modify_previous_rgb_components() {
        let mut palette = Palette::new();
        palette.write_ocs(3, 0x0123);

        let direct = ham6_rgb12(palette, 0x03, 0x0FFF);
        assert_eq!(direct, 0x0123);
        assert_eq!(ham6_rgb12(palette, 0x14, direct), 0x0124);
        assert_eq!(ham6_rgb12(palette, 0x25, direct), 0x0523);
        assert_eq!(ham6_rgb12(palette, 0x36, direct), 0x0163);
    }

    #[test]
    fn manual_ham_bitplane_word_delays_select_by_one_pixel() {
        let mut palette = Palette::new();
        palette.write_ocs(1, 0x0F00);
        let control = ControlState {
            bplcon0: 0xE800,
            ..ControlState::default()
        };
        let base_palettes = [palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let base_controls = [control; FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];
        let mut playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut clxdat = 0u16;
        let mut planes = [0u16; 8];
        planes[0] = 0x8000;
        let segments = [ManualBplSegment {
            line: 0,
            hpos: 0,
            x: 0,
            planes,
            palette,
        }];

        render_manual_bpl_segments(
            &segments,
            &mut fb,
            &mut playfield_mask,
            &mut collision_pixels,
            &mut clxdat,
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
        );

        assert_eq!(fb[0], rgb12_to_rgba8(0));
        assert_eq!(fb[1], rgb12_to_rgba8(0x0F00));
    }

    #[test]
    fn ham_hold_resets_while_display_window_is_closed() {
        let mut palette = Palette::new();
        palette.write_ocs(1, 0x0F00);
        let control = ControlState {
            bplcon0: 0xE800,
            diwstrt: ((PAL_VISIBLE_LINE0 as u16) << 8) | (DIW_HSTART_FB0 as u16 + 1),
            diwstop: ((PAL_VISIBLE_LINE0 as u16 + 1) << 8) | 0x00C1,
            ..ControlState::default()
        };
        let base_palettes = [palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let base_controls = [control; FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];
        let mut playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut clxdat = 0u16;
        let mut planes = [0u16; 8];
        planes[0] = 0x4000;
        let segments = [ManualBplSegment {
            line: 0,
            hpos: 0,
            x: 0,
            planes,
            palette,
        }];

        render_manual_bpl_segments(
            &segments,
            &mut fb,
            &mut playfield_mask,
            &mut collision_pixels,
            &mut clxdat,
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
        );

        assert_eq!(fb[2], rgb12_to_rgba8(0));
    }

    #[test]
    fn ham_pipeline_uses_palette_at_output_pixel_boundary() {
        let mut palette = Palette::new();
        palette.write_ocs(1, 0x0F00);
        let control = ControlState {
            bplcon0: 0xE800,
            ..ControlState::default()
        };
        let base_palettes = [palette; FB_HEIGHT];
        let mut palette_segments = vec![Vec::new(); FB_HEIGHT];
        palette_segments[0].push(PaletteSegment {
            x: 1,
            entry: 1,
            loct: false,
            value: 0x00F0,
        });
        let base_controls = [control; FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];
        let mut playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut clxdat = 0u16;
        let mut planes = [0u16; 8];
        planes[0] = 0x8000;
        let segments = [ManualBplSegment {
            line: 0,
            hpos: 0,
            x: 0,
            planes,
            palette,
        }];

        render_manual_bpl_segments(
            &segments,
            &mut fb,
            &mut playfield_mask,
            &mut collision_pixels,
            &mut clxdat,
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
        );

        assert_eq!(fb[1], rgb12_to_rgba8(0x00F0));
    }

    #[test]
    fn render_events_replay_palette_segments_by_beam_position() {
        let mut state = blank_state();
        let mut base_palettes = [state.palette; FB_HEIGHT];
        let mut palette_segments = vec![Vec::new(); FB_HEIGHT];
        let mut base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let mut control_segments = vec![Vec::new(); FB_HEIGHT];
        let mut manual_bpl_segments = Vec::new();
        let events = [
            beam_event(0x45, COPPER_WAIT_HPOS_FB0 as u32, 0x0180, 0x0402),
            beam_event(0x47, COPPER_WAIT_HPOS_FB0 as u32, 0x0180, 0x0103),
        ];

        apply_render_events(
            &mut state,
            &events,
            &mut base_palettes,
            &mut palette_segments,
            &mut base_controls,
            &mut control_segments,
            &mut manual_bpl_segments,
        );

        let maroon_line = (0x45 - 0x2C) as usize;
        let border_line = (0x47 - 0x2C) as usize;
        assert_eq!(palette_segments[maroon_line][0].entry, 0);
        assert_eq!(palette_segments[maroon_line][0].value, 0x0402);
        assert_eq!(base_palettes[maroon_line + 1][0], 0x0402);
        assert_eq!(palette_segments[border_line][0].entry, 0);
        assert_eq!(palette_segments[border_line][0].value, 0x0103);
        assert_eq!(base_palettes[border_line + 1][0], 0x0103);
    }

    #[test]
    fn color00_overscan_write_does_not_backfill_row_start() {
        let mut state = blank_state();
        state.palette.write_ocs(0, 0x0000);
        state.diwstrt = ((PAL_VISIBLE_LINE0 as u16) << 8) | STANDARD_DIW_HSTART as u16;
        state.diwstop = (((PAL_VISIBLE_LINE0 + 1) as u16) << 8) | STANDARD_DIW_HSTOP as u16;
        let mut base_palettes = [state.palette; FB_HEIGHT];
        let mut palette_segments = vec![Vec::new(); FB_HEIGHT];
        let mut base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let mut control_segments = vec![Vec::new(); FB_HEIGHT];
        let mut manual_bpl_segments = Vec::new();
        let beam_y = PAL_VISIBLE_LINE0 as u32;
        let events = [
            beam_event(beam_y, 68, 0x0180, 0x087A),
            beam_event(beam_y, 76, 0x0180, 0x0000),
        ];

        apply_render_events(
            &mut state,
            &events,
            &mut base_palettes,
            &mut palette_segments,
            &mut base_controls,
            &mut control_segments,
            &mut manual_bpl_segments,
        );

        assert_eq!(palette_segments[0][0].x, 60);
        assert_eq!(palette_segments[0][0].value, 0x087A);
        assert_eq!(palette_segments[0][1].x, 92);
        assert_eq!(palette_segments[0][1].value, 0x0000);
        assert_eq!(base_palettes[0][0], 0x0000);

        let mut fb = vec![0; FB_PIXELS];
        fill_background(
            &mut fb,
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
        );

        assert_eq!(fb[0], rgb12_to_rgba8(0x0000));
        assert_eq!(fb[59], rgb12_to_rgba8(0x0000));
        assert_eq!(fb[60], rgb12_to_rgba8(0x087A));
        assert_eq!(fb[91], rgb12_to_rgba8(0x087A));
        assert_eq!(fb[92], rgb12_to_rgba8(0x0000));
    }

    #[test]
    fn render_events_sample_bplcon_control_at_beam_positions() {
        let mut state = blank_state();
        let mut base_palettes = [state.palette; FB_HEIGHT];
        let mut palette_segments = vec![Vec::new(); FB_HEIGHT];
        let mut base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let mut control_segments = vec![Vec::new(); FB_HEIGHT];
        let mut manual_bpl_segments = Vec::new();
        let events = [
            beam_event(0x50, COPPER_WAIT_HPOS_FB0 as u32, 0x0100, 0x6800),
            beam_event(0x52, COPPER_WAIT_HPOS_FB0 as u32, 0x0100, 0x4000),
        ];

        apply_render_events(
            &mut state,
            &events,
            &mut base_palettes,
            &mut palette_segments,
            &mut base_controls,
            &mut control_segments,
            &mut manual_bpl_segments,
        );

        let ham_line = (0x50 - 0x2C) as usize;
        let direct_line = (0x52 - 0x2C) as usize;
        assert_eq!(control_segments[ham_line][0].control.bplcon0, 0x6800);
        assert_eq!(base_controls[ham_line + 1].bplcon0, 0x6800);
        assert_eq!(control_segments[direct_line][0].control.bplcon0, 0x4000);
        assert_eq!(base_controls[direct_line + 1].bplcon0, 0x4000);
    }

    #[test]
    fn aga_bplcon4_splits_sprite_base_from_bitplane_xor_timing() {
        let mut state = blank_state();
        state.agnus_revision = AgnusRevision::AgaAlice;
        state.bplcon4 = 0xAA09;
        let mut base_palettes = [state.palette; FB_HEIGHT];
        let mut palette_segments = vec![Vec::new(); FB_HEIGHT];
        let mut base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let mut control_segments = vec![Vec::new(); FB_HEIGHT];
        let mut manual_bpl_segments = Vec::new();
        let hpos = (COPPER_WAIT_HPOS_FB0 + 20) as u32;
        let events = [beam_event(0x50, hpos, 0x010C, 0x5507)];

        apply_render_events(
            &mut state,
            &events,
            &mut base_palettes,
            &mut palette_segments,
            &mut base_controls,
            &mut control_segments,
            &mut manual_bpl_segments,
        );

        let line = (0x50 - 0x2C) as usize;
        let sprite_x = sprite_palette_control_framebuffer_x(hpos);
        assert_eq!(sprite_x, color_write_framebuffer_x(hpos).saturating_sub(4));
        let beam_x = beam_to_framebuffer_x_unclamped(hpos) as usize;
        assert!(sprite_x < beam_x);
        assert_eq!(control_segments[line].len(), 2);
        assert_eq!(control_segments[line][0].x, sprite_x);
        assert_eq!(control_segments[line][0].control.bplcon4, 0xAA07);
        assert_eq!(control_segments[line][1].x, beam_x);
        assert_eq!(control_segments[line][1].control.bplcon4, 0x5507);
        assert_eq!(
            control_at_x(base_controls[line], &control_segments[line], beam_x - 1).bplcon4,
            0xAA07
        );
        assert_eq!(
            control_at_x(base_controls[line], &control_segments[line], beam_x).bplcon4,
            0x5507
        );
    }

    #[test]
    fn ddf_events_update_later_bitplane_fetch_geometry() {
        let mut state = RenderState {
            agnus_revision: AgnusRevision::Ecs8372Rev4,
            ddfstrt: 0x0038,
            ddfstop: 0x0038,
            ..blank_state()
        };
        let mut base_palettes = [state.palette; FB_HEIGHT];
        let mut palette_segments = vec![Vec::new(); FB_HEIGHT];
        let mut base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let mut control_segments = vec![Vec::new(); FB_HEIGHT];
        let mut manual_bpl_segments = Vec::new();
        let events = [beam_event(
            0x50,
            COPPER_WAIT_HPOS_FB0 as u32,
            0x0094,
            0x0040,
        )];

        apply_render_events(
            &mut state,
            &events,
            &mut base_palettes,
            &mut palette_segments,
            &mut base_controls,
            &mut control_segments,
            &mut manual_bpl_segments,
        );

        let line = (0x50 - 0x2C) as usize;
        assert_eq!(base_controls[line - 1].words_per_row(320), 1);
        assert_eq!(control_segments[line][0].control.words_per_row(320), 2);
        assert_eq!(base_controls[line + 1].words_per_row(320), 2);
    }

    #[test]
    fn display_plan_events_record_beam_timed_palette_control_and_bpldat_writes() {
        let mut state = RenderState {
            dmacon: DMACON_DMAEN | DMACON_BPLEN,
            bplcon0: 0x1000,
            ddfstrt: 0x0038,
            ddfstop: 0x0038,
            ..blank_state()
        };
        let mut base_palettes = [state.palette; FB_HEIGHT];
        let mut palette_segments = vec![Vec::new(); FB_HEIGHT];
        let mut base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let mut control_segments = vec![Vec::new(); FB_HEIGHT];
        let mut manual_bpl_segments = Vec::new();
        let mut display_line_events = vec![Vec::new(); FB_HEIGHT];
        let events = [
            beam_event(0x50, 0x0040, 0x0180, 0x0ABC),
            beam_event(0x50, 0x0042, 0x0102, 0x0004),
            beam_event(0x50, 0x0044, 0x0108, 0x0002),
            beam_event(0x50, 0x0046, 0x0116, 0x8000),
        ];

        apply_render_events_and_collect_display_plan_events(
            &mut state,
            &events,
            &mut base_palettes,
            &mut palette_segments,
            &mut base_controls,
            &mut control_segments,
            &mut manual_bpl_segments,
            Some(&mut display_line_events),
        );

        let line = (0x50 - 0x2C) as usize;
        assert!(
            display_line_events[line].contains(&DisplayLinePlanEvent::PaletteChange {
                hpos: 0x0040,
                x: color_write_framebuffer_x(0x0040),
                palette: {
                    let mut palette = Palette::from_ocs([0x0103; 32]);
                    palette.write_ocs(0, 0x0ABC);
                    Box::new(palette)
                },
            })
        );
        assert!(display_line_events[line].iter().any(|event| matches!(
            event,
            DisplayLinePlanEvent::ControlChange {
                hpos: 0x0042,
                x: 104,
                control,
            } if control.bplcon1 == 0x0004
        )));
        assert!(display_line_events[line].iter().any(|event| matches!(
            event,
            DisplayLinePlanEvent::ControlChange {
                hpos: 0x0044,
                x: 112,
                control,
            } if control.bpl1mod == 0x0002
        )));
        assert!(
            display_line_events[line].contains(&DisplayLinePlanEvent::BpldatWrite {
                hpos: 0x0046,
                x: beam_to_framebuffer_x_unclamped(0x0046),
                plane: 3,
                value: 0x8000,
            })
        );
    }

    #[test]
    fn events_above_the_visible_area_fold_to_line_zero_start_state() {
        // The boot ROM copper list programs the display window, BPLCON0 and
        // FMODE on the line *before* the window opens (vpos 0x2B for a
        // standard 0x2C start). Those writes happen before the first
        // framebuffer line: they must contribute to line 0's start state,
        // not become mid-line segments at their horizontal position (which
        // split the first display line into border-black and colour-0 spans).
        let mut state = blank_state();
        let mut base_palettes = [state.palette; FB_HEIGHT];
        let mut palette_segments = vec![Vec::new(); FB_HEIGHT];
        let mut base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let mut control_segments = vec![Vec::new(); FB_HEIGHT];
        let mut manual_bpl_segments = Vec::new();
        let events = [
            // Mid-line positions on the line above the visible area.
            beam_event(0x2B, 0x0052, 0x008E, 0x2C81),
            beam_event(0x2B, 0x0056, 0x0100, 0x1200),
            beam_event(0x2B, 0x0060, 0x0180, 0x0ABC),
            // A write on the first visible line keeps its position.
            beam_event(0x2C, 0x0052, 0x0102, 0x0004),
        ];

        apply_render_events(
            &mut state,
            &events,
            &mut base_palettes,
            &mut palette_segments,
            &mut base_controls,
            &mut control_segments,
            &mut manual_bpl_segments,
        );

        for segment in &control_segments[0] {
            if segment.control.bplcon1 == 0x0004 {
                // The on-line event lands at its beam position.
                assert_eq!(segment.x, beam_to_framebuffer_x_unclamped(0x0052) as usize);
            } else {
                assert_eq!(segment.x, 0, "pre-visible control change must fold to x=0");
            }
        }
        assert!(control_segments[0]
            .iter()
            .any(|segment| segment.control.diwstrt == 0x2C81));
        assert_eq!(palette_segments[0][0].x, 0);
    }

    #[test]
    fn same_line_ddfstrt_extension_does_not_fetch_already_missed_words() {
        let mut state = RenderState {
            dmacon: DMACON_DMAEN | DMACON_BPLEN,
            bplcon0: 0x1000,
            ddfstrt: 0x0050,
            ddfstop: 0x0050,
            ..blank_state()
        };
        let mut base_palettes = [state.palette; FB_HEIGHT];
        let mut palette_segments = vec![Vec::new(); FB_HEIGHT];
        let mut base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let mut control_segments = vec![Vec::new(); FB_HEIGHT];
        let mut manual_bpl_segments = Vec::new();
        let events = [beam_event(0x50, 0x0040, 0x0092, 0x0038)];

        apply_render_events(
            &mut state,
            &events,
            &mut base_palettes,
            &mut palette_segments,
            &mut base_controls,
            &mut control_segments,
            &mut manual_bpl_segments,
        );

        let line = (0x50 - 0x2C) as usize;
        assert_eq!(
            line_fetch_hpos_for_word(base_controls[line], &control_segments[line], 0),
            None
        );
        assert_eq!(
            line_fetch_hpos_for_word(base_controls[line], &control_segments[line], 1),
            Some(0x0040)
        );
        assert_eq!(
            line_fetch_hpos_for_word(base_controls[line], &control_segments[line], 3),
            Some(0x0050)
        );
    }

    #[test]
    fn same_line_ddfstrt_shrink_preserves_already_scheduled_words() {
        let mut state = RenderState {
            agnus_revision: AgnusRevision::Ecs8372Rev4,
            dmacon: DMACON_DMAEN | DMACON_BPLEN,
            bplcon0: 0x1000,
            ddfstrt: 0x0038,
            ddfstop: 0x0050,
            ..blank_state()
        };
        let mut base_palettes = [state.palette; FB_HEIGHT];
        let mut palette_segments = vec![Vec::new(); FB_HEIGHT];
        let mut base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let mut control_segments = vec![Vec::new(); FB_HEIGHT];
        let mut manual_bpl_segments = Vec::new();
        let events = [beam_event(0x50, 0x0040, 0x0092, 0x0050)];

        apply_render_events(
            &mut state,
            &events,
            &mut base_palettes,
            &mut palette_segments,
            &mut base_controls,
            &mut control_segments,
            &mut manual_bpl_segments,
        );

        let line = (0x50 - 0x2C) as usize;
        assert_eq!(
            line_fetch_hpos_for_word(base_controls[line], &control_segments[line], 0),
            Some(0x0038)
        );
        assert_eq!(
            line_fetch_hpos_for_word(base_controls[line], &control_segments[line], 1),
            None
        );
    }

    #[test]
    fn display_line_fetch_plan_matches_per_word_scan_across_beam_timed_control() {
        let mut state = RenderState {
            dmacon: DMACON_DMAEN | DMACON_BPLEN,
            bplcon0: 0x3000,
            bplcon1: 0,
            ddfstrt: 0x0038,
            ddfstop: 0x0058,
            ..blank_state()
        };
        let mut base_palettes = [state.palette; FB_HEIGHT];
        let mut palette_segments = vec![Vec::new(); FB_HEIGHT];
        let mut base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let mut control_segments = vec![Vec::new(); FB_HEIGHT];
        let mut manual_bpl_segments = Vec::new();
        let events = [
            beam_event(0x50, 0x003C, 0x0102, 0x0012),
            beam_event(0x50, 0x0040, 0x0092, 0x0050),
            beam_event(0x50, 0x0048, 0x0100, 0x7800),
            beam_event(0x50, 0x0050, 0x0092, 0x0038),
            beam_event(0x50, 0x0058, 0x0094, 0x0060),
        ];

        apply_render_events(
            &mut state,
            &events,
            &mut base_palettes,
            &mut palette_segments,
            &mut base_controls,
            &mut control_segments,
            &mut manual_bpl_segments,
        );

        let line = (0x50 - 0x2C) as usize;
        let words_per_row = line_words_per_row(base_controls[line], &control_segments[line]);
        let dma_planes = line_max_dma_planes(base_controls[line], &control_segments[line]);
        let plans = line_fetch_plans_for_line(
            base_controls[line],
            &control_segments[line],
            words_per_row,
            dma_planes,
        );

        for (word_idx, actual) in plans.iter().enumerate() {
            let expected = line_fetch_plan_for_word(
                base_controls[line],
                &control_segments[line],
                word_idx,
                dma_planes,
            );
            assert_eq!(
                actual.word_fetch_hpos, expected.word_fetch_hpos,
                "word {word_idx}"
            );
            assert_eq!(
                actual.iter().collect::<Vec<_>>(),
                expected.iter().collect::<Vec<_>>(),
                "word {word_idx}"
            );
        }
    }

    #[test]
    fn manual_bpl1dat_snapshots_dma_updated_bpldat_latches() {
        let mut state = RenderState {
            dmacon: DMACON_DMAEN | DMACON_BPLEN,
            bplcon0: 0x3000,
            ddfstrt: 0x0038,
            ddfstop: 0x0038,
            ..blank_state()
        };
        state.bpldat[1] = 0x4000;
        let base_control = ControlState::from_render_state(&state);
        let base_controls = [base_control; FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let hpos = 0x0040;
        let mut segments = [ManualBplSegment {
            line: 0,
            hpos,
            x: beam_to_framebuffer_x_unclamped(hpos),
            planes: [0; 8],
            palette: state.palette,
        }];
        let mut captured_rows = vec![None; FB_HEIGHT];
        let mut planes: [Vec<u16>; 8] = std::array::from_fn(|_| vec![0]);
        planes[1][0] = 0x8000;
        captured_rows[0] = Some(CapturedBitplaneRow {
            nplanes: 3,
            words_per_row: 1,
            planes,
        });
        let events = [beam_event(PAL_VISIBLE_LINE0 as u32, hpos, 0x0110, 0x0000)];

        seed_manual_bpl_segments_from_latches(
            &mut segments,
            state.bpldat,
            &events,
            &base_controls,
            &control_segments,
            &captured_rows,
            PAL_VISIBLE_LINE0,
        );

        assert_eq!(segments[0].planes[0], 0x0000);
        assert_eq!(segments[0].planes[1], 0x8000);
    }

    #[test]
    fn manual_bpl1dat_before_dma_output_stops_at_dma_shifter_load() {
        let mut palette = Palette::new();
        palette.write_ocs(1, 0x0F00);
        let control = ControlState {
            dmacon: DMACON_DMAEN | DMACON_BPLEN,
            bplcon0: 0x1000,
            diwstrt: ((PAL_VISIBLE_LINE0 as u16) << 8) | STANDARD_DIW_HSTART as u16,
            diwstop: (((PAL_VISIBLE_LINE0 + 1) as u16) << 8) | STANDARD_DIW_HSTOP as u16,
            ddfstrt: 0x0050,
            ddfstop: 0x00D0,
            ..ControlState::default()
        };
        let base_palettes = [palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let base_controls = [control; FB_HEIGHT];
        let control_segments = vec![Vec::new(); FB_HEIGHT];
        let dma_output_x = bitplane_dma_output_start_x(
            control,
            &[],
            control.display_window_x().0,
            control.words_per_row(native_frame_width_for_control(control)),
            control.dma_planes(),
        )
        .unwrap();
        let mut fb = vec![rgb12_to_rgba8(0x000F); FB_PIXELS];
        let mut playfield_mask = vec![0u8; FB_PIXELS];
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        let mut clxdat = 0u16;
        let mut planes = [0u16; 8];
        planes[0] = 0xFFFF;
        let segments = [ManualBplSegment {
            line: 0,
            hpos: 0,
            x: dma_output_x as i32 - 8,
            planes,
            palette,
        }];
        let mut dma_output_start_x_by_line = vec![None; FB_HEIGHT];
        dma_output_start_x_by_line[0] = Some(dma_output_x);

        render_manual_bpl_segments_with_visible_line0(
            &segments,
            &mut fb,
            &mut playfield_mask,
            &mut collision_pixels,
            &mut clxdat,
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &dma_output_start_x_by_line,
            PAL_VISIBLE_LINE0,
            0.0,
            0,
        );

        assert_eq!(fb[dma_output_x - 2], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[dma_output_x], rgb12_to_rgba8(0x000F));
    }

    #[test]
    fn modulo_events_update_later_bitplane_row_advance() {
        let mut state = RenderState {
            bpl1mod: 0,
            ..blank_state()
        };
        let mut base_palettes = [state.palette; FB_HEIGHT];
        let mut palette_segments = vec![Vec::new(); FB_HEIGHT];
        let mut base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let mut control_segments = vec![Vec::new(); FB_HEIGHT];
        let mut manual_bpl_segments = Vec::new();
        let events = [beam_event(
            0x50,
            COPPER_WAIT_HPOS_FB0 as u32,
            0x0108,
            0x0004,
        )];

        apply_render_events(
            &mut state,
            &events,
            &mut base_palettes,
            &mut palette_segments,
            &mut base_controls,
            &mut control_segments,
            &mut manual_bpl_segments,
        );

        let line = (0x50 - 0x2C) as usize;
        let before = base_controls[line - 1];
        let after = line_control_at_x(&base_controls, &control_segments, line, 0);
        assert_eq!(before.bpl1mod, 0);
        assert_eq!(after.bpl1mod, 4);

        let mut ptrs = [0x0100, 0, 0, 0, 0, 0, 0, 0];
        advance_bitplane_ptrs_for_rows(&mut ptrs, 1, 1, 1, &before, 0, 0x001F_FFFF);
        assert_eq!(ptrs[0], 0x0102);
        advance_bitplane_ptrs_for_rows(&mut ptrs, 1, 1, 1, &after, 0, 0x001F_FFFF);
        assert_eq!(ptrs[0], 0x0108);
    }

    #[test]
    fn dmacon_events_update_later_bitplane_dma_control() {
        let mut state = RenderState {
            dmacon: DMACON_DMAEN | DMACON_BPLEN,
            bplcon0: 0x1000,
            ..blank_state()
        };
        let mut base_palettes = [state.palette; FB_HEIGHT];
        let mut palette_segments = vec![Vec::new(); FB_HEIGHT];
        let mut base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let mut control_segments = vec![Vec::new(); FB_HEIGHT];
        let mut manual_bpl_segments = Vec::new();
        let events = [beam_event(
            0x50,
            COPPER_WAIT_HPOS_FB0 as u32,
            0x0096,
            DMACON_BPLEN,
        )];

        apply_render_events(
            &mut state,
            &events,
            &mut base_palettes,
            &mut palette_segments,
            &mut base_controls,
            &mut control_segments,
            &mut manual_bpl_segments,
        );

        let line = (0x50 - 0x2C) as usize;
        assert!(base_controls[line - 1].bitplane_dma_enabled());
        assert!(!control_segments[line][0].control.bitplane_dma_enabled());
        assert!(!base_controls[line + 1].bitplane_dma_enabled());
    }

    #[test]
    fn clxcon_events_update_later_collision_control() {
        let mut state = RenderState {
            clxcon: 0,
            ..blank_state()
        };
        let mut base_palettes = [state.palette; FB_HEIGHT];
        let mut palette_segments = vec![Vec::new(); FB_HEIGHT];
        let mut base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let mut control_segments = vec![Vec::new(); FB_HEIGHT];
        let mut manual_bpl_segments = Vec::new();
        let events = [beam_event(
            0x50,
            COPPER_WAIT_HPOS_FB0 as u32,
            0x0098,
            1 << 12,
        )];

        apply_render_events(
            &mut state,
            &events,
            &mut base_palettes,
            &mut palette_segments,
            &mut base_controls,
            &mut control_segments,
            &mut manual_bpl_segments,
        );

        let line = (0x50 - 0x2C) as usize;
        assert_eq!(base_controls[line - 1].clxcon, 0);
        assert_eq!(control_segments[line][0].control.clxcon, 1 << 12);
        assert_eq!(base_controls[line + 1].clxcon, 1 << 12);
    }

    #[test]
    fn sprite_collisions_use_beam_timed_clxcon_control() {
        let mut state = blank_state();
        state.dmacon = DMACON_DMAEN | DMACON_SPREN;
        state.clxcon = 0;
        state.palette.write_ocs(17, 0x0F00);
        let ram = vec![0; 64];
        let base_palettes = [state.palette; FB_HEIGHT];
        let palette_segments = vec![Vec::new(); FB_HEIGHT];
        let base_control = ControlState::from_render_state(&state);
        let base_controls = [base_control; FB_HEIGHT];
        let mut control_segments = vec![Vec::new(); FB_HEIGHT];
        let mut active_control = base_control;
        active_control.clxcon = 1 << 12;
        control_segments[0].push(ControlSegment {
            x: 0,
            control: active_control,
        });
        let mut playfield_mask = vec![0u8; FB_PIXELS];
        playfield_mask[0] = 0x02;
        playfield_mask[1] = 0x02;
        let mut collision_pixels = vec![CollisionPixel::default(); FB_PIXELS];
        collision_pixels[0] = CollisionPixel {
            pf1: false,
            pf2: true,
            pf1_match: false,
            pf2_match: true,
        };
        collision_pixels[1] = collision_pixels[0];
        let mut fb = vec![rgb12_to_rgba8(0); FB_PIXELS];
        let captured = [CapturedSpriteLine {
            sprite: 1,
            hstart: DIW_HSTART_FB0,
            hsub_70ns: false,
            beam_y: PAL_VISIBLE_LINE0,
            data: 0x8000,
            datb: 0,
            attached: false,
            data_ext: [0; 3],
            datb_ext: [0; 3],
            width_words: 1,
        }];

        let clxdat = render_sprites(
            &state,
            &ram,
            &mut fb,
            SpriteClip {
                x_start: 0,
                x_stop: FB_WIDTH,
                y_start: 0,
                y_stop: FB_HEIGHT,
            },
            &base_palettes,
            &palette_segments,
            &base_controls,
            &control_segments,
            &playfield_mask,
            &mut collision_pixels,
            [false; 8],
            &captured,
            true,
        );

        assert_eq!(clxdat & (1 << 5), 1 << 5);
    }

    #[test]
    fn displayed_row_uses_control_at_display_start_not_line_end() {
        let mut state = blank_state();
        let mut base_palettes = [state.palette; FB_HEIGHT];
        let mut palette_segments = vec![Vec::new(); FB_HEIGHT];
        let mut base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let mut control_segments = vec![Vec::new(); FB_HEIGHT];
        let mut manual_bpl_segments = Vec::new();
        let events = [
            beam_event(0x50, (COPPER_WAIT_HPOS_FB0 + 10) as u32, 0x0102, 0x0011),
            beam_event(0x50, (COPPER_WAIT_HPOS_FB0 + 200) as u32, 0x0102, 0x0022),
        ];

        apply_render_events(
            &mut state,
            &events,
            &mut base_palettes,
            &mut palette_segments,
            &mut base_controls,
            &mut control_segments,
            &mut manual_bpl_segments,
        );

        let line = (0x50 - 0x2C) as usize;
        assert_eq!(
            line_control_at_x(&base_controls, &control_segments, line, 64).bplcon1,
            0x0011
        );
        assert_eq!(base_controls[line + 1].bplcon1, 0x0022);
    }

    #[test]
    fn unchanged_control_writes_do_not_create_render_segments() {
        let mut state = blank_state();
        state.bplcon1 = 0x0011;
        let mut base_palettes = [state.palette; FB_HEIGHT];
        let mut palette_segments = vec![Vec::new(); FB_HEIGHT];
        let mut base_controls = [ControlState::from_render_state(&state); FB_HEIGHT];
        let mut control_segments = vec![Vec::new(); FB_HEIGHT];
        let mut manual_bpl_segments = Vec::new();
        let events = [
            beam_event(0x50, COPPER_WAIT_HPOS_FB0 as u32, 0x0102, 0x0011),
            beam_event(0x50, (COPPER_WAIT_HPOS_FB0 + 4) as u32, 0x0102, 0x0011),
            beam_event(0x50, (COPPER_WAIT_HPOS_FB0 + 8) as u32, 0x0102, 0x0022),
        ];

        apply_render_events(
            &mut state,
            &events,
            &mut base_palettes,
            &mut palette_segments,
            &mut base_controls,
            &mut control_segments,
            &mut manual_bpl_segments,
        );

        let line = (0x50 - 0x2C) as usize;
        assert_eq!(control_segments[line].len(), 1);
        assert_eq!(control_segments[line][0].x, 32);
        assert_eq!(control_segments[line][0].control.bplcon1, 0x0022);
    }
}
