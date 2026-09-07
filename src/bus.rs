// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared Amiga bus state used by the CPU core, chipset, and host I/O.
//! It owns chip RAM, ROM, CIA state, and custom-chip state, and exposes
//! typed read/write methods for memory-mapped devices.

use crate::chipset::agnus::{
    bitplane_dma_planes_for_fmode, sprite_dma_disabled_by_bitplane_ddf, wide_fetch_word_address,
    Agnus, AgnusRevision, AgnusTick, VideoStandard, BEAMCON0_DUAL, BEAMCON0_HARDDIS,
    COLORCLOCKS_PER_LINE, NTSC_LINES, NTSC_LONG_COLORCLOCKS_PER_LINE, PAL_LINES,
};
use crate::chipset::blitter::{BlitBusAccess, Blitter};
use crate::chipset::cia::{
    reg_from_addr, Cia, CiaSideEffect, Which, REG_DDRA, REG_DDRB, REG_PRA, REG_PRB, REG_TODLO,
};
use crate::chipset::copper::{Copper, CopperSlotAction, CopperWait, DMACON_COPEN};
use crate::chipset::denise::{
    color_register_value, BitplaneMode, Denise, DeniseRevision, DiwHigh, Palette, BPLCON2_RDRAM,
};
use crate::chipset::keyboard::KeyboardMcu;
use crate::chipset::paula::{
    pending_ipl, Paula, PotPins, DMACON_DMAEN, INT_BLIT, INT_COPER, INT_DSKBLK, INT_DSKSYNC,
    INT_EXTER, INT_PORTS, INT_VERTB, PAULA_CLOCK_HZ,
};
use crate::floppy::FloppyController;
use crate::gayle::Gayle;
use crate::memory::{Memory, RamInit};
use crate::rtc::{Rtc, RtcChip};
use crate::timebase::{Duration, Instant};
use crate::video::{beam::BeamEventIndex, FrameGeometry, FB_HEIGHT, FB_WIDTH, MAX_VISIBLE_LINES};
use log::trace;
use std::io::Write;

const CHIP_BUS_SLOT_CCK: u32 = 1;
const BLITTER_DEADLINE_SLOT_SCAN_LIMIT: u32 = 64;

// Number of consecutive CPU bus-miss color clocks a busy "nice" (BLTPRI=0)
// blitter holds the chip bus before yielding one slot to the waiting CPU.
//
// Grounded in the Minimig RTL (agnus.v): its `bls_cnt` increments on the !cck
// phase of EACH color clock (clk7 has two ticks per color clock, cck and !cck),
// i.e. once per color clock, up to BLS_CNT_MAX=3. So the blitter is blocked
// after the CPU has missed 3 color clocks -- which also matches the HRM rule
// that a waiting 68000 gets one bus cycle in four when BLTPRI=0.
//
// History: this was 2, which over-starved the blitter, then 6 after a misread
// of the RTL doubled the threshold by treating !cck as every-other color clock.
// That over-starved the CPU instead. Cross-emulator DMA accounting for a
// blitter-heavy frame (blitter 34892 cck, CPU 17882 cck) confirms 3.
pub(crate) const BLITTER_SLOWDOWN_CPU_MISS_LIMIT: u8 = 3;

#[cfg(feature = "internal-diagnostics")]
fn exp_miss_limit() -> u8 {
    use std::sync::OnceLock;
    static V: OnceLock<u8> = OnceLock::new();
    *V.get_or_init(|| {
        crate::envcfg::var("COPPERLINE_EXP_MISS_LIMIT")
            .and_then(|s| s.parse::<u8>().ok())
            .unwrap_or(BLITTER_SLOWDOWN_CPU_MISS_LIMIT)
    })
}

#[cfg(not(feature = "internal-diagnostics"))]
fn exp_miss_limit() -> u8 {
    BLITTER_SLOWDOWN_CPU_MISS_LIMIT
}

/// Cached COPPERLINE_DBG_CIA gate (read once). Consulted on the per-device-tick
/// path, so it must not do a live env lookup.
fn dbg_cia_on() -> bool {
    use std::sync::OnceLock;
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| crate::envcfg::flag("COPPERLINE_DBG_CIA"))
}

#[cfg(feature = "internal-diagnostics")]
fn no_bus_arb() -> bool {
    use std::sync::OnceLock;
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| crate::envcfg::flag("COPPERLINE_NO_BUS_ARB"))
}

#[cfg(not(feature = "internal-diagnostics"))]
fn no_bus_arb() -> bool {
    false
}

/// Cached COPPERLINE_DIAG_BLT_SLOTS gate (read once). Enables the blitter
/// slot-trace probe: one stderr line per blitter pipeline cycle with its
/// beam position, for cross-emulator slot-sequence comparison against the
/// vAmiga VAMIGA_BLT_PROBE hooks. Logging only; never alters timing.
pub(crate) fn diag_blt_slots() -> bool {
    use std::sync::OnceLock;
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| crate::envcfg::flag("COPPERLINE_DIAG_BLT_SLOTS"))
}

/// One-shot latch for the COPPERLINE_DIAG_COPLEN coplist dump (it used to clear its
/// own env var to log once; with cached env that no longer works).
static COPLEN_LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(feature = "internal-diagnostics")]
fn no_disk_stall() -> bool {
    use std::sync::OnceLock;
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| crate::envcfg::flag("COPPERLINE_NO_DISK_STALL"))
}

#[cfg(not(feature = "internal-diagnostics"))]
fn no_disk_stall() -> bool {
    false
}

fn external_access_cck_x100_setting() -> u32 {
    #[cfg(feature = "internal-diagnostics")]
    {
        crate::envcfg::var("COPPERLINE_DBG_EXTCCK")
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(200)
    }
    #[cfg(not(feature = "internal-diagnostics"))]
    {
        200
    }
}

/// Paula INTREQ/INTENA -> CPU IPL-pin propagation delay in color clocks
/// (DEFAULT ON). A change to the enabled-pending interrupt set does not reach
/// the 68000's IPL pins combinationally: Paula pipelines the encoded level to
/// the pins over a few chip clocks (vAmiga models the same pipe as iplPipe,
/// with the pin taking the new value ~4 DMA cycles after the level change).
/// Together with the IPL sampling model in cpu.rs (the interrupt decision at
/// an instruction boundary uses the level sampled at the PREVIOUS instruction's
/// last bus access, as on the real 68000), this reproduces the raise-to-entry
/// latency measured against vAmiga and the vAmigaTS real-hardware photos.
///
/// History: this was 65 for a while, calibrated against timing-test row 19
/// with a mis-decoded VHPOSR (the low byte is the cck position, not cck/2);
/// that made every IRQ delivery ~50 cck late and dominated the vAmigaTS
/// cputim/irqtim/inttim divergence. Set COPPERLINE_IRQ_LATENCY_CCK to override
/// (0 disables the pipe AND the boundary-sampling delay = raw model).
///
/// 5 = the ~4-cycle pin pipe plus the one-cck register-change commit a poked
/// INTREQ takes before the level encoder sees it (folded into one constant;
/// an IRQ-latency probe against vAmiga across seven source/loop geometries
/// lands within 0..+7 cck, the residual being per-instruction IPL poll-point
/// detail the m68k core does not model).
const DEFAULT_IRQ_LATENCY_CCK: u32 = 5;

/// CPU clocks a 020+ chip read spends returning data past the data-return
/// colour clock, before the execution unit can use it.
///
/// Derived, not fitted, from `timing-test/rdprobe.asm` on a real A1200: a
/// chip-read loop measures 16 CPU clocks per iteration with a 6-clock loop
/// branch and 20 with a 7-clock one. Since the loop period is a whole number
/// of colour clocks (4 CPU clocks each), those force the un-rounded cost to be
/// exactly 16, so a read occupies 10 clocks from the colour-clock boundary it
/// starts on: two colour clocks of grant and data return, then these two.
const CPU_020_CHIP_READ_RETURN_CLOCKS: u32 = 2;

/// Read the COPPERLINE_IRQ_LATENCY_CCK setting once, at bus construction (stored in
/// `irq_latency_setting`). Unset uses DEFAULT_IRQ_LATENCY_CCK; 0 disables.
fn irq_latency_setting_from_env() -> u32 {
    crate::envcfg::var("COPPERLINE_IRQ_LATENCY_CCK")
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(DEFAULT_IRQ_LATENCY_CCK)
}

/// Interrupt source bits (0..13); excludes INT_MASTER (bit 14) and the
/// set/clear bit (15). Used to detect a newly-raised interrupt.
const IRQ_SOURCE_BITS: u16 = 0x3FFF;

/// Cached COPPERLINE_DIAG_VBI gate (read once): logs the beam position when the
/// VERTB request is asserted. Checked per device tick, so it must not do a live
/// env lookup on that path.
fn diag_vbi() -> bool {
    use std::sync::OnceLock;
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| crate::envcfg::flag("COPPERLINE_DIAG_VBI"))
}

#[derive(Clone, Copy)]
struct CaprowDiag {
    first_vpos: u32,
    last_vpos: u32,
}

impl CaprowDiag {
    fn contains(self, vpos: u32) -> bool {
        (self.first_vpos..=self.last_vpos).contains(&vpos)
    }
}

/// Cached COPPERLINE_DIAG_CAPROW setting (read once). Accepted forms:
/// presence/`all` logs every captured row, `V` logs one beam line, and
/// `START:END` logs an inclusive beam-line range. Checked on every bitplane
/// DMA-word capture call (per beam advance), so it must not do a map lookup.
fn diag_caprow() -> Option<CaprowDiag> {
    use std::sync::OnceLock;
    static V: OnceLock<Option<CaprowDiag>> = OnceLock::new();
    *V.get_or_init(|| {
        let raw = crate::envcfg::var("COPPERLINE_DIAG_CAPROW")?;
        let raw = raw.trim();
        if raw.is_empty() || raw == "1" || raw.eq_ignore_ascii_case("all") {
            return Some(CaprowDiag {
                first_vpos: 0,
                last_vpos: u32::MAX,
            });
        }
        if let Some((first, last)) = raw.split_once(':') {
            let first_vpos = crate::envcfg::parse_u32(first).unwrap_or(0);
            let last_vpos = crate::envcfg::parse_u32(last).unwrap_or(u32::MAX);
            return Some(CaprowDiag {
                first_vpos: first_vpos.min(last_vpos),
                last_vpos: first_vpos.max(last_vpos),
            });
        }
        crate::envcfg::parse_u32(raw).map(|vpos| CaprowDiag {
            first_vpos: vpos,
            last_vpos: vpos,
        })
    })
}

/// Cached COPPERLINE_DIAG_SPRCAP setting (read once). Checked per captured
/// sprite line on the beam-advance path, so it must not do a map lookup.
fn diag_sprcap() -> Option<&'static str> {
    use std::sync::OnceLock;
    static V: OnceLock<Option<String>> = OnceLock::new();
    V.get_or_init(|| crate::envcfg::var("COPPERLINE_DIAG_SPRCAP"))
        .as_deref()
}

fn diag_sprcap_matches(want: &str, beam_y: i32) -> bool {
    let want = want.trim();
    want == "all" || want.parse::<i32>().ok() == Some(beam_y)
}
const CPU_COPPER_BOTTOM_PALETTE_MIN_VPOS: u32 = 0xC0;
const DMACON_DSKEN: u16 = 1 << 4;
const DMACON_SPREN: u16 = 1 << 5;
const DMACON_BLTEN: u16 = 1 << 6;
const DMACON_BPLEN: u16 = 1 << 8;
const DMACON_BLTPRI: u16 = 1 << 10;
const BLTCON0_USE_A: u16 = 1 << 11;
const BLTCON0_USE_B: u16 = 1 << 10;
const BLTCON0_USE_C: u16 = 1 << 9;
const BLTCON0_USE_D: u16 = 1 << 8;
const BLTCON1_LINE: u16 = 1 << 0;
const BLTCON1_DESC: u16 = 1 << 1;
const BLTCON1_IFE: u16 = 1 << 3;
const BLTCON1_EFE: u16 = 1 << 4;
const BLTCON1_DOFF: u16 = 1 << 7;
// The Copper cannot take (or hold) a bus slot on the last-but-two color
// clock of the line: $E0 on short lines, $E1 on NTSC long lines (vAmiga
// busIsFree<COPPER>, keyed on the long-line flag). A fetch chain that hits
// the lockout resumes past the line wrap, which is what a line-end SKIP's
// deferred decision and the cycleE0 vAmigaTS case observe.
const COPPER_BUS_LOCKOUT_HPOS_SHORT_LINE: u32 = 0x00E0;
const COPPER_BUS_LOCKOUT_HPOS_LONG_LINE: u32 = 0x00E1;
const COPER_CPU_IRQ_DELAY_CCK: u32 = 2;
const RENDER_VISIBLE_START_VPOS: u32 = 0x2C;
const RENDER_MIN_OVERSCAN_START_VPOS: u32 = 0x1C;
const PAL_SPRITE_DMA_FIRST_ACTIVE_VPOS: u32 = 0x19;
const NTSC_SPRITE_DMA_FIRST_ACTIVE_VPOS: u32 = 0x14;
const RENDER_VISIBLE_LINES: usize = FB_HEIGHT;
const RENDER_FRAMEBUFFER_WIDTH: i32 = FB_WIDTH as i32;
// Capture-side twin of `bitplane::DIW_HSTART_FB0`; held deep left of the
// standard display start so the captured window matches vAmiga's 716-wide
// cutout and includes the deep-left overscan. Like the renderer twin, this
// puts the hardware-verified standard $81 window edge (and the sprite
// comparator positions, which share Denise's counter) at framebuffer x = 62.
const RENDER_DIW_HSTART_FB0: i32 = 0x62;
/// Denise's sprite serializer emits its first pixel one lo-res pixel after
/// the horizontal comparator matches the SPRxPOS/CTL position: a sprite
/// programmed at the same numeric position as a display-window edge starts
/// one lo-res pixel to its right (vAmiga models this inside `sprhppos`).
/// Measured with timing-test/ddfprobe-sprbar.adf against a 16-px ruler:
/// FS-UAE and vAmiga both place the bar's left edge 2 framebuffer pixels
/// left of the ruler line it straddles; without this delay Copperline
/// placed it 4. Applies identically to DMA-loaded and manually armed
/// sprites (both probes) and to the collision comparators, which sample
/// the same serializer.
pub(crate) const SPRITE_OUTPUT_DELAY_LORES: i32 = 1;
// Standard DIWSTRT $81 is the visible window edge. The first standard
// bitplane sample at DDFSTRT $38 is already one lowres native sample into the
// fetched word, so the fetch/output phase is referenced one color clock earlier.
//
// Capture-side twins of `bitplane::DIW_HSTART_FETCH_REFERENCE_*`. The hi-res
// fetch/display phase sits 3 colour clocks earlier than lo-res, so the
// reference differs by resolution (lo-res $81, hi-res $84). Moved +1 in
// lockstep with RENDER_DIW_HSTART_FB0 so captured bitmap positions stay at
// their hardware-calibrated framebuffer columns. See the bitplane constant
// docs for the vAmiga-verified rationale.
const RENDER_DIW_HSTART_FETCH_REFERENCE_LORES: i32 = 0x81;
const RENDER_DIW_HSTART_FETCH_REFERENCE_HIRES: i32 = 0x84;
// Capture-side twin of `bitplane::COPPER_WAIT_HPOS_FB0`; moved left by 8 colour
// clocks in lockstep with RENDER_DIW_HSTART_FB0.
const RENDER_COPPER_WAIT_HPOS_FB0: u32 = 0x28;
// Agnus DMA scheduling runs four color clocks ahead of Denise's pixel counter.
const DENISE_HPOS_LAG_CCK: u32 = 4;
// Denise applies a register write to its pixel pipeline about four colour
// clocks after the chip-bus slot that carried it, regardless of whether the
// Copper or the CPU drove the bus (see `record_render_write`).
const DENISE_WRITE_EFFECT_DELAY_CCK: u32 = 4;
// Agnus applies its two-cycle register class (DMACON, BPLxPT, BPLxMOD,
// SPRxPT; vAmiga `DMA_CYCLES(2)` in `recordRegisterChange`) two colour
// clocks after the chip-bus slot that carried the write.
const AGNUS_WRITE_EFFECT_DELAY_CCK: u32 = 2;
const BPLCON0_ECSENA: u16 = 1 << 0;
const BPLCON0_SHRES: u16 = 1 << 6;
const BPLCON3_BRDSPRT: u16 = 1 << 1;
const BPLCON3_BRDRBLNK: u16 = 1 << 5;
const BPLCON3_SPRES_MASK: u16 = 0x00C0;
const BPLCON3_SPRES_LORES: u16 = 0x0040;
const BPLCON3_SPRES_HIRES: u16 = 0x0080;
const BPLCON3_SPRES_SHRES: u16 = 0x00C0;
const CLXDAT_SPRITE_PLAYFIELD_MASK: u16 = 0x01FE;
const CLXDAT_SPRITE_SPRITE_MASK: u16 = 0x7E00;
const BITPLANE_DDF_HARD_START: u16 = 0x0018;
const BITPLANE_DDF_HARD_STOP: u16 = 0x00D8;
/// First DMA slot colour clock of each sprite channel (the POS/DATA word;
/// the CTL/DATB word follows two clocks later). Hardware slot chart (and
/// vAmiga's DAS table): sprite N fetches at $15+4N / $17+4N.
const SPRITE_DMA_SLOT1_HPOS: [u32; 8] = [0x15, 0x19, 0x1D, 0x21, 0x25, 0x29, 0x2D, 0x31];
const NANOS_PER_SECOND: u128 = 1_000_000_000;
const VIDEO_FETCH_TIMING_SAMPLE_RATE: u128 = 128;
const VIDEO_COLLISION_TIMING_SAMPLE_RATE: u128 = 16;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CapturedBitplaneRow {
    pub nplanes: usize,
    pub words_per_row: usize,
    pub planes: [Vec<u16>; 8],
    /// Colour clock of the row's first fetch-unit boundary when the DDF
    /// sequencer's run diverges from the register-derived window (missed
    /// stops draining to the hardware stop, late starts). None when the
    /// register-derived geometry already matches (and for wide-FMODE rows).
    pub fetch_origin_cck: Option<u16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CapturedSpriteLine {
    pub sprite: usize,
    pub hstart: i32,
    pub hsub_70ns: bool,
    pub beam_y: i32,
    /// First (or only) data/mask word pair; AGA wide fetches carry the
    /// remaining words in the `_ext` arrays.
    pub data: u16,
    pub datb: u16,
    pub data_ext: [u16; 3],
    pub datb_ext: [u16; 3],
    /// Words per channel per line: 1 (16 px), 2 (32 px), or 4 (64 px),
    /// from FMODE SPR32/SPAGEM.
    pub width_words: u8,
    pub attached: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeldSpriteLine {
    pub line: CapturedSpriteLine,
    pub vstart: i32,
    pub vstop: i32,
}

/// Agnus's per-channel sprite DMA state, modelled at the register level the
/// way the chip works: POS/CTL control words are fetched on the channel's
/// vstop line through its two DMA slots, the vertical comparators run off
/// these register copies at each line start (and immediately on CPU/Copper
/// pokes), the DMA flip-flop is cleared on the vstop line even when SPREN
/// is off, and SPRxPT advances only on words actually fetched.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
struct DisplaySpriteDmaState {
    /// Channel copies of the SPRxPOS/SPRxCTL register words (from DMA
    /// control-word fetches or CPU/Copper writes); the horizontal position
    /// and attach decode for emitted lines comes from these.
    pos: u16,
    ctl: u16,
    /// Vertical comparator values. Mostly derived from pos/ctl, but held
    /// separately because the vertical-blank reset writes vstop without
    /// touching the CTL word.
    vstrt: i32,
    vstop: i32,
    /// The per-sprite DMA flip-flop: set when the line counter passes
    /// vstrt, cleared when it passes vstop (even with SPREN off) and in
    /// the frame's last line.
    dma_enabled: bool,
    /// Line whose start-of-line comparator update has already run (the
    /// update is applied lazily, catching up over skipped lines).
    #[serde(default = "unset_sprite_line_marker")]
    comparator_vpos: i32,
    /// Display latches from the last assembled line: a skipped slot reuses
    /// the stale side, and with DMA off an armed channel keeps displaying
    /// this line until a CTL fetch or poke disarms it.
    last_line: Option<DisplaySpriteLineData>,
    /// Display line of the last DATA fetch: with SSCAN2 the hardware
    /// fetches on every other line and redisplays in between.
    last_data_fetch_vpos: i32,
    /// DATA word(s) fetched by the sprite's first DMA slot, sampled at that
    /// slot's beam time, awaiting the second slot to assemble the line.
    pending_data: Option<(u16, [u16; 3])>,
    /// Line the first slot's DATA fetch ran for.
    #[serde(default = "unset_sprite_line_marker")]
    pending_line_vpos: i32,
    /// Line the first slot's evaluation ran for, so the second slot
    /// completes the line rather than re-running the entry logic.
    #[serde(default = "unset_sprite_line_marker")]
    entry_line_vpos: i32,
}

fn unset_sprite_line_marker() -> i32 {
    i32::MIN
}

impl Default for DisplaySpriteDmaState {
    fn default() -> Self {
        // The line-marker fields must start unset, not at line 0: a derived
        // zero default would make a fresh state claim its per-line work
        // already ran when the beam is on line 0.
        Self {
            pos: 0,
            ctl: 0,
            vstrt: 0,
            vstop: 0,
            dma_enabled: false,
            comparator_vpos: unset_sprite_line_marker(),
            last_line: None,
            last_data_fetch_vpos: unset_sprite_line_marker(),
            pending_data: None,
            pending_line_vpos: unset_sprite_line_marker(),
            entry_line_vpos: unset_sprite_line_marker(),
        }
    }
}

impl DisplaySpriteDmaState {
    /// A POS word (fetch or poke) replaces the vertical start's low bits,
    /// keeping the CTL-supplied high bit.
    fn poke_pos(&mut self, value: u16) {
        self.pos = value;
        self.vstrt = (self.vstrt & 0x100) | i32::from(value >> 8);
    }

    /// A CTL word (fetch or poke) supplies vstart's high bit and the whole
    /// vstop, and disarms the channel's display latches.
    fn poke_ctl(&mut self, value: u16) {
        self.ctl = value;
        self.vstrt = (self.vstrt & 0x0FF) | (i32::from(value & 0x0004) << 6);
        self.vstop = (i32::from(value & 0x0002) << 7) | i32::from(value >> 8);
        self.last_line = None;
    }

    /// The comparators also fire immediately when a poke lands on the
    /// matching line (vstop wins over vstrt when both match).
    fn reevaluate_comparators_at(&mut self, beam_y: i32) {
        if self.vstrt == beam_y {
            self.dma_enabled = true;
        }
        if self.vstop == beam_y {
            self.dma_enabled = false;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SpriteControlRegisterWrite {
    Pos,
    Ctl,
}

/// Where the pre-display sprite-DMA replay has got to in the current frame.
/// `started` is false until the frame's first replay call has seeded the
/// channels from the carried SPRxPT frontier.
#[derive(Clone, Copy, Debug, Default, serde::Serialize, serde::Deserialize)]
struct SpriteDmaReplayCursor {
    started: bool,
    vpos: u32,
    /// Next of the line's sixteen sprite DMA slots (sprite N's slot 1 and 2
    /// interleaved in beam order) still to replay.
    slot_idx: usize,
    event_idx: usize,
    dmacon: u16,
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
struct BitplaneDmaconDelay {
    previous: u16,
    changed_at_cck: u64,
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
struct BitplaneBplcon0Delay {
    previous: u16,
    changed_at_cck: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct BitplaneDdfStartMiss {
    vpos: u32,
    ddfstart: u32,
}

/// Cache key for the memoized bitplane slot plan: every register input that
/// feeds the Agnus fetch-ownership computation. The vpos-dependent gates
/// (vertical display window, DDFSTRT write miss) are evaluated live in
/// `bitplane_slot_active_at`, so DIW registers do not belong here. Likewise,
/// only BPLCON0's plane-count and resolution bits belong here: HAM, dual
/// playfield, lace, color, and genlock affect Denise interpretation of fetched
/// words, but they do not change which chip-bus slots Agnus reserves.
#[derive(Clone, Copy, PartialEq, Eq)]
struct BitplaneSlotKey {
    bplen: bool,
    bplcon0: u16,
    ddfstrt: u16,
    ddfstop: u16,
    fmode: u16,
    harddis: bool,
}

const BPLCON0_SLOT_PLAN_MASK_OCS_ECS: u16 = 0xF040;
const BPLCON0_SLOT_PLAN_MASK_AGA: u16 = BPLCON0_SLOT_PLAN_MASK_OCS_ECS | 0x0010;

fn bitplane_slot_plan_bplcon0_key(bplcon0: u16, aga: bool) -> u16 {
    let mask = if aga {
        BPLCON0_SLOT_PLAN_MASK_AGA
    } else {
        BPLCON0_SLOT_PLAN_MASK_OCS_ECS
    };
    bplcon0 & mask
}

/// Derived bitplane fetch cadence for the current `BitplaneSlotKey`: the
/// line-invariant parts of `bitplane_slot_active_at`.
#[derive(Clone, Copy)]
struct BitplaneSlotPlan {
    start: u32,
    last_fetch_hpos: u32,
    period: u32,
    unit: u32,
    quantum: u32,
    words_per_row: u32,
    hires_like: bool,
    /// Bit n set when a DMA-enabled plane fetches at within-unit order n
    /// (lores cadence only).
    order_mask: u8,
    /// Precomputed per-hpos slot pattern: bit `h` is set when `plan_slot_at`
    /// is true for `hpos == h`. The pattern is purely a function of the fields
    /// above (vpos-independent), so it is memoized once with the plan and the
    /// per-color-clock arbiter does a bit test instead of the div/mod math.
    /// Covers hpos 0..256, which spans every standard/long line; the rare
    /// programmable line with a slot at hpos >= 256 falls back to `plan_slot_at`.
    slot_mask: [u64; 4],
}

/// Width covered by `BitplaneSlotPlan::slot_mask` (hpos 0..SLOT_MASK_BITS).
const SLOT_MASK_BITS: u32 = 256;
const BITPLANE_SLOT_PLAN_CACHE_LEN: usize = 8;

type BitplaneSlotPlanCacheEntry = Option<(BitplaneSlotKey, Option<BitplaneSlotPlan>)>;

struct BitplaneSlotPlanCache {
    entries: [std::cell::Cell<BitplaneSlotPlanCacheEntry>; BITPLANE_SLOT_PLAN_CACHE_LEN],
    next_insert: std::cell::Cell<usize>,
    last_hit: std::cell::Cell<usize>,
}

impl BitplaneSlotPlanCache {
    fn new() -> Self {
        Self {
            entries: std::array::from_fn(|_| std::cell::Cell::new(None)),
            next_insert: std::cell::Cell::new(0),
            last_hit: std::cell::Cell::new(0),
        }
    }

    fn lookup(&self, key: BitplaneSlotKey) -> Option<Option<BitplaneSlotPlan>> {
        let last = self.last_hit.get().min(BITPLANE_SLOT_PLAN_CACHE_LEN - 1);
        if let Some((cached_key, plan)) = self.entries[last].get() {
            if cached_key == key {
                return Some(plan);
            }
        }

        for idx in 0..BITPLANE_SLOT_PLAN_CACHE_LEN {
            if idx == last {
                continue;
            }
            if let Some((cached_key, plan)) = self.entries[idx].get() {
                if cached_key == key {
                    self.last_hit.set(idx);
                    return Some(plan);
                }
            }
        }
        None
    }

    fn insert(&self, key: BitplaneSlotKey, plan: Option<BitplaneSlotPlan>) {
        let idx = self.next_insert.get() % BITPLANE_SLOT_PLAN_CACHE_LEN;
        self.entries[idx].set(Some((key, plan)));
        self.last_hit.set(idx);
        self.next_insert
            .set((idx + 1) % BITPLANE_SLOT_PLAN_CACHE_LEN);
    }

    #[cfg(test)]
    fn entries_snapshot(&self) -> [BitplaneSlotPlanCacheEntry; BITPLANE_SLOT_PLAN_CACHE_LEN] {
        std::array::from_fn(|idx| self.entries[idx].get())
    }

    #[cfg(test)]
    fn last_hit_entry(&self) -> BitplaneSlotPlanCacheEntry {
        self.entries[self.last_hit.get()].get()
    }
}

impl Default for BitplaneSlotPlanCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Read-mostly wide-FMODE bitplane ownership for one scanline. Lines without
/// a fetch-affecting register write reuse the plan's complete slot mask
/// directly; dynamic lines fall back to the block-delay-aware calculation.
struct WideBitplaneHotLine {
    valid: std::cell::Cell<bool>,
    vpos: std::cell::Cell<u32>,
    slot_mask: [std::cell::Cell<u64>; 4],
    plan: std::cell::Cell<Option<BitplaneSlotPlan>>,
}

impl WideBitplaneHotLine {
    fn new() -> Self {
        Self {
            valid: std::cell::Cell::new(false),
            vpos: std::cell::Cell::new(0),
            slot_mask: std::array::from_fn(|_| std::cell::Cell::new(0)),
            plan: std::cell::Cell::new(None),
        }
    }

    fn is_current(&self, vpos: u32) -> bool {
        self.valid.get() && self.vpos.get() == vpos
    }

    fn publish(&self, vpos: u32, plan: Option<BitplaneSlotPlan>) {
        self.valid.set(false);
        let slot_mask = plan.map_or([0; 4], |plan| plan.slot_mask);
        for (destination, source) in self.slot_mask.iter().zip(slot_mask) {
            destination.set(source);
        }
        self.plan.set(plan);
        self.vpos.set(vpos);
        self.valid.set(true);
    }

    fn invalidate(&self) {
        self.valid.set(false);
    }
}

impl Default for WideBitplaneHotLine {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
struct DisplaySpriteLineData {
    hstart: i32,
    hsub_70ns: bool,
    data: u16,
    datb: u16,
    data_ext: [u16; 3],
    datb_ext: [u16; 3],
    width_words: u8,
    attached: bool,
}

fn empty_captured_bitplane_rows() -> Vec<Option<CapturedBitplaneRow>> {
    (0..MAX_VISIBLE_LINES).map(|_| None).collect()
}

fn empty_sprite_collision_sources() -> Vec<Option<Vec<LiveSpriteCollisionSource>>> {
    (0..MAX_VISIBLE_LINES).map(|_| None).collect()
}

fn empty_captured_sprite_lines_by_y() -> Vec<Vec<CapturedSpriteLine>> {
    (0..MAX_VISIBLE_LINES).map(|_| Vec::new()).collect()
}

fn clear_captured_sprite_lines_by_y(lines_by_y: &mut Vec<Vec<CapturedSpriteLine>>) {
    if lines_by_y.len() != MAX_VISIBLE_LINES {
        *lines_by_y = empty_captured_sprite_lines_by_y();
        return;
    }
    for lines in lines_by_y {
        lines.clear();
    }
}

fn empty_sprite_display_enable_x_by_y() -> [Option<usize>; MAX_VISIBLE_LINES] {
    [None; MAX_VISIBLE_LINES]
}

/// Runtime latches omitted by file save states but needed for exact, immediate
/// rollback. This is an in-process snapshot prefix, not part of STATE_VERSION.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct RollbackState {
    data_bus: u16,
    render_blocked: bool,
    frame_lines: [u32; 2],
    h_windows: [Option<(i32, u32)>; 2],
    v_windows: [Option<(i32, u32)>; 2],
}

// Save-state note: Bus and everything it owns derive serde so a snapshot can
// be taken at an emulated-frame boundary (src/savestate.rs). Host-resource
// fields (open files, audio/serial sinks, wall-clock anchors, memo caches)
// are #[serde(skip)] and reattached by the savestate loader; everything else
// is emulated state and must round-trip. New fields are picked up by the
// derive automatically. In a state file each field lands in the chunk of
// its subsystem (src/savestate/chunk.rs: `paula` in PAUL, `agnus` in AGNS,
// anything unclaimed in BUS), and the payloads name their fields, so a new
// field needs only `#[serde(default)]` (or to be an Option) for older
// states to keep loading; bump the owning chunk's version, with a
// migration, only when a change has no correct default.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Bus {
    pub mem: Memory,
    /// Cold-power-on RAM policy. This is machine state rather than a host-side
    /// presentation preference: a save-state restored and then power-cycled
    /// must use the same deterministic pattern as the machine that saved it.
    ram_init: RamInit,
    pub cia_a: Cia,
    pub cia_b: Cia,
    pub paula: Paula,
    pub agnus: Agnus,
    pub copper: Copper,
    pub denise: Denise,
    denise_revision: DeniseRevision,
    /// Effective chip-bus DMA address mask: installed chip RAM capped by the
    /// Agnus revision's address-bus reach. Single owner; refreshed by
    /// configure_chip_dma_masks().
    chip_dma_mask: u32,
    pub blitter: Blitter,
    pub floppy: FloppyController,
    /// Host-side Centronics peripheral. The CIA-A PC/data and FLAG signals are
    /// emulated, but the attached peripheral itself is not machine state and
    /// therefore survives save-state loads like Paula's audio/serial sinks.
    #[serde(skip, default = "crate::parallel::null_parallel_port")]
    parallel_port: Box<dyn crate::parallel::ParallelPort>,
    pub rtc: Rtc,
    /// Whether the $DC0000 RTC is fitted (machine-profile flag; the base
    /// A600 shipped without one). The CPU memory map consults this before
    /// decoding the RTC range.
    rtc_present: bool,
    /// Gayle gate array (A600/A1200 machine profiles); None on machines
    /// without one, which leaves $DA0000/$DE1000 floating as before.
    pub gayle: Option<Gayle>,
    /// Ramsey memory controller (A3000/A4000 machine profiles). Answers on
    /// the same $DE0000 page Gayle uses, so the two are never both fitted.
    #[serde(default)]
    pub ramsey: Option<crate::ramsey::Ramsey>,
    /// Fat Gary bus controller (A3000/A4000 machine profiles): the other three
    /// byte lanes of the page Ramsey answers on. Fitted with it, never alone.
    #[serde(default)]
    pub gary: Option<crate::gary::Gary>,
    /// Super DMAC (A3000 machine profile): the SCSI DMA controller at $DD0000.
    /// Kickstart's scsi.device hangs during init if nothing answers here.
    #[serde(default)]
    pub sdmac: Option<crate::sdmac::Sdmac>,
    /// A4000 motherboard IDE (A4000 machine profile): the ATA task file at
    /// $DD2020.
    #[serde(default)]
    pub ide_a4000: Option<crate::ide_a4000::IdeA4000>,
    /// WinUAE-compatible uaelib trap at $F0FF60 (`[emulation] uaelib`): a
    /// guest-callable warp toggle, debug log and resource registry
    /// (`crate::uaelib`). None when disabled; a CDTV extended ROM at
    /// $F00000 decodes ahead of it and hides it.
    #[serde(default)]
    pub uaelib: Option<crate::uaelib::UaeLib>,
    /// Freezer cartridge (`[cartridge] model`, `crate::cartridge`): the
    /// monitor's 1 MiB bank at $A10000, the custom/CIA register shadows
    /// kept for it, and the pending level-7 freeze interrupt. None when
    /// nothing is fitted.
    #[serde(default)]
    pub cartridge: Option<crate::cartridge::Cartridge>,
    /// `[debug] log_unmapped`: log CPU accesses in this range that no device
    /// decodes, to find the registers a guest expects and we do not provide.
    #[serde(default)]
    pub log_unmapped: Option<std::ops::RangeInclusive<u32>>,
    /// Akiko gate array (CD32 machine profile): ID, the C2P port, and
    /// NVRAM/CD stubs at $B80000.
    pub akiko: Option<crate::akiko::Akiko>,
    /// CDTV DMAC/CD controller (CDTV machine profile): an autoconfig
    /// board with the 6525 TPI and Matshita drive.
    pub cdtv: Option<crate::cdtv::CdtvController>,
    /// Functional Zorro-chain boards (the A2091 SCSI controller, WASM plugin
    /// boards). The chain maps each board's window to a
    /// [`crate::zorro::BoardBacking::Device`] slot index into this vector, and
    /// accesses route here through the [`crate::zorro_device::ZorroDevice`]
    /// boundary. Serialized inline with the bus.
    pub devices: Vec<crate::zorro_device::BoardDevice>,
    /// Emulated-time deadline keeping the front-panel HDD LED lit after the
    /// most recent Gayle IDE activity, so short synchronous transfers stay
    /// visible for a human-perceptible stretch.
    hdd_led_until_cck: u64,
    /// One-time diagnostic when software writes BPLCON3 and the write is
    /// dropped (OCS Denise, or ECS with ENBPLCN3 clear).
    bplcon3_drop_warned: bool,
    /// AGA 32-bit fetch latch: the aligned chip longwords the CPU most
    /// recently fetched opcode words from (two entries, so a tight loop
    /// that straddles a longword boundary still hits). Fetch words from a
    /// latched longword cost no bus slot; chip writes to a latched
    /// longword invalidate it (self-modifying code refetches).
    cpu_fetch_latch: [Option<u32>; 2],

    /// Set by a CIA-A PRA write when /OVL transitions to 1. The
    /// emulator loop notices this between instruction slices, performs
    /// the actual mem_unmap + mem_map_ptr, and clears the flag.
    pub overlay_disable_pending: bool,

    /// The keyboard MCU (6500/1) model. Key transitions from the winit
    /// handler and scripted key events queue here, then clock into
    /// CIA-A bit by bit over the emulated KCLK/KDAT lines.
    pub keyboard: KeyboardMcu,

    /// Set when the keyboard MCU completed its 500 ms KCLK reset hold
    /// (Ctrl+Amiga+Amiga). The emulator loop performs the actual
    /// machine reset between instruction slices and clears the flag,
    /// like `overlay_disable_pending`.
    pub keyboard_system_reset_pending: bool,

    /// Live host input state (mouse counters, buttons, etc.). Mapped
    /// to CIA-A PRA, POTINP, and JOYxDAT on every read of those
    /// registers so Amiga-side software sees up-to-date values.
    pub input: InputState,

    pub poll_stats: PollStats,
    /// Host performance telemetry (call/probe/nanosecond counters), not
    /// machine state: call chunking legitimately differs across a
    /// save-state load, so it is excluded from the serialized layout.
    #[serde(skip)]
    video_pipeline_stats: VideoPipelineStats,

    /// Set by MMIO writes that should end the current CPU slice after
    /// the current instruction has retired.
    pub slice_preempted: bool,

    /// Diagnostic crash context: set when a blit is started whose D
    /// destination lands in the exception-vector / low-memory region, so
    /// the CPU wrapper can dump its instruction history at that moment.
    /// Host diagnostics, not machine state: only `COPPERLINE_DIAG_CRASH`
    /// ever consumes it, so on an undiagnosed run it stays latched from the
    /// first such blit on. Excluded from the serialized layout so a save
    /// state written after that blit is byte-identical whether the run was
    /// resumed (which launders the alarm) or not.
    #[serde(skip)]
    pub diag_lowmem_blit: bool,

    /// Latched agnus-frame-crossing that has not yet been ORed into
    /// Paula INTREQ. INTREQ is a bit latch, so multiple queued VBlanks
    /// collapse into one pending VERTB bit.
    pub pending_vbi: u32,
    pending_copper_frame_start: Option<u32>,
    /// Whether the Copper has been active at any point in the current field:
    /// true from the vertical-blank COP1LC strobe if Copper DMA was enabled
    /// there, or from any COPEN 0->1 write since. While false (a dormant
    /// Copper), a COPxLC write for the Copper's current list retargets the
    /// Copper PC directly instead of only loading the location latch --
    /// hardware behaviour the vAmigaTS Agnus/Copper/lc family photographs
    /// (vAmiga models it as activeInThisFrame in pokeCOPxLC).
    copper_active_in_frame: bool,
    /// Which location register the Copper PC was last strobed from (1 or 2):
    /// the vertical-blank restart selects COP1LC, COPJMPx selects its list.
    /// The dormant-retarget rule above only applies to the matching list.
    copper_current_list: u8,

    /// Paula interrupt bits that were pending when the current
    /// autovector was delivered. CPU INTREQ clears use this to tell
    /// ordinary palette setup apart from beam-timed Copper interrupt
    /// palette changes.
    pub delivered_irq_pending: u16,
    pub(crate) pending_copper_irq_beam: Option<(u32, u32)>,
    pub(crate) delivered_copper_irq_beam: Option<(u32, u32)>,
    coper_cpu_irq_delay_cck: u32,

    /// Paula INTREQ -> CPU IPL-pin propagation pipe (COPPERLINE_IRQ_LATENCY_CCK,
    /// see DEFAULT_IRQ_LATENCY_CCK). When the setting is non-zero, a newly
    /// raised maskable interrupt is held invisible to the CPU for that many cck
    /// (`irq_latency_mask` = the delayed bits, `irq_latency_visible_at` = the
    /// colour clock each delayed bit reaches the pins at,
    /// `irq_latency_last_pending` = previous pending set for rising-edge detect).
    /// INTREQR reads are NOT delayed (the pipe sits between the level encoder
    /// and the pins, not on the register).
    ///
    /// Each source pipes independently: every source has its own path through
    /// the level encoder, so one asserting cannot hold another that is already
    /// on its way to the pins. A single shared countdown restarted by every new
    /// source stretched an earlier interrupt's delay by the later one's, which
    /// on a machine with several active sources (audio channels, the Copper,
    /// vertical blank) jittered handler entry across tens of raster lines
    /// instead of the few colour clocks the pipe is worth.
    irq_latency_visible_at: [u64; 16],
    irq_latency_mask: u16,
    irq_latency_last_pending: u16,
    /// Configured IPL-pipe length in cck (from COPPERLINE_IRQ_LATENCY_CCK or the
    /// default); 0 disables the pipe and the cpu.rs boundary-sampling delay. A
    /// field (not a global) so tests can set it per-instance -- mechanism tests
    /// run with 0 to deliver IRQs immediately.
    pub(crate) irq_latency_setting: u32,

    /// Palette snapshots written by CPU interrupt handlers. The top
    /// snapshot captures the display-start palette; the bottom
    /// snapshot tracks beam-timed Copper interrupt palettes used for
    /// buffer reuse decisions.
    pub beam_top_palette: Palette,
    pub beam_bottom_palette: Palette,
    pub beam_bottom_palette_valid: bool,
    cpu_palette_target: CpuPaletteTarget,
    cpu_palette_target_writes: u8,
    cpu_palette_target_beam: Option<(u32, u32)>,
    current_frame_render_base: RenderRegisterSnapshot,
    last_frame_render_base: Option<RenderRegisterSnapshot>,
    current_frame_render_events: Vec<BeamRegisterWrite>,
    current_frame_collision_events: Vec<BeamRegisterWrite>,
    current_frame_collision_control_events: Vec<BeamRegisterWrite>,
    current_frame_collision_bpldat_events: Vec<BeamRegisterWrite>,
    current_frame_collision_sprite_events: Vec<BeamRegisterWrite>,
    current_frame_collision_control_index: Option<BeamEventIndex>,
    current_frame_collision_bpldat_index: Option<BeamEventIndex>,
    current_frame_collision_sprite_index: Option<BeamEventIndex>,
    current_frame_collision_may_have_dual_playfield: bool,
    last_frame_render_events: Vec<BeamRegisterWrite>,
    beam_bottom_palette_events: Vec<BeamRegisterWrite>,
    pending_beam_bottom_palette_events: Vec<BeamRegisterWrite>,
    last_frame_beam_bottom_palette_events: Vec<BeamRegisterWrite>,
    current_frame_beam_top_palette: Palette,
    last_frame_beam_top_palette: Palette,
    last_frame_beam_top_palette_end: Palette,
    last_frame_beam_bottom_palette: Palette,
    last_frame_beam_bottom_palette_valid: bool,
    current_frame_chip_ram: Vec<u8>,
    last_frame_chip_ram: std::sync::Arc<Vec<u8>>,
    current_frame_chip_ram_writes: Vec<BeamChipRamWrite>,
    last_frame_chip_ram_writes: Vec<BeamChipRamWrite>,
    current_frame_bitplane_rows: Vec<Option<CapturedBitplaneRow>>,
    last_frame_bitplane_rows: std::sync::Arc<Vec<Option<CapturedBitplaneRow>>>,
    /// Released completed-frame rows retained for their eight plane
    /// allocations. Transient: captured contents are never reused, only the
    /// backing capacities.
    #[serde(skip)]
    bitplane_row_pool: Vec<CapturedBitplaneRow>,
    current_frame_sprite_lines: Vec<CapturedSpriteLine>,
    current_frame_sprite_lines_by_y: Vec<Vec<CapturedSpriteLine>>,
    current_frame_sprite_collision_sources: Vec<Option<Vec<LiveSpriteCollisionSource>>>,
    last_frame_sprite_lines: Vec<CapturedSpriteLine>,
    // Sprites whose data was DMA-fetched (off-screen) and then held with SPREN
    // cleared, to be repainted by Copper SPRxPOS repositioning during the
    // visible window. The renderer's manual-sprite path consumes these so it
    // can clip each repositioned segment to the reposition interval (a
    // CapturedSpriteLine cannot be clipped); the bus bar path is suppressed for
    // them. Carries the held pixel data and DMA-established control/window;
    // later SPRxPOS/CTL writes can still reposition the held line.
    #[serde(skip)]
    current_frame_held_sprites: [Option<HeldSpriteLine>; 8],
    #[serde(skip)]
    last_frame_held_sprites: [Option<HeldSpriteLine>; 8],
    #[serde(with = "serde_big_array::BigArray")]
    current_frame_sprite_display_enable_x_by_y: [Option<usize>; MAX_VISIBLE_LINES],
    #[serde(with = "serde_big_array::BigArray")]
    last_frame_sprite_display_enable_x_by_y: [Option<usize>; MAX_VISIBLE_LINES],
    current_frame_sprite_dma_observed: bool,
    last_frame_sprite_dma_observed: bool,
    current_frame_display_snapshot_taken: bool,
    #[serde(skip)]
    ocs_same_line_diw_start_blocked_vpos: Option<u32>,
    /// Agnus vertical display window flop: SET when the beam line matches
    /// DIWSTRT.V, RESET when it matches DIWSTOP.V (reset wins on a tie).
    /// The comparators are equality matches against the live registers, so
    /// rewriting DIWSTRT/DIWSTOP/DIWHIGH mid-frame arms them for a LATER
    /// line but never re-opens a window the old DIWSTOP already closed
    /// this frame - a level `vstart <= v < vstop` test would. Kang Fu
    /// (CD32) closes its main screen at line 284 and rewrites the whole
    /// window plus bitplane pointers during that line for a status bar at
    /// 285-300; the level test resumed fetching in the tail of line 284
    /// and raced the pointer writes, shearing the bar. Re-evaluated at
    /// every line start (the video standard's fixed line 312 PAL / 262
    /// NTSC forces a reset, so a window whose DIWSTOP the beam never
    /// reaches stays open only to the end of its own frame; the frame
    /// wrap's line zero then runs its own comparators) and after DIW
    /// writes. `None` means no evaluation has happened on this timeline
    /// (unit fixtures that teleport the beam and poke registers), which
    /// falls back to the level test. Serialized: the value is
    /// history-dependent and snapshots can be taken mid-frame (a control
    /// session's `state.save` at a breakpoint), where no register-derived
    /// reconstruction is exact.
    diw_vertical_open: Option<bool>,
    #[serde(skip)]
    current_frame_render_blocked: bool,
    current_frame_visible_start_vpos: u32,
    last_frame_visible_start_vpos: u32,
    /// Display geometry latched at the frame wrap (standard fixed canvas
    /// vs ECS/AGA VARBEAMEN programmable scan); see `FrameGeometry`.
    current_frame_geometry: FrameGeometry,
    last_frame_geometry: FrameGeometry,
    /// Sync-anchored presentation window latched at the frame wrap with the
    /// geometry, so the presented frame never mixes its latched geometry
    /// with sync registers rewritten after the wrap. A pure presentation
    /// cache: never serialized (restored None and recomputed after a state
    /// load, like `FrameGeometry::frame_lines`).
    #[serde(skip)]
    current_frame_presentation_h_window: Option<(i32, u32)>,
    #[serde(skip)]
    last_frame_presentation_h_window: Option<(i32, u32)>,
    #[serde(skip)]
    current_frame_presentation_v_window: Option<(i32, u32)>,
    #[serde(skip)]
    last_frame_presentation_v_window: Option<(i32, u32)>,
    lazy_collision_vpos: u32,
    lazy_collision_hpos: u32,
    /// Sticky gate for the per-frame collision accumulation. Collision results
    /// are observable only through a CLXDAT read ($DFF00E); software that never
    /// reads CLXDAT cannot tell whether the latch was maintained. The full-frame
    /// collision scan in `begin_new_beam_frame` (`accumulate_live_collisions_to_
    /// frame_end`) is ~37% of emulation time on a busy demo, so skip it entirely
    /// until the first CLXDAT read. From that read onward this stays true and the
    /// per-frame flush runs exactly as before, so any software that actually uses
    /// collisions is bit-identical. Not serialized: a restored state restarts in
    /// the skipping state and re-arms on the next CLXDAT read (the latch carries
    /// the current frame's collisions, which is all real collision code relies
    /// on; cross-frame historical-OR of an unread latch is not meaningful).
    #[serde(skip)]
    collision_tracking_active: bool,
    cpu_bus_arbitration_enabled: bool,
    cpu_granted_chip_slots: u64,
    cpu_missed_chip_slots: u64,
    /// Monotonic colour clocks spent waiting for a chip-bus grant. Unlike
    /// `cpu_missed_chip_slots`, this is never reset by the optional
    /// per-frame diagnostic logger, so an instruction profiler can take
    /// exact before/after deltas without depending on host diagnostics.
    /// Observer-only state: it is deliberately absent from save states.
    #[serde(skip)]
    cpu_wait_cck_total: u64,
    /// Sub-cck remainder from CPU-internal clock reporting (the core reports
    /// CPU clocks; the bus advances in `cpu_clocks_per_cck`-clock color
    /// clocks).
    cpu_clock_carry: u32,
    /// CPU clocks per color clock: 2 for a stock 7.09 MHz 68000, more for an
    /// accelerated CPU ([cpu] clock_mhz). Scales external-access and
    /// CPU-internal billing; the chip bus itself stays at one slot per cck.
    cpu_clocks_per_cck: u32,
    /// Sub-cck remainder from external (fast RAM/ROM) access billing, in
    /// hundredths of a CPU clock. At high clock ratios a single word access
    /// costs less than one cck; the carry accumulates so no time is lost.
    ext_clock_carry_x100: u32,
    /// True for the 68020+: its local bus cycle is 3 CPU clocks, not the
    /// 68000's 4 (2 cck). Writes can finish after that shorter, posted cycle;
    /// reads also wait for the chipset's data-return phase. Derived from the
    /// CPU model and re-set on construction / state load; not serialized.
    #[serde(skip)]
    cpu_short_bus_cycle: bool,
    /// Whether the current CPU slice charges per instruction, so the chip
    /// clock phase below is fresh at every access. The JIT slice charges one
    /// lump per batch, which would leave the phase stale for every access
    /// after the first; it opts out and keeps the older accounting.
    #[serde(skip)]
    cpu_access_phase_sync: bool,
    /// Posted CPU chip-bus writes not yet retired to a slot (0 or 1). The
    /// 020+ bus unit runs decoupled from the execution unit: a chip write is
    /// accepted at the end of its 3-clock cycle and retires into a later free
    /// chip slot while execution continues (the drain in
    /// `advance_one_chip_bus_quantum`). Only one bus cycle can be in flight,
    /// so a second chip access stalls until the pending write retires. A real
    /// A1200 runs a chip write+dbra loop at the 2-cck port cadence (~8 CPU
    /// clocks per iteration -- `timing-test` rows 3, 10, 12, 18), which a
    /// synchronous whole-slot write bill cannot express.
    cpu_posted_write_debt: u32,
    /// `emulated_cck` time when the chip bus' CPU port is free again after a
    /// granted CPU slot: the port turns around in 2 colour clocks, which is
    /// what paces back-to-back CPU chip accesses (and posted-write drains) to
    /// every other colour clock.
    cpu_chip_port_free_at: u64,
    /// Where the 020+ CPU sits inside the current colour clock, in CPU clocks.
    ///
    /// The CPU and the chip bus run one timeline: the CPU's position is
    /// `emulated_cck * cpu_clocks_per_cck + cpu_chip_clock_phase`. Execution
    /// clocks accumulate here and turn into beam time a whole colour clock at
    /// a time; a chip read, which cannot begin part way through a colour
    /// clock, stalls out the remainder and resets it to zero. That
    /// synchronisation is what makes a real 68EC020 run a loop containing a
    /// chip access a whole number of colour clocks per iteration
    /// (`timing-test/rdprobe.asm`), and what keeps the same loop costing the
    /// same wherever in the frame it starts.
    cpu_chip_clock_phase: u32,
    /// CPU clocks of posted-write transfer time that overlapped execution on
    /// the decoupled 020+ bus unit during the current instruction; the
    /// CPU-side charge subtracts them (`take_cpu_bus_overlap_clocks`).
    cpu_bus_overlap_clocks: u32,
    /// Chip-bus slot of the most recently granted CPU access to custom
    /// register space. A CPU register write is applied to chip state once
    /// its whole bus cycle has been billed (the beam sits past the granted
    /// slot by then), but its Denise/Agnus-effective position is referenced
    /// from the slot that carried the write: `record_render_write` places
    /// CPU-sourced render events at slot + `DENISE_WRITE_EFFECT_DELAY_CCK`,
    /// the same write-to-effect pipeline the Copper's slot-exact writes
    /// take. Also feeds the `COPPERLINE_DIAG_CPU_WRITES` landing trace.
    /// Transient bus-cycle state, rebuilt every access; not serialized.
    #[serde(skip)]
    cpu_custom_access_slot: Option<(u32, u32)>,
    /// Beam position where the CPU requested the most recent custom-register
    /// access, before waiting for fixed DMA, the Copper, or the blitter to
    /// release a chip-bus slot. This feeds the CPU read/write landing traces.
    /// Transient bus-cycle state, rebuilt every access; not serialized.
    #[serde(skip)]
    cpu_custom_request_slot: Option<(u32, u32)>,
    dbg_bpl_cck: Vec<u32>,
    dbg_slotmap: Vec<Vec<u8>>,
    dbg_slotmap_on: bool,
    dbg_slotmap_dumped: bool,
    #[serde(skip)]
    frame_analyzer_enabled: bool,
    /// Allocate the parallel 24-byte DMA/event record grid in addition to
    /// the cheap owner map. Independent so console accounting stays cheap.
    #[serde(skip)]
    frame_analyzer_full: bool,
    /// While the CPU is inside a chip-bus wait loop with the analyzer armed:
    /// the pending access and who is denying it, re-evaluated before every
    /// colour clock the CPU misses so the trace can attribute that clock.
    /// Diagnostic only; never set while the analyzer is off.
    #[serde(skip)]
    cpu_bus_wait: Option<CpuWaitSample>,
    #[serde(skip)]
    cpu_trace_access: Option<CpuTraceAccess>,
    #[serde(skip)]
    cpu_trace_slots: Vec<CpuTraceSlot>,
    #[serde(skip)]
    cia_trace_phase: u8,
    #[serde(skip)]
    cia_trace_slot: Option<(u32, u32)>,
    #[serde(skip)]
    trace_cia_irq_pins: (bool, bool),
    /// Last Paula request latch observed by the full trace/event exporter.
    /// Kept separately from the IPL pipe because INTREQ-set events include
    /// disabled sources too.
    #[serde(skip)]
    trace_intreq: u16,
    #[serde(skip)]
    current_frame_bus_trace: FrameBusTrace,
    #[serde(skip)]
    last_frame_bus_trace: Option<FrameBusTrace>,
    /// Identity of the blit currently represented in the analyzer traces.
    /// Host-only diagnostic state: the real pending blit lives in `blitter`.
    #[serde(skip)]
    active_frame_blit: Option<u64>,
    #[serde(skip)]
    next_frame_blit_id: u64,
    #[serde(skip)]
    bus_event_observers: u32,
    #[serde(skip)]
    bus_events: std::collections::VecDeque<BusEvent>,
    #[serde(skip)]
    bus_event_next_sequence: u64,
    /// Waveform (VCD) capture: `wave_on` is the single hot-path gate for
    /// every sampling tap (true while armed or capturing), `wave_pc_trigger`
    /// the per-instruction gate for the `pc=` trigger. Host-side observer
    /// state, never serialized.
    #[serde(skip)]
    wave_on: bool,
    /// Combined zero-observer gate for the per-colour-clock arbitration path.
    /// The individual diagnostics remain independently controlled; this folds
    /// their four normally-false branches into one.
    #[serde(skip)]
    chip_bus_observers_on: bool,
    #[serde(skip)]
    pub(crate) wave_pc_trigger: bool,
    #[serde(skip)]
    wave: Option<Box<crate::waveform::WaveCapture>>,
    /// Debugger-window custom-register watch offsets ($000-$1FE, word
    /// aligned), mirrored from the CPU machine's InteractiveBreaks, and
    /// the first pending hit since the debugger last polled. Recorded in
    /// the custom-register write path so every writer (CPU and Copper)
    /// is seen, including writes landing while the CPU is in STOP.
    ui_reg_watches: Vec<u16>,
    #[serde(skip)]
    ui_reg_hit: Option<UiRegHit>,
    /// Debugger beam traps and the first pending hit since the debugger
    /// last polled. Checked where the beam advances (`advance_beam`), so a
    /// hit lands at exact beam granularity and even while the CPU sits in
    /// STOP. Transient debug state, never serialized.
    #[serde(skip)]
    ui_beam_traps: Vec<BeamTrap>,
    #[serde(skip)]
    ui_beam_hit: Option<(u16, u16)>,
    /// Debugger Copper breakpoints (instruction addresses) and the first
    /// pending hit since last polled. A hit fires when the live Copper's
    /// PC first arrives at a breakpointed address -- before the
    /// instruction there executes -- whether it got there by sequential
    /// fetch, COPJMP, a CPU strobe write, or the vertical-blank restart.
    /// Transient debug state, never serialized.
    #[serde(skip)]
    ui_copper_breaks: Vec<u32>,
    #[serde(skip)]
    ui_copper_hit: Option<(u32, u16, u16)>,
    /// Debugger video-layer isolation (Video tab). Transient debug state:
    /// the Default of all-layers-visible is restored on state load.
    #[serde(skip)]
    ui_layer_masks: UiLayerMasks,
    /// Watched memory word addresses mirrored from the debugger (also
    /// forwarded into the blitter and floppy so their write sites can
    /// flag hits), plus the writers of recent watched-word writes for
    /// stop attribution. Transient debug state.
    #[serde(skip)]
    ui_mem_watch_addrs: Vec<u32>,
    #[serde(skip)]
    ui_mem_writers: Vec<UiMemWriter>,
    /// Watched words touched by read-side CPU or DMA accesses since the
    /// debugger last polled. Reads leave the value alone, so the
    /// value-compare watch loop cannot see them at all; the channels
    /// latch the accesses here instead and the CPU's post-step check
    /// promotes it. Transient debug state.
    #[serde(skip)]
    ui_mem_reads: Vec<UiMemWriter>,
    /// The Copper PC at the last breakpoint check, so arrival at an
    /// address fires once instead of on every eligible colour clock the
    /// PC rests there.
    #[serde(skip)]
    ui_copper_last_pc: u32,
    /// Start PC of the instruction the CPU is currently executing,
    /// republished here once per instruction by the CPU wrapper so the
    /// chipset paths can attribute an access to the code that made it.
    /// Transient: re-stamped on the next instruction after any restore.
    #[serde(skip)]
    pub(crate) cpu_pc: u32,
    /// The custom-register access validator's report, present only while
    /// the validator is armed (`[debug] validate_chipset`). Transient
    /// diagnostic state; a restore starts a fresh report rather than
    /// resurrecting findings from an abandoned timeline.
    #[serde(skip)]
    regcheck: Option<Box<crate::regcheck::RegCheck>>,
    /// Last value written to each custom register and who wrote it,
    /// indexed by word offset / 2. Armed alongside the validator; this is
    /// the "what set BPLCON3, and from where?" question that otherwise
    /// costs a bisect.
    #[serde(skip)]
    reg_writers: Option<Box<[Option<RegWrite>; 256]>>,
    /// Self-modifying-code detector, present only while armed
    /// (`[debug] detect_smc`). Transient diagnostic state; a restore
    /// starts with a fresh, empty execution map.
    #[serde(skip)]
    pub(crate) smc: Option<Box<crate::smc::SmcTracker>>,
    /// Debugger-injected bus faults. Transient debug state: a restore
    /// does not resurrect them, since they describe an experiment the
    /// operator is running, not machine state.
    #[serde(skip)]
    injected_faults: Vec<FaultInjection>,
    /// Memory heat map, present only while armed. Transient diagnostic
    /// state; a restore starts a fresh, cold map.
    #[serde(skip)]
    heatmap: Option<Box<crate::heatmap::HeatMap>>,
    blitter_slowdown_cpu_misses: u8,
    /// Pending INTREQ.BLIT raise, in colour clocks. Real Agnus raises the
    /// blitter interrupt one clock after the sequencer's BLTDONE cycle
    /// (vAmiga scheduleIrqRel(BLIT, 1) at the terminal micro-cycle), so a
    /// scheduled completion arms this one-cck countdown instead of setting
    /// INTREQ in the final slot itself. Forced drains raise immediately.
    blit_irq_delay_cck: Option<u32>,
    slice_bus_advanced_cck: u32,
    slice_bus_tick: AgnusTick,
    // Deferred timed-device clock. Ticking CIA/serial/pots/audio/floppy/Akiko
    // once per CPU bus access dominated the host profile; instead accumulate the
    // color clocks here and `flush_timed_devices` them in one batch only when a
    // device is actually observed (a CIA/custom/peripheral access) or at an
    // instruction boundary (interrupt recognition). The CIA E-clock divider and
    // every device tick are exact under batching, so observable timing is
    // unchanged. Transient (always flushed to zero at frame boundaries, where
    // save states are taken), so skipped from serialization.
    #[serde(skip)]
    pending_device_cck: u32,
    #[serde(skip)]
    pending_device_tick: AgnusTick,
    /// Set when a device's `tick` actually DMA'd into guest memory
    /// (`DeviceHost::wrote_memory`, or the SDMAC/A2091 write latches) --
    /// e.g. copperhf's worker draining a completed read into the caller's
    /// buffer. The CPU run loop consumes this at the next instruction
    /// boundary AND before serving any data-cache read hit (the JIT batch
    /// path can flush timed devices mid-batch) and drops its data-cache
    /// model, mirroring the `clear_all` the synchronous device-access path
    /// already performs (cpu.rs): without it, DMA'd fast-RAM data reads
    /// back stale through the modelled 030/040/060 data cache. Transient
    /// (consumed at every instruction boundary), so skipped from
    /// serialization.
    #[serde(skip)]
    devices_wrote_memory: bool,
    /// Raster spans covered by `pending_device_cck` while exact bus-event
    /// metadata is armed. Timed devices still advance in their ordinary
    /// instruction-boundary batch; these transient spans only map events
    /// reported by that batch back to the colour clock where they occurred.
    #[serde(skip)]
    pending_device_trace_spans: Vec<BusTraceSpan>,
    audio_pending_cck: u32,
    last_chip_bus_owner: ChipBusOwner,
    /// Last 16-bit value driven on the chip data bus by a real access (display/
    /// audio/sprite DMA, or a mapped CPU read). CPU reads of unmapped addresses
    /// float to this, like the Agnus-arbitrated chip bus on real hardware, which
    /// is dominated by display DMA (often 0 on a blank screen) -- not a fixed
    /// all-ones pattern. Transient; re-established by DMA after a state load.
    #[serde(skip)]
    pub(crate) data_bus: u16,
    device_clock: DeviceClock,
    emulated_cck: u64,
    emulated_frames: u64,
    #[serde(skip)]
    blitter_trace: Option<std::fs::File>,
    display_dma_bplpt: [u32; 8],
    display_dma_sprpt: [u32; 8],
    /// Per-sprite SPRxPT value to seed the next frame's sprite-DMA replay with.
    /// Real Agnus advances SPRxPT through sprite DMA and does not snap it back to
    /// the last Copper/CPU write at the top of a field: once a channel has read
    /// its terminating descriptor its pointer sits past the consumed list. We
    /// model that by carrying the finished channel's DMA frontier across the
    /// frame boundary instead of re-seeding from `denise.sprpt` (the stale last
    /// write), so a sprite descriptor buffer that software rewrites every field
    /// is not re-armed from its previous, now-overwritten address before the
    /// Copper reloads SPRxPT. Captured at each frame start from the live
    /// (fetch-advanced) pointers, and serialized: the live pointers move on
    /// between the frame start and a mid-frame state save, so it cannot be
    /// re-derived at load time.
    #[serde(default)]
    sprite_dma_frame_start_ptr: [u32; 8],
    /// Cursor for the pre-display sprite-DMA replay, which runs in step with
    /// the beam rather than in one batch at the display start: a fetch has to
    /// read chip RAM at its own beam time, and software commonly rewrites the
    /// sprite descriptors during vertical blank, after the vertical-blank
    /// control-word fetch has already happened on hardware. `vpos` is the next
    /// pre-display line to replay, `event_idx` the position reached in this
    /// frame's render-event list, and `dmacon` the DMACON value the replayed
    /// writes have built up so far. Serialized so a state saved part way
    /// through the pre-display window resumes the replay where it stopped.
    #[serde(default)]
    sprite_dma_replay: SpriteDmaReplayCursor,
    // The register-level sprite DMA state (comparator copies, DMA
    // flip-flops, display latches). Chip state: serialized and restored
    // exactly by save states.
    display_dma_sprite_state: [DisplaySpriteDmaState; 8],
    display_dma_clipped_rows_advanced: bool,
    bitplane_dmacon_delay: Option<BitplaneDmaconDelay>,
    bitplane_bplcon0_delay: Option<BitplaneBplcon0Delay>,
    bitplane_ddfstart_miss: Option<BitplaneDdfStartMiss>,
    /// Memoized bitplane fetch plans for `bitplane_slot_active_at`, keyed on
    /// the registers that feed it. The arbiter asks for bitplane ownership on
    /// every slot candidate, and mid-line fetch-shape changes can alternate
    /// between a few valid plans. Per-entry Cells avoid copying the whole cache
    /// on each hit while keeping the `&self` owner-selection call graph intact.
    #[serde(skip)]
    bitplane_slot_plan_cache: BitplaneSlotPlanCache,
    /// Complete slot mask for an unchanged wide-FMODE scanline.
    #[serde(skip)]
    wide_bitplane_hot_line: WideBitplaneHotLine,
    /// Current line once a fetch-affecting register write makes the static
    /// wide-FMODE mask ineligible.
    #[serde(skip)]
    wide_bitplane_dynamic_vpos: std::cell::Cell<Option<u32>>,
    /// Bitplane DDF sequencer flop state at the start of the current line
    /// (see src/bus/ddf_line.rs); carried across lines by the flop walk.
    ddf_seq_line_initial: std::cell::Cell<crate::chipset::ddf_sequencer::DdfState>,
    /// DDFSTRT/DDFSTOP values as of the start of the current line (mid-line
    /// rewrites replay through `ddf_seq_writes`).
    ddf_seq_line_start_regs: std::cell::Cell<(u16, u16)>,
    /// (bmapen, bplcon0) as of the start of the current line; used only when
    /// mid-line DMACON/BPLCON0 writes are in the log.
    ddf_seq_line_start_ctl: std::cell::Cell<(bool, u16)>,
    /// Register writes that reached the sequencer during the current line.
    ddf_seq_writes: std::cell::RefCell<Vec<ddf_line::DdfSeqWrite>>,
    /// The current line's walked fetch table (rebuilt on demand).
    #[serde(skip)]
    ddf_seq_line: std::cell::RefCell<Option<ddf_line::DdfSeqLine>>,
    /// Compact mirror of the current DDF table's slot data. The arbiter and
    /// capture loop query this on every colour clock; keeping the immutable
    /// per-slot values in Cells avoids a dynamic RefCell borrow on that path
    /// while `ddf_seq_line` remains the authoritative lazy table.
    #[serde(skip)]
    ddf_seq_hot_line: ddf_line::DdfSeqHotLine,
    bus_accounting: BusAccounting,
    /// Latches once BEAMCON0.DUAL (A2024/UHRES) is first seen set, so the
    /// "not emulated" warning is logged a single time, not per write.
    uhres_dual_warned: bool,
    /// Stock-ratio cck-per-word for CPU external (fast RAM, ROM) accesses,
    /// in hundredths. Default 200 (= 2.00 cck/word, the real 68000 figure);
    /// diagnostic builds can override it for timing experiments.
    dbg_ext_cck_x100: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum CpuPaletteTarget {
    Top,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BeamWriteSource {
    Cpu,
    CpuCopperIrq,
    Copper,
}

/// One-shot env flag for the CPU write-landing trace
/// (`COPPERLINE_DIAG_CPU_WRITES=1`); see [`Bus::diag_cpu_write`].
fn diag_cpu_writes_on() -> bool {
    use std::sync::OnceLock;
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| crate::envcfg::flag("COPPERLINE_DIAG_CPU_WRITES"))
}

/// One-shot env flag for the CPU read-landing trace
/// (`COPPERLINE_DIAG_CPU_READS=1`); see [`Bus::diag_cpu_read`].
fn diag_cpu_reads_on() -> bool {
    use std::sync::OnceLock;
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| crate::envcfg::flag("COPPERLINE_DIAG_CPU_READS"))
}

fn diag_cpu_read_matches(off: u16, size: usize) -> bool {
    diag_cpu_reads_on()
        && diag_cpu_bus_addr_matches(Some(custom_register_cpu_addr(u64::from(off))), size)
}

/// One-shot env flag for the CPU chip-bus access trace
/// (`COPPERLINE_DIAG_CPU_BUS=1`); see [`Bus::diag_cpu_bus_access`].
fn diag_cpu_bus_on() -> bool {
    use std::sync::OnceLock;
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| crate::envcfg::flag("COPPERLINE_DIAG_CPU_BUS"))
}

fn cpu_bus_access_kind_name(kind: CpuBusAccessKind) -> &'static str {
    match kind {
        CpuBusAccessKind::Fetch => "fetch",
        CpuBusAccessKind::Read => "read",
        CpuBusAccessKind::Write => "write",
        CpuBusAccessKind::Custom => "custom",
    }
}

fn diag_cpu_bus_addr_ranges() -> &'static [(u32, u32)] {
    use std::sync::OnceLock;
    static V: OnceLock<Vec<(u32, u32)>> = OnceLock::new();
    V.get_or_init(|| {
        let Some(spec) = crate::envcfg::var("COPPERLINE_DIAG_CPU_BUS_ADDR") else {
            return Vec::new();
        };
        spec.split(',')
            .filter_map(|part| {
                let part = part.trim();
                if part.is_empty() {
                    return None;
                }
                if let Some((lo, hi)) = part.split_once(':') {
                    let lo = crate::envcfg::parse_u32(lo)?;
                    let hi = crate::envcfg::parse_u32(hi)?;
                    return Some((lo.min(hi), lo.max(hi)));
                }
                let addr = crate::envcfg::parse_u32(part)?;
                Some((addr, addr))
            })
            .collect()
    })
    .as_slice()
}

fn diag_cpu_bus_addr_matches(addr: Option<u32>, size: usize) -> bool {
    let ranges = diag_cpu_bus_addr_ranges();
    if ranges.is_empty() {
        return true;
    }
    let Some(addr) = addr else {
        return false;
    };
    let last = addr.saturating_add(size.max(1) as u32 - 1);
    ranges.iter().any(|&(lo, hi)| addr <= hi && last >= lo)
}

fn custom_register_cpu_addr(addr: u64) -> u32 {
    const CUSTOM_BASE: u32 = 0x00DF_F000;
    const CUSTOM_SIZE: u32 = 0x0000_1000;
    let addr = addr as u32;
    if addr < CUSTOM_SIZE {
        CUSTOM_BASE + addr
    } else {
        addr
    }
}

fn beam_write_source_name(source: BeamWriteSource) -> &'static str {
    match source {
        BeamWriteSource::Cpu => "cpu",
        BeamWriteSource::CpuCopperIrq => "cpu_copper_irq",
        BeamWriteSource::Copper => "copper",
    }
}

/// A debugger-injected bus fault: a CPU access inside this window and
/// matching this direction takes a bus error instead of reaching memory.
///
/// This is a host debugger facility, not emulated hardware: it exists so
/// a guest's own fault handler can be exercised deterministically,
/// rather than by finding an address that happens to be undecoded on the
/// machine under test.
#[derive(Clone, Copy, Debug)]
pub struct FaultInjection {
    /// Inclusive address window.
    pub start: u32,
    pub end: u32,
    pub on_read: bool,
    pub on_write: bool,
    /// Faults left to deliver; `None` never expires.
    pub remaining: Option<u32>,
    pub hits: u64,
}

/// The most recent write to one custom register, for the debugger's
/// "who last touched this register?" view.
#[derive(Clone, Copy, Debug)]
pub struct RegWrite {
    pub value: u16,
    pub writer: crate::regcheck::Writer,
    pub vpos: u16,
    pub hpos: u16,
    pub frame: u64,
}

/// A debugger-window custom-register watch hit: the first watched write
/// since the debugger last polled, with its writer and beam position.
#[derive(Debug, Clone, Copy)]
pub struct UiRegHit {
    pub off: u16,
    pub value: u16,
    pub source: &'static str,
    pub vpos: u16,
    pub hpos: u16,
}

/// A debugger beam trap: halt when the Agnus beam reaches (or first passes)
/// a position. `hpos: None` fires at the first colour clock of the line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BeamTrap {
    pub vpos: u16,
    pub hpos: Option<u16>,
    /// Remove after the first hit (a run-to-position trap).
    pub once: bool,
}

/// Who last wrote a watched memory word, recorded at the write site.
#[derive(Debug, Clone, Copy)]
pub struct UiMemWriter {
    pub addr: u32,
    pub source: crate::debugger::WatchSource,
    pub vpos: u16,
    pub hpos: u16,
}

/// Debugger video-layer isolation masks: bit n set = bitplane n /
/// sprite n is drawn. Output-only filters applied where pixels resolve
/// to colours -- collision accumulation, playfield priority, and every
/// CPU-visible value always use the true data, so hiding a layer can
/// never perturb the emulation. All layers visible by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiLayerMasks {
    pub planes: u8,
    pub sprites: u8,
}

impl Default for UiLayerMasks {
    fn default() -> Self {
        Self {
            planes: 0xFF,
            sprites: 0xFF,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BeamRegisterWrite {
    pub vpos: u32,
    pub hpos: u32,
    pub offset: u16,
    pub value: u16,
    pub source: BeamWriteSource,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BeamChipRamWrite {
    pub vpos: u32,
    pub hpos: u32,
    pub offset: u32,
    bytes: [u8; 4],
    len: u8,
}

impl BeamChipRamWrite {
    fn from_cpu_write(vpos: u32, hpos: u32, offset: usize, size: usize, value: u32) -> Self {
        // A 68020+ three-byte operand can cross chip RAM as one dynamically
        // sized plain-memory transfer. The four-byte record stores that width
        // directly just like byte, word, and long writes.
        debug_assert!((1..=4).contains(&size));
        let mut bytes = [0; 4];
        let len = size.min(bytes.len());
        for (idx, byte) in bytes.iter_mut().enumerate().take(len) {
            let shift = (len - 1 - idx) * 8;
            *byte = ((value >> shift) & 0xFF) as u8;
        }
        Self {
            vpos,
            hpos,
            offset: offset as u32,
            bytes,
            len: len as u8,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_bytes(vpos: u32, hpos: u32, offset: u32, src: &[u8]) -> Self {
        let mut bytes = [0; 4];
        let len = src.len().min(bytes.len());
        bytes[..len].copy_from_slice(&src[..len]);
        Self {
            vpos,
            hpos,
            offset,
            bytes,
            len: len as u8,
        }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RenderRegisterSnapshot {
    pub agnus_revision: AgnusRevision,
    /// BEAMCON0.HARDDIS active when this frame's registers were captured.
    pub harddis: bool,
    pub dmacon: u16,
    pub bplcon0: u16,
    pub bplcon1: u16,
    pub bplcon2: u16,
    pub bplcon3: u16,
    pub bplcon4: u16,
    pub fmode: u16,
    pub clxcon: u16,
    pub clxcon2: u16,
    pub bplpt: [u32; 8],
    pub bpldat: [u16; 8],
    pub sprpt: [u32; 8],
    /// CPU/Copper write-shadow sprite registers (see the matching fields
    /// on `Denise`): the render replay and live collisions are calibrated
    /// against these.
    pub sprpos: [u16; 8],
    pub sprctl: [u16; 8],
    pub sprdata: [u16; 8],
    pub sprdatb: [u16; 8],
    pub spr_armed: [bool; 8],
    /// Hardware-true sprite registers (manual writes AND sprite DMA
    /// fetches): the armed-latch redisplay source for DMA-idle frames.
    pub spr_hw_pos: [u16; 8],
    pub spr_hw_ctl: [u16; 8],
    pub spr_hw_data: [u16; 8],
    pub spr_hw_datb: [u16; 8],
    pub spr_hw_armed: [bool; 8],
    pub bpl1mod: i16,
    pub bpl2mod: i16,
    pub palette: Palette,
    pub diwstrt: u16,
    pub diwstop: u16,
    pub diwhigh: DiwHigh,
    pub ddfstrt: u16,
    pub ddfstop: u16,
    /// Agnus LOF at this frame's start: with BPLCON0 LACE set, true for
    /// the long (upper) field of an interlaced pair. Set after the frame
    /// wrap toggles LOF (see `update_interlace_long_frame`); used by the
    /// presentation deinterlacer to route field lines by parity.
    pub long_field: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuBusAccessKind {
    Fetch,
    Read,
    Write,
    Custom,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ChipBusOwner {
    Refresh,
    Bitplane,
    Sprite,
    Disk,
    Audio,
    Copper,
    Blitter,
    Cpu,
    #[default]
    Idle,
}

pub const CHIP_BUS_OWNER_NAMES: [&str; 9] = [
    "refresh", "bitplane", "sprite", "disk", "audio", "copper", "blitter", "cpu", "idle",
];

/// ECS programmable timing has an 11-bit VTOTAL, so a diagnostic trace must
/// retain rows outside the renderer's deliberately smaller display aperture.
pub const FRAME_ANALYZER_MAX_VPOS: usize = 2048;
pub const FRAME_ANALYZER_MAX_HPOS: usize = 512;

// Single-char codes for the COPPERLINE_DIAG_SLOTMAP per-color-clock owner map,
// chosen to line up with vAmiga's DMA-Debugger slot colours for visual diffing.
fn chip_bus_owner_code(owner: ChipBusOwner) -> u8 {
    match owner {
        ChipBusOwner::Refresh => b'R',
        ChipBusOwner::Bitplane => b'B',
        ChipBusOwner::Sprite => b'S',
        ChipBusOwner::Disk => b'D',
        ChipBusOwner::Audio => b'A',
        ChipBusOwner::Copper => b'C',
        ChipBusOwner::Blitter => b'L',
        ChipBusOwner::Cpu => b'P',
        ChipBusOwner::Idle => b'.',
    }
}

impl ChipBusOwner {
    fn accounting_index(self) -> usize {
        match self {
            ChipBusOwner::Refresh => 0,
            ChipBusOwner::Bitplane => 1,
            ChipBusOwner::Sprite => 2,
            ChipBusOwner::Disk => 3,
            ChipBusOwner::Audio => 4,
            ChipBusOwner::Copper => 5,
            ChipBusOwner::Blitter => 6,
            ChipBusOwner::Cpu => 7,
            ChipBusOwner::Idle => 8,
        }
    }
}

/// Why a CPU chip-bus access was denied the colour clock it asked for: the
/// agent that held the slot, split for the blitter by DMACON BLTPRI, since
/// the two holds have different remedies. `Port` is the 020+ chip port's
/// two-clock turnaround (`cpu_chip_port_free_at`): the CPU is waiting, but
/// on its own bus unit, not on another DMA channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuWaitClass {
    Refresh,
    Bitplane,
    Sprite,
    Disk,
    Audio,
    Copper,
    /// Blitter holding the bus with BLTPRI clear: the "nice" hold before
    /// the blitter-slowdown counter yields a slot.
    BlitterNice,
    /// Blitter holding the bus with BLTPRI set ("blitter nasty"), including
    /// the warm-up fence, where the blitter's own slot needs no bus and the
    /// recorded slot owner is idle.
    BlitterNasty,
    Port,
}

pub const CPU_WAIT_CLASS_NAMES: [&str; 9] = [
    "refresh",
    "bitplane",
    "sprite",
    "disk",
    "audio",
    "copper",
    "blitter",
    "blitter_nasty",
    "port",
];

/// Grid code for a slot the CPU waited through: the denier's owner letter,
/// `N` for the BLTPRI-set blitter, `p` for the 020+ port turnaround, and
/// `.` when the CPU was not waiting (see `chip_bus_owner_code`).
pub fn cpu_wait_class_code(class: CpuWaitClass) -> u8 {
    match class {
        CpuWaitClass::Refresh => b'R',
        CpuWaitClass::Bitplane => b'B',
        CpuWaitClass::Sprite => b'S',
        CpuWaitClass::Disk => b'D',
        CpuWaitClass::Audio => b'A',
        CpuWaitClass::Copper => b'C',
        CpuWaitClass::BlitterNice => b'L',
        CpuWaitClass::BlitterNasty => b'N',
        CpuWaitClass::Port => b'p',
    }
}

pub fn cpu_wait_class_name_for_code(code: u8) -> Option<&'static str> {
    let idx = match code {
        b'R' => 0,
        b'B' => 1,
        b'S' => 2,
        b'D' => 3,
        b'A' => 4,
        b'C' => 5,
        b'L' => 6,
        b'N' => 7,
        b'p' => 8,
        _ => return None,
    };
    Some(CPU_WAIT_CLASS_NAMES[idx])
}

impl CpuWaitClass {
    fn accounting_index(self) -> usize {
        match self {
            CpuWaitClass::Refresh => 0,
            CpuWaitClass::Bitplane => 1,
            CpuWaitClass::Sprite => 2,
            CpuWaitClass::Disk => 3,
            CpuWaitClass::Audio => 4,
            CpuWaitClass::Copper => 5,
            CpuWaitClass::BlitterNice => 6,
            CpuWaitClass::BlitterNasty => 7,
            CpuWaitClass::Port => 8,
        }
    }
}

pub const CPU_BUS_ACCESS_KIND_NAMES: [&str; 4] = ["fetch", "read", "write", "custom"];

impl CpuBusAccessKind {
    fn accounting_index(self) -> usize {
        match self {
            CpuBusAccessKind::Fetch => 0,
            CpuBusAccessKind::Read => 1,
            CpuBusAccessKind::Write => 2,
            CpuBusAccessKind::Custom => 3,
        }
    }
}

/// One colour clock the CPU spent waiting for the chip bus, as the
/// analyzer's trace attributes it: who denied the slot, what kind of access
/// was pending, and the instruction that made it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuWaitSample {
    pub class: CpuWaitClass,
    pub kind: CpuBusAccessKind,
    pub pc: u32,
}

#[derive(Clone, Copy, Debug)]
struct CpuTraceAccess {
    addr: u32,
    size: u8,
    kind: CpuBusAccessKind,
    /// Byte offset of this bus slot inside the CPU's big-endian operand.
    value_offset: u8,
    /// Whole operand width, used to select this slot's data after the memory
    /// operation has completed.
    value_size: u8,
    /// Posted writes can retire after the CPU-side write has completed. Keep
    /// their already-known slot value here instead of peeking unrelated RAM.
    data: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
struct CpuTraceSlot {
    frame: u64,
    vpos: u32,
    hpos: u32,
    value_offset: u8,
    size: u8,
}

fn cpu_trace_access_for_slot(
    addr: u32,
    size: usize,
    kind: CpuBusAccessKind,
    wide_bus: bool,
    slot: u32,
) -> CpuTraceAccess {
    let stride = if wide_bus { 4 } else { 2 };
    let value_offset = (slot * stride) as usize;
    let slot_size = size.saturating_sub(value_offset).min(stride as usize);
    CpuTraceAccess {
        addr: addr.wrapping_add(value_offset as u32),
        size: slot_size.clamp(1, 8) as u8,
        kind,
        value_offset: value_offset.min(7) as u8,
        value_size: size.clamp(1, 8) as u8,
        data: None,
    }
}

fn cpu_trace_data_chunk(value: u64, value_size: u8, value_offset: u8, size: u8) -> u64 {
    let trailing_bytes = value_size.saturating_sub(value_offset.saturating_add(size));
    let shifted = value >> (u32::from(trailing_bytes) * 8);
    let bits = u32::from(size) * 8;
    if bits >= 64 {
        shifted
    } else {
        shifted & ((1u64 << bits) - 1)
    }
}

/// Distinct stalled PCs the trace keeps per frame; further PCs pool into
/// `cpu_wait_pc_other`.
pub const CPU_WAIT_PC_CAP: usize = 4096;

/// WinUAE/Bartman-compatible DMA-record owner classes. Zero is an idle slot.
pub const BUS_RECORD_REFRESH: u8 = 1;
pub const BUS_RECORD_CPU: u8 = 2;
pub const BUS_RECORD_COPPER: u8 = 3;
pub const BUS_RECORD_AUDIO: u8 = 4;
pub const BUS_RECORD_BLITTER: u8 = 5;
pub const BUS_RECORD_BITPLANE: u8 = 6;
pub const BUS_RECORD_SPRITE: u8 = 7;
pub const BUS_RECORD_DISK: u8 = 8;
pub const BUS_RECORD_CONFLICT: u8 = 9;

/// Bartman's established event-bit assignments. `BLITSTARTFINISH` is shared
/// by the two edges; streamed bus events retain the edge name.
pub const BUS_EVENT_BLIT_IRQ: u32 = 1 << 0;
pub const BUS_EVENT_BLIT_FINAL_D: u32 = 1 << 1;
pub const BUS_EVENT_BLIT_START_FINISH: u32 = 1 << 2;
pub const BUS_EVENT_BPL_FETCH_UPDATE: u32 = 1 << 3;
pub const BUS_EVENT_COPPER_WAKE: u32 = 1 << 4;
pub const BUS_EVENT_CPU_IRQ: u32 = 1 << 5;
pub const BUS_EVENT_INTREQ: u32 = 1 << 6;
pub const BUS_EVENT_COPPER_WANTED: u32 = 1 << 7;
pub const BUS_EVENT_NOONE_GETS: u32 = 1 << 8;
pub const BUS_EVENT_CPU_BLITTER_STEAL: u32 = 1 << 9;
pub const BUS_EVENT_CPU_BLITTER_STOLEN: u32 = 1 << 10;
pub const BUS_EVENT_COPPER_SKIP: u32 = 1 << 11;
pub const BUS_EVENT_DDFSTRT: u32 = 1 << 12;
pub const BUS_EVENT_DDFSTOP: u32 = 1 << 13;
pub const BUS_EVENT_DDFSTOP2: u32 = 1 << 14;
pub const BUS_EVENT_SPECIAL: u32 = 1 << 15;
pub const BUS_EVENT_VB: u32 = 1 << 16;
pub const BUS_EVENT_VS: u32 = 1 << 17;
pub const BUS_EVENT_LOF: u32 = 1 << 18;
pub const BUS_EVENT_LOL: u32 = 1 << 19;
pub const BUS_EVENT_HBS: u32 = 1 << 20;
pub const BUS_EVENT_HBE: u32 = 1 << 21;
pub const BUS_EVENT_HDIWS: u32 = 1 << 22;
pub const BUS_EVENT_HDIWE: u32 = 1 << 23;
pub const BUS_EVENT_VDIW: u32 = 1 << 24;
pub const BUS_EVENT_HSS: u32 = 1 << 25;
pub const BUS_EVENT_HSE: u32 = 1 << 26;
pub const BUS_EVENT_CIAA_IRQ: u32 = 1 << 27;
pub const BUS_EVENT_CIAB_IRQ: u32 = 1 << 28;
pub const BUS_EVENT_CPU_STOP: u32 = 1 << 29;
pub const BUS_EVENT_CPU_STOP_IPL: u32 = 1 << 30;

pub fn bus_event_names(events: u32) -> Vec<&'static str> {
    [
        (BUS_EVENT_BLIT_IRQ, "blitter_irq"),
        (BUS_EVENT_BLIT_FINAL_D, "blitter_final_d"),
        (BUS_EVENT_BLIT_START_FINISH, "blitter_start_finish"),
        (BUS_EVENT_BPL_FETCH_UPDATE, "bitplane_fetch"),
        (BUS_EVENT_COPPER_WAKE, "copper_wake"),
        (BUS_EVENT_CPU_IRQ, "cpu_irq"),
        (BUS_EVENT_INTREQ, "intreq_set"),
        (BUS_EVENT_COPPER_WANTED, "copper_wanted"),
        (BUS_EVENT_NOONE_GETS, "noone_gets"),
        (BUS_EVENT_CPU_BLITTER_STEAL, "blitter_denied_by_cpu"),
        (BUS_EVENT_CPU_BLITTER_STOLEN, "cpu_denied_by_blitter"),
        (BUS_EVENT_COPPER_SKIP, "copper_skip"),
        (BUS_EVENT_DDFSTRT, "ddfstrt"),
        (BUS_EVENT_DDFSTOP, "ddfstop"),
        (BUS_EVENT_DDFSTOP2, "ddfstop2"),
        (BUS_EVENT_SPECIAL, "special"),
        (BUS_EVENT_VB, "vb"),
        (BUS_EVENT_VS, "vs"),
        (BUS_EVENT_LOF, "lof"),
        (BUS_EVENT_LOL, "lol"),
        (BUS_EVENT_HBS, "hbs"),
        (BUS_EVENT_HBE, "hbe"),
        (BUS_EVENT_HDIWS, "hdiws"),
        (BUS_EVENT_HDIWE, "hdiwe"),
        (BUS_EVENT_VDIW, "vdiw"),
        (BUS_EVENT_HSS, "hss"),
        (BUS_EVENT_HSE, "hse"),
        (BUS_EVENT_CIAA_IRQ, "cia_a_irq"),
        (BUS_EVENT_CIAB_IRQ, "cia_b_irq"),
        (BUS_EVENT_CPU_STOP, "cpu_stop"),
        (BUS_EVENT_CPU_STOP_IPL, "cpu_stop_ipl"),
    ]
    .into_iter()
    .filter_map(|(bit, name)| (events & bit != 0).then_some(name))
    .collect()
}

pub const BUS_EVENT_OBSERVATION_CAPACITY: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BusEvent {
    pub sequence: u64,
    pub name: &'static str,
    pub events: u32,
    pub frame: u64,
    pub cck: u64,
    pub vpos: u32,
    pub hpos: u32,
    pub ipl: u8,
}

/// A transfer performed without advancing the emulated beam. Floppy turbo
/// DMA can move several words at one position, so those records cannot fit in
/// the one-record-per-colour-clock raster grid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstantaneousBusRecord {
    pub vpos: u32,
    pub hpos: u32,
    pub record: BusSlotRecord,
}

#[derive(Clone, Copy, Debug)]
struct BusTracePosition {
    frame: u64,
    cck: u64,
    vpos: u32,
    hpos: u32,
}

#[derive(Clone, Copy, Debug)]
struct BusTraceSpan {
    position: BusTracePosition,
    offset: u32,
    cck: u32,
}

impl BusTraceSpan {
    fn position_at(self, offset: u32) -> Option<BusTracePosition> {
        let within = offset.checked_sub(self.offset)?;
        (within < self.cck).then_some(BusTracePosition {
            frame: self.position.frame,
            cck: self.position.cck.saturating_add(u64::from(within)),
            vpos: self.position.vpos,
            hpos: self.position.hpos.saturating_add(within),
        })
    }
}

fn bus_trace_position_at(spans: &[BusTraceSpan], offset: u32) -> Option<BusTracePosition> {
    let index = spans.partition_point(|span| span.offset.saturating_add(span.cck) <= offset);
    spans.get(index)?.position_at(offset)
}

/// Fixed 24-byte sidecar record, one per Agnus colour clock. `reg` is a
/// custom-register offset, `0x1000` for CPU memory, or `0xFFFF` when none.
/// `flags` bit 0 is a write; CIA records use bit 1 as valid, bit 2 for CIA-B,
/// bits 8..11 for the register, and bits 12..14 for the E-clock phase.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BusSlotRecord {
    pub data: u64,
    pub addr: u32,
    pub events: u32,
    pub reg: u16,
    pub flags: u16,
    pub kind: u8,
    pub subtype: u8,
    pub size: u8,
    pub ipl: u8,
}

impl Default for BusSlotRecord {
    fn default() -> Self {
        Self {
            data: 0,
            addr: 0,
            events: 0,
            reg: u16::MAX,
            flags: 0,
            kind: 0,
            subtype: 0,
            size: 0,
            ipl: 0,
        }
    }
}

impl BusSlotRecord {
    pub const BYTE_SIZE: usize = 24;

    pub fn write_to(&self, out: &mut impl std::io::Write) -> std::io::Result<()> {
        out.write_all(&self.reg.to_le_bytes())?;
        out.write_all(&[self.kind, self.subtype, self.size, self.ipl])?;
        out.write_all(&self.flags.to_le_bytes())?;
        out.write_all(&self.addr.to_le_bytes())?;
        out.write_all(&self.data.to_le_bytes())?;
        out.write_all(&self.events.to_le_bytes())
    }
}

#[derive(Clone, Debug, Default)]
pub struct FrameRegisterSnapshot {
    pub custom: Vec<u16>,
    pub palette_hi: Vec<u16>,
    pub palette_lo: Vec<u16>,
    pub chipset_flags: u32,
}

/// One blit referenced by the traced frame (Frame Analyzer / console BLITS).
/// `end` stays None while the blit is still running; an in-flight record is
/// carried across a frame boundary under the same stable id.
#[derive(Clone, Debug)]
pub struct FrameBlitRecord {
    /// Stable across frame boundaries, so a blit present in two frame
    /// records can be joined without guessing from its pointers.
    pub id: u64,
    pub bltcon0: u16,
    pub bltcon1: u16,
    pub width_words: u32,
    pub height: u32,
    pub descending: bool,
    pub line_mode: bool,
    pub fill_mode: bool,
    pub channels: [bool; 4],
    pub apt: u32,
    pub bpt: u32,
    pub cpt: u32,
    pub dpt: u32,
    pub modulos: [i16; 4],
    /// A and B shifts, in bits.
    pub shifts: [u8; 2],
    /// First/last A word masks.
    pub masks: [u16; 2],
    pub minterm: u8,
    /// Effective A, B and C constant inputs at the start. For USEB-off area
    /// blits, B is BLTBDAT's write-time-shifted hold latch rather than the
    /// raw register; disabled DMA channels use these values.
    pub data: [u16; 3],
    pub start_frame: u64,
    pub end_frame: Option<u64>,
    pub start: (u16, u16),
    pub end: Option<(u16, u16)>,
    /// Colour clocks on which the blitter pipeline advanced, and clocks on
    /// which a requested pipeline cycle was held by contention/DMA disable.
    pub cycles_used: u64,
    pub cycles_stalled: u64,
    /// Exact DMA transfer stream per channel, retained only while the frame
    /// analyzer is armed. It drives `blit.render` and remains bounded by the
    /// number of colour clocks captured in the frame.
    pub(crate) channel_words: [Vec<u16>; 4],
    pub(crate) channel_addrs: [Vec<u32>; 4],
    pub render_truncated: bool,
}

/// Per-frame chip-bus ownership trace for the interactive frame analyzer.
///
/// One byte records the owner for one Agnus colour clock at `(vpos, hpos)`.
/// It is deliberately compact because it is filled from the bus-arbitration
/// hot path while the analyzer is open.
#[derive(Clone, Debug)]
pub struct FrameBusTrace {
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
    /// Blits referenced by this frame (capped; see FRAME_BLIT_RECORD_CAP).
    /// An in-flight cross-frame record appears in both adjacent traces.
    pub blits: Vec<FrameBlitRecord>,
    owners: Vec<u8>,
    /// Full Bartman-style records. Absent at the cheap owner-only level.
    records: Option<std::sync::Arc<Vec<BusSlotRecord>>>,
    instantaneous_records: Vec<InstantaneousBusRecord>,
    pub registers: FrameRegisterSnapshot,
    /// Colour clocks the CPU spent waiting for the chip bus, by the class
    /// that denied it (`CpuWaitClass::accounting_index`) and by the kind of
    /// access that was pending (`CpuBusAccessKind::accounting_index`).
    pub cpu_wait_cck: u64,
    pub cpu_wait_by_class: [u64; 9],
    pub cpu_wait_by_kind: [u64; 4],
    /// Waited colour clocks per stalled instruction start PC, capped at
    /// `CPU_WAIT_PC_CAP` distinct PCs; the overflow pools in
    /// `cpu_wait_pc_other`. Only ever read out sorted (`top_stalled_pcs`).
    cpu_wait_pcs: std::collections::HashMap<u32, u32>,
    pub cpu_wait_pc_other: u64,
    /// Per-slot CPU wait grid parallel to `owners`: `cpu_wait_class_code`
    /// of the denier, or `.` where the CPU was not waiting.
    cpu_waits: Vec<u8>,
}

/// Blit records kept per traced frame.
pub const FRAME_BLIT_RECORD_CAP: usize = 64;
/// A malformed maximum-size ECS blit must not turn the diagnostic record
/// into an unbounded allocation. A normal PAL frame cannot reach this cap.
pub const FRAME_BLIT_RENDER_WORD_CAP: usize = 262_144;

impl Default for FrameBusTrace {
    fn default() -> Self {
        Self {
            frame: 0,
            seconds: 0.0,
            rows: 0,
            cols: 0,
            line_cck: COLORCLOCKS_PER_LINE,
            visible_start_vpos: RENDER_VISIBLE_START_VPOS,
            visible_lines: RENDER_VISIBLE_LINES,
            display_hpos_start: RENDER_COPPER_WAIT_HPOS_FB0,
            display_hpos_end: RENDER_COPPER_WAIT_HPOS_FB0 + (RENDER_FRAMEBUFFER_WIDTH as u32 / 4),
            owner_cck: [0; 9],
            blitter_busy_cck: 0,
            blitter_starve_cck: [0; 9],
            partial: false,
            blits: Vec::new(),
            owners: Vec::new(),
            records: None,
            instantaneous_records: Vec::new(),
            registers: FrameRegisterSnapshot::default(),
            cpu_wait_cck: 0,
            cpu_wait_by_class: [0; 9],
            cpu_wait_by_kind: [0; 4],
            cpu_wait_pcs: std::collections::HashMap::new(),
            cpu_wait_pc_other: 0,
            cpu_waits: Vec::new(),
        }
    }
}

impl FrameBusTrace {
    fn reset_for_frame_with_level(
        &mut self,
        frame: u64,
        seconds: f64,
        frame_lines: u32,
        line_cck: u32,
        visible_start_vpos: u32,
        visible_lines: usize,
        partial: bool,
        full: bool,
        registers: FrameRegisterSnapshot,
    ) {
        self.frame = frame;
        self.seconds = seconds;
        self.rows = (frame_lines as usize).clamp(1, FRAME_ANALYZER_MAX_VPOS);
        self.cols = (line_cck as usize).clamp(1, FRAME_ANALYZER_MAX_HPOS);
        self.line_cck = line_cck;
        self.visible_start_vpos = visible_start_vpos;
        self.visible_lines = visible_lines.min(FRAME_ANALYZER_MAX_VPOS);
        self.display_hpos_start = RENDER_COPPER_WAIT_HPOS_FB0;
        self.display_hpos_end = (RENDER_COPPER_WAIT_HPOS_FB0
            + (RENDER_FRAMEBUFFER_WIDTH as u32 / 4))
            .min(self.cols as u32);
        self.owner_cck = [0; 9];
        self.blitter_busy_cck = 0;
        self.blitter_starve_cck = [0; 9];
        self.partial = partial;
        self.blits.clear();
        self.owners.resize(self.rows * self.cols, b'.');
        self.owners.fill(b'.');
        self.records = full
            .then(|| std::sync::Arc::new(vec![BusSlotRecord::default(); self.rows * self.cols]));
        self.instantaneous_records.clear();
        self.registers = registers;
        self.reset_cpu_waits();
        self.cpu_waits.resize(self.rows * self.cols, b'.');
        self.cpu_waits.fill(b'.');
    }

    #[cfg(test)]
    fn reset_for_frame(
        &mut self,
        frame: u64,
        seconds: f64,
        frame_lines: u32,
        line_cck: u32,
        visible_start_vpos: u32,
        visible_lines: usize,
        partial: bool,
    ) {
        self.reset_for_frame_with_level(
            frame,
            seconds,
            frame_lines,
            line_cck,
            visible_start_vpos,
            visible_lines,
            partial,
            false,
            FrameRegisterSnapshot::default(),
        );
    }

    fn reset_cpu_waits(&mut self) {
        self.cpu_wait_cck = 0;
        self.cpu_wait_by_class = [0; 9];
        self.cpu_wait_by_kind = [0; 4];
        self.cpu_wait_pcs.clear();
        self.cpu_wait_pc_other = 0;
    }

    fn finish_window(&mut self, visible_start_vpos: u32, visible_lines: usize) {
        self.visible_start_vpos = visible_start_vpos.min(self.rows.saturating_sub(1) as u32);
        self.visible_lines = visible_lines.min(self.rows);
    }

    fn clear(&mut self) {
        self.rows = 0;
        self.cols = 0;
        self.owners.clear();
        self.records = None;
        self.instantaneous_records.clear();
        self.registers = FrameRegisterSnapshot::default();
        self.owner_cck = [0; 9];
        self.blitter_busy_cck = 0;
        self.blitter_starve_cck = [0; 9];
        self.partial = false;
        self.cpu_waits.clear();
        self.reset_cpu_waits();
    }

    fn record_with_ipl(
        &mut self,
        vpos: u32,
        hpos: u32,
        cck: u32,
        owner: ChipBusOwner,
        blitter_busy: bool,
        cpu_wait: Option<CpuWaitSample>,
        ipl: u8,
    ) {
        if self.rows == 0 || self.cols == 0 {
            return;
        }
        let v = vpos as usize;
        let h = hpos as usize;
        if v >= self.rows || h >= self.cols {
            return;
        }
        let idx = owner.accounting_index();
        self.owner_cck[idx] = self.owner_cck[idx].saturating_add(u64::from(cck));
        if blitter_busy {
            self.blitter_busy_cck = self.blitter_busy_cck.saturating_add(u64::from(cck));
            if !matches!(owner, ChipBusOwner::Blitter) {
                self.blitter_starve_cck[idx] =
                    self.blitter_starve_cck[idx].saturating_add(u64::from(cck));
            }
        }
        let code = chip_bus_owner_code(owner);
        let start = v * self.cols;
        let end = (h + cck as usize).min(self.cols);
        let row = &mut self.owners[start..start + self.cols];
        for slot in row.iter_mut().take(end).skip(h) {
            *slot = code;
        }
        if let Some(records) = self.records.as_mut().map(std::sync::Arc::make_mut) {
            let base = match owner {
                ChipBusOwner::Refresh => (BUS_RECORD_REFRESH, 0),
                ChipBusOwner::Cpu => (BUS_RECORD_CPU, 0),
                ChipBusOwner::Copper => (BUS_RECORD_COPPER, 0),
                ChipBusOwner::Audio => (BUS_RECORD_AUDIO, 0),
                ChipBusOwner::Blitter => (BUS_RECORD_BLITTER, 0),
                ChipBusOwner::Bitplane => (BUS_RECORD_BITPLANE, 0),
                ChipBusOwner::Sprite => (BUS_RECORD_SPRITE, 0),
                ChipBusOwner::Disk => (BUS_RECORD_DISK, 0),
                ChipBusOwner::Idle => (0, 0),
            };
            for record in records.iter_mut().take(start + end).skip(start + h) {
                if record.kind == 0 {
                    record.kind = base.0;
                    record.subtype = base.1;
                }
                record.ipl = ipl;
            }
        }
        if let Some(wait) = cpu_wait {
            self.record_cpu_wait(start, h, end, cck, wait);
        }
    }

    #[cfg(test)]
    fn record(
        &mut self,
        vpos: u32,
        hpos: u32,
        cck: u32,
        owner: ChipBusOwner,
        blitter_busy: bool,
        cpu_wait: Option<CpuWaitSample>,
    ) {
        self.record_with_ipl(vpos, hpos, cck, owner, blitter_busy, cpu_wait, 0);
    }

    fn record_cpu_wait(
        &mut self,
        start: usize,
        h: usize,
        end: usize,
        cck: u32,
        wait: CpuWaitSample,
    ) {
        let cck64 = u64::from(cck);
        self.cpu_wait_cck = self.cpu_wait_cck.saturating_add(cck64);
        let class = wait.class.accounting_index();
        self.cpu_wait_by_class[class] = self.cpu_wait_by_class[class].saturating_add(cck64);
        let kind = wait.kind.accounting_index();
        self.cpu_wait_by_kind[kind] = self.cpu_wait_by_kind[kind].saturating_add(cck64);
        if let Some(total) = self.cpu_wait_pcs.get_mut(&wait.pc) {
            *total = total.saturating_add(cck);
        } else if self.cpu_wait_pcs.len() < CPU_WAIT_PC_CAP {
            self.cpu_wait_pcs.insert(wait.pc, cck);
        } else {
            self.cpu_wait_pc_other = self.cpu_wait_pc_other.saturating_add(cck64);
        }
        let code = cpu_wait_class_code(wait.class);
        let row = &mut self.cpu_waits[start..start + self.cols];
        for slot in row.iter_mut().take(end).skip(h) {
            *slot = code;
        }
    }

    pub fn owner_code_at(&self, vpos: usize, hpos: usize) -> u8 {
        if vpos >= self.rows || hpos >= self.cols {
            return b'.';
        }
        self.owners[vpos * self.cols + hpos]
    }

    pub fn owner_row(&self, vpos: usize) -> Option<&[u8]> {
        if vpos >= self.rows || self.cols == 0 {
            return None;
        }
        let start = vpos * self.cols;
        Some(&self.owners[start..start + self.cols])
    }

    /// The CPU wait code at a slot (`cpu_wait_class_code`, or `.`).
    pub fn cpu_wait_code_at(&self, vpos: usize, hpos: usize) -> u8 {
        if vpos >= self.rows || hpos >= self.cols {
            return b'.';
        }
        self.cpu_waits[vpos * self.cols + hpos]
    }

    pub fn cpu_wait_row(&self, vpos: usize) -> Option<&[u8]> {
        if vpos >= self.rows || self.cols == 0 {
            return None;
        }
        let start = vpos * self.cols;
        Some(&self.cpu_waits[start..start + self.cols])
    }

    pub fn full(&self) -> bool {
        self.records.is_some()
    }

    pub fn record_at(&self, vpos: usize, hpos: usize) -> Option<&BusSlotRecord> {
        if vpos >= self.rows || hpos >= self.cols {
            return None;
        }
        self.records.as_ref()?.get(vpos * self.cols + hpos)
    }

    pub fn record_row(&self, vpos: usize) -> Option<&[BusSlotRecord]> {
        if vpos >= self.rows || self.cols == 0 {
            return None;
        }
        let start = vpos * self.cols;
        Some(&self.records.as_ref()?[start..start + self.cols])
    }

    pub fn records(&self) -> Option<&[BusSlotRecord]> {
        self.records.as_deref().map(Vec::as_slice)
    }

    pub fn records_arc(&self) -> Option<std::sync::Arc<Vec<BusSlotRecord>>> {
        self.records.clone()
    }

    pub fn instantaneous_records(&self) -> &[InstantaneousBusRecord] {
        &self.instantaneous_records
    }

    fn push_instantaneous_record(&mut self, record: InstantaneousBusRecord) {
        if self.records.is_some() {
            self.instantaneous_records.push(record);
        }
    }

    fn demote_to_owner_only(&mut self) {
        self.records = None;
        self.instantaneous_records.clear();
    }

    fn annotate_at(&mut self, vpos: u32, hpos: u32, f: impl FnOnce(&mut BusSlotRecord)) {
        let (v, h) = (vpos as usize, hpos as usize);
        if v >= self.rows || h >= self.cols {
            return;
        }
        if let Some(record) = self
            .records
            .as_mut()
            .map(std::sync::Arc::make_mut)
            .and_then(|records| records.get_mut(v * self.cols + h))
        {
            f(record);
        }
    }

    /// The `n` instruction PCs that waited longest for the chip bus this
    /// frame, as `(pc, waited cck)`, longest first; ties order by PC so
    /// the readout is deterministic whatever the map's iteration order.
    pub fn top_stalled_pcs(&self, n: usize) -> Vec<(u32, u32)> {
        let mut pcs: Vec<(u32, u32)> = self
            .cpu_wait_pcs
            .iter()
            .map(|(&pc, &cck)| (pc, cck))
            .collect();
        pcs.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        pcs.truncate(n);
        pcs
    }

    /// Distinct stalled PCs recorded (before the cap's overflow pool).
    pub fn stalled_pc_count(&self) -> usize {
        self.cpu_wait_pcs.len()
    }

    pub fn has_samples(&self) -> bool {
        self.owner_cck.iter().any(|&v| v != 0)
    }
}

/// Per-display-frame chip-bus color-clock accounting. Gated behind
/// `COPPERLINE_DUMP_BUS_ACCOUNTING`; reports where the granted color clocks go
/// and how badly fixed DMA / Copper starve a busy blitter. See
/// docs/internals/timing.md.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct BusAccounting {
    enabled: bool,
    /// Color clocks attributed to each owner this display frame.
    owner_cck: [u64; 9],
    /// Color clocks during which the blitter was busy (whether or not it
    /// was granted the slot).
    blitter_busy_cck: u64,
    /// Of the busy color clocks, how many were taken by each non-blitter
    /// owner (the cycles that stretch a blit out past its granted time).
    blitter_starve_cck: [u64; 9],
    /// Blits started this frame, split line vs normal, with total scheduled
    /// slot (color-clock) cost. Measures whether the blit *workload* is
    /// inflated, independent of arbitration.
    blits_line: u64,
    blits_normal: u64,
    slots_line: u64,
    slots_normal: u64,
}

impl BusAccounting {
    fn from_env() -> Self {
        Self {
            enabled: crate::envcfg::flag("COPPERLINE_DUMP_BUS_ACCOUNTING"),
            ..Self::default()
        }
    }

    fn record_cck(&mut self, owner: ChipBusOwner, cck: u32, blitter_busy: bool) {
        let cck = cck as u64;
        let idx = owner.accounting_index();
        self.owner_cck[idx] += cck;
        if blitter_busy {
            self.blitter_busy_cck += cck;
            if !matches!(owner, ChipBusOwner::Blitter) {
                self.blitter_starve_cck[idx] += cck;
            }
        }
    }

    fn record_blit(&mut self, is_line: bool, slots: u32) {
        let slots = slots as u64;
        if is_line {
            self.blits_line += 1;
            self.slots_line += slots;
        } else {
            self.blits_normal += 1;
            self.slots_normal += slots;
        }
    }

    fn reset_frame(&mut self) {
        self.owner_cck = [0; 9];
        self.blitter_busy_cck = 0;
        self.blitter_starve_cck = [0; 9];
        self.blits_line = 0;
        self.blits_normal = 0;
        self.slots_line = 0;
        self.slots_normal = 0;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrontPanelStatus {
    /// The guest holds CIA-A's /LED line engaged: the PWR LED burns at
    /// full brightness, dimmed -- not extinguished -- when released, as
    /// on an A500 rev 6 or later board. The raw pin state, not the
    /// effective audio filter: the user's filter override changes the
    /// mix, never the LED.
    pub power_led_bright: bool,
    pub fdd_led_on: bool,
    pub fdd_track: Option<u8>,
    /// HDD activity LED: None on machines without an IDE port (no Gayle),
    /// Some(on) when the machine has one.
    pub hdd_led: Option<bool>,
    /// CD activity LED: None on machines without a CD drive, Some(on)
    /// while the drive is reading data or playing audio.
    pub cd_led: Option<bool>,
    /// CD track under the emulated optical head. `None` when no medium is
    /// mounted, including while a replacement disc is travelling in the
    /// tray.
    pub cd_track: Option<u8>,
    pub output_volume_percent: u8,
}

impl Default for FrontPanelStatus {
    fn default() -> Self {
        Self {
            power_led_bright: true,
            fdd_led_on: false,
            fdd_track: None,
            hdd_led: None,
            cd_led: None,
            cd_track: None,
            output_volume_percent: 100,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VideoPipelineStats {
    /// Sampling-gate evaluation counters. The Instant::now timing probes fire
    /// on 1 in SAMPLE_RATE evaluations of these counters, NOT of the *_calls
    /// counters below: *_calls only advance when a call did work, so gating on
    /// them parks the gate open across every no-op call in between (two clock
    /// reads per beam advance - a measurable share of host CPU).
    bitplane_fetch_probes: u64,
    sprite_fetch_probes: u64,
    collision_probes: u64,
    pub bitplane_fetch_calls: u64,
    pub bitplane_fetch_slots: u64,
    pub bitplane_fetch_rows_started: u64,
    pub bitplane_fetch_rows_completed: u64,
    pub bitplane_fetch_nanos: u128,
    pub sprite_fetch_calls: u64,
    pub sprite_fetch_pair_slots: u64,
    pub sprite_fetch_lines: u64,
    pub sprite_fetch_nanos: u128,
    pub collision_calls: u64,
    pub collision_pixels: u64,
    pub collision_control_segments: u64,
    pub collision_full_line_scans: u64,
    pub collision_nanos: u128,
    pub render_frames: u64,
    pub render_events: u64,
    pub render_control_segments: u64,
    pub render_playfield_pixels: u64,
    pub render_manual_bpl_segments: u64,
    pub render_sprite_lines: u64,
    pub render_total_nanos: u128,
    pub render_event_nanos: u128,
    pub render_background_nanos: u128,
    pub render_playfield_nanos: u128,
    pub render_manual_bpl_nanos: u128,
    pub render_sprite_nanos: u128,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct VideoRenderFrameTiming {
    pub events: u64,
    pub control_segments: u64,
    pub playfield_pixels: u64,
    pub manual_bpl_segments: u64,
    pub sprite_lines: u64,
    pub total_nanos: u128,
    pub event_nanos: u128,
    pub background_nanos: u128,
    pub playfield_nanos: u128,
    pub manual_bpl_nanos: u128,
    pub sprite_nanos: u128,
}

impl VideoPipelineStats {
    /// Advance a probe counter and start a timing sample on 1 in `rate`
    /// evaluations. The recorded duration is later scaled back up by `rate`
    /// in `add_sampled_duration`, so the estimate stays unbiased as long as
    /// every potential sample point calls this exactly once.
    #[inline(always)]
    fn probe_timing_sample(probes: &mut u64, rate: u128) -> Option<Instant> {
        #[cfg(feature = "profile-stats")]
        {
            let due = probes.is_multiple_of(rate as u64);
            *probes = probes.wrapping_add(1);
            due.then(Instant::now)
        }
        #[cfg(not(feature = "profile-stats"))]
        {
            let _ = (probes, rate);
            None
        }
    }

    #[cfg(feature = "profile-stats")]
    fn add_sampled_duration(total: &mut u128, elapsed: Option<(Duration, u128)>) {
        if let Some((elapsed, sample_rate)) = elapsed {
            *total = total.saturating_add(elapsed.as_nanos().saturating_mul(sample_rate));
        }
    }

    fn millis(nanos: u128) -> f64 {
        nanos as f64 / 1_000_000.0
    }

    fn nanos_per_item(nanos: u128, items: u64) -> f64 {
        if items == 0 {
            0.0
        } else {
            nanos as f64 / items as f64
        }
    }

    pub fn dump(&self, label: &str) {
        if self.bitplane_fetch_calls
            + self.sprite_fetch_calls
            + self.collision_calls
            + self.render_frames
            == 0
        {
            return;
        }
        log::info!(
            "video pipeline stats ({label}): bitplane_fetch calls={} slots={} rows_started={} rows_completed={} time={:.3}ms, sprite_fetch calls={} pair_slots={} lines={} time={:.3}ms",
            self.bitplane_fetch_calls,
            self.bitplane_fetch_slots,
            self.bitplane_fetch_rows_started,
            self.bitplane_fetch_rows_completed,
            Self::millis(self.bitplane_fetch_nanos),
            self.sprite_fetch_calls,
            self.sprite_fetch_pair_slots,
            self.sprite_fetch_lines,
            Self::millis(self.sprite_fetch_nanos),
        );
        log::info!(
            "video pipeline stats ({label}): collisions calls={} pixels={} full_line_scans={} control_segments={} time={:.3}ms avg={:.1}ns/pixel",
            self.collision_calls,
            self.collision_pixels,
            self.collision_full_line_scans,
            self.collision_control_segments,
            Self::millis(self.collision_nanos),
            Self::nanos_per_item(self.collision_nanos, self.collision_pixels),
        );
        log::info!(
            "video pipeline stats ({label}): render frames={} events={} control_segments={} playfield_pixels={} manual_bpl_segments={} sprite_lines={} total={:.3}ms phases(events={:.3}, background={:.3}, playfield={:.3}, manual_bpl={:.3}, sprites={:.3})",
            self.render_frames,
            self.render_events,
            self.render_control_segments,
            self.render_playfield_pixels,
            self.render_manual_bpl_segments,
            self.render_sprite_lines,
            Self::millis(self.render_total_nanos),
            Self::millis(self.render_event_nanos),
            Self::millis(self.render_background_nanos),
            Self::millis(self.render_playfield_nanos),
            Self::millis(self.render_manual_bpl_nanos),
            Self::millis(self.render_sprite_nanos),
        );
    }
}

/// The kind of controller plugged into an Amiga game port. Selects how the
/// port's pins are driven: which JOYxDAT encoding the port reports, what
/// /FIRx and POTxX/POTxY carry, and whether the CD32 pad's serial button
/// shifter is present. Either port accepts any device, as on real hardware.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PortDevice {
    /// Quadrature mouse: JOYxDAT reports the movement counters, left button
    /// on /FIRx, right on POTxY, middle on POTxX.
    #[default]
    Mouse,
    /// Digital switch joystick: directions encode into JOYxDAT, fire on
    /// /FIRx, button 2 grounds POTxY.
    Joystick,
    /// CD32 joypad: a digital joystick plus the serial button shifter
    /// (POTxX driven low selects shift mode, /FIRx clocks, POTxY is data).
    Cd32Pad,
    /// Analogue paddles / proportional stick: positions present resistances
    /// on POTxX/POTxY; the two buttons ground the LEFT/RIGHT lines.
    Analogue,
    /// Empty port: nothing drives the pins. Reads like an idle mouse (the
    /// JOYxDAT counters hold, the pot pins float).
    None,
    /// A quadrature mouse, driven by a gamepad as well as by the host's
    /// own mouse. Electrically this *is* [`PortDevice::Mouse`] -- the
    /// machine is given one mouse with two hands on it, not two mice --
    /// and it differs only in where the host looks for movement: the
    /// pad's d-pad and left stick move the pointer and its two buttons
    /// click, and no joystick port hears from that pad while it does.
    /// Offered on port 1 alone, which is the port a mouse belongs in.
    ///
    /// Last on purpose: a save state encodes a variant by its position,
    /// so a state written before this existed must still read an empty
    /// port as empty. The pickers order themselves.
    GamepadMouse,
}

impl PortDevice {
    /// Canonical configuration name, as accepted by `[input] port1/port2`.
    pub fn label(self) -> &'static str {
        match self {
            PortDevice::Mouse => "mouse",
            PortDevice::Joystick => "joystick",
            PortDevice::Cd32Pad => "cd32",
            PortDevice::Analogue => "analogue",
            PortDevice::GamepadMouse => "gamepad-mouse",
            PortDevice::None => "none",
        }
    }

    /// Whether this port presents a quadrature mouse to the machine.
    /// True of both mouse devices: they differ in what moves them on the
    /// host, not in anything the Amiga can tell apart.
    pub fn is_mouse(self) -> bool {
        matches!(self, PortDevice::Mouse | PortDevice::GamepadMouse)
    }

    /// What a picker shows the user, as against the config name [`label`]
    /// round-trips. Both pickers read this, so they cannot drift apart.
    ///
    /// [`label`]: PortDevice::label
    pub fn menu_label(self) -> &'static str {
        match self {
            PortDevice::Mouse => "Mouse",
            PortDevice::Joystick => "Joystick",
            PortDevice::Cd32Pad => "CD32 Pad",
            PortDevice::Analogue => "Analogue",
            PortDevice::GamepadMouse => "Gamepad Mouse",
            PortDevice::None => "None",
        }
    }

    /// Parse a configuration name (canonical or alias, case-insensitive).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "mouse" => Some(PortDevice::Mouse),
            "joystick" | "joy" => Some(PortDevice::Joystick),
            "cd32" | "cd32pad" | "pad" => Some(PortDevice::Cd32Pad),
            "analogue" | "analog" | "paddle" => Some(PortDevice::Analogue),
            "gamepad-mouse" | "gamepad_mouse" | "padmouse" => Some(PortDevice::GamepadMouse),
            "none" | "off" => Some(PortDevice::None),
            _ => None,
        }
    }
}

/// One Amiga game port: the device plugged into it plus the state of every
/// line a controller can drive. The JOYxDAT counters are chip-side state, so
/// they survive device changes and JOYTEST loads them whatever is plugged in.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct ControllerPort {
    pub device: PortDevice,
    /// JOYxDAT horizontal/vertical quadrature counters.
    pub counter_x: u8,
    pub counter_y: u8,
    /// Primary button line /FIRx (CIA-A PRA bit 6/7 reports it active-low):
    /// left mouse button / joystick fire / CD32 Red.
    pub fire: bool,
    /// POTxY switch to ground: right mouse button / joystick button 2 /
    /// CD32 Blue.
    pub button2: bool,
    /// POTxX switch to ground: middle mouse button (third button).
    pub button3: bool,
    /// Direction switch lines. On an Analogue device the left/right lines
    /// double as the two paddle buttons (HRM paddle wiring).
    pub up: bool,
    pub down: bool,
    pub left: bool,
    pub right: bool,
    /// CD32 buttons that exist only in the serial report (Red rides `fire`,
    /// Blue rides `button2`).
    pub cd32_play: bool,
    pub cd32_rwd: bool,
    pub cd32_ffw: bool,
    pub cd32_green: bool,
    pub cd32_yellow: bool,
    /// CD32 serial shift position: starts at 8 (Blue), decremented by /FIRx
    /// falling edges the CPU drives while the pad is in serial mode, 1
    /// returns the pad-present bit, 0 returns zeros. Reset to 8 whenever
    /// serial mode is left.
    pub cd32_shifter: i8,
    /// Previous CPU-driven /FIRx output level (DDR and PRA), the shift-clock
    /// falling-edge detector's memory.
    pub cd32_fire_driven_high: bool,
    /// Analogue resistance from +5 V to POTxX/POTxY in ohms; `None` leaves
    /// the pin floating. Values above Paula's specified maximum are clamped
    /// by the converter.
    pub pot_x_ohms: Option<u32>,
    pub pot_y_ohms: Option<u32>,
}

impl Default for ControllerPort {
    fn default() -> Self {
        Self {
            device: PortDevice::Mouse,
            counter_x: 0,
            counter_y: 0,
            fire: false,
            button2: false,
            button3: false,
            up: false,
            down: false,
            left: false,
            right: false,
            cd32_play: false,
            cd32_rwd: false,
            cd32_ffw: false,
            cd32_green: false,
            cd32_yellow: false,
            cd32_shifter: 8,
            cd32_fire_driven_high: false,
            pot_x_ohms: None,
            pot_y_ohms: None,
        }
    }
}

impl ControllerPort {
    /// The JOYxDAT word this port's device presents.
    pub fn joydat(&self) -> u16 {
        match self.device {
            PortDevice::Mouse | PortDevice::GamepadMouse | PortDevice::None => {
                mouse_joydat(self.counter_x, self.counter_y)
            }
            PortDevice::Joystick | PortDevice::Cd32Pad => {
                digital_joydat(self.up, self.down, self.left, self.right)
            }
            PortDevice::Analogue => {
                // Paddle buttons are switches on the LEFT/RIGHT lines, which
                // read back directly as JOYxDAT bits 9 and 1 on top of the
                // idle counters.
                let mut v = mouse_joydat(self.counter_x, self.counter_y);
                if self.left {
                    v |= 0x0200;
                }
                if self.right {
                    v |= 0x0002;
                }
                v
            }
        }
    }

    /// The /FIRx line pulled to ground (CIA-A reports it active-low).
    pub fn fir_asserted(&self) -> bool {
        self.fire
    }

    /// POTxX pin not switched to ground (the polarity `PotPins` carries).
    pub fn pot_x_released(&self) -> bool {
        !self.button3
    }

    /// POTxY pin not switched to ground.
    pub fn pot_y_released(&self) -> bool {
        !self.button2
    }
}

#[derive(Default, Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct InputState {
    /// Index 0 = port 1 (JOY0DAT/POT0/FIR0), 1 = port 2 (JOY1DAT/POT1/FIR1).
    pub ports: [ControllerPort; 2],
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct DeviceClock {
    realtime_enabled: bool,
    // Wall-clock instant corresponding to the current device timeline.
    // CPU chip-bus slots push this into the future; hardware barriers
    // wait for it so chip RAM/custom access cannot outrun real time.
    // Not part of a save state: the loader re-anchors to the host clock.
    #[serde(skip)]
    realtime_anchor: Option<Instant>,
    realtime_cck_remainder: u128,
    realtime_delay_remainder: u128,
    cia_tick_remainder_cck: u32,
}

impl DeviceClock {
    fn set_realtime_enabled(&mut self, enabled: bool) {
        self.realtime_enabled = enabled;
        self.reset();
    }

    fn reset(&mut self) {
        self.realtime_anchor = self.realtime_enabled.then(Instant::now);
        self.realtime_cck_remainder = 0;
        self.realtime_delay_remainder = 0;
        self.cia_tick_remainder_cck = 0;
    }

    fn wait_duration(&self, now: Instant) -> Option<Duration> {
        if !self.realtime_enabled {
            return None;
        }
        let anchor = self.realtime_anchor?;
        (anchor > now).then(|| anchor.duration_since(now))
    }

    fn realtime_cck_due(&mut self, now: Instant) -> u32 {
        if !self.realtime_enabled {
            return 0;
        }
        let Some(anchor) = self.realtime_anchor else {
            self.realtime_anchor = Some(now);
            return 0;
        };
        if now <= anchor {
            return 0;
        }

        let elapsed = now.duration_since(anchor);
        let total = self
            .realtime_cck_remainder
            .saturating_add(elapsed.as_nanos().saturating_mul(PAULA_CLOCK_HZ as u128));
        self.realtime_cck_remainder = total % NANOS_PER_SECOND;
        self.realtime_anchor = Some(now);
        (total / NANOS_PER_SECOND).min(u32::MAX as u128) as u32
    }

    fn note_realtime_device_advance(&mut self, cck: u32) {
        if !self.realtime_enabled || cck == 0 {
            return;
        }
        let anchor = self.realtime_anchor.unwrap_or_else(Instant::now);
        let total = self
            .realtime_delay_remainder
            .saturating_add(cck as u128 * NANOS_PER_SECOND);
        let nanos = total / PAULA_CLOCK_HZ as u128;
        self.realtime_delay_remainder = total % PAULA_CLOCK_HZ as u128;
        let delay = Duration::from_nanos(nanos.min(u64::MAX as u128) as u64);
        self.realtime_anchor = Some(anchor + delay);
    }

    fn cia_ticks_for_cck(&mut self, cck: u32) -> u32 {
        let total = self.cia_tick_remainder_cck + cck;
        self.cia_tick_remainder_cck = total % 5;
        total / 5
    }
}

impl InputState {
    /// Port argument convention across the input API: 0 selects port 1,
    /// every other value port 2.
    fn port_index(port: usize) -> usize {
        usize::from(port != 0)
    }

    /// The device currently plugged into a port.
    pub fn device(&self, port: usize) -> PortDevice {
        self.ports[Self::port_index(port)].device
    }

    /// Change the device plugged into a port. Unplugging releases every line
    /// the old device drove; the JOYxDAT counters are chip-side and hold
    /// their values. A freshly plugged analogue controller presents its pots
    /// centred: a real paddle always shows some resistance.
    pub fn set_port_device(&mut self, port: usize, device: PortDevice) {
        let p = &mut self.ports[Self::port_index(port)];
        if p.device == device {
            return;
        }
        *p = ControllerPort {
            device,
            counter_x: p.counter_x,
            counter_y: p.counter_y,
            ..ControllerPort::default()
        };
        if device == PortDevice::Analogue {
            let centre = crate::chipset::paula::pot_position_resistance_ohms(128);
            p.pot_x_ohms = Some(centre);
            p.pot_y_ohms = Some(centre);
        }
    }

    /// Accumulate mouse quadrature movement into a port's JOYxDAT counters.
    pub fn add_mouse_delta(&mut self, port: usize, dx: i32, dy: i32) {
        let p = &mut self.ports[Self::port_index(port)];
        p.counter_x = p.counter_x.wrapping_add(dx as u8);
        p.counter_y = p.counter_y.wrapping_add(dy as u8);
    }

    /// Mouse buttons by index: 0 = left (/FIRx), 1 = right (POTxY),
    /// 2 = middle (POTxX).
    pub fn set_mouse_button(&mut self, port: usize, index: u8, pressed: bool) {
        let p = &mut self.ports[Self::port_index(port)];
        match index {
            0 => p.fire = pressed,
            1 => p.button2 = pressed,
            _ => p.button3 = pressed,
        }
    }

    /// Set a port's digital-joystick state. Engages the Joystick device on a
    /// Mouse or empty port so JOYxDAT reports directions; a CD32 pad or
    /// analogue controller keeps its device and just has its lines driven --
    /// fire is /FIRx, button 2 grounds POTxY, and the direction switch lines
    /// double as the paddle buttons on an analogue device.
    pub fn set_joystick(
        &mut self,
        port: usize,
        up: bool,
        down: bool,
        left: bool,
        right: bool,
        fire: bool,
        button2: bool,
    ) {
        let idx = Self::port_index(port);
        if self.ports[idx].device.is_mouse() || self.ports[idx].device == PortDevice::None {
            self.set_port_device(port, PortDevice::Joystick);
        }
        let p = &mut self.ports[idx];
        p.up = up;
        p.down = down;
        p.left = left;
        p.right = right;
        p.fire = fire;
        p.button2 = button2;
    }

    /// Set a CD32 joypad's extra buttons. Red and Blue arrive through
    /// `set_joystick` as fire/button2; these five only exist in the pad's
    /// serial report.
    pub fn set_cd32_buttons(
        &mut self,
        port: usize,
        play: bool,
        rwd: bool,
        ffw: bool,
        green: bool,
        yellow: bool,
    ) {
        let p = &mut self.ports[Self::port_index(port)];
        p.cd32_play = play;
        p.cd32_rwd = rwd;
        p.cd32_ffw = ffw;
        p.cd32_green = green;
        p.cd32_yellow = yellow;
    }

    /// Set an analogue controller's stick/paddle position, 0..=255 per axis:
    /// the count the port's POTxDAT byte latches after a POTGO scan. Engages
    /// the Analogue device.
    pub fn set_analogue(&mut self, port: usize, x: u8, y: u8) {
        let idx = Self::port_index(port);
        if self.ports[idx].device != PortDevice::Analogue {
            self.set_port_device(port, PortDevice::Analogue);
        }
        let p = &mut self.ports[idx];
        p.pot_x_ohms = Some(crate::chipset::paula::pot_position_resistance_ohms(x));
        p.pot_y_ohms = Some(crate::chipset::paula::pot_position_resistance_ohms(y));
    }

    /// Connect an analogue paddle pair to a controller port by raw
    /// resistance. Each value is measured from +5 V to the corresponding POT
    /// pin; `None` disconnects that axis. Does not change the port's device.
    pub fn set_paddle_resistance(&mut self, port: usize, x_ohms: Option<u32>, y_ohms: Option<u32>) {
        let p = &mut self.ports[Self::port_index(port)];
        p.pot_x_ohms = x_ohms;
        p.pot_y_ohms = y_ohms;
    }

    /// JOYTEST loads both ports' quadrature counters with the same value,
    /// whatever devices are plugged in.
    pub fn write_joytest(&mut self, val: u16) {
        for p in &mut self.ports {
            p.counter_y = (val >> 8) as u8;
            p.counter_x = val as u8;
        }
    }

    /// The JOYxDAT word for a port (0 = JOY0DAT, other = JOY1DAT).
    pub fn joydat(&self, port: usize) -> u16 {
        self.ports[Self::port_index(port)].joydat()
    }

    /// A machine reset: the chips reset but nothing is unplugged. Each
    /// port keeps its device and analogue knob positions (physical
    /// state), while the chip-side quadrature counters clear, the pad
    /// shifters reload, and the driven lines release -- the host input
    /// path re-asserts anything still physically held on the next
    /// quantum.
    pub fn reset_for_machine_reset(&mut self) {
        for p in &mut self.ports {
            *p = ControllerPort {
                device: p.device,
                pot_x_ohms: p.pot_x_ohms,
                pot_y_ohms: p.pot_y_ohms,
                ..ControllerPort::default()
            };
        }
    }
}

fn mouse_joydat(x: u8, y: u8) -> u16 {
    ((y as u16) << 8) | x as u16
}

/// Encode digital-joystick directions into a JOYxDAT word so the Amiga's
/// documented decode recovers them. Per the Hardware Reference Manual:
///   right = bit1, down = bit1 ^ bit0, left = bit9, up = bit9 ^ bit8.
/// Note left/up live in the high (vertical-counter) byte and right/down in
/// the low (horizontal-counter) byte -- the axes are not split the obvious
/// way, which is a real hardware quirk of how the switches drive the
/// quadrature counters.
fn digital_joydat(up: bool, down: bool, left: bool, right: bool) -> u16 {
    let mut v = 0u16;
    if right {
        v |= 0x0002; // bit 1
    }
    if right ^ down {
        v |= 0x0001; // bit 0  -> down decodes as bit1 ^ bit0
    }
    if left {
        v |= 0x0200; // bit 9
    }
    if left ^ up {
        v |= 0x0100; // bit 8  -> up decodes as bit9 ^ bit8
    }
    v
}

/// Counts reads of each MMIO register so we can see what DiagROM is
/// busy-polling. Dumped on Bus::drop via the `poll-stats` log target.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct PollStats {
    pub cia_a: [u64; 16],
    pub cia_b: [u64; 16],
    /// Read counts per custom register, indexed by `offset >> 1` (registers
    /// are word-aligned, offsets $000..=$FFE). A flat table rather than a
    /// `HashMap`: `tick_read_custom` is hit on every $DFF000 register read,
    /// which Kickstart busy-polls hard, so the default-SipHash probe showed
    /// up on the hot path.
    pub custom: Vec<u64>,
}

impl Default for PollStats {
    fn default() -> Self {
        Self {
            cia_a: [0; 16],
            cia_b: [0; 16],
            custom: vec![0; 0x800],
        }
    }
}

impl PollStats {
    #[inline(always)]
    pub fn tick_read(&mut self, which: &str, reg: usize) {
        #[cfg(feature = "profile-stats")]
        {
            match which {
                "cia_a" => self.cia_a[reg & 0xF] += 1,
                "cia_b" => self.cia_b[reg & 0xF] += 1,
                _ => {}
            }
        }
        #[cfg(not(feature = "profile-stats"))]
        {
            let _ = (which, reg);
        }
    }

    #[inline(always)]
    pub fn tick_read_custom(&mut self, off: u16) {
        #[cfg(feature = "profile-stats")]
        {
            if let Some(count) = self.custom.get_mut((off >> 1) as usize) {
                *count += 1;
            }
        }
        #[cfg(not(feature = "profile-stats"))]
        {
            let _ = off;
        }
    }
    /// Dump the busiest polled registers, for working out what a stuck
    /// guest is spinning on. Opt-in via `COPPERLINE_DIAG_POLLSTATS`: this
    /// runs at every screenshot and frame dump, and headless capture is
    /// the everyday workflow, so unconditional output would bury a normal
    /// run's log in emulator-internal counters.
    pub fn dump_top(&self, label: &str) {
        if !crate::envcfg::flag("COPPERLINE_DIAG_POLLSTATS") {
            return;
        }
        log::info!("== poll stats ({}) ==", label);
        for (i, &n) in self.cia_a.iter().enumerate() {
            if n > 0 {
                log::info!("  cia_a reg ${:X}: {}", i, n);
            }
        }
        for (i, &n) in self.cia_b.iter().enumerate() {
            if n > 0 {
                log::info!("  cia_b reg ${:X}: {}", i, n);
            }
        }
        let mut customs: Vec<(u16, u64)> = self
            .custom
            .iter()
            .enumerate()
            .filter(|(_, &n)| n > 0)
            .map(|(idx, &n)| ((idx as u16) << 1, n))
            .collect();
        customs.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
        for (off, n) in customs.iter().take(20) {
            log::info!("  custom ${:03X}: {}", off, n);
        }
    }
}

fn fixed_standard_frame_lines(video_standard: VideoStandard, lace: bool, long_field: bool) -> u32 {
    let long_lines = match video_standard {
        VideoStandard::Pal => PAL_LINES,
        VideoStandard::Ntsc => NTSC_LINES,
    };
    if lace && !long_field {
        long_lines.saturating_sub(1)
    } else {
        long_lines
    }
}

impl Bus {
    pub fn new(mem: Memory, paula: Paula, floppy: FloppyController) -> Self {
        let current_frame_chip_ram = mem.chip_ram.clone();
        let blitter_trace = crate::envcfg::var_os("COPPERLINE_TRACE_BLITTER").and_then(|path| {
            match std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&path)
            {
                Ok(file) => Some(file),
                Err(err) => {
                    log::warn!(
                        "could not open COPPERLINE_TRACE_BLITTER path {}: {err}",
                        std::path::Path::new(&path).display()
                    );
                    None
                }
            }
        });

        let mut bus = Self {
            mem,
            ram_init: RamInit::Zero,
            cia_a: Cia::new(Which::A),
            cia_b: Cia::new(Which::B),
            paula,
            agnus: Agnus::new(),
            copper: Copper::new(),
            denise: Denise::new(),
            denise_revision: DeniseRevision::Ocs,
            chip_dma_mask: 0x0007_FFFF,
            blitter: Blitter::new(),
            floppy,
            parallel_port: crate::parallel::null_parallel_port(),
            rtc: Rtc::default(),
            rtc_present: true,
            gayle: None,
            ramsey: None,
            gary: None,
            sdmac: None,
            ide_a4000: None,
            uaelib: None,
            cartridge: None,
            log_unmapped: None,
            akiko: None,
            cdtv: None,
            devices: Vec::new(),
            hdd_led_until_cck: 0,
            bplcon3_drop_warned: false,
            cpu_fetch_latch: [None; 2],
            overlay_disable_pending: false,
            keyboard: KeyboardMcu::new(),
            keyboard_system_reset_pending: false,
            input: InputState::default(),
            poll_stats: PollStats::default(),
            video_pipeline_stats: VideoPipelineStats::default(),
            slice_preempted: false,
            diag_lowmem_blit: false,
            pending_vbi: 0,
            pending_copper_frame_start: None,
            copper_active_in_frame: false,
            copper_current_list: 1,
            delivered_irq_pending: 0,
            pending_copper_irq_beam: None,
            delivered_copper_irq_beam: None,
            coper_cpu_irq_delay_cck: 0,
            irq_latency_visible_at: [0; 16],
            irq_latency_mask: 0,
            irq_latency_last_pending: 0,
            irq_latency_setting: irq_latency_setting_from_env(),
            beam_top_palette: Palette::new(),
            beam_bottom_palette: Palette::new(),
            beam_bottom_palette_valid: false,
            cpu_palette_target: CpuPaletteTarget::Top,
            cpu_palette_target_writes: 0,
            cpu_palette_target_beam: None,
            current_frame_render_base: RenderRegisterSnapshot::default(),
            last_frame_render_base: None,
            current_frame_render_events: Vec::new(),
            current_frame_collision_events: Vec::new(),
            current_frame_collision_control_events: Vec::new(),
            current_frame_collision_bpldat_events: Vec::new(),
            current_frame_collision_sprite_events: Vec::new(),
            current_frame_collision_control_index: None,
            current_frame_collision_bpldat_index: None,
            current_frame_collision_sprite_index: None,
            current_frame_collision_may_have_dual_playfield: false,
            last_frame_render_events: Vec::new(),
            beam_bottom_palette_events: Vec::new(),
            pending_beam_bottom_palette_events: Vec::new(),
            last_frame_beam_bottom_palette_events: Vec::new(),
            current_frame_beam_top_palette: Palette::new(),
            last_frame_beam_top_palette: Palette::new(),
            last_frame_beam_top_palette_end: Palette::new(),
            last_frame_beam_bottom_palette: Palette::new(),
            last_frame_beam_bottom_palette_valid: false,
            current_frame_chip_ram,
            last_frame_chip_ram: std::sync::Arc::new(Vec::new()),
            current_frame_chip_ram_writes: Vec::new(),
            last_frame_chip_ram_writes: Vec::new(),
            current_frame_bitplane_rows: empty_captured_bitplane_rows(),
            last_frame_bitplane_rows: std::sync::Arc::new(empty_captured_bitplane_rows()),
            bitplane_row_pool: Vec::with_capacity(MAX_VISIBLE_LINES),
            current_frame_sprite_lines: Vec::new(),
            current_frame_sprite_lines_by_y: empty_captured_sprite_lines_by_y(),
            current_frame_sprite_collision_sources: empty_sprite_collision_sources(),
            last_frame_sprite_lines: Vec::new(),
            current_frame_held_sprites: [None; 8],
            last_frame_held_sprites: [None; 8],
            current_frame_sprite_display_enable_x_by_y: empty_sprite_display_enable_x_by_y(),
            last_frame_sprite_display_enable_x_by_y: empty_sprite_display_enable_x_by_y(),
            current_frame_sprite_dma_observed: false,
            last_frame_sprite_dma_observed: false,
            current_frame_display_snapshot_taken: false,
            ocs_same_line_diw_start_blocked_vpos: None,
            diw_vertical_open: None,
            current_frame_render_blocked: false,
            current_frame_visible_start_vpos: RENDER_VISIBLE_START_VPOS,
            last_frame_visible_start_vpos: RENDER_VISIBLE_START_VPOS,
            current_frame_geometry: FrameGeometry::standard(
                RENDER_VISIBLE_START_VPOS,
                PAL_LINES,
                false,
            ),
            current_frame_presentation_h_window: None,
            last_frame_presentation_h_window: None,
            current_frame_presentation_v_window: None,
            last_frame_presentation_v_window: None,
            last_frame_geometry: FrameGeometry::standard(
                RENDER_VISIBLE_START_VPOS,
                PAL_LINES,
                false,
            ),
            lazy_collision_vpos: RENDER_VISIBLE_START_VPOS,
            lazy_collision_hpos: RENDER_COPPER_WAIT_HPOS_FB0,
            collision_tracking_active: false,
            cpu_bus_arbitration_enabled: false,
            cpu_clock_carry: 0,
            cpu_clocks_per_cck: 2,
            ext_clock_carry_x100: 0,
            cpu_short_bus_cycle: false,
            cpu_access_phase_sync: false,
            cpu_posted_write_debt: 0,
            cpu_chip_port_free_at: 0,
            cpu_chip_clock_phase: 0,
            cpu_bus_overlap_clocks: 0,
            cpu_custom_access_slot: None,
            cpu_custom_request_slot: None,
            cpu_granted_chip_slots: 0,
            cpu_missed_chip_slots: 0,
            cpu_wait_cck_total: 0,
            dbg_bpl_cck: vec![0; 340],
            dbg_slotmap: Vec::new(),
            dbg_slotmap_on: crate::envcfg::flag("COPPERLINE_DIAG_SLOTMAP"),
            dbg_slotmap_dumped: false,
            frame_analyzer_enabled: false,
            frame_analyzer_full: false,
            cpu_bus_wait: None,
            cpu_trace_access: None,
            cpu_trace_slots: Vec::new(),
            cia_trace_phase: 0,
            cia_trace_slot: None,
            trace_cia_irq_pins: (false, false),
            trace_intreq: 0,
            current_frame_bus_trace: FrameBusTrace::default(),
            last_frame_bus_trace: None,
            active_frame_blit: None,
            next_frame_blit_id: 0,
            bus_event_observers: 0,
            bus_events: std::collections::VecDeque::new(),
            bus_event_next_sequence: 0,
            wave_on: false,
            chip_bus_observers_on: false,
            wave_pc_trigger: false,
            wave: None,
            ui_reg_watches: Vec::new(),
            ui_reg_hit: None,
            ui_mem_reads: Vec::new(),
            cpu_pc: 0,
            regcheck: None,
            reg_writers: None,
            smc: None,
            injected_faults: Vec::new(),
            heatmap: None,
            ui_beam_traps: Vec::new(),
            ui_beam_hit: None,
            ui_copper_breaks: Vec::new(),
            ui_copper_hit: None,
            ui_copper_last_pc: 0,
            ui_layer_masks: UiLayerMasks::default(),
            ui_mem_watch_addrs: Vec::new(),
            ui_mem_writers: Vec::new(),
            blitter_slowdown_cpu_misses: 0,
            blit_irq_delay_cck: None,
            slice_bus_advanced_cck: 0,
            slice_bus_tick: AgnusTick::default(),
            pending_device_cck: 0,
            pending_device_tick: AgnusTick::default(),
            devices_wrote_memory: false,
            pending_device_trace_spans: Vec::new(),
            audio_pending_cck: 0,
            last_chip_bus_owner: ChipBusOwner::Idle,
            data_bus: 0,
            device_clock: DeviceClock::default(),
            emulated_cck: 0,
            emulated_frames: 0,
            blitter_trace,
            display_dma_bplpt: [0; 8],
            display_dma_sprpt: [0; 8],
            sprite_dma_frame_start_ptr: [0; 8],
            sprite_dma_replay: SpriteDmaReplayCursor::default(),
            display_dma_sprite_state: [DisplaySpriteDmaState::default(); 8],
            display_dma_clipped_rows_advanced: false,
            bitplane_dmacon_delay: None,
            bitplane_bplcon0_delay: None,
            bitplane_ddfstart_miss: None,
            bitplane_slot_plan_cache: BitplaneSlotPlanCache::new(),
            wide_bitplane_hot_line: WideBitplaneHotLine::new(),
            wide_bitplane_dynamic_vpos: std::cell::Cell::new(None),
            ddf_seq_line_initial: std::cell::Cell::new(Default::default()),
            ddf_seq_line_start_regs: std::cell::Cell::new((0, 0)),
            ddf_seq_line_start_ctl: std::cell::Cell::new((false, 0)),
            ddf_seq_writes: std::cell::RefCell::new(Vec::new()),
            ddf_seq_line: std::cell::RefCell::new(None),
            ddf_seq_hot_line: ddf_line::DdfSeqHotLine::new(),
            bus_accounting: BusAccounting::from_env(),
            uhres_dual_warned: false,
            dbg_ext_cck_x100: external_access_cck_x100_setting(),
        };
        bus.refresh_chip_bus_observers();
        bus.configure_chip_dma_masks();
        // Re-derive the per-frame capture buffers from the same helper the
        // reset paths use, rather than trusting the struct literal above to
        // stay in lockstep with it by hand.
        bus.reset_frame_capture_buffers();
        bus
    }

    pub fn rtc_present(&self) -> bool {
        self.rtc_present
    }

    /// Arm or disarm the custom-register access validator and the
    /// last-writer table. Disarming drops the report: it describes a
    /// window that is over.
    pub fn set_chipset_validation(&mut self, on: bool) {
        if on == self.regcheck.is_some() {
            return;
        }
        if on {
            self.regcheck = Some(Box::default());
            self.reg_writers = Some(Box::new([None; 256]));
        } else {
            self.regcheck = None;
            self.reg_writers = None;
        }
    }

    /// Arm a bus fault over an address window. Returns its index, which
    /// is stable until the list is cleared.
    pub fn inject_bus_fault(&mut self, fault: FaultInjection) -> usize {
        self.injected_faults.push(fault);
        self.injected_faults.len() - 1
    }

    pub fn injected_bus_faults(&self) -> &[FaultInjection] {
        &self.injected_faults
    }

    pub fn clear_injected_bus_faults(&mut self) {
        self.injected_faults.clear();
    }

    /// Whether a CPU access of `len` bytes at `addr` should take an
    /// injected bus error, consuming one of a counted injection's shots.
    ///
    /// Kept behind an emptiness check by every caller so an un-armed
    /// machine pays a single branch per access.
    pub(crate) fn take_injected_fault(&mut self, addr: u32, len: u32, write: bool) -> bool {
        let last = addr.wrapping_add(len.max(1)).wrapping_sub(1);
        for fault in &mut self.injected_faults {
            if write && !fault.on_write || !write && !fault.on_read {
                continue;
            }
            if last < fault.start || addr > fault.end {
                continue;
            }
            if let Some(remaining) = fault.remaining.as_mut() {
                if *remaining == 0 {
                    continue;
                }
                *remaining -= 1;
            }
            fault.hits += 1;
            return true;
        }
        false
    }

    pub(crate) fn bus_faults_armed(&self) -> bool {
        !self.injected_faults.is_empty()
    }

    /// Arm or disarm the self-modifying-code detector. Disarming frees
    /// the execution map; re-arming starts from a blank one, so a report
    /// only ever covers the window it was armed for.
    pub fn set_smc_detection(&mut self, on: bool) {
        if on == self.smc.is_some() {
            return;
        }
        self.smc = on.then(Box::default);
    }

    pub fn smc_detection_armed(&self) -> bool {
        self.smc.is_some()
    }

    /// The self-modifying-code reports, most-repeated first, and how many
    /// were dropped once the report filled.
    pub fn smc_reports(&self) -> (Vec<crate::smc::SmcReport>, u64) {
        match &self.smc {
            Some(smc) => (smc.reports(), smc.dropped),
            None => (Vec::new(), 0),
        }
    }

    pub fn clear_smc_reports(&mut self) {
        if let Some(smc) = self.smc.as_mut() {
            smc.clear_reports();
        }
    }

    pub fn chipset_validation_armed(&self) -> bool {
        self.regcheck.is_some()
    }

    /// The validator's findings, most-repeated first, and how many were
    /// dropped because the report was full.
    pub fn chipset_findings(&self) -> (Vec<crate::regcheck::Report>, u64) {
        match &self.regcheck {
            Some(check) => (check.reports(), check.dropped),
            None => (Vec::new(), 0),
        }
    }

    pub fn clear_chipset_findings(&mut self) {
        if let Some(check) = self.regcheck.as_mut() {
            check.clear();
        }
    }

    /// The last write to custom register `off`, if the last-writer table
    /// is armed and the register has been written since.
    pub fn custom_reg_last_write(&self, off: u16) -> Option<RegWrite> {
        self.reg_writers.as_ref()?[usize::from((off & 0x1FE) >> 1)]
    }

    /// Note a custom-register write for the validator and the
    /// last-writer table. Called from the single write chokepoint, and
    /// only when armed.
    fn note_custom_write(&mut self, off: u16, val: u16, source: BeamWriteSource) {
        // The register bank is $000-$1FE; the write path masks only to
        // $FFE, so the rest of the custom page arrives here too and must
        // not index the tables. Those offsets decode to nothing, which
        // the CPU-side shape check reports separately.
        if off > 0x1FE {
            return;
        }
        let writer = match source {
            BeamWriteSource::Copper => crate::regcheck::Writer::Copper(self.copper.pc()),
            BeamWriteSource::Cpu | BeamWriteSource::CpuCopperIrq => {
                crate::regcheck::Writer::Cpu(self.cpu_pc)
            }
        };
        let (vpos, hpos) = (
            self.agnus.vpos.min(u32::from(u16::MAX)) as u16,
            self.agnus.hpos.min(u32::from(u16::MAX)) as u16,
        );
        if let Some(writers) = self.reg_writers.as_mut() {
            writers[usize::from(off >> 1)] = Some(RegWrite {
                value: val,
                writer,
                vpos,
                hpos,
                frame: self.emulated_frames,
            });
        }
        if self.regcheck.is_none() {
            return;
        }
        use crate::regcheck::{Direction, Finding};
        if matches!(crate::regcheck::direction(off), Some(Direction::ReadOnly)) {
            self.note_chipset_finding(Finding::WrongDirection, off, writer, val, 0, vpos, hpos);
        }
        // Undefined bits are only meaningful on a register the fitted
        // chipset actually has; on one it does not, the whole write is
        // dropped and the bit pattern says nothing.
        if !self.custom_reg_present(off) {
            self.note_chipset_finding(Finding::AbsentRegister, off, writer, val, 0, vpos, hpos);
        } else if let Some(defined) = crate::regcheck::defined_bits(off) {
            let undefined = val & !defined;
            if undefined != 0 {
                self.note_chipset_finding(
                    Finding::UnusedBits,
                    off,
                    writer,
                    val,
                    undefined,
                    vpos,
                    hpos,
                );
            }
        }
        self.check_dma_pointer(off, val, writer, vpos, hpos);
        self.check_device_misuse(off, val, writer, vpos, hpos);
    }

    /// Report a keyboard handshake pulse the 6500/1 could not sample.
    /// Software gets no other signal: the keyboard simply stops sending,
    /// and the guest sees keys go missing.
    fn check_keyboard_handshake(&mut self) {
        // Drained on every handshake edge whether or not the validator is
        // armed. The MCU latches unconditionally, so a pulse from before
        // arming would otherwise surface at the next edge and be
        // attributed to whatever PC happened to be running by then.
        let Some(width) = self.keyboard.take_short_handshake() else {
            return;
        };
        if self.regcheck.is_none() {
            return;
        }
        let writer = crate::regcheck::Writer::Cpu(self.cpu_pc);
        self.note_chipset_finding(
            crate::regcheck::Finding::KeyboardHandshakeShort,
            // CIA-A's serial port is where the handshake is driven; there
            // is no custom register involved, so the report is keyed on
            // the pulse itself.
            0,
            writer,
            width,
            crate::chipset::keyboard::HANDSHAKE_MIN_CCK.min(u64::from(u16::MAX)) as u16,
            self.agnus.vpos.min(u32::from(u16::MAX)) as u16,
            self.agnus.hpos.min(u32::from(u16::MAX)) as u16,
        );
    }

    /// Hardware-misuse checks for the engines behind the registers: a
    /// blit that cannot run, and disk DMA armed against a drive that
    /// cannot serve it.
    ///
    /// These are the cases that hang rather than glitch. A loader that
    /// arms a read with the motor still off waits on DSKBLK forever, and
    /// a blit started with its DMA off never raises BBUSY's completion --
    /// both look like "it froze" with no other evidence.
    fn check_device_misuse(
        &mut self,
        off: u16,
        val: u16,
        writer: crate::regcheck::Writer,
        vpos: u16,
        hpos: u16,
    ) {
        use crate::regcheck::Finding;
        match off {
            // BLTSIZE / BLTSIZH: the blit-start writes.
            0x058 | 0x05E => {
                if self.blitter.busy {
                    self.note_chipset_finding(
                        Finding::BlitterBusy,
                        off,
                        writer,
                        val,
                        0,
                        vpos,
                        hpos,
                    );
                }
                let need = DMACON_DMAEN | DMACON_BLTEN;
                if self.agnus.dmacon & need != need {
                    self.note_chipset_finding(
                        Finding::BlitterDmaOff,
                        off,
                        writer,
                        val,
                        self.agnus.dmacon,
                        vpos,
                        hpos,
                    );
                }
            }
            // DSKLEN, on the write that actually starts the transfer:
            // Paula needs the value written twice, and the first write
            // only latches it, so reporting on that one would name a
            // write after which DMA is by design not armed.
            0x024 if self.floppy.dsklen_write_starts_dma(val) => {
                if let Some(code) = self.floppy.dma_arming_obstacle() {
                    self.note_chipset_finding(
                        Finding::DiskNotReady,
                        off,
                        writer,
                        val,
                        code,
                        vpos,
                        hpos,
                    );
                }
            }
            _ => {}
        }
    }

    /// Flag a DMA pointer aimed past the chip RAM Agnus can address.
    ///
    /// Checked on the high half, where the intent is still visible: the
    /// pointer setters mask the value down to `chip_dma_mask`, so by the
    /// time the pointer is formed the excess has already become a silent
    /// wrap -- which is exactly the bug ("it works with 2 MB chip") and
    /// exactly why it is invisible without this.
    fn check_dma_pointer(
        &mut self,
        off: u16,
        val: u16,
        writer: crate::regcheck::Writer,
        vpos: u16,
        hpos: u16,
    ) {
        let is_pointer_high = matches!(
            off,
            0x020 | 0x080 | 0x084 | 0x048 | 0x04C | 0x050 | 0x054 | 0x0A0 | 0x0B0 | 0x0C0 | 0x0D0
        ) || (0x0E0..=0x0FF).contains(&off) && off & 3 == 0
            || (0x120..=0x13F).contains(&off) && off & 3 == 0;
        if !is_pointer_high {
            return;
        }
        let aimed = u32::from(val) << 16;
        if aimed & !self.chip_dma_mask == 0 {
            return;
        }
        self.note_chipset_finding(
            crate::regcheck::Finding::PointerOutsideChipRam,
            off,
            writer,
            0,
            val,
            vpos,
            hpos,
        );
    }

    /// Record one validator finding and log its first occurrence.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn note_chipset_finding(
        &mut self,
        finding: crate::regcheck::Finding,
        reg: u16,
        writer: crate::regcheck::Writer,
        value: u16,
        detail: u16,
        vpos: u16,
        hpos: u16,
    ) {
        let Some(check) = self.regcheck.as_mut() else {
            return;
        };
        if check.note(finding, reg, writer, value, detail, vpos, hpos) {
            let report = check
                .reports()
                .into_iter()
                .find(|r| r.finding == finding && r.reg == reg && r.writer == writer);
            if let Some(report) = report {
                log::warn!("chipset: {}", crate::regcheck::RegCheck::describe(&report));
            }
        }
    }

    /// Validate the *shape* of a CPU custom-register access: the things
    /// only the CPU path can see, because the Copper can only ever issue
    /// an aligned word write at a canonical address.
    fn note_cpu_custom_access(&mut self, addr: u64, off: u16, size: usize, read: bool) {
        use crate::regcheck::{Direction, Finding, Writer};
        let reg = off & 0x1FE;
        let writer = Writer::Cpu(self.cpu_pc);
        let (vpos, hpos) = (
            self.agnus.vpos.min(u32::from(u16::MAX)) as u16,
            self.agnus.hpos.min(u32::from(u16::MAX)) as u16,
        );
        // Byte *writes* are the misuse: the custom chips have no byte
        // lanes, so the value lands in both halves. A byte read is
        // perfectly well defined -- the chip drives the whole bus and the
        // CPU takes its lane -- and `move.b $DFF006,d0` beam polling is
        // one of the most common idioms in Amiga software.
        if size == 1 && !read {
            let byte = (addr & 1) as u16;
            self.note_chipset_finding(Finding::ByteAccess, reg, writer, 0, byte, vpos, hpos);
        } else if addr & 1 != 0 {
            self.note_chipset_finding(Finding::OddAddress, reg, writer, 0, 0, vpos, hpos);
        }
        // Offsets above the $000-$1FE register bank decode to nothing at
        // all: the write is dropped and the read returns the undriven
        // bus. (Accesses through the wider chip-register window are
        // folded to this page before they reach here, so they are
        // indistinguishable from a canonical access and are not
        // reported.)
        if off > 0x1FE {
            self.note_chipset_finding(Finding::UnmappedOffset, reg, writer, 0, 0, vpos, hpos);
        }
        if read && matches!(crate::regcheck::direction(reg), Some(Direction::WriteOnly)) {
            self.note_chipset_finding(Finding::WrongDirection, reg, writer, 0, 0, vpos, hpos);
        }
    }

    /// Whether the fitted Agnus/Denise implements the register at `off`.
    /// Mirrors the revision gates the write dispatch itself applies, so a
    /// finding and a dropped write always agree.
    fn custom_reg_present(&self, off: u16) -> bool {
        match off {
            // ECS Agnus: programmable sync/blank, UHRES, ECS blitter size.
            0x05A | 0x05C | 0x05E | 0x078 | 0x1C0 | 0x1C2 | 0x1C4 | 0x1C6 | 0x1C8 | 0x1CA
            | 0x1CC | 0x1CE | 0x1D8 | 0x1DC | 0x1DE | 0x1E0 | 0x1E2 => {
                self.blitter_ecs_registers_enabled()
            }
            // ECS Denise: BPLCON3 latches only while BPLCON0.ECSENA is
            // set, which is the gate the write dispatch applies -- so a
            // BPLCON3 write without ECSENA is reported, which is one of
            // the classic silent drops this validator exists to surface.
            0x106 => self.bplcon3_write_enabled(),
            0x1E4 => self.denise_ecs_registers(),
            // AGA Lisa: BPLCON4, CLXCON2, and bitplanes 7-8.
            0x10C | 0x10E => self.denise_is_lisa(),
            0x0F8..=0x0FF | 0x11C | 0x11E => self.aga_enabled(),
            // AGA Alice: FMODE.
            0x1FC => self.aga_enabled(),
            _ => true,
        }
    }

    /// Replace the debugger-window custom-register watch set (word
    /// offsets into $DFF000). A pending unpolled hit is dropped, so a
    /// stale hit cannot fire after its watch was removed.
    pub fn set_ui_reg_watches(&mut self, offsets: &[u16]) {
        self.ui_reg_watches = offsets.to_vec();
        self.ui_reg_hit = None;
    }

    /// Take the pending custom-register watch hit, if any.
    pub fn take_ui_reg_hit(&mut self) -> Option<UiRegHit> {
        self.ui_reg_hit.take()
    }

    /// The debugger's armed beam traps.
    pub fn ui_beam_traps(&self) -> &[BeamTrap] {
        &self.ui_beam_traps
    }

    /// Add a persistent beam trap at (`vpos`, `hpos`), or remove it when
    /// one is already set there. Returns true when the trap is now set.
    pub fn ui_toggle_beam_trap(&mut self, vpos: u16, hpos: Option<u16>) -> bool {
        match self
            .ui_beam_traps
            .iter()
            .position(|trap| trap.vpos == vpos && trap.hpos == hpos && !trap.once)
        {
            Some(pos) => {
                self.ui_beam_traps.remove(pos);
                false
            }
            None => {
                self.ui_beam_traps.push(BeamTrap {
                    vpos,
                    hpos,
                    once: false,
                });
                true
            }
        }
    }

    /// Arm a one-shot run-to-position beam trap, replacing any previous
    /// one-shot (only one run-to target makes sense at a time).
    pub fn ui_arm_beam_trap_once(&mut self, vpos: u16, hpos: Option<u16>) {
        self.ui_beam_traps.retain(|trap| !trap.once);
        self.ui_beam_traps.push(BeamTrap {
            vpos,
            hpos,
            once: true,
        });
    }

    /// Drop an armed one-shot run-to trap (a run-to that ended early).
    pub fn ui_disarm_beam_trap_once(&mut self) {
        self.ui_beam_traps.retain(|trap| !trap.once);
    }

    pub fn ui_clear_beam_traps(&mut self) {
        self.ui_beam_traps.clear();
        self.ui_beam_hit = None;
    }

    /// Take the pending beam-trap hit `(vpos, hpos)`, if any.
    pub fn take_ui_beam_hit(&mut self) -> Option<(u16, u16)> {
        self.ui_beam_hit.take()
    }

    /// Replace the debugger's watched-word mirror, forwarding it into
    /// the blitter and floppy write sites. Pending writer records are
    /// dropped (they described the previous watch set).
    pub fn set_ui_mem_watches(&mut self, addrs: &[u32]) {
        self.ui_mem_watch_addrs = addrs.to_vec();
        self.ui_mem_writers.clear();
        self.ui_mem_reads.clear();
        self.blitter.set_debug_watch_addrs(addrs);
        self.floppy.set_debug_watch_addrs(addrs);
    }

    /// Note a CPU write of `size` bytes at `addr` for watch attribution
    /// (called from the CPU chip/slow RAM write paths when watches are
    /// armed).
    pub(crate) fn ui_note_cpu_ram_write(&mut self, addr: u32, size: usize) {
        if self.ui_mem_watch_addrs.is_empty() {
            return;
        }
        let start = addr & 0x00FF_FFFE;
        let end = addr.wrapping_add(size.max(1) as u32);
        for &watch in &self.ui_mem_watch_addrs {
            if watch.wrapping_add(1) >= start && watch < end {
                Self::push_ui_mem_writer(
                    &mut self.ui_mem_writers,
                    UiMemWriter {
                        addr: watch,
                        source: crate::debugger::WatchSource::Cpu,
                        vpos: self.agnus.vpos.min(u32::from(u16::MAX)) as u16,
                        hpos: self.agnus.hpos.min(u32::from(u16::MAX)) as u16,
                    },
                );
            }
        }
    }

    /// Arm or disarm the memory heat map over a window of the address
    /// space. Re-arming with a different window starts a cold map: the
    /// cells would otherwise carry activity from addresses they no
    /// longer name.
    pub fn set_heat_map(&mut self, window: Option<(u32, u32)>) {
        match window {
            None => self.heatmap = None,
            Some((base, span)) => match self.heatmap.as_mut() {
                // Compared against the span the request becomes, not the
                // one asked for: the map rounds it up, so a caller
                // repeating its own request would otherwise look like a
                // new window every time and wipe what it had collected.
                Some(map)
                    if map.base() == base && map.span() == crate::heatmap::rounded_span(span) => {}
                Some(map) => map.set_window(base, span),
                None => self.heatmap = Some(Box::new(crate::heatmap::HeatMap::new(base, span))),
            },
        }
    }

    pub fn heat_map(&self) -> Option<&crate::heatmap::HeatMap> {
        self.heatmap.as_deref()
    }

    /// Record memory activity for the heat map. Kept behind an
    /// emptiness check by every caller, like the watch hooks.
    pub(crate) fn note_heat(&mut self, addr: u32, len: u32, by: crate::heatmap::Toucher) {
        let frame = self.emulated_frames;
        if let Some(map) = self.heatmap.as_mut() {
            map.touch(addr, len, by, frame);
        }
    }

    pub(crate) fn heat_map_armed(&self) -> bool {
        self.heatmap.is_some()
    }

    /// Note a read-side DMA fetch of `len` bytes from `addr` by `source`,
    /// latching a hit when it covers a watched word.
    ///
    /// Called from the per-channel fetch sites, which are the only places
    /// that know *which* bitplane or sprite is fetching -- the chip-bus
    /// arbiter above them sees only "a bitplane". "Which sprite fetched
    /// this word" is the question a display bug actually poses.
    pub(crate) fn note_dma_read(
        &mut self,
        source: crate::debugger::WatchSource,
        addr: u32,
        len: u32,
    ) {
        if self.heatmap.is_some() {
            self.note_heat(
                addr,
                len,
                crate::heatmap::Toucher::from_watch_source(source),
            );
        }
        self.ui_note_memory_read(source, addr, len);
    }

    /// Latch a data read for debugger read/access watchpoints. CPU callers
    /// use this directly so instruction fetches stay excluded.
    pub(crate) fn ui_note_memory_read(
        &mut self,
        source: crate::debugger::WatchSource,
        addr: u32,
        len: u32,
    ) {
        if self.ui_mem_watch_addrs.is_empty() {
            return;
        }
        let start = u64::from(addr);
        let end = start + u64::from(len.max(1));
        for &watch in &self.ui_mem_watch_addrs {
            let watch_start = u64::from(watch);
            if watch_start < end && watch_start + 2 > start {
                let hit = UiMemWriter {
                    addr: watch,
                    source,
                    vpos: self.agnus.vpos.min(u32::from(u16::MAX)) as u16,
                    hpos: self.agnus.hpos.min(u32::from(u16::MAX)) as u16,
                };
                self.ui_mem_reads
                    .retain(|old| old.addr != hit.addr || old.source != hit.source);
                if self.ui_mem_reads.len() >= 8 {
                    self.ui_mem_reads.remove(0);
                }
                self.ui_mem_reads.push(hit);
            }
        }
    }

    /// Whether any memory watch is armed, so the DMA fetch sites can skip
    /// the call entirely on an unwatched machine.
    /// Whether any per-access observer wants the DMA fetch sites to
    /// report: a memory watch, or the heat map.
    pub(crate) fn mem_watches_armed(&self) -> bool {
        !self.ui_mem_watch_addrs.is_empty() || self.heatmap.is_some()
    }

    /// Take the pending read-watch hits in access order.
    pub fn take_ui_mem_reads(&mut self) -> Vec<UiMemWriter> {
        std::mem::take(&mut self.ui_mem_reads)
    }

    fn push_ui_mem_writer(writers: &mut Vec<UiMemWriter>, writer: UiMemWriter) {
        // Latest write per address wins; the list stays tiny.
        writers.retain(|w| w.addr != writer.addr);
        if writers.len() >= 8 {
            writers.remove(0);
        }
        writers.push(writer);
    }

    /// The writer of the most recent write to watched word `addr`, if
    /// one was recorded since the last take. Drains the blitter/floppy
    /// latches lazily, so the hot DMA paths never touch the bus record.
    pub fn ui_take_mem_writer(&mut self, addr: u32) -> Option<UiMemWriter> {
        let vpos = self.agnus.vpos.min(u32::from(u16::MAX)) as u16;
        let hpos = self.agnus.hpos.min(u32::from(u16::MAX)) as u16;
        if let Some((waddr, _)) = self.blitter.take_debug_watched_write() {
            Self::push_ui_mem_writer(
                &mut self.ui_mem_writers,
                UiMemWriter {
                    addr: waddr & 0x00FF_FFFE,
                    source: crate::debugger::WatchSource::Blitter,
                    vpos,
                    hpos,
                },
            );
        }
        if let Some((waddr, _)) = self.floppy.take_debug_watched_write() {
            Self::push_ui_mem_writer(
                &mut self.ui_mem_writers,
                UiMemWriter {
                    addr: waddr & 0x00FF_FFFE,
                    source: crate::debugger::WatchSource::Disk,
                    vpos,
                    hpos,
                },
            );
        }
        let addr = addr & 0x00FF_FFFE;
        let found = self.ui_mem_writers.iter().position(|w| w.addr == addr)?;
        Some(self.ui_mem_writers.remove(found))
    }

    /// Carry the interactive debug state (beam traps, Copper
    /// breakpoints, layer-isolation masks) from the pre-restore bus into
    /// this freshly deserialized one. These fields are transient
    /// (serde-skipped), so without this a reverse step or state load
    /// would silently disarm the debugger. Pending unpolled hits stay
    /// dropped: they describe the abandoned timeline.
    pub(crate) fn adopt_ui_debug_state(&mut self, previous: &mut Bus) {
        // Armed diagnostics move across with the rest of the debug state.
        // A reverse step or a state load replaces the Bus, and without
        // this the validator, the SMC map, the heat map and any injected
        // faults would silently disarm underneath a session that had
        // asked for them -- while beam traps and watches stayed live,
        // which is the tell that this was an omission. Their collected
        // reports come too: the operator's experiment is still running.
        self.regcheck = previous.regcheck.take();
        self.reg_writers = previous.reg_writers.take();
        self.smc = previous.smc.take();
        self.heatmap = previous.heatmap.take();
        self.injected_faults = std::mem::take(&mut previous.injected_faults);
        self.ui_beam_traps = previous.ui_beam_traps.clone();
        self.ui_copper_breaks = previous.ui_copper_breaks.clone();
        self.ui_copper_last_pc = previous.ui_copper_last_pc;
        self.ui_layer_masks = previous.ui_layer_masks;
        self.frame_analyzer_enabled = previous.frame_analyzer_enabled;
        self.frame_analyzer_full = previous.frame_analyzer_full;
        self.floppy.set_dma_trace_enabled(self.frame_analyzer_full);
        let watches = previous.ui_mem_watch_addrs.clone();
        self.set_ui_mem_watches(&watches);
    }

    /// The debugger's armed Copper breakpoint addresses.
    pub fn ui_copper_breaks(&self) -> &[u32] {
        &self.ui_copper_breaks
    }

    /// Add a Copper breakpoint at a chip-RAM instruction address, or
    /// remove it when already set. Returns true when now set.
    pub fn ui_toggle_copper_break(&mut self, addr: u32) -> bool {
        let addr = addr & 0x00FF_FFFE;
        match self.ui_copper_breaks.iter().position(|&a| a == addr) {
            Some(pos) => {
                self.ui_copper_breaks.remove(pos);
                false
            }
            None => {
                self.ui_copper_breaks.push(addr);
                true
            }
        }
    }

    pub fn ui_clear_copper_breaks(&mut self) {
        self.ui_copper_breaks.clear();
        self.ui_copper_hit = None;
    }

    /// Take the pending Copper breakpoint hit `(pc, vpos, hpos)`, if any.
    pub fn take_ui_copper_hit(&mut self) -> Option<(u32, u16, u16)> {
        self.ui_copper_hit.take()
    }

    /// The Copper's completed-instruction count, for copper stepping.
    pub fn copper_instructions_retired(&self) -> u64 {
        self.copper.instructions_retired()
    }

    /// The debugger's video-layer isolation masks (Video tab).
    pub fn ui_layer_masks(&self) -> UiLayerMasks {
        self.ui_layer_masks
    }

    /// Toggle bitplane `plane` in the presented picture. Returns true
    /// when the plane is now shown.
    pub fn ui_toggle_layer_plane(&mut self, plane: usize) -> bool {
        self.ui_layer_masks.planes ^= 1 << (plane & 7);
        self.ui_layer_masks.planes & (1 << (plane & 7)) != 0
    }

    /// Toggle sprite `sprite` in the presented picture. Returns true
    /// when the sprite is now shown.
    pub fn ui_toggle_layer_sprite(&mut self, sprite: usize) -> bool {
        self.ui_layer_masks.sprites ^= 1 << (sprite & 7);
        self.ui_layer_masks.sprites & (1 << (sprite & 7)) != 0
    }

    /// Fire a Copper breakpoint when the live Copper's PC has newly
    /// arrived at a breakpointed address. Called from the live copper
    /// step path (never the blitter-deadline predictor's cloned
    /// simulation) and after CPU strobe writes, so every way the PC can
    /// move is observed.
    pub(super) fn check_ui_copper_breaks(&mut self) {
        let pc = self.copper.pc();
        if pc == self.ui_copper_last_pc {
            return;
        }
        self.ui_copper_last_pc = pc;
        if self.ui_copper_hit.is_none() && self.ui_copper_breaks.contains(&pc) {
            self.ui_copper_hit = Some((
                pc,
                self.agnus.vpos.min(u32::from(u16::MAX)) as u16,
                self.agnus.hpos.min(u32::from(u16::MAX)) as u16,
            ));
        }
    }

    /// Fire beam traps crossed by the last beam advance from `old`
    /// (exclusive) to the current position (inclusive), in beam order.
    /// `old_frame_lines` bounds targets within the frame the advance
    /// started in, so a trap on a line the scan never reaches does not
    /// fire spuriously at the frame wrap. Fired one-shot traps are
    /// removed; persistent traps stay armed and re-fire every frame.
    pub(super) fn check_ui_beam_traps(
        &mut self,
        old: (u32, u32),
        old_frame_lines: u32,
        new_frames: u32,
    ) {
        let cur = (self.agnus.vpos, self.agnus.hpos);
        let hit_slot = &mut self.ui_beam_hit;
        self.ui_beam_traps.retain(|trap| {
            let target = (u32::from(trap.vpos), u32::from(trap.hpos.unwrap_or(0)));
            let hit = match new_frames {
                0 => old < target && target <= cur,
                1 => (target > old && target.0 < old_frame_lines) || target <= cur,
                // The advance spanned whole frames: any reachable
                // position was crossed.
                _ => target.0 < old_frame_lines || target <= cur,
            };
            if hit && hit_slot.is_none() {
                *hit_slot = Some((trap.vpos, trap.hpos.unwrap_or(0)));
            }
            !(hit && trap.once)
        });
    }

    pub fn attach_gayle(&mut self, gayle: Gayle) {
        self.gayle = Some(gayle);
    }

    pub fn attach_uaelib(&mut self, uaelib: crate::uaelib::UaeLib) {
        self.uaelib = Some(uaelib);
    }

    /// Fit a freezer cartridge and write its configuration block for this
    /// machine. Fit it after the boards that shape the block (Gayle, the
    /// A4000 IDE, Akiko) so the monitor is told about them.
    pub fn attach_cartridge(&mut self, mut cartridge: crate::cartridge::Cartridge) {
        cartridge.configure(self.cartridge_traits());
        self.cartridge = Some(cartridge);
    }

    /// What the cartridge's configuration block says about this machine
    /// (WinUAE's `hrtmon_configure` inputs).
    fn cartridge_traits(&self) -> crate::cartridge::MachineTraits {
        crate::cartridge::MachineTraits {
            aga: self.aga_enabled(),
            cd32: self.akiko.is_some(),
            ntsc: self.agnus.video_standard() == VideoStandard::Ntsc,
            ide: self.gayle.is_some() || self.ide_a4000.is_some(),
            gayle_ide: self.gayle.is_some(),
            chip_ram_bytes: self.mem.chip_ram.len() as u32,
        }
    }

    /// The cartridge's freeze button: snapshot the register shadows into
    /// the bank, point the level-7 vector (`vbr` + $7C) at the monitor and
    /// raise the non-maskable interrupt, which the CPU adapter delivers at
    /// the next instruction boundary. Returns the entry address.
    pub fn cartridge_freeze(&mut self, vbr: u32) -> Result<u32, crate::cartridge::FreezeError> {
        let Some(cartridge) = self.cartridge.as_mut() else {
            return Err(crate::cartridge::FreezeError::NotFitted);
        };
        cartridge.freeze(&mut self.mem, vbr)
    }

    /// Whether a freeze interrupt is waiting for the CPU.
    #[inline]
    pub fn cartridge_nmi_pending(&self) -> bool {
        self.cartridge.as_ref().is_some_and(|c| c.nmi_pending())
    }

    /// Consume the pending freeze interrupt at its acknowledge.
    #[inline]
    pub fn cartridge_take_nmi(&mut self) -> bool {
        self.cartridge.as_mut().is_some_and(|c| c.take_nmi())
    }

    pub fn attach_ramsey(&mut self, ramsey: crate::ramsey::Ramsey) {
        self.ramsey = Some(ramsey);
    }

    pub fn attach_gary(&mut self, gary: crate::gary::Gary) {
        self.gary = Some(gary);
    }

    pub fn attach_sdmac(&mut self, sdmac: crate::sdmac::Sdmac) {
        self.sdmac = Some(sdmac);
    }

    pub fn attach_ide_a4000(&mut self, ide: crate::ide_a4000::IdeA4000) {
        self.ide_a4000 = Some(ide);
    }

    pub fn attach_akiko(&mut self, akiko: crate::akiko::Akiko) {
        self.akiko = Some(akiko);
    }

    /// Attach the functional Zorro-chain boards. Their slot indices must match
    /// the [`crate::zorro::BoardBacking::Device`] indices assigned when the
    /// boards were added to the chain.
    pub fn attach_devices(&mut self, devices: Vec<crate::zorro_device::BoardDevice>) {
        self.devices = devices;
    }

    pub fn attach_cdtv(&mut self, cdtv: crate::cdtv::CdtvController) {
        self.cdtv = Some(cdtv);
    }

    pub fn set_rtc_present(&mut self, present: bool) {
        self.rtc_present = present;
    }

    /// Fit the configured clock part (machine-profile default or
    /// `[machine] rtc_chip`). Swapping parts starts from power-on state,
    /// so apply it before the deterministic seed.
    pub fn set_rtc_chip(&mut self, chip: RtcChip) {
        if self.rtc.chip() != chip {
            self.rtc = Rtc::new(chip);
        }
    }

    pub fn set_video_standard(&mut self, video_standard: VideoStandard) {
        self.agnus.set_video_standard(video_standard);
        self.current_frame_geometry.frame_lines = fixed_standard_frame_lines(
            video_standard,
            self.current_frame_geometry.lace,
            self.current_frame_render_base.long_field,
        );
        self.last_frame_geometry.frame_lines = fixed_standard_frame_lines(
            video_standard,
            self.last_frame_geometry.lace,
            self.frame_render_base().long_field,
        );
    }

    /// Preset entry point: an OCS Agnus pairs with an OCS Denise, an ECS
    /// Agnus with an ECS Denise. Mixed machines (e.g. late A500s with an ECS
    /// Agnus and OCS Denise) go through set_chipset_revisions.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn set_agnus_revision(&mut self, revision: AgnusRevision) {
        let denise = if revision.is_ecs() {
            DeniseRevision::Ecs8373
        } else {
            DeniseRevision::Ocs
        };
        self.set_chipset_revisions(revision, denise);
    }

    pub fn set_chipset_revisions(&mut self, agnus: AgnusRevision, denise: DeniseRevision) {
        self.agnus.set_revision(agnus);
        self.denise_revision = denise;
        if !denise.is_ecs() {
            self.denise.diwhigh = 0;
            self.denise.diwhigh_written = false;
        }
        self.configure_chip_dma_masks();
    }

    fn denise_ecs_registers(&self) -> bool {
        self.denise_revision.is_ecs()
    }

    /// ECS Denise ENBPLCN3: BPLCON0 bit 0 must be set for BPLCON3 writes to
    /// latch (8373 spec). OCS Denise has no BPLCON3 at all; AGA Lisa always
    /// accepts the write (the palette BANK/LOCT mechanics depend on it).
    fn bplcon3_write_enabled(&self) -> bool {
        self.denise_is_lisa()
            || (self.denise_ecs_registers() && self.denise.bplcon0 & BPLCON0_ECSENA != 0)
    }

    fn denise_is_lisa(&self) -> bool {
        matches!(self.denise_revision, DeniseRevision::AgaLisa)
    }

    /// AGA plane-count decode applies (Alice fetch + Lisa display; mixed
    /// AGA/non-AGA chip pairs never shipped).
    fn aga_enabled(&self) -> bool {
        matches!(self.agnus.revision(), AgnusRevision::AgaAlice)
    }

    pub fn reset_custom_chips_from_cpu_reset(&mut self) {
        // The CPU RESET instruction asserts the external reset line without
        // resetting the CPU core or clearing RAM. Reuse the warm device reset
        // so CIAs, custom chips, expansion boards, and /OVL all return to
        // reset state, then restore the emulator's monotonic time coordinate.
        let emulated_cck = self.emulated_cck;
        let emulated_frames = self.emulated_frames;
        self.reset_for_keyboard_reset();
        self.emulated_cck = emulated_cck;
        self.emulated_frames = emulated_frames;
    }

    fn effective_diwhigh(&self) -> DiwHigh {
        if self.denise_ecs_registers() && self.denise.diwhigh_written {
            DiwHigh::ecs_explicit(self.denise.diwhigh)
        } else {
            DiwHigh::ocs_implicit()
        }
    }

    fn configure_chip_dma_masks(&mut self) {
        // The writable DMA pointer high bits follow the Agnus revision's
        // address-bus reach as well as the installed chip RAM: an 8372A
        // drops bit 20 even with 2 MB fitted, and OCS stops at 512 KiB.
        let mask = chip_dma_addr_mask(self.mem.chip_ram.len())
            & self.agnus.revision().dma_addr_capability_mask();
        self.chip_dma_mask = mask;
        self.agnus.set_dma_addr_mask(mask);
        self.denise.set_dma_addr_mask(mask);
        self.blitter.set_dma_addr_mask(mask);
        self.paula.set_dma_addr_mask(mask);
        self.floppy.set_dma_addr_mask(mask);
        let ptr_mask = mask & !1;
        for ptr in &mut self.display_dma_bplpt {
            *ptr &= ptr_mask;
        }
        for ptr in &mut self.display_dma_sprpt {
            *ptr &= ptr_mask;
        }
    }

    fn blitter_ecs_registers_enabled(&self) -> bool {
        !matches!(self.agnus.revision(), AgnusRevision::Ocs)
    }

    /// Relaxed DDF stop ceiling for every effective-DDF-window computation:
    /// BEAMCON0.HARDDIS on ECS, and always on AGA Alice (the AGA fetch
    /// sequencer has no hardwired $D8 stop; the relaxed ceiling is still
    /// bounded by the canvas).
    fn harddis_active(&self) -> bool {
        self.aga_enabled()
            || (!matches!(self.agnus.revision(), AgnusRevision::Ocs)
                && self.agnus.beamcon0() & BEAMCON0_HARDDIS != 0)
    }

    /// External light-pen pulse at the current beam position. No input
    /// device is wired to this yet; tests (and future controller-port
    /// plumbing) call it directly.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn light_pen_pulse(&mut self) {
        self.agnus.trigger_light_pen();
    }

    /// Queue an Amiga raw key press (a 7-bit raw scan code).
    pub fn enqueue_key(&mut self, amiga_raw_keycode: u8) {
        self.enqueue_key_event(amiga_raw_keycode, true);
    }

    /// Queue an Amiga raw key transition with the keyboard MCU, which
    /// serializes it as `~((rawkey << 1) | up_down)` over KCLK/KDAT.
    pub fn enqueue_key_event(&mut self, amiga_raw_keycode: u8, pressed: bool) {
        self.keyboard.key_transition(amiga_raw_keycode, pressed);
    }

    /// Colour clocks until the keyboard MCU next needs to act (KCLK
    /// edge or protocol timeout); caps the emulator's idle fast-forward.
    pub fn next_keyboard_event_cck(&self) -> Option<u32> {
        self.keyboard.next_event_cck()
    }

    /// Reset the per-frame render/capture buffers to their empty starting
    /// state. Shared by a real hardware reset (`reset_for_keyboard_reset`)
    /// and the state-load path (`reset_transient_video_after_state_load`)
    /// for exactly the fields those two callers already reset to
    /// identical values, so a new capture-buffer field only has to be
    /// listed here once instead of drifting between the two copies.
    ///
    /// Deliberately NOT included: `current_frame_render_blocked` and
    /// `sprite_dma_frame_start_ptr` (the two callers legitimately disagree
    /// on their value -- a state load derives them from the just-restored
    /// beam/pointer state instead of zeroing them), and the fields only
    /// `reset_for_keyboard_reset` touches (palettes, collision tracking,
    /// frame geometry, DMA pointers/delays) because those are either
    /// power-on-only resets or are themselves part of the serialized save
    /// state and must not be clobbered after a state load.
    fn reset_frame_capture_buffers(&mut self) {
        self.last_frame_render_base = None;
        self.last_frame_render_events.clear();
        self.last_frame_beam_bottom_palette_events.clear();
        self.current_frame_chip_ram.clear();
        self.current_frame_chip_ram
            .extend_from_slice(&self.mem.chip_ram);
        self.last_frame_chip_ram = std::sync::Arc::new(Vec::new());
        self.current_frame_chip_ram_writes.clear();
        self.last_frame_chip_ram_writes.clear();
        self.current_frame_bitplane_rows = empty_captured_bitplane_rows();
        self.last_frame_bitplane_rows = std::sync::Arc::new(empty_captured_bitplane_rows());
        self.bitplane_row_pool.clear();
        self.current_frame_sprite_lines.clear();
        clear_captured_sprite_lines_by_y(&mut self.current_frame_sprite_lines_by_y);
        self.current_frame_sprite_collision_sources = empty_sprite_collision_sources();
        self.last_frame_sprite_lines.clear();
        self.current_frame_held_sprites = [None; 8];
        self.last_frame_held_sprites = [None; 8];
        self.current_frame_sprite_display_enable_x_by_y = empty_sprite_display_enable_x_by_y();
        self.last_frame_sprite_display_enable_x_by_y = empty_sprite_display_enable_x_by_y();
        self.current_frame_sprite_dma_observed = false;
        self.last_frame_sprite_dma_observed = false;
        self.current_frame_bus_trace.clear();
        self.last_frame_bus_trace = None;
        self.active_frame_blit = None;
        self.next_frame_blit_id = 0;
    }

    pub fn reset_for_keyboard_reset(&mut self) {
        // The serial sink hears about the reset first: an in-process
        // synthesizer releases what the interrupted guest left
        // sounding, exactly as if its line had dropped.
        self.paula.serial.machine_reset();
        let video_standard = self.agnus.video_standard();
        let agnus_revision = self.agnus.revision();
        self.cia_a = Cia::new(Which::A);
        self.cia_b = Cia::new(Which::B);
        self.paula.reset_registers();
        self.agnus = Agnus::with_video_standard_and_revision(video_standard, agnus_revision);
        self.copper = Copper::new();
        self.denise = Denise::new();
        self.blitter = Blitter::new();
        self.configure_chip_dma_masks();
        self.rtc.reset();
        if let Some(gayle) = self.gayle.as_mut() {
            gayle.reset();
        }
        if let Some(gary) = self.gary.as_mut() {
            gary.reset();
        }
        if let Some(ramsey) = self.ramsey.as_mut() {
            ramsey.reset();
        }
        if let Some(sdmac) = self.sdmac.as_mut() {
            sdmac.reset();
        }
        if let Some(ide) = self.ide_a4000.as_mut() {
            ide.reset();
        }
        if let Some(akiko) = self.akiko.as_mut() {
            akiko.reset();
        }
        if let Some(cdtv) = self.cdtv.as_mut() {
            cdtv.reset();
        }
        for dev in &mut self.devices {
            crate::zorro_device::ZorroDevice::reset(dev);
        }
        if let Some(uaelib) = self.uaelib.as_mut() {
            uaelib.reset();
        }
        if self.cartridge.is_some() {
            let traits = self.cartridge_traits();
            if let Some(cartridge) = self.cartridge.as_mut() {
                cartridge.reset(traits);
            }
        }
        self.mem.zorro.warm_reset();
        self.hdd_led_until_cck = 0;
        self.overlay_disable_pending = false;
        // The MCU restarts its power-up flow; physically held keys stay
        // held and are reported in the upcoming $FD/$FE stream.
        self.keyboard.begin_power_up();
        self.keyboard_system_reset_pending = false;
        self.input.reset_for_machine_reset();
        self.video_pipeline_stats = VideoPipelineStats::default();
        self.slice_preempted = false;
        self.pending_vbi = 0;
        self.pending_copper_frame_start = None;
        self.copper_active_in_frame = false;
        self.copper_current_list = 1;
        self.delivered_irq_pending = 0;
        self.pending_copper_irq_beam = None;
        self.delivered_copper_irq_beam = None;
        self.coper_cpu_irq_delay_cck = 0;
        self.irq_latency_visible_at = [0; 16];
        self.irq_latency_mask = 0;
        self.irq_latency_last_pending = 0;
        self.trace_cia_irq_pins = (false, false);
        self.trace_intreq = 0;
        self.beam_top_palette = Palette::new();
        self.beam_bottom_palette = Palette::new();
        self.beam_bottom_palette_valid = false;
        self.cpu_palette_target = CpuPaletteTarget::Top;
        self.cpu_palette_target_writes = 0;
        self.cpu_palette_target_beam = None;
        self.current_frame_render_base = RenderRegisterSnapshot::default();
        self.current_frame_render_events.clear();
        self.current_frame_collision_events.clear();
        self.current_frame_collision_control_events.clear();
        self.current_frame_collision_bpldat_events.clear();
        self.current_frame_collision_sprite_events.clear();
        self.current_frame_collision_control_index = None;
        self.current_frame_collision_bpldat_index = None;
        self.current_frame_collision_sprite_index = None;
        self.current_frame_collision_may_have_dual_playfield = false;
        self.beam_bottom_palette_events.clear();
        self.pending_beam_bottom_palette_events.clear();
        self.current_frame_beam_top_palette = self.beam_top_palette;
        self.last_frame_beam_top_palette = Palette::new();
        self.last_frame_beam_top_palette_end = Palette::new();
        self.last_frame_beam_bottom_palette = Palette::new();
        self.last_frame_beam_bottom_palette_valid = false;
        self.reset_frame_capture_buffers();
        self.display_dma_sprite_state = [DisplaySpriteDmaState::default(); 8];
        self.current_frame_display_snapshot_taken = false;
        self.ocs_same_line_diw_start_blocked_vpos = None;
        // A reset starts a fresh timeline: the flop has no history yet and
        // falls back to the level test until a comparator event runs.
        self.diw_vertical_open = None;
        self.current_frame_render_blocked = false;
        self.current_frame_visible_start_vpos = RENDER_VISIBLE_START_VPOS;
        self.last_frame_visible_start_vpos = RENDER_VISIBLE_START_VPOS;
        let frame_lines = self.agnus.current_frame_lines();
        self.current_frame_geometry =
            FrameGeometry::standard(RENDER_VISIBLE_START_VPOS, frame_lines, false);
        self.last_frame_geometry =
            FrameGeometry::standard(RENDER_VISIBLE_START_VPOS, frame_lines, false);
        self.current_frame_presentation_h_window = None;
        self.last_frame_presentation_h_window = None;
        self.current_frame_presentation_v_window = None;
        self.last_frame_presentation_v_window = None;
        self.lazy_collision_vpos = RENDER_VISIBLE_START_VPOS;
        self.lazy_collision_hpos = RENDER_COPPER_WAIT_HPOS_FB0;
        self.collision_tracking_active = false;
        self.cpu_bus_arbitration_enabled = false;
        self.blitter_slowdown_cpu_misses = 0;
        self.slice_bus_advanced_cck = 0;
        self.slice_bus_tick = AgnusTick::default();
        self.last_chip_bus_owner = ChipBusOwner::Idle;
        self.device_clock.reset();
        self.emulated_cck = 0;
        self.emulated_frames = 0;
        self.cpu_posted_write_debt = 0;
        self.cpu_chip_port_free_at = 0;
        self.cpu_chip_clock_phase = 0;
        self.cpu_bus_overlap_clocks = 0;
        self.cpu_wait_cck_total = 0;
        self.display_dma_bplpt = [0; 8];
        self.display_dma_sprpt = [0; 8];
        self.sprite_dma_frame_start_ptr = [0; 8];
        // The pre-display replay cursor indexes the frame's render-event log
        // and the pointers cleared above, so it has to restart with them: a
        // cursor carried over would resume part way down a field that no
        // longer exists and skip the first field's sprite DMA.
        self.sprite_dma_replay = SpriteDmaReplayCursor::default();
        self.display_dma_clipped_rows_advanced = false;
        self.bitplane_dmacon_delay = None;
        self.bitplane_bplcon0_delay = None;
        self.bitplane_ddfstart_miss = None;
        self.wide_bitplane_hot_line.invalidate();
        self.wide_bitplane_dynamic_vpos.set(None);
        self.mem.overlay = true;
        // A 68000 RESET returns the A1000 WCS latch to boot mode (boot ROM at
        // $F80000, WCS writable) without clearing the WCS, so the boot ROM can
        // re-run the Kickstart it already loaded instead of reading the disk.
        self.mem.wcs_write_protected = false;
        self.floppy.reset_external_drives();
        self.floppy.write_prb(0xFF);
    }

    /// Select the RAM pattern used by subsequent cold power-on resets. Initial
    /// machine construction fills Memory before building the Bus so the
    /// renderer's first chip-RAM snapshot already sees the selected pattern.
    pub fn set_ram_init(&mut self, init: RamInit) {
        self.ram_init = init;
    }

    /// Full cold boot. Reinitialises RAM according to the configured policy
    /// and then runs the same chip/CIA reset as a keyboard reset. Unlike
    /// Ctrl-Amiga-Amiga, RAM is not preserved, so the machine comes up as if
    /// it had been power-cycled. Fill RAM first so the keyboard-reset path
    /// snapshots the new chip RAM for the renderer.
    pub fn power_on_reset(&mut self) {
        self.mem.power_on_reset_with(self.ram_init);
        self.reset_for_keyboard_reset();
        // The warm-reset path above keeps a spinning disc's cached
        // lead-in; a power cycle stops the mechanism, so a mounted disc
        // pays the cold spin-up again.
        if let Some(akiko) = self.akiko.as_mut() {
            akiko.rearm_cold_spin_up();
        }
    }

    pub fn front_panel_status(&self) -> FrontPanelStatus {
        FrontPanelStatus {
            power_led_bright: self.paula.led_filter_guest_on(),
            fdd_led_on: self.floppy.activity_led_on(),
            fdd_track: self.floppy.selected_track(),
            hdd_led: (self.gayle.is_some()
                || self.ide_a4000.is_some()
                || self.has_hard_disk_controller()
                || self.has_filesys_mount())
            .then_some(self.emulated_cck < self.hdd_led_until_cck),
            cd_led: self
                .cdtv
                .as_ref()
                .map(|cdtv| cdtv.activity_led_on())
                .or_else(|| self.akiko.as_ref().map(|akiko| akiko.activity_led_on()))
                // A SCSI CD-ROM's data traffic rides the HDD LED with the
                // rest of the bus; its own LED shows CD-DA playback (and
                // brings the status-bar eject button with it).
                .or_else(|| {
                    self.scsi_cd_ref()
                        .map(crate::scsi::ScsiCdRom::audio_playing)
                }),
            // Match the same presence-priority used by the LED and the media
            // controls: an empty built-in drive remains the front-panel drive
            // rather than falling through to an expansion CD-ROM.
            cd_track: if let Some(cdtv) = self.cdtv.as_ref() {
                cdtv.current_track()
            } else if let Some(akiko) = self.akiko.as_ref() {
                akiko.current_track()
            } else {
                self.scsi_cd_ref()
                    .and_then(crate::scsi::ScsiCdRom::current_track)
            },
            output_volume_percent: self.paula.output_volume_percent(),
        }
    }

    /// Whether the machine has a CD drive: the CDTV DMAC, the CD32 Akiko,
    /// or a CD-ROM target on a SCSI bus.
    pub fn cd_drive_present(&self) -> bool {
        self.cdtv.is_some() || self.akiko.is_some() || self.scsi_cd_ref().is_some()
    }

    /// Let go of every real disk of the host's, wherever it hangs, and say how
    /// many went.
    ///
    /// A drive is powered by the machine: with the Amiga off it stops, and an
    /// off machine holds nothing. What it lets go of is only its borrowed
    /// copy -- the session's own hold (`blockdev`'s reservation) stays, so
    /// the disk is still taken from the host and powering on lends it again
    /// without a second permission prompt. Image-backed drives are untouched:
    /// a file is held against nobody.
    pub fn release_host_disks(&mut self) -> usize {
        let mut released = 0;
        if let Some(gayle) = self.gayle.as_mut() {
            released += gayle.release_host_disks();
        }
        if let Some(ide) = self.ide_a4000.as_mut() {
            released += ide.release_host_disks();
        }
        if let Some(sdmac) = self.sdmac.as_mut() {
            released += sdmac.release_host_disks();
        }
        for device in &mut self.devices {
            released += match device {
                crate::zorro_device::BoardDevice::A2091(board) => board.release_host_disks(),
                crate::zorro_device::BoardDevice::A4091(board) => board.release_host_disks(),
                crate::zorro_device::BoardDevice::IdeZorro(board) => board.release_host_disks(),
                _ => 0,
            };
        }
        released
    }

    /// Open the configured real disks again and give them back to the machine,
    /// saying how many went back on.
    ///
    /// The counterpart to [`Self::release_host_disks`], and the reason that one
    /// is safe: a machine powered on borrows its drives again, exactly as the
    /// physical floppy drives are taken back. Nothing is asked of the user --
    /// the disks are still held by the session's reservation, having only been
    /// taken off the emulated cable -- and a disk that has since been
    /// unplugged is reported and skipped, leaving that slot empty as it would
    /// be on real hardware.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn attach_host_disks(&mut self, cfg: &crate::config::Config) -> usize {
        let mut attached = 0;
        for disk in &cfg.host_disks {
            // Ide/Lide/Scsi each open through their own driver and land on
            // their own controller, so the dispatch is a straight match on
            // the attachment point rather than routing everything through a
            // shared slot number.
            let opened = match disk.attach {
                crate::config::HostDiskAttach::IdeMaster
                | crate::config::HostDiskAttach::IdeSlave => {
                    let slot = match disk.attach {
                        crate::config::HostDiskAttach::IdeMaster => 0,
                        _ => 1,
                    };
                    if self.gayle.is_none() && self.ide_a4000.is_none() {
                        false
                    } else {
                        match crate::ata::IdeDrive::open_host_disk(
                            &disk.device,
                            disk.fingerprint.as_deref(),
                            disk.identity_confirmed,
                            disk.writable,
                        ) {
                            Ok(drive) => {
                                if let Some(gayle) = self.gayle.as_mut() {
                                    gayle.attach_drive(slot, drive);
                                } else if let Some(ide) = self.ide_a4000.as_mut() {
                                    ide.attach_drive(slot, drive);
                                }
                                true
                            }
                            Err(error) => {
                                log::warn!(
                                    "ide: {} asked for host disk {}, which is not available: \
                                     {error}",
                                    disk.attach.label(),
                                    disk.device
                                );
                                false
                            }
                        }
                    }
                }
                crate::config::HostDiskAttach::LideMaster(ch)
                | crate::config::HostDiskAttach::LideSlave(ch) => {
                    let channel = usize::from(ch);
                    let slot =
                        matches!(disk.attach, crate::config::HostDiskAttach::LideSlave(_)) as usize;
                    let board = self.devices.iter_mut().find_map(|device| match device {
                        crate::zorro_device::BoardDevice::IdeZorro(board) => Some(board),
                        _ => None,
                    });
                    match board {
                        None => false,
                        Some(board) => match crate::ata::IdeDrive::open_host_disk(
                            &disk.device,
                            disk.fingerprint.as_deref(),
                            disk.identity_confirmed,
                            disk.writable,
                        ) {
                            Ok(drive) => {
                                board.attach_drive(channel, slot, drive);
                                true
                            }
                            Err(error) => {
                                log::warn!(
                                    "lide: {} asked for host disk {}, which is not available: \
                                     {error}",
                                    disk.attach.label(),
                                    disk.device
                                );
                                false
                            }
                        },
                    }
                }
                crate::config::HostDiskAttach::Scsi(unit) => {
                    let unit = usize::from(unit);
                    // Which controller is to have it, settled before the disk
                    // is opened: a drive is moved into one place, so there is
                    // no second place to try once it exists.
                    let board = self.devices.iter().position(|device| {
                        matches!(
                            device,
                            crate::zorro_device::BoardDevice::A2091(_)
                                | crate::zorro_device::BoardDevice::A4091(_)
                        )
                    });
                    if self.sdmac.is_none() && board.is_none() {
                        continue;
                    }
                    match crate::scsi::ScsiDisk::open_host_disk(
                        &disk.device,
                        disk.fingerprint.as_deref(),
                        disk.identity_confirmed,
                        disk.writable,
                    ) {
                        Ok(drive) => {
                            if let Some(sdmac) = self.sdmac.as_mut() {
                                sdmac.attach_drive(unit, drive);
                            } else if let Some(at) = board {
                                match &mut self.devices[at] {
                                    crate::zorro_device::BoardDevice::A2091(board) => {
                                        board.attach_drive(unit, drive);
                                    }
                                    crate::zorro_device::BoardDevice::A4091(board) => {
                                        board.attach_drive(unit, drive);
                                    }
                                    _ => {}
                                }
                            }
                            true
                        }
                        Err(error) => {
                            log::warn!(
                                "scsi: unit {unit} asked for host disk {}, which is not \
                                 available: {error}",
                                disk.device
                            );
                            false
                        }
                    }
                }
            };
            if opened {
                attached += 1;
                log::info!(
                    "{}: host disk {} back on with the machine",
                    disk.attach.label(),
                    disk.device
                );
            }
        }
        attached
    }

    /// The first SCSI or ATAPI CD-ROM drive across the machine's storage
    /// buses (the A3000 motherboard SCSI, then the Zorro SCSI boards, then
    /// Gayle/A4000/lide IDE), when one is fitted.
    pub fn scsi_cd_ref(&self) -> Option<&crate::scsi::ScsiCdRom> {
        if let Some(cd) = self.sdmac.as_ref().and_then(crate::sdmac::Sdmac::first_cd) {
            return Some(cd);
        }
        if let Some(cd) = self.devices.iter().find_map(|dev| match dev {
            crate::zorro_device::BoardDevice::A2091(board) => board.first_cd(),
            crate::zorro_device::BoardDevice::A4091(board) => board.first_cd(),
            _ => None,
        }) {
            return Some(cd);
        }
        if let Some(cd) = self.gayle.as_ref().and_then(Gayle::first_atapi_ref) {
            return Some(cd);
        }
        if let Some(cd) = self
            .ide_a4000
            .as_ref()
            .and_then(crate::ide_a4000::IdeA4000::first_atapi_ref)
        {
            return Some(cd);
        }
        self.devices.iter().find_map(|dev| match dev {
            crate::zorro_device::BoardDevice::IdeZorro(board) => board.first_atapi_ref(),
            _ => None,
        })
    }

    /// Mutable view of the first SCSI or ATAPI CD-ROM drive; the disc-swap
    /// target.
    pub fn scsi_cd_mut(&mut self) -> Option<&mut crate::scsi::ScsiCdRom> {
        if self
            .sdmac
            .as_ref()
            .is_some_and(|sdmac| sdmac.first_cd().is_some())
        {
            return self
                .sdmac
                .as_mut()
                .and_then(crate::sdmac::Sdmac::first_cd_mut);
        }
        if self.devices.iter().any(|dev| {
            matches!(dev, crate::zorro_device::BoardDevice::A2091(board) if board.first_cd().is_some())
                || matches!(dev, crate::zorro_device::BoardDevice::A4091(board) if board.first_cd().is_some())
        }) {
            return self.devices.iter_mut().find_map(|dev| match dev {
                crate::zorro_device::BoardDevice::A2091(board) => board.first_cd_mut(),
                crate::zorro_device::BoardDevice::A4091(board) => board.first_cd_mut(),
                _ => None,
            });
        }
        if self
            .gayle
            .as_ref()
            .is_some_and(|gayle| gayle.first_atapi_ref().is_some())
        {
            return self.gayle.as_mut().and_then(Gayle::first_atapi_mut);
        }
        if self
            .ide_a4000
            .as_ref()
            .is_some_and(|ide| ide.first_atapi_ref().is_some())
        {
            return self
                .ide_a4000
                .as_mut()
                .and_then(crate::ide_a4000::IdeA4000::first_atapi_mut);
        }
        self.devices.iter_mut().find_map(|dev| match dev {
            crate::zorro_device::BoardDevice::IdeZorro(board) => board.first_atapi_mut(),
            _ => None,
        })
    }

    /// The SCSI CD-ROM drive's playback status line for the debugger's
    /// audio tab, when a drive is present and a play operation has state.
    pub fn scsi_cd_playback_line(&self) -> Option<String> {
        self.scsi_cd_ref()
            .and_then(crate::scsi::ScsiCdRom::playback_line)
    }

    /// The Toccata sound board, when one is configured, for the debugger's
    /// audio tab.
    pub fn toccata_board(&self) -> Option<&crate::toccata::Toccata> {
        self.devices.iter().find_map(|dev| match dev {
            crate::zorro_device::BoardDevice::Toccata(board) => Some(board.as_ref()),
            _ => None,
        })
    }

    /// The copperhf.device board, when one is configured (`[copperhf]`), for
    /// the CCP `copperhf.*` hot attach/detach methods.
    pub fn copperhf_board_mut(&mut self) -> Option<&mut crate::copperhf::CopperhfBoard> {
        self.devices.iter_mut().find_map(|dev| match dev {
            crate::zorro_device::BoardDevice::Copperhf(board) => Some(board),
            _ => None,
        })
    }

    /// Block until the copperhf.device board (if configured) has no
    /// requests in flight -- every completion already surfaced, every
    /// guest-visible effect already applied. A no-op if there is no
    /// `[copperhf]` board or nothing is in flight. Required before any of
    /// `CopperhfBoard`'s quiesced-only unit mutators (attach/eject) run, and
    /// before serializing a save state, so neither races the worker
    /// thread's own view of the unit table (`src/copperhf.rs`'s module doc,
    /// "Ownership and quiescing").
    pub fn copperhf_quiesce(&mut self) {
        let Self { devices, mem, .. } = self;
        for dev in devices.iter_mut() {
            if let crate::zorro_device::BoardDevice::Copperhf(board) = dev {
                let mut host = crate::zorro_device::DeviceHost::new(mem);
                board.quiesce(&mut host);
            }
        }
    }

    /// The MHI MPEG decoder board, when one is configured, for the
    /// debugger's audio tab.
    #[cfg(feature = "mhi")]
    pub fn mhi_board(&self) -> Option<&crate::mhi::Mhi> {
        self.devices.iter().find_map(|dev| match dev {
            crate::zorro_device::BoardDevice::Mhi(board) => Some(board.as_ref()),
            _ => None,
        })
    }

    /// Immutable CD32 FMV genlock/video snapshot for the native renderer.
    /// The decoded pixel storage is shared, so taking one for the render
    /// worker is constant-time.
    #[cfg(feature = "cd32-fmv")]
    pub fn fmv_presentation(&self) -> Option<crate::cd32_fmv::FmvPresentation> {
        self.devices.iter().find_map(|dev| match dev {
            crate::zorro_device::BoardDevice::Cd32Fmv(board) => Some(board.presentation()),
            _ => None,
        })
    }

    /// Whether the machine has a hard-disk controller, which is what gives it
    /// an HDD LED: a Zorro board, or the A3000's motherboard SCSI.
    fn has_hard_disk_controller(&self) -> bool {
        self.sdmac.is_some()
            || self.devices.iter().any(|d| {
                matches!(
                    d,
                    crate::zorro_device::BoardDevice::A2091(_)
                        | crate::zorro_device::BoardDevice::A4091(_)
                        | crate::zorro_device::BoardDevice::IdeZorro(_)
                )
            })
    }

    /// Whether a host-folder filesystem board serves at least one mount. Such
    /// a board has no disk controller, but its packet traffic rides the HDD
    /// LED like any other Zorro storage device.
    fn has_filesys_mount(&self) -> bool {
        self.devices.iter().any(|d| {
            matches!(
                d,
                crate::zorro_device::BoardDevice::Filesys(f) if f.has_mounts()
            )
        })
    }

    /// Whether an RTG board is driving the display (native chipset output
    /// is presented otherwise). Cheap; safe to poll every frame.
    pub fn rtg_active(&self) -> bool {
        self.devices.iter().any(|d| match d {
            crate::zorro_device::BoardDevice::Z3660(z) => z.rtg_active(),
            crate::zorro_device::BoardDevice::Picasso2(p) => p.rtg_active(),
            crate::zorro_device::BoardDevice::GraffityZ2(g) => g.rtg_active(),
            crate::zorro_device::BoardDevice::GraffityZ3(g) => g.rtg_active(),
            _ => false,
        })
    }

    /// Compose the active RTG board frame into `out` as
    /// presentation pixels, returning its dimensions; `None` while no RTG
    /// board is driving the display (native chipset output shown).
    pub fn rtg_frame(&self, out: &mut Vec<u32>) -> Option<(u32, u32)> {
        self.devices.iter().find_map(|d| match d {
            crate::zorro_device::BoardDevice::Z3660(z) => z.rtg_frame(out),
            crate::zorro_device::BoardDevice::Picasso2(p) => p.rtg_frame(out),
            crate::zorro_device::BoardDevice::GraffityZ2(g) => g.rtg_frame(out),
            crate::zorro_device::BoardDevice::GraffityZ3(g) => g.rtg_frame(out),
            _ => None,
        })
    }

    /// Whether a disc is mounted (or waiting in the tray).
    pub fn cd_disc_inserted(&self) -> bool {
        self.cdtv
            .as_ref()
            .map(crate::cdtv::CdtvController::has_disc)
            .or_else(|| self.akiko.as_ref().map(crate::akiko::Akiko::has_disc))
            .or_else(|| self.scsi_cd_ref().map(crate::scsi::ScsiCdRom::has_disc))
            .unwrap_or(false)
    }

    /// Runtime disc insert with media-change notification. On CDTV the
    /// disc lands after a short tray delay (the same media-change STCH
    /// path as `[cd] insert_delay`); Akiko mounts immediately and
    /// volunteers a media-status packet; a SCSI CD-ROM mounts after its
    /// tray delay and raises a medium-change unit attention. `path` names
    /// the image for the SCSI drive's logs.
    pub fn cd_insert_disc(&mut self, image: crate::cdrom::CdImage, path: &std::path::Path) {
        // A second or so of tray time keeps the eject and insert
        // media-change interrupts distinct for cdtv.device.
        const CDTV_TRAY_SECS: f64 = 1.0;
        if let Some(cdtv) = self.cdtv.as_mut() {
            cdtv.eject_disc();
            cdtv.insert_disc_after(image, CDTV_TRAY_SECS);
        } else if let Some(akiko) = self.akiko.as_mut() {
            // Model tray time on Akiko too: eject (media-absent) then mount
            // after a delay (media-present), so cd.device sees the
            // absent->present change instead of an instantaneous swap.
            akiko.insert_disc_after(image, CDTV_TRAY_SECS);
        } else if let Some(cd) = self.scsi_cd_mut() {
            cd.swap_disc(image, path);
        }
    }

    /// Runtime disc eject with media-change notification.
    pub fn cd_eject_disc(&mut self) {
        if let Some(cdtv) = self.cdtv.as_mut() {
            cdtv.eject_disc();
        } else if let Some(akiko) = self.akiko.as_mut() {
            akiko.eject_disc();
        } else if let Some(cd) = self.scsi_cd_mut() {
            cd.eject();
        }
    }

    /// Record Gayle IDE activity: keep the HDD LED lit for a short stretch
    /// of emulated time (transfers complete synchronously, so without a
    /// hold the LED would never be visibly on).
    pub fn note_hdd_activity(&mut self) {
        // ~100 ms of emulated time per activity burst.
        const HDD_LED_HOLD_CCK: u64 = (PAULA_CLOCK_HZ / 10) as u64;
        self.hdd_led_until_cck = self.emulated_cck + HDD_LED_HOLD_CCK;
    }

    /// Move the host-side resources that a save state does not capture from
    /// the currently live Bus into this freshly deserialized one: the Paula
    /// audio/serial sinks, serial observer, and the open blitter trace file. The realtime
    /// device-clock anchor needs no carry-over -- it deserializes as None
    /// and `realtime_cck_due` re-anchors to the host clock on first use.
    pub(crate) fn adopt_host_resources(&mut self, live: &mut Bus) -> anyhow::Result<()> {
        // Open fallible resources before moving anything out of the live bus,
        // preserving apply_state's all-or-nothing contract on bridge failure.
        for device in &mut self.devices {
            if let crate::zorro_device::BoardDevice::A2065(board) = device {
                board.reattach_backend()?;
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        self.materialize_saved_host_disks()?;
        std::mem::swap(&mut self.paula.serial, &mut live.paula.serial);
        // A host sink is not serialized. Give a stateful one one explicit
        // timeline-jump boundary after it moves, so it cannot retain data
        // from the abandoned future.
        self.paula.serial.reset_after_timeline_jump();
        // The sink just moved onto this deserialized CIA-B's PRA/DDRA, whose
        // /DTR and /RTS levels it never saw a write for (the swap bypasses
        // `cia_b_write`). Push them once so a resumed machine's DTR/RTS reach
        // the sink exactly as if the guest had just written them.
        let pra = self.cia_b.port_a_pins();
        self.paula
            .serial
            .set_control_outputs(pra & 0x80 == 0, pra & 0x40 == 0);
        std::mem::swap(
            &mut self.paula.serial_observer,
            &mut live.paula.serial_observer,
        );
        std::mem::swap(&mut self.paula.audio, &mut live.paula.audio);
        std::mem::swap(&mut self.parallel_port, &mut live.parallel_port);
        self.blitter_trace = live.blitter_trace.take();
        self.paula.adopt_host_taps(&mut live.paula);
        // Bus-event subscriptions belong to live control connections, not to
        // the emulated timeline. Preserve their queue and monotonic sequence
        // so an observer's existing cursor remains valid after state.load.
        std::mem::swap(&mut self.bus_event_observers, &mut live.bus_event_observers);
        std::mem::swap(&mut self.bus_events, &mut live.bus_events);
        std::mem::swap(
            &mut self.bus_event_next_sequence,
            &mut live.bus_event_next_sequence,
        );
        // Drive speed is host configuration, not machine state: a loaded
        // state keeps the running session's setting.
        self.floppy.set_speed_percent(live.floppy.speed_percent());
        Ok(())
    }

    /// Why the live machine cannot safely execute frames that are then
    /// rewound. This covers dynamic host couplings that a static config
    /// cannot: mounted media, persistent clock/NVRAM storage, peripherals,
    /// and active trace writers.
    pub fn runahead_host_block_reason(&self) -> Option<&'static str> {
        if let Some(reason) = self.floppy.runahead_block_reason() {
            return Some(reason);
        }
        if self.cd_disc_inserted() {
            // CdImage deserialization reopens its source files. Keeping that
            // out of a per-refresh restore also avoids replaying host-backed
            // decoder state for audio tracks.
            return Some("CD image mounted");
        }
        if self
            .akiko
            .as_ref()
            .is_some_and(crate::akiko::Akiko::persistent_nvram)
        {
            return Some("persistent CD32 NVRAM");
        }
        if self.rtc_present && !self.rtc.runahead_safe() {
            return Some("live or persistent real-time clock");
        }
        if !self.parallel_port.runahead_safe() {
            return Some("parallel host peripheral");
        }
        if self.wave_on {
            return Some("waveform capture");
        }
        if self.blitter_trace.is_some()
            || crate::envcfg::var_os("COPPERLINE_DUMP_BLITMEM").is_some()
        {
            return Some("file-backed hardware trace");
        }
        None
    }

    /// Why transient debugger state makes speculative execution unsafe.
    /// These observers are deliberately absent from save states, and restore
    /// carries some of them forward from the abandoned Bus. Letting them run
    /// speculatively would consume one-shot faults, record discarded accesses,
    /// or silently disarm the frame analyzer on every anchor restore.
    pub fn runahead_debug_block_reason(&self) -> Option<&'static str> {
        if !self.ui_beam_traps.is_empty() || !self.ui_copper_breaks.is_empty() {
            return Some("debugger stop conditions armed");
        }
        if self.bus_faults_armed() {
            return Some("injected bus fault armed");
        }
        if self.chipset_validation_armed() {
            return Some("chipset validation armed");
        }
        if self.smc_detection_armed() {
            return Some("SMC detection armed");
        }
        if self.heat_map_armed() {
            return Some("memory heat map armed");
        }
        if self.frame_analyzer_enabled {
            return Some("frame analyzer armed");
        }
        None
    }

    /// Acquire physical disks named by a fully decoded state. Deserialization
    /// itself only creates placeholders; doing the fallible host work here
    /// prevents malformed trailing state from unmounting media. The reservation
    /// transaction and explicit drive removal make a multi-disk failure atomic.
    #[cfg(not(target_arch = "wasm32"))]
    fn materialize_saved_host_disks(&mut self) -> anyhow::Result<()> {
        let mut pending = Vec::new();
        if let Some(gayle) = &self.gayle {
            gayle.pending_host_disks(&mut pending);
        }
        if let Some(ide) = &self.ide_a4000 {
            ide.pending_host_disks(&mut pending);
        }
        if let Some(sdmac) = &self.sdmac {
            sdmac.wd.pending_host_disks(&mut pending);
        }
        for device in &self.devices {
            match device {
                crate::zorro_device::BoardDevice::A2091(board) => {
                    board.wd.pending_host_disks(&mut pending);
                }
                crate::zorro_device::BoardDevice::A4091(board) => {
                    board.pending_host_disks(&mut pending);
                }
                crate::zorro_device::BoardDevice::IdeZorro(board) => {
                    board.pending_host_disks(&mut pending);
                }
                _ => {}
            }
        }
        if pending.is_empty() {
            return Ok(());
        }

        let mut reservations = crate::blockdev::ReservationTransaction::begin(&pending)?;
        let result = (|| {
            if let Some(gayle) = &mut self.gayle {
                gayle.materialize_host_disks()?;
            }
            if let Some(ide) = &mut self.ide_a4000 {
                ide.materialize_host_disks()?;
            }
            if let Some(sdmac) = &mut self.sdmac {
                sdmac.wd.materialize_host_disks()?;
            }
            for device in &mut self.devices {
                match device {
                    crate::zorro_device::BoardDevice::A2091(board) => {
                        board.wd.materialize_host_disks()?;
                    }
                    crate::zorro_device::BoardDevice::A4091(board) => {
                        board.materialize_host_disks()?;
                    }
                    crate::zorro_device::BoardDevice::IdeZorro(board) => {
                        board.materialize_host_disks()?;
                    }
                    _ => {}
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            // Close every lent handle before dropping its reservation, so a
            // backend can remount volumes immediately rather than racing the
            // still-live candidate bus.
            self.release_host_disks();
            reservations.rollback();
            return Err(error);
        }
        reservations.commit();
        Ok(())
    }

    pub(crate) fn rollback_state(&self) -> RollbackState {
        RollbackState {
            data_bus: self.data_bus,
            render_blocked: self.current_frame_render_blocked,
            frame_lines: [
                self.current_frame_geometry.frame_lines,
                self.last_frame_geometry.frame_lines,
            ],
            h_windows: [
                self.current_frame_presentation_h_window,
                self.last_frame_presentation_h_window,
            ],
            v_windows: [
                self.current_frame_presentation_v_window,
                self.last_frame_presentation_v_window,
            ],
        }
    }

    pub(crate) fn restore_rollback_state(&mut self, state: RollbackState) {
        self.data_bus = state.data_bus;
        self.current_frame_render_blocked = state.render_blocked;
        [
            self.current_frame_geometry.frame_lines,
            self.last_frame_geometry.frame_lines,
        ] = state.frame_lines;
        [
            self.current_frame_presentation_h_window,
            self.last_frame_presentation_h_window,
        ] = state.h_windows;
        [
            self.current_frame_presentation_v_window,
            self.last_frame_presentation_v_window,
        ] = state.v_windows;
        // These are derived acceleration caches. Rebuild through the dynamic
        // path until the next line, preserving all captured events and latches.
        self.wide_bitplane_hot_line.invalidate();
        self.wide_bitplane_dynamic_vpos.set(Some(self.agnus.vpos));
    }

    pub(crate) fn reset_transient_video_after_state_load(&mut self) {
        let current_frame_lines = self.frame_lines_for_geometry(
            self.current_frame_geometry,
            self.current_frame_render_base.long_field,
        );
        self.current_frame_geometry.frame_lines = current_frame_lines;
        self.last_frame_geometry.frame_lines = current_frame_lines;
        if self.current_frame_geometry.programmable {
            self.current_frame_visible_start_vpos = self.current_frame_geometry.visible_start_vpos;
        } else {
            self.current_frame_visible_start_vpos = RENDER_MIN_OVERSCAN_START_VPOS;
            self.current_frame_geometry.visible_start_vpos = RENDER_MIN_OVERSCAN_START_VPOS;
        }
        self.last_frame_visible_start_vpos = self.current_frame_visible_start_vpos;
        self.last_frame_geometry.visible_start_vpos = self.current_frame_visible_start_vpos;
        self.current_frame_presentation_h_window = self.compute_presentation_h_window();
        self.last_frame_presentation_h_window = self.current_frame_presentation_h_window;
        self.current_frame_presentation_v_window = self.compute_presentation_v_window();
        self.last_frame_presentation_v_window = self.current_frame_presentation_v_window;
        self.lazy_collision_vpos = self.current_frame_visible_start_vpos;
        self.ocs_same_line_diw_start_blocked_vpos = None;
        // The vertical display flop travels in the state (it is
        // history-dependent, and control-session snapshots can be taken
        // mid-frame where no register-derived reconstruction is exact), so
        // nothing to re-derive here.
        // Per-line wide-FMODE cache eligibility is deliberately not part of
        // the save-state schema. A restored line may contain a DDF, FMODE or
        // delayed BPLCON0/DMACON transition, so rebuilding one whole-line mask
        // from the register value at the restore point could skip the dynamic
        // path's previous-value/block-boundary handling. Keep the remainder of
        // this line dynamic; rollover makes an unchanged following line
        // eligible for publication again.
        self.wide_bitplane_hot_line.invalidate();
        self.wide_bitplane_dynamic_vpos.set(Some(self.agnus.vpos));
        self.reset_frame_capture_buffers();
        self.current_frame_render_blocked = self.agnus.vpos != 0 || self.agnus.hpos != 0;
    }

    pub(crate) fn reset_transient_diagnostics_after_state_load(&mut self) {
        // The code-path ring that consumes this alarm is debugger-local state,
        // so a serialized pending alarm has no useful history after a restore.
        self.diag_lowmem_blit = false;
        // Diagnostics are start-up settings, not machine state: re-derive the
        // env-gated flags so a loaded state honours the current environment
        // rather than the one the state was saved under.
        self.dbg_slotmap_on = crate::envcfg::flag("COPPERLINE_DIAG_SLOTMAP");
        self.dbg_slotmap_dumped = false;
        self.bus_accounting = BusAccounting::from_env();
        self.trace_intreq = self.paula.intreq & IRQ_SOURCE_BITS;
        self.trace_cia_irq_pins = (
            self.cia_a.irq_line_asserted(),
            self.cia_b.irq_line_asserted(),
        );
        self.refresh_chip_bus_observers();
        if self.frame_analyzer_enabled {
            self.reset_current_frame_bus_trace(true);
        }
    }

    pub fn emulated_seconds(&self) -> f64 {
        self.emulated_cck as f64 / PAULA_CLOCK_HZ as f64
    }

    pub fn emulated_frames(&self) -> u64 {
        self.emulated_frames
    }

    /// Total colour clocks emulated since power-on. The monotonic timeline
    /// coordinate behind `emulated_seconds`; used by reverse debugging to
    /// label snapshots and report the beam time of a reconstructed event.
    pub fn emulated_cck(&self) -> u64 {
        self.emulated_cck
    }

    /// Publish the emulated-to-host time mapping to the serial sink so a
    /// timing-sensitive backend (MIDI) can schedule output. See
    /// [`crate::serial::SerialTimeAnchor`].
    pub fn set_serial_time_anchor(&mut self, anchor: crate::serial::SerialTimeAnchor) {
        self.paula.set_serial_time_anchor(anchor);
    }

    /// Attach a Centronics peripheral to CIA-A port B. The default is a null
    /// peripheral (unplugged cable), so attaching one is an explicit host-side
    /// choice and does not change ordinary machine behavior.
    pub fn attach_parallel_port(&mut self, port: Box<dyn crate::parallel::ParallelPort>) {
        self.parallel_port = port;
    }

    pub fn flush_parallel_port(&mut self) -> std::io::Result<()> {
        self.parallel_port.flush()
    }

    /// The live MIDI sink, when the serial port is in MIDI mode, for switching
    /// devices from the runtime menu.
    #[cfg(feature = "midi")]
    pub fn midi_serial_mut(&mut self) -> Option<&mut crate::midi::MidiSerialSink> {
        self.paula.serial.as_midi()
    }

    /// The same sink shared, for state that is read rather than switched.
    #[cfg(feature = "midi")]
    pub fn midi_serial(&self) -> Option<&crate::midi::MidiSerialSink> {
        self.paula.serial.as_midi_ref()
    }

    pub fn live_audio_output_lead_seconds(&self) -> f64 {
        self.paula.live_audio_output_lead_seconds()
    }

    pub fn live_audio_status(&self) -> crate::audio::AudioRuntimeStatus {
        self.paula.live_audio_status()
    }

    pub fn set_live_audio_suspended(&mut self, suspended: bool) {
        self.paula.set_live_audio_suspended(suspended);
    }

    pub fn set_live_audio_discard(&mut self, on: bool) {
        self.paula.set_live_audio_discard(on);
    }

    pub fn reset_live_audio_after_timeline_jump(&mut self) {
        self.paula.reset_live_audio_after_timeline_jump();
    }

    pub fn output_volume_percent(&self) -> u8 {
        self.paula.output_volume_percent()
    }

    pub fn set_output_volume_percent(&mut self, percent: u8) {
        self.paula.set_output_volume_percent(percent);
    }

    pub fn adjust_output_volume_percent(&mut self, delta: i16) {
        let adjusted = i16::from(self.output_volume_percent()).saturating_add(delta);
        self.set_output_volume_percent(adjusted.clamp(0, 100) as u8);
    }

    pub fn dump_video_pipeline_stats(&self, label: &str) {
        self.video_pipeline_stats.dump(label);
    }

    pub fn reset_profile_stats(&mut self) {
        self.video_pipeline_stats = VideoPipelineStats::default();
        self.poll_stats = PollStats::default();
    }

    #[inline(always)]
    pub(crate) fn record_video_render_frame(&mut self, timing: VideoRenderFrameTiming) {
        #[cfg(feature = "profile-stats")]
        {
            let stats = &mut self.video_pipeline_stats;
            stats.render_frames = stats.render_frames.saturating_add(1);
            stats.render_events = stats.render_events.saturating_add(timing.events);
            stats.render_control_segments = stats
                .render_control_segments
                .saturating_add(timing.control_segments);
            stats.render_playfield_pixels = stats
                .render_playfield_pixels
                .saturating_add(timing.playfield_pixels);
            stats.render_manual_bpl_segments = stats
                .render_manual_bpl_segments
                .saturating_add(timing.manual_bpl_segments);
            stats.render_sprite_lines = stats
                .render_sprite_lines
                .saturating_add(timing.sprite_lines);
            stats.render_total_nanos = stats.render_total_nanos.saturating_add(timing.total_nanos);
            stats.render_event_nanos = stats.render_event_nanos.saturating_add(timing.event_nanos);
            stats.render_background_nanos = stats
                .render_background_nanos
                .saturating_add(timing.background_nanos);
            stats.render_playfield_nanos = stats
                .render_playfield_nanos
                .saturating_add(timing.playfield_nanos);
            stats.render_manual_bpl_nanos = stats
                .render_manual_bpl_nanos
                .saturating_add(timing.manual_bpl_nanos);
            stats.render_sprite_nanos = stats
                .render_sprite_nanos
                .saturating_add(timing.sprite_nanos);
        }
        #[cfg(not(feature = "profile-stats"))]
        {
            let _ = timing;
        }
    }

    #[inline(always)]
    fn record_bitplane_fetch_timing(
        &mut self,
        slots: usize,
        rows_started: usize,
        rows_completed: usize,
        elapsed: Option<(Duration, u128)>,
    ) {
        #[cfg(feature = "profile-stats")]
        {
            if slots == 0 && rows_started == 0 && rows_completed == 0 {
                return;
            }
            let stats = &mut self.video_pipeline_stats;
            stats.bitplane_fetch_calls = stats.bitplane_fetch_calls.saturating_add(1);
            stats.bitplane_fetch_slots = stats.bitplane_fetch_slots.saturating_add(slots as u64);
            stats.bitplane_fetch_rows_started = stats
                .bitplane_fetch_rows_started
                .saturating_add(rows_started as u64);
            stats.bitplane_fetch_rows_completed = stats
                .bitplane_fetch_rows_completed
                .saturating_add(rows_completed as u64);
            VideoPipelineStats::add_sampled_duration(&mut stats.bitplane_fetch_nanos, elapsed);
        }
        #[cfg(not(feature = "profile-stats"))]
        {
            let _ = (slots, rows_started, rows_completed, elapsed);
        }
    }

    #[inline(always)]
    fn record_sprite_fetch_timing(
        &mut self,
        pair_slots: usize,
        lines: usize,
        elapsed: Option<(Duration, u128)>,
    ) {
        #[cfg(feature = "profile-stats")]
        {
            if pair_slots == 0 {
                return;
            }
            let stats = &mut self.video_pipeline_stats;
            stats.sprite_fetch_calls = stats.sprite_fetch_calls.saturating_add(1);
            stats.sprite_fetch_pair_slots = stats
                .sprite_fetch_pair_slots
                .saturating_add(pair_slots as u64);
            stats.sprite_fetch_lines = stats.sprite_fetch_lines.saturating_add(lines as u64);
            VideoPipelineStats::add_sampled_duration(&mut stats.sprite_fetch_nanos, elapsed);
        }
        #[cfg(not(feature = "profile-stats"))]
        {
            let _ = (pair_slots, lines, elapsed);
        }
    }

    #[inline(always)]
    fn record_live_collision_timing(
        &mut self,
        pixels: u64,
        control_segments: usize,
        full_line_scan: bool,
        elapsed: Option<(Duration, u128)>,
    ) {
        #[cfg(feature = "profile-stats")]
        {
            if pixels == 0 {
                return;
            }
            let stats = &mut self.video_pipeline_stats;
            stats.collision_calls = stats.collision_calls.saturating_add(1);
            stats.collision_pixels = stats.collision_pixels.saturating_add(pixels);
            stats.collision_control_segments = stats
                .collision_control_segments
                .saturating_add(control_segments as u64);
            if full_line_scan {
                stats.collision_full_line_scans = stats.collision_full_line_scans.saturating_add(1);
            }
            VideoPipelineStats::add_sampled_duration(&mut stats.collision_nanos, elapsed);
        }
        #[cfg(not(feature = "profile-stats"))]
        {
            let _ = (pixels, control_segments, full_line_scan, elapsed);
        }
    }

    fn ensure_current_collision_control_index(&mut self) {
        if self.current_frame_collision_control_index.is_none() {
            self.current_frame_collision_control_index = Some(
                BeamEventIndex::from_register_writes(&self.current_frame_collision_control_events),
            );
        }
    }

    fn ensure_current_collision_bpldat_index(&mut self) {
        if self.current_frame_collision_bpldat_index.is_none() {
            self.current_frame_collision_bpldat_index = Some(BeamEventIndex::from_register_writes(
                &self.current_frame_collision_bpldat_events,
            ));
        }
    }

    fn ensure_current_collision_sprite_index(&mut self) {
        if self.current_frame_collision_sprite_index.is_none() {
            self.current_frame_collision_sprite_index = Some(BeamEventIndex::from_register_writes(
                &self.current_frame_collision_sprite_events,
            ));
        }
    }

    fn live_playfield_collision_may_have_dual_playfield(&self) -> bool {
        self.current_frame_collision_may_have_dual_playfield
            || self.denise.bplcon0 & 0x0400 != 0
            || self.current_frame_render_base.bplcon0 & 0x0400 != 0
    }

    fn trace_blitter_start(&mut self, bltsize: u16, source: BeamWriteSource) {
        let mut h = ((bltsize >> 6) & 0x03FF) as u32;
        if h == 0 {
            h = 1024;
        }
        let mut w = (bltsize & 0x003F) as u32;
        if w == 0 {
            w = 64;
        }
        self.trace_blitter_start_dims("bltsize", bltsize, h, w, source);
    }

    fn trace_blitter_start_ecs(&mut self, bltsizh: u16, source: BeamWriteSource) {
        let mut h = (self.blitter.bltsizv & 0x7FFF) as u32;
        if h == 0 {
            h = 32_768;
        }
        let mut w = (bltsizh & 0x07FF) as u32;
        if w == 0 {
            w = 2_048;
        }
        self.trace_blitter_start_dims("bltsizh", bltsizh, h, w, source);
    }

    fn trace_blitter_start_dims(
        &mut self,
        event_name: &str,
        size_register: u16,
        h: u32,
        w: u32,
        source: BeamWriteSource,
    ) {
        if self.blitter_trace.is_none() {
            return;
        }
        let con0 = self.blitter.bltcon0;
        let con1 = self.blitter.bltcon1;
        let line = con1 & 0x0001 != 0;
        let fill = !line && con1 & 0x0018 != 0;
        let source = beam_write_source_name(source);
        let entry = format!(
            "{{\"event\":\"{}\",\"source\":\"{}\",\"emu_secs\":{:.6},\"emu_frame\":{},\"vpos\":{},\"hpos\":{},\"bltsize\":{},\"h\":{},\"w\":{},\"line\":{},\"fill\":{},\"line_octant\":{},\"bltcon0\":{},\"bltcon1\":{},\"use_a\":{},\"use_b\":{},\"use_c\":{},\"use_d\":{},\"lf\":{},\"ash\":{},\"bsh\":{},\"sign\":{},\"sing\":{},\"desc\":{},\"ife\":{},\"efe\":{},\"fci\":{},\"bltafwm\":{},\"bltalwm\":{},\"bltapt\":{},\"bltbpt\":{},\"bltcpt\":{},\"bltdpt\":{},\"bltamod\":{},\"bltbmod\":{},\"bltcmod\":{},\"bltdmod\":{},\"bltadat\":{},\"bltbdat\":{},\"bltcdat\":{},\"dmacon\":{},\"fmode\":{},\"bplcon0\":{},\"bplcon1\":{},\"bplcon2\":{},\"bpl1mod\":{},\"bpl2mod\":{},\"ddfstrt\":{},\"ddfstop\":{},\"diwstrt\":{},\"diwstop\":{},\"bplpt\":[{},{},{},{},{},{},{},{}]}}",
            event_name,
            source,
            self.emulated_seconds(),
            self.emulated_frames,
            self.agnus.vpos,
            self.agnus.hpos,
            size_register,
            h,
            w,
            line,
            fill,
            (con1 >> 2) & 0x0007,
            con0,
            con1,
            con0 & 0x0800 != 0,
            con0 & 0x0400 != 0,
            con0 & 0x0200 != 0,
            con0 & 0x0100 != 0,
            con0 & 0x00FF,
            (con0 >> 12) & 0x000F,
            (con1 >> 12) & 0x000F,
            con1 & 0x0040 != 0,
            con1 & 0x0002 != 0,
            con1 & 0x0002 != 0 && !line,
            con1 & 0x0008 != 0,
            con1 & 0x0010 != 0,
            con1 & 0x0004 != 0,
            self.blitter.bltafwm,
            self.blitter.bltalwm,
            self.blitter.bltapt,
            self.blitter.bltbpt,
            self.blitter.bltcpt,
            self.blitter.bltdpt,
            self.blitter.bltamod,
            self.blitter.bltbmod,
            self.blitter.bltcmod,
            self.blitter.bltdmod,
            self.blitter.bltadat,
            self.blitter.bltbdat,
            self.blitter.bltcdat,
            self.agnus.dmacon,
            self.agnus.fmode(),
            self.denise.bplcon0,
            self.denise.bplcon1,
            self.denise.bplcon2,
            self.denise.bpl1mod,
            self.denise.bpl2mod,
            self.denise.ddfstrt,
            self.denise.ddfstop,
            self.denise.diwstrt,
            self.denise.diwstop,
            self.denise.bplpt[0],
            self.denise.bplpt[1],
            self.denise.bplpt[2],
            self.denise.bplpt[3],
            self.denise.bplpt[4],
            self.denise.bplpt[5],
            self.denise.bplpt[6],
            self.denise.bplpt[7],
        );
        if let Some(file) = self.blitter_trace.as_mut() {
            let _ = writeln!(file, "{entry}");
        }
    }

    fn trace_blitter_forced_finish(&mut self, was_busy: bool) {
        let secs = self.emulated_seconds();
        let frames = self.emulated_frames;
        let vpos = self.agnus.vpos;
        let hpos = self.agnus.hpos;
        let Some(file) = self.blitter_trace.as_mut() else {
            return;
        };
        let _ = writeln!(
            file,
            "{{\"event\":\"forced_finish\",\"emu_secs\":{secs:.6},\"emu_frame\":{frames},\"vpos\":{vpos},\"hpos\":{hpos},\"was_busy\":{was_busy}}}"
        );
    }

    fn trace_blitter_completion(&mut self, source: &'static str, intreq_before: u16) {
        let secs = self.emulated_seconds();
        let frames = self.emulated_frames;
        let vpos = self.agnus.vpos;
        let hpos = self.agnus.hpos;
        let intreq = self.paula.intreq;
        let intena = self.paula.intena;
        let dmacon = self.agnus.dmacon;
        let fmode = self.agnus.fmode();
        let busy = self.blitter.busy;
        let bzero = self.blitter.bzero;
        let bltcon0 = self.blitter.bltcon0;
        let bltcon1 = self.blitter.bltcon1;
        let bltdpt = self.blitter.bltdpt;
        let Some(file) = self.blitter_trace.as_mut() else {
            return;
        };
        let _ = writeln!(
            file,
            "{{\"event\":\"completion\",\"source\":\"{source}\",\"emu_secs\":{secs:.6},\"emu_frame\":{frames},\"vpos\":{vpos},\"hpos\":{hpos},\"intreq_before\":{intreq_before},\"intreq\":{intreq},\"intena\":{intena},\"dmacon\":{dmacon},\"fmode\":{fmode},\"busy\":{busy},\"bzero\":{bzero},\"bltcon0\":{bltcon0},\"bltcon1\":{bltcon1},\"bltdpt\":{bltdpt}}}"
        );
    }

    fn trace_dmaconr_read(&mut self, value: u16) {
        let secs = self.emulated_seconds();
        let frames = self.emulated_frames;
        let vpos = self.agnus.vpos;
        let hpos = self.agnus.hpos;
        let busy = self.blitter.busy;
        let bzero = self.blitter.bzero;
        let fmode = self.agnus.fmode();
        let Some(file) = self.blitter_trace.as_mut() else {
            return;
        };
        let _ = writeln!(
            file,
            "{{\"event\":\"dmaconr_read\",\"emu_secs\":{secs:.6},\"emu_frame\":{frames},\"vpos\":{vpos},\"hpos\":{hpos},\"value\":{value},\"fmode\":{fmode},\"busy\":{busy},\"bzero\":{bzero}}}"
        );
    }

    pub fn set_cpu_bus_arbitration_enabled(&mut self, enabled: bool) {
        // Diagnostic builds can force CPU chip-bus arbitration off for A/B
        // timing experiments. Normal builds always keep hardware contention on.
        let enabled = enabled && !no_bus_arb();
        self.cpu_bus_arbitration_enabled = enabled;
        if !enabled {
            self.blitter_slowdown_cpu_misses = 0;
        }
        self.cpu_clock_carry = 0;
        self.ext_clock_carry_x100 = 0;
    }

    /// Configure the CPU clock ratio ([cpu] clock_mhz) so external-access and
    /// CPU-internal billing scale with the configured CPU speed.
    pub fn set_cpu_clocks_per_cck(&mut self, clocks: u32) {
        self.cpu_clocks_per_cck = clocks.max(1);
    }

    /// Select the local bus cycle length: the 68020+ completes its side of an
    /// access in 3 CPU clocks where the 68000 takes 4. Chipset reads still
    /// have a separate data-return phase; writes can be posted after the
    /// shorter cycle. Derived from the CPU model.
    /// Whether the CPU runs the shorter 020+ bus cycle (see the field doc);
    /// also distinguishes the 68000's shared chip data bus from the 020+
    /// local bus for the floating-bus latch.
    pub(crate) fn cpu_short_bus_cycle(&self) -> bool {
        self.cpu_short_bus_cycle
    }

    pub fn set_cpu_short_bus_cycle(&mut self, enabled: bool) {
        self.cpu_short_bus_cycle = enabled;
    }

    /// Advance the chipset for CPU-internal (non-bus) clocks reported by the
    /// cycle-exact core via `AddressBus::sync`. The chip bus stays free for
    /// DMA during this time; timed devices tick along. Sub-cck clock counts
    /// carry over to the next call so no time is lost to the
    /// clocks-per-cck conversion.
    pub fn sync_cpu_internal_clocks(&mut self, cpu_clocks: u32) {
        if !self.cpu_bus_arbitration_enabled {
            return;
        }
        let total = cpu_clocks + std::mem::take(&mut self.cpu_clock_carry);
        let cck = total / self.cpu_clocks_per_cck;
        self.cpu_clock_carry = total % self.cpu_clocks_per_cck;
        if cck == 0 {
            return;
        }
        let tick = self.advance_chipset(cck);
        self.record_slice_bus_advance(cck, tick);
    }

    /// Advance the chipset for a CPU access to memory that is NOT on the chip
    /// bus (ROM, fast RAM): the bus cycle takes 4 CPU clocks per word at the
    /// configured CPU clock, during which the chip bus is entirely free for
    /// DMA. At the stock 2-clocks-per-cck ratio that is the real 68000 figure
    /// of 2 cck per word; an accelerated CPU completes it proportionally
    /// faster, with sub-cck remainders carried so no time is lost.
    pub fn cpu_external_access(&mut self, words: u32) {
        if !self.cpu_bus_arbitration_enabled || words == 0 {
            return;
        }
        // dbg_ext_cck_x100 expresses the stock-ratio cost in hundredths of a
        // cck per word (default 200 = 4 CPU clocks/word); 2x converts it to
        // hundredths of a CPU clock.
        let clocks_x100 =
            words * 2 * self.dbg_ext_cck_x100 + std::mem::take(&mut self.ext_clock_carry_x100);
        let denom = self.cpu_clocks_per_cck * 100;
        let cck = clocks_x100 / denom;
        self.ext_clock_carry_x100 = clocks_x100 % denom;
        if cck == 0 {
            return;
        }
        let tick = self.advance_chipset(cck);
        self.record_slice_bus_advance(cck, tick);
        self.credit_cpu_off_chip_access(cck);
    }

    /// Advance the chipset for a CPU access to a motherboard peripheral (CIA,
    /// RTC, autoconfig, undecoded space). These stay on the slow motherboard
    /// bus no matter how fast the CPU is clocked, so bill the stock 68000
    /// figure of 2 cck per word regardless of the configured CPU speed.
    /// A CPU access to a CIA ($BFxxxx). These are 6800-style VPA cycles: the
    /// 68000 synchronizes to the E clock (CPU/10, six clocks low, four high)
    /// before the data transfer, so the access costs 6..15 CPU cycles
    /// depending on the E phase it starts in, not a plain bus cycle. The
    /// delay table is vAmiga's Agnus::syncWithEClock at Copperline's colour
    /// clock resolution (one cck = two CPU clocks; the CIA PHI2 divider
    /// remainder is the live E phase). Software loops that poll or toggle a
    /// CIA lock to the E clock through this - the vAmigaTS CIA/cnt ramps
    /// count it directly.
    pub fn cpu_cia_access(&mut self, words: u32) {
        if !self.cpu_bus_arbitration_enabled || words == 0 {
            return;
        }
        self.flush_timed_devices();
        const E_SYNC_DELAY_CCK: [u32; 5] = [6, 5, 4, 3, 7];
        let phase = (self.device_clock.cia_tick_remainder_cck as usize).min(4);
        self.cia_trace_phase = phase as u8;
        let delay = E_SYNC_DELAY_CCK[phase];
        let cck = delay + words * 2;
        let tick = self.advance_chipset(cck);
        self.record_slice_bus_advance(cck, tick);
        self.credit_cpu_off_chip_access(cck);
        self.flush_timed_devices();
        self.cia_trace_slot = Some((self.agnus.vpos, self.agnus.hpos));
    }

    pub fn cpu_slow_external_access(&mut self, words: u32) {
        if !self.cpu_bus_arbitration_enabled || words == 0 {
            return;
        }
        let cck = words * 2;
        let tick = self.advance_chipset(cck);
        self.record_slice_bus_advance(cck, tick);
        self.credit_cpu_off_chip_access(cck);
        // This access targets a motherboard peripheral (CIA, RTC, Akiko, Gayle,
        // A2091, autoconfig). The caller reads/writes the device immediately
        // after, so apply the deferred device clocks now -- including this
        // access -- so the device reflects time right up to the observation.
        self.flush_timed_devices();
    }

    /// The bus advance accumulated so far in the current CPU slice (color
    /// clocks). Used to measure how much of an instruction's time was spent on
    /// the bus, so the remaining CPU-internal cycles can be advanced separately.
    pub fn slice_bus_advanced_cck(&self) -> u32 {
        self.slice_bus_advanced_cck
    }

    /// Advance the chipset clock by `cck` color clocks of CPU-INTERNAL execution
    /// time -- cycles where the 68000 is computing, not driving the bus. On real
    /// hardware the beam and all DMA channels keep running during these cycles;
    /// modelling them keeps the chipset clock locked to the CPU's full
    /// instruction time (cpu_cck) rather than only its bus accesses (bus_cck), so
    /// DMA phase and frame timing track wall-clock like silicon. The CPU is not
    /// granted any slot here (owner is whatever DMA the chipset schedules).
    pub fn advance_cpu_internal_cycles(&mut self, cck: u32) {
        if !self.cpu_bus_arbitration_enabled || cck == 0 {
            return;
        }
        let tick = self.advance_chipset(cck);
        self.record_slice_bus_advance(cck, tick);
    }

    /// Charge an instruction's execution clocks against the shared 020+ chip
    /// clock phase, advancing the chipset by the whole colour clocks that
    /// completes and leaving the remainder in `cpu_chip_clock_phase`.
    ///
    /// The CPU and the chip bus keep one timeline, so nothing here is
    /// reconciled against the bus time an access already advanced: the access
    /// credits its own clocks back (`take_cpu_bus_overlap_clocks`) and what
    /// arrives here is the execution time that did not overlap it.
    pub fn charge_cpu_clocks_to_cck(&mut self, clocks: u32) -> u32 {
        if !self.cpu_bus_arbitration_enabled {
            return 0;
        }
        let total = clocks + self.cpu_chip_clock_phase;
        let cck = total / self.cpu_clocks_per_cck;
        self.cpu_chip_clock_phase = total % self.cpu_clocks_per_cck;
        self.advance_cpu_internal_cycles(cck);
        cck
    }

    /// Bill CPU clocks that elapsed on the chip bus into the same phase.
    /// Unlike an instruction charge these have already happened, so whole
    /// colour clocks advance the beam here rather than at the next boundary.
    fn bill_cpu_bus_clocks(&mut self, clocks: u32) {
        let total = clocks + self.cpu_chip_clock_phase;
        self.cpu_chip_clock_phase = total % self.cpu_clocks_per_cck;
        for _ in 0..(total / self.cpu_clocks_per_cck) {
            let (cck, tick) = self.advance_one_chip_bus_quantum(None);
            self.record_slice_bus_advance(cck, tick);
        }
    }

    /// Select the per-instruction charging path for the current CPU slice.
    /// See `cpu_access_phase_sync`.
    pub fn set_cpu_access_phase_sync(&mut self, enabled: bool) {
        self.cpu_access_phase_sync = enabled;
    }

    /// Whether the current slice charges per instruction, so the chip clock
    /// phase is fresh at every access.
    pub fn cpu_access_phase_sync(&self) -> bool {
        self.cpu_access_phase_sync
    }

    /// The CPU's position inside the current colour clock, in CPU clocks.
    pub fn cpu_chip_clock_phase(&self) -> u32 {
        self.cpu_chip_clock_phase
    }

    pub fn set_realtime_devices_enabled(&mut self, enabled: bool) {
        self.device_clock.set_realtime_enabled(enabled);
    }

    pub fn sync_realtime_devices(&mut self) -> u32 {
        if !self.device_clock.realtime_enabled {
            return 0;
        }
        if let Some(wait) = self.device_clock.wait_duration(Instant::now()) {
            std::thread::sleep(wait);
        }
        self.sync_realtime_devices_to(Instant::now())
    }

    pub(crate) fn sync_realtime_devices_to(&mut self, now: Instant) -> u32 {
        let cck = self.device_clock.realtime_cck_due(now);
        if cck != 0 {
            self.advance_devices(cck);
        }
        cck
    }

    pub fn take_slice_bus_advance(&mut self) -> (u32, AgnusTick) {
        let cck = std::mem::take(&mut self.slice_bus_advanced_cck);
        let tick = std::mem::take(&mut self.slice_bus_tick);
        (cck, tick)
    }

    pub fn grant_cpu_bus_access(&mut self, size: usize, kind: CpuBusAccessKind) {
        self.grant_cpu_bus_access_at(None, size, kind);
    }

    /// CPU access to the chip bus with the AGA 32-bit data path modelled:
    /// on Alice machines one bus slot moves a longword, and sequential
    /// opcode-word fetches from the same aligned longword ride a single
    /// access (the 020 fetches 32 bits at a time). Chip writes drop the
    /// fetch latch so self-modifying code refetches.
    pub fn grant_cpu_bus_access_at(
        &mut self,
        addr: Option<u32>,
        size: usize,
        kind: CpuBusAccessKind,
    ) {
        if !self.cpu_bus_arbitration_enabled {
            return;
        }

        if matches!(kind, CpuBusAccessKind::Custom) {
            self.sync_realtime_devices();
        }

        let wide_bus = self.aga_enabled();
        if wide_bus && matches!(kind, CpuBusAccessKind::Write) {
            if let Some(addr) = addr {
                let first = addr & !3;
                let last = addr.wrapping_add(size.max(1) as u32 - 1) & !3;
                for entry in &mut self.cpu_fetch_latch {
                    if *entry == Some(first) || *entry == Some(last) {
                        *entry = None;
                    }
                }
            } else {
                self.cpu_fetch_latch = [None; 2];
            }
        }
        let slots = if wide_bus {
            if let (CpuBusAccessKind::Fetch, Some(addr), 2) = (kind, addr, size) {
                let longword = addr & !3;
                if self.cpu_fetch_latch.contains(&Some(longword)) {
                    return;
                }
                self.cpu_fetch_latch[1] = self.cpu_fetch_latch[0];
                self.cpu_fetch_latch[0] = Some(longword);
                1
            } else {
                (size.max(1) as u32).div_ceil(4)
            }
        } else {
            bus_slots_for_cpu_access(size)
        };
        // The chip bus' CPU port is single-ported: a new access finds it free
        // only after any posted write has retired, so reads cannot pass a
        // pending write and a second write stalls on the one in-flight cycle.
        self.settle_cpu_posted_writes();
        self.cpu_trace_slots.clear();
        if self.cpu_short_bus_cycle && matches!(kind, CpuBusAccessKind::Write) {
            // Post the write instead of stalling for its slot: the 020+ bus
            // unit accepts it at the end of its 3-clock cycle and the
            // transfer overlaps the following execution, so those clocks are
            // credited back against the instruction's CPU charge instead of
            // advancing the beam here. The write retires into a later free
            // chip slot (the drain in `advance_one_chip_bus_quantum`) at the
            // port's 2-cck cadence; see `cpu_posted_write_debt`.
            for slot in 0..slots {
                if slot > 0 {
                    self.settle_cpu_posted_writes();
                }
                self.cpu_posted_write_debt = 1;
                if self.frame_analyzer_full {
                    self.cpu_trace_access = addr
                        .map(|addr| cpu_trace_access_for_slot(addr, size, kind, wide_bus, slot));
                }
                self.cpu_bus_overlap_clocks = self.cpu_bus_overlap_clocks.saturating_add(3);
            }
            return;
        }
        // Every access the CPU waits for credits its clocks back against the
        // instruction charge, because the m68k timing table already allots the
        // instruction the time for its own access.
        let credit = self.cpu_short_bus_cycle && self.cpu_access_phase_sync;
        // Only a chip-RAM read is known to synchronise the CPU to the chip
        // clock; see `sync_cpu_to_chip_clock`. Fetches and custom-register
        // accesses are left unsynchronised until a probe measures them.
        let phase_sync = credit && matches!(kind, CpuBusAccessKind::Read);
        if phase_sync {
            self.sync_cpu_to_chip_clock();
        }
        let bus_clocks_start = self.cpu_bus_clock_position();
        let trace_cpu_bus = diag_cpu_bus_on() && diag_cpu_bus_addr_matches(addr, size);
        for slot in 0..slots {
            self.flush_audio_before_audio_dma_slot();
            let request_slot = (self.agnus.vpos, self.agnus.hpos);
            if matches!(kind, CpuBusAccessKind::Custom) {
                self.cpu_custom_request_slot = Some(request_slot);
            }
            let mut wait_cck = 0;
            while !self.cpu_can_use_current_slot()
                || (self.cpu_short_bus_cycle && self.emulated_cck < self.cpu_chip_port_free_at)
            {
                if self.frame_analyzer_enabled {
                    self.cpu_bus_wait = Some(self.cpu_bus_wait_sample(kind, true));
                }
                let (cck, tick) = self.advance_one_chip_bus_quantum(None);
                wait_cck += cck;
                self.note_cpu_missed_chip_bus_cycle(cck);
                self.record_slice_bus_advance(cck, tick);
                self.flush_audio_before_audio_dma_slot();
            }
            self.cpu_bus_wait = None;
            let grant_slot = (self.agnus.vpos, self.agnus.hpos);
            if matches!(kind, CpuBusAccessKind::Custom) {
                // Remember the slot that carries a custom-register access:
                // a register write's Denise/Agnus-effective position is
                // referenced from this slot (see `record_render_write`),
                // not from the beam position after the bus cycle's tail.
                // A long-word access stores its second (low-word) slot.
                self.cpu_custom_access_slot = Some(grant_slot);
            }
            if self.wave_on {
                self.wave_note_cpu_access(addr, kind, wait_cck);
            }
            if self.frame_analyzer_full {
                self.cpu_trace_access =
                    addr.map(|addr| cpu_trace_access_for_slot(addr, size, kind, wide_bus, slot));
            }
            let slot_start_cck = self.emulated_cck;
            let (cck, tick) = self.advance_one_chip_bus_quantum(Some(ChipBusOwner::Cpu));
            self.cpu_trace_access = None;
            self.note_cpu_granted_chip_bus_cycle();
            self.record_slice_bus_advance(cck, tick);
            if self.cpu_short_bus_cycle {
                self.cpu_chip_port_free_at = slot_start_cck + 2;
            }
            // After the granted slot (one cck), the CPU's bus cycle runs out
            // its remaining clocks with the chip bus free for DMA. The 68000's
            // 4-clock cycle leaves one whole cck (2 clocks at the stock ratio).
            // The 020+ has no separate tail: its shorter cycle ends on the
            // data-return colour clock billed below, and where the CPU resumes
            // inside that clock is carried by `cpu_chip_clock_phase`.
            if !self.cpu_short_bus_cycle {
                let (cck, tick) = self.advance_one_chip_bus_quantum(None);
                self.record_slice_bus_advance(cck, tick);
            }
            if trace_cpu_bus {
                self.diag_cpu_bus_access(
                    kind,
                    addr,
                    size,
                    slot + 1,
                    slots,
                    request_slot,
                    grant_slot,
                    wait_cck,
                );
            }
        }
        if self.cpu_short_bus_cycle
            && (matches!(kind, CpuBusAccessKind::Read)
                || wide_bus && matches!(kind, CpuBusAccessKind::Fetch))
        {
            self.bill_020_read_data_wait();
            if matches!(kind, CpuBusAccessKind::Read) {
                // Without the shared phase (the JIT slice) the read return
                // keeps its older single-clock accumulation.
                let clocks = if phase_sync {
                    CPU_020_CHIP_READ_RETURN_CLOCKS
                } else {
                    1
                };
                self.bill_cpu_bus_clocks(clocks);
            }
        }
        if credit {
            self.credit_cpu_bus_clocks_since(bus_clocks_start);
        }
    }

    /// The CPU's absolute position on the shared timeline, in CPU clocks.
    fn cpu_bus_clock_position(&self) -> u64 {
        self.emulated_cck * u64::from(self.cpu_clocks_per_cck)
            + u64::from(self.cpu_chip_clock_phase)
    }

    /// Credit the clocks an access spent on the chip bus back against the
    /// instruction's own charge. The m68k timing table already allots a
    /// memory-source instruction the time for its access, so without this the
    /// access would be billed twice; what survives the credit is the wait the
    /// table did not know about.
    /// Credit an access that ran off the chip bus (fast RAM, ROM, CIA, slow
    /// motherboard space) back against the instruction charge, on the same
    /// terms as a chip access: the timing table already allots the memory
    /// reference, so only the wait beyond it should stretch the instruction.
    /// Overlap credit for a write the bus decodes to nothing: the 020+
    /// posts the cycle and keeps executing, so part of the core's nominal
    /// write-cycle charge overlaps the following instructions. Called
    /// once per byte lane of the sized access (the unmapped fall-through
    /// recurses per byte), crediting one clock each -- two per aligned
    /// word -- which lands a word-write+DBF loop over the CD32's empty
    /// $A80000 window on the measured 8.0 CPU clocks per iteration
    /// (tools/cd32-probe row UWR; the uncredited charge runs it at 10,
    /// and two clocks per byte overshoot to 6 through the phase
    /// reconciliation). The 68000 has no posted writes; its nominal
    /// charge stands.
    pub(crate) fn credit_cpu_posted_void_write(&mut self) {
        if !self.cpu_short_bus_cycle || !self.cpu_access_phase_sync {
            return;
        }
        self.cpu_bus_overlap_clocks = self.cpu_bus_overlap_clocks.saturating_add(1);
    }

    fn credit_cpu_off_chip_access(&mut self, cck: u32) {
        if !self.cpu_short_bus_cycle || !self.cpu_access_phase_sync {
            return;
        }
        let clocks = cck.saturating_mul(self.cpu_clocks_per_cck);
        self.cpu_bus_overlap_clocks = self.cpu_bus_overlap_clocks.saturating_add(clocks);
    }

    fn credit_cpu_bus_clocks_since(&mut self, start: u64) {
        let spent = self.cpu_bus_clock_position().saturating_sub(start);
        self.cpu_bus_overlap_clocks = self
            .cpu_bus_overlap_clocks
            .saturating_add(spent.min(u64::from(u32::MAX)) as u32);
    }

    /// CPU chip-bus access trace (`COPPERLINE_DIAG_CPU_BUS=1`): logs each
    /// requested CPU chip-bus slot, the slot granted by Agnus arbitration, and
    /// the beam position after the bus cycle tail has elapsed.
    fn diag_cpu_bus_access(
        &self,
        kind: CpuBusAccessKind,
        addr: Option<u32>,
        size: usize,
        slot: u32,
        slots: u32,
        request_slot: (u32, u32),
        grant_slot: (u32, u32),
        wait_cck: u32,
    ) {
        let (rv, rh) = request_slot;
        let (v, h) = grant_slot;
        match addr {
            Some(addr) => eprintln!(
                "CPUBUS kind={} addr={:08x} size={} slot={}/{} rv={:03x} rh={:02x} v={:03x} h={:02x} wait={} ev={:03x} eh={:02x}",
                cpu_bus_access_kind_name(kind),
                addr,
                size,
                slot,
                slots,
                rv,
                rh,
                v,
                h,
                wait_cck,
                self.agnus.vpos,
                self.agnus.hpos
            ),
            None => eprintln!(
                "CPUBUS kind={} addr=-------- size={} slot={}/{} rv={:03x} rh={:02x} v={:03x} h={:02x} wait={} ev={:03x} eh={:02x}",
                cpu_bus_access_kind_name(kind),
                size,
                slot,
                slots,
                rv,
                rh,
                v,
                h,
                wait_cck,
                self.agnus.vpos,
                self.agnus.hpos
            ),
        }
    }

    fn bill_020_read_data_wait(&mut self) {
        let (cck, tick) = self.advance_one_chip_bus_quantum(None);
        self.record_slice_bus_advance(cck, tick);
    }

    /// Synchronise the CPU to the chip clock before a chip access it has to
    /// wait for. The 020's chip bus cycle cannot begin part way through a
    /// colour clock, so a CPU sitting mid-clock stalls out the remainder.
    ///
    /// The phase is a debt of clocks already charged but not yet turned into
    /// beam time, so advancing one whole colour clock bills those pending
    /// clocks plus the stall, exactly and with nothing double counted. This is
    /// what makes a chip-access loop run a whole number of colour clocks per
    /// iteration on real hardware, and what makes the period independent of
    /// the phase the loop happened to be entered with. A posted write does not
    /// synchronise (the bus unit takes it behind the execution unit), which is
    /// why a real A1200 write loop measures the same at both loop-branch
    /// alignments while a read loop carries the branch clock into a whole
    /// extra colour clock (`timing-test/rdprobe.asm` rows 2/3 against 0/1).
    fn sync_cpu_to_chip_clock(&mut self) {
        if self.cpu_chip_clock_phase == 0 {
            return;
        }
        self.cpu_chip_clock_phase = 0;
        // Not a missed CPU slot: the CPU is synchronising to the chip clock,
        // not being denied the bus, so this must not feed the blitter
        // starvation counter.
        let (cck, tick) = self.advance_one_chip_bus_quantum(None);
        self.record_slice_bus_advance(cck, tick);
    }

    /// Advance the chipset until a posted CPU chip write has retired into a
    /// free chip slot (the drain in `advance_one_chip_bus_quantum`). Quanta
    /// that could not retire it count as missed CPU slots, like the grant
    /// wait loop's.
    fn settle_cpu_posted_writes(&mut self) {
        while self.cpu_posted_write_debt > 0 {
            let debt_before = self.cpu_posted_write_debt;
            if self.frame_analyzer_enabled {
                // Attributed as the write it is retiring, from the ordinary
                // arbitration the drain is subject to (no BLS yield, no
                // fence). A quantum that does drain it records the CPU as
                // the slot owner instead, so the sample only lands on
                // genuinely missed clocks.
                self.cpu_bus_wait = Some(self.cpu_bus_wait_sample(CpuBusAccessKind::Write, false));
            }
            let (cck, tick) = self.advance_one_chip_bus_quantum(None);
            if self.cpu_posted_write_debt == debt_before {
                self.note_cpu_missed_chip_bus_cycle(cck);
            }
            self.record_slice_bus_advance(cck, tick);
        }
        self.cpu_bus_wait = None;
        self.cpu_trace_access = None;
    }

    /// The analyzer's attribution of the colour clock the CPU is about to
    /// miss: who holds the slot from the waiting access's point of view
    /// (`for_cpu`: the CPU's own view for a synchronous grant, the ordinary
    /// arbitration for a posted-write drain; see `cpu_bus_denial_class`),
    /// what access is pending, and the instruction that made it.
    /// Side-effect free; only evaluated while the frame analyzer is armed.
    fn cpu_bus_wait_sample(&self, kind: CpuBusAccessKind, for_cpu: bool) -> CpuWaitSample {
        CpuWaitSample {
            class: self.cpu_bus_denial_class(for_cpu),
            kind,
            pc: self.cpu_pc,
        }
    }

    /// CPU clocks of posted-write transfer time that overlapped execution on
    /// the decoupled 020+ bus unit during the current instruction. The
    /// CPU-side charge subtracts them from the instruction's clock total so
    /// the beam does not advance for time the bus unit hid.
    pub fn take_cpu_bus_overlap_clocks(&mut self) -> u32 {
        std::mem::take(&mut self.cpu_bus_overlap_clocks)
    }

    /// A 020+ CPU read of a custom register costs one colour clock more than a
    /// chip-RAM read of the same width. Chip RAM answers out of Agnus' DRAM
    /// controller, which has the row already open for the granted slot; a
    /// register read has to cross to the addressed chip (Agnus, Denise or
    /// Paula), be driven back onto the 16-bit chipset bus, and only then meet
    /// the CPU's data-return phase. A 68000 cannot see the difference - its
    /// four-clock bus cycle is longer than either path - but a 14 MHz 020
    /// samples early enough that the extra chip-crossing shows up as a whole
    /// slot. Beam-polling loops (VHPOSR/INTREQR) are what this decides:
    /// `timing-test` rows 16, 17 and 21 fit 27% more iterations per frame
    /// without it (A1200 reference in `timing-test/README.md`).
    fn bill_custom_register_return(&mut self) {
        let (cck, tick) = self.advance_one_chip_bus_quantum(None);
        self.record_slice_bus_advance(cck, tick);
    }

    fn note_cpu_missed_chip_bus_cycle(&mut self, cck: u32) {
        self.cpu_missed_chip_slots = self.cpu_missed_chip_slots.wrapping_add(1);
        self.cpu_wait_cck_total = self.cpu_wait_cck_total.wrapping_add(u64::from(cck));
        if self.blitter_slowdown_counter_enabled() && self.blitter.current_slot_counts_for_bls() {
            self.blitter_slowdown_cpu_misses = self
                .blitter_slowdown_cpu_misses
                .saturating_add(1)
                .min(exp_miss_limit());
        } else {
            self.blitter_slowdown_cpu_misses = 0;
        }
    }

    /// Monotonic colour clocks for which CPU accesses were denied the chip bus.
    pub fn cpu_wait_cck_total(&self) -> u64 {
        self.cpu_wait_cck_total
    }

    fn note_cpu_granted_chip_bus_cycle(&mut self) {
        self.blitter_slowdown_cpu_misses = 0;
        // Cumulative count of chip-bus slots the CPU is granted, used by the
        // real-time pacing profile.
        self.cpu_granted_chip_slots = self.cpu_granted_chip_slots.wrapping_add(1);
    }

    /// Cumulative chip-bus slots granted to the CPU.
    pub fn cpu_granted_chip_slots(&self) -> u64 {
        self.cpu_granted_chip_slots
    }

    fn finish_pending_blitter(&mut self) {
        let was_busy = self.blitter.busy;
        if self.blitter.finish_scheduled_now(&mut self.mem.chip_ram) {
            self.latch_blitter_completion("forced");
            self.trace_blitter_forced_finish(was_busy);
        }
    }

    fn latch_blitter_completion(&mut self, source: &'static str) {
        self.note_bus_event_named(BUS_EVENT_BLIT_START_FINISH, Some("blitter_finish"));
        if self.frame_analyzer_enabled {
            let end = (
                self.agnus.vpos.min(u32::from(u16::MAX)) as u16,
                self.agnus.hpos.min(u32::from(u16::MAX)) as u16,
            );
            let completed = self
                .current_frame_bus_trace
                .blits
                .iter_mut()
                .rev()
                .find(|record| Some(record.id) == self.active_frame_blit)
                .map(|record| {
                    record.end = Some(end);
                    record.end_frame = Some(self.emulated_frames);
                    record.clone()
                });
            // A blit crossing a frame boundary is visible from both the
            // just-finished and current frame. Finalise the older reference
            // too, with the same totals and transfer stream.
            if let (Some(last), Some(completed)) =
                (self.last_frame_bus_trace.as_mut(), completed.as_ref())
            {
                if let Some(record) = last
                    .blits
                    .iter_mut()
                    .rev()
                    .find(|record| record.id == completed.id)
                {
                    *record = completed.clone();
                }
            }
        }
        self.active_frame_blit = None;
        if diag_blt_slots() {
            eprintln!(
                "BLTP {} {} {} END source={source}",
                self.emulated_frames, self.agnus.vpos, self.agnus.hpos
            );
        }
        if source == "forced" {
            // Drained synchronously (mid-blit register write): the whole
            // hardware timeline has already been collapsed, so raise now
            // and drop any raise already armed by the terminal cycle.
            self.blit_irq_delay_cck = None;
            self.raise_blit_irq(source);
        }
        // Scheduled completions do not raise here: INTREQ.BLIT is armed
        // when the sequencer ENTERS its terminal BLTDONE cycle (see
        // note_blitter_slot_ticked), one colour clock after that cycle's
        // first attempt -- which real Agnus asserts even while a contended
        // final D write is still blocked (vAmiga scheduleIrqRel(BLIT, 1)
        // runs before the bus allocation check).
    }

    /// Post-tick bookkeeping shared by the blitter's bus-slot and
    /// idle-cycle tick sites: arm the INTREQ.BLIT raise when the sequencer
    /// just entered its terminal BLTDONE cycle. Armed with 2 because the
    /// arming tick's own advance_beam decrements it once in the same
    /// colour clock; the raise then lands at the end of the terminal
    /// cycle's first attempt.
    pub(crate) fn note_blitter_slot_ticked(&mut self) {
        if self.blitter.take_irq_arm() {
            self.blit_irq_delay_cck = Some(2);
        }
    }

    fn raise_blit_irq(&mut self, source: &'static str) {
        self.note_bus_event_named(BUS_EVENT_BLIT_IRQ, Some("blitter_irq"));
        let intreq_before = self.paula.intreq;
        self.paula.intreq |= INT_BLIT;
        self.note_irq_source_asserted();
        self.trace_blitter_completion(source, intreq_before);
    }

    #[cfg(test)]
    pub fn last_chip_bus_owner(&self) -> ChipBusOwner {
        self.last_chip_bus_owner
    }

    pub fn advance_chipset(&mut self, target_cck: u32) -> AgnusTick {
        let mut total = AgnusTick::default();

        let mut remaining = target_cck;
        while remaining > 0 {
            let (cck, tick) = self.advance_one_chip_bus_quantum_limited(None, remaining);
            remaining = remaining.saturating_sub(cck);
            add_agnus_tick(&mut total, tick);
        }

        total
    }

    /// Long stopped-CPU advance that keeps a steady Copper WAIT dormant up
    /// to its exact comparator deadline. Ordinary CPU-driven advances are
    /// deliberately left on [`Self::advance_chipset`]: their spans are only a
    /// few colour clocks, so calculating a far-future deadline would cost
    /// more than the comparator calls it replaces.
    fn advance_chipset_cpu_idle(&mut self, target_cck: u32) -> AgnusTick {
        let mut total = AgnusTick::default();

        let mut remaining = target_cck;
        let mut invariant_copper_deadline = self
            .invariant_copper_deadline_cck()
            .filter(|&deadline| deadline != 0);
        while remaining > 0 {
            let before_deadline = invariant_copper_deadline.is_some();
            let quantum_limit = invariant_copper_deadline
                .map(|deadline| remaining.min(deadline))
                .unwrap_or(remaining);
            let (cck, tick) = self.advance_one_chip_bus_quantum_limited_inner(
                None,
                quantum_limit,
                before_deadline,
            );
            remaining = remaining.saturating_sub(cck);
            add_agnus_tick(&mut total, tick);
            invariant_copper_deadline = invariant_copper_deadline
                .and_then(|deadline| deadline.checked_sub(cck))
                .filter(|&deadline| deadline != 0);
            if invariant_copper_deadline.is_none() {
                invariant_copper_deadline = self
                    .invariant_copper_deadline_cck()
                    .filter(|&deadline| deadline != 0);
            }
        }

        total
    }

    /// Advance devices while the CPU is halted in STOP. This is the only
    /// caller that presents spans long enough for the sleeping-Copper
    /// deadline optimization to pay for itself.
    pub fn advance_cpu_idle_devices(&mut self, cck: u32) -> AgnusTick {
        self.flush_timed_devices();
        let tick = self.advance_chipset_cpu_idle(cck);
        let spans = std::mem::take(&mut self.pending_device_trace_spans);
        self.tick_timed_devices(cck, tick, &spans);
        self.pending_device_trace_spans = spans;
        self.pending_device_trace_spans.clear();
        tick
    }

    pub fn advance_devices(&mut self, cck: u32) -> AgnusTick {
        // Apply any color clocks deferred by the CPU access path before ticking
        // this (idle/stopped-CPU or test-driven) span, so device time stays
        // ordered, then tick this span directly.
        self.flush_timed_devices();
        let tick = self.advance_chipset(cck);
        let spans = std::mem::take(&mut self.pending_device_trace_spans);
        self.tick_timed_devices(cck, tick, &spans);
        self.pending_device_trace_spans = spans;
        self.pending_device_trace_spans.clear();
        tick
    }

    /// Consume the "a device tick DMA'd into guest memory" flag (see the
    /// field's own comment). The CPU run loop checks this after every
    /// timed-device flush and drops its data-cache model when it returns
    /// true, keeping asynchronous device DMA coherent with the modelled
    /// 030/040/060 data cache.
    pub fn take_devices_wrote_memory(&mut self) -> bool {
        std::mem::take(&mut self.devices_wrote_memory)
    }

    pub fn next_blitter_completion_cck(&self) -> Option<u32> {
        if !self.blitter_dma_enabled() {
            return None;
        }
        let slots = self.blitter.scheduled_slots_remaining()?;
        let prediction = self
            .blitter
            .scheduled_slot_access_pattern(BLITTER_DEADLINE_SLOT_SCAN_LIMIT)
            .filter(|&(_, count)| count == slots)
            .and_then(|(mask, count)| self.cck_until_blitter_completes(mask, count));
        Some(prediction.unwrap_or_else(|| slots.saturating_mul(CHIP_BUS_SLOT_CCK).max(1)))
    }

    pub fn next_serial_event_cck(&self) -> Option<u32> {
        self.paula.next_serial_event_cck()
    }

    pub fn next_pot_event_cck(&self) -> Option<u32> {
        // The pot counters step at H-sync, so while a scan is running cap the
        // advance at the next line boundary (the same deadline CIA-B TOD uses)
        // so a CPU read of POTxDAT lands on the up-to-date value. When no scan
        // is running the pots impose no deadline.
        if !self.paula.pot_running() {
            return None;
        }
        self.agnus.cck_until_line_ticks(1)
    }

    pub fn next_audio_irq_cck(&self) -> Option<u32> {
        let cck = self.paula.next_audio_irq_cck(self.agnus.dmacon)?;
        Some(cck.saturating_sub(self.audio_pending_cck).max(1))
    }

    pub fn cpu_visible_intreq(&self) -> u16 {
        let mut visible = self.paula.intreq;
        if self.coper_cpu_irq_delay_cck != 0 {
            visible &= !INT_COPER;
        }
        // Hold each newly-raised interrupt invisible until its own pipe delay
        // has elapsed; sources released earlier stay visible.
        let mut delayed = self.irq_latency_mask;
        while delayed != 0 {
            let bit = delayed.trailing_zeros() as usize;
            delayed &= delayed - 1;
            if self.emulated_cck < self.irq_latency_visible_at[bit] {
                visible &= !(1u16 << bit);
            }
        }
        visible
    }

    /// Detect a newly-raised maskable interrupt and arm its recognition-latency
    /// countdown. Called per device tick, after intreq/intena have settled.
    fn arm_irq_recognition_latency(&mut self) {
        self.note_intreq_set_edges();
        let setting = self.irq_latency_setting;
        if setting == 0 {
            return;
        }
        let pending = self.current_enabled_irq_sources();
        let newly = pending & !self.irq_latency_last_pending;
        self.arm_irq_latency_bits(newly, setting);
        // Drop any bits that are no longer pending (acked while still delayed).
        self.irq_latency_mask &= pending;
        self.irq_latency_last_pending = pending;
    }

    /// Colour clocks until the earliest interrupt still inside its
    /// recognition pipe reaches the CPU's IPL pins, for bounding an idle
    /// STOP nap. `None` when nothing is in flight.
    pub fn next_irq_visible_cck(&self) -> Option<u32> {
        let mut delayed = self.irq_latency_mask;
        let mut soonest: Option<u64> = None;
        while delayed != 0 {
            let bit = delayed.trailing_zeros() as usize;
            delayed &= delayed - 1;
            let at = self.irq_latency_visible_at[bit];
            if at > self.emulated_cck {
                let remaining = at - self.emulated_cck;
                soonest = Some(soonest.map_or(remaining, |s: u64| s.min(remaining)));
            }
        }
        soonest.map(|cck| cck.min(u64::from(u32::MAX)) as u32)
    }

    /// Start each named source's own recognition pipe. A source already on its
    /// way to the pins keeps the deadline it has: re-arming it here is what
    /// made one source's assertion postpone another's.
    fn arm_irq_latency_bits(&mut self, bits: u16, setting: u32) {
        let mut remaining = bits & !self.irq_latency_mask;
        if remaining == 0 {
            return;
        }
        let deadline = self.emulated_cck.saturating_add(u64::from(setting));
        self.irq_latency_mask |= remaining;
        while remaining != 0 {
            let bit = remaining.trailing_zeros() as usize;
            remaining &= remaining - 1;
            self.irq_latency_visible_at[bit] = deadline;
        }
    }

    fn current_enabled_irq_sources(&self) -> u16 {
        if self.paula.intena & crate::chipset::paula::INT_MASTER != 0 {
            self.paula.intena & self.paula.intreq & IRQ_SOURCE_BITS
        } else {
            0
        }
    }

    /// CPU INTENA/INTREQ writes change Paula's mask/latch state, but they are
    /// usually not new asynchronous interrupt-source edges. Keep the delayed-bit
    /// state coherent without hiding an already-latched source again; real
    /// recognition latency is armed where Paula/CIA/blitter sources assert.
    ///
    /// PORTS is level-fed by CIA-A/Gayle-style INT2 sources and is left visible
    /// immediately when software unmasks an already-latched level. Other newly
    /// exposed sources still represent a freshly-present CPU IPL input and pass
    /// through interrupt recognition.
    fn note_irq_latches_changed(&mut self) {
        self.note_intreq_set_edges();
        let pending = self.current_enabled_irq_sources();
        let newly = pending & !self.irq_latency_last_pending;
        let delayed = newly & !INT_PORTS;
        if delayed != 0 && self.irq_latency_setting != 0 {
            self.arm_irq_latency_bits(delayed, self.irq_latency_setting);
        }
        self.irq_latency_mask &= pending;
        self.irq_latency_last_pending = pending;
    }

    fn note_irq_source_asserted(&mut self) {
        self.arm_irq_recognition_latency();
    }

    fn note_intreq_set_edges(&mut self) {
        let current = self.paula.intreq & IRQ_SOURCE_BITS;
        if current & !self.trace_intreq != 0 {
            self.note_bus_event_named(BUS_EVENT_INTREQ, Some("intreq_set"));
        }
        self.trace_intreq = current;
    }

    pub fn note_cpu_irq_recognized(&mut self, level: u8) {
        self.note_bus_event_named_at(
            BUS_EVENT_CPU_IRQ,
            Some("cpu_irq_recognized"),
            None,
            Some(level),
        );
        if self.frame_analyzer_full {
            self.current_frame_bus_trace
                .annotate_at(self.agnus.vpos, self.agnus.hpos, |record| {
                    record.ipl = level
                });
        }
    }

    pub fn note_cpu_stop(&mut self, ipl_wake: bool) {
        self.note_bus_event_named(
            if ipl_wake {
                BUS_EVENT_CPU_STOP_IPL
            } else {
                BUS_EVENT_CPU_STOP
            },
            Some(if ipl_wake {
                "cpu_stop_ipl_wake"
            } else {
                "cpu_stop_entered"
            }),
        );
    }

    pub fn next_frame_event_cck(&self) -> u32 {
        self.agnus.cck_until_next_frame().max(1)
    }

    pub fn next_display_start_event_cck(&self) -> Option<u32> {
        let display_start = self.display_start_vpos_for_current_control();
        if self.current_frame_display_snapshot_taken || self.agnus.vpos >= display_start {
            return None;
        }
        self.agnus.cck_until_line_start(display_start)
    }

    fn display_start_vpos_for_current_control(&self) -> u32 {
        // Programmable scans anchor at the geometry's visible window (from
        // the programmable vertical blank); the PAL/NTSC DIW clamp below
        // only makes sense for the fixed 15 kHz field.
        if self.current_frame_geometry.programmable {
            return self.current_frame_geometry.visible_start_vpos;
        }
        visible_start_vpos_for_diw(
            self.denise.diwstrt,
            self.denise.diwstop,
            self.effective_diwhigh(),
        )
    }

    /// Display geometry for the frame that is just starting, computed once
    /// at the frame wrap. Mid-frame BEAMCON0/HTOTAL/VTOTAL writes affect
    /// the live beam immediately but take presentation effect at the next
    /// wrap, like the interlace long-field flag. Standard frames keep a fixed
    /// overscan render top; DIW controls the display-start snapshot separately.
    fn compute_frame_geometry(&self) -> FrameGeometry {
        let lace = self.denise.bplcon0 & 0x0004 != 0;
        if let Some((start, lines)) = self.agnus.programmable_visible_window() {
            return FrameGeometry {
                programmable: true,
                visible_start_vpos: start,
                visible_lines: (lines as usize).clamp(1, MAX_VISIBLE_LINES),
                line_cck: self.agnus.programmable_line_cck().unwrap_or(227),
                frame_lines: self.agnus.current_frame_lines(),
                lace,
            };
        }
        let frame_lines =
            fixed_standard_frame_lines(self.agnus.video_standard(), lace, self.agnus.lof);
        FrameGeometry::standard(self.current_frame_visible_start_vpos, frame_lines, lace)
    }

    fn frame_lines_for_geometry(&self, geometry: FrameGeometry, long_field: bool) -> u32 {
        if geometry.programmable {
            self.agnus.current_frame_lines()
        } else {
            fixed_standard_frame_lines(self.agnus.video_standard(), geometry.lace, long_field)
        }
    }

    /// Keep a standard frame's geometry in step with the framebuffer render
    /// origin (programmable geometry derives its window from the beam registers
    /// instead and is left alone).
    #[cfg(test)]
    fn refresh_frame_geometry_visible_start(&mut self) {
        if !self.current_frame_geometry.programmable {
            self.current_frame_geometry.visible_start_vpos = self.current_frame_visible_start_vpos;
        }
    }

    pub fn next_cia_b_tod_alarm_cck(&self) -> Option<u32> {
        self.agnus
            .cck_until_line_ticks(self.cia_b.next_tod_alarm_ticks()?)
    }

    pub fn next_copper_wakeup_cck(&self) -> Option<u32> {
        if !self.copper_dma_enabled() {
            return None;
        }
        if let Some(cck) = self.cck_until_pending_copper_frame_start() {
            return Some(cck);
        }
        let Some(wait) = self.copper.waiting() else {
            // A running Copper (or one in a WAIT/SKIP tail) fetches an
            // instruction word every other colour clock, and its next MOVE
            // can raise INTREQ: bound the stopped-CPU fast-forward to a
            // couple of colour clocks so a wake-up interrupt written by the
            // instruction stream is recognized on time. A halted Copper
            // (illegal register write) fetches nothing until restarted.
            if self.copper.is_stopped() {
                return None;
            }
            return Some(2);
        };
        self.copper_wait_wakeup_cck(wait)
    }

    /// Exact deadline before which the Copper state is invariant: either a
    /// pending vertical-blank COP1LC restart or the steady comparator phase of
    /// a WAIT. Before this point `advance_chipset` may leave the per-quantum
    /// Copper step dormant. Instruction-tail and wake-up phases return `None`
    /// and retain their exact cycle steps.
    fn invariant_copper_deadline_cck(&self) -> Option<u32> {
        if !self.copper_dma_enabled() {
            return None;
        }
        if let Some(cck) = self.cck_until_pending_copper_frame_start() {
            return Some(cck);
        }
        let wait_deadline = self.copper_wait_wakeup_cck(self.copper.sleeping_wait()?)?;
        // A field wrap stops the current Copper and schedules the vertical-
        // blank COP1LC strobe. That state transition can precede a wait whose
        // low-byte vertical target lies in the next field, so never carry a
        // sleeping-WAIT deadline across the wrap.
        Some(wait_deadline.min(self.agnus.cck_until_next_frame()))
    }

    fn copper_wait_wakeup_cck(&self, wait: CopperWait) -> Option<u32> {
        let position_cck = self.cck_until_copper_wait_position(wait)?;
        if position_cck == 0 && wait.blitter_wait_enabled() && self.blitter.busy {
            return self.next_blitter_completion_cck();
        }
        Some(position_cck)
    }

    fn tick_timed_devices(
        &mut self,
        cck: u32,
        agnus_tick: AgnusTick,
        trace_spans: &[BusTraceSpan],
    ) {
        if agnus_tick.new_frames > 0 {
            self.pending_vbi = 1;
        }

        // Gayle drives INT2 (PORTS) as a level; Paula's INTREQ latch keeps
        // getting set while the line is asserted.
        if self
            .gayle
            .as_ref()
            .is_some_and(crate::gayle::Gayle::int2_line)
        {
            self.paula.intreq |= INT_PORTS;
        }

        // The A4000's IDE has no interrupt latch of its own: the drive's INTRQ
        // is the INT2 line, and the driver drops it by reading the status.
        if self
            .ide_a4000
            .as_ref()
            .is_some_and(crate::ide_a4000::IdeA4000::int2_line)
        {
            self.paula.intreq |= INT_PORTS;
        }

        // Gayle/A4000 IDE are motherboard devices, not entries in the Zorro
        // `devices` chain below, so an attached ATAPI drive needs its own
        // explicit tick here (disc-swap mounting, CD-DA audio streaming) --
        // nothing else drives it. Both master and slave can be ATAPI at
        // once, so this ticks every drive on the cable, not just the one
        // `first_atapi_mut` would pick as the disc-swap target.
        {
            let Self { gayle, paula, .. } = self;
            if let Some(gayle) = gayle.as_mut() {
                gayle.tick_atapi(cck, paula.cd_audio_mut());
            }
        }
        {
            let Self {
                ide_a4000, paula, ..
            } = self;
            if let Some(ide_a4000) = ide_a4000.as_mut() {
                ide_a4000.tick_atapi(cck, paula.cd_audio_mut());
            }
        }

        // SDMAC (A3000 motherboard SCSI): advance the WD33C93 and its DMA, and
        // level-feed its INT2 line like Gayle's. Kickstart's own scsi.device
        // drives it, so nothing boots until this interrupt arrives.
        {
            let Self {
                sdmac,
                mem,
                paula,
                devices_wrote_memory,
                ..
            } = self;
            if let Some(sdmac) = sdmac.as_mut() {
                sdmac.tick(cck, mem, paula.cd_audio_mut());
                if sdmac.take_wrote_memory() {
                    *devices_wrote_memory = true;
                }
                if sdmac.int_line() {
                    paula.intreq |= INT_PORTS;
                }
            }
        }

        // Akiko: advance the CD controller (sector DMA pacing, command
        // and response rings) and level-feed its INT2 line like Gayle's.
        if let Some(akiko) = self.akiko.as_mut() {
            akiko.tick(cck, &mut self.mem, self.paula.cd_audio_mut());
            if akiko.int2_line() {
                self.paula.intreq |= INT_PORTS;
            }
        }

        // CDTV DMAC: advance through the ZorroDevice boundary (its tick streams
        // CD audio from Paula's ring) and level-feed its INT2 line like Gayle's.
        if let Some(cdtv) = self.cdtv.as_mut() {
            {
                let mut host = crate::zorro_device::DeviceHost::with_cd_audio(
                    &mut self.mem,
                    self.paula.cd_audio_mut(),
                );
                crate::zorro_device::ZorroDevice::tick(cdtv, cck, &mut host);
                if host.wrote_memory() {
                    self.devices_wrote_memory = true;
                }
            }
            if crate::zorro_device::ZorroDevice::int2_line(cdtv) {
                self.paula.intreq |= INT_PORTS;
            }
        }

        // Functional Zorro-chain boards (A2091 SCSI, WASM plugins): advance each
        // -- delivering delayed interrupts and any DMA that became ready -- and
        // level-feed its INT2/INT6 lines like Gayle's.
        {
            let Self {
                devices,
                mem,
                paula,
                devices_wrote_memory,
                ..
            } = self;
            for (slot, dev) in devices.iter_mut().enumerate() {
                let (cd_audio, toccata_audio, mhi_audio) = paula.audio_rings_mut();
                let mut host = crate::zorro_device::DeviceHost::for_slot_with_audio(
                    &mut *mem,
                    slot,
                    cd_audio,
                    toccata_audio,
                    mhi_audio,
                );
                crate::zorro_device::ZorroDevice::tick(dev, cck, &mut host);
                // A tick that DMA'd into guest memory (copperhf draining a
                // completed read, a plugin board writing a buffer) wrote
                // behind the CPU's modelled data cache; flag it so the run
                // loop drops the cache. Keyed on actual DMA WRITES
                // (`wrote_memory`), not `touched_memory`: boards like the
                // A2091 tick `memory_mut` unconditionally, and latching on
                // mere access would clear the data cache every tick --
                // disabling it in practice.
                if host.wrote_memory() {
                    *devices_wrote_memory = true;
                }
                if crate::zorro_device::ZorroDevice::int2_line(dev) {
                    paula.intreq |= INT_PORTS;
                }
                if crate::zorro_device::ZorroDevice::int6_line(dev) {
                    paula.intreq |= INT_EXTER;
                }
            }
        }

        let cia_remainder = self.device_clock.cia_tick_remainder_cck;
        let ticks = self.device_clock.cia_ticks_for_cck(cck);
        let cia_a_edge_tick = (!trace_spans.is_empty() && !self.trace_cia_irq_pins.0)
            .then(|| self.cia_a.first_irq_edge_tick_within(ticks))
            .flatten();
        let cia_b_edge_tick = (!trace_spans.is_empty() && !self.trace_cia_irq_pins.1)
            .then(|| self.cia_b.first_irq_edge_tick_within(ticks))
            .flatten();
        // Cached once: this runs on the per-device-tick path, so a live env
        // lookup here would cost real-time performance for a debug-only logger.
        let dbg_cia = dbg_cia_on();
        if self.cia_a.tick(ticks) {
            self.paula.intreq |= INT_PORTS;
            if dbg_cia {
                log::info!(
                    "cia A irq secs={:.5} f={} icr={:#04X}",
                    self.emulated_seconds(),
                    self.emulated_frames,
                    self.cia_a.debug_icr_data(),
                );
            }
        }
        if self.cia_b.tick(ticks) {
            self.paula.intreq |= INT_EXTER;
            if dbg_cia {
                log::info!(
                    "cia B irq secs={:.5} f={} icr={:#04X}",
                    self.emulated_seconds(),
                    self.emulated_frames,
                    self.cia_b.debug_icr_data(),
                );
            }
        }
        // Predicting on cloned CIAs above leaves this live update batched, but
        // still lets the diagnostic trace stamp each timer-driven pin edge on
        // its exact E-clock slot before TOD and external inputs below can add
        // edges at the post-advance beam position.
        if !trace_spans.is_empty() {
            let position_for_tick = |tick: u32| {
                let first_elapsed_cck = 5u32.saturating_sub(cia_remainder);
                let elapsed_cck =
                    first_elapsed_cck.saturating_add(tick.saturating_sub(1).saturating_mul(5));
                bus_trace_position_at(trace_spans, elapsed_cck.saturating_sub(1))
            };
            self.refresh_cia_irq_lines_at_positions(
                cia_a_edge_tick.and_then(position_for_tick),
                cia_b_edge_tick.and_then(position_for_tick),
            );
        }

        for _ in 0..agnus_tick.new_frames {
            if self.cia_a.tick_tod() {
                self.paula.intreq |= INT_PORTS;
            }
        }
        for _ in 0..agnus_tick.new_lines {
            // Denise clocks the POTxDAT counters at H-sync, the same per-line
            // clock as CIA-B's TOD, so advance the pot scan once per new line.
            let discharge_lines = match self.agnus.video_standard() {
                VideoStandard::Pal => 8,
                VideoStandard::Ntsc => 7,
            };
            self.paula.tick_pot_hsync(self.pot_pins(), discharge_lines);
            if self.cia_b.tick_tod() {
                self.paula.intreq |= INT_EXTER;
                if dbg_cia {
                    log::info!(
                        "cia B TOD alarm (tick) secs={:.5} f={}",
                        self.emulated_seconds(),
                        self.emulated_frames,
                    );
                }
            }
        }
        for _ in 0..agnus_tick.new_frames {
            if self
                .cia_b
                .sync_tod_to_frame(self.agnus.nominal_frame_lines())
            {
                self.paula.intreq |= INT_EXTER;
                if dbg_cia {
                    log::info!(
                        "cia B TOD alarm (frame sync) secs={:.5} f={}",
                        self.emulated_seconds(),
                        self.emulated_frames,
                    );
                }
            }
        }

        if !self.keyboard.is_idle() && self.keyboard.tick(cck, &mut self.cia_a) {
            self.paula.intreq |= INT_PORTS;
        }
        if self.keyboard.take_system_reset_request() {
            self.keyboard_system_reset_pending = true;
            self.slice_preempted = true;
        }

        // emulated_cck already covers this span, so it is the color clock at
        // the span's end. Paula uses it to stamp any serial byte that finishes
        // here; passing it avoids arithmetic on the common (no byte) path.
        // A freshly-raised serial interrupt preempts the slice like any
        // other interrupt source: OR-ing it in silently would leave a
        // running CPU blind to RBF until its instruction slice ran out,
        // which is longer than a character time at BBS baud rates -- the
        // next word would overrun the one-word receive buffer before the
        // guest's handler ever ran.
        let serial_irq = self.paula.tick_serial(cck, self.emulated_cck);
        if serial_irq != 0 && self.paula.latch_interrupt_sources(serial_irq) {
            self.slice_preempted = true;
        }
        let dmacon = self.agnus.dmacon;
        self.flush_audio();
        // The floppy mechanism is quiescent for almost all of normal running
        // (no DMA, motor off, no drive selected). Skip the whole block -- an
        // `is_idle()` recompute plus the DSKBLK/DSKSYNC/index-pulse polling and
        // drive-sound feed -- on a single cached bool while that holds. The
        // cache is cleared by every activation write and recomputed in
        // `floppy.tick`, so a newly active drive is serviced from the next
        // access. Drive sounds need no feed once idle: the spin levels are
        // already zeroed and the tails decay in Paula's mixer.
        if !self.floppy.is_idle_cached() {
            self.floppy.set_adkcon(self.paula.adkcon);
            let disk_irq = self.floppy.tick(cck, dmacon, &mut self.mem.chip_ram);
            for transfer in self.floppy.take_dma_trace() {
                let trace_position = if transfer.instantaneous {
                    trace_spans.last().and_then(|span| {
                        span.position_at(span.offset.saturating_add(span.cck).saturating_sub(1))
                    })
                } else {
                    bus_trace_position_at(trace_spans, transfer.cck_offset).or_else(|| {
                        trace_spans.last().and_then(|span| {
                            span.position_at(span.offset.saturating_add(span.cck).saturating_sub(1))
                        })
                    })
                };
                let reg = if transfer.memory_write {
                    0x0008
                } else {
                    0x0026
                };
                let flags = u16::from(transfer.memory_write);
                if transfer.instantaneous {
                    self.record_instantaneous_bus_slot_at(
                        trace_position,
                        BUS_RECORD_DISK,
                        0,
                        reg,
                        transfer.addr,
                        u64::from(transfer.data),
                        2,
                        flags,
                    );
                } else {
                    self.annotate_bus_slot_at(
                        trace_position,
                        BUS_RECORD_DISK,
                        0,
                        reg,
                        transfer.addr,
                        u64::from(transfer.data),
                        2,
                        flags,
                    );
                }
            }
            if disk_irq {
                self.paula.intreq |= INT_DSKBLK;
                // Companion to COPPERLINE_DBG_DSKLEN: the wall-time DSKBLK
                // raise, closing each arm -> completion interval.
                if crate::envcfg::flag("COPPERLINE_DBG_DSKLEN") {
                    log::info!(
                        "dskblk f={} secs={:.4} v={} h={}",
                        self.emulated_frames,
                        self.emulated_seconds(),
                        self.agnus.vpos,
                        self.agnus.hpos,
                    );
                }
            }
            if self.floppy.take_sync_irq() {
                self.paula.intreq |= INT_DSKSYNC;
            }
            if self.floppy.take_index_pulse() {
                let flag_irq = self.cia_b.assert_flag();
                self.cia_b.release_flag();
                if flag_irq {
                    self.paula.intreq |= INT_EXTER;
                }
            }
            self.feed_drive_sounds();
        }
        self.refresh_cia_irq_lines_at(None);
        self.flush_pending_vbi();
        self.arm_irq_recognition_latency();
    }

    /// Forward floppy mechanism activity (head steps, motor spin
    /// levels) to the synthesized drive sound effects mixed into
    /// Paula's host output. State updates here trail the
    /// already-flushed audio batch by at most one chipset tick, well
    /// under a single host sample.
    fn feed_drive_sounds(&mut self) {
        let steps = self.floppy.take_sound_steps();
        let spins = self.floppy.motor_spin_levels();
        let sounds = self.paula.drive_sounds_mut();
        if !sounds.enabled() {
            return;
        }
        for _ in 0..steps {
            sounds.step_pulse();
        }
        for (drive, spin) in spins.into_iter().enumerate() {
            sounds.set_motor_spin(drive, spin);
        }
    }

    fn refresh_cia_irq_lines_at(&mut self, trace_position: Option<BusTracePosition>) {
        self.refresh_cia_irq_lines_at_positions(trace_position, trace_position);
    }

    fn refresh_cia_irq_lines_at_positions(
        &mut self,
        cia_a_position: Option<BusTracePosition>,
        cia_b_position: Option<BusTracePosition>,
    ) {
        let pins = (
            self.cia_a.irq_line_asserted(),
            self.cia_b.irq_line_asserted(),
        );
        if pins.0 && !self.trace_cia_irq_pins.0 {
            self.note_bus_event_named_at(
                BUS_EVENT_CIAA_IRQ,
                Some("cia_a_irq"),
                cia_a_position,
                None,
            );
        }
        if pins.1 && !self.trace_cia_irq_pins.1 {
            self.note_bus_event_named_at(
                BUS_EVENT_CIAB_IRQ,
                Some("cia_b_irq"),
                cia_b_position,
                None,
            );
        }
        self.trace_cia_irq_pins = pins;
        if pins.0 {
            self.paula.intreq |= INT_PORTS;
        }
        if pins.1 {
            self.paula.intreq |= INT_EXTER;
        }
    }

    pub fn flush_pending_vbi(&mut self) {
        if self.pending_vbi == 0 {
            return;
        }
        // VERTB is asserted at the frame wrap (vpos 0, hpos 0), which the
        // timing-test row 22 confirmed matches real HW (FS-UAE/vAmiga both raise
        // it at vpos 0). The ~70 cck that real hardware adds before the handler
        // runs is interrupt RECOGNITION LATENCY, modelled separately (see
        // irq_latency_setting / arm_irq_recognition_latency), not a raise delay.
        // Once a frame, ask any real drive whether its disk has been swapped or
        // its write-protect tab moved. Unlike an image, the medium changes
        // without the emulator being told, and the drive has to be asked even
        // with the motor stopped -- otherwise a disk put in after boot is never
        // noticed. Outside the INTREQ test on purpose: that only passes when
        // VERTB goes from acknowledged to raised, and software which disables
        // interrupts and polls the beam instead -- trackloaders and demos,
        // exactly what a real disk gets pointed at -- leaves the bit set for
        // good. Gated on it, the drive would be asked once and never again.
        // `pending_vbi` already paces this to once per frame wrap.
        #[cfg(feature = "fluxbridge")]
        self.floppy.poll_bridge_media();
        if self.paula.intreq & INT_VERTB == 0 {
            self.paula.intreq |= INT_VERTB;
            if diag_vbi() {
                log::info!(
                    "vbi-assert secs={:.5} v={} h={}",
                    self.emulated_seconds(),
                    self.agnus.vpos,
                    self.agnus.hpos
                );
            }
        }
        self.pending_vbi = 0;
    }

    pub fn flush_audio(&mut self) -> u16 {
        let cck = std::mem::take(&mut self.audio_pending_cck);
        if cck == 0 {
            return 0;
        }
        let irq = self.paula.advance_audio(cck, self.agnus.dmacon);
        self.paula.latch_interrupt_sources(irq);
        irq
    }

    pub fn frame_render_events(&self) -> &[BeamRegisterWrite] {
        if self.last_frame_render_base.is_some() {
            &self.last_frame_render_events
        } else {
            &self.current_frame_render_events
        }
    }

    pub fn frame_render_base(&self) -> RenderRegisterSnapshot {
        self.last_frame_render_base
            .unwrap_or(self.current_frame_render_base)
    }

    pub fn frame_render_available(&self) -> bool {
        self.last_frame_render_base.is_some()
    }

    pub fn frame_visible_start_vpos(&self) -> u32 {
        if self.last_frame_render_base.is_some() {
            self.last_frame_visible_start_vpos
        } else {
            self.current_frame_visible_start_vpos
        }
    }

    /// Display geometry of the frame the renderer is about to draw
    /// (the completed frame once one exists, like `frame_render_base`).
    /// The horizontal glass window for presenting a programmable scan, as
    /// (source x, width) in framebuffer pixels: a multisync monitor
    /// anchors its visible raster at the horizontal sync pulse, showing
    /// the line from the sync trailing edge to the next pulse. The
    /// captured aperture starts 31 colour clocks into the line (fb x = 0
    /// sits at Denise tick 62), so the sync-anchored window's origin can
    /// be negative. None on standard frames or when the mode leaves sync
    /// unprogrammed (the presentation then falls back to the time-linear
    /// whole-line map).
    pub fn frame_presentation_h_window(&self) -> Option<(i32, u32)> {
        if self.last_frame_render_base.is_some() {
            self.last_frame_presentation_h_window
        } else {
            self.current_frame_presentation_h_window
        }
    }

    /// Compute the window from the live Agnus sync latches and the
    /// just-latched frame geometry; called at the frame wrap (and after a
    /// state load) so presentation always sees a consistent snapshot.
    pub(crate) fn compute_presentation_h_window(&self) -> Option<(i32, u32)> {
        const CAPTURE_APERTURE_START_CCK: i32 = 31;
        let geometry = self.current_frame_geometry;
        if !geometry.programmable {
            return None;
        }
        let (hsstrt, hsstop) = self.agnus.programmable_hsync_window()?;
        let sync_len = hsstop - hsstrt;
        let visible_cck = geometry.line_cck.saturating_sub(sync_len).max(1);
        let src_x0 = (hsstop as i32 - CAPTURE_APERTURE_START_CCK) * 4;
        Some((src_x0, visible_cck * 4))
    }

    /// The vertical glass window for presenting a programmable scan, as
    /// (captured rows above the glass top negated, glass rows): a multisync
    /// monitor locks its vertical deflection to the programmed sync pulse,
    /// so the glass covers the frame from the sync trailing edge to the
    /// next pulse and the picture sits where the mode's own porches place
    /// it. The offset is the captured window's first line relative to
    /// VSSTOP. None on standard frames or when the mode leaves vertical
    /// sync unprogrammed (the presentation then keeps stretching the
    /// captured rows over the full glass height).
    pub fn frame_presentation_v_window(&self) -> Option<(i32, u32)> {
        if self.last_frame_render_base.is_some() {
            self.last_frame_presentation_v_window
        } else {
            self.current_frame_presentation_v_window
        }
    }

    /// Vertical counterpart of `compute_presentation_h_window`, latched at
    /// the same frame wrap.
    pub(crate) fn compute_presentation_v_window(&self) -> Option<(i32, u32)> {
        let geometry = self.current_frame_geometry;
        if !geometry.programmable {
            return None;
        }
        let (vsstrt, vsstop) = self.agnus.programmable_vsync_window()?;
        let sync_len = vsstop - vsstrt;
        let glass_lines = geometry.frame_lines.saturating_sub(sync_len).max(1);
        let offset = geometry.visible_start_vpos as i32 - vsstop as i32;
        Some((offset, glass_lines))
    }

    pub fn frame_geometry(&self) -> FrameGeometry {
        if self.last_frame_render_base.is_some() {
            self.last_frame_geometry
        } else {
            self.current_frame_geometry
        }
    }

    /// Canvas supersample factor of the frame the renderer is about to
    /// draw (see `bitplane::canvas_scale_for`): callers that render
    /// straight from the bus size their buffer and stride with this.
    pub fn frame_canvas_scale(&self) -> usize {
        crate::video::bitplane::canvas_scale_for(
            self.frame_geometry().programmable,
            self.frame_render_base().bplcon0,
            self.frame_render_events(),
        )
    }

    /// Beam-line count for the frame the renderer is about to draw. This is
    /// latched with `FrameGeometry`; the fallback covers old in-process
    /// snapshots whose transient geometry field predates `frame_lines`.
    pub fn frame_lines(&self) -> u32 {
        let frame_lines = self.frame_geometry().frame_lines;
        if frame_lines == 0 {
            self.agnus.current_frame_lines()
        } else {
            frame_lines
        }
    }

    pub fn frame_analyzer_enabled(&self) -> bool {
        self.frame_analyzer_enabled
    }

    pub fn set_frame_analyzer_enabled(&mut self, enabled: bool) {
        if self.frame_analyzer_enabled == enabled {
            return;
        }
        if !enabled && self.frame_analyzer_full {
            self.flush_timed_devices();
        }
        self.frame_analyzer_enabled = enabled;
        if !enabled {
            self.frame_analyzer_full = false;
            self.floppy.set_dma_trace_enabled(false);
        }
        self.refresh_chip_bus_observers();
        if enabled {
            self.reset_current_frame_bus_trace(true);
        } else {
            self.current_frame_bus_trace.clear();
            self.last_frame_bus_trace = None;
            self.active_frame_blit = None;
        }
    }

    pub fn frame_analyzer_full(&self) -> bool {
        self.frame_analyzer_enabled && self.frame_analyzer_full
    }

    /// Select the full Bartman-style slot level. Enabling it also arms the
    /// owner trace; disabling it retains the cheap trace when still armed.
    pub fn set_frame_analyzer_full(&mut self, enabled: bool) {
        let was_enabled = self.frame_analyzer_enabled;
        let changed = self.frame_analyzer_full != enabled;
        if changed {
            self.flush_timed_devices();
        }
        self.frame_analyzer_full = enabled;
        self.floppy.set_dma_trace_enabled(enabled);
        if enabled {
            self.frame_analyzer_enabled = true;
            if changed {
                self.trace_cia_irq_pins = (
                    self.cia_a.irq_line_asserted(),
                    self.cia_b.irq_line_asserted(),
                );
                self.trace_intreq = self.paula.intreq & IRQ_SOURCE_BITS;
            }
        }
        if changed || (!was_enabled && enabled) {
            self.refresh_chip_bus_observers();
        }
        if enabled && (changed || !was_enabled) {
            self.reset_current_frame_bus_trace(true);
        } else if changed {
            // A pane can hand an in-flight trace to an owner-only profile.
            // Keep the cheap ownership and wait grids gathered so far; only
            // discard the full records the new level no longer retains.
            self.current_frame_bus_trace.demote_to_owner_only();
        }
    }

    /// Start a new trace epoch at the current beam position without changing
    /// the analyzer's level. Profiles use this before taking their RAM
    /// baseline so no pre-capture slots can be replayed against that snapshot.
    pub fn restart_frame_analyzer_trace(&mut self) {
        if !self.frame_analyzer_enabled {
            return;
        }
        self.flush_timed_devices();
        self.last_frame_bus_trace = None;
        self.reset_current_frame_bus_trace(true);
    }

    pub fn frame_bus_trace(&self) -> Option<&FrameBusTrace> {
        self.last_frame_bus_trace
            .as_ref()
            .filter(|trace| trace.has_samples())
            .or_else(|| {
                self.current_frame_bus_trace
                    .has_samples()
                    .then_some(&self.current_frame_bus_trace)
            })
    }

    fn annotate_bus_slot(
        &mut self,
        vpos: u32,
        hpos: u32,
        kind: u8,
        subtype: u8,
        reg: u16,
        addr: u32,
        data: u64,
        size: u8,
        flags: u16,
    ) {
        self.annotate_bus_slot_at(
            Some(BusTracePosition {
                frame: self.emulated_frames,
                cck: self.emulated_cck,
                vpos,
                hpos,
            }),
            kind,
            subtype,
            reg,
            addr,
            data,
            size,
            flags,
        );
    }

    fn precise_timed_device_trace(&self) -> bool {
        self.frame_analyzer_full || self.bus_event_observers != 0
    }

    #[allow(clippy::too_many_arguments)]
    fn annotate_bus_slot_at(
        &mut self,
        trace_position: Option<BusTracePosition>,
        kind: u8,
        subtype: u8,
        reg: u16,
        addr: u32,
        data: u64,
        size: u8,
        flags: u16,
    ) {
        if !self.frame_analyzer_full {
            return;
        }
        let position = trace_position.unwrap_or(BusTracePosition {
            frame: self.emulated_frames,
            cck: self.emulated_cck,
            vpos: self.agnus.vpos,
            hpos: self.agnus.hpos,
        });
        let trace = if self.current_frame_bus_trace.frame == position.frame {
            Some(&mut self.current_frame_bus_trace)
        } else {
            self.last_frame_bus_trace
                .as_mut()
                .filter(|trace| trace.frame == position.frame)
        };
        if let Some(trace) = trace {
            trace.annotate_at(position.vpos, position.hpos, |record| {
                record.kind = kind;
                record.subtype = subtype;
                record.reg = reg;
                record.addr = addr;
                record.data = data;
                record.size = size;
                record.flags = flags;
            });
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn record_instantaneous_bus_slot_at(
        &mut self,
        trace_position: Option<BusTracePosition>,
        kind: u8,
        subtype: u8,
        reg: u16,
        addr: u32,
        data: u64,
        size: u8,
        flags: u16,
    ) {
        if !self.frame_analyzer_full {
            return;
        }
        let position = trace_position.unwrap_or(BusTracePosition {
            frame: self.emulated_frames,
            cck: self.emulated_cck,
            vpos: self.agnus.vpos,
            hpos: self.agnus.hpos,
        });
        let ipl = pending_ipl(self.paula.intena & self.cpu_visible_intreq());
        let trace = if self.current_frame_bus_trace.frame == position.frame {
            Some(&mut self.current_frame_bus_trace)
        } else {
            self.last_frame_bus_trace
                .as_mut()
                .filter(|trace| trace.frame == position.frame)
        };
        if let Some(trace) = trace {
            trace.push_instantaneous_record(InstantaneousBusRecord {
                vpos: position.vpos,
                hpos: position.hpos,
                record: BusSlotRecord {
                    reg,
                    kind,
                    subtype,
                    size,
                    ipl,
                    flags,
                    addr,
                    data,
                    events: 0,
                },
            });
        }
    }

    pub fn note_bus_event(&mut self, events: u32) {
        self.note_bus_event_named(events, None);
    }

    fn note_bus_event_named(&mut self, events: u32, name: Option<&'static str>) {
        self.note_bus_event_named_at(events, name, None, None);
    }

    fn note_bus_event_named_at(
        &mut self,
        events: u32,
        name: Option<&'static str>,
        trace_position: Option<BusTracePosition>,
        ipl_override: Option<u8>,
    ) {
        let position = trace_position.unwrap_or(BusTracePosition {
            frame: self.emulated_frames,
            cck: self.emulated_cck,
            vpos: self.agnus.vpos,
            hpos: self.agnus.hpos,
        });
        if self.frame_analyzer_full {
            let trace = if self.current_frame_bus_trace.frame == position.frame {
                Some(&mut self.current_frame_bus_trace)
            } else {
                self.last_frame_bus_trace
                    .as_mut()
                    .filter(|trace| trace.frame == position.frame)
            };
            if let Some(trace) = trace {
                trace.annotate_at(position.vpos, position.hpos, |record| {
                    record.events |= events
                });
            }
        }
        if self.bus_event_observers != 0 {
            if let Some(name) = name {
                if self.bus_events.len() == BUS_EVENT_OBSERVATION_CAPACITY {
                    self.bus_events.pop_front();
                }
                let sequence = self.bus_event_next_sequence;
                self.bus_event_next_sequence = sequence.saturating_add(1);
                self.bus_events.push_back(BusEvent {
                    sequence,
                    name,
                    events,
                    frame: position.frame,
                    cck: position.cck,
                    vpos: position.vpos,
                    hpos: position.hpos,
                    ipl: ipl_override.unwrap_or_else(|| {
                        pending_ipl(self.paula.intena & self.cpu_visible_intreq())
                    }),
                });
            }
        }
    }

    pub fn set_bus_event_observation_enabled(&mut self, enabled: bool) {
        if enabled {
            if self.bus_event_observers == 0 {
                self.flush_timed_devices();
                self.bus_events.clear();
                self.trace_cia_irq_pins = (
                    self.cia_a.irq_line_asserted(),
                    self.cia_b.irq_line_asserted(),
                );
                self.trace_intreq = self.paula.intreq & IRQ_SOURCE_BITS;
            }
            self.bus_event_observers = self.bus_event_observers.saturating_add(1);
        } else {
            if self.bus_event_observers == 1 {
                self.flush_timed_devices();
            }
            self.bus_event_observers = self.bus_event_observers.saturating_sub(1);
            if self.bus_event_observers == 0 {
                self.bus_events.clear();
            }
        }
    }

    pub fn bus_event_cursor(&self) -> u64 {
        self.bus_event_next_sequence
    }

    pub fn bus_events_since(&self, cursor: u64) -> (Vec<BusEvent>, u64, u64) {
        let oldest = self
            .bus_events
            .front()
            .map_or(self.bus_event_next_sequence, |event| event.sequence);
        let dropped = oldest.saturating_sub(cursor);
        let start = cursor.max(oldest);
        let events = self
            .bus_events
            .iter()
            .filter(|event| event.sequence >= start)
            .cloned()
            .collect();
        (events, self.bus_event_next_sequence, dropped)
    }

    fn beam_trace_events_at(&self, vpos: u32, hpos: u32) -> u32 {
        let mut events = self.ddf_trace_events_at(hpos);
        if let Some((start, stop)) = self.agnus.programmable_horizontal_blank() {
            if hpos == start {
                events |= BUS_EVENT_HBS;
            }
            if hpos == stop {
                events |= BUS_EVENT_HBE;
            }
        }
        if let Some((start, stop)) = self.agnus.programmable_hsync_window() {
            if hpos == start {
                events |= BUS_EVENT_HSS;
            }
            if hpos == stop {
                events |= BUS_EVENT_HSE;
            }
        }
        if hpos == u32::from(diw_h_start(self.denise.diwstrt, self.effective_diwhigh()) / 2) {
            events |= BUS_EVENT_HDIWS;
        }
        if hpos == u32::from(diw_h_stop(self.denise.diwstop, self.effective_diwhigh()) / 2) {
            events |= BUS_EVENT_HDIWE;
        }
        // Bartman's WinUAE exporter attaches the live vertical signal states
        // to the first refresh slot of every line, rather than emitting only
        // their comparator edges. This makes VB/VS/VDIW useful as a per-line
        // guide even for the fixed PAL/NTSC generators.
        if hpos == 1 {
            if self.beam_vblank_active(vpos) {
                events |= BUS_EVENT_VB;
            }
            if self.beam_vsync_active(vpos) {
                events |= BUS_EVENT_VS;
            }
            if self.diw_vertical_open_at(vpos) {
                events |= BUS_EVENT_VDIW;
            }
        }
        // LOF/LOL are likewise line state, placed on the second refresh slot
        // by the exporter. They describe every row of a long field/line.
        if hpos == 3 {
            if self.agnus.lof {
                events |= BUS_EVENT_LOF;
            }
            if self.agnus.lol {
                events |= BUS_EVENT_LOL;
            }
        }
        events
    }

    fn beam_vblank_active(&self, vpos: u32) -> bool {
        if let Some((start, stop)) = self.agnus.programmable_vertical_blank() {
            return beam_window_contains(vpos, start, stop, self.agnus.current_frame_lines());
        }

        // The fixed blank begins on the final field line, wraps through line
        // zero, and clears at the first display-DMA line (PAL $19, NTSC $14).
        let stop = match self.agnus.video_standard() {
            VideoStandard::Pal => 25,
            VideoStandard::Ntsc => 20,
        };
        vpos < stop || vpos.saturating_add(1) >= self.agnus.current_frame_lines()
    }

    fn beam_vsync_active(&self, vpos: u32) -> bool {
        if let Some((start, stop)) = self.agnus.programmable_vsync_window() {
            return beam_window_contains(vpos, start, stop, self.agnus.current_frame_lines());
        }

        let (start, stop) = match self.agnus.video_standard() {
            // PAL's long-field pulse starts a line later; both fields end at
            // line 6. NTSC uses the same lines for both field lengths.
            VideoStandard::Pal => (if self.agnus.lof { 3 } else { 2 }, 6),
            VideoStandard::Ntsc => (3, 7),
        };
        (start..stop).contains(&vpos)
    }

    fn annotate_pending_cpu_access(&mut self, vpos: u32, hpos: u32) {
        let Some(access) = self.cpu_trace_access else {
            return;
        };
        let subtype = u8::from(!matches!(access.kind, CpuBusAccessKind::Fetch));
        let reg = if matches!(access.kind, CpuBusAccessKind::Custom) {
            0x1000 | (access.addr as u16 & 0x01FE)
        } else {
            0x1000
        };
        let data = access
            .data
            .unwrap_or_else(|| self.peek_cpu_trace_data(access.addr, access.size));
        self.annotate_bus_slot(
            vpos,
            hpos,
            BUS_RECORD_CPU,
            subtype,
            reg,
            access.addr,
            data,
            access.size,
            u16::from(matches!(access.kind, CpuBusAccessKind::Write)),
        );
        self.cpu_trace_slots.push(CpuTraceSlot {
            frame: self.emulated_frames,
            vpos,
            hpos,
            value_offset: access.value_offset,
            size: access.size,
        });
    }

    fn peek_cpu_trace_data(&self, addr: u32, size: u8) -> u64 {
        (0..size).fold(0, |data, offset| {
            let byte_addr = addr.wrapping_add(u32::from(offset));
            let word = self.peek_word_any(byte_addr & !1);
            let byte = if byte_addr & 1 == 0 {
                (word >> 8) as u8
            } else {
                word as u8
            };
            (data << 8) | u64::from(byte)
        })
    }

    pub(crate) fn update_last_cpu_trace_data(&mut self, data: u64, size: u8) {
        for slot in &self.cpu_trace_slots {
            let chunk = cpu_trace_data_chunk(data, size, slot.value_offset, slot.size);
            let trace = if self.current_frame_bus_trace.frame == slot.frame {
                Some(&mut self.current_frame_bus_trace)
            } else {
                self.last_frame_bus_trace
                    .as_mut()
                    .filter(|trace| trace.frame == slot.frame)
            };
            if let Some(trace) = trace {
                trace.annotate_at(slot.vpos, slot.hpos, |record| record.data = chunk);
            }
        }
        if let Some(access) = self.cpu_trace_access.as_mut() {
            access.data = Some(cpu_trace_data_chunk(
                data,
                access.value_size,
                access.value_offset,
                access.size,
            ));
        }
    }

    fn annotate_cia_access(
        &mut self,
        cia_b: bool,
        reg: usize,
        addr: u64,
        value: u8,
        size: usize,
        write: bool,
    ) {
        if !self.frame_analyzer_full {
            return;
        }
        let (vpos, hpos) = self
            .cia_trace_slot
            .unwrap_or((self.agnus.vpos, self.agnus.hpos));
        let flags = u16::from(write)
            | 1u16 << 1
            | u16::from(cia_b) << 2
            | (reg as u16 & 0x0F) << 8
            | u16::from(self.cia_trace_phase.min(7)) << 12;
        let (base, window_size) = if cia_b {
            (crate::cpu::CIA_B_BASE, crate::cpu::CIA_B_SIZE)
        } else {
            (crate::cpu::CIA_A_BASE, crate::cpu::CIA_A_SIZE)
        };
        let raw_addr = addr as u32;
        let cpu_addr = if (base..base + window_size).contains(&raw_addr) {
            raw_addr
        } else {
            base.wrapping_add(raw_addr)
        };
        self.current_frame_bus_trace
            .annotate_at(vpos, hpos, |record| {
                record.reg = 0x1000;
                record.addr = cpu_addr;
                record.data = if cia_b && size == 2 {
                    u64::from(value) << 8
                } else {
                    u64::from(value)
                };
                record.size = size.min(2) as u8;
                record.flags |= flags;
                record.events |= BUS_EVENT_SPECIAL;
            });
    }

    pub fn current_render_events(&self) -> &[BeamRegisterWrite] {
        &self.current_frame_render_events
    }

    pub fn frame_bottom_palette_events(&self) -> &[BeamRegisterWrite] {
        if self.last_frame_render_base.is_some() {
            &self.last_frame_beam_bottom_palette_events
        } else {
            &self.beam_bottom_palette_events
        }
    }

    pub fn current_render_base(&self) -> RenderRegisterSnapshot {
        self.current_frame_render_base
    }

    pub fn frame_top_palette_end(&self) -> Palette {
        if self.last_frame_render_base.is_some() {
            self.last_frame_beam_top_palette_end
        } else {
            self.beam_top_palette
        }
    }

    pub fn frame_palette_split(&self) -> (Palette, Palette, bool) {
        if self.last_frame_render_base.is_some() {
            (
                self.last_frame_beam_top_palette,
                self.last_frame_beam_bottom_palette,
                self.last_frame_beam_bottom_palette_valid,
            )
        } else {
            (
                self.beam_top_palette,
                self.beam_bottom_palette,
                self.beam_bottom_palette_valid,
            )
        }
    }

    pub fn frame_chip_ram(&self) -> &[u8] {
        if crate::envcfg::flag("COPPERLINE_RENDER_LIVE_CHIP_RAM") {
            return &self.mem.chip_ram;
        }
        if self.last_frame_render_base.is_some()
            && self.last_frame_chip_ram.len() == self.mem.chip_ram.len()
        {
            &self.last_frame_chip_ram
        } else {
            &self.mem.chip_ram
        }
    }

    /// Shared ownership of the immutable completed-frame RAM snapshot. The
    /// render worker keeps this allocation alive while the next frame runs,
    /// avoiding a second full chip-RAM copy when a [`RenderInput`] is queued.
    pub(crate) fn frame_chip_ram_shared(&self) -> std::sync::Arc<Vec<u8>> {
        if !crate::envcfg::flag("COPPERLINE_RENDER_LIVE_CHIP_RAM")
            && self.last_frame_render_base.is_some()
            && self.last_frame_chip_ram.len() == self.mem.chip_ram.len()
        {
            std::sync::Arc::clone(&self.last_frame_chip_ram)
        } else {
            std::sync::Arc::new(self.frame_chip_ram().to_vec())
        }
    }

    pub fn frame_chip_ram_writes(&self) -> &[BeamChipRamWrite] {
        if crate::envcfg::flag("COPPERLINE_RENDER_LIVE_CHIP_RAM") {
            return &[];
        }
        if self.last_frame_render_base.is_some() {
            &self.last_frame_chip_ram_writes
        } else {
            &self.current_frame_chip_ram_writes
        }
    }

    pub fn frame_captured_bitplane_rows(&self) -> &[Option<CapturedBitplaneRow>] {
        if crate::envcfg::flag("COPPERLINE_RENDER_LIVE_CHIP_RAM") {
            return &[];
        }
        if self.last_frame_render_base.is_some()
            && self.last_frame_bitplane_rows.len() == MAX_VISIBLE_LINES
        {
            &self.last_frame_bitplane_rows
        } else {
            &self.current_frame_bitplane_rows
        }
    }

    /// Shared ownership of the completed frame's captured DMA rows. Each row
    /// owns up to eight plane vectors, so sharing the immutable bundle avoids
    /// thousands of small deep-clone allocations per queued render.
    pub(crate) fn frame_captured_bitplane_rows_shared(
        &self,
    ) -> std::sync::Arc<Vec<Option<CapturedBitplaneRow>>> {
        if !crate::envcfg::flag("COPPERLINE_RENDER_LIVE_CHIP_RAM")
            && self.last_frame_render_base.is_some()
            && self.last_frame_bitplane_rows.len() == MAX_VISIBLE_LINES
        {
            std::sync::Arc::clone(&self.last_frame_bitplane_rows)
        } else {
            std::sync::Arc::new(self.frame_captured_bitplane_rows().to_vec())
        }
    }

    pub fn frame_captured_sprite_lines(&self) -> &[CapturedSpriteLine] {
        if crate::envcfg::flag("COPPERLINE_RENDER_LIVE_CHIP_RAM") {
            return &[];
        }
        if self.last_frame_render_base.is_some() {
            &self.last_frame_sprite_lines
        } else {
            &self.current_frame_sprite_lines
        }
    }

    pub fn frame_held_sprites(&self) -> [Option<HeldSpriteLine>; 8] {
        if crate::envcfg::flag("COPPERLINE_RENDER_LIVE_CHIP_RAM") {
            return [None; 8];
        }
        if self.last_frame_render_base.is_some() {
            self.last_frame_held_sprites
        } else {
            self.current_frame_held_sprites
        }
    }

    pub fn frame_sprite_display_enable_x_by_y(&self) -> &[Option<usize>] {
        if crate::envcfg::flag("COPPERLINE_RENDER_LIVE_CHIP_RAM") {
            return &[];
        }
        if self.last_frame_render_base.is_some() {
            &self.last_frame_sprite_display_enable_x_by_y
        } else {
            &self.current_frame_sprite_display_enable_x_by_y
        }
    }

    pub fn frame_sprite_dma_observed(&self) -> bool {
        if crate::envcfg::flag("COPPERLINE_RENDER_LIVE_CHIP_RAM") {
            return false;
        }
        if self.last_frame_render_base.is_some() {
            self.last_frame_sprite_dma_observed
        } else {
            self.current_frame_sprite_dma_observed
        }
    }

    pub fn begin_cpu_slice(&mut self) {
        self.blitter_slowdown_cpu_misses = 0;
        self.slice_bus_advanced_cck = 0;
        self.slice_bus_tick = AgnusTick::default();
    }

    // -----------------------------------------------------------------
    // CIA dispatch
    // -----------------------------------------------------------------

    fn sync_cia_irq_trace_after_access(&mut self, cia_b: bool) {
        let asserted = if cia_b {
            self.cia_b.irq_line_asserted()
        } else {
            self.cia_a.irq_line_asserted()
        };
        let previous = if cia_b {
            self.trace_cia_irq_pins.1
        } else {
            self.trace_cia_irq_pins.0
        };
        if asserted && !previous {
            self.note_bus_event_named_at(
                if cia_b {
                    BUS_EVENT_CIAB_IRQ
                } else {
                    BUS_EVENT_CIAA_IRQ
                },
                Some(if cia_b { "cia_b_irq" } else { "cia_a_irq" }),
                None,
                None,
            );
        }
        if cia_b {
            self.trace_cia_irq_pins.1 = asserted;
        } else {
            self.trace_cia_irq_pins.0 = asserted;
        }
    }

    pub fn cia_a_read(&mut self, addr: u64, size: usize) -> u64 {
        self.sync_realtime_devices();
        let reg = reg_from_addr(addr);
        let mut v = self.cia_a.read(reg);
        if reg == REG_PRA {
            // PRA bits 6 and 7 (/FIR0, /FIR1) report the live port-1/port-2
            // primary button: left-mouse button or joystick fire (they share
            // the FIR line). Active-low: 0 = pressed, 1 = released.
            if self.input.ports[0].fir_asserted() {
                v &= !0x40;
            } else {
                v |= 0x40;
            }
            if self.input.ports[1].fir_asserted() {
                v &= !0x80;
            } else {
                v |= 0x80;
            }
            // PRA bits 2-5 are the selected floppy drive's active-low
            // status lines: /CHNG, /WPRO, /TK0, /RDY.
            v = (v & !0x3C) | self.floppy.cia_a_status_bits();
        }
        if reg == REG_PRB {
            // The parallel data pins are CIA-A port B. An input peripheral (an
            // audio sampler digitizing the data lines) drives them, so let it
            // override the byte the guest reads back from $BFE101.
            if let Some(byte) = self.parallel_port.read_data(self.emulated_cck) {
                v = byte;
            }
        }
        trace!("cia_a R reg={:X} sz={} val={:02X}", reg, size, v);
        self.poll_stats.tick_read("cia_a", reg);
        self.service_parallel_strobe();
        self.annotate_cia_access(false, reg, addr, v, size, false);
        self.sync_cia_irq_trace_after_access(false);
        v as u64
    }

    pub fn cia_a_write(&mut self, addr: u64, size: usize, val: u64) -> CiaSideEffect {
        self.sync_realtime_devices();
        let byte = (val & 0xFF) as u8;
        let reg = reg_from_addr(addr);
        self.annotate_cia_access(false, reg, addr, byte, size, true);
        trace!("cia_a W reg={:X} sz={} val={:02X}", reg, size, byte);
        if let Some(cartridge) = self.cartridge.as_mut() {
            cartridge.note_cia_write(false, reg, byte);
        }
        let eff = self.cia_a.write(reg, byte);
        if self.cia_a.irq_line_asserted() {
            self.paula.intreq |= INT_PORTS;
        }
        self.sync_cia_irq_trace_after_access(false);
        match eff.keyboard_handshake {
            Some(true) => self.keyboard.amiga_kdat_edge(true),
            Some(false) => {
                self.keyboard.amiga_kdat_edge(false);
                self.check_keyboard_handshake();
            }
            None => {}
        }
        if reg == REG_PRA || reg == REG_DDRA {
            // CIA-A PRA bit 1 is /LED. On post-A1000 Amigas this line also
            // switches the analogue audio low-pass filter: low = LED bright +
            // filter engaged, high = LED dim/off + filter bypassed. The A1000
            // does not have that circuit -- its filter is fixed and the LED is
            // not on this line -- so the LED never switches the filter there.
            // The A1000 is the machine with a WCS fitted at $FC0000.
            let is_a1000 = !self.mem.wcs.is_empty();
            if !is_a1000 {
                let pra = self.cia_a.read(REG_PRA);
                self.paula.set_led_filter_guest(pra & 0x02 == 0);
            }
            self.cd32_pad_cia_clock();
        }
        self.service_parallel_strobe();
        eff
    }

    /// Whether a port's CD32 pad is in serial (shift) mode: the CPU drives
    /// that port's POTxX pin low through POTGO (output-enable set, data
    /// clear; POT0X lives at bits 9/8, POT1X at bits 13/12).
    fn cd32_pad_serial_mode(&self, port: usize) -> bool {
        let x_dat = (8 + 4 * port) as u16;
        self.input.ports[port].device == PortDevice::Cd32Pad
            && self.paula.potgo & (0x3 << x_dat) == (0x2 << x_dat)
    }

    /// CD32 pad shift clock: with a pad in serial mode, a falling edge the
    /// CPU drives on that port's /FIRx line (CIA-A PRA bit 6/7 with DDR
    /// output) steps its shift register. Outside serial mode the register
    /// reloads.
    fn cd32_pad_cia_clock(&mut self) {
        let pra = self.cia_a.peek_register(REG_PRA);
        let ddra = self.cia_a.peek_register(REG_DDRA);
        for port in 0..2 {
            if self.input.ports[port].device != PortDevice::Cd32Pad {
                continue;
            }
            let fir = 0x40u8 << port;
            if self.cd32_pad_serial_mode(port) {
                let p = &mut self.input.ports[port];
                if ddra & fir != 0 && p.cd32_fire_driven_high && pra & fir == 0 {
                    p.cd32_shifter = (p.cd32_shifter - 1).max(0);
                }
            } else {
                self.input.ports[port].cd32_shifter = 8;
            }
            self.input.ports[port].cd32_fire_driven_high = ddra & pra & fir != 0;
        }
    }

    /// The serial button bit for a pad's current shift position, active-low
    /// on its POTxY pin. Order from 8 down: Blue, Red, Yellow, Green, FFW,
    /// RWD, Play; 1 is the always-high pad-present bit; 0 reads zero.
    fn cd32_pad_serial_bit(&self, port: usize) -> bool {
        let p = &self.input.ports[port];
        match p.cd32_shifter {
            0 => false,
            1 => true,
            2 => !p.cd32_play,
            3 => !p.cd32_rwd,
            4 => !p.cd32_ffw,
            5 => !p.cd32_green,
            6 => !p.cd32_yellow,
            7 => !p.fire,    // Red
            _ => !p.button2, // Blue
        }
    }

    pub fn cia_b_read(&mut self, addr: u64, size: usize) -> u64 {
        self.sync_realtime_devices();
        let reg = reg_from_addr(addr);
        let mut v = self.cia_b.read(reg);
        if reg == REG_PRA {
            // CIA-B PA0-2 are the parallel port's Centronics status inputs
            // (BUSY, POUT, SEL), pulled up when nothing drives them. An
            // attached peripheral holds them at its own levels; pins the
            // guest has switched to outputs stay CIA-driven.
            let ddr = self.cia_b.port_a_ddr();
            if let Some(lines) = self.parallel_port.control_lines() {
                let inputs = !ddr & 0x07;
                v = (v & !inputs) | (lines & inputs);
            }
            // CIA-B PA3-5 are the RS-232 handshake inputs /DSR, /CTS, and
            // /CD, arriving through the motherboard's inverting 1489
            // receivers: a line the far-end device asserts reads as a low
            // pin, and an unasserted (or unplugged) line is pulled up.
            // serial.device's 7-wire handshake and SDCMD_QUERY status read
            // them here. The levels come from whatever the serial port is
            // wired to on the host; pins the guest has switched to outputs
            // stay CIA-driven, like the Centronics inputs above.
            let inputs = !ddr & crate::serial::CIAB_PA_SERIAL_INPUTS;
            if inputs != 0 {
                let levels = self.paula.serial.control_lines().cia_b_pa_levels();
                v = (v & !inputs) | (levels & inputs);
            }
        }
        trace!("cia_b R reg={:X} sz={} val={:02X}", reg, size, v);
        self.poll_stats.tick_read("cia_b", reg);
        self.annotate_cia_access(true, reg, addr, v, size, false);
        self.sync_cia_irq_trace_after_access(true);
        if size == 2 {
            (v as u64) << 8
        } else {
            v as u64
        }
    }

    pub fn cia_b_write(&mut self, addr: u64, size: usize, val: u64) -> CiaSideEffect {
        self.sync_realtime_devices();
        let byte = if size == 2 {
            ((val >> 8) & 0xFF) as u8
        } else {
            (val & 0xFF) as u8
        };
        let reg = reg_from_addr(addr);
        self.annotate_cia_access(true, reg, addr, byte, size, true);
        trace!("cia_b W reg={:X} sz={} val={:02X}", reg, size, byte);
        if let Some(cartridge) = self.cartridge.as_mut() {
            cartridge.note_cia_write(true, reg, byte);
        }
        let anchor_tod = reg == REG_TODLO && !self.cia_b.tod_writes_alarm();
        let eff = self.cia_b.write(reg, byte);
        if self.cia_b.irq_line_asserted() {
            self.paula.intreq |= INT_EXTER;
        }
        self.sync_cia_irq_trace_after_access(true);
        if anchor_tod {
            self.cia_b.anchor_tod_to_frame(0);
        }
        if reg == REG_PRB || reg == REG_DDRB {
            let prb = self.cia_b.port_b_pins();
            self.floppy.write_prb(prb);
        }
        if reg == REG_PRA || reg == REG_DDRA {
            let pra = self.cia_b.port_a_pins();
            self.paula
                .serial
                .set_control_outputs(pra & 0x80 == 0, pra & 0x40 == 0);
        }
        eff
    }

    /// Consume CIA-A's one-shot `PC` pulse as the external Centronics
    /// `/STROBE`. The parallel data pins are CIA-A port B, and CIA-A's `PC`
    /// output pulses on any port-B access, so a guest that writes a byte to
    /// `$BFE101` and toggles the strobe drives this path. An accepting
    /// peripheral returns an `/ACK`; its falling edge is fed into CIA-A FLAG,
    /// whose existing one-E-clock interrupt delay then drives Paula PORTS
    /// through the normal timed-device path.
    fn service_parallel_strobe(&mut self) {
        if !self.cia_a.take_pc_pulse() {
            return;
        }
        let data = self.cia_a.port_b_pins();
        if self.parallel_port.strobe(data, self.emulated_cck) {
            let _ = self.cia_a.assert_flag();
            self.cia_a.release_flag();
        }
    }

    // -----------------------------------------------------------------
    // Custom chip ($DFF000) dispatch
    // -----------------------------------------------------------------

    pub fn custom_read(&mut self, addr: u64, size: usize) -> u64 {
        self.grant_cpu_bus_access_at(
            Some(custom_register_cpu_addr(addr)),
            size,
            CpuBusAccessKind::Custom,
        );
        if self.cpu_short_bus_cycle {
            self.bill_020_read_data_wait();
            self.bill_custom_register_return();
            // A custom read crosses the chipset's own 16-bit bus, so it keeps
            // the single synchroniser clock rather than the chip-RAM figure;
            // it feeds the shared phase like any other bus time. Custom
            // accesses are deliberately not synchronised to the chip clock
            // until a probe measures them (the copper-poll beam row is exact
            // today and would move).
            self.bill_cpu_bus_clocks(1);
        }
        // Read-only custom registers (INTREQR, DSKBYTR, SERDATR, POTxDAT, ...)
        // reflect timed-device state, so apply the deferred device clocks before
        // reading.
        self.flush_timed_devices();
        let off = (addr & 0xFFF) as u16;
        if self.regcheck.is_some() {
            self.note_cpu_custom_access(addr, off, size, true);
        }
        self.poll_stats.tick_read_custom(off & 0xFFE);
        let value = match size {
            1 => {
                let val = self.read_custom_word(off & 0xFFE);
                trace!("custom R8  off={:03X} val_word={:04X}", off, val);
                if diag_cpu_read_matches(off & 0xFFE, size) {
                    self.diag_cpu_read(off & 0xFFE, size, val as u64);
                }
                if addr & 1 == 0 {
                    (val >> 8) as u64
                } else {
                    (val & 0xFF) as u64
                }
            }
            4 => {
                // MOVE.L from $DFFxxx reads two consecutive register
                // words: high word at addr, low word at addr+2. Each
                // register is 16 bits wide on the custom chip bus.
                let hi = self.read_custom_word(off);
                let lo = self.read_custom_word(off.wrapping_add(2));
                let v = ((hi as u64) << 16) | (lo as u64);
                trace!("custom R32 off={:03X} val={:08X}", off, v);
                if diag_cpu_read_matches(off, size) {
                    self.diag_cpu_read(off, size, hi as u64);
                }
                if diag_cpu_read_matches(off.wrapping_add(2), size) {
                    self.diag_cpu_read(off.wrapping_add(2), size, lo as u64);
                }
                v
            }
            _ => {
                let val = self.read_custom_word(off);
                trace!("custom R16 off={:03X} val={:04X}", off, val);
                if diag_cpu_read_matches(off, size) {
                    self.diag_cpu_read(off, size, val as u64);
                }
                val as u64
            }
        };
        self.update_last_cpu_trace_data(value, size.min(4) as u8);
        value
    }

    /// CPU custom-register read trace
    /// (`COPPERLINE_DIAG_CPU_READS=1`): logs every CPU custom-register read's
    /// granted chip-bus slot and the beam position after timed devices were
    /// flushed for the value being returned.
    fn diag_cpu_read(&self, off: u16, size: usize, value: u64) {
        let (rv, rh) = self
            .cpu_custom_request_slot
            .unwrap_or((self.agnus.vpos, self.agnus.hpos));
        let (v, h) = self
            .cpu_custom_access_slot
            .unwrap_or((self.agnus.vpos, self.agnus.hpos));
        eprintln!(
            "CPUPROBE PEEK rv={:03x} rh={:02x} v={:03x} h={:02x} reg={:03x} size={} val={:04x} ev={:03x} eh={:02x}",
            rv,
            rh,
            v,
            h,
            off & 0x1FE,
            size,
            value,
            self.agnus.vpos,
            self.agnus.hpos
        );
    }

    /// One-shot env flag for the CPU write-landing trace
    /// (`COPPERLINE_DIAG_CPU_WRITES=1`): logs every CPU custom-register
    /// write's granted chip-bus slot (and the beam position the write's
    /// effect applies at) to stderr, for cross-emulator comparison against
    /// vAmiga's `VAMIGA_CPU_PROBE` trace.
    fn diag_cpu_write(&self, off: u16, word: u16) {
        let (rv, rh) = self
            .cpu_custom_request_slot
            .unwrap_or((self.agnus.vpos, self.agnus.hpos));
        let (v, h) = self
            .cpu_custom_access_slot
            .unwrap_or((self.agnus.vpos, self.agnus.hpos));
        eprintln!(
            "CPUPROBE POKE rv={:03x} rh={:02x} v={:03x} h={:02x} reg={:03x} val={:04x} ev={:03x} eh={:02x}",
            rv,
            rh,
            v,
            h,
            off & 0x1FE,
            word,
            self.agnus.vpos,
            self.agnus.hpos
        );
    }

    /// Returns true if the write set a new INTREQ bit and the caller
    /// should preempt the slice so the freshly-asserted IRQ can be
    /// delivered before agnus has a chance to OR in VERTB.
    pub fn custom_write(&mut self, addr: u64, size: usize, val: u64) -> bool {
        self.grant_cpu_bus_access_at(
            Some(custom_register_cpu_addr(addr)),
            size,
            CpuBusAccessKind::Custom,
        );
        // Apply deferred device clocks before the write lands: registers such as
        // INTREQ/INTENA/ADKCON/DSKLEN/AUDxxx/SERDAT change timed-device state, so
        // the device must first be advanced to this color clock (e.g. so a
        // pending IRQ is latched before an INTREQ clear).
        self.flush_timed_devices();
        let off = (addr & 0xFFF) as u16;
        if self.regcheck.is_some() {
            self.note_cpu_custom_access(addr, off, size, false);
        }
        self.update_last_cpu_trace_data(val, size.min(4) as u8);
        match size {
            1 => {
                // A 68000 byte write drives the byte onto BOTH halves of
                // the data bus, and the custom chips have no byte lanes:
                // they latch the full 16-bit word regardless of which
                // strobe (/UDS or /LDS) the CPU asserted. `move.b
                // v,COLOR00+1` therefore lands $vvvv in the register, not
                // an addressed-lane merge (vAmigaTS CIA/oldcnt cnt1/cnt3/
                // cnt5 ramps show the mirrored high byte on hardware;
                // vAmiga's CPU poke8 to custom space doubles the byte the
                // same way).
                let b = (val & 0xFF) as u16;
                let word = (b << 8) | b;
                trace!("custom W8  off={:03X} val={:02X}", off, b);
                if diag_cpu_writes_on() {
                    self.diag_cpu_write(off & 0xFFE, word);
                }
                self.write_custom_word_from(off & 0xFFE, word, BeamWriteSource::Cpu)
            }
            4 => {
                // MOVE.L to $DFFxxx writes two consecutive register
                // words. DiagROM relies on this for `move.l #copper,
                // COP1LCH` setting both halves of the pointer in one
                // instruction.
                let hi = ((val >> 16) & 0xFFFF) as u16;
                let lo = (val & 0xFFFF) as u16;
                trace!("custom W32 off={:03X} val={:08X}", off, val);
                if diag_cpu_writes_on() {
                    self.diag_cpu_write(off, hi);
                    self.diag_cpu_write(off.wrapping_add(2), lo);
                }
                let p1 = self.write_custom_word_from(off, hi, BeamWriteSource::Cpu);
                let p2 = self.write_custom_word_from(off.wrapping_add(2), lo, BeamWriteSource::Cpu);
                p1 || p2
            }
            _ => {
                let word = (val & 0xFFFF) as u16;
                trace!("custom W16 off={:03X} val={:04X}", off, word);
                if diag_cpu_writes_on() {
                    self.diag_cpu_write(off, word);
                }
                self.write_custom_word_from(off, word, BeamWriteSource::Cpu)
            }
        }
    }

    fn blitter_start_may_write_lowmem(&self) -> bool {
        if self.blitter.bltdpt >= 0x1000 {
            return false;
        }
        let con0 = self.blitter.bltcon0;
        let con1 = self.blitter.bltcon1;
        if con1 & BLTCON1_LINE != 0 {
            // In line mode the store is gated by channel C, and the first
            // pixel is written through BLTDPT before the D pointer follows C.
            con0 & BLTCON0_USE_C != 0
        } else {
            con0 & BLTCON0_USE_D != 0 && con1 & BLTCON1_DOFF == 0
        }
    }

    /// COPPERLINE_DIAG_BLITREGS=START:END plus COPPERLINE_DUMP_BLITMEM
    /// diagnostics shared by the classic (BLTSIZE) and ECS (BLTSIZH) blit
    /// start paths. `h`/`w` are the decoded blit dimensions.
    fn diag_blit_start(&self, h: u32, w: u32) {
        if let Some(spec) = crate::envcfg::var("COPPERLINE_DIAG_BLITREGS") {
            let mut parts = spec.split(':');
            let lo_t: f64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let hi_t: f64 = parts
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(f64::INFINITY);
            let secs = self.emulated_seconds();
            if (lo_t..hi_t).contains(&secs) {
                let b = &self.blitter;
                log::info!(
                    "blitregs t={secs:.6} f={} v={} h={} con0={:04X} con1={:04X} fwm={:04X} lwm={:04X} \
                     apt={:06X} bpt={:06X} cpt={:06X} dpt={:06X} amod={} bmod={} cmod={} dmod={} \
                     adat={:04X} bdat={:04X} cdat={:04X} size h={h} w={w}",
                    self.emulated_frames,
                    self.agnus.vpos,
                    self.agnus.hpos,
                    b.bltcon0,
                    b.bltcon1,
                    b.bltafwm,
                    b.bltalwm,
                    b.bltapt,
                    b.bltbpt,
                    b.bltcpt,
                    b.bltdpt,
                    b.bltamod,
                    b.bltbmod,
                    b.bltcmod,
                    b.bltdmod,
                    b.bltadat,
                    b.bltbdat,
                    b.bltcdat,
                );
            }
        }
        if let Some(spec) = crate::envcfg::var("COPPERLINE_DUMP_BLITMEM") {
            let parts: Vec<&str> = spec.split(':').collect();
            if parts.len() == 4 {
                let secs = self.emulated_seconds();
                let lo_t: f64 = parts[0].parse().unwrap_or(0.0);
                let hi_t: f64 = parts[1].parse().unwrap_or(0.0);
                let lo = usize::from_str_radix(parts[2], 16).unwrap_or(0);
                let hi = usize::from_str_radix(parts[3], 16).unwrap_or(0);
                if (lo_t..hi_t).contains(&secs) && hi > lo && hi <= self.mem.chip_ram.len() {
                    let dir = crate::envcfg::var("COPPERLINE_DUMP_BLITMEM_DIR")
                        .map(std::path::PathBuf::from)
                        .unwrap_or_else(|| std::env::temp_dir().join("copperline-blitdump"));
                    match std::fs::create_dir_all(&dir) {
                        Ok(()) => {
                            let path = dir.join(format!("{:.6}.bin", secs));
                            if let Err(err) = std::fs::write(&path, &self.mem.chip_ram[lo..hi]) {
                                log::warn!("writing blit memory dump {}: {err}", path.display());
                            }
                        }
                        Err(err) => {
                            log::warn!("creating blit memory dump dir {}: {err}", dir.display())
                        }
                    }
                }
            }
        }
    }

    /// Record a started blit into the analyzer's frame trace (no-op
    /// while the analyzer is closed).
    pub(crate) fn record_frame_blit_start(&mut self, height: u32, width_words: u32) {
        self.note_bus_event_named(BUS_EVENT_BLIT_START_FINISH, Some("blitter_start"));
        if !self.frame_analyzer_enabled
            || self.current_frame_bus_trace.blits.len() >= FRAME_BLIT_RECORD_CAP
        {
            return;
        }
        let id = self.next_frame_blit_id;
        self.next_frame_blit_id = self.next_frame_blit_id.saturating_add(1);
        self.active_frame_blit = Some(id);
        let con0 = self.blitter.bltcon0;
        let con1 = self.blitter.bltcon1;
        let line_mode = con1 & BLTCON1_LINE != 0;
        self.current_frame_bus_trace.blits.push(FrameBlitRecord {
            id,
            bltcon0: con0,
            bltcon1: con1,
            width_words,
            height,
            descending: !line_mode && con1 & BLTCON1_DESC != 0,
            line_mode,
            fill_mode: !line_mode && con1 & (BLTCON1_IFE | BLTCON1_EFE) != 0,
            channels: [
                con0 & BLTCON0_USE_A != 0,
                con0 & BLTCON0_USE_B != 0,
                con0 & BLTCON0_USE_C != 0,
                con0 & BLTCON0_USE_D != 0 && con1 & BLTCON1_DOFF == 0,
            ],
            apt: self.blitter.bltapt,
            bpt: self.blitter.bltbpt,
            cpt: self.blitter.bltcpt,
            dpt: self.blitter.bltdpt,
            modulos: [
                self.blitter.bltamod,
                self.blitter.bltbmod,
                self.blitter.bltcmod,
                self.blitter.bltdmod,
            ],
            shifts: [(con0 >> 12) as u8, (con1 >> 12) as u8],
            masks: [self.blitter.bltafwm, self.blitter.bltalwm],
            minterm: con0 as u8,
            data: [
                self.blitter.bltadat,
                if con0 & BLTCON0_USE_B != 0 || line_mode {
                    self.blitter.bltbdat
                } else {
                    self.blitter.b_hold_latch()
                },
                self.blitter.bltcdat,
            ],
            start_frame: self.emulated_frames,
            end_frame: None,
            start: (
                self.agnus.vpos.min(u32::from(u16::MAX)) as u16,
                self.agnus.hpos.min(u32::from(u16::MAX)) as u16,
            ),
            end: None,
            cycles_used: 0,
            cycles_stalled: 0,
            channel_words: std::array::from_fn(|_| Vec::new()),
            channel_addrs: std::array::from_fn(|_| Vec::new()),
            render_truncated: false,
        });
    }

    /// Account one elapsed colour clock to the active blit. `advanced`
    /// means the sequencer processed its pending bus/internal micro-cycle;
    /// otherwise the clock extended the blit through contention or disabled
    /// DMA. Called from the same arbitration decision that ticks the blitter.
    pub(super) fn record_frame_blit_quantum(&mut self, cck: u32, advanced: bool) {
        let Some(id) = self.active_frame_blit else {
            return;
        };
        let Some(record) = self
            .current_frame_bus_trace
            .blits
            .iter_mut()
            .rev()
            .find(|record| record.id == id)
        else {
            return;
        };
        let counter = if advanced {
            &mut record.cycles_used
        } else {
            &mut record.cycles_stalled
        };
        *counter = counter.saturating_add(u64::from(cck));
    }

    /// Retain the exact word/address transferred by a blitter DMA slot for
    /// the channel visualisers. Disabled channels need no entries: their
    /// BLTxDAT constants are already in the record.
    pub(super) fn record_frame_blit_access(&mut self, access: BlitBusAccess) {
        if access.size == 0 {
            return;
        }
        let Some(id) = self.active_frame_blit else {
            return;
        };
        let Some(record) = self
            .current_frame_bus_trace
            .blits
            .iter_mut()
            .rev()
            .find(|record| record.id == id)
        else {
            return;
        };
        let channel = usize::from(access.channel);
        if channel >= 4 {
            return;
        }
        if record.channel_words[channel].len() >= FRAME_BLIT_RENDER_WORD_CAP {
            record.render_truncated = true;
            return;
        }
        record.channel_words[channel].push(access.data);
        record.channel_addrs[channel].push(access.addr);
    }

    fn record_blit_accounting(&mut self) {
        if !self.bus_accounting.enabled {
            return;
        }
        if let (Some(is_line), Some(slots)) = (
            self.blitter.pending_is_line(),
            self.blitter.scheduled_slots_remaining(),
        ) {
            self.bus_accounting.record_blit(is_line, slots);
        }
    }

    /// Read a big-endian word straight out of chip RAM (debug probes only).
    /// One-line summary of the display/DMA chipset state, for the debugger.
    /// Shows the live bitplane pointers, the render-captured base pointers, and
    /// the key display registers (DMACON, DIW/DDF window, BPLCONx).
    pub fn debug_dmacon(&self) -> u16 {
        self.agnus.dmacon
    }

    pub fn debug_display_state(&self) -> String {
        format!(
            "dmacon={:#06X} diwstrt={:#06X} diwstop={:#06X} ddfstrt={:#06X} ddfstop={:#06X} \
             bplcon0={:#06X} bplcon1={:#06X} bplcon2={:#06X} bpl1mod={} bpl2mod={} \
             bplpt={:08X?} dispbplpt={:08X?}",
            self.agnus.dmacon,
            self.denise.diwstrt,
            self.denise.diwstop,
            self.denise.ddfstrt,
            self.denise.ddfstop,
            self.denise.bplcon0,
            self.denise.bplcon1,
            self.denise.bplcon2,
            self.denise.bpl1mod,
            self.denise.bpl2mod,
            self.denise.bplpt,
            self.display_dma_bplpt,
        )
    }

    /// Side-effect-free debugger view of a custom-register word.
    ///
    /// This is intentionally not the CPU-visible custom bus read path:
    /// inspecting state from GDB or the in-window debugger must not acknowledge
    /// interrupts, advance collision latches, flush audio, or return an
    /// undriven-bus value for write-only registers whose internal latch is
    /// useful to inspect.
    pub fn debug_custom_word(&self, off: u16) -> Option<u16> {
        let off = off & 0x1FE;
        let value = match off {
            0x002 => {
                let mut r = self.agnus.dmacon & 0x07FF;
                if self.blitter.busy {
                    r |= 1 << 14;
                }
                if self.blitter.bzero {
                    r |= 1 << 13;
                }
                r
            }
            0x004 => self.agnus.read_vposr(),
            0x006 => self.agnus.read_vhposr(),
            0x010 => self.paula.adkcon,
            0x01C => self.paula.intena,
            0x01E => self.cpu_visible_intreq(),
            0x080 => ((self.agnus.cop1lc >> 16) & 0x001F) as u16,
            0x082 => (self.agnus.cop1lc & 0xFFFE) as u16,
            0x084 => ((self.agnus.cop2lc >> 16) & 0x001F) as u16,
            0x086 => (self.agnus.cop2lc & 0xFFFE) as u16,
            0x096 => self.agnus.dmacon,
            0x09A => self.paula.intena,
            0x09C => self.cpu_visible_intreq(),
            0x09E => self.paula.adkcon,
            audio @ 0x0A0..=0x0DF => self.paula.peek_audio_reg_latch(audio - 0x0A0)?,
            other => self.custom_reg_latch(other)?,
        };
        Some(value)
    }

    /// The writable RAM regions a memory hunt should scan: chip RAM,
    /// slow/ranger RAM, motherboard fast RAM, accelerator (CPU-slot) fast
    /// RAM, and configured Zorro RAM boards, as (base, length) pairs.
    pub fn writable_ram_regions(&self) -> Vec<(u32, u32)> {
        let mut regions = Vec::new();
        if !self.mem.chip_ram.is_empty() {
            regions.push((
                crate::memory::CHIP_RAM_BASE as u32,
                self.mem.chip_ram.len() as u32,
            ));
        }
        if !self.mem.slow_ram.is_empty() {
            regions.push((
                crate::memory::SLOW_RAM_BASE as u32,
                self.mem.slow_ram.len() as u32,
            ));
        }
        if !self.mem.mb_ram.is_empty() {
            regions.push((self.mem.mb_ram_base() as u32, self.mem.mb_ram.len() as u32));
        }
        if !self.mem.accel_ram.is_empty() {
            regions.push((
                crate::memory::ACCEL_RAM_BASE as u32,
                self.mem.accel_ram.len() as u32,
            ));
        }
        regions.extend(self.mem.zorro.ram_regions());
        regions
    }

    /// The regions a debugger pattern search should sweep: every writable
    /// RAM bank plus the Kickstart and extended-ROM windows, in ascending
    /// address order. Sweeping the decoded map rather than a fixed
    /// address span is what lets a search reach RAM above the 24-bit
    /// space (motherboard, CPU-slot, and Zorro III banks) without walking
    /// the gigabytes of undecoded address space between them.
    ///
    /// Chip RAM is offered as the whole $000000-$1FFFFF select window
    /// rather than the fitted bank: Agnus decodes fewer address bits than
    /// the window on the smaller parts, so the bank repeats inside it and
    /// a search of CPU-visible memory sees every image.
    pub fn searchable_regions(&self) -> Vec<(u32, u32)> {
        let mut regions = self.writable_ram_regions();
        for (base, len) in regions.iter_mut() {
            if *base == crate::memory::CHIP_RAM_BASE as u32 {
                *len = crate::memory::CHIP_WINDOW_SIZE as u32;
            }
        }
        if !self.mem.rom.is_empty() {
            regions.push((crate::memory::ROM_BASE as u32, self.mem.rom.len() as u32));
        }
        if !self.mem.wcs.is_empty() {
            regions.push((crate::memory::WCS_BASE as u32, self.mem.wcs.len() as u32));
        }
        if !self.mem.extended_rom.is_empty() {
            regions.push((
                self.mem.extended_rom_base as u32,
                self.mem.extended_rom.len() as u32,
            ));
        }
        if let Some(cartridge) = self.cartridge.as_ref() {
            regions.push((cartridge.base(), cartridge.size() as u32));
        }
        regions.sort_by_key(|(base, _)| *base);
        regions
    }

    /// Read a 16-bit big-endian word from whichever RAM/ROM region maps
    /// `addr` (chip, fast, slow, motherboard, accelerator, ROM, or WCS), for the
    /// debugger's memory dumps. Returns 0 for unmapped addresses.
    pub fn peek_word_any(&self, addr: u32) -> u16 {
        use crate::memory::{ACCEL_RAM_BASE, CHIP_WINDOW_SIZE, ROM_BASE, SLOW_RAM_BASE, WCS_BASE};
        if let Some((board, off)) = self.mem.zorro.region_at(addr, 2) {
            let ram = self.mem.zorro.board_ram(board);
            return ((ram[off] as u16) << 8) | ram[off + 1] as u16;
        }
        // Device-board windows: serve what the device can answer without
        // side effects (e.g. the A2091 boot ROM, where scsi.device task
        // and node names live), 0 for live registers.
        if let Some((crate::zorro::BoardBacking::Device(dev), off)) =
            self.mem.zorro.device_region_at(addr, 2)
        {
            return crate::zorro_device::ZorroDevice::peek_word(&self.devices[dev], off)
                .unwrap_or(0);
        }
        // CPU accesses to the entire 2 MiB chip window are routed through
        // Agnus. Smaller Agnus revisions ignore their unsupported high
        // address bits, so the fitted image repeats inside that window.
        if u64::from(addr) < CHIP_WINDOW_SIZE {
            let off = (addr & self.agnus.revision().dma_addr_capability_mask()) as usize;
            if off
                .checked_add(1)
                .is_some_and(|last| last < self.mem.chip_ram.len())
            {
                return ((self.mem.chip_ram[off] as u16) << 8) | self.mem.chip_ram[off + 1] as u16;
            }
        }
        let a = addr as usize;
        let regions: [(usize, &[u8]); 6] = [
            (SLOW_RAM_BASE as usize, &self.mem.slow_ram),
            (self.mem.mb_ram_base() as usize, &self.mem.mb_ram),
            (ACCEL_RAM_BASE as usize, &self.mem.accel_ram),
            (ROM_BASE as usize, &self.mem.rom),
            (WCS_BASE as usize, &self.mem.wcs),
            (self.mem.extended_rom_base as usize, &self.mem.extended_rom),
        ];
        for (base, mem) in regions {
            if a >= base && a.wrapping_sub(base) + 1 < mem.len() {
                let off = a - base;
                return ((mem[off] as u16) << 8) | mem[off + 1] as u16;
            }
        }
        // The uaelib trap stub (after the ROM windows: a CDTV extended ROM
        // at $F00000 hides it on the real bus too).
        if crate::uaelib::UaeLib::decodes(addr) {
            if let Some(word) = self
                .uaelib
                .as_ref()
                .and_then(|u| u.peek_word(addr.wrapping_sub(crate::uaelib::UAELIB_BASE)))
            {
                return word;
            }
        }
        // The freezer cartridge's bank.
        if let Some(off) = self.cartridge.as_ref().and_then(|c| c.offset(addr, 2)) {
            let bank = self.cartridge.as_ref().map_or(&[][..], |c| c.bank());
            return ((bank[off] as u16) << 8) | bank[off + 1] as u16;
        }
        0
    }

    pub fn peek_chip_word(&self, addr: usize) -> u16 {
        let ram = &self.mem.chip_ram;
        if addr + 1 >= ram.len() {
            return 0;
        }
        ((ram[addr] as u16) << 8) | ram[addr + 1] as u16
    }

    /// Emit the per-display-frame chip-bus color-clock accounting and reset
    /// the accumulators. Called once per beam frame from begin_new_beam_frame.
    fn log_bus_accounting_frame(&mut self) {
        if !self.bus_accounting.enabled {
            return;
        }
        let total: u64 = self.bus_accounting.owner_cck.iter().sum();
        if total == 0 {
            return;
        }
        let blit_idx = ChipBusOwner::Blitter.accounting_index();
        let blit_grant = self.bus_accounting.owner_cck[blit_idx];
        let blit_busy = self.bus_accounting.blitter_busy_cck;
        let grant_pct = if blit_busy > 0 {
            blit_grant as f64 / blit_busy as f64 * 100.0
        } else {
            0.0
        };
        let mut owners = String::new();
        let mut starve = String::new();
        for i in 0..9 {
            if self.bus_accounting.owner_cck[i] > 0 {
                owners.push_str(&format!(
                    " {}={}",
                    CHIP_BUS_OWNER_NAMES[i], self.bus_accounting.owner_cck[i]
                ));
            }
            if self.bus_accounting.blitter_starve_cck[i] > 0 {
                starve.push_str(&format!(
                    " {}={}",
                    CHIP_BUS_OWNER_NAMES[i], self.bus_accounting.blitter_starve_cck[i]
                ));
            }
        }
        // Optional diagnostic: sample a chip-RAM word so frame windows can be
        // grouped by a software counter. Meaningless unless the watched address
        // is known for the workload under inspection.
        let diag_ctr = self.peek_chip_word(0x4_C4EE);
        log::info!(
            "bus-acct frame={} t={:.3}s diag_ctr={} total_cck={} blit_busy={} blit_grant={} grant_pct={:.1} blits(line={}/{}cck normal={}/{}cck) |{} | blit_starve:{}",
            self.emulated_frames,
            self.emulated_seconds(),
            diag_ctr,
            total,
            blit_busy,
            blit_grant,
            grant_pct,
            self.bus_accounting.blits_line,
            self.bus_accounting.slots_line,
            self.bus_accounting.blits_normal,
            self.bus_accounting.slots_normal,
            owners,
            if starve.is_empty() { " none" } else { &starve },
        );
        log::info!(
            "bus-acct-cpu frame={} cpu_granted_slots={} cpu_missed_slots={}",
            self.emulated_frames,
            self.cpu_granted_chip_slots,
            self.cpu_missed_chip_slots,
        );
        self.cpu_granted_chip_slots = 0;
        self.cpu_missed_chip_slots = 0;
        self.bus_accounting.reset_frame();
    }

    fn reset_current_frame_bus_trace(&mut self, partial: bool) {
        if !self.frame_analyzer_enabled {
            return;
        }
        // Carry an in-flight blit into the next trace before reset. It keeps
        // the same id/start position and accumulated transfers so the two
        // frame records join losslessly.
        let carried_blit = self.active_frame_blit.and_then(|id| {
            self.current_frame_bus_trace
                .blits
                .iter()
                .rev()
                .find(|record| record.id == id)
                .cloned()
        });
        let registers = self.frame_register_snapshot();
        self.current_frame_bus_trace.reset_for_frame_with_level(
            self.emulated_frames,
            self.emulated_seconds(),
            self.agnus.current_frame_lines(),
            self.agnus.current_line_cck(),
            self.current_frame_visible_start_vpos,
            self.current_frame_geometry.visible_lines,
            partial,
            self.frame_analyzer_full,
            registers,
        );
        if let Some(blit) = carried_blit {
            self.current_frame_bus_trace.blits.push(blit);
        }
    }

    fn frame_register_snapshot(&self) -> FrameRegisterSnapshot {
        let custom = (0u16..0x200)
            .step_by(2)
            .map(|off| self.debug_custom_word(off).unwrap_or(0))
            .collect();
        let palette_hi = (0..256)
            .map(|entry| {
                self.denise
                    .palette
                    .read_banked(entry / 32, entry % 32, false)
            })
            .collect();
        let palette_lo = (0..256)
            .map(|entry| {
                self.denise
                    .palette
                    .read_banked(entry / 32, entry % 32, true)
            })
            .collect();
        let chipset_flags = u32::from(self.aga_enabled())
            | (u32::from(!matches!(self.agnus.revision(), AgnusRevision::Ocs)) << 1)
            | (u32::from(matches!(self.agnus.video_standard(), VideoStandard::Ntsc)) << 2)
            | (u32::from(self.denise.bplcon0 & 0x0004 != 0) << 3)
            | (u32::from(self.agnus.lof) << 4);
        FrameRegisterSnapshot {
            custom,
            palette_hi,
            palette_lo,
            chipset_flags,
        }
    }

    fn finish_frame_bus_trace(&mut self) {
        if !self.frame_analyzer_enabled {
            self.current_frame_bus_trace.clear();
            self.last_frame_bus_trace = None;
            return;
        }
        self.current_frame_bus_trace.finish_window(
            self.current_frame_visible_start_vpos,
            self.current_frame_geometry.visible_lines,
        );
        if self.current_frame_bus_trace.has_samples() {
            self.last_frame_bus_trace = Some(self.current_frame_bus_trace.clone());
        }
    }

    /// The `COPPERLINE_DIAG_*` frame-start dumps (bpl-dma summary, slotmap
    /// dump, copper-list length walk, BUF0/sprite snapshots), split out of
    /// `begin_new_beam_frame` so that function's real frame-boundary
    /// bookkeeping is reviewable on its own instead of interleaved with
    /// ~200 lines of opt-in diagnostic logging.
    fn diag_log_frame_start(&mut self) {
        if crate::envcfg::flag("COPPERLINE_DIAG_DISPLAY") {
            let lines: Vec<usize> = self
                .dbg_bpl_cck
                .iter()
                .enumerate()
                .filter(|(_, &c)| c > 0)
                .map(|(v, _)| v)
                .collect();
            if let (Some((&first, _)), Some((&last, _))) = (lines.split_first(), lines.split_last())
            {
                let total: u32 = self.dbg_bpl_cck.iter().sum();
                let (min, max) = lines
                    .iter()
                    .map(|&v| self.dbg_bpl_cck[v])
                    .fold((u32::MAX, 0), |(lo, hi), c| (lo.min(c), hi.max(c)));
                let anomalous: Vec<usize> = lines
                    .iter()
                    .copied()
                    .filter(|&v| self.dbg_bpl_cck[v] != 44)
                    .collect();
                log::info!(
                    "bpl-dma frame={} lines={} (v{}..v{}) total_cck={} per_line_min={} max={} anomalous_lines({})={:?}",
                    self.emulated_frames,
                    lines.len(),
                    first,
                    last,
                    total,
                    min,
                    max,
                    anomalous.len(),
                    &anomalous[..anomalous.len().min(50)],
                );
            }
            for c in self.dbg_bpl_cck.iter_mut() {
                *c = 0;
            }
        }
        if self.dbg_slotmap_on && !self.dbg_slotmap_dumped && !self.dbg_slotmap.is_empty() {
            // Dump the per-color-clock slot-owner map once, for the first frame
            // that contains a 4-plane (loading-screen gear) band. Covers the
            // 2->4->2 plane transition so it can be diffed against vAmiga's
            // DMA Debugger line by line. Codes: R refresh, B bitplane, S sprite,
            // D disk, A audio, C copper, L blitter, P cpu, . idle.
            let bcount = |v: usize| self.dbg_slotmap[v].iter().filter(|&&b| b == b'B').count();
            // Default: dump the first 4-plane (gear) frame. COPPERLINE_DIAG_SLOTMAP_AT
            // (seconds) instead dumps the first frame at/after that time -- used to
            // capture the 3D-vector-scene BLITWAIT contention.
            let trigger = match crate::envcfg::var("COPPERLINE_DIAG_SLOTMAP_AT")
                .and_then(|s| s.parse::<f64>().ok())
            {
                Some(at) => self.emulated_seconds() >= at,
                None => (140..200).any(|v: usize| bcount(v) > 50),
            };
            if trigger {
                self.dbg_slotmap_dumped = true;
                let (range_start, range_end) = crate::envcfg::var("COPPERLINE_DIAG_SLOTMAP_RANGE")
                    .and_then(|raw| {
                        let (start, end) = raw.split_once(':')?;
                        Some((
                            start.trim().parse::<usize>().ok()?,
                            end.trim().parse::<usize>().ok()?,
                        ))
                    })
                    .filter(|&(start, end)| start <= end && end < self.dbg_slotmap.len())
                    .unwrap_or((138, 198));
                log::info!(
                    "slotmap frame={} band v{}..{} (hpos 0..=0xE2); codes R/B/S/D/A/C/L/P/.",
                    self.emulated_frames,
                    range_start,
                    range_end
                );
                for v in range_start..=range_end {
                    let row = &self.dbg_slotmap[v];
                    let line: String = row[..227.min(row.len())]
                        .iter()
                        .map(|&b| b as char)
                        .collect();
                    let nb = bcount(v);
                    let ncpu = row.iter().filter(|&&b| b == b'P').count();
                    log::info!("slotmap v={v:3} B={nb:3} P={ncpu:3} |{line}");
                }
            }
        }
        if self.dbg_slotmap_on && !self.dbg_slotmap.is_empty() {
            for row in self.dbg_slotmap.iter_mut() {
                for slot in row.iter_mut() {
                    *slot = b'.';
                }
            }
        }
        if crate::envcfg::flag("COPPERLINE_DIAG_COPLEN")
            && !COPLEN_LOGGED.load(std::sync::atomic::Ordering::Relaxed)
        {
            let at = crate::envcfg::var("COPPERLINE_DIAG_COPLEN")
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(52.0);
            let secs = self.emulated_seconds();
            if secs >= at && self.last_chip_bus_owner != ChipBusOwner::Refresh {
                // walk the copper-1 list and count instructions to the terminating
                // WAIT($FF,$FE), to compare against how many the copper actually ran.
                let base = self.agnus.cop1lc as usize;
                let mut n = 0usize;
                let mut addr = base;
                let mut ended = false;
                while n < 4000 {
                    let w0 = self.peek_chip_word(addr);
                    let w1 = self.peek_chip_word(addr + 2);
                    n += 1;
                    addr += 4;
                    if w0 & 1 != 0 && w0 == 0xFFFF && w1 == 0xFFFE {
                        ended = true;
                        break;
                    }
                }
                // scan again, logging control-flow instructions (WAIT/SKIP and any
                // MOVE to COPJMP1/2 = reg 0x088/0x08A) that could cause looping.
                let mut a2 = base;
                let mut cf = String::new();
                for _ in 0..n.min(400) {
                    let w0 = self.peek_chip_word(a2);
                    let w1 = self.peek_chip_word(a2 + 2);
                    if w0 & 1 == 0 {
                        let reg = w0 & 0x01FE;
                        if reg == 0x088 || reg == 0x08A {
                            cf.push_str(&format!(" COPJMP@{a2:#08X}(reg{reg:#05X})"));
                        }
                    } else if w1 & 1 != 0 {
                        cf.push_str(&format!(" SKIP@{a2:#08X}"));
                    }
                    a2 += 4;
                }
                log::info!(
                    "coplen f={} cop1lc={:#08X} list_instructions={} ended={} ctrlflow:{}",
                    self.emulated_frames,
                    self.agnus.cop1lc,
                    n,
                    ended,
                    if cf.is_empty() {
                        " (none -- pure MOVE/WAIT list)".into()
                    } else {
                        cf
                    },
                );
                log::info!(
                    "bplsetup f={} bplpt={:08X?} bpl1mod={} bpl2mod={} bplcon0={:#06X} ddf={:#06X}..{:#06X}",
                    self.emulated_frames,
                    self.denise.bplpt,
                    self.denise.bpl1mod,
                    self.denise.bpl2mod,
                    self.denise.bplcon0,
                    self.denise.ddfstrt,
                    self.denise.ddfstop,
                );
                // dump all 4 planes at line 90 (each plane base + 24*352) to see if
                // every plane has structured data or only plane 0 is filled.
                for (p, base) in [0x03E606usize, 0x03E65E, 0x03E6B6, 0x03E70E]
                    .iter()
                    .enumerate()
                {
                    let a = base + (90 - 66) * 352;
                    let mut s = String::new();
                    for k in 0..14 {
                        s.push_str(&format!("{:04X} ", self.peek_chip_word(a + k * 2)));
                    }
                    log::info!("memdump logo-pl{}-line90@{a:#08X}: {s}", p + 1);
                }
                // walk the list and log every MOVE to a COLOR register (0x180-0x1BE)
                // with its immediate value -- the source-of-truth palette in the
                // copper list itself, to compare against what gets applied.
                let mut a3 = base;
                let mut cols = String::new();
                for _ in 0..n {
                    let w0 = self.peek_chip_word(a3);
                    let w1 = self.peek_chip_word(a3 + 2);
                    if w0 & 1 == 0 {
                        let reg = w0 & 0x01FE;
                        if (0x180..=0x1BE).contains(&reg) {
                            let idx = (reg - 0x180) / 2;
                            cols.push_str(&format!(" c{idx:02}@{a3:#08X}={:04X}", w1 & 0x0FFF));
                        }
                    } else if w0 == 0xFFFF && w1 == 0xFFFE {
                        break;
                    }
                    a3 += 4;
                }
                log::info!("coplist-colors:{cols}");
                COPLEN_LOGGED.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        if crate::envcfg::flag("COPPERLINE_DIAG_BUF0") {
            let secs = self.emulated_seconds();
            // log once per ~second across the run
            if secs.fract() < 0.02 {
                let mut nz = 0;
                for row in 0..272usize {
                    for w in 0..22usize {
                        if self.peek_chip_word(0x042EC0 + row * 44 + w * 2) != 0 {
                            nz += 1;
                        }
                    }
                }
                log::info!("buf0 nonzero-words={nz} t={secs:.4}");
            }
        }
        if crate::envcfg::flag("COPPERLINE_DIAG_SPRITES") {
            let secs = self.emulated_seconds();
            if (44.44..44.52).contains(&secs) {
                let spren = (self.agnus.dmacon >> 5) & 1;
                let aud = self.agnus.dmacon & 0x000F;
                let mut s = String::new();
                for i in 0..8 {
                    let st = &self.display_dma_sprite_state[i];
                    let ena = st.dma_enabled as u8;
                    let armed = st.last_line.is_some() as u8;
                    s.push_str(&format!(
                        " s{i}[ena={ena} armed={armed} vstrt={} vstop={} ptr={:06X}]",
                        st.vstrt, st.vstop, self.display_dma_sprpt[i]
                    ));
                }
                log::info!(
                    "sprites f={} SPREN={} AUD={:X}{}",
                    self.emulated_frames,
                    spren,
                    aud,
                    s
                );
            }
        }
    }
}

fn bus_slots_for_cpu_access(size: usize) -> u32 {
    (size.max(1) as u32).div_ceil(2)
}

fn add_agnus_tick(total: &mut AgnusTick, tick: AgnusTick) {
    total.new_lines = total.new_lines.saturating_add(tick.new_lines);
    total.new_frames = total.new_frames.saturating_add(tick.new_frames);
}

fn diw_v_start(diwstrt: u16, diwhigh: DiwHigh) -> u16 {
    diwhigh.v_start(diwstrt)
}

fn diw_v_stop(diwstop: u16, diwhigh: DiwHigh) -> u16 {
    diwhigh.v_stop(diwstop)
}

fn display_window_unprogrammed(diwstrt: u16, diwstop: u16) -> bool {
    diwstrt == 0 && diwstop == 0
}

fn visible_start_vpos_for_diw(diwstrt: u16, diwstop: u16, diwhigh: DiwHigh) -> u32 {
    if display_window_unprogrammed(diwstrt, diwstop) {
        return RENDER_VISIBLE_START_VPOS;
    }
    u32::from(diw_v_start(diwstrt, diwhigh))
        .clamp(RENDER_MIN_OVERSCAN_START_VPOS, RENDER_VISIBLE_START_VPOS)
}

fn clipped_display_rows_before_visible(
    diwstrt: u16,
    diwstop: u16,
    diwhigh: DiwHigh,
    visible_start_vpos: u32,
) -> usize {
    if display_window_unprogrammed(diwstrt, diwstop) {
        return 0;
    }
    (visible_start_vpos as i32 - diw_v_start(diwstrt, diwhigh) as i32).max(0) as usize
}

fn visible_framebuffer_y(
    vpos: u32,
    visible_start_vpos: u32,
    visible_lines: usize,
) -> Option<usize> {
    vpos.checked_sub(visible_start_vpos)
        .map(|y| y as usize)
        .filter(|&y| y < visible_lines)
}

impl Bus {
    /// Re-evaluate the vertical display flop (`diw_vertical_open`) against
    /// the current beam line: called at each line start and after any
    /// DIWSTRT/DIWSTOP/DIWHIGH write, mirroring the continuously-running
    /// hardware comparators (an equality match on DIWSTOP.V resets the
    /// flop, one on DIWSTRT.V sets it, reset winning a tie; any other line
    /// leaves it alone). The video standard's fixed line 312 (PAL) / 262
    /// (NTSC) also resets the flop, winning over a DIWSTRT match on that
    /// line, so a window whose DIWSTOP the beam never reaches stays open
    /// only to the end of its own frame and never bleeds into the next
    /// frame's top. On the hardwired beam schedule the comparator is the
    /// fixed standard line exactly as vAmiga models it
    /// (Sequencer::eolHandler, which deliberately does NOT use the current
    /// field's last line): an interlaced short field ends one line earlier
    /// and is not force-reset. VARBEAMEN bypasses the hardwired sync
    /// decode, and its programmable frame may end before line 312 or run
    /// past it mid-frame (DblPAL), so there the reset follows the
    /// programmable frame's last line instead.
    /// timing-test/vdiwprobe-flop pins the progressive-PAL render, where
    /// the fixed line and the frame's last line coincide.
    /// TODO: no hardware evidence yet pins the reset line on interlaced
    /// short fields or programmable totals; an interlaced or VARBEAMEN
    /// probe photographed on real hardware would.
    /// An unprogrammed window (both registers zero, the power-on state)
    /// keeps the level fallback so a bare machine still scans its whole
    /// overscan canvas.
    pub(crate) fn reevaluate_diw_vertical_flop(&mut self) {
        let vpos = self.agnus.vpos;
        if display_window_unprogrammed(self.denise.diwstrt, self.denise.diwstop) {
            self.diw_vertical_open = Some(display_window_contains_vpos(
                self.denise.diwstrt,
                self.denise.diwstop,
                self.effective_diwhigh(),
                vpos,
            ));
            return;
        }
        let forced_reset_line = match self.agnus.programmable_frame_lines() {
            Some(lines) => lines - 1,
            None => match self.agnus.video_standard() {
                VideoStandard::Pal => 312,
                VideoStandard::Ntsc => 262,
            },
        };
        let diwhigh = self.effective_diwhigh();
        let stop = u32::from(diw_v_stop(self.denise.diwstop, diwhigh));
        let start = u32::from(diw_v_start(self.denise.diwstrt, diwhigh));
        if vpos == stop || vpos == forced_reset_line {
            self.diw_vertical_open = Some(false);
        } else if vpos == start {
            self.diw_vertical_open = Some(true);
        }
    }

    /// The vertical display gate for a beam line: the live flop for the
    /// current line, the level approximation for any other (blitter slot
    /// lookahead walks future lines, where the flop's history does not
    /// exist yet) and for a timeline where no seed or comparator event has
    /// run (unit fixtures that teleport the beam and poke registers).
    pub(crate) fn diw_vertical_open_at(&self, vpos: u32) -> bool {
        if vpos == self.agnus.vpos {
            if let Some(open) = self.diw_vertical_open {
                return open;
            }
        }
        display_window_contains_vpos(
            self.denise.diwstrt,
            self.denise.diwstop,
            self.effective_diwhigh(),
            vpos,
        )
    }
}

fn display_window_contains_vpos(diwstrt: u16, diwstop: u16, diwhigh: DiwHigh, vpos: u32) -> bool {
    if display_window_unprogrammed(diwstrt, diwstop) {
        return (RENDER_VISIBLE_START_VPOS
            ..RENDER_VISIBLE_START_VPOS + RENDER_VISIBLE_LINES as u32)
            .contains(&vpos);
    }
    let start = diw_v_start(diwstrt, diwhigh) as u32;
    let mut stop = diw_v_stop(diwstop, diwhigh) as u32;
    // DIWSTRT.V == DIWSTOP.V never opens: both flop comparators match on
    // the same line and reset wins the tie (reevaluate_diw_vertical_flop),
    // so the level approximation must read the empty window as closed,
    // not as a full wrap-around one. The approximation cannot see a flop
    // an earlier DIWSTRT already opened this frame (a mid-frame rewrite
    // to a tie leaves it open until the shared comparator line); callers
    // that can know the flop's history -- the live flop for the current
    // line, the frame's DMA capture for rendered rows -- prefer it over
    // this level test.
    if start == stop {
        return false;
    }
    let mut v = vpos;
    if stop < start {
        stop += 0x100;
        if v < start {
            v += 0x100;
        }
    }
    v >= start && v < stop
}

fn beam_window_contains(vpos: u32, start: u32, stop: u32, total: u32) -> bool {
    if total == 0 || start >= total || stop >= total || start == stop {
        return false;
    }
    if start < stop {
        (start..stop).contains(&vpos)
    } else {
        vpos >= start || vpos < stop
    }
}

fn bitplane_words_per_row(
    revision: AgnusRevision,
    bplcon0: u16,
    fmode: u16,
    ddfstrt: u16,
    ddfstop: u16,
    harddis: bool,
) -> usize {
    let fallback = if bitplane_shres(bplcon0) {
        RENDER_FRAMEBUFFER_WIDTH as usize * 2
    } else if bitplane_hires(bplcon0) {
        RENDER_FRAMEBUFFER_WIDTH as usize
    } else {
        RENDER_FRAMEBUFFER_WIDTH as usize / 2
    } / 16;
    let Some((start, stop)) = effective_ddf_window(revision, bplcon0, ddfstrt, ddfstop, harddis)
    else {
        return fallback;
    };
    let unit = bitplane_fetch_unit(bplcon0, fmode);
    let start = crate::chipset::agnus::anchor_bitplane_fetch_start(start, unit);
    let blocks = crate::chipset::agnus::bitplane_fetch_blocks(u32::from(stop - start), unit);
    let words = blocks * (unit / bitplane_fetch_cck_per_word(bplcon0)) as usize;
    words.max(1)
}

// These fetch-mode helpers used to be independently-maintained twins of
// the agnus.rs functions of the same name/shape; they now delegate to the
// single agnus.rs implementation (kept as thin wrappers here so none of
// this file's many call sites had to change).
fn bitplane_shres(bplcon0: u16) -> bool {
    crate::chipset::agnus::bitplane_shres(bplcon0)
}

fn bitplane_hires(bplcon0: u16) -> bool {
    crate::chipset::agnus::bitplane_hires(bplcon0)
}

fn bitplane_fetch_quantum(fmode: u16) -> u32 {
    crate::chipset::agnus::bitplane_fetch_quantum(fmode)
}

fn bitplane_fetch_period(bplcon0: u16, fmode: u16) -> u32 {
    crate::chipset::agnus::bitplane_fetch_period(bplcon0, fmode)
}

fn bitplane_fetch_unit(bplcon0: u16, fmode: u16) -> u32 {
    crate::chipset::agnus::bitplane_fetch_unit(bplcon0, fmode)
}

fn plane_mask_for_count(nplanes: usize) -> u16 {
    if nplanes >= 8 {
        0x00FF
    } else {
        (1u16 << nplanes) - 1
    }
}

/// FMODE SSCAN2 (bit 15, Alice only): sprite scan doubling. Sprite data DMA
/// fetches a new line only on every second display line of the sprite; the
/// in-between line redisplays the previous data, so each fetched line covers
/// two display lines of a double-scan mode.
fn sprite_scan_doubled(fmode: u16) -> bool {
    fmode & 0x8000 != 0
}

/// AGA FMODE sprite fetch width (SPR32/SPAGEM, bits 2-3): 16-bit words per
/// sprite channel fetch.
fn sprite_fetch_quantum(fmode: u16) -> u32 {
    match (fmode >> 2) & 0x0003 {
        0 => 1,
        3 => 4,
        _ => 2,
    }
}

fn bitplane_fetch_cck_per_word(bplcon0: u16) -> u32 {
    crate::chipset::agnus::bitplane_fetch_cck_per_word(bplcon0)
}

fn chip_dma_addr_mask(chip_ram_len: usize) -> u32 {
    let bytes = chip_ram_len.next_power_of_two().clamp(2, 0x0020_0000usize);
    (bytes - 1) as u32
}

// The vertical-blank COP1LC strobe wakes the Copper early on the restart
// line: its first instruction-word fetch lands on the hpos $02 access cycle
// and a leading MOVE's write on $04, matching the vAmiga copper trace
// (jumpbpu image-list upload; first MOVE write at v=0 h=$04). The value was
// 6 while copper WAIT releases ran four colour clocks late; it moved in
// lockstep with the WAIT comparator lookahead fix so un-waited frame-start
// streams keep their calibrated screen positions.
pub(crate) const COPPER_FRAME_START_HPOS: u32 = 2;

fn copper_frame_start_vpos(_video_standard: VideoStandard) -> u32 {
    // The Copper is restarted (COP1LC reloaded into the Copper PC) at the very
    // top of every frame and runs through the vertical-blank lines, not just
    // the displayed region. Demos rely on this: their copper lists do
    // frame-top setup -- and crucially trigger the per-frame CPU work via a
    // copper MOVE to INTREQ (a SOFT/copper interrupt) -- during vblank. Holding
    // the Copper idle until the end of vblank delayed that trigger by ~25
    // lines, collapsing the CPU's pre-display work margin. Restarting at line 0
    // restores the margin real hardware gives before early display DMA fetches.
    0
}

fn sprite_dma_first_active_vpos(video_standard: VideoStandard) -> u32 {
    // Hard vertical blank inhibits sprite DMA near the top of a standard
    // field. The first line after that blank is one line earlier than bitplane
    // DMA: PAL line $19 and NTSC line $14.
    match video_standard {
        VideoStandard::Pal => PAL_SPRITE_DMA_FIRST_ACTIVE_VPOS,
        VideoStandard::Ntsc => NTSC_SPRITE_DMA_FIRST_ACTIVE_VPOS,
    }
}

fn next_chip_bus_quantum_at(hpos: u32, line_cck: u32) -> u32 {
    CHIP_BUS_SLOT_CCK.min(line_cck.saturating_sub(hpos).max(1))
}

fn effective_ddf_hpos(revision: AgnusRevision, bplcon0: u16, raw: u16) -> u16 {
    effective_ddf_start_hpos_raw(revision, bplcon0, raw)
}

fn effective_ddf_start_hpos_raw(revision: AgnusRevision, bplcon0: u16, raw: u16) -> u16 {
    crate::chipset::agnus::effective_bitplane_ddf_start_hpos(revision, bplcon0, raw)
}

fn effective_ddf_start_hpos(revision: AgnusRevision, bplcon0: u16, raw: u16) -> u16 {
    let start = effective_ddf_start_hpos_raw(revision, bplcon0, raw);
    if start == 0 {
        0
    } else {
        start.clamp(BITPLANE_DDF_HARD_START, BITPLANE_DDF_HARD_STOP)
    }
}

fn effective_ddf_window(
    revision: AgnusRevision,
    bplcon0: u16,
    ddfstrt: u16,
    ddfstop: u16,
    harddis: bool,
) -> Option<(u16, u16)> {
    crate::chipset::agnus::effective_bitplane_ddf_window(
        revision, bplcon0, ddfstrt, ddfstop, harddis,
    )
}

fn bitplane_fetch_order(bplcon0: u16, plane: usize) -> u32 {
    crate::chipset::agnus::bitplane_fetch_order(bplcon0, plane)
}

fn read_chip_word_wrapping(ram: &[u8], addr: u32) -> u16 {
    let len = ram.len();
    let a = addr as usize % len;
    u16::from_be_bytes([ram[a], ram[(a + 1) % len]])
}

fn sprite_vstart_from_words(pos: u16, ctl: u16) -> i32 {
    (((pos >> 8) & 0x00FF) | ((ctl & 0x0004) << 6)) as i32
}

fn sprite_vstop_from_ctl(ctl: u16) -> i32 {
    (((ctl >> 8) & 0x00FF) | ((ctl & 0x0002) << 7)) as i32
}

fn sprite_hstart_from_words(pos: u16, ctl: u16) -> i32 {
    (((pos & 0x00FF) << 1) | (ctl & 0x0001)) as i32
}

/// Alice FMODE.SSCAN2 masks the high bit of the sprite horizontal comparator
/// as well as scan-doubling its DMA data. Thus HSTART $100..$1FF aliases
/// $000..$0FF while SSCAN2 is active.
pub(crate) fn sprite_hstart_for_fmode(hstart: i32, fmode: u16) -> i32 {
    if fmode & 0x8000 != 0 {
        hstart & 0x00FF
    } else {
        hstart
    }
}

fn sprite_hsub_70ns_from_ctl(ctl: u16) -> bool {
    ctl & 0x0010 != 0
}

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
struct LiveSpriteCollisionSource {
    group: usize,
    hstart: i32,
    hsub_70ns: bool,
    words: [u16; 4],
    requires_odd_enable: bool,
}

#[derive(Clone, Copy)]
struct LiveManualSpriteCollisionSource {
    #[cfg_attr(not(test), allow(dead_code))]
    sprite: usize,
    source: LiveSpriteCollisionSource,
    x_start: i32,
    x_stop: i32,
}

fn live_sprite_playfield_collision_sources(
    lines: &[CapturedSpriteLine],
    beam_y: i32,
    fmode: u16,
) -> Vec<LiveSpriteCollisionSource> {
    live_sprite_collision_sources_with_beam_gated_odd(lines, beam_y, fmode)
}

fn live_sprite_collision_sources_with_beam_gated_odd(
    lines: &[CapturedSpriteLine],
    beam_y: i32,
    fmode: u16,
) -> Vec<LiveSpriteCollisionSource> {
    live_sprite_collision_sources_with_odd_policy(lines, beam_y, fmode, 0, true)
}

fn live_sprite_collision_sources_with_odd_policy(
    lines: &[CapturedSpriteLine],
    beam_y: i32,
    fmode: u16,
    clxcon: u16,
    include_disabled_odd: bool,
) -> Vec<LiveSpriteCollisionSource> {
    let mut sources = Vec::new();

    for sprite in 0..8 {
        let Some(line) = lines
            .iter()
            .find(|line| line.sprite == sprite && line.beam_y == beam_y)
        else {
            continue;
        };
        let group = sprite / 2;
        let requires_odd_enable = sprite & 1 != 0;
        if requires_odd_enable && !include_disabled_odd && clxcon & (1 << (12 + group)) == 0 {
            continue;
        }
        push_live_sprite_collision_source_if_visible(
            &mut sources,
            LiveSpriteCollisionSource {
                group,
                hstart: sprite_hstart_for_fmode(line.hstart, fmode),
                hsub_70ns: line.hsub_70ns,
                words: [line.data, line.datb, 0, 0],
                requires_odd_enable,
            },
        );
    }

    sources
}

fn push_live_sprite_collision_source_if_visible(
    sources: &mut Vec<LiveSpriteCollisionSource>,
    source: LiveSpriteCollisionSource,
) {
    if live_sprite_source_has_pixels(&source) {
        sources.push(source);
    }
}

fn sprite_pixel_repeat_subpixels_for_control(
    agnus_revision: AgnusRevision,
    bplcon0: u16,
    bplcon3: u16,
) -> i32 {
    match bplcon3 & BPLCON3_SPRES_MASK {
        0 => {
            if bplcon0 & BPLCON0_SHRES != 0 {
                2
            } else {
                4
            }
        }
        BPLCON3_SPRES_LORES => 4,
        BPLCON3_SPRES_HIRES => 2,
        BPLCON3_SPRES_SHRES => {
            if matches!(agnus_revision, AgnusRevision::AgaAlice) {
                1
            } else {
                2
            }
        }
        _ => unreachable!(),
    }
}

fn live_sprite_sprite_collision_bits(
    sources: &[LiveSpriteCollisionSource],
    control_replay: &LiveCollisionLineReplay,
    beam_y: i32,
    x_start: i32,
    x_stop: i32,
    display_enable_x: Option<i32>,
    latched_clxdat: u16,
) -> u16 {
    if sources.len() < 2 {
        return 0;
    }
    let x_start = x_start.max(0);
    let x_stop = x_stop.min(RENDER_FRAMEBUFFER_WIDTH);
    if x_start >= x_stop {
        return 0;
    }
    let target_mask = live_sprite_sprite_possible_clx_mask(sources, x_start, x_stop);
    let needed_mask = target_mask & !latched_clxdat;
    if needed_mask == 0 {
        return 0;
    }

    let mut clxdat = 0u16;
    let constant_control = control_replay.constant_control();
    for (idx, source) in sources.iter().enumerate() {
        for other in &sources[idx + 1..] {
            if source.group == other.group {
                continue;
            }
            let clx_bit = sprite_sprite_clx_bit(source.group, other.group);
            if clx_bit == 0 || needed_mask & clx_bit == 0 || clxdat & clx_bit != 0 {
                continue;
            }
            let Some((pair_x_start, pair_x_stop)) =
                live_sprite_source_pair_x_range(source, other, x_start, x_stop)
            else {
                continue;
            };
            if let Some(control) = constant_control {
                let Some((visible_x_start, visible_x_stop)) =
                    live_sprite_visible_x_range_for_control(
                        control,
                        beam_y,
                        pair_x_start,
                        pair_x_stop,
                        display_enable_x,
                    )
                else {
                    continue;
                };
                for x in visible_x_start..visible_x_stop {
                    if !live_sprite_source_collision_matches_with_control(
                        source,
                        control,
                        control.clxcon,
                        x,
                    ) {
                        continue;
                    }
                    if !live_sprite_source_collision_matches_with_control(
                        other,
                        control,
                        control.clxcon,
                        x,
                    ) {
                        continue;
                    }
                    clxdat |= clx_bit;
                    break;
                }
                if clxdat & needed_mask == needed_mask {
                    return clxdat;
                }
                continue;
            }
            for x in pair_x_start..pair_x_stop {
                let control = control_replay.control_for_x(x);
                if !live_sprite_pixel_inside_display_window(control, beam_y, x, display_enable_x) {
                    continue;
                }
                if !live_sprite_source_collision_matches(source, control_replay, control.clxcon, x)
                {
                    continue;
                }
                if !live_sprite_source_collision_matches(other, control_replay, control.clxcon, x) {
                    continue;
                }
                clxdat |= clx_bit;
                break;
            }
            if clxdat & needed_mask == needed_mask {
                return clxdat;
            }
        }
    }

    clxdat
}

fn live_manual_sprite_sprite_collision_bits_in_range(
    frame_base: RenderRegisterSnapshot,
    source_index: &BeamEventIndex,
    control_replay: &LiveCollisionLineReplay,
    beam_y: i32,
    x_start: i32,
    x_stop: i32,
    display_enable_x: Option<i32>,
    latched_clxdat: u16,
) -> u16 {
    if beam_y < 0 {
        return 0;
    }
    let x_start = x_start.max(0);
    let x_stop = x_stop.min(RENDER_FRAMEBUFFER_WIDTH);
    if x_start >= x_stop {
        return 0;
    }
    let sources =
        live_manual_sprite_collision_sources(frame_base, source_index, beam_y, x_start, x_stop);
    if sources.len() < 2 {
        return 0;
    }
    let target_mask = live_manual_sprite_sprite_possible_clx_mask(&sources, x_start, x_stop);
    let needed_mask = target_mask & !latched_clxdat;
    if needed_mask == 0 {
        return 0;
    }

    let mut clxdat = 0u16;
    if let Some(control) = control_replay.constant_control() {
        let Some((visible_x_start, visible_x_stop)) = live_sprite_visible_x_range_for_control(
            control,
            beam_y,
            x_start,
            x_stop,
            display_enable_x,
        ) else {
            return 0;
        };
        for x in visible_x_start..visible_x_stop {
            let mut occupied_groups = 0u8;
            for source in &sources {
                if x < source.x_start || x >= source.x_stop {
                    continue;
                }
                if !live_sprite_source_collision_matches_with_control(
                    &source.source,
                    control,
                    control.clxcon,
                    x,
                ) {
                    continue;
                }
                for other_group in 0..4 {
                    if occupied_groups & (1 << other_group) != 0
                        && other_group != source.source.group
                    {
                        let clx_bit = sprite_sprite_clx_bit(source.source.group, other_group);
                        if needed_mask & clx_bit != 0 {
                            clxdat |= clx_bit;
                        }
                    }
                }
                if clxdat & needed_mask == needed_mask {
                    return clxdat;
                }
                occupied_groups |= 1 << source.source.group;
            }
        }
        return clxdat;
    }

    for x in x_start..x_stop {
        let control = control_replay.control_for_x(x);
        if !live_sprite_pixel_inside_display_window(control, beam_y, x, display_enable_x) {
            continue;
        }
        let mut occupied_groups = 0u8;
        for source in &sources {
            if x < source.x_start || x >= source.x_stop {
                continue;
            }
            if !live_sprite_source_collision_matches(
                &source.source,
                control_replay,
                control.clxcon,
                x,
            ) {
                continue;
            }
            for other_group in 0..4 {
                if occupied_groups & (1 << other_group) != 0 && other_group != source.source.group {
                    let clx_bit = sprite_sprite_clx_bit(source.source.group, other_group);
                    if needed_mask & clx_bit != 0 {
                        clxdat |= clx_bit;
                    }
                }
            }
            if clxdat & needed_mask == needed_mask {
                return clxdat;
            }
            occupied_groups |= 1 << source.source.group;
        }
    }
    clxdat
}

fn live_manual_sprite_playfield_collision_bits_in_range(
    row: &CapturedBitplaneRow,
    frame_base: RenderRegisterSnapshot,
    source_index: &BeamEventIndex,
    playfield_control: &LiveCollisionLineReplay,
    sprite_control: &LiveCollisionLineReplay,
    beam_y: i32,
    x_start: i32,
    x_stop: i32,
    display_enable_x: Option<i32>,
    latched_clxdat: u16,
) -> u16 {
    if beam_y < 0 {
        return 0;
    }
    let x_start = x_start.max(0);
    let x_stop = x_stop.min(RENDER_FRAMEBUFFER_WIDTH);
    if x_start >= x_stop {
        return 0;
    }
    let sources =
        live_manual_sprite_collision_sources(frame_base, source_index, beam_y, x_start, x_stop);
    if sources.is_empty() {
        return 0;
    }
    let target_mask = live_manual_sprite_playfield_possible_clx_mask(&sources, x_start, x_stop);
    let needed_mask = target_mask & !latched_clxdat;
    if needed_mask == 0 {
        return 0;
    }

    let mut clxdat = 0u16;
    let sprite_constant_control = sprite_control.constant_control();
    let (x_start, x_stop) = if let Some(control) = sprite_constant_control {
        let Some(range) = live_sprite_visible_x_range_for_control(
            control,
            beam_y,
            x_start,
            x_stop,
            display_enable_x,
        ) else {
            return 0;
        };
        range
    } else {
        (x_start, x_stop)
    };
    for x in x_start..x_stop {
        let control = playfield_control.control_for_x(x);
        let Some(collision) = live_bitplane_collision_pixel_at(
            row,
            control.agnus_revision,
            control.bplcon0,
            control.bplcon1,
            control.clxcon,
            control.clxcon2,
            control.diwstrt,
            control.diwstop,
            control.diwhigh,
            control.ddfstrt,
            control.bpldat,
            x,
        ) else {
            continue;
        };
        for source in &sources {
            if x < source.x_start || x >= source.x_stop {
                continue;
            }
            let sprite_matches = if let Some(sprite_control_at_x) = sprite_constant_control {
                live_sprite_source_collision_matches_with_control(
                    &source.source,
                    sprite_control_at_x,
                    control.clxcon,
                    x,
                )
            } else {
                let sprite_control_at_x = sprite_control.control_for_x(x);
                live_sprite_pixel_inside_display_window(
                    sprite_control_at_x,
                    beam_y,
                    x,
                    display_enable_x,
                ) && live_sprite_source_collision_matches(
                    &source.source,
                    sprite_control,
                    control.clxcon,
                    x,
                )
            };
            if !sprite_matches {
                continue;
            }
            if collision.pf1_match {
                let clx_bit = 1 << (source.source.group + 1);
                if needed_mask & clx_bit != 0 {
                    clxdat |= clx_bit;
                }
            }
            if collision.pf2_match {
                let clx_bit = 1 << (source.source.group + 5);
                if needed_mask & clx_bit != 0 {
                    clxdat |= clx_bit;
                }
            }
            if clxdat & needed_mask == needed_mask {
                return clxdat;
            }
        }
    }
    clxdat
}

fn live_manual_sprite_collision_sources(
    frame_base: RenderRegisterSnapshot,
    event_index: &BeamEventIndex,
    beam_y: i32,
    x_start: i32,
    x_stop: i32,
) -> Vec<LiveManualSpriteCollisionSource> {
    let mut sprpos = frame_base.sprpos;
    let mut sprctl = frame_base.sprctl;
    let mut sprdata = frame_base.sprdata;
    let mut sprdatb = frame_base.sprdatb;
    let mut spr_armed = frame_base.spr_armed;
    let mut interval_start = [0i32; 8];
    let mut sources = Vec::new();

    let Some(line) = beam_y
        .checked_sub(RENDER_VISIBLE_START_VPOS as i32)
        .map(|line| line as usize)
        .filter(|&line| line < RENDER_VISIBLE_LINES)
    else {
        return sources;
    };

    for event in event_index.sprite_register_writes_before_visible_line(line) {
        apply_live_manual_sprite_event(
            &mut sprpos,
            &mut sprctl,
            &mut sprdata,
            &mut sprdatb,
            &mut spr_armed,
            *event,
        );
    }

    let line_events = event_index
        .line(line)
        .map(|line| line.sprite_register_writes())
        .unwrap_or(&[]);

    for event in line_events {
        let off = event.offset & 0x01FE;
        let sprite = ((off - 0x140) / 8) as usize;
        if sprite >= 8 {
            continue;
        }
        let event_x = if event.vpos < beam_y as u32 {
            0
        } else {
            live_manual_sprite_event_x(*event)
        };
        let source_stop = live_manual_sprite_preserved_source_stop(
            *event,
            sprpos[sprite],
            sprctl[sprite],
            bitplane_shres(frame_base.bplcon0),
            frame_base.fmode,
            event_x,
            x_stop,
        );
        push_live_manual_sprite_source(
            &mut sources,
            sprite,
            sprpos[sprite],
            sprctl[sprite],
            sprdata[sprite],
            sprdatb[sprite],
            spr_armed[sprite],
            bitplane_shres(frame_base.bplcon0),
            frame_base.fmode,
            beam_y,
            interval_start[sprite].max(x_start),
            source_stop.min(x_stop),
        );
        apply_live_manual_sprite_event(
            &mut sprpos,
            &mut sprctl,
            &mut sprdata,
            &mut sprdatb,
            &mut spr_armed,
            *event,
        );
        interval_start[sprite] = event_x;
    }

    for sprite in 0..8 {
        push_live_manual_sprite_source(
            &mut sources,
            sprite,
            sprpos[sprite],
            sprctl[sprite],
            sprdata[sprite],
            sprdatb[sprite],
            spr_armed[sprite],
            bitplane_shres(frame_base.bplcon0),
            frame_base.fmode,
            beam_y,
            interval_start[sprite].max(x_start),
            x_stop,
        );
    }

    combine_live_manual_sprite_collision_sources(sources)
}

fn live_manual_sprite_preserved_source_stop(
    event: BeamRegisterWrite,
    sprpos: u16,
    sprctl: u16,
    shres: bool,
    fmode: u16,
    event_x: i32,
    query_x_stop: i32,
) -> i32 {
    let off = event.offset & 0x01FE;
    if !(0x140..=0x17F).contains(&off) || (off - 0x140) & 0x0006 != 0 {
        return event_x;
    }
    let hstart = sprite_hstart_for_fmode(sprite_hstart_from_words(sprpos, sprctl), fmode);
    let base_x = (hstart + SPRITE_OUTPUT_DELAY_LORES - RENDER_DIW_HSTART_FB0) * 2
        + i32::from(shres && sprite_hsub_70ns_from_ctl(sprctl));
    if event_x >= base_x {
        query_x_stop
    } else {
        event_x
    }
}

fn live_manual_sprite_event_x(event: BeamRegisterWrite) -> i32 {
    let off = event.offset & 0x01FE;
    if (0x140..=0x17F).contains(&off) && (off - 0x140) & 0x0006 == 0 {
        let hpos = event.hpos.saturating_sub(DENISE_HPOS_LAG_CCK);
        return ((hpos as i32 * 2 - RENDER_DIW_HSTART_FB0) * 2).clamp(0, RENDER_FRAMEBUFFER_WIDTH);
    }
    ((event.hpos.saturating_sub(RENDER_COPPER_WAIT_HPOS_FB0)).saturating_mul(4))
        .min(RENDER_FRAMEBUFFER_WIDTH as u32) as i32
}

fn apply_live_manual_sprite_event(
    sprpos: &mut [u16; 8],
    sprctl: &mut [u16; 8],
    sprdata: &mut [u16; 8],
    sprdatb: &mut [u16; 8],
    spr_armed: &mut [bool; 8],
    event: BeamRegisterWrite,
) {
    let off = event.offset & 0x01FE;
    if !(0x140..=0x17F).contains(&off) {
        return;
    }
    let sprite = ((off - 0x140) / 8) as usize;
    if sprite >= 8 {
        return;
    }
    match (off - 0x140) & 0x0006 {
        0x0 => sprpos[sprite] = event.value,
        0x2 => {
            sprctl[sprite] = event.value;
            spr_armed[sprite] = false;
        }
        0x4 => {
            sprdata[sprite] = event.value;
            spr_armed[sprite] = true;
        }
        0x6 => sprdatb[sprite] = event.value,
        _ => {}
    }
}

fn combine_live_manual_sprite_collision_sources(
    sources: Vec<LiveManualSpriteCollisionSource>,
) -> Vec<LiveManualSpriteCollisionSource> {
    let mut combined = Vec::new();
    for source in sources {
        push_live_manual_collision_source_if_visible(
            &mut combined,
            source,
            source.x_start,
            source.x_stop,
        );
    }

    combined
}

fn push_live_manual_collision_source_if_visible(
    combined: &mut Vec<LiveManualSpriteCollisionSource>,
    mut source: LiveManualSpriteCollisionSource,
    x_start: i32,
    x_stop: i32,
) {
    if x_start >= x_stop || !live_sprite_source_has_pixels(&source.source) {
        return;
    }
    source.x_start = x_start;
    source.x_stop = x_stop;
    combined.push(source);
}

fn live_sprite_source_has_pixels(source: &LiveSpriteCollisionSource) -> bool {
    source.words.iter().any(|&word| word != 0)
}

fn live_sprite_source_framebuffer_bounds(source: &LiveSpriteCollisionSource) -> (i32, i32) {
    let x_start = (source.hstart + SPRITE_OUTPUT_DELAY_LORES - RENDER_DIW_HSTART_FB0) * 2
        + i32::from(source.hsub_70ns);
    (x_start, x_start + 32)
}

fn live_sprite_source_may_overlap_x_range(
    source: &LiveSpriteCollisionSource,
    x_start: i32,
    x_stop: i32,
) -> bool {
    let (source_start, source_stop) = live_sprite_source_framebuffer_bounds(source);
    x_start < source_stop && x_stop > source_start
}

fn live_sprite_source_pair_x_range(
    a: &LiveSpriteCollisionSource,
    b: &LiveSpriteCollisionSource,
    x_start: i32,
    x_stop: i32,
) -> Option<(i32, i32)> {
    let (a_start, a_stop) = live_sprite_source_framebuffer_bounds(a);
    let (b_start, b_stop) = live_sprite_source_framebuffer_bounds(b);
    let start = x_start.max(a_start).max(b_start);
    let stop = x_stop.min(a_stop).min(b_stop);
    (start < stop).then_some((start, stop))
}

fn live_sprite_sprite_possible_clx_mask(
    sources: &[LiveSpriteCollisionSource],
    x_start: i32,
    x_stop: i32,
) -> u16 {
    let mut mask = 0;
    for (idx, source) in sources.iter().enumerate() {
        for other in &sources[idx + 1..] {
            if source.group == other.group
                || live_sprite_source_pair_x_range(source, other, x_start, x_stop).is_none()
            {
                continue;
            }
            mask |= sprite_sprite_clx_bit(source.group, other.group);
        }
    }
    mask & CLXDAT_SPRITE_SPRITE_MASK
}

fn live_manual_sprite_sprite_possible_clx_mask(
    sources: &[LiveManualSpriteCollisionSource],
    x_start: i32,
    x_stop: i32,
) -> u16 {
    let mut mask = 0;
    for (idx, source) in sources.iter().enumerate() {
        for other in &sources[idx + 1..] {
            if source.source.group == other.source.group
                || source.x_start.max(other.x_start).max(x_start)
                    >= source.x_stop.min(other.x_stop).min(x_stop)
            {
                continue;
            }
            mask |= sprite_sprite_clx_bit(source.source.group, other.source.group);
        }
    }
    mask & CLXDAT_SPRITE_SPRITE_MASK
}

fn sprite_playfield_clx_mask_for_group(group: usize) -> u16 {
    (1 << (group + 1)) | (1 << (group + 5))
}

fn live_sprite_playfield_possible_clx_mask(
    sources: &[LiveSpriteCollisionSource],
    x_start: i32,
    x_stop: i32,
) -> u16 {
    let mut mask = 0;
    for source in sources {
        if live_sprite_source_may_overlap_x_range(source, x_start, x_stop) {
            mask |= sprite_playfield_clx_mask_for_group(source.group);
        }
    }
    mask & CLXDAT_SPRITE_PLAYFIELD_MASK
}

fn live_manual_sprite_playfield_possible_clx_mask(
    sources: &[LiveManualSpriteCollisionSource],
    x_start: i32,
    x_stop: i32,
) -> u16 {
    let mut mask = 0;
    for source in sources {
        if source.x_start < x_stop && source.x_stop > x_start {
            mask |= sprite_playfield_clx_mask_for_group(source.source.group);
        }
    }
    mask & CLXDAT_SPRITE_PLAYFIELD_MASK
}

fn live_sprite_sources_have_group_pair_overlap(
    sources: &[LiveSpriteCollisionSource],
    x_start: i32,
    x_stop: i32,
) -> bool {
    let x_start = x_start.max(0);
    let x_stop = x_stop.min(RENDER_FRAMEBUFFER_WIDTH);
    if x_start >= x_stop {
        return false;
    }

    for (idx, source) in sources.iter().enumerate() {
        for other in &sources[idx + 1..] {
            if source.group == other.group {
                continue;
            }
            if live_sprite_source_pair_x_range(source, other, x_start, x_stop).is_some() {
                return true;
            }
        }
    }
    false
}

#[allow(clippy::too_many_arguments)]
fn push_live_manual_sprite_source(
    sources: &mut Vec<LiveManualSpriteCollisionSource>,
    sprite: usize,
    sprpos: u16,
    sprctl: u16,
    sprdata: u16,
    sprdatb: u16,
    spr_armed: bool,
    shres: bool,
    fmode: u16,
    beam_y: i32,
    x_start: i32,
    x_stop: i32,
) {
    if x_start >= x_stop || !spr_armed {
        return;
    }
    let vstart = sprite_vstart_from_words(sprpos, sprctl);
    let vstop = sprite_vstop_from_ctl(sprctl);
    if beam_y < vstart || beam_y >= vstop {
        return;
    }
    sources.push(LiveManualSpriteCollisionSource {
        sprite,
        source: LiveSpriteCollisionSource {
            group: sprite / 2,
            hstart: sprite_hstart_for_fmode(sprite_hstart_from_words(sprpos, sprctl), fmode),
            hsub_70ns: shres && sprite_hsub_70ns_from_ctl(sprctl),
            words: [sprdata, sprdatb, 0, 0],
            requires_odd_enable: sprite & 1 != 0,
        },
        x_start,
        x_stop,
    });
}

#[derive(Clone, Copy, Default)]
struct LiveSpritePixelPresence {
    even: bool,
    odd: bool,
}

fn live_sprite_source_pixel_presence(
    source: &LiveSpriteCollisionSource,
    control_replay: &LiveCollisionLineReplay,
    x: i32,
) -> LiveSpritePixelPresence {
    if let Some(control) = control_replay.constant_control() {
        return live_sprite_source_pixel_presence_with_control(source, control, x);
    }

    let (sprite_base_x, sprite_stop_x) = live_sprite_source_framebuffer_bounds(source);
    if x < sprite_base_x || x >= sprite_stop_x {
        return LiveSpritePixelPresence::default();
    }
    let target_start = x * 2;
    let target_stop = target_start + 2;
    let mut subpixel_cursor = sprite_base_x * 2;
    let mut presence = LiveSpritePixelPresence::default();
    for bit in (0..16).rev() {
        let sprite_control = control_replay.control_for_x(subpixel_cursor.div_euclid(2));
        let sprite_pixel_repeat = sprite_pixel_repeat_subpixels_for_control(
            sprite_control.agnus_revision,
            sprite_control.bplcon0,
            sprite_control.bplcon3,
        );
        let subpixel_stop = subpixel_cursor + sprite_pixel_repeat;
        if subpixel_cursor < target_stop && subpixel_stop > target_start {
            let low = source.words[0] & (1 << bit) != 0 || source.words[1] & (1 << bit) != 0;
            let high = source.words[2] & (1 << bit) != 0 || source.words[3] & (1 << bit) != 0;
            if source.requires_odd_enable {
                presence.odd |= low;
            } else {
                presence.even |= low;
                presence.odd |= high;
            }
        }
        subpixel_cursor = subpixel_stop;
        if subpixel_cursor >= target_stop {
            break;
        }
    }
    presence
}

fn live_sprite_source_pixel_presence_with_control(
    source: &LiveSpriteCollisionSource,
    control: LiveCollisionControl,
    x: i32,
) -> LiveSpritePixelPresence {
    let sprite_base_x = (source.hstart + SPRITE_OUTPUT_DELAY_LORES - RENDER_DIW_HSTART_FB0) * 2
        + i32::from(source.hsub_70ns);
    let sprite_pixel_repeat = sprite_pixel_repeat_subpixels_for_control(
        control.agnus_revision,
        control.bplcon0,
        control.bplcon3,
    );
    let offset = x * 2 - sprite_base_x * 2;
    if offset < 0 {
        return LiveSpritePixelPresence::default();
    }
    let mut presence = LiveSpritePixelPresence::default();
    for subpixel in offset..offset + 2 {
        let bit_offset = subpixel / sprite_pixel_repeat;
        if !(0..16).contains(&bit_offset) {
            continue;
        }
        let bit = 15 - bit_offset;
        let mask = 1 << bit;
        let low = source.words[0] & mask != 0 || source.words[1] & mask != 0;
        let high = source.words[2] & mask != 0 || source.words[3] & mask != 0;
        if source.requires_odd_enable {
            presence.odd |= low;
        } else {
            presence.even |= low;
            presence.odd |= high;
        }
    }
    presence
}

fn live_sprite_source_collision_matches(
    source: &LiveSpriteCollisionSource,
    control_replay: &LiveCollisionLineReplay,
    clxcon: u16,
    x: i32,
) -> bool {
    let presence = live_sprite_source_pixel_presence(source, control_replay, x);
    presence.even || (presence.odd && clxcon & (1 << (12 + source.group)) != 0)
}

fn live_sprite_source_collision_matches_with_control(
    source: &LiveSpriteCollisionSource,
    control: LiveCollisionControl,
    clxcon: u16,
    x: i32,
) -> bool {
    let presence = live_sprite_source_pixel_presence_with_control(source, control, x);
    presence.even || (presence.odd && clxcon & (1 << (12 + source.group)) != 0)
}

fn live_sprite_visible_x_range_for_control(
    control: LiveCollisionControl,
    beam_y: i32,
    x_start: i32,
    x_stop: i32,
    display_enable_x: Option<i32>,
) -> Option<(i32, i32)> {
    let x_start = x_start.max(0);
    let x_stop = x_stop.min(RENDER_FRAMEBUFFER_WIDTH);
    if x_start >= x_stop {
        return None;
    }
    if live_border_sprite_enabled(control) {
        return Some((x_start, x_stop));
    }
    let display_enable_x = display_enable_x?;
    if beam_y < 0 {
        return None;
    }
    // Bitplane DMA records DIW's left edge here; a manual BPL1DAT write records
    // its beam position. Either path can make OCS/ECS sprites active vertically,
    // while DIW still clips the horizontal span.
    let (window_x_start, window_x_stop) =
        live_display_window_x(control.diwstrt, control.diwstop, control.diwhigh);
    let x_start = x_start.max(display_enable_x).max(window_x_start);
    let x_stop = if window_x_stop <= window_x_start {
        x_stop
    } else {
        x_stop.min(window_x_stop)
    };
    (x_start < x_stop).then_some((x_start, x_stop))
}

fn live_sprite_pixel_inside_display_window(
    control: LiveCollisionControl,
    beam_y: i32,
    framebuffer_x: i32,
    display_enable_x: Option<i32>,
) -> bool {
    if live_border_sprite_enabled(control) {
        return true;
    }
    if beam_y < 0 {
        return false;
    }
    if display_enable_x.is_none_or(|enable_x| framebuffer_x < enable_x) {
        return false;
    }
    // See `live_sprite_visible_x_range_for_control`: display_enable_x carries
    // the per-line manual-BPL1DAT/DMA gate, so do not apply a separate DIW
    // vertical test here.
    live_display_window_contains_x(
        control.diwstrt,
        control.diwstop,
        control.diwhigh,
        framebuffer_x,
    )
}

fn live_border_sprite_enabled(control: LiveCollisionControl) -> bool {
    control.bplcon0 & BPLCON0_ECSENA != 0
        && control.bplcon3 & BPLCON3_BRDSPRT != 0
        && control.bplcon3 & BPLCON3_BRDRBLNK == 0
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

#[cfg(test)]
fn live_bitplane_collision_bits(
    row: &CapturedBitplaneRow,
    control_replay: &LiveCollisionLineReplay,
    beam_y: i32,
) -> u16 {
    live_bitplane_collision_bits_in_range(row, control_replay, beam_y, 0, RENDER_FRAMEBUFFER_WIDTH)
}

fn live_bitplane_collision_bits_in_range(
    row: &CapturedBitplaneRow,
    control_replay: &LiveCollisionLineReplay,
    _beam_y: i32,
    x_start: i32,
    x_stop: i32,
) -> u16 {
    if row.nplanes < 2 {
        return 0;
    }

    let x_start = x_start.max(0);
    let x_stop = x_stop.min(RENDER_FRAMEBUFFER_WIDTH);
    if x_start >= x_stop {
        return 0;
    }

    for x in x_start..x_stop {
        let control = control_replay.control_for_x(x);
        if control.bplcon0 & 0x0400 == 0 {
            continue;
        }
        let Some(collision) = live_bitplane_collision_pixel_at(
            row,
            control.agnus_revision,
            control.bplcon0,
            control.bplcon1,
            control.clxcon,
            control.clxcon2,
            control.diwstrt,
            control.diwstop,
            control.diwhigh,
            control.ddfstrt,
            control.bpldat,
            x,
        ) else {
            continue;
        };
        if collision.pf1 && collision.pf2 {
            return 1;
        }
    }
    0
}

#[derive(Clone, Copy, Default)]
struct LivePlayfieldCollisionPixel {
    pf1: bool,
    pf2: bool,
    pf1_match: bool,
    pf2_match: bool,
}

#[derive(Clone, Copy)]
struct LiveCollisionControl {
    agnus_revision: AgnusRevision,
    bplcon0: u16,
    bplcon1: u16,
    bplcon3: u16,
    clxcon: u16,
    clxcon2: u16,
    diwstrt: u16,
    diwstop: u16,
    diwhigh: DiwHigh,
    ddfstrt: u16,
    bpldat: [u16; 8],
}

impl LiveCollisionControl {
    fn from_current(
        agnus_revision: AgnusRevision,
        bplcon0: u16,
        bplcon1: u16,
        bplcon3: u16,
        clxcon: u16,
        clxcon2: u16,
        diwstrt: u16,
        diwstop: u16,
        diwhigh: DiwHigh,
        ddfstrt: u16,
        bpldat: [u16; 8],
    ) -> Self {
        Self {
            agnus_revision,
            bplcon0,
            bplcon1,
            bplcon3,
            clxcon,
            clxcon2,
            diwstrt,
            diwstop,
            diwhigh,
            ddfstrt,
            bpldat,
        }
    }

    fn from_snapshot(snapshot: RenderRegisterSnapshot) -> Self {
        Self {
            agnus_revision: snapshot.agnus_revision,
            bplcon0: snapshot.bplcon0,
            bplcon1: snapshot.bplcon1,
            bplcon3: snapshot.bplcon3,
            clxcon: snapshot.clxcon,
            clxcon2: snapshot.clxcon2,
            diwstrt: snapshot.diwstrt,
            diwstop: snapshot.diwstop,
            diwhigh: snapshot.diwhigh,
            ddfstrt: snapshot.ddfstrt,
            bpldat: snapshot.bpldat,
        }
    }

    fn apply_write(&mut self, offset: u16, value: u16) {
        match offset & 0x01FE {
            0x08E => self.diwstrt = value,
            0x090 => self.diwstop = value,
            0x092 => self.ddfstrt = value,
            // Lisa resets CLXCON2 on a CLXCON write; pre-AGA CLXCON2 is
            // always zero, so mirroring the render replay unconditionally
            // changes nothing there.
            0x098 => {
                self.clxcon = value;
                self.clxcon2 = 0;
            }
            0x100 => self.bplcon0 = value,
            0x102 => self.bplcon1 = value,
            0x106 => self.bplcon3 = value,
            // Capture already admits CLXCON2 only for Lisa. Apply the
            // recorded write without consulting Agnus so a supported mixed
            // Lisa/ECS-Agnus configuration replays the same controls as the
            // renderer.
            0x10E => self.clxcon2 = value & 0x0FFF,
            0x1E4 => self.diwhigh = DiwHigh::ecs_explicit(value),
            off @ 0x110..=0x11A => {
                let plane = ((off - 0x110) / 2) as usize;
                if plane < self.bpldat.len() {
                    self.bpldat[plane] = value;
                }
            }
            _ => {}
        }
    }
}

#[derive(Clone, Copy)]
struct LiveCollisionControlSegment {
    x: i32,
    control: LiveCollisionControl,
}

struct LiveCollisionLineReplay {
    line_start: LiveCollisionControl,
    segments: Vec<LiveCollisionControlSegment>,
}

impl LiveCollisionLineReplay {
    fn from_index(
        current_control: LiveCollisionControl,
        frame_base: RenderRegisterSnapshot,
        index: &BeamEventIndex,
        beam_y: i32,
    ) -> Self {
        let Some(line) = beam_y
            .checked_sub(RENDER_VISIBLE_START_VPOS as i32)
            .map(|line| line as usize)
            .filter(|&line| line < RENDER_VISIBLE_LINES)
        else {
            return Self {
                line_start: current_control,
                segments: Vec::new(),
            };
        };

        let line_events = index
            .line(line)
            .map(|events| events.video_control_writes())
            .unwrap_or(&[]);
        let has_control_events = !line_events.is_empty()
            || index.video_control_writes_before_visible_line(line).count() != 0;
        if !has_control_events {
            return Self {
                line_start: current_control,
                segments: Vec::new(),
            };
        }

        let mut line_start = LiveCollisionControl::from_snapshot(frame_base);
        for event in index.video_control_writes_before_visible_line(line) {
            line_start.apply_write(event.offset, event.value);
        }
        let mut control = line_start;
        let mut segments: Vec<LiveCollisionControlSegment> = Vec::with_capacity(line_events.len());
        for event in line_events {
            control.apply_write(event.offset, event.value);
            let x = framebuffer_x_for_live_collision_hpos(event.hpos);
            if let Some(last) = segments.last_mut() {
                if last.x == x {
                    last.control = control;
                    continue;
                }
            }
            segments.push(LiveCollisionControlSegment { x, control });
        }
        Self {
            line_start,
            segments,
        }
    }

    fn control_for_x(&self, framebuffer_x: i32) -> LiveCollisionControl {
        if self.segments.is_empty() {
            return self.line_start;
        }
        let framebuffer_x = framebuffer_x.max(0);
        match self
            .segments
            .binary_search_by(|segment| segment.x.cmp(&framebuffer_x))
        {
            Ok(idx) => self.segments[idx].control,
            Err(0) => self.line_start,
            Err(idx) => self.segments[idx - 1].control,
        }
    }

    fn constant_control(&self) -> Option<LiveCollisionControl> {
        self.segments.is_empty().then_some(self.line_start)
    }

    fn segment_count(&self) -> usize {
        self.segments.len()
    }

    fn dual_playfield_in_range(&self, x_start: i32, x_stop: i32) -> bool {
        let x_start = x_start.max(0);
        let x_stop = x_stop.min(RENDER_FRAMEBUFFER_WIDTH);
        if x_start >= x_stop {
            return false;
        }
        if self.control_for_x(x_start).bplcon0 & 0x0400 != 0 {
            return true;
        }
        self.segments.iter().any(|segment| {
            segment.x >= x_start && segment.x < x_stop && segment.control.bplcon0 & 0x0400 != 0
        })
    }
}

fn framebuffer_x_for_live_collision_hpos(hpos: u32) -> i32 {
    hpos.saturating_sub(RENDER_COPPER_WAIT_HPOS_FB0)
        .saturating_mul(4)
        .min(RENDER_FRAMEBUFFER_WIDTH as u32) as i32
}

fn live_sprite_playfield_collision_bits_in_range(
    row: &CapturedBitplaneRow,
    sources: &[LiveSpriteCollisionSource],
    playfield_control: &LiveCollisionLineReplay,
    sprite_control: &LiveCollisionLineReplay,
    beam_y: i32,
    x_start: i32,
    x_stop: i32,
    display_enable_x: Option<i32>,
    latched_clxdat: u16,
) -> u16 {
    if sources.is_empty() {
        return 0;
    }
    let x_start = x_start.max(0);
    let x_stop = x_stop.min(RENDER_FRAMEBUFFER_WIDTH);
    if x_start >= x_stop {
        return 0;
    }
    let target_mask = live_sprite_playfield_possible_clx_mask(sources, x_start, x_stop);
    let needed_mask = target_mask & !latched_clxdat;
    if needed_mask == 0 {
        return 0;
    }

    let mut clxdat = 0u16;
    let sprite_constant_control = sprite_control.constant_control();
    for source in sources {
        let source_mask = sprite_playfield_clx_mask_for_group(source.group) & needed_mask;
        if source_mask == 0 || clxdat & source_mask == source_mask {
            continue;
        }
        let (source_start, source_stop) = live_sprite_source_framebuffer_bounds(source);
        let source_x_start = x_start.max(source_start);
        let source_x_stop = x_stop.min(source_stop);
        if source_x_start >= source_x_stop {
            continue;
        }
        let (source_x_start, source_x_stop) = if let Some(control) = sprite_constant_control {
            let Some(range) = live_sprite_visible_x_range_for_control(
                control,
                beam_y,
                source_x_start,
                source_x_stop,
                display_enable_x,
            ) else {
                continue;
            };
            range
        } else {
            (source_x_start, source_x_stop)
        };
        for x in source_x_start..source_x_stop {
            let control = playfield_control.control_for_x(x);
            let sprite_matches = if let Some(sprite_control_at_x) = sprite_constant_control {
                live_sprite_source_collision_matches_with_control(
                    source,
                    sprite_control_at_x,
                    control.clxcon,
                    x,
                )
            } else {
                let sprite_control_at_x = sprite_control.control_for_x(x);
                live_sprite_pixel_inside_display_window(
                    sprite_control_at_x,
                    beam_y,
                    x,
                    display_enable_x,
                ) && live_sprite_source_collision_matches(source, sprite_control, control.clxcon, x)
            };
            if !sprite_matches {
                continue;
            }
            let Some(collision) = live_bitplane_collision_pixel_at(
                row,
                control.agnus_revision,
                control.bplcon0,
                control.bplcon1,
                control.clxcon,
                control.clxcon2,
                control.diwstrt,
                control.diwstop,
                control.diwhigh,
                control.ddfstrt,
                control.bpldat,
                x,
            ) else {
                continue;
            };
            if collision.pf1_match {
                let clx_bit = 1 << (source.group + 1);
                if needed_mask & clx_bit != 0 {
                    clxdat |= clx_bit;
                }
            }
            if collision.pf2_match {
                let clx_bit = 1 << (source.group + 5);
                if needed_mask & clx_bit != 0 {
                    clxdat |= clx_bit;
                }
            }
            if clxdat & source_mask == source_mask {
                break;
            }
            if clxdat & needed_mask == needed_mask {
                return clxdat;
            }
        }
    }

    clxdat
}

#[allow(clippy::too_many_arguments)]
fn live_manual_bpl_collision_bits_in_range(
    frame_base: RenderRegisterSnapshot,
    bpldat_index: &BeamEventIndex,
    sprite_index: &BeamEventIndex,
    control_replay: &LiveCollisionLineReplay,
    sprite_lines: &[CapturedSpriteLine],
    beam_y: i32,
    x_start: i32,
    x_stop: i32,
    vertical_open: bool,
    display_enable_x: Option<i32>,
) -> u16 {
    const MANUAL_BPL_WORD_BITS: usize = 16;
    const MAX_BPLCON1_DELAY: usize = 15;
    const MAX_MANUAL_BPL_NATIVE_SAMPLES: usize = MANUAL_BPL_WORD_BITS + MAX_BPLCON1_DELAY;

    if beam_y < 0 {
        return 0;
    }
    let x_start = x_start.max(0);
    let x_stop = x_stop.min(RENDER_FRAMEBUFFER_WIDTH);
    if x_start >= x_stop {
        return 0;
    }
    let Some(line) = beam_y
        .checked_sub(RENDER_VISIBLE_START_VPOS as i32)
        .map(|line| line as usize)
        .filter(|&line| line < RENDER_VISIBLE_LINES)
    else {
        return 0;
    };

    let mut bpldat = frame_base.bpldat;
    for event in bpldat_index.bitplane_data_writes_before_visible_line(line) {
        apply_live_bpldat_event(&mut bpldat, event.offset, event.value);
    }

    let mut clxdat = 0u16;
    let line_events = bpldat_index
        .line(line)
        .map(|line| line.bitplane_data_writes())
        .unwrap_or(&[]);
    for event in line_events {
        let off = event.offset & 0x01FE;
        apply_live_bpldat_event(&mut bpldat, event.offset, event.value);
        if off == 0x110 {
            let segment_x = (event.hpos as i32 - RENDER_COPPER_WAIT_HPOS_FB0 as i32) * 4;
            clxdat |= live_manual_bpl_word_collision_bits(
                bpldat,
                frame_base,
                sprite_index,
                control_replay,
                sprite_lines,
                beam_y,
                segment_x,
                x_start,
                x_stop,
                MAX_MANUAL_BPL_NATIVE_SAMPLES,
                vertical_open,
                display_enable_x,
            );
        }
    }
    clxdat
}

fn apply_live_bpldat_event(bpldat: &mut [u16; 8], offset: u16, value: u16) {
    let off = offset & 0x01FE;
    if !matches!(off, 0x110..=0x11A) {
        return;
    }
    let plane = ((off - 0x110) / 2) as usize;
    if plane < bpldat.len() {
        bpldat[plane] = value;
    }
}

/// AGA extends the collision decode past the classic 6 bitplanes (Alice
/// BPU3 displays eight planes, Lisa's CLXCON2 gates planes 7-8). The live
/// path follows the same Alice-gated decode the renderer uses
/// (`ControlState::aga`); OCS/ECS keeps the 6-plane decode.
fn live_collision_aga_decode(agnus_revision: AgnusRevision) -> bool {
    matches!(agnus_revision, AgnusRevision::AgaAlice)
}

#[allow(clippy::too_many_arguments)]
fn live_manual_bpl_word_collision_bits(
    planes: [u16; 8],
    frame_base: RenderRegisterSnapshot,
    sprite_index: &BeamEventIndex,
    control_replay: &LiveCollisionLineReplay,
    sprite_lines: &[CapturedSpriteLine],
    beam_y: i32,
    segment_x: i32,
    x_start: i32,
    x_stop: i32,
    max_native_samples: usize,
    vertical_open: bool,
    display_enable_x: Option<i32>,
) -> u16 {
    const MANUAL_BPL_WORD_BITS: usize = 16;

    let sources = live_sprite_playfield_collision_sources(sprite_lines, beam_y, frame_base.fmode);
    let manual_sources =
        live_manual_sprite_collision_sources(frame_base, sprite_index, beam_y, x_start, x_stop);
    let mut clxdat = 0u16;
    let mut x_cursor = segment_x;
    let mut native_idx = 0usize;
    while native_idx < max_native_samples {
        let source_control = control_replay.control_for_x(x_cursor);
        let shres = bitplane_shres(source_control.bplcon0);
        let hires = bitplane_hires(source_control.bplcon0);
        let pixel_repeat = if hires || shres { 1 } else { 2 };
        let native_step = if shres { 2 } else { 1 };
        let mode = BitplaneMode::from_bplcon0(
            source_control.bplcon0,
            live_collision_aga_decode(source_control.agnus_revision),
        );
        let nplanes = mode.display_planes().min(planes.len());
        let dual_playfield = source_control.bplcon0 & 0x0400 != 0;
        let mut idx = 0u8;
        let mut word_active = false;
        for plane in 0..nplanes {
            let delay =
                live_scroll_for_plane(source_control.bplcon0, source_control.bplcon1, plane);
            if native_idx < delay {
                word_active = true;
                continue;
            }
            let source_bit = native_idx - delay;
            if source_bit >= MANUAL_BPL_WORD_BITS {
                continue;
            }
            word_active = true;
            let bit = 15 - source_bit;
            if planes[plane] & (1 << bit) != 0 {
                idx |= 1 << plane;
            }
        }
        if shres {
            let right_native_idx = native_idx + 1;
            for plane in 0..nplanes {
                let delay =
                    live_scroll_for_plane(source_control.bplcon0, source_control.bplcon1, plane);
                if right_native_idx < delay {
                    word_active = true;
                    continue;
                }
                let source_bit = right_native_idx - delay;
                if source_bit >= MANUAL_BPL_WORD_BITS {
                    continue;
                }
                word_active = true;
                let bit = 15 - source_bit;
                if planes[plane] & (1 << bit) != 0 {
                    idx |= 1 << plane;
                }
            }
        }
        if word_active {
            let collision = live_playfield_collision_pixel(
                idx,
                source_control.clxcon,
                source_control.clxcon2,
                dual_playfield,
            );
            for dx in 0..pixel_repeat {
                let x = x_cursor + dx;
                if x < x_start || x >= x_stop {
                    continue;
                }
                let pixel_control = control_replay.control_for_x(x);
                if !vertical_open
                    || !live_display_window_contains_x(
                        pixel_control.diwstrt,
                        pixel_control.diwstop,
                        pixel_control.diwhigh,
                        x,
                    )
                {
                    continue;
                }
                if collision.pf1_match && collision.pf2_match {
                    clxdat |= 1;
                }
                let sprite_visible = live_sprite_pixel_inside_display_window(
                    pixel_control,
                    beam_y,
                    x,
                    display_enable_x,
                );
                if !sprite_visible {
                    continue;
                }
                for source in &sources {
                    if !live_sprite_source_collision_matches(
                        source,
                        control_replay,
                        pixel_control.clxcon,
                        x,
                    ) {
                        continue;
                    }
                    if collision.pf1_match {
                        clxdat |= 1 << (source.group + 1);
                    }
                    if collision.pf2_match {
                        clxdat |= 1 << (source.group + 5);
                    }
                }
                for source in &manual_sources {
                    if x < source.x_start || x >= source.x_stop {
                        continue;
                    }
                    if !live_sprite_source_collision_matches(
                        &source.source,
                        control_replay,
                        pixel_control.clxcon,
                        x,
                    ) {
                        continue;
                    }
                    if collision.pf1_match {
                        clxdat |= 1 << (source.source.group + 1);
                    }
                    if collision.pf2_match {
                        clxdat |= 1 << (source.source.group + 5);
                    }
                }
            }
        }
        x_cursor += pixel_repeat;
        native_idx += native_step;
    }
    clxdat
}

fn live_bitplane_collision_pixel_at(
    row: &CapturedBitplaneRow,
    agnus_revision: AgnusRevision,
    bplcon0: u16,
    bplcon1: u16,
    clxcon: u16,
    clxcon2: u16,
    diwstrt: u16,
    diwstop: u16,
    diwhigh: DiwHigh,
    ddfstrt: u16,
    bpldat: [u16; 8],
    framebuffer_x: i32,
) -> Option<LivePlayfieldCollisionPixel> {
    if framebuffer_x < 0 {
        return None;
    }
    let (window_x_start, window_x_stop) = live_display_window_x(diwstrt, diwstop, diwhigh);
    if framebuffer_x < window_x_start
        || (window_x_stop > window_x_start && framebuffer_x >= window_x_stop)
    {
        return None;
    }
    let shres = bitplane_shres(bplcon0);
    let hires = bitplane_hires(bplcon0);
    let pixel_repeat = if hires || shres { 1 } else { 2 };
    let native_samples_per_pixel = if shres { 2 } else { 1 };
    let output_native_x =
        ((framebuffer_x - window_x_start) as usize / pixel_repeat) * native_samples_per_pixel;
    let fetch_start_native_x =
        live_fetch_start_native_x(agnus_revision, bplcon0, diwstrt, diwhigh, ddfstrt);
    let relative_native_x = output_native_x.checked_sub(fetch_start_native_x)?;
    let native_x = relative_native_x
        + live_fetch_origin_native_offset(agnus_revision, bplcon0, diwstrt, diwhigh, ddfstrt);
    let fetched_pixels = row.words_per_row * 16;
    let mode = BitplaneMode::from_bplcon0(bplcon0, live_collision_aga_decode(agnus_revision));
    let nplanes = mode.display_planes().min(row.nplanes);
    let dma_planes = mode.dma_planes().min(nplanes);
    let mut idx = 0u8;
    for plane in 0..nplanes {
        let delay = live_scroll_for_plane(bplcon0, bplcon1, plane);
        if native_x < delay {
            continue;
        }
        let fetch_x = native_x - delay;
        if fetch_x >= fetched_pixels {
            continue;
        }
        let word = if plane < dma_planes {
            row.planes[plane][fetch_x / 16]
        } else {
            bpldat[plane]
        };
        let bit = 15 - (fetch_x & 0x0F);
        if word & (1 << bit) != 0 {
            idx |= 1 << plane;
        }
    }
    if shres {
        let mut right_idx = 0u8;
        let right_native_x = native_x + 1;
        for plane in 0..nplanes {
            let delay = live_scroll_for_plane(bplcon0, bplcon1, plane);
            if right_native_x < delay {
                continue;
            }
            let fetch_x = right_native_x - delay;
            if fetch_x >= fetched_pixels {
                continue;
            }
            let word = if plane < dma_planes {
                row.planes[plane][fetch_x / 16]
            } else {
                bpldat[plane]
            };
            let bit = 15 - (fetch_x & 0x0F);
            if word & (1 << bit) != 0 {
                right_idx |= 1 << plane;
            }
        }
        idx |= right_idx;
    }
    Some(live_playfield_collision_pixel(
        idx,
        clxcon,
        clxcon2,
        bplcon0 & 0x0400 != 0,
    ))
}

fn live_playfield_collision_pixel(
    idx: u8,
    clxcon: u16,
    clxcon2: u16,
    dual_playfield: bool,
) -> LivePlayfieldCollisionPixel {
    let even_match = live_clxcon_planes_match(idx, clxcon, clxcon2, 1);
    let odd_match_raw = live_clxcon_planes_match(idx, clxcon, clxcon2, 0);
    let odd_match = odd_match_raw && (dual_playfield || even_match);
    LivePlayfieldCollisionPixel {
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

fn live_clxcon_planes_match(idx: u8, clxcon: u16, clxcon2: u16, first_plane: usize) -> bool {
    let mut matches = true;
    // Every CLXCON/CLXCON2-enabled plane participates in the match, not just
    // the planes the display currently fetches: a plane enabled beyond the BPU
    // count reads as 0 and still gates the collision (vAmiga checkS2PCollisions
    // compares `(dBuffer & enbp) == (mvbp & enbp)` over all planes). Regression:
    // Denise/Sprites/collision/sprcoll* set CLXCON match bits for absent planes
    // over a low-plane-count playfield. Planes 1-6 take their enable/match bits
    // from CLXCON; the AGA planes 7-8 from CLXCON2 (ENBP7/ENBP8 in bits 6-7,
    // MVBP7/MVBP8 in bits 0-1) -- with CLXCON2 zero the extra planes stay
    // disabled and pre-AGA results are unchanged.
    for plane in (first_plane..8).step_by(2) {
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

/// Per-plane BPLCON1 scroll in native samples, mirroring
/// `ControlState::pf1_scroll`/`pf2_scroll`: the OCS/ECS nibble counts
/// lo-res pixels, so it scales to 2 hi-res / 4 super-hi-res samples per
/// step and the comparison narrows with the word cadence (hi-res uses 3
/// nibble bits, super-hi-res 2).
fn live_scroll_for_plane(bplcon0: u16, bplcon1: u16, plane: usize) -> usize {
    let nibble = if plane & 1 != 0 {
        ((bplcon1 >> 4) & 0x000F) as usize
    } else {
        (bplcon1 & 0x000F) as usize
    };
    if bitplane_shres(bplcon0) {
        (nibble & 0x3) * 4
    } else if bitplane_hires(bplcon0) {
        (nibble & 0x7) * 2
    } else {
        nibble
    }
}

fn live_fetch_start_native_x(
    agnus_revision: AgnusRevision,
    bplcon0: u16,
    diwstrt: u16,
    diwhigh: DiwHigh,
    ddfstrt: u16,
) -> usize {
    (-live_fetch_origin_native_shift(agnus_revision, bplcon0, diwstrt, diwhigh, ddfstrt)).max(0)
        as usize
}

fn live_fetch_origin_native_offset(
    agnus_revision: AgnusRevision,
    bplcon0: u16,
    diwstrt: u16,
    diwhigh: DiwHigh,
    ddfstrt: u16,
) -> usize {
    live_fetch_origin_native_shift(agnus_revision, bplcon0, diwstrt, diwhigh, ddfstrt).max(0)
        as usize
}

fn live_fetch_origin_native_shift(
    agnus_revision: AgnusRevision,
    bplcon0: u16,
    diwstrt: u16,
    diwhigh: DiwHigh,
    ddfstrt: u16,
) -> i32 {
    let shres = bitplane_shres(bplcon0);
    let hires = bitplane_hires(bplcon0);
    let pixel_repeat = if hires || shres { 1 } else { 2 };
    let native_samples_per_pixel = if shres { 2 } else { 1 };
    let fetch_reference = if hires {
        RENDER_DIW_HSTART_FETCH_REFERENCE_HIRES
    } else {
        RENDER_DIW_HSTART_FETCH_REFERENCE_LORES
    };
    let display_native_shift =
        ((diw_h_start(diwstrt, diwhigh) as i32 - fetch_reference) * 2) / pixel_repeat;
    let display_native_shift = display_native_shift * native_samples_per_pixel;
    let standard_ddf = if hires || shres { 0x003C } else { 0x0038 };
    let ddf_native_scale = if shres {
        8
    } else if hires {
        4
    } else {
        2
    };
    let ddf_native_shift = (effective_ddf_start_hpos(agnus_revision, bplcon0, ddfstrt) as i32
        - standard_ddf)
        * ddf_native_scale;
    display_native_shift - ddf_native_shift
}

fn live_display_window_contains_x(
    diwstrt: u16,
    diwstop: u16,
    diwhigh: DiwHigh,
    framebuffer_x: i32,
) -> bool {
    let (left, right) = live_display_window_x(diwstrt, diwstop, diwhigh);
    framebuffer_x >= left && (right <= left || framebuffer_x < right)
}

fn live_display_window_x(diwstrt: u16, diwstop: u16, diwhigh: DiwHigh) -> (i32, i32) {
    if display_window_unprogrammed(diwstrt, diwstop) {
        return (0, RENDER_FRAMEBUFFER_WIDTH);
    }
    let start = diw_h_start(diwstrt, diwhigh) as i32;
    let mut stop = diw_h_stop(diwstop, diwhigh) as i32;
    if stop <= start {
        stop += 0x100;
    }
    let left = ((start - RENDER_DIW_HSTART_FB0).max(0) * 2).min(RENDER_FRAMEBUFFER_WIDTH);
    let mut right = ((stop - RENDER_DIW_HSTART_FB0).max(0) * 2).min(RENDER_FRAMEBUFFER_WIDTH);
    if RENDER_FRAMEBUFFER_WIDTH.saturating_sub(right) <= 2 {
        right = RENDER_FRAMEBUFFER_WIDTH;
    }
    (left, right)
}

fn diw_h_start(diwstrt: u16, diwhigh: DiwHigh) -> u16 {
    diwhigh.h_start(diwstrt)
}

fn diw_h_stop(diwstop: u16, diwhigh: DiwHigh) -> u16 {
    diwhigh.h_stop(diwstop)
}

fn is_render_relevant_custom_write(off: u16) -> bool {
    matches!(
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
            | 0x110..=0x11E
            | 0x0E0..=0x0FF
            | 0x120..=0x13F
            | 0x140..=0x17F
            | 0x1E4
            | 0x1FC
            | 0x180..=0x1BE
    )
}

fn is_live_collision_relevant_custom_write(off: u16) -> bool {
    matches!(
        off & 0x01FE,
        0x08E | 0x090 | 0x092 | 0x098 | 0x100 | 0x102 | 0x106 | 0x110..=0x11A
            | 0x140..=0x17F
            | 0x1E4
    )
}

fn is_live_collision_control_custom_write(off: u16) -> bool {
    matches!(
        off & 0x01FE,
        0x08E | 0x090 | 0x092 | 0x098 | 0x100 | 0x102 | 0x106 | 0x110..=0x11A | 0x1E4
    )
}

fn is_live_collision_bpldat_custom_write(off: u16) -> bool {
    matches!(off & 0x01FE, 0x110..=0x11A)
}

fn is_live_collision_sprite_custom_write(off: u16) -> bool {
    matches!(off & 0x01FE, 0x140..=0x17F)
}

fn is_audio_timing_custom_write(off: u16) -> bool {
    // DMACON, INTREQ (audio-interrupt acks gate the state machine's
    // AUDxIP tests), ADKCON, the audio registers, and the beam-rate
    // registers the mixer clock derives from.
    matches!(off, 0x096 | 0x09C | 0x09E | 0x0A0..=0x0DF | 0x1C0 | 0x1DC)
}

fn palette_event_sequences_equivalent(a: &[BeamRegisterWrite], b: &[BeamRegisterWrite]) -> bool {
    !a.is_empty()
        && a.len() == b.len()
        && a.iter().zip(b).all(|(a, b)| {
            (a.offset & 0x01FE) == (b.offset & 0x01FE)
                && color_register_value(a.value) == color_register_value(b.value)
                && matches!(a.offset & 0x01FE, 0x180..=0x1BE)
        })
}

mod collisions;
mod custom_regs;
mod ddf_line;
mod dma_slots;
mod frame_capture;
mod wave;

#[cfg(test)]
mod tests;
