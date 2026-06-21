// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared Amiga bus state used by the CPU core, chipset, and host I/O.
//! It owns chip RAM, ROM, CIA state, and custom-chip state, and exposes
//! typed read/write methods for memory-mapped devices.

use crate::chipset::agnus::{
    ddf_hard_bounds, sprite_dma_disabled_by_bitplane_ddf, Agnus, AgnusRevision, AgnusTick,
    VideoStandard, BEAMCON0_DUAL, BEAMCON0_HARDDIS, COLORCLOCKS_PER_LINE,
    NTSC_LONG_COLORCLOCKS_PER_LINE,
};
use crate::chipset::blitter::Blitter;
use crate::chipset::cia::{
    reg_from_addr, Cia, CiaSideEffect, Which, REG_DDRA, REG_DDRB, REG_PRA, REG_PRB, REG_TODLO,
};
use crate::chipset::copper::{Copper, CopperSlotAction, CopperWait, DMACON_COPEN};
use crate::chipset::denise::{
    color_register_value, BitplaneMode, Denise, DeniseRevision, DiwHigh, Palette,
};
use crate::chipset::keyboard::KeyboardMcu;
use crate::chipset::paula::{
    Paula, PotPins, DMACON_DMAEN, INT_BLIT, INT_COPER, INT_DSKBLK, INT_DSKSYNC, INT_EXTER,
    INT_PORTS, INT_VERTB, NTSC_AUDIO_MIN_PERIOD_CCK, PAL_AUDIO_MIN_PERIOD_CCK, PAULA_CLOCK_HZ,
};
use crate::floppy::FloppyController;
use crate::gayle::Gayle;
use crate::memory::Memory;
use crate::rtc::Msm6242Rtc;
use crate::video::{beam::BeamEventIndex, FrameGeometry, FB_HEIGHT, FB_WIDTH, MAX_VISIBLE_LINES};
use log::trace;
use std::collections::HashSet;
use std::io::Write;
use std::time::{Duration, Instant};

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

/// 68000/Amiga interrupt-recognition latency in color clocks (DEFAULT ON).
/// Real hardware takes ~96-100 cck from an interrupt request to the handler's
/// first instruction; Copperline's bare model took ~48 (finish-instruction + the
/// 44-cycle exception only), i.e. it delivered interrupts ~50 cck too early.
/// The timing-test rows 19 (handler entry) and 22 (raise position), run on
/// FS-UAE and vAmiga, localised the gap to recognition latency (the raise
/// position matches; only the raise->entry time differed). Default 65 cck makes
/// row 19 match real HW (~hpos 116 vs vAmiga 114 / FS-UAE 122). Set
/// COPPERLINE_IRQ_LATENCY_CCK to override (0 disables = the old behaviour).
const DEFAULT_IRQ_LATENCY_CCK: u32 = 65;

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

fn parse_diag_u32(raw: &str) -> Option<u32> {
    let raw = raw.trim();
    if let Some(hex) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).ok()
    } else {
        raw.parse::<u32>().ok()
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
            let first_vpos = parse_diag_u32(first).unwrap_or(0);
            let last_vpos = parse_diag_u32(last).unwrap_or(u32::MAX);
            return Some(CaprowDiag {
                first_vpos: first_vpos.min(last_vpos),
                last_vpos: first_vpos.max(last_vpos),
            });
        }
        parse_diag_u32(raw).map(|vpos| CaprowDiag {
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
const CPU_COPPER_BOTTOM_PALETTE_MIN_VPOS: u32 = 0xC0;
#[cfg(test)]
const DMACON_AUD_MASK: u16 = 0x000F;
const DMACON_DSKEN: u16 = 1 << 4;
const DMACON_SPREN: u16 = 1 << 5;
const DMACON_BLTEN: u16 = 1 << 6;
const DMACON_BPLEN: u16 = 1 << 8;
const DMACON_BLTPRI: u16 = 1 << 10;
const BLTCON1_DOFF: u16 = 1 << 7;
const COPPER_BUS_LOCKOUT_HPOS: u32 = 0x00E1;
const COPER_CPU_IRQ_DELAY_CCK: u32 = 2;
const RENDER_VISIBLE_START_VPOS: u32 = 0x2C;
const RENDER_MIN_OVERSCAN_START_VPOS: u32 = 0x1C;
const RENDER_VISIBLE_LINES: usize = FB_HEIGHT;
const RENDER_FRAMEBUFFER_WIDTH: i32 = FB_WIDTH as i32;
// Capture-side twin of `bitplane::DIW_HSTART_FB0`; held 8 colour clocks (16
// lo-res pixels) left of the standard display start so the captured window
// matches vAmiga's 716-wide cutout and includes the deep-left overscan.
const RENDER_DIW_HSTART_FB0: i32 = 0x61;
// Standard DIWSTRT $81 is the visible window edge. The first standard
// bitplane sample at DDFSTRT $38 is already one lowres native sample into the
// fetched word, so the fetch/output phase is referenced one color clock earlier.
//
// Capture-side twins of `bitplane::DIW_HSTART_FETCH_REFERENCE_*`. The hi-res
// fetch/display phase sits 3 colour clocks earlier than lo-res, so the
// reference differs by resolution (lo-res $80, hi-res $83). See the bitplane
// constant docs for the vAmiga-verified rationale.
const RENDER_DIW_HSTART_FETCH_REFERENCE_LORES: i32 = 0x80;
const RENDER_DIW_HSTART_FETCH_REFERENCE_HIRES: i32 = 0x83;
// Capture-side twin of `bitplane::COPPER_WAIT_HPOS_FB0`; moved left by 8 colour
// clocks in lockstep with RENDER_DIW_HSTART_FB0.
const RENDER_COPPER_WAIT_HPOS_FB0: u32 = 0x28;
// Agnus DMA scheduling runs four color clocks ahead of Denise's pixel counter.
const DENISE_HPOS_LAG_CCK: u32 = 4;
const BPLCON0_ECSENA: u16 = 1 << 0;
const BPLCON0_SHRES: u16 = 1 << 6;
const BPLCON3_BRDSPRT: u16 = 1 << 1;
const BPLCON3_SPRES_MASK: u16 = 0x00C0;
const BPLCON3_SPRES_LORES: u16 = 0x0040;
const BPLCON3_SPRES_HIRES: u16 = 0x0080;
const BPLCON3_SPRES_SHRES: u16 = 0x00C0;
const BITPLANE_DDF_HARD_START: u16 = 0x0018;
const BITPLANE_DDF_HARD_STOP: u16 = 0x00D8;
const OCS_LORES_BPL_SEQUENCE: [usize; 8] = [8, 4, 6, 2, 7, 3, 5, 1];
const SPRITE_DMA_PAIR_CAPTURE_HPOS: [u32; 4] = [0x018, 0x020, 0x028, 0x030];
const NANOS_PER_SECOND: u128 = 1_000_000_000;
const VIDEO_FETCH_TIMING_SAMPLE_RATE: u128 = 128;
const VIDEO_COLLISION_TIMING_SAMPLE_RATE: u128 = 16;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CapturedBitplaneRow {
    pub nplanes: usize,
    pub words_per_row: usize,
    pub planes: [Vec<u16>; 8],
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
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

#[derive(Clone, Copy, Debug)]
pub struct HeldSpriteLine {
    pub line: CapturedSpriteLine,
    pub vstart: i32,
    pub vstop: i32,
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize, serde::Deserialize)]
struct DisplaySpriteDmaState {
    control: Option<DisplaySpriteControl>,
    next_ptr: Option<u32>,
    terminated: bool,
    data_dma_active: bool,
    last_line: Option<DisplaySpriteLineData>,
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
/// feeds the plan computation. The vpos-dependent gates (vertical display
/// window, DDFSTRT write miss) are evaluated live in `bitplane_slot_active_at`,
/// so DIW registers do not belong here. The effective (write-delayed) DMACON
/// and BPLCON0 values are keyed, so a delay expiring re-keys automatically.
#[derive(Clone, Copy, PartialEq, Eq)]
struct BitplaneSlotKey {
    bplen: bool,
    bplcon0: u16,
    ddfstrt: u16,
    ddfstop: u16,
    fmode: u16,
    harddis: bool,
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

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
struct DisplaySpriteControl {
    vstart: i32,
    vstop: i32,
    hstart: i32,
    hsub_70ns: bool,
    #[serde(skip, default = "unset_sprite_data_vstart")]
    data_vstart: i32,
    data_base: u32,
    next_ptr: u32,
    attached: bool,
}

fn unset_sprite_data_vstart() -> i32 {
    i32::MIN
}

impl DisplaySpriteControl {
    fn effective_data_vstart(self) -> i32 {
        if self.data_vstart == unset_sprite_data_vstart() {
            self.vstart
        } else {
            self.data_vstart
        }
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

// Save-state note: Bus and everything it owns derive serde so a snapshot can
// be taken at an emulated-frame boundary (src/savestate.rs). Host-resource
// fields (open files, audio/serial sinks, wall-clock anchors, memo caches)
// are #[serde(skip)] and reattached by the savestate loader; everything else
// is emulated state and must round-trip. New fields are picked up by the
// derive automatically -- bump savestate::STATE_VERSION when the layout
// changes incompatibly.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Bus {
    pub mem: Memory,
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
    pub rtc: Msm6242Rtc,
    /// Whether the $DC0000 RTC is fitted (machine-profile flag; the base
    /// A600 shipped without one). The CPU memory map consults this before
    /// decoding the RTC range.
    rtc_present: bool,
    /// Gayle gate array (A600/A1200 machine profiles); None on machines
    /// without one, which leaves $DA0000/$DE1000 floating as before.
    pub gayle: Option<Gayle>,
    /// Akiko gate array (CD32 machine profile): ID, the C2P port, and
    /// NVRAM/CD stubs at $B80000.
    pub akiko: Option<crate::akiko::Akiko>,
    /// CDTV DMAC/CD controller (CDTV machine profile): an autoconfig
    /// board with the 6525 TPI and Matshita drive.
    pub cdtv: Option<crate::cdtv::CdtvController>,
    /// A2091 SCSI controller (Zorro II autoconfig board, `[scsi]` config);
    /// the chain maps its window, accesses route here.
    pub a2091: Option<crate::a2091::A2091>,
    /// Emulated-time deadline keeping the front-panel HDD LED lit after the
    /// most recent Gayle IDE activity, so short synchronous transfers stay
    /// visible for a human-perceptible stretch.
    hdd_led_until_cck: u64,
    /// CD32 joypad shift register position for port 2: starts at 8
    /// (Blue), decremented by /FIR1 falling edges the CPU drives while
    /// the pad is in serial mode, 1 returns the pad-present bit, 0
    /// returns zeros. Reset to 8 whenever serial mode is left.
    cd32_pad_shifter: i8,
    /// Previous CPU-driven /FIR1 output level (DDR & PRA & 0x80), for
    /// the shift-clock edge detector.
    cd32_pad_fire_oldstate: u8,
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
    video_pipeline_stats: VideoPipelineStats,

    /// Set by MMIO writes that should end the current CPU slice after
    /// the current instruction has retired.
    pub slice_preempted: bool,

    /// Diagnostic crash context: set when a blit is started whose D
    /// destination lands in the exception-vector / low-memory region, so
    /// the CPU wrapper can dump its instruction history at that moment.
    pub diag_lowmem_blit: bool,

    /// Latched agnus-frame-crossing that has not yet been ORed into
    /// Paula INTREQ. INTREQ is a bit latch, so multiple queued VBlanks
    /// collapse into one pending VERTB bit.
    pub pending_vbi: u32,
    pending_copper_frame_start: Option<u32>,

    /// Paula interrupt bits that were pending when the current
    /// autovector was delivered. CPU INTREQ clears use this to tell
    /// ordinary palette setup apart from beam-timed Copper interrupt
    /// palette changes.
    pub delivered_irq_pending: u16,
    pub(crate) pending_copper_irq_beam: Option<(u32, u32)>,
    pub(crate) delivered_copper_irq_beam: Option<(u32, u32)>,
    coper_cpu_irq_delay_cck: u32,

    /// General 68000 interrupt-recognition latency (COPPERLINE_IRQ_LATENCY_CCK).
    /// Real HW takes ~96-100 cck from a VERTB request to the handler's first
    /// instruction; Copperline's bare model takes ~48 (finish-instruction + the
    /// 44-cycle exception only). The timing-test rows 19 (handler entry) and 22
    /// (raise position) localised the ~50 cck gap to interrupt RECOGNITION
    /// latency, not the raise position. When the setting is non-zero, a newly
    /// raised maskable interrupt is held invisible to the CPU for that many cck
    /// (`irq_latency_mask` = the delayed bits, `irq_latency_cck` = countdown,
    /// `irq_latency_last_pending` = previous pending set for rising-edge detect).
    irq_latency_cck: u32,
    irq_latency_mask: u16,
    irq_latency_last_pending: u16,
    /// Configured recognition latency in cck (from COPPERLINE_IRQ_LATENCY_CCK or the
    /// default); 0 disables the model. A field (not a global) so tests can set
    /// it per-instance -- mechanism tests run with 0 to deliver IRQs immediately.
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
    last_frame_chip_ram: Vec<u8>,
    current_frame_chip_ram_writes: Vec<BeamChipRamWrite>,
    last_frame_chip_ram_writes: Vec<BeamChipRamWrite>,
    current_frame_bitplane_rows: Vec<Option<CapturedBitplaneRow>>,
    last_frame_bitplane_rows: Vec<Option<CapturedBitplaneRow>>,
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
    current_frame_visible_start_vpos: u32,
    last_frame_visible_start_vpos: u32,
    /// Display geometry latched at the frame wrap (standard fixed canvas
    /// vs ECS/AGA VARBEAMEN programmable scan); see `FrameGeometry`.
    current_frame_geometry: FrameGeometry,
    last_frame_geometry: FrameGeometry,
    lazy_collision_vpos: u32,
    lazy_collision_hpos: u32,
    cpu_bus_arbitration_enabled: bool,
    cpu_granted_chip_slots: u64,
    cpu_missed_chip_slots: u64,
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
    /// True for the 68020+: its chip-bus cycle is 3 CPU clocks, not the
    /// 68000's 4 (2 cck) -- so after the granted slot it bills a shorter tail
    /// (write-posting and the faster 020 bus). Derived from the CPU model and
    /// re-set on construction / state load; not serialized.
    #[serde(skip)]
    cpu_short_bus_cycle: bool,
    /// Sub-cck remainder (in CPU clocks) for the 020+ short-bus-cycle tail:
    /// the 1-clock tail at the stock 2-clock ratio is half a cck, accumulated
    /// here so the fractional cck are not lost.
    #[serde(skip)]
    cpu_bus_tail_carry: u32,
    dbg_bpl_cck: Vec<u32>,
    dbg_slotmap: Vec<Vec<u8>>,
    dbg_slotmap_on: bool,
    dbg_slotmap_dumped: bool,
    /// Debugger-window custom-register watch offsets ($000-$1FE, word
    /// aligned), mirrored from the CPU machine's InteractiveBreaks, and
    /// the first pending hit since the debugger last polled. Recorded in
    /// the custom-register write path so every writer (CPU and Copper)
    /// is seen, including writes landing while the CPU is in STOP.
    ui_reg_watches: Vec<u16>,
    #[serde(skip)]
    ui_reg_hit: Option<UiRegHit>,
    blitter_slowdown_cpu_misses: u8,
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
    // Derived from sprite DMA descriptor/control fetches. Kept in the bincode
    // layout for compatibility, then reset after a state load so stale decoded
    // latches are rebuilt from the restored pointer context.
    display_dma_sprite_state: [DisplaySpriteDmaState; 8],
    display_dma_clipped_rows_advanced: bool,
    bitplane_dmacon_delay: Option<BitplaneDmaconDelay>,
    bitplane_bplcon0_delay: Option<BitplaneBplcon0Delay>,
    bitplane_ddfstart_miss: Option<BitplaneDdfStartMiss>,
    /// Memoized bitplane fetch plan for `bitplane_slot_active_at`, keyed on
    /// the registers that feed it. The arbiter asks for the bitplane slot
    /// owner several times per color clock; recomputing the DDF window and
    /// fetch cadence each time was a measurable share of host CPU. A Cell
    /// keeps the `&self` owner-selection call graph intact.
    #[serde(skip)]
    bitplane_slot_plan_cache: std::cell::Cell<Option<(BitplaneSlotKey, Option<BitplaneSlotPlan>)>>,
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

fn beam_write_source_name(source: BeamWriteSource) -> &'static str {
    match source {
        BeamWriteSource::Cpu => "cpu",
        BeamWriteSource::CpuCopperIrq => "cpu_copper_irq",
        BeamWriteSource::Copper => "copper",
    }
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
        debug_assert!(matches!(size, 1 | 2 | 4));
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
    pub sprpos: [u16; 8],
    pub sprctl: [u16; 8],
    pub sprdata: [u16; 8],
    pub sprdatb: [u16; 8],
    pub spr_armed: [bool; 8],
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

const CHIP_BUS_OWNER_NAMES: [&str; 9] = [
    "refresh", "bitplane", "sprite", "disk", "audio", "copper", "blitter", "cpu", "idle",
];

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
    pub power_led_on: bool,
    pub fdd_led_on: bool,
    pub fdd_track: Option<u8>,
    /// HDD activity LED: None on machines without an IDE port (no Gayle),
    /// Some(on) when the machine has one.
    pub hdd_led: Option<bool>,
    /// CD activity LED: None on machines without a CD drive, Some(on)
    /// while the drive is reading data or playing audio.
    pub cd_led: Option<bool>,
    pub output_volume_percent: u8,
}

impl Default for FrontPanelStatus {
    fn default() -> Self {
        Self {
            power_led_on: false,
            fdd_led_on: false,
            fdd_track: None,
            hdd_led: None,
            cd_led: None,
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
    fn probe_timing_sample(probes: &mut u64, rate: u128) -> Option<Instant> {
        let due = probes.is_multiple_of(rate as u64);
        *probes = probes.wrapping_add(1);
        due.then(Instant::now)
    }

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

#[derive(Default, Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct InputState {
    pub mouse_x_port1: u8,
    pub mouse_y_port1: u8,
    pub mouse_x_port2: u8,
    pub mouse_y_port2: u8,
    pub lmb_port1: bool,
    pub lmb_port2: bool,
    pub rmb_port1: bool,
    pub rmb_port2: bool,
    /// A CD32 joypad is plugged into port 2: enables the pad's serial
    /// button protocol (POT1X low selects shift mode, the /FIR1 line
    /// clocks, POT1Y carries the active-low button bits). Red rides the
    /// normal fire line (`lmb_port2`), Blue the button-2 line
    /// (`rmb_port2`); the rest only exist serially.
    pub cd32_pad_port2: bool,
    pub cd32_play_port2: bool,
    pub cd32_rwd_port2: bool,
    pub cd32_ffw_port2: bool,
    pub cd32_green_port2: bool,
    pub cd32_yellow_port2: bool,
    pub mmb_port1: bool,
    pub mmb_port2: bool,
    /// Digital-joystick direction lines per controller port. When the
    /// matching `joystick_portN` flag is set, JOYxDAT reports these as the
    /// Gray-coded direction bits an Amiga game decodes, instead of the mouse
    /// quadrature counters. Fire reuses the FIR0/FIR1 lines (`lmb_portN`),
    /// which CIA-A PRA bits 6/7 already report.
    pub joy_up_port1: bool,
    pub joy_down_port1: bool,
    pub joy_left_port1: bool,
    pub joy_right_port1: bool,
    pub joy_up_port2: bool,
    pub joy_down_port2: bool,
    pub joy_left_port2: bool,
    pub joy_right_port2: bool,
    /// True when a digital joystick (not a mouse) is the active device on the
    /// port, so JOYxDAT returns the direction encoding.
    pub joystick_port1: bool,
    pub joystick_port2: bool,
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
    pub fn add_mouse_delta_port1(&mut self, dx: i32, dy: i32) {
        self.mouse_x_port1 = self.mouse_x_port1.wrapping_add(dx as u8);
        self.mouse_y_port1 = self.mouse_y_port1.wrapping_add(dy as u8);
    }

    pub fn write_joytest(&mut self, val: u16) {
        self.mouse_y_port1 = (val >> 8) as u8;
        self.mouse_x_port1 = val as u8;
        self.mouse_y_port2 = (val >> 8) as u8;
        self.mouse_x_port2 = val as u8;
    }

    pub fn joy0dat(&self) -> u16 {
        if self.joystick_port1 {
            digital_joydat(
                self.joy_up_port1,
                self.joy_down_port1,
                self.joy_left_port1,
                self.joy_right_port1,
            )
        } else {
            mouse_joydat(self.mouse_x_port1, self.mouse_y_port1)
        }
    }

    pub fn joy1dat(&self) -> u16 {
        if self.joystick_port2 {
            digital_joydat(
                self.joy_up_port2,
                self.joy_down_port2,
                self.joy_left_port2,
                self.joy_right_port2,
            )
        } else {
            mouse_joydat(self.mouse_x_port2, self.mouse_y_port2)
        }
    }

    /// Set the port-2 digital joystick state from a host gamepad. Marks the
    /// port as a joystick so JOY1DAT reports directions. Fire (button 1)
    /// drives /FIR1 (CIA-A PRA bit 7) through the shared `lmb_port2` line;
    /// button 2 drives POT1Y, read back through POTGOR (the shared
    /// `rmb_port2` line), matching a real Amiga's second joystick button.
    pub fn set_joystick_port2(
        &mut self,
        up: bool,
        down: bool,
        left: bool,
        right: bool,
        fire: bool,
        button2: bool,
    ) {
        self.joystick_port2 = true;
        self.joy_up_port2 = up;
        self.joy_down_port2 = down;
        self.joy_left_port2 = left;
        self.joy_right_port2 = right;
        self.lmb_port2 = fire;
        self.rmb_port2 = button2;
    }

    /// Set the CD32 joypad's extra buttons (port 2). Red and Blue arrive
    /// through `set_joystick_port2` as fire/button2; these five only
    /// exist in the pad's serial report.
    pub fn set_cd32_buttons_port2(
        &mut self,
        play: bool,
        rwd: bool,
        ffw: bool,
        green: bool,
        yellow: bool,
    ) {
        self.cd32_play_port2 = play;
        self.cd32_rwd_port2 = rwd;
        self.cd32_ffw_port2 = ffw;
        self.cd32_green_port2 = green;
        self.cd32_yellow_port2 = yellow;
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
    pub fn tick_read(&mut self, which: &str, reg: usize) {
        match which {
            "cia_a" => self.cia_a[reg & 0xF] += 1,
            "cia_b" => self.cia_b[reg & 0xF] += 1,
            _ => {}
        }
    }
    pub fn tick_read_custom(&mut self, off: u16) {
        if let Some(count) = self.custom.get_mut((off >> 1) as usize) {
            *count += 1;
        }
    }
    pub fn dump_top(&self, label: &str) {
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

fn audio_min_period_for_video_standard(video_standard: VideoStandard) -> u16 {
    match video_standard {
        VideoStandard::Pal => PAL_AUDIO_MIN_PERIOD_CCK,
        VideoStandard::Ntsc => NTSC_AUDIO_MIN_PERIOD_CCK,
    }
}

fn audio_min_period_for_agnus(agnus: &Agnus) -> u16 {
    let base = u32::from(audio_min_period_for_video_standard(agnus.video_standard()));
    let Some(line_cck) = agnus.programmable_line_cck() else {
        return base as u16;
    };
    let nominal_line_cck = match agnus.video_standard() {
        VideoStandard::Pal => COLORCLOCKS_PER_LINE,
        VideoStandard::Ntsc => NTSC_LONG_COLORCLOCKS_PER_LINE,
    };
    let scaled = base
        .saturating_mul(line_cck.max(1))
        .div_ceil(nominal_line_cck);
    scaled.clamp(1, u32::from(u16::MAX)) as u16
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
            rtc: Msm6242Rtc::default(),
            rtc_present: true,
            gayle: None,
            akiko: None,
            cdtv: None,
            a2091: None,
            hdd_led_until_cck: 0,
            cd32_pad_shifter: 8,
            cd32_pad_fire_oldstate: 0,
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
            delivered_irq_pending: 0,
            pending_copper_irq_beam: None,
            delivered_copper_irq_beam: None,
            coper_cpu_irq_delay_cck: 0,
            irq_latency_cck: 0,
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
            last_frame_chip_ram: Vec::new(),
            current_frame_chip_ram_writes: Vec::new(),
            last_frame_chip_ram_writes: Vec::new(),
            current_frame_bitplane_rows: empty_captured_bitplane_rows(),
            last_frame_bitplane_rows: empty_captured_bitplane_rows(),
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
            current_frame_visible_start_vpos: RENDER_VISIBLE_START_VPOS,
            last_frame_visible_start_vpos: RENDER_VISIBLE_START_VPOS,
            current_frame_geometry: FrameGeometry::standard(RENDER_VISIBLE_START_VPOS, false),
            last_frame_geometry: FrameGeometry::standard(RENDER_VISIBLE_START_VPOS, false),
            lazy_collision_vpos: RENDER_VISIBLE_START_VPOS,
            lazy_collision_hpos: RENDER_COPPER_WAIT_HPOS_FB0,
            cpu_bus_arbitration_enabled: false,
            cpu_clock_carry: 0,
            cpu_clocks_per_cck: 2,
            ext_clock_carry_x100: 0,
            cpu_short_bus_cycle: false,
            cpu_bus_tail_carry: 0,
            cpu_granted_chip_slots: 0,
            cpu_missed_chip_slots: 0,
            dbg_bpl_cck: vec![0; 340],
            dbg_slotmap: Vec::new(),
            dbg_slotmap_on: crate::envcfg::flag("COPPERLINE_DIAG_SLOTMAP"),
            dbg_slotmap_dumped: false,
            ui_reg_watches: Vec::new(),
            ui_reg_hit: None,
            blitter_slowdown_cpu_misses: 0,
            slice_bus_advanced_cck: 0,
            slice_bus_tick: AgnusTick::default(),
            pending_device_cck: 0,
            pending_device_tick: AgnusTick::default(),
            audio_pending_cck: 0,
            last_chip_bus_owner: ChipBusOwner::Idle,
            data_bus: 0,
            device_clock: DeviceClock::default(),
            emulated_cck: 0,
            emulated_frames: 0,
            blitter_trace,
            display_dma_bplpt: [0; 8],
            display_dma_sprpt: [0; 8],
            display_dma_sprite_state: [DisplaySpriteDmaState::default(); 8],
            display_dma_clipped_rows_advanced: false,
            bitplane_dmacon_delay: None,
            bitplane_bplcon0_delay: None,
            bitplane_ddfstart_miss: None,
            bitplane_slot_plan_cache: std::cell::Cell::new(None),
            bus_accounting: BusAccounting::from_env(),
            uhres_dual_warned: false,
            dbg_ext_cck_x100: external_access_cck_x100_setting(),
        };
        bus.configure_chip_dma_masks();
        bus
    }

    pub fn rtc_present(&self) -> bool {
        self.rtc_present
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

    pub fn attach_gayle(&mut self, gayle: Gayle) {
        self.gayle = Some(gayle);
    }

    pub fn attach_akiko(&mut self, akiko: crate::akiko::Akiko) {
        self.akiko = Some(akiko);
    }

    pub fn attach_a2091(&mut self, a2091: crate::a2091::A2091) {
        self.a2091 = Some(a2091);
    }

    pub fn attach_cdtv(&mut self, cdtv: crate::cdtv::CdtvController) {
        self.cdtv = Some(cdtv);
    }

    pub fn set_rtc_present(&mut self, present: bool) {
        self.rtc_present = present;
    }

    pub fn set_video_standard(&mut self, video_standard: VideoStandard) {
        self.agnus.set_video_standard(video_standard);
        self.refresh_paula_audio_min_period();
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
        self.refresh_paula_audio_min_period();
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
        self.agnus.reset_copcon();
        self.floppy.reset_external_drives();
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

    fn refresh_paula_audio_min_period(&mut self) {
        self.paula
            .set_audio_min_period_cck(audio_min_period_for_agnus(&self.agnus));
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

    pub fn reset_for_keyboard_reset(&mut self) {
        let video_standard = self.agnus.video_standard();
        let agnus_revision = self.agnus.revision();
        self.cia_a = Cia::new(Which::A);
        self.cia_b = Cia::new(Which::B);
        self.paula.reset_registers();
        self.agnus = Agnus::with_video_standard_and_revision(video_standard, agnus_revision);
        self.refresh_paula_audio_min_period();
        self.copper = Copper::new();
        self.denise = Denise::new();
        self.blitter = Blitter::new();
        self.configure_chip_dma_masks();
        self.rtc = Msm6242Rtc::default();
        if let Some(gayle) = self.gayle.as_mut() {
            gayle.reset();
        }
        if let Some(akiko) = self.akiko.as_mut() {
            akiko.reset();
        }
        if let Some(cdtv) = self.cdtv.as_mut() {
            cdtv.reset();
        }
        if let Some(a2091) = self.a2091.as_mut() {
            a2091.reset();
        }
        self.hdd_led_until_cck = 0;
        self.overlay_disable_pending = false;
        // The MCU restarts its power-up flow; physically held keys stay
        // held and are reported in the upcoming $FD/$FE stream.
        self.keyboard.begin_power_up();
        self.keyboard_system_reset_pending = false;
        self.input = InputState::default();
        self.video_pipeline_stats = VideoPipelineStats::default();
        self.slice_preempted = false;
        self.pending_vbi = 0;
        self.pending_copper_frame_start = None;
        self.delivered_irq_pending = 0;
        self.pending_copper_irq_beam = None;
        self.delivered_copper_irq_beam = None;
        self.coper_cpu_irq_delay_cck = 0;
        self.irq_latency_cck = 0;
        self.irq_latency_mask = 0;
        self.irq_latency_last_pending = 0;
        self.beam_top_palette = Palette::new();
        self.beam_bottom_palette = Palette::new();
        self.beam_bottom_palette_valid = false;
        self.cpu_palette_target = CpuPaletteTarget::Top;
        self.cpu_palette_target_writes = 0;
        self.cpu_palette_target_beam = None;
        self.current_frame_render_base = RenderRegisterSnapshot::default();
        self.last_frame_render_base = None;
        self.current_frame_render_events.clear();
        self.current_frame_collision_events.clear();
        self.current_frame_collision_control_events.clear();
        self.current_frame_collision_bpldat_events.clear();
        self.current_frame_collision_sprite_events.clear();
        self.current_frame_collision_control_index = None;
        self.current_frame_collision_bpldat_index = None;
        self.current_frame_collision_sprite_index = None;
        self.current_frame_collision_may_have_dual_playfield = false;
        self.last_frame_render_events.clear();
        self.beam_bottom_palette_events.clear();
        self.pending_beam_bottom_palette_events.clear();
        self.last_frame_beam_bottom_palette_events.clear();
        self.current_frame_beam_top_palette = self.beam_top_palette;
        self.last_frame_beam_top_palette = Palette::new();
        self.last_frame_beam_top_palette_end = Palette::new();
        self.last_frame_beam_bottom_palette = Palette::new();
        self.last_frame_beam_bottom_palette_valid = false;
        self.current_frame_chip_ram.clear();
        self.current_frame_chip_ram
            .extend_from_slice(&self.mem.chip_ram);
        self.last_frame_chip_ram.clear();
        self.current_frame_chip_ram_writes.clear();
        self.last_frame_chip_ram_writes.clear();
        self.current_frame_bitplane_rows = empty_captured_bitplane_rows();
        self.last_frame_bitplane_rows = empty_captured_bitplane_rows();
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
        self.current_frame_display_snapshot_taken = false;
        self.current_frame_visible_start_vpos = RENDER_VISIBLE_START_VPOS;
        self.last_frame_visible_start_vpos = RENDER_VISIBLE_START_VPOS;
        self.current_frame_geometry = FrameGeometry::standard(RENDER_VISIBLE_START_VPOS, false);
        self.last_frame_geometry = FrameGeometry::standard(RENDER_VISIBLE_START_VPOS, false);
        self.lazy_collision_vpos = RENDER_VISIBLE_START_VPOS;
        self.lazy_collision_hpos = RENDER_COPPER_WAIT_HPOS_FB0;
        self.cpu_bus_arbitration_enabled = false;
        self.blitter_slowdown_cpu_misses = 0;
        self.slice_bus_advanced_cck = 0;
        self.slice_bus_tick = AgnusTick::default();
        self.last_chip_bus_owner = ChipBusOwner::Idle;
        self.device_clock.reset();
        self.emulated_cck = 0;
        self.emulated_frames = 0;
        self.display_dma_bplpt = [0; 8];
        self.display_dma_sprpt = [0; 8];
        self.display_dma_sprite_state = [DisplaySpriteDmaState::default(); 8];
        self.display_dma_clipped_rows_advanced = false;
        self.bitplane_dmacon_delay = None;
        self.bitplane_bplcon0_delay = None;
        self.bitplane_ddfstart_miss = None;
        self.mem.overlay = true;
        self.floppy.reset_external_drives();
        self.floppy.write_prb(0xFF);
    }

    /// Full cold boot. Clears RAM to its power-on (zeroed) state and then
    /// runs the same chip/CIA reset as a keyboard reset. Unlike
    /// Ctrl-Amiga-Amiga, RAM is not preserved, so the machine comes up as
    /// if it had been power-cycled. Clear RAM first so the keyboard-reset
    /// path snapshots the zeroed chip RAM for the renderer.
    pub fn power_on_reset(&mut self) {
        self.mem.power_on_reset();
        self.reset_for_keyboard_reset();
    }

    pub fn front_panel_status(&self) -> FrontPanelStatus {
        let pra = self.cia_a.peek_register(REG_PRA);
        FrontPanelStatus {
            power_led_on: pra & 0x02 == 0,
            fdd_led_on: self.floppy.activity_led_on(),
            fdd_track: self.floppy.selected_track(),
            hdd_led: (self.gayle.is_some() || self.a2091.is_some())
                .then_some(self.emulated_cck < self.hdd_led_until_cck),
            cd_led: self
                .cdtv
                .as_ref()
                .map(|cdtv| cdtv.activity_led_on())
                .or_else(|| self.akiko.as_ref().map(|akiko| akiko.activity_led_on())),
            output_volume_percent: self.paula.output_volume_percent(),
        }
    }

    /// Whether the machine has a CD drive (CDTV DMAC or CD32 Akiko).
    pub fn cd_drive_present(&self) -> bool {
        self.cdtv.is_some() || self.akiko.is_some()
    }

    /// Whether a disc is mounted (or, on CDTV, waiting in the tray).
    pub fn cd_disc_inserted(&self) -> bool {
        self.cdtv.as_ref().is_some_and(|cdtv| cdtv.has_disc())
            || self.akiko.as_ref().is_some_and(|akiko| akiko.has_disc())
    }

    /// Runtime disc insert with media-change notification. On CDTV the
    /// disc lands after a short tray delay (the same media-change STCH
    /// path as `[cd] insert_delay`); Akiko mounts immediately and
    /// volunteers a media-status packet.
    pub fn cd_insert_disc(&mut self, image: crate::cdrom::CdImage) {
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
        }
    }

    /// Runtime disc eject with media-change notification.
    pub fn cd_eject_disc(&mut self) {
        if let Some(cdtv) = self.cdtv.as_mut() {
            cdtv.eject_disc();
        } else if let Some(akiko) = self.akiko.as_mut() {
            akiko.eject_disc();
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
    /// audio/serial sinks and the open blitter trace file. The realtime
    /// device-clock anchor needs no carry-over -- it deserializes as None
    /// and `realtime_cck_due` re-anchors to the host clock on first use.
    pub(crate) fn adopt_host_resources(&mut self, live: &mut Bus) {
        std::mem::swap(&mut self.paula.serial, &mut live.paula.serial);
        std::mem::swap(&mut self.paula.audio, &mut live.paula.audio);
        self.blitter_trace = live.blitter_trace.take();
    }

    pub(crate) fn reset_transient_video_after_state_load(&mut self) {
        self.last_frame_render_base = None;
        self.last_frame_render_events.clear();
        self.last_frame_beam_bottom_palette_events.clear();
        self.current_frame_chip_ram.clear();
        self.current_frame_chip_ram
            .extend_from_slice(&self.mem.chip_ram);
        self.last_frame_chip_ram.clear();
        self.current_frame_chip_ram_writes.clear();
        self.last_frame_chip_ram_writes.clear();
        self.current_frame_bitplane_rows = empty_captured_bitplane_rows();
        self.last_frame_bitplane_rows = empty_captured_bitplane_rows();
        self.current_frame_sprite_lines.clear();
        self.last_frame_sprite_lines.clear();
        clear_captured_sprite_lines_by_y(&mut self.current_frame_sprite_lines_by_y);
        self.current_frame_sprite_collision_sources = empty_sprite_collision_sources();
        self.current_frame_held_sprites = [None; 8];
        self.last_frame_held_sprites = [None; 8];
        self.current_frame_sprite_display_enable_x_by_y = empty_sprite_display_enable_x_by_y();
        self.last_frame_sprite_display_enable_x_by_y = empty_sprite_display_enable_x_by_y();
        self.current_frame_sprite_dma_observed = false;
        self.last_frame_sprite_dma_observed = false;
        self.display_dma_sprite_state = [DisplaySpriteDmaState::default(); 8];
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

    pub fn live_audio_output_lead_seconds(&self) -> f64 {
        self.paula.live_audio_output_lead_seconds()
    }

    pub fn live_audio_status(&self) -> crate::audio::AudioRuntimeStatus {
        self.paula.live_audio_status()
    }

    pub fn set_live_audio_suspended(&mut self, suspended: bool) {
        self.paula.set_live_audio_suspended(suspended);
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

    pub(crate) fn record_video_render_frame(&mut self, timing: VideoRenderFrameTiming) {
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

    fn record_bitplane_fetch_timing(
        &mut self,
        slots: usize,
        rows_started: usize,
        rows_completed: usize,
        elapsed: Option<(Duration, u128)>,
    ) {
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

    fn record_sprite_fetch_timing(
        &mut self,
        pair_slots: usize,
        lines: usize,
        elapsed: Option<(Duration, u128)>,
    ) {
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

    fn record_live_collision_timing(
        &mut self,
        pixels: u64,
        control_segments: usize,
        full_line_scan: bool,
        elapsed: Option<(Duration, u128)>,
    ) {
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

    /// Select the chip-bus cycle length: the 68020+ completes a word access in
    /// 3 CPU clocks where the 68000 takes 4, so its post-grant tail is shorter
    /// (write-posting; faster reads). Derived from the CPU model.
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
    }

    /// Advance the chipset for a CPU access to a motherboard peripheral (CIA,
    /// RTC, autoconfig, undecoded space). These stay on the slow motherboard
    /// bus no matter how fast the CPU is clocked, so bill the stock 68000
    /// figure of 2 cck per word regardless of the configured CPU speed.
    pub fn cpu_slow_external_access(&mut self, words: u32) {
        if !self.cpu_bus_arbitration_enabled || words == 0 {
            return;
        }
        let cck = words * 2;
        let tick = self.advance_chipset(cck);
        self.record_slice_bus_advance(cck, tick);
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
        for _ in 0..slots {
            self.flush_audio_before_audio_dma_slot();
            while !self.cpu_can_use_current_slot() {
                let (cck, tick) = self.advance_one_chip_bus_quantum(None);
                self.note_cpu_missed_chip_bus_cycle();
                self.record_slice_bus_advance(cck, tick);
                self.flush_audio_before_audio_dma_slot();
            }
            let (cck, tick) = self.advance_one_chip_bus_quantum(Some(ChipBusOwner::Cpu));
            self.note_cpu_granted_chip_bus_cycle();
            self.record_slice_bus_advance(cck, tick);
            // After the granted slot (one cck), the CPU's bus cycle runs out
            // its remaining clocks with the chip bus free for DMA. The 68000's
            // 4-clock cycle leaves one whole cck (2 clocks at the stock ratio);
            // the 68020+'s 3-clock cycle leaves only one clock -- half a cck at
            // the stock ratio, and none at all once a slot is >= 3 clocks
            // (14 MHz), which is the write-posting / faster-020-bus effect.
            // Bill the 020 tail in CPU clocks through a carry so the fractional
            // cck are not lost.
            if self.cpu_short_bus_cycle {
                self.cpu_bus_tail_carry += 3u32.saturating_sub(self.cpu_clocks_per_cck);
                while self.cpu_bus_tail_carry >= self.cpu_clocks_per_cck {
                    self.cpu_bus_tail_carry -= self.cpu_clocks_per_cck;
                    let (cck, tick) = self.advance_one_chip_bus_quantum(None);
                    self.record_slice_bus_advance(cck, tick);
                }
            } else {
                let (cck, tick) = self.advance_one_chip_bus_quantum(None);
                self.record_slice_bus_advance(cck, tick);
            }
        }
        if self.cpu_short_bus_cycle && !wide_bus && matches!(kind, CpuBusAccessKind::Read) {
            self.bill_020_read_data_wait();
        }
    }

    fn bill_020_read_data_wait(&mut self) {
        let (cck, tick) = self.advance_one_chip_bus_quantum(None);
        self.record_slice_bus_advance(cck, tick);
    }

    fn note_cpu_missed_chip_bus_cycle(&mut self) {
        self.cpu_missed_chip_slots = self.cpu_missed_chip_slots.wrapping_add(1);
        if self.blitter_slowdown_counter_enabled() {
            self.blitter_slowdown_cpu_misses = self
                .blitter_slowdown_cpu_misses
                .saturating_add(1)
                .min(exp_miss_limit());
        } else {
            self.blitter_slowdown_cpu_misses = 0;
        }
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

    pub fn advance_devices(&mut self, cck: u32) -> AgnusTick {
        // Apply any color clocks deferred by the CPU access path before ticking
        // this (idle/stopped-CPU or test-driven) span, so device time stays
        // ordered, then tick this span directly.
        self.flush_timed_devices();
        let tick = self.advance_chipset(cck);
        self.tick_timed_devices(cck, tick);
        tick
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
        self.paula.next_pot_event_cck()
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
        // Hold a newly-raised interrupt invisible during its recognition latency.
        if self.irq_latency_cck != 0 {
            visible &= !self.irq_latency_mask;
        }
        visible
    }

    /// Detect a newly-raised maskable interrupt and arm its recognition-latency
    /// countdown. Called per device tick, after intreq/intena have settled.
    fn arm_irq_recognition_latency(&mut self) {
        let setting = self.irq_latency_setting;
        if setting == 0 {
            return;
        }
        let pending = self.current_enabled_irq_sources();
        let newly = pending & !self.irq_latency_last_pending;
        if newly != 0 {
            self.irq_latency_mask |= newly;
            self.irq_latency_cck = setting;
        }
        // Drop any bits that are no longer pending (acked while still delayed).
        self.irq_latency_mask &= pending;
        self.irq_latency_last_pending = pending;
    }

    fn current_enabled_irq_sources(&self) -> u16 {
        if self.paula.intena & crate::chipset::paula::INT_MASTER != 0 {
            self.paula.intena & self.paula.intreq & IRQ_SOURCE_BITS
        } else {
            0
        }
    }

    /// CPU INTENA/INTREQ writes change Paula's mask/latch state, but they are
    /// not new asynchronous interrupt-source edges. Keep the delayed-bit state
    /// coherent without hiding an already-latched source again; real recognition
    /// latency is armed where Paula/CIA/blitter sources assert.
    fn note_irq_latches_changed(&mut self) {
        let pending = self.current_enabled_irq_sources();
        self.irq_latency_mask &= pending;
        self.irq_latency_last_pending = pending;
    }

    fn note_irq_source_asserted(&mut self) {
        self.arm_irq_recognition_latency();
    }

    pub fn next_frame_event_cck(&self) -> u32 {
        self.agnus.cck_until_next_frame().max(1)
    }

    pub fn next_display_start_event_cck(&self) -> Option<u32> {
        let visible_start = self.visible_start_vpos_for_current_control();
        if self.current_frame_display_snapshot_taken || self.agnus.vpos >= visible_start {
            return None;
        }
        self.agnus.cck_until_line_start(visible_start)
    }

    fn visible_start_vpos_for_current_control(&self) -> u32 {
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
    /// wrap, like the interlace long-field flag. Standard frames track the
    /// DIW-refined visible start via `refresh_frame_geometry_visible_start`.
    fn compute_frame_geometry(&self) -> FrameGeometry {
        let lace = self.denise.bplcon0 & 0x0004 != 0;
        if let Some((start, lines)) = self.agnus.programmable_visible_window() {
            return FrameGeometry {
                programmable: true,
                visible_start_vpos: start,
                visible_lines: (lines as usize).clamp(1, MAX_VISIBLE_LINES),
                line_cck: self.agnus.programmable_line_cck().unwrap_or(227),
                lace,
            };
        }
        FrameGeometry::standard(self.current_frame_visible_start_vpos, lace)
    }

    /// Keep a standard frame's geometry in step with the lazily refined
    /// DIW visible start (programmable geometry derives its window from
    /// the beam registers instead and is left alone).
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
        let wait = self.copper.waiting()?;
        let position_cck = self.cck_until_copper_wait_position(wait)?;
        if position_cck == 0 && wait.blitter_wait_enabled() && self.blitter.busy {
            return self.next_blitter_completion_cck();
        }
        Some(position_cck)
    }

    fn tick_timed_devices(&mut self, cck: u32, agnus_tick: AgnusTick) {
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

        // Akiko: advance the CD controller (sector DMA pacing, command
        // and response rings) and level-feed its INT2 line like Gayle's.
        if let Some(akiko) = self.akiko.as_mut() {
            akiko.tick(cck, &mut self.mem.chip_ram, self.paula.cd_audio_mut());
            if akiko.int2_line() {
                self.paula.intreq |= INT_PORTS;
            }
        }

        if let Some(cdtv) = self.cdtv.as_mut() {
            cdtv.tick(cck, self.paula.cd_audio_mut());
            if cdtv.int2_line() {
                self.paula.intreq |= INT_PORTS;
            }
        }

        // A2091 SCSI: deliver delayed WD33C93 interrupts (and any DMA that
        // became ready) and level-feed its INT2 line like Gayle's.
        if let Some(a2091) = self.a2091.as_mut() {
            a2091.tick(cck, &mut self.mem);
            if a2091.int2_line() {
                self.paula.intreq |= INT_PORTS;
            }
        }

        let ticks = self.device_clock.cia_ticks_for_cck(cck);
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

        for _ in 0..agnus_tick.new_frames {
            if self.cia_a.tick_tod() {
                self.paula.intreq |= INT_PORTS;
            }
        }
        for _ in 0..agnus_tick.new_lines {
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

        self.paula.intreq |= self.paula.tick_serial(cck);
        self.paula.tick_pots(cck);
        let dmacon = self.agnus.dmacon;
        self.flush_audio();
        // The floppy mechanism is quiescent for almost all of normal running
        // (no DMA, motor off, no drive selected). Skip the whole block -- an
        // `is_idle()` recompute plus the DSKBLK/DSKSYNC/index-pulse polling and
        // drive-sound feed -- on a single cached bool while that holds. The
        // cache is cleared by every activation write and recomputed in
        // `floppy.tick`, so a newly active drive is serviced from the next
        // access. Drive sounds need no feed once idle: the spin/read levels are
        // already zeroed and the tails decay in Paula's mixer.
        if !self.floppy.is_idle_cached() {
            self.floppy.set_adkcon(self.paula.adkcon);
            if self.floppy.tick(cck, dmacon, &mut self.mem.chip_ram) {
                self.paula.intreq |= INT_DSKBLK;
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
            self.feed_drive_sounds(dmacon);
        }
        self.refresh_cia_irq_lines();
        self.flush_pending_vbi();
        self.arm_irq_recognition_latency();
    }

    /// Forward floppy mechanism activity (head steps, motor spin
    /// levels, read/write DMA) to the synthesized drive sound effects
    /// mixed into Paula's host output. State updates here trail the
    /// already-flushed audio batch by at most one chipset tick, well
    /// under a single host sample.
    fn feed_drive_sounds(&mut self, dmacon: u16) {
        let steps = self.floppy.take_sound_steps();
        let spins = self.floppy.motor_spin_levels();
        let reading = self.floppy.dma_active(dmacon);
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
        sounds.set_read_active(reading);
    }

    fn refresh_cia_irq_lines(&mut self) {
        if self.cia_a.irq_line_asserted() {
            self.paula.intreq |= INT_PORTS;
        }
        if self.cia_b.irq_line_asserted() {
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

    pub fn frame_visible_start_vpos(&self) -> u32 {
        if self.last_frame_render_base.is_some() {
            self.last_frame_visible_start_vpos
        } else {
            self.current_frame_visible_start_vpos
        }
    }

    /// Display geometry of the frame the renderer is about to draw
    /// (the completed frame once one exists, like `frame_render_base`).
    pub fn frame_geometry(&self) -> FrameGeometry {
        if self.last_frame_render_base.is_some() {
            self.last_frame_geometry
        } else {
            self.current_frame_geometry
        }
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

    pub fn cia_a_read(&mut self, addr: u64, size: usize) -> u64 {
        self.sync_realtime_devices();
        let reg = reg_from_addr(addr);
        let mut v = self.cia_a.read(reg);
        if reg == REG_PRA {
            // PRA bits 6 and 7 (/FIR0, /FIR1) report the live port-1/port-2
            // primary button: left-mouse button or joystick fire (they share
            // the FIR line). Active-low: 0 = pressed, 1 = released.
            if self.input.lmb_port1 {
                v &= !0x40;
            } else {
                v |= 0x40;
            }
            if self.input.lmb_port2 {
                v &= !0x80;
            } else {
                v |= 0x80;
            }
            // PRA bits 2-5 are the selected floppy drive's active-low
            // status lines: /CHNG, /WPRO, /TK0, /RDY.
            v = (v & !0x3C) | self.floppy.cia_a_status_bits();
        }
        trace!("cia_a R reg={:X} sz={} val={:02X}", reg, size, v);
        self.poll_stats.tick_read("cia_a", reg);
        v as u64
    }

    pub fn cia_a_write(&mut self, addr: u64, size: usize, val: u64) -> CiaSideEffect {
        self.sync_realtime_devices();
        let byte = (val & 0xFF) as u8;
        let reg = reg_from_addr(addr);
        trace!("cia_a W reg={:X} sz={} val={:02X}", reg, size, byte);
        let eff = self.cia_a.write(reg, byte);
        if self.cia_a.irq_line_asserted() {
            self.paula.intreq |= INT_PORTS;
        }
        match eff {
            CiaSideEffect::KeyboardHandshakeStart => self.keyboard.amiga_kdat_edge(true),
            CiaSideEffect::KeyboardHandshakeEnd => self.keyboard.amiga_kdat_edge(false),
            _ => {}
        }
        if reg == REG_PRA || reg == REG_DDRA {
            // CIA-A PRA bit 1 is /LED. On post-A1000 Amigas this line
            // also controls the optional analogue audio low-pass filter:
            // low = LED bright + filter enabled, high = LED dim/off +
            // filter bypassed.
            let pra = self.cia_a.read(REG_PRA);
            self.paula.set_led_filter_enabled(pra & 0x02 == 0);
            self.cd32_pad_cia_clock();
        }
        eff
    }

    /// Whether the port-2 CD32 pad is in serial (shift) mode: the CPU
    /// drives POT1X (P5) low through POTGO (OUTRX set, DATRX clear).
    fn cd32_pad_serial_mode(&self) -> bool {
        self.input.cd32_pad_port2 && self.paula.potgo & 0x3000 == 0x2000
    }

    /// CD32 pad shift clock: with the pad in serial mode, a falling edge
    /// the CPU drives on /FIR1 (CIA-A PRA bit 7 with DDR output) steps
    /// the shift register. Outside serial mode the register reloads.
    fn cd32_pad_cia_clock(&mut self) {
        if !self.input.cd32_pad_port2 {
            return;
        }
        const FIR1: u8 = 0x80;
        let pra = self.cia_a.peek_register(REG_PRA);
        let ddra = self.cia_a.peek_register(REG_DDRA);
        if self.cd32_pad_serial_mode() {
            if ddra & FIR1 != 0 && (pra & FIR1) != self.cd32_pad_fire_oldstate && pra & FIR1 == 0 {
                self.cd32_pad_shifter = (self.cd32_pad_shifter - 1).max(0);
            }
        } else {
            self.cd32_pad_shifter = 8;
        }
        self.cd32_pad_fire_oldstate = ddra & pra & FIR1;
    }

    /// The serial button bit for the current shift position, active-low
    /// on POT1Y. Order from 8 down: Blue, Red, Yellow, Green, FFW, RWD,
    /// Play; 1 is the always-high pad-present bit; 0 reads zero.
    fn cd32_pad_serial_bit(&self) -> bool {
        let input = &self.input;
        match self.cd32_pad_shifter {
            0 => false,
            1 => true,
            2 => !input.cd32_play_port2,
            3 => !input.cd32_rwd_port2,
            4 => !input.cd32_ffw_port2,
            5 => !input.cd32_green_port2,
            6 => !input.cd32_yellow_port2,
            7 => !input.lmb_port2, // Red
            _ => !input.rmb_port2, // Blue
        }
    }

    pub fn cia_b_read(&mut self, addr: u64, size: usize) -> u64 {
        self.sync_realtime_devices();
        let reg = reg_from_addr(addr);
        let v = self.cia_b.read(reg);
        trace!("cia_b R reg={:X} sz={} val={:02X}", reg, size, v);
        self.poll_stats.tick_read("cia_b", reg);
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
        trace!("cia_b W reg={:X} sz={} val={:02X}", reg, size, byte);
        let anchor_tod = reg == REG_TODLO && !self.cia_b.tod_writes_alarm();
        let eff = self.cia_b.write(reg, byte);
        if self.cia_b.irq_line_asserted() {
            self.paula.intreq |= INT_EXTER;
        }
        if anchor_tod {
            self.cia_b.anchor_tod_to_frame(0);
        }
        if reg == REG_PRB || reg == REG_DDRB {
            let prb = self.cia_b.read(REG_PRB);
            self.floppy.write_prb(prb);
        }
        eff
    }

    // -----------------------------------------------------------------
    // Custom chip ($DFF000) dispatch
    // -----------------------------------------------------------------

    pub fn custom_read(&mut self, addr: u64, size: usize) -> u64 {
        self.grant_cpu_bus_access(size, CpuBusAccessKind::Custom);
        if self.cpu_short_bus_cycle && !self.aga_enabled() {
            self.bill_020_read_data_wait();
        }
        // Read-only custom registers (INTREQR, DSKBYTR, SERDATR, POTxDAT, ...)
        // reflect timed-device state, so apply the deferred device clocks before
        // reading.
        self.flush_timed_devices();
        let off = (addr & 0xFFF) as u16;
        self.poll_stats.tick_read_custom(off & 0xFFE);
        match size {
            1 => {
                let val = self.read_custom_word(off & 0xFFE);
                trace!("custom R8  off={:03X} val_word={:04X}", off, val);
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
                v
            }
            _ => {
                let val = self.read_custom_word(off);
                trace!("custom R16 off={:03X} val={:04X}", off, val);
                val as u64
            }
        }
    }

    /// Returns true if the write set a new INTREQ bit and the caller
    /// should preempt the slice so the freshly-asserted IRQ can be
    /// delivered before agnus has a chance to OR in VERTB.
    pub fn custom_write(&mut self, addr: u64, size: usize, val: u64) -> bool {
        self.grant_cpu_bus_access(size, CpuBusAccessKind::Custom);
        // Apply deferred device clocks before the write lands: registers such as
        // INTREQ/INTENA/ADKCON/DSKLEN/AUDxxx/SERDAT change timed-device state, so
        // the device must first be advanced to this color clock (e.g. so a
        // pending IRQ is latched before an INTREQ clear).
        self.flush_timed_devices();
        let off = (addr & 0xFFF) as u16;
        match size {
            1 => {
                let b = (val & 0xFF) as u16;
                let word_off = off & 0xFFE;
                let word = if let Some(cur) = self.custom_byte_write_latch(word_off) {
                    if off & 1 == 0 {
                        (cur & 0x00FF) | (b << 8)
                    } else {
                        (cur & 0xFF00) | b
                    }
                } else {
                    // Some write-only command/strobe registers do not
                    // have a meaningful word latch in this model. Keep
                    // the old mirrored-byte behavior for those rare
                    // byte writes rather than inventing state.
                    (b << 8) | b
                };
                trace!("custom W8  off={:03X} val={:02X}", off, b);
                self.write_custom_word_from(word_off, word, BeamWriteSource::Cpu)
            }
            4 => {
                // MOVE.L to $DFFxxx writes two consecutive register
                // words. DiagROM relies on this for `move.l #copper,
                // COP1LCH` setting both halves of the pointer in one
                // instruction.
                let hi = ((val >> 16) & 0xFFFF) as u16;
                let lo = (val & 0xFFFF) as u16;
                trace!("custom W32 off={:03X} val={:08X}", off, val);
                let p1 = self.write_custom_word_from(off, hi, BeamWriteSource::Cpu);
                let p2 = self.write_custom_word_from(off.wrapping_add(2), lo, BeamWriteSource::Cpu);
                p1 || p2
            }
            _ => {
                let word = (val & 0xFFFF) as u16;
                trace!("custom W16 off={:03X} val={:04X}", off, word);
                self.write_custom_word_from(off, word, BeamWriteSource::Cpu)
            }
        }
    }

    fn read_custom_word(&mut self, off: u16) -> u16 {
        match off & 0xFFE {
            0x002 => {
                // DMACONR. Bit 14 = BBUSY (blitter busy), bit 13 = BZERO
                // (last blit's D was all zero).
                let mut r = self.agnus.dmacon & 0x07FF;
                if self.blitter.busy {
                    r |= 1 << 14;
                }
                if self.blitter.bzero {
                    r |= 1 << 13;
                }
                self.trace_dmaconr_read(r);
                r
            }
            0x004 => self.agnus.read_vposr(), // VPOSR
            0x006 => self.agnus.read_vhposr(),
            0x00A => self.input.joy0dat(), // JOY0DAT (mouse port 1 counters)
            0x00C => self.input.joy1dat(), // JOY1DAT (mouse port 2 counters)
            0x00E => {
                self.accumulate_live_collisions_until_current_beam();
                self.denise.read_clxdat()
            } // CLXDAT
            0x008 => self.floppy.read_dskdatr(), // DSKDATR
            0x010 => self.paula.adkcon,    // ADKCONR
            0x012 => self.paula.read_potdat(0), // POT0DAT
            0x014 => self.paula.read_potdat(1), // POT1DAT
            0x016 => {
                // POTGOR
                let mut v = self.paula.read_potgor(self.pot_pins());
                if self.cd32_pad_serial_mode() {
                    // P5 reads low (driven), P9 carries the serial bit.
                    v &= !(1 << 12);
                    if self.cd32_pad_serial_bit() && !self.input.rmb_port2 {
                        v |= 1 << 14;
                    } else {
                        v &= !(1 << 14);
                    }
                } else if self.input.cd32_pad_port2 {
                    // Leaving serial mode reloads the shift register.
                    // (Interior mutability not needed: POTGOR reads come
                    // through custom_read with &mut self.)
                    self.cd32_pad_shifter = 8;
                }
                v
            }
            0x018 => self.paula.read_serdatr(), // SERDATR
            0x01A => {
                let r = self
                    .floppy
                    .read_dskbytr(self.agnus.dmacon, self.paula.adkcon);
                if self.floppy.take_sync_irq() {
                    self.paula.intreq |= INT_DSKSYNC;
                }
                r
            }
            0x01C => self.paula.intena, // INTENAR
            0x01E => {
                self.flush_audio();
                self.paula.intreq // INTREQR
            }
            off @ 0x0A0..=0x0DF => {
                self.flush_audio();
                self.paula.read_audio_reg(off - 0x0A0)
            }
            // DENISEID: ECS Denise (8373) drives 0xFFFC; software detects ECS
            // via the low byte (0xFC). OCS Denise (8362) has no such register,
            // so it falls through to the undriven-bus fallback below.
            // HHPOSR (ECS Agnus): UHRES dual-mode H counter readback. The
            // counter is not emulated, so this reads the HHPOSW latch.
            0x1DA if self.agnus.revision().is_ecs() => self.agnus.hhpos(),
            0x07C if self.denise_revision.id().is_some() => self.denise_revision.id().unwrap_or(0),
            _ => {
                // Real write-only custom registers leave the CPU reading an
                // undriven custom bus. Copperline does not model the previous
                // bus owner yet, so write-only and unmapped custom reads use
                // deterministic zero. Byte writes still consult private
                // latches through `custom_byte_write_latch`.
                0
            }
        }
    }

    fn pot_pins(&self) -> PotPins {
        PotPins {
            left_x_released: !self.input.mmb_port1,
            left_y_released: !self.input.rmb_port1,
            right_x_released: !self.input.mmb_port2,
            right_y_released: !self.input.rmb_port2,
        }
    }

    fn custom_byte_write_latch(&self, off: u16) -> Option<u16> {
        match off & 0xFFE {
            0x02E => Some(self.agnus.copcon),
            0x032 => Some(self.paula.serper),
            0x034 => Some(self.paula.potgo),
            0x098 => Some(self.denise.clxcon),
            0x040 => Some(self.blitter.bltcon0),
            0x042 => Some(self.blitter.bltcon1),
            0x044 => Some(self.blitter.bltafwm),
            0x046 => Some(self.blitter.bltalwm),
            0x05A if self.blitter_ecs_registers_enabled() => Some(self.blitter.bltcon0 & 0x00FF),
            0x05C if self.blitter_ecs_registers_enabled() => Some(self.blitter.bltsizv),
            0x048 => Some(((self.blitter.bltcpt >> 16) & 0x001F) as u16),
            0x04A => Some((self.blitter.bltcpt & 0xFFFE) as u16),
            0x04C => Some(((self.blitter.bltbpt >> 16) & 0x001F) as u16),
            0x04E => Some((self.blitter.bltbpt & 0xFFFE) as u16),
            0x050 => Some(((self.blitter.bltapt >> 16) & 0x001F) as u16),
            0x052 => Some((self.blitter.bltapt & 0xFFFE) as u16),
            0x054 => Some(((self.blitter.bltdpt >> 16) & 0x001F) as u16),
            0x056 => Some((self.blitter.bltdpt & 0xFFFE) as u16),
            0x060 => Some(self.blitter.bltcmod as u16),
            0x062 => Some(self.blitter.bltbmod as u16),
            0x064 => Some(self.blitter.bltamod as u16),
            0x066 => Some(self.blitter.bltdmod as u16),
            0x070 => Some(self.blitter.bltcdat),
            0x072 => Some(self.blitter.bltbdat),
            0x074 => Some(self.blitter.bltadat),
            // Audio registers (AUDxLC/LEN/PER/VOL/DAT) deliberately fall
            // through to the mirrored-byte path below. On a real 68000 a
            // byte write drives the value onto both data-bus halves, and
            // Paula latches the full 16-bit word, so `move.b #v,AUDxVOL`
            // (an even address) lands the value in the volume bits 0..6
            // exactly as `move.w` would. Some music players use this to
            // set a channel's volume via the "set volume" effect (e.g.
            // Magic Pockets' title tune drops its echo voice to a low
            // volume this way); merging with the latched low byte would
            // leave the volume at its previous full value.
            0x08E => Some(self.denise.diwstrt),
            0x090 => Some(self.denise.diwstop),
            0x1E4 => self.denise_ecs_registers().then_some(self.denise.diwhigh),
            0x1C0 if self.blitter_ecs_registers_enabled() => Some(self.agnus.htotal()),
            0x1DC if self.blitter_ecs_registers_enabled() => Some(self.agnus.beamcon0()),
            0x1C2 if self.blitter_ecs_registers_enabled() => Some(self.agnus.hsstop()),
            0x1C4 if self.blitter_ecs_registers_enabled() => Some(self.agnus.hbstrt()),
            0x1C6 if self.blitter_ecs_registers_enabled() => Some(self.agnus.hbstop()),
            0x1C8 if self.blitter_ecs_registers_enabled() => Some(self.agnus.vtotal()),
            0x1CA if self.blitter_ecs_registers_enabled() => Some(self.agnus.vsstop()),
            0x1CC if self.blitter_ecs_registers_enabled() => Some(self.agnus.vbstrt()),
            0x1CE if self.blitter_ecs_registers_enabled() => Some(self.agnus.vbstop()),
            0x1DE if self.blitter_ecs_registers_enabled() => Some(self.agnus.hsstrt()),
            0x1E0 if self.blitter_ecs_registers_enabled() => Some(self.agnus.vsstrt()),
            0x1E2 if self.blitter_ecs_registers_enabled() => Some(self.agnus.hcenter()),
            0x078 if self.blitter_ecs_registers_enabled() => Some(self.agnus.sprhdat()),
            0x1D8 if self.blitter_ecs_registers_enabled() => Some(self.agnus.hhpos()),
            0x092 => Some(self.denise.ddfstrt),
            0x094 => Some(self.denise.ddfstop),
            0x100 => Some(self.denise.bplcon0),
            0x102 => Some(self.denise.bplcon1),
            0x104 => Some(self.denise.bplcon2),
            0x106 => Some(self.denise.bplcon3),
            0x10C if self.denise_is_lisa() => Some(self.denise.bplcon4),
            0x10E if self.denise_is_lisa() => Some(self.denise.clxcon2),
            0x1FC if matches!(self.agnus.revision(), AgnusRevision::AgaAlice) => {
                Some(self.agnus.fmode())
            }
            0x108 => Some(self.denise.bpl1mod as u16),
            0x10A => Some(self.denise.bpl2mod as u16),
            off @ 0x110..=0x11E => {
                let idx = ((off - 0x110) / 2) as usize;
                let max = if self.aga_enabled() { 8 } else { 6 };
                (idx < max).then_some(self.denise.bpldat[idx])
            }
            off @ 0x0E0..=0x0FF => {
                let idx = ((off - 0x0E0) / 4) as usize;
                let max = if self.aga_enabled() { 8 } else { 6 };
                (idx < max).then(|| {
                    if off & 2 == 0 {
                        ((self.denise.bplpt[idx] >> 16) & 0x001F) as u16
                    } else {
                        (self.denise.bplpt[idx] & 0xFFFE) as u16
                    }
                })
            }
            off @ 0x120..=0x13F => {
                let idx = ((off - 0x120) / 4) as usize;
                (idx < 8).then(|| {
                    if off & 2 == 0 {
                        ((self.denise.sprpt[idx] >> 16) & 0x001F) as u16
                    } else {
                        (self.denise.sprpt[idx] & 0xFFFE) as u16
                    }
                })
            }
            off @ 0x140..=0x17F => {
                let idx = ((off - 0x140) / 8) as usize;
                if idx >= 8 {
                    return None;
                }
                match (off - 0x140) & 0x0006 {
                    0x0 => Some(self.denise.sprpos[idx]),
                    0x2 => Some(self.denise.sprctl[idx]),
                    0x4 => Some(self.denise.sprdata[idx]),
                    0x6 => Some(self.denise.sprdatb[idx]),
                    _ => None,
                }
            }
            off @ 0x180..=0x1BE => {
                let idx = ((off - 0x180) / 2) as usize;
                (idx < 32).then_some(self.denise.palette[idx])
            }
            _ => None,
        }
    }

    /// Returns true if the write asserted a new INTREQ bit (caller
    /// should preempt the slice to deliver the IRQ promptly).
    fn write_custom_word_from(&mut self, off: u16, val: u16, source: BeamWriteSource) -> bool {
        let off = off & 0xFFE;
        // Debugger-window register watch: record the first watched write
        // until the debugger polls it. The CpuCopperIrq attribution is a
        // render-pipeline nuance; the writer is the CPU.
        if !self.ui_reg_watches.is_empty()
            && self.ui_reg_hit.is_none()
            && self.ui_reg_watches.contains(&(off & 0x1FE))
        {
            self.ui_reg_hit = Some(UiRegHit {
                off: off & 0x1FE,
                value: val,
                source: match source {
                    BeamWriteSource::Copper => "copper",
                    BeamWriteSource::Cpu | BeamWriteSource::CpuCopperIrq => "cpu",
                },
                vpos: self.agnus.vpos as u16,
                hpos: self.agnus.hpos as u16,
            });
        }
        if is_audio_timing_custom_write(off) {
            self.flush_audio();
        }
        if is_render_relevant_custom_write(off)
            && !matches!(off, 0x180..=0x1BE)
            && (off != 0x1E4 || self.denise_ecs_registers())
            && (off != 0x106 || self.bplcon3_write_enabled())
            && (!matches!(off, 0x0F8..=0x0FF | 0x11C | 0x11E) || self.aga_enabled())
            && (!matches!(off, 0x10C | 0x10E) || self.denise_is_lisa())
            && (off != 0x1FC || self.aga_enabled())
        {
            self.record_render_write(off, val, source);
        }

        match off {
            0x038 | 0x03A | 0x03C | 0x03E => {
                // STREQU/STRVBL/STRHOR/STRLONG are Denise sync strobes.
                // Copperline's current video path derives sync/blanking from
                // the configured beam standard, so accepting these writes
                // as explicit no-ops documents them as outside the current
                // OCS-visible model.
                false
            }
            0x02A => {
                self.agnus.write_vposw(val);
                false
            }
            0x02C => {
                self.agnus.write_vhposw(val);
                false
            }
            0x02E => {
                if !matches!(source, BeamWriteSource::Copper)
                    || !matches!(self.agnus.revision(), AgnusRevision::Ocs)
                {
                    self.agnus.write_copcon(val);
                }
                false
            }
            0x030 => {
                let irq = self.paula.write_serdat(val);
                self.paula.intreq |= irq;
                if irq != 0 {
                    self.note_irq_source_asserted();
                }
                irq & self.paula.intena != 0
            }
            0x032 => {
                self.paula.serper = val;
                false
            }
            0x034 => {
                self.paula.write_potgo(val);
                false
            }
            0x036 => {
                self.input.write_joytest(val);
                false
            }
            0x020 => {
                self.floppy.set_dskpt_high(val);
                false
            }
            0x022 => {
                self.floppy.set_dskpt_low(val);
                false
            }
            0x024 => {
                // Debug: COPPERLINE_DBG_DSKLEN logs each DSKLEN write (disk read/write
                // arming) with its word count and beam position, to correlate disk
                // activity with scene/animation timing.
                if crate::envcfg::flag("COPPERLINE_DBG_DSKLEN") {
                    log::info!(
                        "dsklen f={} secs={:.4} v={} dskpt={:#08X} write={:#06X} dma={} wr={} words={}",
                        self.emulated_frames,
                        self.emulated_seconds(),
                        self.agnus.vpos,
                        self.floppy.dskpt(),
                        val,
                        (val >> 15) & 1,
                        (val >> 14) & 1,
                        val & 0x3FFF,
                    );
                }
                if self.floppy.write_dsklen(val, self.paula.adkcon) {
                    self.paula.intreq |= crate::chipset::paula::INT_DSKBLK;
                    self.note_irq_source_asserted();
                    return self.paula.intena & crate::chipset::paula::INT_DSKBLK != 0;
                }
                false
            }
            0x026 => {
                self.floppy.write_dskdat(val);
                false
            }
            0x08E => {
                if crate::envcfg::flag("COPPERLINE_DIAG_DISPLAY") {
                    log::info!(
                        "disp f={} v={} h={} DIWSTRT={:#06X}",
                        self.emulated_frames,
                        self.agnus.vpos,
                        self.agnus.hpos,
                        val
                    );
                }
                self.denise.diwstrt = val;
                // ECS DIWHIGH only supplies the window MSBs when it is written
                // *after* DIWSTRT/DIWSTOP (HRM p.306). A later DIWSTRT/DIWSTOP
                // write reverts to implicit (OCS-complement) MSB decoding until
                // DIWHIGH is rewritten. Without this, a stale DIWHIGH left by
                // a previous ECS display shrinks the vertical window of an OCS
                // program booted afterwards (it sets DIWSTRT/DIWSTOP but never
                // DIWHIGH), so no bitplane DMA falls inside the window and the
                // display goes black.
                self.denise.diwhigh_written = false;
                self.capture_same_line_display_start_if_due();
                false
            }
            0x090 => {
                if crate::envcfg::flag("COPPERLINE_DIAG_DISPLAY") {
                    log::info!(
                        "disp f={} v={} h={} DIWSTOP={:#06X}",
                        self.emulated_frames,
                        self.agnus.vpos,
                        self.agnus.hpos,
                        val
                    );
                }
                self.denise.diwstop = val;
                self.denise.diwhigh_written = false;
                self.capture_same_line_display_start_if_due();
                false
            }
            0x098 => {
                self.denise.clxcon = val;
                // AGA: a CLXCON write resets CLXCON2 (planes 7-8 collision
                // control returns to disabled).
                if self.denise_is_lisa() {
                    self.denise.clxcon2 = 0;
                }
                false
            }
            0x092 => {
                if crate::envcfg::flag("COPPERLINE_DIAG_DISPLAY") {
                    log::info!(
                        "disp f={} v={} h={} DDFSTRT={:#06X}",
                        self.emulated_frames,
                        self.agnus.vpos,
                        self.agnus.hpos,
                        val
                    );
                }
                self.denise.ddfstrt = val;
                self.record_ddfstrt_write_match_miss(val);
                false
            }
            0x094 => {
                if crate::envcfg::flag("COPPERLINE_DIAG_DISPLAY") {
                    log::info!(
                        "disp f={} v={} h={} DDFSTOP={:#06X}",
                        self.emulated_frames,
                        self.agnus.vpos,
                        self.agnus.hpos,
                        val
                    );
                }
                self.denise.ddfstop = val;
                false
            }
            0x080 => {
                self.agnus.set_cop1lc_high(val);
                false
            }
            0x082 => {
                self.agnus.set_cop1lc_low(val);
                false
            }
            0x084 => {
                self.agnus.set_cop2lc_high(val);
                false
            }
            0x086 => {
                self.agnus.set_cop2lc_low(val);
                false
            }
            0x088 => {
                self.pending_copper_frame_start = None;
                self.copper.jump(self.agnus.cop1lc);
                false
            }
            0x08A => {
                self.pending_copper_frame_start = None;
                self.copper.jump(self.agnus.cop2lc);
                false
            }
            0x096 => {
                let previous = self.effective_bitplane_dmacon();
                self.agnus.write_dmacon(val);
                if crate::envcfg::flag("COPPERLINE_DIAG_AUDIO_NOTES")
                    && (val & 0x000F != 0 || self.agnus.dmacon & 0x000F != previous & 0x000F)
                {
                    log::info!(
                        "audio-ctl frame={} DMACON write={:#06X} -> {:#06X}",
                        self.emulated_frames,
                        val,
                        self.agnus.dmacon
                    );
                }
                if crate::envcfg::flag("COPPERLINE_DIAG_DISPLAY") && self.agnus.dmacon != previous {
                    log::info!(
                        "disp f={} v={} h={} DMACON write={:#06X} -> dmacon={:#06X} (AUD={:X} SPR={} DSK={} BLT={})",
                        self.emulated_frames,
                        self.agnus.vpos,
                        self.agnus.hpos,
                        val,
                        self.agnus.dmacon,
                        self.agnus.dmacon & 0x000F,
                        (self.agnus.dmacon >> 5) & 1,
                        (self.agnus.dmacon >> 4) & 1,
                        (self.agnus.dmacon >> 6) & 1,
                    );
                }
                if self.agnus.dmacon != previous {
                    self.record_bitplane_dmacon_write(previous);
                }
                false
            }
            // Blitter $040-$074. BLTSIZE ($058) triggers the blit and
            // starts scheduled DMA. The CPU slice is stopped after the
            // write instruction retires so Agnus can grant early blitter
            // slots before ROM-only code reuses chip-memory sources.
            0x040 => {
                self.blitter.write_bltcon0(val);
                false
            }
            0x042 => {
                let val = if self.blitter_ecs_registers_enabled() {
                    val
                } else {
                    val & !BLTCON1_DOFF
                };
                self.blitter.write_bltcon1(val);
                false
            }
            0x044 => {
                self.finish_pending_blitter();
                self.blitter.bltafwm = val;
                false
            }
            0x046 => {
                self.finish_pending_blitter();
                self.blitter.bltalwm = val;
                false
            }
            0x048 => {
                self.finish_pending_blitter();
                self.blitter.set_cpt_high(val);
                false
            }
            0x04A => {
                self.finish_pending_blitter();
                self.blitter.set_cpt_low(val);
                false
            }
            0x04C => {
                self.finish_pending_blitter();
                self.blitter.set_bpt_high(val);
                false
            }
            0x04E => {
                self.finish_pending_blitter();
                self.blitter.set_bpt_low(val);
                false
            }
            0x050 => {
                self.finish_pending_blitter();
                self.blitter.set_apt_high(val);
                false
            }
            0x052 => {
                self.finish_pending_blitter();
                self.blitter.set_apt_low(val);
                false
            }
            0x054 => {
                self.finish_pending_blitter();
                self.blitter.set_dpt_high(val);
                false
            }
            0x056 => {
                self.finish_pending_blitter();
                self.blitter.set_dpt_low(val);
                false
            }
            0x058 => {
                self.finish_pending_blitter();
                // Starting a new blit consumes a stale pending blitter-done
                // interrupt request: INTREQ.BLIT reflects "the last started blit
                // has finished", so a BLTSIZE write while the request is still
                // set (never acknowledged) clears it, and the interrupt for this
                // blit fires only when it actually completes.
                //
                // A BLTSIZE write can follow a long run of polling-only blits
                // with INTREQ.BLIT still stale-set. Enabling INTENA.BLIT for the
                // new blit must not take an immediate interrupt before software
                // patches its handler operands; on real hardware the new
                // BLTSIZE consumes the stale request and the next request is
                // raised only when the new blit completes. This is also
                // consistent with how the OS blitter queue manages the interrupt
                // via INTENA rather than relying on INTREQ acks while the blitter
                // is idle.
                self.paula.intreq &= !crate::chipset::paula::INT_BLIT;
                self.note_irq_latches_changed();
                // A blit over the exception-vector area is useful crash context:
                // flag it so the CPU wrapper can dump the instruction history.
                if self.blitter.bltcon0 & 0x0100 != 0 && self.blitter.bltdpt < 0x1000 {
                    self.diag_lowmem_blit = true;
                }
                self.diag_blit_start(((val as u32) >> 6) & 0x3FF, (val as u32) & 0x3F);
                // COPPERLINE_DBG_BLIT="<lo_secs>:<hi_secs>": log each blit's key
                // parameters (control words, D pointer/modulo, size, mode flags)
                // with its beam position, within the time window. For
                // investigating which blit produces a given rendered region.
                if let Some(spec) = crate::envcfg::var("COPPERLINE_DBG_BLIT") {
                    let mut parts = spec.split(':');
                    let lo: f64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                    let hi: f64 = parts
                        .next()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(f64::MAX);
                    let secs = self.emulated_seconds();
                    if (lo..hi).contains(&secs) {
                        let h = (val >> 6) & 0x3FF;
                        let w = val & 0x3F;
                        let c1 = self.blitter.bltcon1;
                        log::info!(
                            "blit t={secs:.5} f={} v={} h={} bltcon0={:#06X} bltcon1={:#06X} \
                             dpt={:#08X} dmod={} size={}x{} {}{}{}",
                            self.emulated_frames,
                            self.agnus.vpos,
                            self.agnus.hpos,
                            self.blitter.bltcon0,
                            c1,
                            self.blitter.bltdpt,
                            self.blitter.bltdmod,
                            h,
                            w,
                            if c1 & 0x0001 != 0 { "LINE " } else { "" },
                            if c1 & 0x0008 != 0 { "FILL " } else { "" },
                            if c1 & 0x0002 != 0 { "DESC " } else { "" },
                        );
                    }
                }
                self.trace_blitter_start(val, source);
                self.blitter.start_scheduled(val, &self.mem.chip_ram);
                self.record_blit_accounting();
                self.slice_preempted = true;
                false
            }
            0x05A => {
                if !self.blitter_ecs_registers_enabled() {
                    return false;
                }
                self.finish_pending_blitter();
                self.blitter.bltcon0 = (self.blitter.bltcon0 & 0xFF00) | (val & 0x00FF);
                false
            }
            0x05C => {
                if !self.blitter_ecs_registers_enabled() {
                    return false;
                }
                self.finish_pending_blitter();
                self.blitter.bltsizv = val & 0x7FFF;
                false
            }
            0x05E => {
                if !self.blitter_ecs_registers_enabled() {
                    return false;
                }
                self.finish_pending_blitter();
                // Same as BLTSIZE: starting a new blit consumes a stale pending
                // blitter-done interrupt request.
                self.paula.intreq &= !crate::chipset::paula::INT_BLIT;
                self.note_irq_latches_changed();
                self.trace_blitter_start_ecs(val, source);
                self.diag_blit_start(u32::from(self.blitter.bltsizv), (val as u32) & 0x07FF);
                self.blitter.start_scheduled_ecs(val, &self.mem.chip_ram);
                self.record_blit_accounting();
                self.slice_preempted = true;
                false
            }
            0x060 => {
                self.finish_pending_blitter();
                self.blitter.bltcmod = (val & 0xFFFE) as i16;
                false
            }
            0x062 => {
                self.finish_pending_blitter();
                self.blitter.bltbmod = (val & 0xFFFE) as i16;
                false
            }
            0x064 => {
                self.finish_pending_blitter();
                self.blitter.bltamod = (val & 0xFFFE) as i16;
                false
            }
            0x066 => {
                self.finish_pending_blitter();
                self.blitter.bltdmod = (val & 0xFFFE) as i16;
                false
            }
            0x070 => {
                self.blitter.write_bltcdat(val);
                false
            }
            0x072 => {
                self.blitter.write_bltbdat(val);
                false
            }
            0x074 => {
                self.blitter.write_bltadat(val);
                false
            }
            0x07E => {
                if self.floppy.write_dsksync(val) {
                    self.paula.intreq |= INT_DSKSYNC;
                    self.note_irq_source_asserted();
                    return self.paula.intena & INT_DSKSYNC != 0;
                }
                false
            }
            0x09A => {
                // COPPERLINE_DBG_CIA: log INTENA writes touching EXTER or the
                // master enable, to order them against CIA-B TOD-alarm latches
                // (this ordering identified the 9 Fingers TOD-alarm guru).
                if dbg_cia_on() && val & (INT_EXTER | 0x4000) != 0 {
                    log::info!(
                        "INTENA write {:#06X} from {:?} secs={:.5} (intena was {:#06X}, intreq {:#06X})",
                        val,
                        source,
                        self.emulated_seconds(),
                        self.paula.intena,
                        self.paula.intreq,
                    );
                }
                self.paula.write_intena(val);
                self.note_irq_latches_changed();
                false
            }
            0x09C => {
                // COPPERLINE_DBG_CIA: same for INTREQ writes touching EXTER.
                if dbg_cia_on() && val & INT_EXTER != 0 {
                    log::info!(
                        "INTREQ write {:#06X} from {:?} secs={:.5} (intreq was {:#06X})",
                        val,
                        source,
                        self.emulated_seconds(),
                        self.paula.intreq,
                    );
                }
                // Crash diagnostics: log INTREQ writes that touch the blitter bit,
                // to identify which code set or acknowledged the request.
                if crate::envcfg::flag("COPPERLINE_DIAG_CRASH")
                    && val & crate::chipset::paula::INT_BLIT != 0
                {
                    log::warn!(
                        "INTREQ write {:#06X} from {:?} at t={:.4} (intreq was {:#06X})",
                        val,
                        source,
                        self.emulated_seconds(),
                        self.paula.intreq,
                    );
                }
                let coper_was_pending = self.paula.intreq & INT_COPER != 0;
                let asserted = self.paula.write_intreq(val);
                let copper_asserted = matches!(source, BeamWriteSource::Copper)
                    && val & 0x8000 != 0
                    && val & INT_COPER != 0
                    && !coper_was_pending;
                if copper_asserted {
                    self.pending_copper_irq_beam = Some((self.agnus.vpos, self.agnus.hpos));
                    self.coper_cpu_irq_delay_cck = COPER_CPU_IRQ_DELAY_CCK;
                }
                if copper_asserted && asserted {
                    self.note_irq_source_asserted();
                } else {
                    self.note_irq_latches_changed();
                }
                if val & 0x8000 == 0 && val & INT_COPER != 0 {
                    self.pending_copper_irq_beam = None;
                    self.coper_cpu_irq_delay_cck = 0;
                }
                if matches!(source, BeamWriteSource::Cpu) {
                    self.note_intreq_palette_target(val);
                }
                asserted
            }
            0x09E => {
                if crate::envcfg::flag("COPPERLINE_DIAG_AUDIO_NOTES") {
                    log::info!(
                        "audio-ctl frame={} ADKCON write={:#06X}",
                        self.emulated_frames,
                        val
                    );
                }
                self.paula.write_adkcon(val);
                false
            }
            0x1C0 => {
                if self.blitter_ecs_registers_enabled() {
                    self.agnus.write_htotal(val);
                    self.refresh_paula_audio_min_period();
                }
                false
            }
            0x1DC => {
                if self.blitter_ecs_registers_enabled() {
                    self.agnus.write_beamcon0(val);
                    self.refresh_paula_audio_min_period();
                    if val & BEAMCON0_DUAL != 0 && !self.uhres_dual_warned {
                        log::warn!(
                            "BEAMCON0 DUAL set: A2024/Productivity (UHRES dual-monitor) display is not emulated"
                        );
                        self.uhres_dual_warned = true;
                    }
                }
                false
            }
            // ECS Agnus programmable sync/blank latches + UHRES SPRHDAT.
            // Stored only; no scan-rate geometry derived yet (TODO section 9).
            0x1C2 => {
                if self.blitter_ecs_registers_enabled() {
                    self.agnus.write_hsstop(val);
                }
                false
            }
            0x1C4 => {
                if self.blitter_ecs_registers_enabled() {
                    self.agnus.write_hbstrt(val);
                }
                false
            }
            0x1C6 => {
                if self.blitter_ecs_registers_enabled() {
                    self.agnus.write_hbstop(val);
                }
                false
            }
            0x1C8 => {
                if self.blitter_ecs_registers_enabled() {
                    self.agnus.write_vtotal(val);
                }
                false
            }
            0x1CA => {
                if self.blitter_ecs_registers_enabled() {
                    self.agnus.write_vsstop(val);
                }
                false
            }
            0x1CC => {
                if self.blitter_ecs_registers_enabled() {
                    self.agnus.write_vbstrt(val);
                }
                false
            }
            0x1CE => {
                if self.blitter_ecs_registers_enabled() {
                    self.agnus.write_vbstop(val);
                }
                false
            }
            0x1DE => {
                if self.blitter_ecs_registers_enabled() {
                    self.agnus.write_hsstrt(val);
                }
                false
            }
            0x1E0 => {
                if self.blitter_ecs_registers_enabled() {
                    self.agnus.write_vsstrt(val);
                }
                false
            }
            0x1E2 => {
                if self.blitter_ecs_registers_enabled() {
                    self.agnus.write_hcenter(val);
                }
                false
            }
            0x078 => {
                if self.blitter_ecs_registers_enabled() {
                    self.agnus.write_sprhdat(val);
                }
                false
            }
            0x1D8 => {
                if self.blitter_ecs_registers_enabled() {
                    self.agnus.write_hhposw(val);
                }
                false
            }
            // Audio block: AUD0..AUD3, 16 bytes per channel.
            off @ 0x0A0..=0x0DF => {
                // Timestamp audio register writes (AUDxLC/LEN/PER/VOL) by
                // emulated frame to trace note triggers and buffer chaining.
                if crate::envcfg::flag("COPPERLINE_DIAG_AUDIO_NOTES") {
                    let lane = (off - 0x0A0) & 0x0F;
                    if matches!(lane, 0x0 | 0x2 | 0x4 | 0x6 | 0x8) {
                        let ch = (off - 0x0A0) / 0x10;
                        let kind = match lane {
                            0x0 => "LCH",
                            0x2 => "LCL",
                            0x4 => "LEN",
                            0x6 => "PER",
                            _ => "VOL",
                        };
                        log::info!(
                            "audio-note frame={} ch={} {}={}",
                            self.emulated_frames,
                            ch,
                            kind,
                            val
                        );
                    }
                }
                self.paula.write_audio_reg(off - 0x0A0, val);
                false
            }
            0x100 => {
                // ERSY/LPEN/COLOR/GAUD side effects are external sync,
                // light-pen, colorburst, and genlock-audio controls. The
                // current renderer keeps the bits for mode/register replay
                // but intentionally does not model those board-level pins.
                let previous = self.effective_bitplane_bplcon0();
                if crate::envcfg::flag("COPPERLINE_DIAG_DISPLAY") && val != previous {
                    let bpu = (val >> 12) & 0x7;
                    let hires = (val >> 15) & 1;
                    log::info!(
                        "disp f={} v={} h={} BPLCON0={:#06X} bpu={} hires={}",
                        self.emulated_frames,
                        self.agnus.vpos,
                        self.agnus.hpos,
                        val,
                        bpu,
                        hires
                    );
                }
                self.denise.bplcon0 = val;
                // Agnus snoops BPLCON0.LPEN (bit 3) for the light-pen beam
                // latch; the comment above still holds for the board pins.
                self.agnus.set_lpen(val & 0x0008 != 0);
                // BPLCON0.ERSY (bit 1) with no genlock attached stops the
                // beam counters; the boot ROM genlock probe depends on
                // the VPOSR/VHPOSR readback freezing while it is set.
                self.agnus.set_ersy(val & 0x0002 != 0);
                if self.denise.bplcon0 != previous {
                    self.record_bitplane_bplcon0_write(previous);
                }
                false
            }
            0x102 => {
                self.denise.bplcon1 = val;
                false
            }
            0x104 => {
                self.denise.bplcon2 = val;
                false
            }
            0x106 => {
                // ECS Denise only latches BPLCON3 while BPLCON0 bit 0
                // (ENBPLCN3/ECSENA) is set; OCS Denise has no BPLCON3.
                if self.bplcon3_write_enabled() {
                    self.denise.bplcon3 = val;
                } else if !self.bplcon3_drop_warned && val != 0 {
                    self.bplcon3_drop_warned = true;
                    log::info!(
                        "BPLCON3 write {val:#06X} dropped ({}); further drops not logged",
                        if self.denise_ecs_registers() {
                            "ENBPLCN3 clear"
                        } else {
                            "OCS Denise"
                        }
                    );
                }
                false
            }
            // AGA Lisa registers: latched only until the AGA display path
            // lands (plan 3.3/3.4); unreachable from config today because
            // Lisa is not selectable.
            0x10C => {
                if self.denise_is_lisa() {
                    if crate::envcfg::flag("COPPERLINE_DIAG_DISPLAY") && self.denise.bplcon4 != val
                    {
                        log::info!(
                            "disp f={} v={} h={} BPLCON4={:#06X}",
                            self.emulated_frames,
                            self.agnus.vpos,
                            self.agnus.hpos,
                            val
                        );
                    }
                    self.denise.bplcon4 = val;
                }
                false
            }
            0x10E => {
                if self.denise_is_lisa() {
                    self.denise.clxcon2 = val & 0x0FFF;
                }
                false
            }
            // AGA Alice FMODE (write_fmode gates on the revision itself).
            0x1FC => {
                self.agnus.write_fmode(val);
                if crate::envcfg::flag("COPPERLINE_DIAG_DISPLAY") {
                    log::info!(
                        "disp f={} v={} h={} FMODE={:#06X}",
                        self.emulated_frames,
                        self.agnus.vpos,
                        self.agnus.hpos,
                        self.agnus.fmode()
                    );
                }
                false
            }
            0x108 => {
                self.denise.bpl1mod = (val & 0xFFFE) as i16;
                if crate::envcfg::flag("COPPERLINE_DIAG_DISPLAY") {
                    log::info!(
                        "disp f={} v={} h={} BPL1MOD={}",
                        self.emulated_frames,
                        self.agnus.vpos,
                        self.agnus.hpos,
                        self.denise.bpl1mod
                    );
                }
                false
            }
            0x10A => {
                self.denise.bpl2mod = (val & 0xFFFE) as i16;
                if crate::envcfg::flag("COPPERLINE_DIAG_DISPLAY") {
                    log::info!(
                        "disp f={} v={} h={} BPL2MOD={}",
                        self.emulated_frames,
                        self.agnus.vpos,
                        self.agnus.hpos,
                        self.denise.bpl2mod
                    );
                }
                false
            }
            off @ 0x110..=0x11E => {
                let idx = ((off - 0x110) / 2) as usize;
                if idx < if self.aga_enabled() { 8 } else { 6 } {
                    if idx == 0 {
                        self.record_sprite_display_enable_at(self.agnus.vpos, self.agnus.hpos);
                    }
                    self.denise.write_bpldat(idx, val);
                }
                false
            }
            // Sprite pointers: SPR0PTH/PTL .. SPR7PTH/PTL.
            off @ 0x120..=0x13F => {
                let idx = ((off - 0x120) / 4) as usize;
                if idx < 8 {
                    if off & 2 == 0 {
                        self.denise.set_sprpt_high(idx, val);
                    } else {
                        self.denise.set_sprpt_low(idx, val);
                    }
                    if diag_sprcap().is_some() && off & 2 != 0 {
                        log::info!(
                            "sprptw f={} v={} h={} s{idx} = {:06X}",
                            self.emulated_frames,
                            self.agnus.vpos,
                            self.agnus.hpos,
                            self.denise.sprpt[idx]
                        );
                    }
                    self.display_dma_sprpt[idx] = self.denise.sprpt[idx];
                    if off & 2 != 0 {
                        self.apply_display_sprite_pointer_low_write(idx);
                    }
                }
                false
            }
            // Sprite position/control/data registers:
            // SPRxPOS, SPRxCTL, SPRxDATA, SPRxDATB.
            off @ 0x140..=0x17F => {
                let idx = ((off - 0x140) / 8) as usize;
                let reg = (off - 0x140) & 0x0006;
                if idx < 8 {
                    match reg {
                        0x0 => {
                            self.denise.sprpos[idx] = val;
                            self.latch_display_sprite_dma_control_from_registers(idx);
                        }
                        0x2 => {
                            self.denise.write_sprctl(idx, val);
                            self.latch_display_sprite_dma_control_from_registers(idx);
                        }
                        0x4 => self.denise.write_sprdata(idx, val),
                        0x6 => self.denise.write_sprdatb(idx, val),
                        _ => {}
                    }
                }
                false
            }
            0x1E4 => {
                if self.denise_ecs_registers() {
                    self.denise.diwhigh = val;
                    self.denise.diwhigh_written = true;
                }
                false
            }
            // Bitplane pointers: $0E0..$0F4 high, +2 low; BPL7/BPL8 at
            // $0F8/$0FC exist on AGA Alice only.
            off @ 0x0E0..=0x0FF => {
                let idx = ((off - 0x0E0) / 4) as usize;
                if idx < if self.aga_enabled() { 8 } else { 6 } {
                    if off & 2 == 0 {
                        self.denise.set_bplpt_high(idx, val);
                    } else {
                        self.denise.set_bplpt_low(idx, val);
                    }
                    self.display_dma_bplpt[idx] = self.denise.bplpt[idx];
                    if off & 2 != 0 && crate::envcfg::flag("COPPERLINE_DIAG_BPLPT") {
                        log::info!(
                            "bplpt f={} v={} h={} src={} BPL{}PT={:#08X} cop1lc={:#08X} coppc={:#08X}",
                            self.emulated_frames,
                            self.agnus.vpos,
                            self.agnus.hpos,
                            beam_write_source_name(source),
                            idx + 1,
                            self.denise.bplpt[idx],
                            self.agnus.cop1lc,
                            self.copper.pc(),
                        );
                    }
                }
                false
            }
            // Color palette $180..$1BE in pairs of two bytes.
            off @ 0x180..=0x1BE => {
                let idx = ((off - 0x180) / 2) as usize;
                if idx < 32 {
                    let color = color_register_value(val);
                    let render_source = if matches!(source, BeamWriteSource::Cpu)
                        && matches!(self.cpu_palette_target, CpuPaletteTarget::Bottom)
                    {
                        BeamWriteSource::CpuCopperIrq
                    } else {
                        source
                    };
                    self.record_render_write(off, val, render_source);
                    if self.denise_is_lisa() {
                        // AGA: BPLCON3 BANK/LOCT route the write into the
                        // 256-entry store. Bank 0 with LOCT clear is the
                        // OCS-compatible case. The replay renderer still
                        // tracks bank 0 only until plan 3.3 lands.
                        self.denise.palette.write_banked(
                            crate::chipset::denise::Palette::bank_from_bplcon3(self.denise.bplcon3),
                            idx,
                            crate::chipset::denise::Palette::loct_from_bplcon3(self.denise.bplcon3),
                            color,
                        );
                    } else {
                        self.denise.palette.write_ocs(idx, color);
                    }
                    if matches!(source, BeamWriteSource::Cpu) {
                        self.write_cpu_palette_snapshot(idx, color);
                    }
                }
                false
            }
            _ => false,
        }
    }

    fn advance_one_chip_bus_quantum(
        &mut self,
        forced_owner: Option<ChipBusOwner>,
    ) -> (u32, AgnusTick) {
        self.advance_one_chip_bus_quantum_limited(forced_owner, self.next_chip_bus_quantum())
    }

    fn advance_one_chip_bus_quantum_limited(
        &mut self,
        forced_owner: Option<ChipBusOwner>,
        max_cck: u32,
    ) -> (u32, AgnusTick) {
        let cck = self.next_chip_bus_quantum().min(max_cck).max(1);
        self.flush_audio_before_audio_dma_slot();

        // Advance the Copper's two-cycle cadence on every Copper-eligible color
        // clock: it fetches on every other one and yields the idle half (and
        // any sleeping WAIT cycle) to the blitter/CPU. `allow_fetch` is false
        // when a forced owner (a granted CPU access) already holds this cycle.
        let eligible =
            cck >= CHIP_BUS_SLOT_CCK && self.copper_wait_free_cycle_available_at(self.agnus.hpos);
        let copper_took_bus = eligible && self.step_copper_eligible_slot(forced_owner.is_none());
        if !eligible && cck >= CHIP_BUS_SLOT_CCK && self.copper_comparator_runs_at(self.agnus.hpos)
        {
            // A fixed DMA owner (bitplane/sprite/disk/audio/refresh) holds
            // this color clock, but the Copper's WAIT/SKIP comparator is
            // combinational and keeps running: only instruction fetches need
            // a bus slot. Without this, a wait whose only releasable color
            // clock sits under display fetch (e.g. hpos $DE inside the last
            // DDFSTOP=$D8 fetch unit of an overscan screen) never wakes: the
            // line-end blackout covers the following ccks and an 8-bit
            // vertical target like WAIT vp=$FF goes false again after the
            // line-255 rollover. With allow_fetch=false a Running Copper
            // cannot fetch here, so the slot is never taken from its owner.
            let _ = self.step_copper_eligible_slot(false);
        }

        let owner = match forced_owner {
            Some(owner) => owner,
            None if copper_took_bus => ChipBusOwner::Copper,
            None if eligible => self.free_chip_bus_slot_owner(),
            None => self.scheduled_dma_owner(false),
        };
        self.last_chip_bus_owner = owner;
        if self.bus_accounting.enabled {
            self.bus_accounting
                .record_cck(owner, cck, self.blitter.busy);
            if matches!(owner, ChipBusOwner::Bitplane) {
                let v = self.agnus.vpos as usize;
                if v < self.dbg_bpl_cck.len() {
                    self.dbg_bpl_cck[v] += cck;
                }
            }
        }
        if self.dbg_slotmap_on {
            let v = self.agnus.vpos as usize;
            let h = self.agnus.hpos as usize;
            if self.dbg_slotmap.is_empty() {
                self.dbg_slotmap = vec![vec![b'.'; 256]; 320];
            }
            if v < self.dbg_slotmap.len() {
                let code = chip_bus_owner_code(owner);
                let row = &mut self.dbg_slotmap[v];
                let end = (h + cck as usize).min(row.len());
                for slot in row.iter_mut().take(end).skip(h) {
                    *slot = code;
                }
            }
        }
        // The Copper was already stepped above (or is held without fetching at
        // the end-of-line lockout); only drive the other owners here.
        if !matches!(owner, ChipBusOwner::Copper) {
            self.process_chip_bus_owner(owner);
        }
        // A busy blitter's idle pipeline cycles leave the chip bus free, but
        // they still advance on Agnus slots that are available to the
        // CPU/blitter/Copper arbitration domain. Fixed DMA slots stall even an
        // idle blitter phase; otherwise display DMA would not slow area fills.
        if !matches!(owner, ChipBusOwner::Blitter)
            && matches!(
                owner,
                ChipBusOwner::Idle | ChipBusOwner::Cpu | ChipBusOwner::Copper
            )
            && self.blitter.busy
            && self.blitter_dma_enabled()
            && !self.blitter.current_slot_needs_bus()
            && self.blitter.tick_scheduled_slot(&mut self.mem.chip_ram)
        {
            self.latch_blitter_completion("idle_pipeline");
        }
        let tick = self.advance_beam(cck);
        self.audio_pending_cck = self.audio_pending_cck.saturating_add(cck);
        (cck, tick)
    }

    /// Step the Copper through one eligible color clock and apply any register
    /// write it produced. Returns whether the Copper used the bus this cycle.
    fn step_copper_eligible_slot(&mut self, allow_fetch: bool) -> bool {
        let cop1lc = self.agnus.cop1lc;
        let cop2lc = self.agnus.cop2lc;
        let vpos = self.agnus.vpos;
        let hpos = self.agnus.hpos;
        let blitter_busy = self.blitter.busy;
        let line_cck = self.agnus.current_line_cck();
        let mut copper = std::mem::take(&mut self.copper);
        let action = copper.step_eligible_slot(
            &self.mem.chip_ram,
            vpos,
            hpos,
            blitter_busy,
            cop1lc,
            cop2lc,
            allow_fetch,
            line_cck,
        );
        self.copper = copper;
        match action {
            CopperSlotAction::Idle => false,
            CopperSlotAction::BusUsed => true,
            CopperSlotAction::Move { register, value } => {
                if self.copper_can_write_custom(register) {
                    let _ = self.write_custom_word_from(register, value, BeamWriteSource::Copper);
                } else {
                    self.copper.stop();
                }
                true
            }
        }
    }

    /// Owner of a Copper-eligible free color clock that the Copper did not take
    /// (its idle half, a sleeping WAIT, or a stopped Copper): the blitter if it
    /// is running and its current pipeline cycle accesses the bus, otherwise
    /// idle/CPU.
    fn free_chip_bus_slot_owner(&self) -> ChipBusOwner {
        if self.blitter.busy && self.blitter_dma_enabled() && self.blitter.current_slot_needs_bus()
        {
            ChipBusOwner::Blitter
        } else {
            ChipBusOwner::Idle
        }
    }

    fn advance_beam(&mut self, cck: u32) -> AgnusTick {
        let old_vpos = self.agnus.vpos;
        let old_hpos = self.agnus.hpos;
        let old_emulated_cck = self.emulated_cck;
        self.emulated_cck = self.emulated_cck.saturating_add(cck as u64);
        self.coper_cpu_irq_delay_cck = self.coper_cpu_irq_delay_cck.saturating_sub(cck);
        if self.irq_latency_cck != 0 {
            self.irq_latency_cck = self.irq_latency_cck.saturating_sub(cck);
            if self.irq_latency_cck == 0 {
                self.irq_latency_mask = 0;
            }
        }
        let tick = self.agnus.advance_by_cck(cck);
        if tick.new_frames == 0 && tick.new_lines == 0 {
            self.capture_sprite_dma_words_if_due(old_vpos, old_hpos, self.agnus.hpos);
            self.capture_bitplane_dma_words_if_due(
                old_vpos,
                old_hpos,
                self.agnus.hpos,
                old_emulated_cck,
            );
        }
        if tick.new_lines != 0 || tick.new_frames != 0 {
            self.bitplane_ddfstart_miss = None;
        }
        let visible_start = self.visible_start_vpos_for_current_control();
        if tick.new_frames == 0 && old_vpos < visible_start && self.agnus.vpos >= visible_start {
            self.current_frame_visible_start_vpos = visible_start;
            self.refresh_frame_geometry_visible_start();
            self.capture_current_frame_display_start();
        }
        for _ in 0..tick.new_frames {
            self.emulated_frames = self.emulated_frames.saturating_add(1);
            self.begin_new_beam_frame();
        }
        self.start_pending_copper_frame_if_due();
        tick
    }

    fn accumulate_live_collisions_until_current_beam(&mut self) {
        const VISIBLE_END_HPOS: u32 =
            RENDER_COPPER_WAIT_HPOS_FB0 + (RENDER_FRAMEBUFFER_WIDTH as u32 / 4);
        let visible_start_vpos = self.current_frame_visible_start_vpos;
        let visible_end_vpos =
            visible_start_vpos + self.current_frame_geometry.visible_lines as u32;
        let mut end_vpos = self.agnus.vpos;
        let mut end_hpos = self.agnus.hpos.min(VISIBLE_END_HPOS);

        if end_vpos < visible_start_vpos {
            return;
        }
        if end_vpos >= visible_end_vpos {
            end_vpos = visible_end_vpos - 1;
            end_hpos = VISIBLE_END_HPOS;
        }
        if end_hpos <= RENDER_COPPER_WAIT_HPOS_FB0 && end_vpos == visible_start_vpos {
            return;
        }

        let start_vpos = self.lazy_collision_vpos.max(visible_start_vpos);
        if start_vpos > end_vpos {
            return;
        }
        let start_hpos = if self.lazy_collision_vpos < visible_start_vpos {
            RENDER_COPPER_WAIT_HPOS_FB0
        } else {
            self.lazy_collision_hpos.min(VISIBLE_END_HPOS)
        };
        if start_vpos == end_vpos && start_hpos >= end_hpos {
            return;
        }

        for vpos in start_vpos..=end_vpos {
            let old_hpos = if vpos == start_vpos {
                start_hpos
            } else {
                RENDER_COPPER_WAIT_HPOS_FB0
            };
            let new_hpos = if vpos == end_vpos {
                end_hpos
            } else {
                VISIBLE_END_HPOS
            };
            if new_hpos <= old_hpos {
                continue;
            }
            self.accumulate_live_playfield_collisions_if_due(vpos, old_hpos, new_hpos);
            self.accumulate_live_manual_bpl_collisions_if_due(vpos, old_hpos, new_hpos);
            self.accumulate_live_sprite_sprite_collisions_if_due(vpos, old_hpos, new_hpos);
            self.accumulate_live_manual_sprite_collisions_if_due(vpos, old_hpos, new_hpos);
        }

        self.lazy_collision_vpos = end_vpos;
        self.lazy_collision_hpos = end_hpos;
    }

    fn accumulate_live_collisions_to_frame_end(&mut self) {
        const VISIBLE_END_HPOS: u32 =
            RENDER_COPPER_WAIT_HPOS_FB0 + (RENDER_FRAMEBUFFER_WIDTH as u32 / 4);
        let visible_start_vpos = self.current_frame_visible_start_vpos;
        let visible_end_vpos =
            visible_start_vpos + self.current_frame_geometry.visible_lines as u32;
        if visible_end_vpos <= visible_start_vpos {
            return;
        }

        let end_vpos = visible_end_vpos - 1;
        let start_vpos = self.lazy_collision_vpos.max(visible_start_vpos);
        if start_vpos > end_vpos {
            return;
        }
        let start_hpos = if self.lazy_collision_vpos < visible_start_vpos {
            RENDER_COPPER_WAIT_HPOS_FB0
        } else {
            self.lazy_collision_hpos.min(VISIBLE_END_HPOS)
        };
        if start_vpos == end_vpos && start_hpos >= VISIBLE_END_HPOS {
            return;
        }

        for vpos in start_vpos..=end_vpos {
            let old_hpos = if vpos == start_vpos {
                start_hpos
            } else {
                RENDER_COPPER_WAIT_HPOS_FB0
            };
            let new_hpos = VISIBLE_END_HPOS;
            if new_hpos <= old_hpos {
                continue;
            }
            self.accumulate_live_playfield_collisions_if_due(vpos, old_hpos, new_hpos);
            self.accumulate_live_manual_bpl_collisions_if_due(vpos, old_hpos, new_hpos);
            self.accumulate_live_sprite_sprite_collisions_if_due(vpos, old_hpos, new_hpos);
            self.accumulate_live_manual_sprite_collisions_if_due(vpos, old_hpos, new_hpos);
        }

        self.lazy_collision_vpos = end_vpos;
        self.lazy_collision_hpos = VISIBLE_END_HPOS;
    }

    fn accumulate_live_sprite_sprite_collisions_if_due(
        &mut self,
        vpos: u32,
        old_hpos: u32,
        new_hpos: u32,
    ) {
        let visible_start_vpos = self.current_frame_visible_start_vpos;
        if new_hpos <= RENDER_COPPER_WAIT_HPOS_FB0 {
            return;
        }
        let Some(fb_y) = visible_framebuffer_y(
            vpos,
            visible_start_vpos,
            self.current_frame_geometry.visible_lines,
        ) else {
            return;
        };
        let x_start = old_hpos
            .saturating_sub(RENDER_COPPER_WAIT_HPOS_FB0)
            .saturating_mul(4)
            .min(RENDER_FRAMEBUFFER_WIDTH as u32) as i32;
        let x_stop = new_hpos
            .saturating_sub(RENDER_COPPER_WAIT_HPOS_FB0)
            .saturating_mul(4)
            .min(RENDER_FRAMEBUFFER_WIDTH as u32) as i32;
        if x_start >= x_stop {
            return;
        }
        self.ensure_current_frame_sprite_collision_sources_for_y(fb_y, vpos);
        let has_overlapping_source_pair = {
            let sources = self.current_frame_sprite_collision_sources[fb_y]
                .as_deref()
                .unwrap_or(&[]);
            live_sprite_sources_have_group_pair_overlap(sources, x_start, x_stop)
        };
        if !has_overlapping_source_pair {
            return;
        }
        self.ensure_current_collision_control_index();
        let sources = self.current_frame_sprite_collision_sources[fb_y]
            .as_deref()
            .unwrap_or(&[]);
        let started = VideoPipelineStats::probe_timing_sample(
            &mut self.video_pipeline_stats.collision_probes,
            VIDEO_COLLISION_TIMING_SAMPLE_RATE,
        );
        let current_control = LiveCollisionControl::from_current(
            self.denise.bplcon0,
            self.denise.bplcon1,
            self.denise.bplcon3,
            self.denise.clxcon,
            self.denise.diwstrt,
            self.denise.diwstop,
            self.effective_diwhigh(),
            self.denise.ddfstrt,
            self.denise.bpldat,
        );
        let frame_base = self.current_frame_render_base;
        let control_index = self.current_frame_collision_control_index.as_ref().unwrap();
        let control_replay = LiveCollisionLineReplay::from_index(
            current_control,
            frame_base,
            control_index,
            vpos as i32,
        );
        let sprite_display_enable_x =
            self.current_frame_sprite_display_enable_x_by_y[fb_y].map(|x| x as i32);
        let clxdat = live_sprite_sprite_collision_bits(
            sources,
            &control_replay,
            vpos as i32,
            x_start,
            x_stop,
            sprite_display_enable_x,
        );
        self.denise.or_clxdat(clxdat);
        self.record_live_collision_timing(
            (x_stop - x_start) as u64,
            control_replay.segment_count(),
            false,
            started.map(|started| (started.elapsed(), VIDEO_COLLISION_TIMING_SAMPLE_RATE)),
        );
    }

    fn accumulate_live_playfield_collisions_if_due(
        &mut self,
        vpos: u32,
        old_hpos: u32,
        new_hpos: u32,
    ) {
        let visible_start_vpos = self.current_frame_visible_start_vpos;
        if new_hpos <= RENDER_COPPER_WAIT_HPOS_FB0 {
            return;
        }
        let Some(fb_y) = visible_framebuffer_y(
            vpos,
            visible_start_vpos,
            self.current_frame_geometry.visible_lines,
        ) else {
            return;
        };
        let x_start = old_hpos
            .saturating_sub(RENDER_COPPER_WAIT_HPOS_FB0)
            .saturating_mul(4)
            .min(RENDER_FRAMEBUFFER_WIDTH as u32) as i32;
        let x_stop = new_hpos
            .saturating_sub(RENDER_COPPER_WAIT_HPOS_FB0)
            .saturating_mul(4)
            .min(RENDER_FRAMEBUFFER_WIDTH as u32) as i32;
        if x_start >= x_stop {
            return;
        }
        if self.current_frame_bitplane_rows[fb_y].is_none() {
            return;
        }
        self.ensure_current_frame_sprite_collision_sources_for_y(fb_y, vpos);
        let has_overlapping_sprite_source = {
            let sprite_sources = self.current_frame_sprite_collision_sources[fb_y]
                .as_deref()
                .unwrap_or(&[]);
            sprite_sources
                .iter()
                .any(|source| live_sprite_source_may_overlap_x_range(source, x_start, x_stop))
        };
        let needs_dual_playfield_collision =
            self.live_playfield_collision_may_have_dual_playfield() && self.denise.clxdat & 1 == 0;
        if !has_overlapping_sprite_source && !needs_dual_playfield_collision {
            return;
        }
        self.ensure_current_collision_control_index();
        let sprite_sources = self.current_frame_sprite_collision_sources[fb_y]
            .as_deref()
            .unwrap_or(&[]);
        let started = VideoPipelineStats::probe_timing_sample(
            &mut self.video_pipeline_stats.collision_probes,
            VIDEO_COLLISION_TIMING_SAMPLE_RATE,
        );
        let current_control = LiveCollisionControl::from_current(
            self.denise.bplcon0,
            self.denise.bplcon1,
            self.denise.bplcon3,
            self.denise.clxcon,
            self.denise.diwstrt,
            self.denise.diwstop,
            self.effective_diwhigh(),
            self.denise.ddfstrt,
            self.denise.bpldat,
        );
        let frame_base = self.current_frame_render_base;
        let control_index = self.current_frame_collision_control_index.as_ref().unwrap();
        let control_replay = LiveCollisionLineReplay::from_index(
            current_control,
            frame_base,
            control_index,
            vpos as i32,
        );
        let needs_dual_playfield_collision = needs_dual_playfield_collision
            && control_replay.dual_playfield_in_range(x_start, x_stop);
        if !has_overlapping_sprite_source && !needs_dual_playfield_collision {
            return;
        }
        let sprite_display_enable_x =
            self.current_frame_sprite_display_enable_x_by_y[fb_y].map(|x| x as i32);
        let clxdat = {
            let row = self.current_frame_bitplane_rows[fb_y].as_ref().unwrap();
            let mut bits = 0;
            if needs_dual_playfield_collision {
                bits |= live_bitplane_collision_bits_in_range(
                    row,
                    &control_replay,
                    vpos as i32,
                    x_start,
                    x_stop,
                );
            }
            if has_overlapping_sprite_source {
                bits |= live_sprite_playfield_collision_bits_in_range(
                    row,
                    sprite_sources,
                    &control_replay,
                    &control_replay,
                    vpos as i32,
                    x_start,
                    x_stop,
                    sprite_display_enable_x,
                );
            }
            bits
        };
        self.denise.or_clxdat(clxdat);
        self.record_live_collision_timing(
            (x_stop - x_start) as u64,
            control_replay.segment_count(),
            false,
            started.map(|started| (started.elapsed(), VIDEO_COLLISION_TIMING_SAMPLE_RATE)),
        );
    }

    fn accumulate_live_manual_bpl_collisions_if_due(
        &mut self,
        vpos: u32,
        old_hpos: u32,
        new_hpos: u32,
    ) {
        let visible_start_vpos = self.current_frame_visible_start_vpos;
        if new_hpos <= RENDER_COPPER_WAIT_HPOS_FB0
            || self.current_frame_collision_bpldat_events.is_empty()
        {
            return;
        }
        let Some(fb_y) = visible_framebuffer_y(
            vpos,
            visible_start_vpos,
            self.current_frame_geometry.visible_lines,
        ) else {
            return;
        };
        let x_start = old_hpos
            .saturating_sub(RENDER_COPPER_WAIT_HPOS_FB0)
            .saturating_mul(4)
            .min(RENDER_FRAMEBUFFER_WIDTH as u32) as i32;
        let x_stop = new_hpos
            .saturating_sub(RENDER_COPPER_WAIT_HPOS_FB0)
            .saturating_mul(4)
            .min(RENDER_FRAMEBUFFER_WIDTH as u32) as i32;
        if x_start >= x_stop {
            return;
        }
        let current_line_sprite_lines_empty = self.current_frame_sprite_lines_by_y[fb_y].is_empty();
        if current_line_sprite_lines_empty
            && self.current_frame_collision_sprite_events.is_empty()
            && (self.denise.clxdat & 1 != 0
                || !self.live_playfield_collision_may_have_dual_playfield())
        {
            return;
        }
        let started = VideoPipelineStats::probe_timing_sample(
            &mut self.video_pipeline_stats.collision_probes,
            VIDEO_COLLISION_TIMING_SAMPLE_RATE,
        );
        let current_control = LiveCollisionControl::from_current(
            self.denise.bplcon0,
            self.denise.bplcon1,
            self.denise.bplcon3,
            self.denise.clxcon,
            self.denise.diwstrt,
            self.denise.diwstop,
            self.effective_diwhigh(),
            self.denise.ddfstrt,
            self.denise.bpldat,
        );
        let frame_base = self.current_frame_render_base;
        self.ensure_current_collision_control_index();
        let control_index = self.current_frame_collision_control_index.as_ref().unwrap();
        let control_replay = LiveCollisionLineReplay::from_index(
            current_control,
            frame_base,
            control_index,
            vpos as i32,
        );
        if current_line_sprite_lines_empty
            && self.current_frame_collision_sprite_events.is_empty()
            && !control_replay.dual_playfield_in_range(x_start, x_stop)
        {
            return;
        }
        self.ensure_current_collision_bpldat_index();
        self.ensure_current_collision_sprite_index();
        let bpldat_index = self.current_frame_collision_bpldat_index.as_ref().unwrap();
        let sprite_index = self.current_frame_collision_sprite_index.as_ref().unwrap();
        let sprite_display_enable_x =
            self.current_frame_sprite_display_enable_x_by_y[fb_y].map(|x| x as i32);
        let clxdat = live_manual_bpl_collision_bits_in_range(
            frame_base,
            bpldat_index,
            sprite_index,
            &control_replay,
            &self.current_frame_sprite_lines_by_y[fb_y],
            vpos as i32,
            x_start,
            x_stop,
            sprite_display_enable_x,
        );
        self.denise.or_clxdat(clxdat);
        self.record_live_collision_timing(
            (x_stop - x_start) as u64,
            control_replay.segment_count(),
            false,
            started.map(|started| (started.elapsed(), VIDEO_COLLISION_TIMING_SAMPLE_RATE)),
        );
    }

    fn accumulate_live_manual_sprite_collisions_if_due(
        &mut self,
        vpos: u32,
        old_hpos: u32,
        new_hpos: u32,
    ) {
        let visible_start_vpos = self.current_frame_visible_start_vpos;
        if new_hpos <= RENDER_COPPER_WAIT_HPOS_FB0
            || self.current_frame_collision_sprite_events.is_empty()
        {
            return;
        }
        let Some(fb_y) = visible_framebuffer_y(
            vpos,
            visible_start_vpos,
            self.current_frame_geometry.visible_lines,
        ) else {
            return;
        };
        let x_start = old_hpos
            .saturating_sub(RENDER_COPPER_WAIT_HPOS_FB0)
            .saturating_mul(4)
            .min(RENDER_FRAMEBUFFER_WIDTH as u32) as i32;
        let x_stop = new_hpos
            .saturating_sub(RENDER_COPPER_WAIT_HPOS_FB0)
            .saturating_mul(4)
            .min(RENDER_FRAMEBUFFER_WIDTH as u32) as i32;
        if x_start >= x_stop {
            return;
        }
        let started = VideoPipelineStats::probe_timing_sample(
            &mut self.video_pipeline_stats.collision_probes,
            VIDEO_COLLISION_TIMING_SAMPLE_RATE,
        );
        self.ensure_current_collision_control_index();
        self.ensure_current_collision_sprite_index();
        let row = self.current_frame_bitplane_rows[fb_y].as_ref();
        let current_control = LiveCollisionControl::from_current(
            self.denise.bplcon0,
            self.denise.bplcon1,
            self.denise.bplcon3,
            self.denise.clxcon,
            self.denise.diwstrt,
            self.denise.diwstop,
            self.effective_diwhigh(),
            self.denise.ddfstrt,
            self.denise.bpldat,
        );
        let frame_base = self.current_frame_render_base;
        let control_index = self.current_frame_collision_control_index.as_ref().unwrap();
        let control_replay = LiveCollisionLineReplay::from_index(
            current_control,
            frame_base,
            control_index,
            vpos as i32,
        );
        let sprite_index = self.current_frame_collision_sprite_index.as_ref().unwrap();
        let sprite_display_enable_x =
            self.current_frame_sprite_display_enable_x_by_y[fb_y].map(|x| x as i32);
        let mut clxdat = live_manual_sprite_sprite_collision_bits_in_range(
            frame_base,
            sprite_index,
            &control_replay,
            vpos as i32,
            x_start,
            x_stop,
            sprite_display_enable_x,
        );
        if let Some(row) = row {
            clxdat |= live_manual_sprite_playfield_collision_bits_in_range(
                row,
                frame_base,
                sprite_index,
                &control_replay,
                &control_replay,
                vpos as i32,
                x_start,
                x_stop,
                sprite_display_enable_x,
            );
        }
        self.denise.or_clxdat(clxdat);
        self.record_live_collision_timing(
            (x_stop - x_start) as u64,
            control_replay.segment_count(),
            false,
            started.map(|started| (started.elapsed(), VIDEO_COLLISION_TIMING_SAMPLE_RATE)),
        );
    }

    fn process_chip_bus_owner(&mut self, owner: ChipBusOwner) {
        match owner {
            // The Copper is stepped directly in advance_one_chip_bus_quantum_limited
            // via step_copper_eligible_slot (its cadence needs per-color-clock
            // gap accounting), so it never reaches here.
            ChipBusOwner::Blitter => {
                if self.blitter.tick_scheduled_slot(&mut self.mem.chip_ram) {
                    self.latch_blitter_completion("bus_slot");
                }
            }
            ChipBusOwner::Audio => self.step_audio_dma_slot(),
            ChipBusOwner::Copper
            | ChipBusOwner::Refresh
            | ChipBusOwner::Bitplane
            | ChipBusOwner::Sprite
            | ChipBusOwner::Disk
            | ChipBusOwner::Cpu
            | ChipBusOwner::Idle => {}
        }
    }

    fn step_audio_dma_slot(&mut self) {
        self.flush_audio();
        let Some(channel) = Self::audio_dma_channel_at(self.agnus.hpos) else {
            return;
        };
        let Some(request) = self.paula.audio_dma_request(channel) else {
            return;
        };
        let word = self.read_chip_word_for_audio_dma(request.address);
        self.data_bus = word;
        let irq = self.paula.grant_audio_dma(channel, word);
        self.paula.latch_interrupt_sources(irq);
    }

    fn copper_dma_enabled(&self) -> bool {
        self.agnus.dmacon & (DMACON_DMAEN | DMACON_COPEN) == (DMACON_DMAEN | DMACON_COPEN)
    }

    fn copper_can_write_custom(&self, off: u16) -> bool {
        let off = off & 0x01FE;
        if off <= 0x03E {
            return !matches!(self.agnus.revision(), AgnusRevision::Ocs)
                && self.agnus.copper_danger_enabled();
        }
        // COPJMP1/2 are handled as Copper control-flow strobes above.
        if (0x040..=0x07E).contains(&off) {
            return self.agnus.copper_danger_enabled();
        }
        true
    }

    fn blitter_dma_enabled(&self) -> bool {
        self.agnus.dmacon & (DMACON_DMAEN | DMACON_BLTEN) == (DMACON_DMAEN | DMACON_BLTEN)
    }

    fn blitter_slowdown_counter_enabled(&self) -> bool {
        self.blitter.busy && self.blitter_dma_enabled() && self.agnus.dmacon & DMACON_BLTPRI == 0
    }

    fn blitter_yields_to_waiting_cpu(&self) -> bool {
        self.blitter_slowdown_counter_enabled()
            && self.blitter_slowdown_cpu_misses >= exp_miss_limit()
    }

    fn cpu_can_use_current_slot(&self) -> bool {
        matches!(
            self.scheduled_dma_owner(true),
            ChipBusOwner::Cpu | ChipBusOwner::Idle
        )
    }

    fn scheduled_dma_owner(&self, for_cpu: bool) -> ChipBusOwner {
        if let Some(owner) = self.fixed_dma_owner_at(self.agnus.vpos, self.agnus.hpos) {
            return owner;
        }
        if self.agnus.dmacon & DMACON_DMAEN == 0 {
            return ChipBusOwner::Idle;
        }
        // The Copper claims the slot only on its access-parity color clock; on
        // the odd (idle-half) color clocks it yields to the blitter/CPU, which
        // is how the OCS Copper's 4-color-clock MOVE leaves alternate cycles
        // free. The cadence is locked to the beam, so a dense MOVE list lands at
        // the same hpos on every line.
        if self.copper_ready_for_slot() && Copper::hpos_is_access_cycle(self.agnus.hpos) {
            return ChipBusOwner::Copper;
        }
        if self.blitter.busy && self.blitter_dma_enabled() {
            // Idle blit pipeline cycles (the "-" slots in the HRM cycle diagrams,
            // e.g. the first empty D phase after source fetches or a line blit's
            // internal Bresenham cycles) never claim the bus: per the HRM they
            // are available to the other DMA channels or the 68000, and MiniMig
            // only asserts the blitter's dma_req on channel-access states. The
            // pipeline still advances through them -- see
            // advance_one_chip_bus_quantum_limited.
            if !self.blitter.current_slot_needs_bus() {
                return ChipBusOwner::Idle;
            }
            // With BLTPRI=0 the blitter is "nice" but still holds the chip bus:
            // it yields to the CPU only once the CPU has been starved for
            // BLITTER_SLOWDOWN_CPU_MISS_LIMIT cycles, not on every even slot-pair.
            // Granting the CPU a regular alternate slot here used to split the
            // bus ~1:1, but real OCS gives a busy blitter ~2:1 over a BLITWAIT-ing
            // CPU (cross-emulator DMA accounting on a blitter-heavy frame:
            // blitter 34892, CPU 17882). The old even/odd grant starved the
            // blitter so big fills overran the frame and flickered.
            if for_cpu && self.blitter_yields_to_waiting_cpu() {
                return ChipBusOwner::Idle;
            }
            return ChipBusOwner::Blitter;
        }
        ChipBusOwner::Idle
    }

    fn fixed_dma_owner_at(&self, vpos: u32, hpos: u32) -> Option<ChipBusOwner> {
        if Self::refresh_slot_active_at(hpos) {
            return Some(ChipBusOwner::Refresh);
        }
        if self.agnus.dmacon & DMACON_DMAEN == 0 {
            return self
                .bitplane_slot_active_at(vpos, hpos)
                .then_some(ChipBusOwner::Bitplane);
        }
        if self.disk_slot_active_at(hpos) {
            return Some(ChipBusOwner::Disk);
        }
        if self.audio_slot_active_at(hpos) {
            return Some(ChipBusOwner::Audio);
        }
        if self.sprite_slot_active_at(hpos) {
            return Some(ChipBusOwner::Sprite);
        }
        if self.bitplane_slot_active_at(vpos, hpos) {
            return Some(ChipBusOwner::Bitplane);
        }
        None
    }

    /// Predict the color clocks until the pending blit completes by walking its
    /// remaining slot access pattern against the beam. Access slots (mask bit
    /// set) consume the next color clock the blitter can win (not fixed DMA,
    /// not Copper); idle pipeline slots consume exactly one color clock
    /// unconditionally, matching the live arbitration where they never claim
    /// the bus and can never be stalled.
    fn cck_until_blitter_completes(&self, access_mask: u64, slot_count: u32) -> Option<u32> {
        if slot_count == 0 || slot_count > BLITTER_DEADLINE_SLOT_SCAN_LIMIT {
            return None;
        }

        let mut copper = self.copper.clone();
        let mut slot_idx = 0u32;
        let mut elapsed = 0u32;
        let mut hpos = self.agnus.hpos;
        let mut vpos = self.agnus.vpos;
        let mut lol = self.agnus.lol;
        let mut pending_copper_frame_start = self.pending_copper_frame_start;
        let frame_lines = self.agnus.current_frame_lines();
        let max_scan_cck = frame_lines.saturating_mul(NTSC_LONG_COLORCLOCKS_PER_LINE);

        while elapsed < max_scan_cck {
            if let Some(cop1lc) = pending_copper_frame_start
                .filter(|_| vpos >= copper_frame_start_vpos(self.agnus.video_standard()))
            {
                copper.frame_start(cop1lc);
                pending_copper_frame_start = None;
            }
            let line_cck = self.agnus.line_cck_for(lol);
            let quantum = next_chip_bus_quantum_at(hpos, line_cck);

            // Mirror the live path's per-color-clock Copper cadence on the
            // clone (stepped on every non-fixed-DMA color clock) so the
            // blitter only claims the color clocks the Copper leaves free
            // (its idle halves, sleeping WAITs, gaps). The shared
            // step_eligible_slot keeps prediction and execution from
            // drifting apart.
            let slot_grantable =
                quantum >= CHIP_BUS_SLOT_CCK && self.fixed_dma_owner_at(vpos, hpos).is_none();
            let copper_blocks = if !slot_grantable {
                // Fixed DMA owns this color clock, but the Copper's WAIT/SKIP
                // comparator keeps running (mirrors the live path's
                // comparator-only advance with allow_fetch=false).
                if quantum >= CHIP_BUS_SLOT_CCK
                    && pending_copper_frame_start.is_none()
                    && self.copper_dma_enabled()
                    && hpos != COPPER_BUS_LOCKOUT_HPOS
                {
                    let _ = copper.step_eligible_slot(
                        &self.mem.chip_ram,
                        vpos,
                        hpos,
                        self.blitter.busy,
                        self.agnus.cop1lc,
                        self.agnus.cop2lc,
                        false,
                        line_cck,
                    );
                }
                false
            } else if pending_copper_frame_start.is_some() {
                true
            } else if !self.copper_dma_enabled() {
                false
            } else if hpos == COPPER_BUS_LOCKOUT_HPOS {
                copper.is_running()
            } else {
                !matches!(
                    copper.step_eligible_slot(
                        &self.mem.chip_ram,
                        vpos,
                        hpos,
                        self.blitter.busy,
                        self.agnus.cop1lc,
                        self.agnus.cop2lc,
                        true,
                        line_cck,
                    ),
                    CopperSlotAction::Idle
                )
            };

            let slot_needs_bus = access_mask & (1u64 << slot_idx) != 0;
            let slot_consumed = if slot_needs_bus {
                slot_grantable && !copper_blocks
            } else {
                // Idle pipeline cycle: bus-free, but still stalled by fixed DMA
                // slots just like the live path.
                slot_grantable
            };
            if slot_consumed {
                slot_idx += 1;
                if slot_idx == slot_count {
                    return Some(elapsed.saturating_add(quantum).max(1));
                }
            }

            elapsed = elapsed.saturating_add(quantum);
            hpos = hpos.saturating_add(quantum);
            if hpos >= line_cck {
                hpos = 0;
                vpos = vpos.saturating_add(1);
                if self.agnus.long_line_toggles() {
                    lol = !lol;
                }
                if vpos >= frame_lines {
                    vpos = 0;
                }
            }
        }

        None
    }

    fn refresh_slot_active_at(hpos: u32) -> bool {
        // The OCS Agnus does 4 memory-refresh cycles per line, on ODD color
        // clocks (WinUAE: REFRESH_FIRST_HPOS=3, slots every other cck; HRM DMA
        // time-slot chart: refresh/disk/audio/sprite all sit on the alternate
        // slots). The parity matters: the Copper's bus fetches use the EVEN
        // color clocks (WinUAE COPPER_CYCLE_POLARITY), so on real hardware
        // refresh NEVER blocks a Copper fetch. Putting refresh on even slots
        // (a misreading of MiniMig's 2x-hpos numbering) delayed Copper MOVE
        // streams at the start of every line by ~8 cck, which broke demos that
        // rely on a post-WAIT register burst completing before DDFSTRT; if a
        // BPLCON0 plane-count switch lands after the line's fetches begin, the
        // planes are misaligned.
        //
        // Positions 1/3/5/7 sit just before Copperline's disk slots (9/B/D) and
        // audio slots (F/11/13/15), mirroring the HRM chart's contiguous
        // odd-slot fixed-DMA band.
        matches!(hpos, 0x001 | 0x003 | 0x005 | 0x007)
    }

    fn disk_slot_active_at(&self, hpos: u32) -> bool {
        // Standard OCS disk DMA reserves three slots per line (the actual
        // floppy->chip-RAM transfer is rate-based in `floppy.tick`, so this
        // reservation only models the CPU/blitter stall). The previous code
        // reserved a six-slot band (0x009-0x00E), double the hardware count,
        // which over-stalled the CPU during disk loading. Copperline does not model
        // the ECS "fast disk" slot expansion, so three is correct here.
        // Diagnostic builds can remove disk DMA CPU/blitter stalls entirely
        // for timing experiments. Normal builds always reserve the slots.
        if no_disk_stall() {
            return false;
        }
        self.agnus.dmacon & DMACON_DSKEN != 0
            && self.floppy.dma_active(self.agnus.dmacon)
            && matches!(hpos, 0x009 | 0x00B | 0x00D)
    }

    fn audio_slot_active_at(&self, hpos: u32) -> bool {
        // Each of the four audio channels has one fixed DMA slot (hpos 0x00F,
        // 0x011, 0x013, 0x015). On real Paula a channel only *uses* that slot
        // (stalling the CPU/blitter) on the line where its period counter
        // actually requests a word -- roughly once per 2*AUDxPER cck, which at
        // music periods is well under once per line. Reserve the slot only when
        // the channel has a pending DMA request, the same `dma_request` flag that
        // gates the actual fetch in `step_audio_dma_slot`; the flag is current
        // here because `flush_audio_before_audio_dma_slot` advances Paula to this
        // hpos before owner selection. Previously the slot was reserved every
        // line for every enabled channel (~1252 cck/frame), a ~3-4x
        // over-reservation that stole slots from the blitter on idle audio lines.
        if self.agnus.dmacon & DMACON_DMAEN == 0 {
            return false;
        }
        match Self::audio_dma_channel_at(hpos) {
            Some(channel) => {
                self.agnus.dmacon & (1 << channel) != 0
                    && self.paula.audio_dma_request(channel).is_some()
            }
            None => false,
        }
    }

    fn audio_dma_channel_at(hpos: u32) -> Option<usize> {
        match hpos {
            0x00F => Some(0),
            0x011 => Some(1),
            0x013 => Some(2),
            0x015 => Some(3),
            _ => None,
        }
    }

    fn flush_audio_before_audio_dma_slot(&mut self) {
        if Self::audio_dma_channel_at(self.agnus.hpos).is_some() {
            self.flush_audio();
        }
    }

    fn read_chip_word_for_audio_dma(&self, address: u32) -> u16 {
        if self.mem.chip_ram.is_empty() {
            return 0;
        }
        let off = (address as usize) % self.mem.chip_ram.len();
        let hi = self.mem.chip_ram[off] as u16;
        let lo = self.mem.chip_ram[(off + 1) % self.mem.chip_ram.len()] as u16;
        (hi << 8) | lo
    }

    fn sprite_slot_active_at(&self, hpos: u32) -> bool {
        // Real OCS sprite DMA fetches only on lines where a sprite is actually
        // active (within its vstart..vstop), not on every line. The reserved
        // slots map to sprite pairs by `SPRITE_DMA_PAIR_CAPTURE_HPOS`
        // (0x18->sprites 0/1, 0x20->2/3, 0x28->4/5, 0x30->6/7), so reserve a
        // pair's slot only when one of its sprites is fetching data this line --
        // gating on the same `data_dma_active` the renderer uses, so the bus
        // model and the captured image agree. Parked/off-screen sprites free
        // their slot for the CPU/blitter; previously they were reserved
        // unconditionally whenever SPREN was on (~2504 cck/frame of phantom DMA
        // stolen from the blitter).
        if self.agnus.dmacon & DMACON_SPREN == 0 {
            return false;
        }
        // Sprite DMA slots sit on ODD color clocks (same parity as refresh/
        // disk/audio -- the HRM chart's fixed-DMA band), so they never block
        // the Copper's even-clock fetches. Each active sprite pair reserves
        // the two odd slots of its 8-cck band (0x19/0x1B, 0x21/0x23, ...).
        if !(0x019..=0x037).contains(&hpos) || hpos & 1 == 0 {
            return false;
        }
        let rel = hpos - 0x019;
        if rel % 8 >= 4 {
            return false;
        }
        let pair = (rel / 8) as usize;
        if pair >= 4 {
            return false;
        }
        let first = pair * 2;
        self.display_dma_sprite_state[first].data_dma_active
            || self.display_dma_sprite_state[first + 1].data_dma_active
    }

    fn record_bitplane_dmacon_write(&mut self, previous: u16) {
        self.bitplane_dmacon_delay = Some(BitplaneDmaconDelay {
            previous,
            changed_at_cck: self.emulated_cck,
        });
    }

    fn effective_bitplane_dmacon(&self) -> u16 {
        self.effective_bitplane_dmacon_at(self.emulated_cck)
    }

    fn effective_bitplane_dmacon_at(&self, emulated_cck: u64) -> u16 {
        if let Some(delay) = self.bitplane_dmacon_delay {
            if emulated_cck.saturating_sub(delay.changed_at_cck) < 2 {
                return delay.previous;
            }
        }
        self.agnus.dmacon
    }

    fn record_bitplane_bplcon0_write(&mut self, previous: u16) {
        self.bitplane_bplcon0_delay = Some(BitplaneBplcon0Delay {
            previous,
            changed_at_cck: self.emulated_cck,
        });
    }

    fn effective_bitplane_bplcon0(&self) -> u16 {
        self.effective_bitplane_bplcon0_at(self.emulated_cck)
    }

    fn effective_bitplane_bplcon0_at(&self, emulated_cck: u64) -> u16 {
        if let Some(delay) = self.bitplane_bplcon0_delay {
            if emulated_cck.saturating_sub(delay.changed_at_cck) < 3 {
                return delay.previous;
            }
        }
        self.denise.bplcon0
    }

    // Agnus latches the bitplane plane count / resolution at the start of each
    // DDF fetch block rather than continuously. A BPLCON0 write at or before a
    // block's first cycle configures that block's fetch; a write that lands
    // mid-block only affects the next block. This is the cycle-accurate version
    // of the coarse three-CCK `effective_bitplane_bplcon0_at` delay: it lets a
    // write exactly at DDFSTRT enable the earliest-slot plane on the same line
    // (e.g. lores plane 4, which fetches first), while still deferring a write
    // that arrives after the block has begun.
    fn bitplane_bplcon0_for_block(&self, block_start_cck: i128) -> u16 {
        if let Some(delay) = self.bitplane_bplcon0_delay {
            if i128::from(delay.changed_at_cck) > block_start_cck {
                return delay.previous;
            }
        }
        self.denise.bplcon0
    }

    fn record_ddfstrt_write_match_miss(&mut self, ddfstrt: u16) {
        let bplcon0 = self.effective_bitplane_bplcon0();
        let ddfstart = u32::from(effective_ddf_hpos(bplcon0, ddfstrt));
        if ddfstart != 0 && ddfstart == self.agnus.hpos {
            self.bitplane_ddfstart_miss = Some(BitplaneDdfStartMiss {
                vpos: self.agnus.vpos,
                ddfstart,
            });
        }
    }

    fn bitplane_ddfstart_missed_on_line(&self, vpos: u32, ddfstart: u32) -> bool {
        self.bitplane_ddfstart_miss
            .is_some_and(|miss| miss.vpos == vpos && miss.ddfstart == ddfstart)
    }

    fn bitplane_slot_active_at(&self, vpos: u32, hpos: u32) -> bool {
        let mut bplcon0 = self.effective_bitplane_bplcon0();
        let mut plan = self.bitplane_slot_plan_for_bplcon0(bplcon0);
        if plan.is_none() {
            if let Some(delay) = self.bitplane_bplcon0_delay {
                bplcon0 = delay.previous;
                plan = self.bitplane_slot_plan_for_bplcon0(bplcon0);
            }
        }
        let Some(mut plan) = plan else {
            return false;
        };
        // Bitplane DMA only runs inside the vertical display window (set at
        // DIWSTRT.V, cleared at DIWSTOP.V), so the top-border and vertical-
        // blank lines are free for the blitter/CPU. Without this gate the
        // arbiter reserved bitplane slots on all 313 lines, starving a busy
        // blitter on the ~28 off-window lines. Mirrors the capture path's
        // display_window_contains_vpos check so the two cannot drift.
        if !display_window_contains_vpos(
            self.denise.diwstrt,
            self.denise.diwstop,
            self.effective_diwhigh(),
            vpos,
        ) {
            return false;
        }
        if self.bitplane_ddfstart_missed_on_line(vpos, plan.start) {
            return false;
        }
        if hpos >= plan.start {
            for _ in 0..2 {
                let block_span = if plan.hires_like {
                    plan.period
                } else {
                    plan.unit
                }
                .max(1);
                let rel = hpos - plan.start;
                let block_start_hpos = plan.start + (rel / block_span) * block_span;
                let block_start_cck = i128::from(self.emulated_cck)
                    - i128::from(hpos.saturating_sub(block_start_hpos));
                let block_bplcon0 = self.bitplane_bplcon0_for_block(block_start_cck);
                if block_bplcon0 == bplcon0 {
                    break;
                }
                bplcon0 = block_bplcon0;
                let Some(block_plan) = self.bitplane_slot_plan_for_bplcon0(bplcon0) else {
                    return false;
                };
                plan = block_plan;
                if hpos < plan.start || self.bitplane_ddfstart_missed_on_line(vpos, plan.start) {
                    return false;
                }
            }
        }
        // Cheap hpos rejection first via the memoized slot bitmask (which also
        // encodes the start/last_fetch_hpos bounds). The vpos gates below only
        // matter on color clocks that are actually bitplane slots, so testing
        // the pattern first lets the off-slot majority skip them entirely.
        let is_slot = if hpos < SLOT_MASK_BITS {
            plan.slot_mask[(hpos / 64) as usize] & (1u64 << (hpos % 64)) != 0
        } else {
            // Programmable line wider than the bitmask: fall back to the math.
            Self::plan_slot_at(&plan, hpos)
        };
        if !is_slot {
            return false;
        }
        true
    }

    /// Whether `hpos` is a bitplane fetch slot for `plan`, from the fetch
    /// cadence alone (vpos-independent). This is the exact per-color-clock math
    /// that `bitplane_slot_active_at` used inline; it is now memoized into
    /// `BitplaneSlotPlan::slot_mask` and kept here for that precompute and for
    /// the wide-programmable-line fallback.
    fn plan_slot_at(plan: &BitplaneSlotPlan, hpos: u32) -> bool {
        if hpos < plan.start || hpos > plan.last_fetch_hpos {
            return false;
        }
        let rel = hpos - plan.start;
        if plan.hires_like {
            return rel.is_multiple_of(plan.period)
                && (rel / plan.period) * plan.quantum < plan.words_per_row;
        }
        if (rel / plan.unit) * plan.quantum >= plan.words_per_row {
            return false;
        }
        let unit_off = rel % plan.unit;
        if unit_off >= 8 {
            return false;
        }
        let order = unit_off;
        plan.order_mask & (1u8 << order) != 0
    }

    fn bitplane_slot_plan_for_bplcon0(&self, bplcon0: u16) -> Option<BitplaneSlotPlan> {
        let dmacon = self.effective_bitplane_dmacon();
        let key = BitplaneSlotKey {
            bplen: dmacon & (DMACON_DMAEN | DMACON_BPLEN) == (DMACON_DMAEN | DMACON_BPLEN),
            bplcon0,
            ddfstrt: self.denise.ddfstrt,
            ddfstop: self.denise.ddfstop,
            fmode: self.agnus.fmode(),
            harddis: self.harddis_active(),
        };
        if let Some((cached_key, plan)) = self.bitplane_slot_plan_cache.get() {
            if cached_key == key {
                return plan;
            }
        }
        let plan = self.compute_bitplane_slot_plan(&key);
        self.bitplane_slot_plan_cache.set(Some((key, plan)));
        plan
    }

    fn compute_bitplane_slot_plan(&self, key: &BitplaneSlotKey) -> Option<BitplaneSlotPlan> {
        if !key.bplen {
            return None;
        }
        let bplcon0 = key.bplcon0;
        let nplanes = BitplaneMode::from_bplcon0(bplcon0, self.aga_enabled()).dma_planes();
        if nplanes == 0 {
            return None;
        }
        let (start, stop) = effective_ddf_window(
            self.agnus.revision(),
            bplcon0,
            key.ddfstrt,
            key.ddfstop,
            key.harddis,
        )?;
        let start = u32::from(start);
        // Mirrors the capture loop's FMODE cadence so arbitration and
        // capture cannot drift: wider fetches reserve fewer slots.
        let fmode = key.fmode;
        let quantum = bitplane_fetch_quantum(fmode);
        let period = bitplane_fetch_period(bplcon0, fmode);
        let unit = bitplane_fetch_unit(bplcon0, fmode);
        // The DDFSTRT comparator starts the sequencer. Wide FMODE increases
        // the unit length between fetch groups; it does not move the first
        // group back to an absolute unit boundary.
        let start = u32::from(crate::chipset::agnus::anchor_bitplane_fetch_start(
            start as u16,
            unit,
        ));
        // The sequencer completes whole units from the DDF start:
        // a DDFSTOP inside a unit extends the fetch to the end of the unit
        // starting at-or-after it (see agnus::bitplane_fetch_blocks), so the
        // last slot can land past DDFSTOP.
        let blocks =
            crate::chipset::agnus::bitplane_fetch_blocks(u32::from(stop) - start, unit) as u32;
        let last_fetch_hpos = start + blocks * unit - 1;
        let words_per_row = bitplane_words_per_row(
            self.agnus.revision(),
            bplcon0,
            fmode,
            key.ddfstrt,
            key.ddfstop,
            key.harddis,
        ) as u32;
        let mut order_mask = 0u8;
        for plane in 0..nplanes.min(8) {
            order_mask |= 1u8 << bitplane_fetch_order(bplcon0, plane);
        }
        let mut plan = BitplaneSlotPlan {
            start,
            last_fetch_hpos,
            period,
            unit,
            quantum,
            words_per_row,
            hires_like: bitplane_hires(bplcon0) || bitplane_shres(bplcon0),
            order_mask,
            slot_mask: [0u64; 4],
        };
        // Memoize the vpos-independent fetch pattern so the per-color-clock
        // arbiter does a bit test instead of the div/mod in `plan_slot_at`.
        for hpos in plan.start..=plan.last_fetch_hpos.min(SLOT_MASK_BITS - 1) {
            if Self::plan_slot_at(&plan, hpos) {
                plan.slot_mask[(hpos / 64) as usize] |= 1u64 << (hpos % 64);
            }
        }
        Some(plan)
    }

    fn copper_ready_for_slot(&self) -> bool {
        if self.pending_copper_frame_start.is_some() {
            return false;
        }
        if !self.copper_dma_enabled() {
            return false;
        }
        self.copper.is_running()
    }

    fn copper_wait_free_cycle_available_at(&self, hpos: u32) -> bool {
        self.copper_comparator_runs_at(hpos)
            && self.fixed_dma_owner_at(self.agnus.vpos, hpos).is_none()
    }

    /// Whether the Copper's WAIT/SKIP comparator advances this color clock.
    /// Unlike a bus slot, the comparator does not arbitrate against fixed DMA:
    /// it keeps evaluating while bitplane/sprite/disk/audio DMA owns the bus.
    fn copper_comparator_runs_at(&self, hpos: u32) -> bool {
        self.pending_copper_frame_start.is_none()
            && self.copper_dma_enabled()
            && !self.copper_bus_lockout_active_at(hpos)
    }

    fn copper_bus_lockout_active_at(&self, hpos: u32) -> bool {
        hpos == COPPER_BUS_LOCKOUT_HPOS
    }

    fn cck_until_copper_wait_position(&self, wait: CopperWait) -> Option<u32> {
        if wait.is_end_of_list() {
            return None;
        }
        if wait.is_satisfied(self.agnus.vpos, self.agnus.hpos) {
            return Some(0);
        }

        let line_cck = self.agnus.current_line_cck();
        if wait.compare_mask() == 0xFFFE {
            return self.cck_until_full_mask_copper_wait(wait);
        }

        let mut vpos = self.agnus.vpos;
        let mut hpos = self.agnus.hpos;
        let frame_lines = self.agnus.current_frame_lines();
        let frame_cck = frame_lines.saturating_mul(line_cck);
        for delta in 1..=frame_cck {
            hpos += 1;
            if hpos >= line_cck {
                hpos = 0;
                vpos += 1;
                if vpos >= frame_lines {
                    vpos = 0;
                }
            }
            if wait.is_satisfied(vpos, hpos) {
                return Some(delta);
            }
        }
        None
    }

    fn cck_until_full_mask_copper_wait(&self, wait: CopperWait) -> Option<u32> {
        let target_h = (wait.position_bits() & 0x00FE) as u32;
        let frame_lines = self.agnus.current_frame_lines();

        for line_delta in 0..=frame_lines {
            let vpos = (self.agnus.vpos + line_delta) % frame_lines;
            let line_start_delta = if line_delta == 0 {
                0
            } else {
                self.agnus.cck_until_line_ticks(line_delta)?
            };
            let target_line_cck = self.line_cck_after_lines(line_delta);

            if line_delta == 0 {
                if target_h < target_line_cck
                    && self.agnus.hpos <= target_h
                    && wait.is_satisfied(vpos, target_h)
                {
                    return Some(target_h - self.agnus.hpos);
                }
            } else if wait.is_satisfied(vpos, 0) {
                return Some(line_start_delta);
            } else if target_h < target_line_cck && wait.is_satisfied(vpos, target_h) {
                return Some(line_start_delta + target_h);
            }
        }

        None
    }

    fn line_cck_after_lines(&self, line_delta: u32) -> u32 {
        if !self.agnus.long_line_toggles() {
            // PAL, LOLDIS, or programmable VARBEAMEN: every line is the same.
            return self.agnus.current_line_cck();
        }
        let target_lol = if line_delta.is_multiple_of(2) {
            self.agnus.lol
        } else {
            !self.agnus.lol
        };
        self.agnus.line_cck_for(target_lol)
    }

    fn next_chip_bus_quantum(&self) -> u32 {
        next_chip_bus_quantum_at(self.agnus.hpos, self.agnus.current_line_cck())
    }

    fn cck_until_pending_copper_frame_start(&self) -> Option<u32> {
        self.pending_copper_frame_start?;
        let target_vpos = copper_frame_start_vpos(self.agnus.video_standard());
        if self.agnus.vpos >= target_vpos {
            return Some(0);
        }
        self.agnus.cck_until_line_start(target_vpos)
    }

    fn start_pending_copper_frame_if_due(&mut self) {
        let Some(cop1lc) = self.pending_copper_frame_start else {
            return;
        };
        if self.agnus.vpos < copper_frame_start_vpos(self.agnus.video_standard()) {
            return;
        }
        self.pending_copper_frame_start = None;
        self.copper.frame_start(cop1lc);
    }

    fn record_slice_bus_advance(&mut self, cck: u32, tick: AgnusTick) {
        self.slice_bus_advanced_cck = self.slice_bus_advanced_cck.saturating_add(cck);
        add_agnus_tick(&mut self.slice_bus_tick, tick);
        if self.device_clock.realtime_enabled {
            self.device_clock.note_realtime_device_advance(cck);
        }
        // Defer the timed-device tick: accumulate these color clocks and apply
        // them in one batch at the next device observation or instruction
        // boundary (see `flush_timed_devices`). The chipset/beam advance above
        // already happened per color clock; only the CIA/serial/pots/audio/
        // floppy/Akiko devices, whose state the CPU can only observe through a
        // register read or an interrupt, are batched.
        self.pending_device_cck = self.pending_device_cck.saturating_add(cck);
        add_agnus_tick(&mut self.pending_device_tick, tick);
    }

    /// Apply any deferred timed-device color clocks (see `record_slice_bus_
    /// advance`). Called before every device-register observation (CIA, custom,
    /// and other peripheral reads/writes) and at each instruction boundary, so
    /// the CPU never sees a stale device or a late interrupt. Batching is exact:
    /// the CIA E-clock divider carries its remainder and every device tick is
    /// linear in the color-clock count.
    pub fn flush_timed_devices(&mut self) {
        let cck = std::mem::take(&mut self.pending_device_cck);
        if cck == 0 {
            return;
        }
        let tick = std::mem::take(&mut self.pending_device_tick);
        self.tick_timed_devices(cck, tick);
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

    /// Read a 16-bit big-endian word from whichever RAM/ROM region maps
    /// `addr` (chip, fast, slow, or ROM), for the debugger's memory dumps.
    /// Returns 0 for unmapped addresses.
    pub fn peek_word_any(&self, addr: u32) -> u16 {
        use crate::memory::{CHIP_RAM_BASE, ROM_BASE, SLOW_RAM_BASE};
        if let Some((board, off)) = self.mem.zorro.region_at(addr, 2) {
            let ram = self.mem.zorro.board_ram(board);
            return ((ram[off] as u16) << 8) | ram[off + 1] as u16;
        }
        let a = addr as usize;
        let regions: [(usize, &[u8]); 4] = [
            (CHIP_RAM_BASE as usize, &self.mem.chip_ram),
            (SLOW_RAM_BASE as usize, &self.mem.slow_ram),
            (ROM_BASE as usize, &self.mem.rom),
            (self.mem.extended_rom_base as usize, &self.mem.extended_rom),
        ];
        for (base, mem) in regions {
            if a >= base && a.wrapping_sub(base) + 1 < mem.len() {
                let off = a - base;
                return ((mem[off] as u16) << 8) | mem[off + 1] as u16;
            }
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

    fn begin_new_beam_frame(&mut self) {
        if crate::envcfg::flag("COPPERLINE_DIAG_DISPLAY") {
            let lines: Vec<usize> = self
                .dbg_bpl_cck
                .iter()
                .enumerate()
                .filter(|(_, &c)| c > 0)
                .map(|(v, _)| v)
                .collect();
            if !lines.is_empty() {
                let total: u32 = self.dbg_bpl_cck.iter().sum();
                let first = *lines.first().unwrap();
                let last = *lines.last().unwrap();
                let per_line: Vec<u32> = lines.iter().map(|&v| self.dbg_bpl_cck[v]).collect();
                let min = *per_line.iter().min().unwrap();
                let max = *per_line.iter().max().unwrap();
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
                log::info!(
                    "slotmap frame={} band v138..198 (hpos 0..=0xE2); codes R/B/S/D/A/C/L/P/.",
                    self.emulated_frames
                );
                for v in 138..=198usize {
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
                    let act = st.data_dma_active as u8;
                    let term = st.terminated as u8;
                    let (cvs, cve) = st.control.map(|c| (c.vstart, c.vstop)).unwrap_or((-1, -1));
                    s.push_str(&format!(
                        " s{i}[act={act} term={term} ctl_vs={cvs} ctl_ve={cve} nxt={:?}]",
                        st.next_ptr
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
        self.accumulate_live_collisions_to_frame_end();
        self.log_bus_accounting_frame();
        self.last_frame_render_base = Some(self.current_frame_render_base);
        self.last_frame_visible_start_vpos = self.current_frame_visible_start_vpos;
        self.last_frame_geometry = self.current_frame_geometry;
        self.last_frame_render_events = std::mem::take(&mut self.current_frame_render_events);
        self.current_frame_collision_events.clear();
        self.current_frame_collision_control_events.clear();
        self.current_frame_collision_bpldat_events.clear();
        self.current_frame_collision_sprite_events.clear();
        self.current_frame_collision_control_index = None;
        self.current_frame_collision_bpldat_index = None;
        self.current_frame_collision_sprite_index = None;
        self.last_frame_chip_ram_writes = std::mem::take(&mut self.current_frame_chip_ram_writes);
        self.last_frame_beam_top_palette = self.current_frame_beam_top_palette;
        self.last_frame_beam_top_palette_end = self.beam_top_palette;
        self.last_frame_beam_bottom_palette = self.beam_bottom_palette;
        self.last_frame_beam_bottom_palette_valid = self.beam_bottom_palette_valid;
        self.last_frame_beam_bottom_palette_events = self.beam_bottom_palette_events.clone();
        // Promote the just-finished frame's chip-RAM snapshot to `last` by
        // swapping buffers instead of copying 2 MB. `capture_current_frame_
        // display_start` already filled `current_frame_chip_ram` for any frame
        // that reached its display window, so move that buffer across and
        // recycle the old `last` buffer as the next `current`. A frame that
        // never displayed (no capture taken) has no meaningful snapshot, so
        // fall back to a live copy for the renderer's blank/border output.
        if self.current_frame_display_snapshot_taken
            && self.current_frame_chip_ram.len() == self.mem.chip_ram.len()
        {
            std::mem::swap(
                &mut self.last_frame_chip_ram,
                &mut self.current_frame_chip_ram,
            );
        } else {
            self.last_frame_chip_ram.clear();
            self.last_frame_chip_ram
                .extend_from_slice(&self.mem.chip_ram);
        }
        self.last_frame_bitplane_rows = std::mem::replace(
            &mut self.current_frame_bitplane_rows,
            empty_captured_bitplane_rows(),
        );
        self.last_frame_sprite_lines = std::mem::take(&mut self.current_frame_sprite_lines);
        self.last_frame_held_sprites = std::mem::take(&mut self.current_frame_held_sprites);
        clear_captured_sprite_lines_by_y(&mut self.current_frame_sprite_lines_by_y);
        self.current_frame_sprite_collision_sources = empty_sprite_collision_sources();
        self.last_frame_sprite_display_enable_x_by_y = std::mem::replace(
            &mut self.current_frame_sprite_display_enable_x_by_y,
            empty_sprite_display_enable_x_by_y(),
        );
        self.last_frame_sprite_dma_observed = self.current_frame_sprite_dma_observed;
        self.current_frame_sprite_dma_observed = false;
        // The next frame's snapshot is taken lazily at its display start
        // (`capture_current_frame_display_start`), which clears and refills
        // this buffer. Eagerly copying chip RAM here would just be overwritten,
        // so only clear it; a frame that never displays falls back to a live
        // copy at the next wrap (see the swap/extend above).
        self.current_frame_chip_ram.clear();
        self.current_frame_beam_top_palette = self.beam_top_palette;
        self.current_frame_display_snapshot_taken = false;
        self.current_frame_visible_start_vpos = RENDER_VISIBLE_START_VPOS;
        self.current_frame_render_base = self.capture_render_snapshot();
        self.current_frame_collision_may_have_dual_playfield =
            self.current_frame_render_base.bplcon0 & 0x0400 != 0;
        self.display_dma_bplpt = self.denise.bplpt;
        self.display_dma_sprpt = self.denise.sprpt;
        self.display_dma_sprite_state = [DisplaySpriteDmaState::default(); 8];
        self.display_dma_clipped_rows_advanced = false;
        self.lazy_collision_vpos = self.current_frame_visible_start_vpos;
        self.lazy_collision_hpos = RENDER_COPPER_WAIT_HPOS_FB0;
        self.agnus
            .update_interlace_long_frame(self.denise.bplcon0 & 0x0004 != 0);
        // The snapshot above was captured before the frame wrap toggled
        // LOF; record the settled value for the field about to render.
        self.current_frame_render_base.long_field = self.agnus.lof;
        self.current_frame_geometry = self.compute_frame_geometry();
        if self.current_frame_geometry.programmable {
            self.current_frame_visible_start_vpos = self.current_frame_geometry.visible_start_vpos;
            self.lazy_collision_vpos = self.current_frame_visible_start_vpos;
        }
        self.pending_copper_frame_start = Some(self.agnus.cop1lc);
        self.copper.stop();
    }

    pub(crate) fn record_cpu_chip_ram_write(&mut self, offset: usize, size: usize, value: u32) {
        self.current_frame_chip_ram_writes
            .push(BeamChipRamWrite::from_cpu_write(
                self.agnus.vpos,
                self.agnus.hpos,
                offset,
                size,
                value,
            ));
    }

    fn capture_current_frame_display_start(&mut self) {
        if self.current_frame_display_snapshot_taken {
            return;
        }
        self.current_frame_visible_start_vpos = self.visible_start_vpos_for_current_control();
        self.refresh_frame_geometry_visible_start();
        self.lazy_collision_vpos = self.current_frame_visible_start_vpos;
        self.current_frame_chip_ram.clear();
        self.current_frame_chip_ram
            .extend_from_slice(&self.mem.chip_ram);
        self.current_frame_beam_top_palette = self.beam_top_palette;
        self.current_frame_display_snapshot_taken = true;
        self.advance_display_dma_for_clipped_rows();
        self.advance_sprite_dma_to_visible_start();
        self.capture_held_sprites_for_visible_window();
    }

    /// After the offscreen sprite-DMA replay, snapshot any sprite that has
    /// fetched data but whose DMA is now disabled (SPREN cleared): it is being
    /// "held" and will be repainted by Copper SPRxPOS repositioning across the
    /// visible window. The renderer's manual-sprite path consumes these (it can
    /// clip each repositioned segment); the bus bar path is suppressed for them.
    fn capture_held_sprites_for_visible_window(&mut self) {
        self.current_frame_held_sprites = [None; 8];
        if self.agnus.dmacon & (DMACON_DMAEN | DMACON_SPREN) == (DMACON_DMAEN | DMACON_SPREN) {
            // Sprite DMA is still active: the normal capture path handles it.
            return;
        }
        for sprite in 0..8 {
            let state = self.display_dma_sprite_state[sprite];
            if !state.data_dma_active {
                continue;
            }
            let Some(line_data) = state.last_line else {
                continue;
            };
            let Some(control) = state.control else {
                continue;
            };
            self.current_frame_held_sprites[sprite] = Some(HeldSpriteLine {
                line: CapturedSpriteLine {
                    sprite,
                    hstart: line_data.hstart,
                    hsub_70ns: line_data.hsub_70ns,
                    beam_y: 0,
                    data: line_data.data,
                    datb: line_data.datb,
                    data_ext: line_data.data_ext,
                    datb_ext: line_data.datb_ext,
                    width_words: line_data.width_words,
                    attached: line_data.attached,
                },
                vstart: control.vstart,
                vstop: control.vstop,
            });
        }
    }

    fn apply_display_sprite_pointer_low_write(&mut self, sprite: usize) {
        self.apply_display_sprite_pointer_low_write_at(sprite, self.agnus.vpos, self.agnus.hpos);
    }

    fn apply_display_sprite_pointer_low_write_at(&mut self, sprite: usize, vpos: u32, hpos: u32) {
        if sprite >= 8 {
            return;
        }
        let pair_slot_hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[sprite / 2];
        let state = self.display_dma_sprite_state[sprite];
        if hpos > pair_slot_hpos && state.control.is_some() && !state.data_dma_active {
            self.display_dma_sprite_state[sprite] = DisplaySpriteDmaState::default();
            return;
        }
        self.retarget_display_sprite_dma_pointer_at(sprite, vpos, hpos);
    }

    fn latch_display_sprite_dma_control_from_registers(&mut self, sprite: usize) {
        if sprite >= 8 {
            return;
        }
        self.latch_display_sprite_dma_control_from_words_at(
            sprite,
            self.denise.sprpos[sprite],
            self.denise.sprctl[sprite],
            self.agnus.vpos,
            self.agnus.hpos,
            self.agnus.dmacon,
        );
    }

    fn latch_display_sprite_dma_control_from_words_at(
        &mut self,
        sprite: usize,
        pos: u16,
        ctl: u16,
        vpos: u32,
        hpos: u32,
        dmacon: u16,
    ) {
        if sprite >= 8 {
            return;
        }

        let vstart = sprite_vstart_from_words(pos, ctl);
        let raw_vstop = sprite_vstop_from_ctl(ctl);
        let vstop = if raw_vstop < vstart {
            self.agnus.current_frame_lines() as i32
        } else {
            raw_vstop
        };
        let height = vstop - vstart;
        if height <= 0 {
            self.display_dma_sprite_state[sprite] = DisplaySpriteDmaState::default();
            return;
        }

        let quantum = sprite_fetch_quantum(self.agnus.fmode());
        let data_lines = if sprite_scan_doubled(self.agnus.fmode()) {
            (height as u32).div_ceil(2)
        } else {
            height as u32
        };
        let data_base = self.display_dma_sprpt[sprite] & self.chip_dma_mask & !1;
        let mut control = DisplaySpriteControl {
            vstart,
            vstop,
            hstart: sprite_hstart_from_words(pos, ctl),
            hsub_70ns: bitplane_shres(self.denise.bplcon0) && sprite_hsub_70ns_from_ctl(ctl),
            data_vstart: vstart,
            data_base,
            next_ptr: data_base.wrapping_add(data_lines.saturating_mul(4 * quantum))
                & self.chip_dma_mask
                & !1,
            attached: ctl & 0x0080 != 0,
        };

        let mut state = self.display_dma_sprite_state[sprite];
        let previous_control = state.control;
        let beam_y = vpos as i32;
        let in_window = beam_y >= control.vstart && beam_y < control.vstop;
        let sprite_dma_enabled =
            dmacon & (DMACON_DMAEN | DMACON_SPREN) == (DMACON_DMAEN | DMACON_SPREN);
        let reaches_current_fetch_slot = beam_y == control.vstart
            && hpos <= SPRITE_DMA_PAIR_CAPTURE_HPOS[sprite / 2]
            && sprite_dma_enabled;
        let keep_held_line =
            !sprite_dma_enabled && in_window && state.data_dma_active && state.last_line.is_some();
        let keep_active_dma_line =
            sprite_dma_enabled && in_window && state.data_dma_active && state.last_line.is_some();

        if keep_held_line || keep_active_dma_line {
            if let Some(previous_control) = previous_control {
                control.data_vstart = previous_control.effective_data_vstart();
                control.data_base = previous_control.data_base;
                control.next_ptr = previous_control.next_ptr;
            }
        }

        state.control = Some(control);
        state.next_ptr = Some(control.next_ptr);
        state.terminated = false;
        state.data_dma_active =
            in_window && (reaches_current_fetch_slot || keep_held_line || keep_active_dma_line);
        if !keep_held_line && !keep_active_dma_line {
            state.last_line = None;
        }
        self.display_dma_sprite_state[sprite] = state;
    }

    fn retarget_display_sprite_dma_pointer_at(&mut self, sprite: usize, vpos: u32, hpos: u32) {
        if sprite >= 8 {
            return;
        }

        let mut state = self.display_dma_sprite_state[sprite];
        let Some(mut control) = state.control else {
            self.display_dma_sprite_state[sprite] = DisplaySpriteDmaState::default();
            return;
        };

        let beam_y = vpos as i32;
        if beam_y >= control.vstop {
            self.display_dma_sprite_state[sprite] = DisplaySpriteDmaState::default();
            return;
        }

        let quantum = sprite_fetch_quantum(self.agnus.fmode());
        let line_bytes = 4 * quantum;
        let mut line = if beam_y <= control.vstart {
            0
        } else {
            (beam_y - control.vstart) as u32
        };
        if beam_y >= control.vstart && hpos > SPRITE_DMA_PAIR_CAPTURE_HPOS[sprite / 2] {
            line = line.saturating_add(1);
        }
        let line = if sprite_scan_doubled(self.agnus.fmode()) {
            line.div_ceil(2)
        } else {
            line
        };

        let ptr = self.display_dma_sprpt[sprite] & self.chip_dma_mask & !1;
        control.data_base =
            ptr.wrapping_sub(line.saturating_mul(line_bytes)) & self.chip_dma_mask & !1;
        control.data_vstart = control.vstart;
        let height = (control.vstop - control.vstart).max(0) as u32;
        let data_lines = if sprite_scan_doubled(self.agnus.fmode()) {
            height.div_ceil(2)
        } else {
            height
        };
        control.next_ptr = control
            .data_base
            .wrapping_add(data_lines.saturating_mul(line_bytes))
            & self.chip_dma_mask
            & !1;
        state.control = Some(control);
        state.next_ptr = Some(control.next_ptr);
        state.terminated = false;
        state.data_dma_active = beam_y >= control.vstart && beam_y < control.vstop;
        state.last_line = None;
        self.display_dma_sprite_state[sprite] = state;

        if diag_sprcap().is_some() {
            log::info!(
                "sprptr f={} v={} h={} s{} ptr={:06X} vstart={} vstop={} hstart={} line={} data_base={:06X} next={:06X}",
                self.emulated_frames,
                vpos,
                hpos,
                sprite,
                ptr,
                control.vstart,
                control.vstop,
                control.hstart,
                line,
                control.data_base,
                control.next_ptr
            );
        }
    }

    fn capture_same_line_display_start_if_due(&mut self) {
        if self.current_frame_display_snapshot_taken
            || matches!(self.agnus.revision(), AgnusRevision::Ocs)
            || display_window_unprogrammed(self.denise.diwstrt, self.denise.diwstop)
        {
            return;
        }
        let visible_start = self.visible_start_vpos_for_current_control();
        if visible_start != self.agnus.vpos
            || !display_window_contains_vpos(
                self.denise.diwstrt,
                self.denise.diwstop,
                self.effective_diwhigh(),
                self.agnus.vpos,
            )
        {
            return;
        }
        self.capture_current_frame_display_start();
    }

    fn advance_display_dma_for_clipped_rows(&mut self) {
        if self.display_dma_clipped_rows_advanced {
            return;
        }
        self.display_dma_clipped_rows_advanced = true;
        let visible_start = self.current_frame_visible_start_vpos;
        let rows = clipped_display_rows_before_visible(
            self.denise.diwstrt,
            self.denise.diwstop,
            self.effective_diwhigh(),
            visible_start,
        );
        if rows == 0 {
            return;
        }
        // Bitplane DMA only fetched on the clipped lines where it was
        // actually enabled at the time: replay this frame's BPLCON0/DMACON
        // writes across the span rather than sampling the registers at the
        // visible start. Regression example: the CDTV extended-ROM boot
        // screen opens DIW at line 5 but raises BPLCON0 from 0 to 6 planes
        // only at line 24; advancing every clipped row ran the pointers 19
        // rows ahead, walking off the end of the image (and into the next
        // plane's data) near the bottom of the frame.
        let base = self.current_frame_render_base;
        let mut bplcon0 = base.bplcon0;
        let mut dmacon = base.dmacon;
        let first_vpos = visible_start.saturating_sub(rows as u32);
        // Writes landing before the line's hard fetch start still govern
        // that line's fetch; later ones take effect from the next line.
        let fetch_gate_hpos = u32::from(BITPLANE_DDF_HARD_START);
        let writes: Vec<(u32, u32, u16, u16)> = self
            .current_render_events()
            .iter()
            .filter(|w| matches!(w.offset, 0x096 | 0x100) && w.vpos < visible_start)
            .map(|w| (w.vpos, w.hpos, w.offset, w.value))
            .collect();
        let mut idx = 0;
        for vpos in first_vpos..visible_start {
            while idx < writes.len()
                && (writes[idx].0 < vpos
                    || (writes[idx].0 == vpos && writes[idx].1 < fetch_gate_hpos))
            {
                let (_, _, offset, value) = writes[idx];
                match offset {
                    0x096 => {
                        if value & 0x8000 != 0 {
                            dmacon |= value & 0x7FFF;
                        } else {
                            dmacon &= !value;
                        }
                    }
                    0x100 => bplcon0 = value,
                    _ => {}
                }
                idx += 1;
            }
            if dmacon & (DMACON_DMAEN | DMACON_BPLEN) != (DMACON_DMAEN | DMACON_BPLEN) {
                continue;
            }
            let nplanes = BitplaneMode::from_bplcon0(bplcon0, self.aga_enabled()).dma_planes();
            if nplanes == 0 {
                continue;
            }
            if effective_ddf_window(
                self.agnus.revision(),
                bplcon0,
                self.denise.ddfstrt,
                self.denise.ddfstop,
                self.harddis_active(),
            )
            .is_none()
            {
                continue;
            }
            let words_per_row = bitplane_words_per_row(
                self.agnus.revision(),
                bplcon0,
                self.agnus.fmode(),
                self.denise.ddfstrt,
                self.denise.ddfstop,
                self.harddis_active(),
            );
            self.advance_display_dma_ptrs(1, nplanes, words_per_row, vpos);
        }
    }

    fn advance_sprite_dma_to_visible_start(&mut self) {
        let visible_start = self.current_frame_visible_start_vpos;
        if visible_start == 0 {
            return;
        }

        // Sprite DMA runs from the top of the frame, independent of the bitplane
        // display window: a sprite that starts in the top border has its
        // control/data words fetched before the first framebuffer line, so
        // advance the DMA state across those offscreen lines. Crucially, SPREN
        // can be toggled within the frame -- software may enable sprite DMA
        // only briefly off-screen to load reused sprites, then clear it before
        // the visible window and reposition the held sprites per line.
        // So replay this frame's DMACON and SPRxPT writes across the offscreen
        // span and run the sprite fetch only on lines where SPREN was actually
        // enabled, rather than sampling registers at the visible start.
        let base = self.current_frame_render_base;
        self.display_dma_sprpt = base.sprpt;
        self.display_dma_sprite_state = [DisplaySpriteDmaState::default(); 8];
        let mut dmacon = base.dmacon;
        let writes: Vec<(u32, u32, u16, u16)> = self
            .current_render_events()
            .iter()
            .filter(|w| {
                let off = w.offset & 0x01FE;
                w.vpos < visible_start && (off == 0x096 || (0x120..=0x13F).contains(&off))
            })
            .map(|w| (w.vpos, w.hpos, w.offset & 0x01FE, w.value))
            .collect();
        let mut idx = 0;
        for vpos in 0..visible_start {
            for (pair, &capture_hpos) in SPRITE_DMA_PAIR_CAPTURE_HPOS.iter().enumerate() {
                while idx < writes.len()
                    && (writes[idx].0 < vpos
                        || (writes[idx].0 == vpos && writes[idx].1 < capture_hpos))
                {
                    let (event_vpos, event_hpos, offset, value) = writes[idx];
                    self.apply_sprite_dma_replay_write(
                        offset,
                        value,
                        event_vpos,
                        event_hpos,
                        &mut dmacon,
                    );
                    idx += 1;
                }

                if dmacon & (DMACON_DMAEN | DMACON_SPREN) != (DMACON_DMAEN | DMACON_SPREN) {
                    continue;
                }
                for sprite in pair * 2..pair * 2 + 2 {
                    if sprite_dma_disabled_by_bitplane_ddf(
                        sprite,
                        self.agnus.revision(),
                        self.effective_bitplane_bplcon0(),
                        self.agnus.fmode(),
                        self.effective_bitplane_dmacon(),
                        self.denise.ddfstrt,
                        self.denise.ddfstop,
                        self.harddis_active(),
                    ) {
                        continue;
                    }
                    let _ = self.captured_sprite_line_at(sprite, vpos);
                }
            }
            while idx < writes.len() && writes[idx].0 == vpos {
                let (event_vpos, event_hpos, offset, value) = writes[idx];
                self.apply_sprite_dma_replay_write(
                    offset,
                    value,
                    event_vpos,
                    event_hpos,
                    &mut dmacon,
                );
                idx += 1;
            }
        }
    }

    fn apply_sprite_dma_replay_write(
        &mut self,
        offset: u16,
        value: u16,
        vpos: u32,
        hpos: u32,
        dmacon: &mut u16,
    ) {
        if offset == 0x096 {
            if value & 0x8000 != 0 {
                *dmacon |= value & 0x7FFF;
            } else {
                *dmacon &= !value;
            }
            return;
        }

        let idx = ((offset - 0x120) / 4) as usize;
        if idx >= 8 {
            return;
        }
        if offset & 2 == 0 {
            let cur = self.display_dma_sprpt[idx];
            self.display_dma_sprpt[idx] = (cur & 0x0000_FFFF) | ((value as u32 & 0x001F) << 16);
        } else {
            let cur = self.display_dma_sprpt[idx];
            self.display_dma_sprpt[idx] = (cur & 0x00FF_0000) | (value as u32 & 0xFFFE);
            self.apply_display_sprite_pointer_low_write_at(idx, vpos, hpos);
        }
    }

    fn capture_sprite_dma_words_if_due(&mut self, vpos: u32, old_hpos: u32, new_hpos: u32) {
        // No sprite DMA pair slot lies in [old_hpos, new_hpos): nothing below
        // can run (the per-pair loop checks the same window), so skip the
        // sprite-state scan on the vast majority of beam advances.
        if old_hpos > SPRITE_DMA_PAIR_CAPTURE_HPOS[3] || new_hpos <= SPRITE_DMA_PAIR_CAPTURE_HPOS[0]
        {
            return;
        }
        let sprite_dma_enabled =
            self.agnus.dmacon & (DMACON_DMAEN | DMACON_SPREN) == (DMACON_DMAEN | DMACON_SPREN);
        let sprite_vertical_bar_active = self
            .display_dma_sprite_state
            .iter()
            .any(|state| state.data_dma_active && state.last_line.is_some());
        if !sprite_dma_enabled && !sprite_vertical_bar_active {
            return;
        }
        let Some(fb_y) = visible_framebuffer_y(
            vpos,
            self.current_frame_visible_start_vpos,
            self.current_frame_geometry.visible_lines,
        ) else {
            return;
        };
        let started = VideoPipelineStats::probe_timing_sample(
            &mut self.video_pipeline_stats.sprite_fetch_probes,
            VIDEO_FETCH_TIMING_SAMPLE_RATE,
        );
        let mut pair_slots = 0usize;
        let mut fetched_lines = 0usize;
        let bitplane_bplcon0 = self.effective_bitplane_bplcon0();
        let bitplane_dmacon = self.effective_bitplane_dmacon();
        for (pair, &capture_hpos) in SPRITE_DMA_PAIR_CAPTURE_HPOS.iter().enumerate() {
            if old_hpos > capture_hpos || new_hpos <= capture_hpos {
                continue;
            }
            if sprite_dma_enabled {
                pair_slots += 1;
                self.current_frame_sprite_dma_observed = true;
            }
            let mut captured_line = false;
            for sprite in pair * 2..pair * 2 + 2 {
                if sprite_dma_disabled_by_bitplane_ddf(
                    sprite,
                    self.agnus.revision(),
                    bitplane_bplcon0,
                    self.agnus.fmode(),
                    bitplane_dmacon,
                    self.denise.ddfstrt,
                    self.denise.ddfstop,
                    self.harddis_active(),
                ) {
                    continue;
                }
                let line = if sprite_dma_enabled {
                    self.captured_sprite_line_at(sprite, vpos)
                } else {
                    self.captured_sprite_vertical_bar_line_at(sprite, vpos)
                };
                if let Some(line) = line {
                    // COPPERLINE_DIAG_SPRCAP=BEAMY|all: log captured sprite
                    // lines on that beam line (frame, channel, position, words,
                    // and the chip-RAM descriptor/data addresses they fetched).
                    if let Some(want) = diag_sprcap() {
                        if want.trim() == "all"
                            || want.trim().parse::<i32>().ok() == Some(line.beam_y)
                        {
                            let st = &self.display_dma_sprite_state[sprite];
                            log::info!(
                                "sprcap f={} s{} y={} hstart={} hsub={} att={} w={} A={:04X} {:04X?} B={:04X} {:04X?} data_base={:06X} next={:06X?}",
                                self.emulated_frames,
                                line.sprite,
                                line.beam_y,
                                line.hstart,
                                u8::from(line.hsub_70ns),
                                u8::from(line.attached),
                                line.width_words,
                                line.data,
                                line.data_ext,
                                line.datb,
                                line.datb_ext,
                                st.control.map(|c| c.data_base).unwrap_or(0),
                                st.next_ptr
                            );
                        }
                    }
                    self.current_frame_sprite_lines.push(line);
                    self.current_frame_sprite_lines_by_y[fb_y].push(line);
                    self.current_frame_sprite_dma_observed = true;
                    captured_line = true;
                    fetched_lines += 1;
                }
            }
            if captured_line {
                self.current_frame_sprite_collision_sources[fb_y] = None;
            }
        }
        self.record_sprite_fetch_timing(
            pair_slots,
            fetched_lines,
            started.map(|started| (started.elapsed(), VIDEO_FETCH_TIMING_SAMPLE_RATE)),
        );
    }

    fn ensure_current_frame_sprite_collision_sources_for_y(&mut self, fb_y: usize, vpos: u32) {
        if self.current_frame_sprite_collision_sources[fb_y].is_none() {
            self.current_frame_sprite_collision_sources[fb_y] =
                Some(live_sprite_collision_sources_with_beam_gated_odd(
                    &self.current_frame_sprite_lines_by_y[fb_y],
                    vpos as i32,
                ));
        }
    }

    fn captured_sprite_line_at(&mut self, sprite: usize, vpos: u32) -> Option<CapturedSpriteLine> {
        let ram_len = self.mem.chip_ram.len();
        if ram_len == 0 {
            return None;
        }
        let ram_mask = self.chip_dma_mask;
        let beam_y = vpos as i32;
        let mut state = self.display_dma_sprite_state[sprite];
        let mut descriptor_can_match_current_vstart =
            state.control.is_some() || state.next_ptr.is_none();

        let mut visited_descriptor_ptrs = HashSet::new();
        loop {
            if state.terminated {
                self.display_dma_sprite_state[sprite] = state;
                return None;
            }

            if let Some(control) = state.control {
                if beam_y >= control.vstop {
                    state.next_ptr = Some(control.next_ptr);
                    state.control = None;
                    state.data_dma_active = false;
                    state.last_line = None;
                    descriptor_can_match_current_vstart = false;
                } else if !state.data_dma_active {
                    if beam_y == control.vstart {
                        state.data_dma_active = true;
                    } else {
                        self.display_dma_sprite_state[sprite] = state;
                        return None;
                    }
                }

                if let Some(control) = state.control {
                    if !state.data_dma_active {
                        self.display_dma_sprite_state[sprite] = state;
                        return None;
                    }
                    let quantum = sprite_fetch_quantum(self.agnus.fmode());
                    // SSCAN2 fetches sprite data only on every second display
                    // line; the in-between line redisplays the same data.
                    let mut line = (beam_y - control.effective_data_vstart()) as u32;
                    if sprite_scan_doubled(self.agnus.fmode()) {
                        line /= 2;
                    }
                    let line_bytes = 4 * quantum;
                    let data_ptr = control
                        .data_base
                        .wrapping_add(line.saturating_mul(line_bytes))
                        & ram_mask
                        & !1;
                    let datb_ptr = data_ptr.wrapping_add(2 * quantum);
                    let mut data_ext = [0u16; 3];
                    let mut datb_ext = [0u16; 3];
                    for w in 1..quantum as usize {
                        data_ext[w - 1] = read_chip_word_wrapping(
                            &self.mem.chip_ram,
                            data_ptr.wrapping_add(2 * w as u32),
                        );
                        datb_ext[w - 1] = read_chip_word_wrapping(
                            &self.mem.chip_ram,
                            datb_ptr.wrapping_add(2 * w as u32),
                        );
                    }
                    let line_data = DisplaySpriteLineData {
                        hstart: control.hstart,
                        hsub_70ns: control.hsub_70ns,
                        data: read_chip_word_wrapping(&self.mem.chip_ram, data_ptr),
                        datb: read_chip_word_wrapping(&self.mem.chip_ram, datb_ptr),
                        data_ext,
                        datb_ext,
                        width_words: quantum as u8,
                        attached: control.attached,
                    };
                    state.last_line = Some(line_data);
                    self.display_dma_sprite_state[sprite] = state;
                    return Some(CapturedSpriteLine {
                        sprite,
                        hstart: line_data.hstart,
                        hsub_70ns: line_data.hsub_70ns,
                        beam_y,
                        data: line_data.data,
                        datb: line_data.datb,
                        data_ext: line_data.data_ext,
                        datb_ext: line_data.datb_ext,
                        width_words: line_data.width_words,
                        attached: line_data.attached,
                    });
                }
            }

            let descriptor_ptr =
                state.next_ptr.unwrap_or(self.display_dma_sprpt[sprite]) & ram_mask & !1;
            if !visited_descriptor_ptrs.insert(descriptor_ptr) {
                state.terminated = true;
                state.data_dma_active = false;
                state.last_line = None;
                self.display_dma_sprite_state[sprite] = state;
                return None;
            }

            // AGA wide fetches also widen the control-word slots: POS is the
            // first word of the first fetch, CTL the first word of the second.
            let quantum = sprite_fetch_quantum(self.agnus.fmode());
            let mut ptr = descriptor_ptr;
            let pos = read_chip_word_wrapping(&self.mem.chip_ram, ptr);
            let ctl = read_chip_word_wrapping(&self.mem.chip_ram, ptr.wrapping_add(2 * quantum));
            ptr = ptr.wrapping_add(4 * quantum) & ram_mask & !1;
            if pos == 0 && ctl == 0 {
                state.terminated = true;
                state.data_dma_active = false;
                state.last_line = None;
                self.display_dma_sprite_state[sprite] = state;
                return None;
            }

            let vstart = sprite_vstart_from_words(pos, ctl);
            let raw_vstop = sprite_vstop_from_ctl(ctl);
            // An inverted vertical pair (vstop < vstart) does not disable the
            // sprite. Agnus arms it at vstart; its vstop comparator already
            // passed for this field, so it does not fire again until the field
            // wraps, and the sprite keeps fetching data to the bottom of the
            // frame. Clamp the effective vstop to the frame bottom -- the
            // per-field VBLANK reset re-fetches this descriptor, which covers
            // the 0..vstop wrap tail on the next field. Treating vstop<vstart
            // as "off" drops full-height strips that are intentionally reused
            // and repositioned every line by SPRxPOS writes.
            let vstop = if raw_vstop < vstart {
                self.agnus.current_frame_lines() as i32
            } else {
                raw_vstop
            };
            let height = vstop - vstart;
            if height <= 0 {
                // Equal start/stop descriptors idle the sprite stream for
                // this field. Do not scan onward into the following words:
                // they are often bitmap data for a later rearmed sprite.
                state.terminated = true;
                state.control = None;
                state.data_dma_active = false;
                state.last_line = None;
                self.display_dma_sprite_state[sprite] = state;
                return None;
            }

            // With SSCAN2 each fetched data line covers two display lines,
            // so the descriptor consumes only ceil(height/2) data lines.
            let data_lines = if sprite_scan_doubled(self.agnus.fmode()) {
                (height as u32).div_ceil(2)
            } else {
                height as u32
            };
            let control = DisplaySpriteControl {
                vstart,
                vstop,
                hstart: sprite_hstart_from_words(pos, ctl),
                hsub_70ns: bitplane_shres(self.denise.bplcon0) && sprite_hsub_70ns_from_ctl(ctl),
                data_vstart: vstart,
                data_base: ptr,
                next_ptr: ptr.wrapping_add(data_lines.saturating_mul(4 * quantum)) & ram_mask & !1,
                attached: ctl & 0x0080 != 0,
            };

            state.control = Some(control);
            state.data_dma_active = false;
            state.last_line = None;
            if beam_y < control.vstart {
                self.display_dma_sprite_state[sprite] = state;
                return None;
            }
            if beam_y == control.vstart && descriptor_can_match_current_vstart {
                state.data_dma_active = true;
                continue;
            }
            if beam_y < control.vstop {
                self.display_dma_sprite_state[sprite] = state;
                return None;
            }
        }
    }

    fn captured_sprite_vertical_bar_line_at(
        &mut self,
        sprite: usize,
        vpos: u32,
    ) -> Option<CapturedSpriteLine> {
        // Sprites captured as "held" at the visible start are repainted by the
        // renderer's manual-sprite path (which clips each Copper-repositioned
        // segment), so do not also emit a full-width bar for them here.
        if self.current_frame_held_sprites[sprite].is_some() {
            return None;
        }
        let beam_y = vpos as i32;
        let mut state = self.display_dma_sprite_state[sprite];
        let control = state.control?;
        if beam_y >= control.vstop {
            state.next_ptr = Some(control.next_ptr);
            state.control = None;
            state.data_dma_active = false;
            state.last_line = None;
            self.display_dma_sprite_state[sprite] = state;
            return None;
        }
        if beam_y < control.vstart || !state.data_dma_active {
            self.display_dma_sprite_state[sprite] = state;
            return None;
        }
        let line_data = state.last_line?;
        self.display_dma_sprite_state[sprite] = state;
        // Position the held strip at the sprite's *current* SPRxPOS/CTL, not
        // the fetch-time hstart: with sprite DMA off the Copper (or CPU) can
        // reposition a reused sprite by rewriting SPRxPOS, so the held data
        // must follow it. For a sprite left where the DMA fetched it this is
        // the same value.
        let pos = self.denise.sprpos[sprite];
        let ctl = self.denise.sprctl[sprite];
        Some(CapturedSpriteLine {
            sprite,
            hstart: sprite_hstart_from_words(pos, ctl),
            hsub_70ns: bitplane_shres(self.denise.bplcon0) && sprite_hsub_70ns_from_ctl(ctl),
            beam_y,
            data: line_data.data,
            datb: line_data.datb,
            data_ext: line_data.data_ext,
            datb_ext: line_data.datb_ext,
            width_words: line_data.width_words,
            attached: line_data.attached,
        })
    }

    fn capture_bitplane_dma_words_if_due(
        &mut self,
        vpos: u32,
        old_hpos: u32,
        new_hpos: u32,
        old_emulated_cck: u64,
    ) {
        let display_bplcon0 = self.effective_bitplane_bplcon0_at(old_emulated_cck);
        let mode = BitplaneMode::from_bplcon0(display_bplcon0, self.aga_enabled());
        let display_planes = mode.display_planes();
        if !display_window_contains_vpos(
            self.denise.diwstrt,
            self.denise.diwstop,
            self.effective_diwhigh(),
            vpos,
        ) {
            return;
        }
        let Some(fb_y) = visible_framebuffer_y(
            vpos,
            self.current_frame_visible_start_vpos,
            self.current_frame_geometry.visible_lines,
        ) else {
            return;
        };

        let ram_len = self.mem.chip_ram.len();
        if ram_len == 0 {
            return;
        }
        let Some((effective_ddfstart, effective_ddfstop)) = effective_ddf_window(
            self.agnus.revision(),
            display_bplcon0,
            self.denise.ddfstrt,
            self.denise.ddfstop,
            self.harddis_active(),
        ) else {
            return;
        };
        let effective_ddfstart = u32::from(effective_ddfstart);
        let effective_ddfstop = u32::from(effective_ddfstop);
        // AGA FMODE: each fetch slot moves `quantum` consecutive words per
        // plane; the per-plane cadence stretches to `period` colour clocks
        // and the lores slot sequence spreads across the `unit`-cck block.
        let fmode = self.agnus.fmode();
        let quantum = bitplane_fetch_quantum(fmode) as usize;
        // Wide-FMODE units lengthen the gap between groups of fetched words,
        // but the sequencer is still armed by the DDFSTRT comparator itself.
        // Lores plane-order slots are packed into the first eight cycles of
        // that unit; the remaining cycles are free for the blitter/CPU.
        let ddfstart = effective_ddfstart;
        if self.bitplane_ddfstart_missed_on_line(vpos, ddfstart) {
            return;
        }
        if new_hpos <= ddfstart {
            return;
        }
        let ddfstart_cck = if old_hpos <= ddfstart {
            Some(i128::from(
                old_emulated_cck.saturating_add(u64::from(ddfstart - old_hpos)),
            ))
        } else {
            old_emulated_cck
                .checked_sub(u64::from(old_hpos - ddfstart))
                .map(i128::from)
        };
        let anchor_bplcon0 = ddfstart_cck
            .map(|cck| self.bitplane_bplcon0_for_block(cck))
            .unwrap_or(display_bplcon0);
        let anchor_mode = BitplaneMode::from_bplcon0(anchor_bplcon0, self.aga_enabled());
        let anchor_dma_planes = anchor_mode.dma_planes();
        let period = bitplane_fetch_period(anchor_bplcon0, fmode);
        let unit = bitplane_fetch_unit(anchor_bplcon0, fmode);
        let started = VideoPipelineStats::probe_timing_sample(
            &mut self.video_pipeline_stats.bitplane_fetch_probes,
            VIDEO_FETCH_TIMING_SAMPLE_RATE,
        );
        let words_per_row = bitplane_words_per_row(
            self.agnus.revision(),
            anchor_bplcon0,
            self.agnus.fmode(),
            self.denise.ddfstrt,
            self.denise.ddfstop,
            self.harddis_active(),
        );
        let mut rows_started = 0usize;
        let mut slots = 0usize;
        let mut line_complete = false;
        let mut line_complete_plane_mask = 0u16;
        let addr_mask = self.chip_dma_mask;
        let hires_like = bitplane_hires(anchor_bplcon0) || bitplane_shres(anchor_bplcon0);
        let last_word_idx = words_per_row.saturating_sub(1);
        if diag_caprow().is_some_and(|spec| spec.contains(vpos))
            && old_hpos <= ddfstart
            && new_hpos > ddfstart
        {
            log::info!(
                "caprow f={} v={} h={} dmacon={:#06X} bplcon0={:#06X} dma_bplcon0={:#06X} bplcon1={:#06X} bplcon2={:#06X} bplcon4={:#06X} fmode={:#06X} diw={:#06X}/{:#06X}/{:?} ddf={:#04X}/{:#04X} eff={:#04X}-{:#04X} anchor={:#04X} unit={} period={} quantum={} wpr={} display_planes={} dma_planes={} mod={}/{} bplpt={:#08X},{:#08X},{:#08X},{:#08X},{:#08X},{:#08X},{:#08X},{:#08X}",
                self.emulated_frames,
                vpos,
                self.agnus.hpos,
                self.effective_bitplane_dmacon(),
                display_bplcon0,
                anchor_bplcon0,
                self.denise.bplcon1,
                self.denise.bplcon2,
                self.denise.bplcon4,
                fmode,
                self.denise.diwstrt,
                self.denise.diwstop,
                self.effective_diwhigh(),
                self.denise.ddfstrt,
                self.denise.ddfstop,
                effective_ddfstart,
                effective_ddfstop,
                ddfstart,
                unit,
                period,
                quantum,
                words_per_row,
                display_planes,
                anchor_dma_planes,
                self.denise.bpl1mod,
                self.denise.bpl2mod,
                self.display_dma_bplpt[0],
                self.display_dma_bplpt[1],
                self.display_dma_bplpt[2],
                self.display_dma_bplpt[3],
                self.display_dma_bplpt[4],
                self.display_dma_bplpt[5],
                self.display_dma_bplpt[6],
                self.display_dma_bplpt[7],
            );
        }
        for hpos in old_hpos..new_hpos {
            let hpos_emulated_cck =
                old_emulated_cck.saturating_add(u64::from(hpos.saturating_sub(old_hpos)));
            if self.effective_bitplane_dmacon_at(hpos_emulated_cck) & (DMACON_DMAEN | DMACON_BPLEN)
                != (DMACON_DMAEN | DMACON_BPLEN)
            {
                continue;
            }
            if hpos < ddfstart {
                continue;
            }
            let rel = hpos - ddfstart;
            if hires_like {
                if rel % period != 0 {
                    continue;
                }
                let word_base = (rel / period) as usize * quantum;
                if word_base >= words_per_row {
                    continue;
                }
                let block_start_cck = i128::from(hpos_emulated_cck);
                let block_bplcon0 = self.bitplane_bplcon0_for_block(block_start_cck);
                let block_mode = BitplaneMode::from_bplcon0(block_bplcon0, self.aga_enabled());
                let block_dma_planes = block_mode.dma_planes();
                if block_dma_planes == 0 {
                    continue;
                }
                let block_display_planes = block_mode.display_planes();
                for plane in 0..block_dma_planes.min(8) {
                    if plane == 0 {
                        self.record_sprite_display_enable_for_bitplane_dma(vpos, block_bplcon0);
                    }
                    for w in 0..quantum.min(words_per_row - word_base) {
                        let word_idx = word_base + w;
                        let addr = self.display_dma_bplpt[plane] & addr_mask;
                        let fetched = read_chip_word_wrapping(&self.mem.chip_ram, addr);
                        self.data_bus = fetched;
                        if self.capture_bitplane_fetch_word(
                            fb_y,
                            block_display_planes,
                            block_dma_planes,
                            words_per_row,
                            plane,
                            word_idx,
                            fetched,
                        ) {
                            rows_started += 1;
                        }
                        self.denise.write_bpldat(plane, fetched);
                        self.display_dma_bplpt[plane] =
                            self.display_dma_bplpt[plane].wrapping_add(2) & addr_mask;
                        if word_idx == last_word_idx {
                            line_complete = true;
                            line_complete_plane_mask = plane_mask_for_count(block_dma_planes);
                        }
                    }
                    slots += 1;
                }
            } else {
                let word_base = (rel / unit) as usize * quantum;
                if word_base >= words_per_row {
                    continue;
                }
                let unit_off = rel % unit;
                if unit_off >= 8 {
                    continue;
                }
                let order = unit_off;
                let block_start_cck = i128::from(hpos_emulated_cck) - i128::from(unit_off);
                let block_bplcon0 = self.bitplane_bplcon0_for_block(block_start_cck);
                let block_mode = BitplaneMode::from_bplcon0(block_bplcon0, self.aga_enabled());
                let block_dma_planes = block_mode.dma_planes();
                if block_dma_planes == 0 {
                    continue;
                }
                let block_display_planes = block_mode.display_planes();
                let block_last_order = (0..block_dma_planes.min(8))
                    .map(|plane| bitplane_fetch_order(block_bplcon0, plane))
                    .max()
                    .unwrap_or(0);
                for plane in 0..block_dma_planes.min(8) {
                    if bitplane_fetch_order(block_bplcon0, plane) != order {
                        continue;
                    }
                    if plane == 0 {
                        self.record_sprite_display_enable_for_bitplane_dma(vpos, block_bplcon0);
                    }
                    for w in 0..quantum.min(words_per_row - word_base) {
                        let word_idx = word_base + w;
                        let addr = self.display_dma_bplpt[plane] & addr_mask;
                        let fetched = read_chip_word_wrapping(&self.mem.chip_ram, addr);
                        self.data_bus = fetched;
                        if self.capture_bitplane_fetch_word(
                            fb_y,
                            block_display_planes,
                            block_dma_planes,
                            words_per_row,
                            plane,
                            word_idx,
                            fetched,
                        ) {
                            rows_started += 1;
                        }
                        self.denise.write_bpldat(plane, fetched);
                        self.display_dma_bplpt[plane] =
                            self.display_dma_bplpt[plane].wrapping_add(2) & addr_mask;
                        if word_idx == last_word_idx && order == block_last_order {
                            line_complete = true;
                            line_complete_plane_mask = plane_mask_for_count(block_dma_planes);
                        }
                    }
                    slots += 1;
                }
            }
        }

        if slots == 0 {
            return;
        }
        if line_complete {
            self.advance_display_dma_modulos_for_mask(line_complete_plane_mask, self.agnus.vpos);
        }

        self.record_bitplane_fetch_timing(
            slots,
            rows_started,
            usize::from(line_complete),
            started.map(|started| (started.elapsed(), VIDEO_FETCH_TIMING_SAMPLE_RATE)),
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn capture_bitplane_fetch_word(
        &mut self,
        fb_y: usize,
        display_planes: usize,
        dma_planes: usize,
        words_per_row: usize,
        plane: usize,
        word_idx: usize,
        fetched: u16,
    ) -> bool {
        let row_needs_init = match &self.current_frame_bitplane_rows[fb_y] {
            Some(row) => row.nplanes != display_planes || row.words_per_row != words_per_row,
            None => true,
        };
        if row_needs_init {
            let old_row = self.current_frame_bitplane_rows[fb_y].take();
            let mut row = CapturedBitplaneRow {
                nplanes: display_planes,
                words_per_row,
                planes: std::array::from_fn(|_| vec![0; words_per_row]),
            };
            for plane in dma_planes..display_planes {
                row.planes[plane].fill(self.denise.bpldat[plane]);
            }
            if let Some(old_row) = old_row {
                let copy_planes = old_row.nplanes.min(display_planes).min(8);
                let copy_words = old_row.words_per_row.min(words_per_row);
                for plane in 0..copy_planes {
                    row.planes[plane][..copy_words]
                        .copy_from_slice(&old_row.planes[plane][..copy_words]);
                }
            }
            self.current_frame_bitplane_rows[fb_y] = Some(row);
        }
        if let Some(row) = self.current_frame_bitplane_rows[fb_y].as_mut() {
            row.planes[plane][word_idx] = fetched;
        }
        row_needs_init
    }

    fn advance_display_dma_ptrs(
        &mut self,
        rows: usize,
        nplanes: usize,
        words_per_row: usize,
        first_vpos: u32,
    ) {
        for row in 0..rows {
            for plane in 0..nplanes.min(8) {
                self.display_dma_bplpt[plane] =
                    self.display_dma_bplpt[plane].wrapping_add((words_per_row * 2) as u32);
            }
            self.advance_display_dma_modulos(nplanes, words_per_row, first_vpos + row as u32);
        }
    }

    /// FMODE BSCAN2 (bit 14, Alice only) scan-doubles bitplanes: both plane
    /// groups share one end-of-line modulo, selected by the line parity
    /// relative to DIWSTRT's vertical start - the matching-parity line adds
    /// BPL1MOD, the doubled line BPL2MOD (WinUAE model). Software doubles
    /// each fetched row by rewinding with BPL1MOD = -(row bytes) and
    /// advancing with BPL2MOD.
    fn display_dma_modulo_for_plane(&self, plane: usize, vpos: u32) -> i16 {
        if self.agnus.fmode() & 0x4000 != 0 {
            return if (u32::from(self.denise.diwstrt >> 8) ^ vpos) & 1 != 0 {
                self.denise.bpl2mod
            } else {
                self.denise.bpl1mod
            };
        }
        if plane & 1 == 0 {
            self.denise.bpl1mod
        } else {
            self.denise.bpl2mod
        }
    }

    fn advance_display_dma_modulos(&mut self, nplanes: usize, _words_per_row: usize, vpos: u32) {
        self.advance_display_dma_modulos_for_mask(plane_mask_for_count(nplanes), vpos);
    }

    fn advance_display_dma_modulos_for_mask(&mut self, plane_mask: u16, vpos: u32) {
        for plane in 0..8 {
            if plane_mask & (1u16 << plane) == 0 {
                continue;
            }
            let modulo = self.display_dma_modulo_for_plane(plane, vpos);
            self.display_dma_bplpt[plane] = ((self.display_dma_bplpt[plane] as i64)
                .wrapping_add(modulo as i64) as u32)
                & self.chip_dma_mask;
        }
        if crate::envcfg::flag("COPPERLINE_DIAG_FETCH") && (66..76).contains(&self.agnus.vpos) {
            log::info!(
                "fetch v={} plane_mask={:#04X} bplpt0={:#08X} (expect 0x03E606+{}*352={:#08X})",
                self.agnus.vpos,
                plane_mask,
                self.display_dma_bplpt[0],
                self.agnus.vpos - 66,
                0x03E606u32 + (self.agnus.vpos - 66 + 1) * 352,
            );
        }
    }

    fn record_render_write(&mut self, offset: u16, value: u16, source: BeamWriteSource) {
        let (vpos, hpos) = (self.agnus.vpos, self.agnus.hpos);
        let event = BeamRegisterWrite {
            vpos,
            hpos,
            offset,
            value,
            source,
        };
        if matches!(source, BeamWriteSource::CpuCopperIrq)
            && matches!(offset & 0x01FE, 0x180..=0x1BE)
        {
            let (target_vpos, target_hpos) = self.cpu_palette_target_beam.unwrap_or((vpos, hpos));
            if target_vpos >= CPU_COPPER_BOTTOM_PALETTE_MIN_VPOS {
                if self.cpu_palette_target_writes == 0 {
                    self.pending_beam_bottom_palette_events.clear();
                }
                self.pending_beam_bottom_palette_events
                    .push(BeamRegisterWrite {
                        vpos: target_vpos,
                        hpos: target_hpos,
                        offset,
                        value,
                        source,
                    });
            }
        }
        self.current_frame_render_events.push(event);
        if is_live_collision_relevant_custom_write(offset) {
            self.current_frame_collision_events.push(event);
        }
        if is_live_collision_control_custom_write(offset) {
            self.current_frame_collision_control_events.push(event);
            self.current_frame_collision_control_index = None;
        }
        if is_live_collision_bpldat_custom_write(offset) {
            self.current_frame_collision_bpldat_events.push(event);
            self.current_frame_collision_bpldat_index = None;
        }
        if is_live_collision_sprite_custom_write(offset) {
            self.current_frame_collision_sprite_events.push(event);
            self.current_frame_collision_sprite_index = None;
        }
        if (offset & 0x01FE) == 0x100 && value & 0x0400 != 0 {
            self.current_frame_collision_may_have_dual_playfield = true;
        }
    }

    fn record_sprite_display_enable_at(&mut self, vpos: u32, hpos: u32) {
        let Some(fb_y) = visible_framebuffer_y(
            vpos,
            self.current_frame_visible_start_vpos,
            self.current_frame_geometry.visible_lines,
        ) else {
            return;
        };
        let denise_hpos = hpos.saturating_sub(DENISE_HPOS_LAG_CCK);
        let x = framebuffer_x_for_live_collision_hpos(denise_hpos) as usize;
        self.record_sprite_display_enable_x(fb_y, x);
    }

    fn record_sprite_display_enable_for_bitplane_dma(&mut self, vpos: u32, bplcon0: u16) {
        let Some(fb_y) = visible_framebuffer_y(
            vpos,
            self.current_frame_visible_start_vpos,
            self.current_frame_geometry.visible_lines,
        ) else {
            return;
        };
        let (window_x_start, _) = live_display_window_x(
            self.denise.diwstrt,
            self.denise.diwstop,
            self.effective_diwhigh(),
        );
        let pixel_repeat = if bitplane_hires(bplcon0) || bitplane_shres(bplcon0) {
            1
        } else {
            2
        };
        let native_samples_per_pixel = if bitplane_shres(bplcon0) { 2 } else { 1 };
        let fetch_start_native_x = live_fetch_start_native_x(
            bplcon0,
            self.denise.diwstrt,
            self.effective_diwhigh(),
            self.denise.ddfstrt,
        );
        let skipped_pixels = fetch_start_native_x.div_ceil(native_samples_per_pixel) * pixel_repeat;
        let x = window_x_start.max(0) as usize + skipped_pixels;
        self.record_sprite_display_enable_x(fb_y, x);
    }

    fn record_sprite_display_enable_x(&mut self, fb_y: usize, x: usize) {
        let enable_x = &mut self.current_frame_sprite_display_enable_x_by_y[fb_y];
        *enable_x = Some(enable_x.map_or(x, |old| old.min(x)));
    }

    fn commit_pending_bottom_palette_events(&mut self) {
        if self.pending_beam_bottom_palette_events.is_empty() {
            return;
        }
        if palette_event_sequences_equivalent(
            &self.beam_bottom_palette_events,
            &self.pending_beam_bottom_palette_events,
        ) {
            let current_vpos = self
                .beam_bottom_palette_events
                .first()
                .map(|event| event.vpos)
                .unwrap_or(u32::MAX);
            let pending_vpos = self
                .pending_beam_bottom_palette_events
                .first()
                .map(|event| event.vpos)
                .unwrap_or(u32::MAX);
            if pending_vpos < current_vpos {
                self.beam_bottom_palette_events =
                    std::mem::take(&mut self.pending_beam_bottom_palette_events);
            } else {
                self.pending_beam_bottom_palette_events.clear();
            }
        } else {
            self.beam_bottom_palette_events =
                std::mem::take(&mut self.pending_beam_bottom_palette_events);
        }
    }

    fn capture_render_snapshot(&self) -> RenderRegisterSnapshot {
        RenderRegisterSnapshot {
            agnus_revision: self.agnus.revision(),
            harddis: self.harddis_active(),
            dmacon: self.agnus.dmacon,
            bplcon0: self.denise.bplcon0,
            bplcon1: self.denise.bplcon1,
            bplcon2: self.denise.bplcon2,
            bplcon3: self.denise.bplcon3,
            bplcon4: self.denise.bplcon4,
            fmode: self.agnus.fmode(),
            clxcon: self.denise.clxcon,
            clxcon2: self.denise.clxcon2,
            bplpt: self.denise.bplpt,
            bpldat: self.denise.bpldat,
            sprpt: self.denise.sprpt,
            sprpos: self.denise.sprpos,
            sprctl: self.denise.sprctl,
            sprdata: self.denise.sprdata,
            sprdatb: self.denise.sprdatb,
            spr_armed: self.denise.spr_armed,
            bpl1mod: self.denise.bpl1mod,
            bpl2mod: self.denise.bpl2mod,
            palette: self.denise.palette,
            diwstrt: self.denise.diwstrt,
            diwstop: self.denise.diwstop,
            diwhigh: self.effective_diwhigh(),
            ddfstrt: self.denise.ddfstrt,
            ddfstop: self.denise.ddfstop,
            // LOF for this frame is settled by update_interlace_long_frame
            // after the wrap; the caller patches it in (see new_frame).
            long_field: self.agnus.lof,
        }
    }

    fn note_intreq_palette_target(&mut self, val: u16) {
        if val & 0x8000 != 0 {
            return;
        }
        let clears_coper = val & crate::chipset::paula::INT_COPER != 0;
        let clears_vertb = val & crate::chipset::paula::INT_VERTB != 0;
        let handling_coper = self.delivered_irq_pending & crate::chipset::paula::INT_COPER != 0;
        if clears_coper && handling_coper {
            self.cpu_palette_target = CpuPaletteTarget::Bottom;
            self.cpu_palette_target_writes = 0;
            self.cpu_palette_target_beam = self.delivered_copper_irq_beam;
        } else if clears_vertb {
            self.cpu_palette_target = CpuPaletteTarget::Top;
            self.cpu_palette_target_writes = 0;
            self.cpu_palette_target_beam = None;
        }
        self.delivered_irq_pending &= !(val & 0x7FFF);
        if clears_coper {
            self.delivered_copper_irq_beam = None;
        }
    }

    fn write_cpu_palette_snapshot(&mut self, idx: usize, color: u16) {
        let target = self.cpu_palette_target;
        match target {
            CpuPaletteTarget::Top => {
                self.beam_top_palette.write_ocs(idx, color);
            }
            CpuPaletteTarget::Bottom => {
                self.beam_top_palette.write_ocs(idx, color);
                let target_vpos = self
                    .cpu_palette_target_beam
                    .map(|(vpos, _)| vpos)
                    .unwrap_or(self.agnus.vpos);
                if target_vpos >= CPU_COPPER_BOTTOM_PALETTE_MIN_VPOS {
                    self.beam_bottom_palette.write_ocs(idx, color);
                    self.beam_bottom_palette_valid = true;
                }
                self.cpu_palette_target_writes = self.cpu_palette_target_writes.saturating_add(1);
                if idx == 15 || idx == 31 || self.cpu_palette_target_writes >= 16 {
                    self.commit_pending_bottom_palette_events();
                    self.cpu_palette_target = CpuPaletteTarget::Top;
                    self.cpu_palette_target_writes = 0;
                    self.cpu_palette_target_beam = None;
                }
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

fn display_window_contains_vpos(diwstrt: u16, diwstop: u16, diwhigh: DiwHigh, vpos: u32) -> bool {
    if display_window_unprogrammed(diwstrt, diwstop) {
        return (RENDER_VISIBLE_START_VPOS
            ..RENDER_VISIBLE_START_VPOS + RENDER_VISIBLE_LINES as u32)
            .contains(&vpos);
    }
    let start = diw_v_start(diwstrt, diwhigh) as u32;
    let mut stop = diw_v_stop(diwstop, diwhigh) as u32;
    let mut v = vpos;
    if stop <= start {
        stop += 0x100;
        if v < start {
            v += 0x100;
        }
    }
    v >= start && v < stop
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

fn bitplane_shres(bplcon0: u16) -> bool {
    bplcon0 & BPLCON0_SHRES != 0
}

fn bitplane_hires(bplcon0: u16) -> bool {
    bplcon0 & 0x8000 != 0 && !bitplane_shres(bplcon0)
}

fn bitplane_hires_like_ddf(bplcon0: u16) -> bool {
    bitplane_hires(bplcon0) || bitplane_shres(bplcon0)
}

// Capture-side twins of the agnus fetch-mode helpers (see agnus.rs).
fn bitplane_fetch_quantum(fmode: u16) -> u32 {
    match fmode & 0x0003 {
        0 => 1,
        3 => 4,
        _ => 2,
    }
}

fn bitplane_fetch_period(bplcon0: u16, fmode: u16) -> u32 {
    bitplane_fetch_cck_per_word(bplcon0) * bitplane_fetch_quantum(fmode)
}

fn bitplane_fetch_unit(bplcon0: u16, fmode: u16) -> u32 {
    bitplane_fetch_period(bplcon0, fmode).max(8)
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
    if bitplane_shres(bplcon0) {
        2
    } else if bitplane_hires(bplcon0) {
        4
    } else {
        8
    }
}

fn chip_dma_addr_mask(chip_ram_len: usize) -> u32 {
    let bytes = chip_ram_len.next_power_of_two().clamp(2, 0x0020_0000usize);
    (bytes - 1) as u32
}

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

fn next_chip_bus_quantum_at(hpos: u32, line_cck: u32) -> u32 {
    CHIP_BUS_SLOT_CCK.min(line_cck.saturating_sub(hpos).max(1))
}

fn effective_ddf_hpos(bplcon0: u16, raw: u16) -> u16 {
    effective_ddf_start_hpos_raw(bplcon0, raw)
}

fn effective_ddf_start_hpos_raw(bplcon0: u16, raw: u16) -> u16 {
    if bitplane_hires_like_ddf(bplcon0) {
        raw & 0x00FC
    } else {
        raw & 0x00F8
    }
}

fn effective_ddf_stop_hpos(_bplcon0: u16, raw: u16) -> u16 {
    raw & 0x00FC
}

fn effective_ddf_start_hpos(bplcon0: u16, raw: u16) -> u16 {
    let start = effective_ddf_start_hpos_raw(bplcon0, raw);
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
    let (hard_start, hard_stop) = ddf_hard_bounds(harddis);
    let start = effective_ddf_start_hpos_raw(bplcon0, ddfstrt);
    let mut stop = effective_ddf_stop_hpos(bplcon0, ddfstop);
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

fn bitplane_fetch_order(bplcon0: u16, plane: usize) -> u32 {
    if bitplane_hires(bplcon0) || bitplane_shres(bplcon0) {
        return plane as u32;
    }

    let plane_num = plane + 1;
    OCS_LORES_BPL_SEQUENCE
        .iter()
        .position(|&candidate| candidate == plane_num)
        .unwrap_or(7) as u32
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
) -> Vec<LiveSpriteCollisionSource> {
    live_sprite_collision_sources_with_beam_gated_odd(lines, beam_y)
}

fn live_sprite_collision_sources_with_beam_gated_odd(
    lines: &[CapturedSpriteLine],
    beam_y: i32,
) -> Vec<LiveSpriteCollisionSource> {
    live_sprite_collision_sources_with_odd_policy(lines, beam_y, 0, true)
}

fn live_sprite_collision_sources_with_odd_policy(
    lines: &[CapturedSpriteLine],
    beam_y: i32,
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
                hstart: line.hstart,
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

fn sprite_pixel_repeat_for_control(bplcon0: u16, bplcon3: u16) -> i32 {
    match bplcon3 & BPLCON3_SPRES_MASK {
        0 => {
            if bplcon0 & BPLCON0_SHRES != 0 {
                1
            } else {
                2
            }
        }
        BPLCON3_SPRES_LORES => 2,
        BPLCON3_SPRES_HIRES | BPLCON3_SPRES_SHRES => 1,
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
) -> u16 {
    if sources.len() < 2 {
        return 0;
    }
    let x_start = x_start.max(0);
    let x_stop = x_stop.min(RENDER_FRAMEBUFFER_WIDTH);
    if x_start >= x_stop {
        return 0;
    }

    let mut clxdat = 0u16;
    for (idx, source) in sources.iter().enumerate() {
        for other in &sources[idx + 1..] {
            if source.group == other.group {
                continue;
            }
            let clx_bit = sprite_sprite_clx_bit(source.group, other.group);
            if clx_bit == 0 || clxdat & clx_bit != 0 {
                continue;
            }
            let Some((pair_x_start, pair_x_stop)) =
                live_sprite_source_pair_x_range(source, other, x_start, x_stop)
            else {
                continue;
            };
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

    let mut clxdat = 0u16;
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
                    clxdat |= sprite_sprite_clx_bit(source.source.group, other_group);
                }
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

    let mut clxdat = 0u16;
    for x in x_start..x_stop {
        let control = playfield_control.control_for_x(x);
        let Some(collision) = live_bitplane_collision_pixel_at(
            row,
            control.bplcon0,
            control.bplcon1,
            control.clxcon,
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
            let sprite_control_at_x = sprite_control.control_for_x(x);
            if !live_sprite_pixel_inside_display_window(
                sprite_control_at_x,
                beam_y,
                x,
                display_enable_x,
            ) {
                continue;
            }
            if !live_sprite_source_collision_matches(
                &source.source,
                sprite_control,
                control.clxcon,
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
    event_x: i32,
    query_x_stop: i32,
) -> i32 {
    let off = event.offset & 0x01FE;
    if !(0x140..=0x17F).contains(&off) || (off - 0x140) & 0x0006 != 0 {
        return event_x;
    }
    let base_x = (sprite_hstart_from_words(sprpos, sprctl) - RENDER_DIW_HSTART_FB0) * 2
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
    let x_start = (source.hstart - RENDER_DIW_HSTART_FB0) * 2 + i32::from(source.hsub_70ns);
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
            hstart: sprite_hstart_from_words(sprpos, sprctl),
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
    let mut x_cursor = sprite_base_x;
    for bit in (0..16).rev() {
        let sprite_control = control_replay.control_for_x(x_cursor);
        let sprite_pixel_repeat =
            sprite_pixel_repeat_for_control(sprite_control.bplcon0, sprite_control.bplcon3);
        let x_stop = x_cursor + sprite_pixel_repeat;
        if x >= x_cursor && x < x_stop {
            let low = source.words[0] & (1 << bit) != 0 || source.words[1] & (1 << bit) != 0;
            let high = source.words[2] & (1 << bit) != 0 || source.words[3] & (1 << bit) != 0;
            return if source.requires_odd_enable {
                LiveSpritePixelPresence {
                    even: false,
                    odd: low,
                }
            } else {
                LiveSpritePixelPresence {
                    even: low,
                    odd: high,
                }
            };
        }
        x_cursor = x_stop;
    }
    LiveSpritePixelPresence::default()
}

fn live_sprite_source_pixel_presence_with_control(
    source: &LiveSpriteCollisionSource,
    control: LiveCollisionControl,
    x: i32,
) -> LiveSpritePixelPresence {
    let sprite_base_x = (source.hstart - RENDER_DIW_HSTART_FB0) * 2 + i32::from(source.hsub_70ns);
    let sprite_pixel_repeat = sprite_pixel_repeat_for_control(control.bplcon0, control.bplcon3);
    let offset = x - sprite_base_x;
    if offset < 0 {
        return LiveSpritePixelPresence::default();
    }
    let bit_offset = offset / sprite_pixel_repeat;
    if !(0..16).contains(&bit_offset) {
        return LiveSpritePixelPresence::default();
    }
    let bit = 15 - bit_offset;
    let mask = 1 << bit;
    let low = source.words[0] & mask != 0 || source.words[1] & mask != 0;
    let high = source.words[2] & mask != 0 || source.words[3] & mask != 0;
    if source.requires_odd_enable {
        LiveSpritePixelPresence {
            even: false,
            odd: low,
        }
    } else {
        LiveSpritePixelPresence {
            even: low,
            odd: high,
        }
    }
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

fn live_sprite_pixel_inside_display_window(
    control: LiveCollisionControl,
    beam_y: i32,
    framebuffer_x: i32,
    display_enable_x: Option<i32>,
) -> bool {
    if control.bplcon0 & BPLCON0_ECSENA != 0 && control.bplcon3 & BPLCON3_BRDSPRT != 0 {
        return true;
    }
    if beam_y < 0 {
        return false;
    }
    if display_enable_x.is_none_or(|enable_x| framebuffer_x < enable_x) {
        return false;
    }
    display_window_contains_vpos(
        control.diwstrt,
        control.diwstop,
        control.diwhigh,
        beam_y as u32,
    ) && live_display_window_contains_x(
        control.diwstrt,
        control.diwstop,
        control.diwhigh,
        framebuffer_x,
    )
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
            control.bplcon0,
            control.bplcon1,
            control.clxcon,
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
    bplcon0: u16,
    bplcon1: u16,
    bplcon3: u16,
    clxcon: u16,
    diwstrt: u16,
    diwstop: u16,
    diwhigh: DiwHigh,
    ddfstrt: u16,
    bpldat: [u16; 8],
}

impl LiveCollisionControl {
    fn from_current(
        bplcon0: u16,
        bplcon1: u16,
        bplcon3: u16,
        clxcon: u16,
        diwstrt: u16,
        diwstop: u16,
        diwhigh: DiwHigh,
        ddfstrt: u16,
        bpldat: [u16; 8],
    ) -> Self {
        Self {
            bplcon0,
            bplcon1,
            bplcon3,
            clxcon,
            diwstrt,
            diwstop,
            diwhigh,
            ddfstrt,
            bpldat,
        }
    }

    fn from_snapshot(snapshot: RenderRegisterSnapshot) -> Self {
        Self {
            bplcon0: snapshot.bplcon0,
            bplcon1: snapshot.bplcon1,
            bplcon3: snapshot.bplcon3,
            clxcon: snapshot.clxcon,
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
            0x098 => self.clxcon = value,
            0x100 => self.bplcon0 = value,
            0x102 => self.bplcon1 = value,
            0x106 => self.bplcon3 = value,
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
) -> u16 {
    if sources.is_empty() {
        return 0;
    }
    let x_start = x_start.max(0);
    let x_stop = x_stop.min(RENDER_FRAMEBUFFER_WIDTH);
    if x_start >= x_stop {
        return 0;
    }

    let mut clxdat = 0u16;
    for source in sources {
        let (source_start, source_stop) = live_sprite_source_framebuffer_bounds(source);
        let source_x_start = x_start.max(source_start);
        let source_x_stop = x_stop.min(source_stop);
        if source_x_start >= source_x_stop {
            continue;
        }
        for x in source_x_start..source_x_stop {
            let sprite_control_at_x = sprite_control.control_for_x(x);
            if !live_sprite_pixel_inside_display_window(
                sprite_control_at_x,
                beam_y,
                x,
                display_enable_x,
            ) {
                continue;
            }
            let control = playfield_control.control_for_x(x);
            if !live_sprite_source_collision_matches(source, sprite_control, control.clxcon, x) {
                continue;
            }
            let Some(collision) = live_bitplane_collision_pixel_at(
                row,
                control.bplcon0,
                control.bplcon1,
                control.clxcon,
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
                clxdat |= 1 << (source.group + 1);
            }
            if collision.pf2_match {
                clxdat |= 1 << (source.group + 5);
            }
        }
    }

    clxdat
}

fn live_manual_bpl_collision_bits_in_range(
    frame_base: RenderRegisterSnapshot,
    bpldat_index: &BeamEventIndex,
    sprite_index: &BeamEventIndex,
    control_replay: &LiveCollisionLineReplay,
    sprite_lines: &[CapturedSpriteLine],
    beam_y: i32,
    x_start: i32,
    x_stop: i32,
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

/// Live collisions evaluate at most the classic 6 bitplanes until the AGA
/// CLXCON2 collision extensions are interpreted (plan 3.4).
const COLLISIONS_AGA_DECODE: bool = false;

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
    display_enable_x: Option<i32>,
) -> u16 {
    const MANUAL_BPL_WORD_BITS: usize = 16;

    let sources = live_sprite_playfield_collision_sources(sprite_lines, beam_y);
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
        // Collision sampling stays on the pre-AGA 6-plane decode until
        // CLXCON2 / 8-plane collisions land (plan 3.4).
        let mode = BitplaneMode::from_bplcon0(source_control.bplcon0, COLLISIONS_AGA_DECODE);
        let nplanes = mode.display_planes().min(planes.len());
        let dual_playfield = source_control.bplcon0 & 0x0400 != 0;
        let mut idx = 0u8;
        let mut word_active = false;
        for plane in 0..nplanes {
            let delay = live_scroll_for_plane(source_control.bplcon1, plane);
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
                let delay = live_scroll_for_plane(source_control.bplcon1, plane);
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
            let collision =
                live_playfield_collision_pixel(idx, nplanes, source_control.clxcon, dual_playfield);
            for dx in 0..pixel_repeat {
                let x = x_cursor + dx;
                if x < x_start || x >= x_stop {
                    continue;
                }
                let pixel_control = control_replay.control_for_x(x);
                if !display_window_contains_vpos(
                    pixel_control.diwstrt,
                    pixel_control.diwstop,
                    pixel_control.diwhigh,
                    beam_y as u32,
                ) || !live_display_window_contains_x(
                    pixel_control.diwstrt,
                    pixel_control.diwstop,
                    pixel_control.diwhigh,
                    x,
                ) {
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
    bplcon0: u16,
    bplcon1: u16,
    clxcon: u16,
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
    let fetch_start_native_x = live_fetch_start_native_x(bplcon0, diwstrt, diwhigh, ddfstrt);
    let relative_native_x = output_native_x.checked_sub(fetch_start_native_x)?;
    let native_x =
        relative_native_x + live_fetch_origin_native_offset(bplcon0, diwstrt, diwhigh, ddfstrt);
    let fetched_pixels = row.words_per_row * 16;
    // Collision sampling stays on the pre-AGA 6-plane decode until CLXCON2 /
    // 8-plane collisions land (plan 3.4).
    let mode = BitplaneMode::from_bplcon0(bplcon0, COLLISIONS_AGA_DECODE);
    let nplanes = mode.display_planes().min(row.nplanes).min(6);
    let dma_planes = mode.dma_planes().min(nplanes);
    let mut idx = 0u8;
    for plane in 0..nplanes {
        let delay = live_scroll_for_plane(bplcon1, plane);
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
            let delay = live_scroll_for_plane(bplcon1, plane);
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
        nplanes,
        clxcon,
        bplcon0 & 0x0400 != 0,
    ))
}

fn live_playfield_collision_pixel(
    idx: u8,
    nplanes: usize,
    clxcon: u16,
    dual_playfield: bool,
) -> LivePlayfieldCollisionPixel {
    let even_match = live_clxcon_planes_match(idx, nplanes, clxcon, 1);
    let odd_match_raw = live_clxcon_planes_match(idx, nplanes, clxcon, 0);
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

fn live_clxcon_planes_match(idx: u8, nplanes: usize, clxcon: u16, first_plane: usize) -> bool {
    let mut matches = true;
    for plane in (first_plane..nplanes.min(6)).step_by(2) {
        if clxcon & (1 << (6 + plane)) == 0 {
            continue;
        }
        let desired = clxcon & (1 << plane) != 0;
        let actual = idx & (1 << plane) != 0;
        matches &= desired == actual;
    }
    matches
}

fn live_scroll_for_plane(bplcon1: u16, plane: usize) -> usize {
    if plane & 1 != 0 {
        ((bplcon1 >> 4) & 0x000F) as usize
    } else {
        (bplcon1 & 0x000F) as usize
    }
}

fn live_fetch_start_native_x(bplcon0: u16, diwstrt: u16, diwhigh: DiwHigh, ddfstrt: u16) -> usize {
    (-live_fetch_origin_native_shift(bplcon0, diwstrt, diwhigh, ddfstrt)).max(0) as usize
}

fn live_fetch_origin_native_offset(
    bplcon0: u16,
    diwstrt: u16,
    diwhigh: DiwHigh,
    ddfstrt: u16,
) -> usize {
    live_fetch_origin_native_shift(bplcon0, diwstrt, diwhigh, ddfstrt).max(0) as usize
}

fn live_fetch_origin_native_shift(
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
    let ddf_native_shift =
        (effective_ddf_start_hpos(bplcon0, ddfstrt) as i32 - standard_ddf) * ddf_native_scale;
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
    matches!(off, 0x096 | 0x09E | 0x0A0..=0x0DF | 0x1C0 | 0x1DC)
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

#[cfg(test)]
mod tests {
    use super::{
        bitplane_words_per_row, clipped_display_rows_before_visible, display_window_contains_vpos,
        diw_h_start, diw_h_stop, diw_v_start, diw_v_stop, framebuffer_x_for_live_collision_hpos,
        live_bitplane_collision_bits, live_display_window_x, live_manual_sprite_collision_sources,
        live_sprite_playfield_collision_bits_in_range, live_sprite_sprite_collision_bits,
        visible_start_vpos_for_diw, BeamChipRamWrite, BeamRegisterWrite, BeamWriteSource, Bus,
        CapturedBitplaneRow, CapturedSpriteLine, ChipBusOwner, CpuBusAccessKind, DeviceClock,
        DisplaySpriteControl, DisplaySpriteDmaState, DisplaySpriteLineData, LiveCollisionControl,
        LiveCollisionLineReplay, LiveSpriteCollisionSource, RenderRegisterSnapshot,
        BLITTER_SLOWDOWN_CPU_MISS_LIMIT, BLTCON1_DOFF, BPLCON0_ECSENA, BPLCON3_BRDSPRT,
        BPLCON3_SPRES_HIRES, COPPER_BUS_LOCKOUT_HPOS, DENISE_HPOS_LAG_CCK, DMACON_AUD_MASK,
        DMACON_BLTEN, DMACON_BLTPRI, DMACON_BPLEN, DMACON_SPREN, RENDER_COPPER_WAIT_HPOS_FB0,
        RENDER_DIW_HSTART_FB0, RENDER_MIN_OVERSCAN_START_VPOS, RENDER_VISIBLE_LINES,
        RENDER_VISIBLE_START_VPOS, SPRITE_DMA_PAIR_CAPTURE_HPOS,
    };
    use crate::audio::AudioSink;
    use crate::chipset::agnus::{
        AgnusRevision, AgnusTick, VideoStandard, BEAMCON0_DUAL, BEAMCON0_HARDDIS, BEAMCON0_PAL,
        BEAMCON0_VARBEAMEN, COLORCLOCKS_PER_LINE, NTSC_LONG_COLORCLOCKS_PER_LINE,
    };
    use crate::chipset::cia::{
        REG_CRA, REG_CRB, REG_ICR, REG_PRA, REG_TAHI, REG_TALO, REG_TBHI, REG_TBLO, REG_TODHI,
        REG_TODLO, REG_TODMID,
    };
    use crate::chipset::copper::{CopperWait, DMACON_COPEN};
    use crate::chipset::denise::{rgb12_to_rgba8, DeniseRevision, DiwHigh, COLOR_TRANSPARENCY_BIT};
    use crate::chipset::paula::{
        Paula, DMACON_DMAEN, INT_AUD0, INT_BLIT, INT_COPER, INT_EXTER, INT_MASTER, INT_PORTS,
        INT_VERTB, NTSC_AUDIO_MIN_PERIOD_CCK, PAL_AUDIO_MIN_PERIOD_CCK, PAULA_CLOCK_HZ,
    };
    use crate::floppy::{FloppyController, ADF_SIZE};
    use crate::memory::Memory;
    use crate::serial::SerialSink;
    use crate::video::beam::BeamEventIndex;
    use crate::video::{bitplane, FB_PIXELS, FB_WIDTH};
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    const STANDARD_DIW_HSTART: i32 = 0x81;
    const STANDARD_VISIBLE_X0: usize = ((STANDARD_DIW_HSTART - RENDER_DIW_HSTART_FB0) * 2) as usize;
    const RENDER_COLOR_WRITE_HPOS_FB0: u32 = 0x34;
    static BUS_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn render_color_write_x(hpos: u32) -> usize {
        hpos.saturating_sub(RENDER_COLOR_WRITE_HPOS_FB0)
            .saturating_mul(4)
            .min(FB_WIDTH as u32) as usize
    }

    struct NoopSerial;

    impl SerialSink for NoopSerial {
        fn write_byte(&mut self, _b: u8) {}
        fn flush(&mut self) {}
    }

    struct NoopAudio;

    impl AudioSink for NoopAudio {
        fn push(&mut self, _left: f32, _right: f32) {}
        fn flush(&mut self) {}
    }

    type SharedFrames = Rc<RefCell<Vec<(f32, f32)>>>;

    struct CollectAudio {
        frames: SharedFrames,
    }

    impl AudioSink for CollectAudio {
        fn push(&mut self, left: f32, right: f32) {
            self.frames.borrow_mut().push((left, right));
        }

        fn flush(&mut self) {}
    }

    fn empty_bus() -> Bus {
        empty_bus_with_chip_ram(512 * 1024)
    }

    fn empty_bus_with_chip_ram(chip_ram_bytes: usize) -> Bus {
        Bus::new(
            Memory {
                chip_ram: vec![0; chip_ram_bytes],
                slow_ram: Vec::new(),
                rom: vec![0; 512 * 1024],
                overlay: true,
                zorro: crate::zorro::ZorroChain::default(),
                extended_rom: Vec::new(),
                extended_rom_base: 0,
            },
            Paula::new(Box::new(NoopSerial), Box::new(NoopAudio)),
            FloppyController::default(),
        )
    }

    fn empty_bus_with_collect_audio() -> (Bus, SharedFrames) {
        let frames = Rc::new(RefCell::new(Vec::new()));
        let bus = Bus::new(
            Memory {
                chip_ram: vec![0; 512 * 1024],
                slow_ram: Vec::new(),
                rom: vec![0; 512 * 1024],
                overlay: true,
                zorro: crate::zorro::ZorroChain::default(),
                extended_rom: Vec::new(),
                extended_rom_base: 0,
            },
            Paula::new(
                Box::new(NoopSerial),
                Box::new(CollectAudio {
                    frames: Rc::clone(&frames),
                }),
            ),
            FloppyController::default(),
        );
        (bus, frames)
    }

    fn temp_bus_adf() -> std::path::PathBuf {
        let id = BUS_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("copperline-bus-{}-{id}.adf", std::process::id()))
    }

    /// Drive the Amiga-side KDAT handshake the way the boot ROM does:
    /// SPMODE out (KDAT low), >= 85 us of device time, SPMODE back in.
    fn keyboard_handshake(bus: &mut Bus) {
        let cra = (REG_CRA as u64) << 8;
        let _ = bus.cia_a_write(cra, 1, 0x40);
        bus.advance_devices(310);
        let _ = bus.cia_a_write(cra, 1, 0x00);
    }

    fn write_chip_word(bus: &mut Bus, off: usize, val: u16) {
        let bytes = val.to_be_bytes();
        bus.mem.chip_ram[off] = bytes[0];
        bus.mem.chip_ram[off + 1] = bytes[1];
    }

    /// Build a captured bitplane row sized to `words_per_row`, placing
    /// `plane_words[p]` into plane `p` (each a list of (word_index,
    /// value)). The render path only accepts a captured row whose
    /// `words_per_row` matches the value it computes from the line's
    /// display window, so beam-replay tests must size their injected
    /// rows to that width rather than a single word.
    fn captured_row(
        nplanes: usize,
        words_per_row: usize,
        plane_words: &[&[(usize, u16)]],
    ) -> CapturedBitplaneRow {
        let mut planes: [Vec<u16>; 8] = Default::default();
        for (p, plane) in planes.iter_mut().enumerate().take(nplanes) {
            *plane = vec![0u16; words_per_row];
            if let Some(words) = plane_words.get(p) {
                for &(idx, value) in *words {
                    plane[idx] = value;
                }
            }
        }
        CapturedBitplaneRow {
            nplanes,
            words_per_row,
            planes,
        }
    }

    fn run_copper_moves_at(bus: &mut Bus, cop1: usize, vpos: u32, hpos: u32, moves: &[(u16, u16)]) {
        for (idx, &(register, value)) in moves.iter().enumerate() {
            let off = cop1 + idx * 4;
            write_chip_word(bus, off, register);
            write_chip_word(bus, off + 2, value);
        }
        let end = cop1 + moves.len() * 4;
        write_chip_word(bus, end, 0xFFFF);
        write_chip_word(bus, end + 2, 0xFFFE);

        bus.agnus.dmacon |= DMACON_DMAEN | DMACON_COPEN;
        bus.agnus.vpos = vpos;
        bus.agnus.hpos = hpos;
        bus.copper.jump(cop1 as u32);
        // Each MOVE spans 4 color clocks (fetch, idle, fetch+write, idle), so
        // advance four per move plus a little slack for the trailing fetch.
        bus.advance_chipset((moves.len() * 4 + 2) as u32);
    }

    fn write_copper_wait_then_move(
        bus: &mut Bus,
        cop1: usize,
        wait_first: u16,
        wait_second: u16,
        move_register: u16,
        move_value: u16,
    ) {
        write_chip_word(bus, cop1, wait_first);
        write_chip_word(bus, cop1 + 2, wait_second);
        write_chip_word(bus, cop1 + 4, move_register);
        write_chip_word(bus, cop1 + 6, move_value);
        write_chip_word(bus, cop1 + 8, 0xFFFF);
        write_chip_word(bus, cop1 + 10, 0xFFFE);
    }

    fn run_copper_guarded_move(revision: AgnusRevision, register: u16, cdang: bool) -> Bus {
        let mut bus = empty_bus();
        bus.set_agnus_revision(revision);
        let cop1 = 0x0100usize;
        write_chip_word(&mut bus, cop1, register);
        write_chip_word(&mut bus, cop1 + 2, 0x1234);
        write_chip_word(&mut bus, cop1 + 4, 0x0182);
        write_chip_word(&mut bus, cop1 + 6, 0x0ABC);
        write_chip_word(&mut bus, cop1 + 8, 0xFFFF);
        write_chip_word(&mut bus, cop1 + 10, 0xFFFE);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN;
        bus.agnus.hpos = 0x20;
        if cdang {
            assert!(!bus.custom_write(0x02E, 2, 0x0002));
        }
        assert!(!bus.custom_write(0x080, 2, 0x0000));
        assert!(!bus.custom_write(0x082, 2, cop1 as u64));
        assert!(!bus.custom_write(0x088, 2, 0xFFFF));
        bus.advance_chipset(8);
        bus
    }

    fn bus_with_pending_two_word_a_to_d_blit() -> Bus {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BLTEN;
        bus.agnus.hpos = 0x20;
        bus.blitter.bltcon0 = 0x09F0;
        bus.blitter.bltcon1 = 0;
        bus.blitter.bltafwm = 0xFFFF;
        bus.blitter.bltalwm = 0xFFFF;
        bus.blitter.bltapt = 0x10;
        bus.blitter.bltdpt = 0x20;
        write_chip_word(&mut bus, 0x10, 0x1111);
        write_chip_word(&mut bus, 0x12, 0x2222);
        write_chip_word(&mut bus, 0x14, 0x3333);

        assert!(!bus.custom_write(0x058, 2, ((1 << 6) | 2) as u64));
        assert!(bus.blitter.busy);
        assert_eq!(bus.next_blitter_completion_cck(), Some(8));
        bus
    }

    fn assert_busy_blitter_register_write_drains_current_blit<F>(
        off: u16,
        value: u16,
        assert_latched: F,
    ) where
        F: FnOnce(&Bus),
    {
        let mut bus = bus_with_pending_two_word_a_to_d_blit();

        assert!(!bus.custom_write(off as u64, 2, value as u64));

        assert!(!bus.blitter.busy);
        assert_eq!(&bus.mem.chip_ram[0x20..0x24], &[0x11, 0x11, 0x22, 0x22]);
        assert_ne!(bus.paula.intreq & INT_BLIT, 0);
        assert_latched(&bus);
    }

    fn write_chip_word_wrapping(bus: &mut Bus, off: usize, val: u16) {
        let len = bus.mem.chip_ram.len();
        let bytes = val.to_be_bytes();
        bus.mem.chip_ram[off % len] = bytes[0];
        bus.mem.chip_ram[(off + 1) % len] = bytes[1];
    }

    fn sprite_control_words(vstart: u16, vstop: u16, hstart: u16) -> (u16, u16) {
        let pos = ((vstart & 0x00FF) << 8) | ((hstart >> 1) & 0x00FF);
        let ctl = ((vstop & 0x00FF) << 8)
            | ((vstart & 0x0100) >> 6)
            | ((vstop & 0x0100) >> 7)
            | (hstart & 0x0001);
        (pos, ctl)
    }

    #[test]
    fn realtime_clock_produces_paula_cck_from_elapsed_wall_time() {
        let mut clock = DeviceClock::default();
        clock.set_realtime_enabled(true);
        let start = Instant::now();
        clock.realtime_anchor = Some(start);

        assert_eq!(
            clock.realtime_cck_due(start + Duration::from_secs(1)),
            PAULA_CLOCK_HZ
        );
    }

    #[test]
    fn realtime_clock_carries_fractional_cck() {
        let mut clock = DeviceClock::default();
        clock.set_realtime_enabled(true);
        let start = Instant::now();
        clock.realtime_anchor = Some(start);

        assert_eq!(clock.realtime_cck_due(start + Duration::from_nanos(1)), 0);
        assert_eq!(clock.realtime_cck_due(start + Duration::from_nanos(282)), 1);
    }

    #[test]
    fn pending_vbi_collapses_into_intreq_latch() {
        let mut bus = empty_bus();
        bus.paula.intreq = INT_VERTB;
        bus.pending_vbi = 1;

        bus.flush_pending_vbi();
        assert_eq!(bus.paula.intreq, INT_VERTB);
        assert_eq!(bus.pending_vbi, 0);

        bus.paula.intreq = 0;
        bus.pending_vbi = 1;
        bus.flush_pending_vbi();
        assert_eq!(bus.paula.intreq, INT_VERTB);
        assert_eq!(bus.pending_vbi, 0);
    }

    #[test]
    fn frame_event_deadline_tracks_next_vbi() {
        let mut bus = empty_bus();
        bus.agnus.vpos = crate::chipset::agnus::PAL_LINES - 1;
        bus.agnus.hpos = COLORCLOCKS_PER_LINE - 4;

        assert_eq!(bus.next_frame_event_cck(), 4);
        let tick = bus.advance_devices(4);
        assert_eq!(tick.new_frames, 1);
        assert_ne!(bus.paula.intreq & INT_VERTB, 0);
    }

    #[test]
    fn display_start_deadline_tracks_snapshot_boundary() {
        let mut bus = empty_bus();
        bus.agnus.vpos = RENDER_VISIBLE_START_VPOS - 1;
        bus.agnus.hpos = COLORCLOCKS_PER_LINE - 3;
        bus.mem.chip_ram[0] = 0x12;

        assert_eq!(bus.next_display_start_event_cck(), Some(3));
        let tick = bus.advance_chipset(3);
        assert_eq!(tick.new_lines, 1);
        assert!(bus.current_frame_display_snapshot_taken);
        assert_eq!(bus.current_frame_chip_ram[0], 0x12);
        assert_eq!(bus.next_display_start_event_cck(), None);
    }

    #[test]
    fn display_start_deadline_tracks_early_vertical_overscan_diw() {
        let mut bus = empty_bus();
        bus.denise.diwstrt = 0x1C81;
        bus.agnus.vpos = RENDER_MIN_OVERSCAN_START_VPOS - 1;
        bus.agnus.hpos = COLORCLOCKS_PER_LINE - 3;
        bus.mem.chip_ram[0] = 0x12;

        assert_eq!(bus.next_display_start_event_cck(), Some(3));
        let tick = bus.advance_chipset(3);
        assert_eq!(tick.new_lines, 1);
        assert!(bus.current_frame_display_snapshot_taken);
        assert_eq!(
            bus.current_frame_visible_start_vpos,
            RENDER_MIN_OVERSCAN_START_VPOS
        );
        assert_eq!(bus.current_frame_chip_ram[0], 0x12);
        assert_eq!(bus.next_display_start_event_cck(), None);
    }

    #[test]
    fn cia_b_tod_alarm_deadline_tracks_hsync() {
        let mut bus = empty_bus();
        bus.agnus.hpos = COLORCLOCKS_PER_LINE - 5;
        bus.cia_b.write(REG_TODHI, 0);
        bus.cia_b.write(REG_TODMID, 0);
        bus.cia_b.write(REG_TODLO, 0);
        bus.cia_b.write(REG_CRB, 0x80);
        bus.cia_b.write(REG_TODHI, 0);
        bus.cia_b.write(REG_TODMID, 0);
        bus.cia_b.write(REG_TODLO, 2);
        bus.cia_b.write(REG_CRB, 0);

        assert_eq!(
            bus.next_cia_b_tod_alarm_cck(),
            Some(5 + COLORCLOCKS_PER_LINE)
        );
        let tick = bus.advance_devices(5 + COLORCLOCKS_PER_LINE);
        assert_eq!(tick.new_lines, 2);
        assert_ne!(bus.cia_b.read(REG_ICR) & 0x04, 0);
    }

    #[test]
    fn cia_b_mask_enable_propagates_latched_timer_interrupt_to_paula() {
        let mut bus = empty_bus();
        let addr = |reg: usize| (reg as u64) << 8;

        let _ = bus.cia_b_write(addr(REG_TALO), 1, 0);
        let _ = bus.cia_b_write(addr(REG_TAHI), 1, 0);
        let _ = bus.cia_b_write(addr(REG_CRA), 1, 0x01);

        assert!(!bus.cia_b.tick(1));
        assert_eq!(bus.paula.intreq & INT_EXTER, 0);

        let _ = bus.cia_b_write(addr(REG_ICR), 1, 0x80 | 0x01);

        assert_ne!(bus.paula.intreq & INT_EXTER, 0);
    }

    #[test]
    fn floppy_index_flag_sync_delay_is_visible_to_cia_b_icr_polling() {
        let path = temp_bus_adf();
        std::fs::write(&path, vec![0u8; ADF_SIZE]).unwrap();
        let mut bus = empty_bus();
        bus.floppy.insert_disk_image(0, path.clone(), true).unwrap();
        bus.floppy.write_prb(0x77);

        let index_cck = bus
            .floppy
            .next_index_pulse_cck()
            .expect("motor-on selected disk should report the next index edge");
        assert!(index_cck > 1);
        assert_eq!(bus.cia_b.read(REG_ICR) & 0x10, 0);

        bus.advance_devices(index_cck - 1);
        assert_eq!(bus.cia_b.read(REG_ICR) & 0x10, 0);

        bus.advance_devices(1);
        assert_eq!(bus.cia_b.read(REG_ICR) & 0x10, 0);

        bus.advance_devices(1);
        assert_ne!(bus.cia_b.read(REG_ICR) & 0x10, 0);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn realtime_slice_bus_advance_ticks_shared_device_clock() {
        let mut bus = empty_bus();
        bus.device_clock.realtime_enabled = true;

        bus.record_slice_bus_advance(2, AgnusTick::default());
        bus.flush_timed_devices();

        assert_eq!(bus.device_clock.cia_tick_remainder_cck, 2);
    }

    #[test]
    fn real_mode_cpu_bus_advance_ticks_cia_without_double_counting() {
        let mut bus = empty_bus();
        bus.cia_b.write(REG_TBLO, 4);
        bus.cia_b.write(REG_TBHI, 0);
        bus.cia_b.write(REG_CRB, 0x11);

        // Device ticks are deferred from the per-access advance and applied in
        // one batch at the next observation/boundary; flush to apply them here.
        bus.record_slice_bus_advance(10, AgnusTick::default());
        bus.flush_timed_devices();
        assert_eq!(bus.cia_b.tb_count, 2);

        // The cycle-exact model has no post-slice reconciliation: a slice whose
        // bus time fully covered its device time advances nothing afterwards.
        // (advance_devices flushes pending first, then ticks its own 0 cck.)
        bus.advance_devices(0);
        assert_eq!(bus.cia_b.tb_count, 2);
    }

    #[test]
    fn audio_time_flushes_before_audio_register_write() {
        let (mut bus, frames) = empty_bus_with_collect_audio();
        bus.paula.set_led_filter_enabled(false);
        bus.mem.chip_ram[0] = 0x7F;
        bus.mem.chip_ram[1] = 0x7F;
        bus.mem.chip_ram[2] = 0x7F;
        bus.mem.chip_ram[3] = 0x7F;
        bus.paula.write_audio_reg(0x00, 0);
        bus.paula.write_audio_reg(0x02, 0);
        bus.paula.write_audio_reg(0x04, 2);
        bus.paula.write_audio_reg(0x06, 80);
        bus.paula.write_audio_reg(0x08, 64);
        bus.agnus.write_dmacon(0x8000 | DMACON_DMAEN | 0x0001);

        bus.advance_chipset(400);
        frames.borrow_mut().clear();

        let _ = bus.custom_write(0xDFF0A8, 2, 0);
        let frames = frames.borrow();
        assert!(
            frames.iter().any(|(left, _)| left.abs() > 0.5),
            "pending audio should be mixed with the old volume before AUD0VOL is changed: {frames:?}"
        );
    }

    #[test]
    fn audio_irq_deadline_accounts_for_pending_audio_time() {
        let mut bus = empty_bus();
        bus.paula.set_audio_min_period_cck(1);
        bus.mem.chip_ram[0] = 0x11;
        bus.mem.chip_ram[1] = 0x22;
        bus.mem.chip_ram[2] = 0x33;
        bus.mem.chip_ram[3] = 0x44;
        bus.paula.write_audio_reg(0x00, 0);
        bus.paula.write_audio_reg(0x02, 0);
        bus.paula.write_audio_reg(0x04, 2);
        bus.paula.write_audio_reg(0x06, 10);
        bus.paula.write_audio_reg(0x08, 64);
        bus.agnus.write_dmacon(0x8000 | DMACON_DMAEN | 0x0001);

        let _ = bus.paula.tick_audio(1, bus.agnus.dmacon, &bus.mem.chip_ram);
        assert_eq!(bus.next_audio_irq_cck(), Some(29));

        bus.audio_pending_cck = 28;
        assert_eq!(bus.next_audio_irq_cck(), Some(1));
    }

    #[test]
    fn enabled_audio_dma_reserves_only_actively_fetching_channel_slots() {
        let mut bus = empty_bus();
        // Channel 0 enabled but with no pending DMA request yet: its slot is NOT
        // reserved (a fixed audio slot is free for the CPU/blitter on lines the
        // channel does not fetch).
        bus.agnus.dmacon = DMACON_DMAEN | 0x0001;
        bus.agnus.hpos = 0x00F;
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Idle);

        // Arming the channel (DMA-enable edge) raises a pending request, so now
        // its slot (0x00F) reserves.
        let _ = bus.paula.advance_audio(0, bus.agnus.dmacon);
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Audio);

        // The even gap after it (0x010) is not an audio cycle, and the other
        // channels' slots (0x011/0x013/0x015) are free -- those channels are
        // disabled -- so the CPU/blitter may use all of these.
        for hpos in [0x010, 0x011, 0x013, 0x015, 0x016] {
            bus.agnus.hpos = hpos;
            assert_eq!(
                bus.scheduled_dma_owner(false),
                ChipBusOwner::Idle,
                "hpos {hpos:#05X} should not be reserved for audio"
            );
        }

        // Channels 1 and 3 enabled and armed: only their slots (0x011, 0x015)
        // reserve while their requests are pending.
        bus.agnus.dmacon = DMACON_DMAEN | 0x000A;
        let _ = bus.paula.advance_audio(0, bus.agnus.dmacon);
        for (hpos, owner) in [
            (0x00F, ChipBusOwner::Idle),
            (0x011, ChipBusOwner::Audio),
            (0x013, ChipBusOwner::Idle),
            (0x015, ChipBusOwner::Audio),
        ] {
            bus.agnus.hpos = hpos;
            assert_eq!(bus.scheduled_dma_owner(false), owner, "hpos {hpos:#05X}");
        }

        // No channels enabled: nothing reserved.
        bus.agnus.dmacon = DMACON_DMAEN;
        bus.agnus.hpos = 0x00F;
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Idle);
    }

    #[test]
    fn video_standard_selects_paula_audio_dma_period_floor() {
        let mut bus = empty_bus();
        assert_eq!(bus.paula.audio_min_period_cck(), PAL_AUDIO_MIN_PERIOD_CCK);

        bus.set_video_standard(VideoStandard::Ntsc);
        assert_eq!(bus.paula.audio_min_period_cck(), NTSC_AUDIO_MIN_PERIOD_CCK);
        bus.reset_for_keyboard_reset();
        assert_eq!(bus.paula.audio_min_period_cck(), NTSC_AUDIO_MIN_PERIOD_CCK);

        bus.set_video_standard(VideoStandard::Pal);
        assert_eq!(bus.paula.audio_min_period_cck(), PAL_AUDIO_MIN_PERIOD_CCK);
    }

    #[test]
    fn ecs_htotal_varbeamen_scales_paula_audio_dma_period_floor() {
        // Keep the PAL bit set alongside VARBEAMEN so the machine stays PAL
        // while enabling the programmable beam (the PAL bit is authoritative
        // on ECS; VARBEAMEN alone would select NTSC totals).
        let beamcon0_pal_varbeam = (BEAMCON0_PAL | BEAMCON0_VARBEAMEN) as u64;
        let mut bus = empty_bus();
        assert!(!bus.custom_write(0xDFF1C0, 2, 113));
        assert!(!bus.custom_write(0xDFF1DC, 2, beamcon0_pal_varbeam));
        assert_eq!(bus.agnus.current_line_cck(), COLORCLOCKS_PER_LINE);
        assert_eq!(bus.paula.audio_min_period_cck(), PAL_AUDIO_MIN_PERIOD_CCK);

        bus.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        assert!(!bus.custom_write(0xDFF1C0, 2, 113));
        assert_eq!(bus.paula.audio_min_period_cck(), PAL_AUDIO_MIN_PERIOD_CCK);

        assert!(!bus.custom_write(0xDFF1DC, 2, beamcon0_pal_varbeam));
        assert_eq!(bus.agnus.current_line_cck(), 114);
        assert_eq!(bus.paula.audio_min_period_cck(), 62);

        assert!(!bus.custom_write(0xDFF1C0, 2, (COLORCLOCKS_PER_LINE - 1) as u64));
        assert_eq!(bus.agnus.current_line_cck(), COLORCLOCKS_PER_LINE);
        assert_eq!(bus.paula.audio_min_period_cck(), PAL_AUDIO_MIN_PERIOD_CCK);

        assert!(!bus.custom_write(0xDFF1C0, 2, 113));
        assert_eq!(bus.paula.audio_min_period_cck(), 62);
        bus.set_agnus_revision(AgnusRevision::Ocs);
        assert_eq!(bus.agnus.current_line_cck(), COLORCLOCKS_PER_LINE);
        assert_eq!(bus.paula.audio_min_period_cck(), PAL_AUDIO_MIN_PERIOD_CCK);
    }

    #[test]
    fn beamcon0_dual_warns_once_on_ecs() {
        let mut bus = empty_bus();
        bus.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        assert!(!bus.uhres_dual_warned);
        // PAL bit keeps the standard; DUAL trips the one-time UHRES warning.
        bus.custom_write(0xDFF1DC, 2, (BEAMCON0_PAL | BEAMCON0_DUAL) as u64);
        assert!(bus.uhres_dual_warned);
        // A second DUAL write must not re-arm (no per-write log spam).
        bus.custom_write(0xDFF1DC, 2, (BEAMCON0_PAL | BEAMCON0_DUAL) as u64);
        assert!(bus.uhres_dual_warned);
    }

    #[test]
    fn harddis_widens_bitplane_arbitration_window() {
        // A 4-plane lores display with ddfstop past the 0xD8 hard stop. The
        // 22nd fetch word (hpos ~0xE0..0xE7) exists only when HARDDIS relaxes
        // the ceiling to 0xE0; without it, stop clamps to 0xD8 and the last
        // fetch hpos is 0xDF. hpos 0xE1 is the plane-4 fetch of that extra word.
        let mut bus = empty_bus();
        bus.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.denise.bplcon0 = 0x4200; // 4 planes, lores
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x00E0;
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0xF4C1;
        bus.agnus.vpos = 0x40;
        bus.agnus.hpos = 0x0E1;

        // PAL bit set, no HARDDIS: stop clamps to 0xD8 -> 0xE1 is past the window.
        assert!(!bus.custom_write(0xDFF1DC, 2, BEAMCON0_PAL as u64));
        assert_ne!(bus.scheduled_dma_owner(false), ChipBusOwner::Bitplane);

        // HARDDIS relaxes the stop to 0xE0, adding the 22nd fetch word.
        assert!(!bus.custom_write(0xDFF1DC, 2, (BEAMCON0_PAL | BEAMCON0_HARDDIS) as u64));
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Bitplane);
    }

    #[test]
    fn audio_dma_enable_sets_intreq_on_next_paula_tick() {
        let mut bus = empty_bus();
        bus.mem.chip_ram[0] = 0x12;
        bus.mem.chip_ram[1] = 0x34;

        let _ = bus.custom_write(0xDFF0A0, 2, 0);
        let _ = bus.custom_write(0xDFF0A2, 2, 0);
        let _ = bus.custom_write(0xDFF0A4, 2, 1);
        let _ = bus.custom_write(0xDFF0A6, 2, 8);
        let _ = bus.custom_write(0xDFF0A8, 2, 64);

        assert!(!bus.custom_write(0xDFF096, 2, (0x8000 | DMACON_DMAEN | 0x0001) as u64));
        assert_eq!(bus.paula.intreq & INT_AUD0, 0);

        bus.advance_devices(1);
        assert_eq!(bus.paula.intreq & INT_AUD0, INT_AUD0);

        assert!(!bus.custom_write(0xDFF09C, 2, INT_AUD0 as u64));
        assert_eq!(bus.paula.intreq & INT_AUD0, 0);
    }

    #[test]
    fn audio_dma_slot_fetches_first_word_and_starts_playback() {
        let mut bus = empty_bus();
        bus.mem.chip_ram[0x04] = 0x12;
        bus.mem.chip_ram[0x05] = 0x34;
        bus.mem.chip_ram[0x06] = 0x56;
        bus.mem.chip_ram[0x07] = 0x78;
        bus.paula.set_audio_min_period_cck(1);
        bus.paula.write_audio_reg(0x00, 0);
        bus.paula.write_audio_reg(0x02, 4);
        bus.paula.write_audio_reg(0x04, 2);
        bus.paula.write_audio_reg(0x06, 8);
        // A stale playback pointer is overwritten by the AUDxLC reload on
        // the DMA-enable edge.
        bus.paula.set_audio_dma_ptr_for_test(0, 0x20);
        bus.agnus.dmacon = DMACON_DMAEN | 0x0001;
        bus.agnus.hpos = 0x00F;

        let _ = bus.paula.advance_audio(0, bus.agnus.dmacon);
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Audio);

        // A single audio slot fetches word 0 from AUDxLC and starts
        // playback -- no separate pointer-reload slot.
        bus.advance_chipset(1);
        assert_eq!(bus.last_chip_bus_owner(), ChipBusOwner::Audio);
        assert_eq!(bus.paula.audio_current_sample_for_test(0), Some(0x12));
        assert_eq!(bus.paula.audio_dma_ptr_for_test(0), Some(6));
    }

    #[test]
    fn paula_interrupt_write_addresses_use_write_only_custom_bus_readback() {
        let mut bus = empty_bus();

        bus.paula.write_intena(0x8000 | INT_MASTER | INT_VERTB);
        bus.paula.write_intreq(0x8000 | INT_AUD0);

        assert_eq!(bus.custom_read(0x01C, 2), (INT_MASTER | INT_VERTB) as u64);
        assert_eq!(bus.custom_read(0x01E, 2), INT_AUD0 as u64);
        assert_eq!(bus.custom_read(0x09A, 2), 0);
        assert_eq!(bus.custom_read(0x09C, 2), 0);
    }

    #[test]
    fn dmacon_masks_stored_bits_and_dmaconr_derives_status() {
        let mut bus = empty_bus();

        bus.agnus.write_dmacon(0x8000 | 0x7FFF);

        assert_eq!(bus.agnus.dmacon, 0x07FF);
        bus.blitter.busy = true;
        bus.blitter.bzero = true;
        assert_eq!(bus.custom_read(0x002, 2), 0x67FF);
    }

    #[test]
    fn dma_modulo_registers_are_word_aligned() {
        let mut bus = empty_bus();

        let _ = bus.custom_write(0x060, 2, 0x0001);
        let _ = bus.custom_write(0x062, 2, 0xFFFF);
        let _ = bus.custom_write(0x064, 2, 0x8001);
        let _ = bus.custom_write(0x066, 2, 0x7FFF);
        let _ = bus.custom_write(0x108, 2, 0xFFFF);
        let _ = bus.custom_write(0x10A, 2, 0x0001);

        assert_eq!(bus.blitter.bltcmod, 0);
        assert_eq!(bus.blitter.bltbmod, -2);
        assert_eq!(bus.blitter.bltamod, i16::MIN);
        assert_eq!(bus.blitter.bltdmod, 0x7FFE);
        assert_eq!(bus.denise.bpl1mod, -2);
        assert_eq!(bus.denise.bpl2mod, 0);
    }

    #[test]
    fn custom_byte_write_updates_only_addressed_lane_for_stateful_registers() {
        let mut bus = empty_bus();

        let _ = bus.custom_write(0xDFF074, 1, 0x80);
        assert_eq!(bus.blitter.bltadat, 0x8000);

        let _ = bus.custom_write(0xDFF075, 1, 0x00);
        assert_eq!(bus.blitter.bltadat, 0x8000);

        let _ = bus.custom_write(0xDFF075, 1, 0x01);
        assert_eq!(bus.blitter.bltadat, 0x8001);

        // Audio registers do NOT use the addressed-lane latch: a real
        // 68000 drives a byte write onto both data-bus halves and Paula
        // latches the full word, so `move.b #v,AUDxPER/VOL` mirrors the
        // byte into both lanes. (Magic Pockets sets its echo voice volume
        // with a byte write to AUDxVOL and relies on this.)
        let _ = bus.custom_write(0xDFF0A6, 1, 0x12);
        assert_eq!(bus.paula.peek_audio_reg_latch(0x06), Some(0x1212));

        let _ = bus.custom_write(0xDFF0A7, 1, 0x34);
        assert_eq!(bus.paula.peek_audio_reg_latch(0x06), Some(0x3434));
    }

    #[test]
    fn sync_strobes_are_documented_noops() {
        let mut bus = empty_bus();

        for off in [0x038, 0x03A, 0x03C, 0x03E] {
            assert!(!bus.custom_write(off, 2, 0xFFFF));
        }
    }

    #[test]
    fn bplcon3_writes_require_ecs_denise_and_enbplcn3() {
        use crate::chipset::denise::BPLCON3_PF2OF_DEFAULT;

        // OCS Denise has no BPLCON3: writes never latch.
        let mut ocs = empty_bus();
        assert!(!ocs.custom_write(0x100, 2, 0x0001));
        assert!(!ocs.custom_write(0x106, 2, 0x1234));
        assert_eq!(ocs.denise.bplcon3, BPLCON3_PF2OF_DEFAULT);

        // ECS Denise drops BPLCON3 writes while ENBPLCN3 (BPLCON0 bit 0)
        // is clear and latches them while it is set.
        let mut ecs = empty_bus();
        ecs.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        assert!(!ecs.custom_write(0x106, 2, 0x1234));
        assert_eq!(ecs.denise.bplcon3, BPLCON3_PF2OF_DEFAULT);
        assert!(!ecs.custom_write(0x100, 2, 0x0001));
        assert!(!ecs.custom_write(0x106, 2, 0x5678));
        assert_eq!(ecs.denise.bplcon3, 0x5678);

        // Clearing ENBPLCN3 again keeps the last latched value but blocks
        // further writes.
        assert!(!ecs.custom_write(0x100, 2, 0x0000));
        assert!(!ecs.custom_write(0x106, 2, 0x9ABC));
        assert_eq!(ecs.denise.bplcon3, 0x5678);
    }

    #[test]
    fn ddf_low_bits_are_masked_by_fetch_mode() {
        assert_eq!(
            bitplane_words_per_row(AgnusRevision::Ocs, 0x0000, 0, 0x0038, 0x0043, false),
            bitplane_words_per_row(AgnusRevision::Ocs, 0x0000, 0, 0x0038, 0x0040, false)
        );
        assert_eq!(
            bitplane_words_per_row(AgnusRevision::Ocs, 0x0000, 0, 0x003C, 0x0040, false),
            bitplane_words_per_row(AgnusRevision::Ocs, 0x0000, 0, 0x0038, 0x0040, false)
        );
        // Low-res DDFSTOP bit 2 is still inside the fetch-unit span: $B6
        // extends through the $B4 unit, yielding the 30-byte row expected by
        // five interleaved bitplanes.
        assert_eq!(
            bitplane_words_per_row(AgnusRevision::Ocs, 0x0000, 0, 0x004A, 0x00B6, false),
            15
        );
        // Hires DDF has 4-cck granularity: the start's low bits shift the
        // window by half a fetch unit. The sequencer still runs whole 8-cck
        // units (the unit starting at-or-after DDFSTOP completes), so the
        // half-unit shift is visible in the word count when the stop lands
        // mid-unit.
        assert_eq!(
            bitplane_words_per_row(AgnusRevision::Ocs, 0x8000, 0, 0x003C, 0x0044, false),
            4
        );
        assert_eq!(
            bitplane_words_per_row(AgnusRevision::Ocs, 0x8000, 0, 0x0038, 0x0044, false),
            6
        );
        // CDTV extended-ROM trademark screen: hires $64/$A8 with $28 modulos
        // requires 20 words/row (10 whole units); truncating the partial
        // tail fetched 18 and sheared every row.
        assert_eq!(
            bitplane_words_per_row(AgnusRevision::Ocs, 0x8000, 0, 0x0064, 0x00A8, false),
            20
        );
    }

    #[test]
    fn dma_pointer_registers_follow_configured_chip_ram_mask() {
        let mut ocs = empty_bus();

        assert!(!ocs.custom_write(0x080, 2, 0x001F));
        assert!(!ocs.custom_write(0x082, 2, 0xFFFE));
        assert_eq!(ocs.agnus.cop1lc, 0x07FFFE);

        assert!(!ocs.custom_write(0x020, 2, 0x001F));
        assert!(!ocs.custom_write(0x022, 2, 0xFFFE));
        assert_eq!(ocs.floppy.dskpt(), 0x07FFFE);

        assert!(!ocs.custom_write(0x050, 2, 0x001F));
        assert!(!ocs.custom_write(0x052, 2, 0xFFFE));
        assert_eq!(ocs.blitter.bltapt, 0x07FFFE);

        assert!(!ocs.custom_write(0x0E0, 2, 0x001F));
        assert!(!ocs.custom_write(0x0E2, 2, 0xFFFE));
        assert_eq!(ocs.denise.bplpt[0], 0x07FFFE);

        assert!(!ocs.custom_write(0x120, 2, 0x001F));
        assert!(!ocs.custom_write(0x122, 2, 0xFFFE));
        assert_eq!(ocs.denise.sprpt[0], 0x07FFFE);

        assert!(!ocs.custom_write(0x0A0, 2, 0x001F));
        assert!(!ocs.custom_write(0x0A2, 2, 0xFFFE));
        assert_eq!(ocs.paula.peek_audio_reg_latch(0x00), Some(0x0007));
        assert_eq!(ocs.paula.peek_audio_reg_latch(0x02), Some(0xFFFE));

        let mut ecs = empty_bus_with_chip_ram(2 * 1024 * 1024);
        ecs.set_agnus_revision(AgnusRevision::Ecs8375);
        assert!(!ecs.custom_write(0x080, 2, 0x001F));
        assert!(!ecs.custom_write(0x082, 2, 0xFFFE));
        assert_eq!(ecs.agnus.cop1lc, 0x1FFFFE);

        // The 8372A's address bus stops at 1 MB even with 2 MB installed,
        // and an OCS Agnus at 512 KiB.
        let mut ecs_1m = empty_bus_with_chip_ram(2 * 1024 * 1024);
        ecs_1m.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        assert!(!ecs_1m.custom_write(0x080, 2, 0x001F));
        assert!(!ecs_1m.custom_write(0x082, 2, 0xFFFE));
        assert_eq!(ecs_1m.agnus.cop1lc, 0x0FFFFE);

        let mut ocs_2m = empty_bus_with_chip_ram(2 * 1024 * 1024);
        assert!(!ocs_2m.custom_write(0x080, 2, 0x001F));
        assert!(!ocs_2m.custom_write(0x082, 2, 0xFFFE));
        assert_eq!(ocs_2m.agnus.cop1lc, 0x07FFFE);
    }

    /// Plan 1.1: walk every DMA pointer register and check the writable
    /// high bits follow the Agnus revision's address-bus reach.
    #[test]
    fn dma_pointer_high_bits_follow_agnus_revision() {
        for (revision, mask) in [
            (AgnusRevision::Ocs, 0x0007_FFFEu32),
            (AgnusRevision::Ecs8372Rev4, 0x000F_FFFE),
            (AgnusRevision::Ecs8375, 0x001F_FFFE),
        ] {
            let mut bus = empty_bus_with_chip_ram(2 * 1024 * 1024);
            bus.set_agnus_revision(revision);
            let write_ptr = |bus: &mut Bus, high: u16| {
                assert!(!bus.custom_write(u64::from(high), 2, 0x001F));
                assert!(!bus.custom_write(u64::from(high) + 2, 2, 0xFFFE));
            };

            write_ptr(&mut bus, 0x020);
            assert_eq!(bus.floppy.dskpt(), mask, "{revision:?} DSKPT");

            for (idx, high) in [0x048u16, 0x04C, 0x050, 0x054].iter().enumerate() {
                write_ptr(&mut bus, *high);
                let got = match idx {
                    0 => bus.blitter.bltcpt,
                    1 => bus.blitter.bltbpt,
                    2 => bus.blitter.bltapt,
                    _ => bus.blitter.bltdpt,
                };
                assert_eq!(got, mask, "{revision:?} BLTxPT {high:#X}");
            }

            write_ptr(&mut bus, 0x080);
            assert_eq!(bus.agnus.cop1lc, mask, "{revision:?} COP1LC");
            write_ptr(&mut bus, 0x084);
            assert_eq!(bus.agnus.cop2lc, mask, "{revision:?} COP2LC");

            for ch in 0..4u16 {
                write_ptr(&mut bus, 0x0A0 + ch * 0x10);
                assert_eq!(
                    bus.paula.peek_audio_reg_latch(ch * 0x10),
                    Some(((mask >> 16) & 0x001F) as u16),
                    "{revision:?} AUD{ch}LCH"
                );
                assert_eq!(
                    bus.paula.peek_audio_reg_latch(ch * 0x10 + 2),
                    Some((mask & 0xFFFE) as u16),
                    "{revision:?} AUD{ch}LCL"
                );
            }

            for plane in 0..6u16 {
                write_ptr(&mut bus, 0x0E0 + plane * 4);
                assert_eq!(
                    bus.denise.bplpt[plane as usize], mask,
                    "{revision:?} BPL{plane}PT"
                );
            }

            for sprite in 0..8u16 {
                write_ptr(&mut bus, 0x120 + sprite * 4);
                assert_eq!(
                    bus.denise.sprpt[sprite as usize], mask,
                    "{revision:?} SPR{sprite}PT"
                );
            }
        }
    }

    #[test]
    fn power_on_reset_keeps_copcon_cdang_clear() {
        let bus = empty_bus();

        assert_eq!(bus.agnus.copcon, 0);
        assert!(!bus.agnus.copper_danger_enabled());
    }

    #[test]
    fn cold_power_on_reset_clears_chip_ram_and_restores_overlay() {
        let mut bus = empty_bus();
        bus.mem.chip_ram[0] = 0xAA;
        bus.mem.chip_ram[1024] = 0x55;
        bus.mem.overlay = false;

        bus.power_on_reset();

        assert!(
            bus.mem.chip_ram.iter().all(|&b| b == 0),
            "chip RAM must be zeroed on cold boot"
        );
        assert!(
            bus.mem.overlay,
            "ROM overlay must be re-enabled on cold boot"
        );
    }

    #[test]
    fn cpu_reset_drives_external_floppy_reset_line() {
        let mut bus = empty_bus();
        bus.floppy.write_prb(0x6F);
        assert!(bus.floppy.activity_led_on());

        bus.reset_custom_chips_from_cpu_reset();

        assert!(!bus.floppy.activity_led_on());
    }

    #[test]
    fn copper_jump_latches_bitplane_state_from_chip_ram() {
        let mut bus = empty_bus();
        let mut pc = 0x0100;
        for (reg, val) in [
            (0x0E0, 0x0000),
            (0x0E2, 0x6090),
            (0x100, 0x4200),
            (0x180, 0x0123),
            (0xFFFF, 0xFFFE),
        ] {
            write_chip_word(&mut bus, pc, reg);
            write_chip_word(&mut bus, pc + 2, val);
            pc += 4;
        }

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN;
        bus.agnus.hpos = 0x20;
        assert!(!bus.custom_write(0x080, 2, 0x0000));
        assert!(!bus.custom_write(0x082, 2, 0x0100));
        assert!(!bus.custom_write(0x088, 2, 0xFFFF));
        // Four MOVEs at the 4-color-clock Copper cadence (last write at +14).
        bus.advance_chipset(18);

        assert_eq!(bus.denise.bplpt[0], 0x6090);
        assert_eq!(bus.denise.bplcon0, 0x4200);
        assert_eq!(bus.denise.palette[0], 0x0123);
    }

    #[test]
    fn copper_interrupt_wait_fires_coper_at_programmed_line() {
        let mut bus = empty_bus();
        let cop1 = 0x0100usize;
        write_chip_word(&mut bus, cop1, 0xD421);
        write_chip_word(&mut bus, cop1 + 2, 0xFFFE);
        write_chip_word(&mut bus, cop1 + 4, 0x009C);
        write_chip_word(&mut bus, cop1 + 6, 0x8010);
        write_chip_word(&mut bus, cop1 + 8, 0xFFFF);
        write_chip_word(&mut bus, cop1 + 10, 0xFFFE);
        bus.agnus.cop1lc = cop1 as u32;
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN;
        assert!(!bus.custom_write(0x088, 2, 0xFFFF));

        let target_cck = 0xD4 * COLORCLOCKS_PER_LINE + 0x20;
        bus.advance_chipset(target_cck);
        assert_eq!(bus.paula.intreq & INT_COPER, 0);

        // The Copper wakes on its tail check at the target position (hpos 0x20),
        // spends a dummy wake-up cycle, then fetches its INTREQ MOVE on the
        // next two even (access-parity) color clocks (0x24, 0x26) before the
        // write lands.
        bus.advance_chipset(6);
        assert_eq!(bus.paula.intreq & INT_COPER, 0);

        bus.advance_chipset(1);
        assert_ne!(bus.paula.intreq & INT_COPER, 0);
    }

    #[test]
    fn copper_coper_intreq_reaches_cpu_after_chipset_irq_delay() {
        let mut bus = empty_bus();
        bus.agnus.vpos = 0x40;
        bus.agnus.hpos = 0x20;

        assert!(bus.write_custom_word_from(0x09C, 0x8000 | INT_COPER, BeamWriteSource::Copper,));

        assert_ne!(bus.paula.intreq & INT_COPER, 0);
        assert_eq!(bus.cpu_visible_intreq() & INT_COPER, 0);
        assert_eq!(bus.pending_copper_irq_beam, Some((0x40, 0x20)));

        bus.advance_chipset(1);
        assert_eq!(bus.cpu_visible_intreq() & INT_COPER, 0);

        bus.advance_chipset(1);
        assert_ne!(bus.cpu_visible_intreq() & INT_COPER, 0);
    }

    #[test]
    fn clearing_coper_intreq_cancels_pending_cpu_irq_delay() {
        let mut bus = empty_bus();
        assert!(bus.write_custom_word_from(0x09C, 0x8000 | INT_COPER, BeamWriteSource::Copper,));

        assert!(!bus.write_custom_word_from(0x09C, INT_COPER, BeamWriteSource::Cpu));

        assert_eq!(bus.paula.intreq & INT_COPER, 0);
        assert_eq!(bus.cpu_visible_intreq() & INT_COPER, 0);
        assert_eq!(bus.pending_copper_irq_beam, None);
    }

    #[test]
    fn intena_unmasks_latched_source_without_new_recognition_delay() {
        let mut bus = empty_bus();
        bus.irq_latency_setting = 65;
        bus.paula.intreq = INT_VERTB;

        assert!(!bus.custom_write(0x09A, 2, u64::from(0x8000 | INT_MASTER | INT_VERTB)));

        assert_ne!(bus.paula.intena & INT_MASTER, 0);
        assert_ne!(bus.cpu_visible_intreq() & INT_VERTB, 0);
        assert_eq!(bus.irq_latency_mask & INT_VERTB, 0);
        assert_eq!(bus.irq_latency_last_pending & INT_VERTB, INT_VERTB);
    }

    #[test]
    fn copper_jump_does_not_raise_delayed_coper_immediately() {
        let mut bus = empty_bus();
        let cop1 = 0x0100usize;
        write_chip_word(&mut bus, cop1, 0xD407);
        write_chip_word(&mut bus, cop1 + 2, 0xFFFE);
        write_chip_word(&mut bus, cop1 + 4, 0x009C);
        write_chip_word(&mut bus, cop1 + 6, 0x8010);
        write_chip_word(&mut bus, cop1 + 8, 0xFFFF);
        write_chip_word(&mut bus, cop1 + 10, 0xFFFE);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN;
        assert!(!bus.custom_write(0x080, 2, 0x0000));
        assert!(!bus.custom_write(0x082, 2, cop1 as u64));
        assert!(!bus.custom_write(0x088, 2, 0xFFFF));

        assert_eq!(bus.paula.intreq & INT_COPER, 0);
    }

    #[test]
    fn copper_dma_enable_gates_instruction_execution() {
        let mut bus = empty_bus();
        let cop1 = 0x0100usize;
        write_chip_word(&mut bus, cop1, 0x0100);
        write_chip_word(&mut bus, cop1 + 2, 0x4200);
        write_chip_word(&mut bus, cop1 + 4, 0xFFFF);
        write_chip_word(&mut bus, cop1 + 6, 0xFFFE);

        assert!(!bus.custom_write(0x080, 2, 0x0000));
        assert!(!bus.custom_write(0x082, 2, cop1 as u64));
        assert!(!bus.custom_write(0x088, 2, 0xFFFF));
        assert_eq!(bus.denise.bplcon0, 0);

        bus.agnus.hpos = 0x20;
        assert!(!bus.custom_write(0x096, 2, (0x8000 | DMACON_DMAEN | DMACON_COPEN) as u64));
        bus.advance_chipset(4);
        assert_eq!(bus.denise.bplcon0, 0x4200);
    }

    #[test]
    fn copper_dma_enable_gates_current_pc_until_copjmp_strobe() {
        let mut bus = empty_bus();
        let cop1 = 0x0100usize;
        let stale = 0x0200usize;
        write_chip_word(&mut bus, cop1, 0x0100);
        write_chip_word(&mut bus, cop1 + 2, 0x4200);
        write_chip_word(&mut bus, cop1 + 4, 0xFFFF);
        write_chip_word(&mut bus, cop1 + 6, 0xFFFE);
        write_chip_word(&mut bus, stale, 0x0180);
        write_chip_word(&mut bus, stale + 2, 0x0999);
        write_chip_word(&mut bus, stale + 4, 0xFFFF);
        write_chip_word(&mut bus, stale + 6, 0xFFFE);

        bus.agnus.dmacon = DMACON_DMAEN;
        bus.copper.jump(stale as u32);
        bus.agnus.hpos = 0x20;
        assert!(!bus.custom_write(0x080, 2, 0x0000));
        assert!(!bus.custom_write(0x082, 2, cop1 as u64));
        assert!(!bus.custom_write(0x096, 2, (0x8000 | DMACON_COPEN) as u64));
        bus.advance_chipset(4);

        assert_eq!(bus.denise.palette[0], 0x0999);
        assert_eq!(bus.denise.bplcon0, 0);

        assert!(!bus.custom_write(0x088, 2, 0xFFFF));
        bus.advance_chipset(4);
        assert_eq!(bus.denise.bplcon0, 0x4200);
    }

    #[test]
    fn automatic_copper_restart_uses_live_cop1lc_at_frame_boundary() {
        let mut bus = empty_bus();
        let cop1 = 0x0100usize;
        let cop2 = 0x0200usize;
        write_chip_word(&mut bus, cop1, 0x0180);
        write_chip_word(&mut bus, cop1 + 2, 0x0555);
        write_chip_word(&mut bus, cop1 + 4, 0xFFFF);
        write_chip_word(&mut bus, cop1 + 6, 0xFFFE);
        write_chip_word(&mut bus, cop2, 0x0180);
        write_chip_word(&mut bus, cop2 + 2, 0x0666);
        write_chip_word(&mut bus, cop2 + 4, 0xFFFF);
        write_chip_word(&mut bus, cop2 + 6, 0xFFFE);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN;
        assert!(!bus.custom_write(0x080, 2, 0x0000));
        assert!(!bus.custom_write(0x082, 2, cop1 as u64));
        bus.agnus.cop1lc = cop2 as u32;
        bus.agnus.vpos = crate::chipset::agnus::PAL_LINES - 1;
        bus.agnus.hpos = COLORCLOCKS_PER_LINE - 1;

        // The Copper restarts at the top of the frame (vpos 0), not at the end
        // of vblank, so the live COP1LC (cop2) is picked up immediately as the
        // beam wraps -- no delay until the end of vblank.
        bus.advance_chipset(1);
        assert_eq!(bus.pending_copper_frame_start, None);
        assert_eq!(bus.denise.palette[0], 0);

        // From hpos 0 the Copper waits out the refresh band (0x00-0x08) and its
        // idle-half color clock at hpos 0x09, then its single MOVE fetches on
        // the next two even (access-parity) color clocks: write at hpos 0x0C.
        bus.advance_chipset(13);
        assert_eq!(bus.denise.palette[0], 0x0666);
    }

    #[test]
    fn next_copper_wakeup_cck_tracks_wait_beam_position() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN;
        bus.agnus.vpos = 0x50;
        bus.agnus.hpos = 0x10;
        bus.copper.wait(CopperWait::new(0x5021, 0xFFFE));

        assert_eq!(bus.next_copper_wakeup_cck(), Some(0x10));

        bus.agnus.hpos = 0x20;
        assert_eq!(bus.next_copper_wakeup_cck(), Some(0));

        bus.agnus.dmacon = DMACON_COPEN;
        assert_eq!(bus.next_copper_wakeup_cck(), None);
    }

    #[test]
    fn next_copper_wakeup_cck_waits_for_vertical_low_byte_rollover_after_line_255() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN;
        bus.agnus.vpos = 0xFF;
        bus.agnus.hpos = 0xDF;
        bus.copper.wait(CopperWait::new(0x1B01, 0xFFFE));

        let expected = bus.agnus.cck_until_line_start(0x11B).unwrap();
        assert_eq!(
            expected,
            (COLORCLOCKS_PER_LINE - 0xDF) + 0x1B * COLORCLOCKS_PER_LINE
        );
        assert_eq!(bus.next_copper_wakeup_cck(), Some(expected));

        bus.agnus.vpos = 0x100;
        bus.agnus.hpos = 0;
        assert_eq!(
            bus.next_copper_wakeup_cck(),
            Some(0x1B * COLORCLOCKS_PER_LINE)
        );

        bus.agnus.vpos = 0x11B;
        assert_eq!(bus.next_copper_wakeup_cck(), Some(0));
    }

    #[test]
    fn next_copper_wakeup_cck_keeps_high_half_full_mask_wait_satisfied_on_line_255() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN;
        bus.agnus.vpos = 0xFF;
        bus.agnus.hpos = COLORCLOCKS_PER_LINE - 1;
        bus.copper.wait(CopperWait::new(0xFC01, 0xFFFE));

        assert_eq!(bus.next_copper_wakeup_cck(), Some(0));
    }

    /// Run a Copper list that waits out the 8-bit vertical rollover (WAIT
    /// vp=$FF hp=$DE, then WAIT vp=$36 for line $136/310) and then MOVEs
    /// $0123 into COLOR00. Returns the beam line the write landed on.
    fn copper_line_310_move_lands_at(bitplane_dma: bool) -> Option<u32> {
        let mut bus = empty_bus();
        let cop1 = 0x0100usize;
        write_chip_word(&mut bus, cop1, 0xFFDF);
        write_chip_word(&mut bus, cop1 + 2, 0xFFFE);
        write_chip_word(&mut bus, cop1 + 4, 0x3601);
        write_chip_word(&mut bus, cop1 + 6, 0xFFFE);
        write_chip_word(&mut bus, cop1 + 8, 0x0180);
        write_chip_word(&mut bus, cop1 + 10, 0x0123);
        write_chip_word(&mut bus, cop1 + 12, 0xFFFF);
        write_chip_word(&mut bus, cop1 + 14, 0xFFFE);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN;
        if bitplane_dma {
            // Deep-overscan display: the last DDFSTOP=$D8 lores fetch unit
            // occupies hpos $D8..$DF, covering the wait's only releasable
            // color clock at $DE (later ccks fall in the line-end blackout).
            assert!(!bus.custom_write(0x08E, 2, 0x0571)); // DIWSTRT
            assert!(!bus.custom_write(0x090, 2, 0x40D1)); // DIWSTOP
            assert!(!bus.custom_write(0x092, 2, 0x0030)); // DDFSTRT
            assert!(!bus.custom_write(0x094, 2, 0x00D8)); // DDFSTOP
            assert!(!bus.custom_write(0x100, 2, 0x6200)); // BPLCON0 6 planes
            bus.agnus.dmacon |= DMACON_BPLEN;
        }
        bus.agnus.vpos = 0x18;
        bus.agnus.hpos = 0x30;
        bus.copper.jump(cop1 as u32);

        for _ in 0..(313 * 227) {
            bus.advance_chipset(1);
            if bus.denise.palette[0] == 0x0123 {
                return Some(bus.agnus.vpos);
            }
            if bus.agnus.vpos == 0 && bus.agnus.hpos == 0 {
                break;
            }
        }
        None
    }

    #[test]
    fn copper_wait_past_vertical_rollover_releases_on_quiet_bus() {
        // CDTV extended-ROM boot list idiom: WAIT vp=$FF hp=$DE releases in
        // the last color clocks of line 255, then WAIT vp=$36 targets line
        // $136 (310) after the 8-bit rollover, then a MOVE turns the display
        // off. The MOVE must land at line 310.
        assert_eq!(copper_line_310_move_lands_at(false), Some(310));
    }

    #[test]
    fn copper_wait_comparator_runs_under_fixed_bitplane_dma() {
        // Same list with overscan bitplane DMA fetching through hpos $DE:
        // the comparator is combinational on real Agnus and keeps running
        // while fixed DMA owns the bus, so the wait still releases on line
        // 255 and the display-off MOVE still lands at line 310. Regression:
        // the CDTV boot screen left bitplane DMA running through the bottom
        // vblank, fetching garbage rows past the image (noise band at the
        // bottom of the screen).
        assert_eq!(copper_line_310_move_lands_at(true), Some(310));
    }

    #[test]
    fn next_copper_wakeup_cck_accounts_for_ntsc_long_lines() {
        let mut bus = empty_bus();
        bus.set_video_standard(VideoStandard::Ntsc);
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN;
        bus.copper.wait(CopperWait::new(0x0201, 0xFFFE));

        assert_eq!(
            bus.next_copper_wakeup_cck(),
            Some(COLORCLOCKS_PER_LINE + NTSC_LONG_COLORCLOCKS_PER_LINE)
        );
    }

    #[test]
    fn copper_wait_with_bfd_caps_to_blitter_completion_after_position_match() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN | DMACON_BLTEN;
        bus.agnus.vpos = 0x50;
        bus.agnus.hpos = 0x20;
        bus.copper.wait(CopperWait::new(0x5021, 0x7FFE));
        bus.blitter.bltcon0 = 0x0100;
        bus.blitter.start_scheduled((1 << 6) | 1, &bus.mem.chip_ram);

        assert_eq!(bus.next_blitter_completion_cck(), Some(6));
        assert_eq!(bus.next_copper_wakeup_cck(), Some(6));
    }

    #[test]
    fn copper_wait_with_bfd_clear_resumes_after_busy_blitter_finishes() {
        let mut bus = empty_bus();
        let cop1 = 0x0100usize;
        write_copper_wait_then_move(&mut bus, cop1, 0x5021, 0x7FFE, 0x0180, 0x0555);
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN | DMACON_BLTEN;
        bus.agnus.vpos = 0x50;
        bus.agnus.hpos = 0x1E;
        bus.copper.jump(cop1 as u32);
        // The WAIT instruction's two-word fetch now spans 3 color clocks
        // (fetch, idle, fetch) before the Copper parks.
        bus.advance_chipset(3);
        assert!(bus.copper.waiting().is_some());

        bus.blitter.bltcon0 = 0x0100;
        bus.blitter.start_scheduled((1 << 6) | 1, &bus.mem.chip_ram);
        assert_eq!(bus.next_copper_wakeup_cck(), Some(6));

        bus.advance_chipset(5);
        assert!(bus.copper.waiting().is_some());
        assert_eq!(bus.denise.palette[0], 0);

        bus.advance_chipset(1);
        assert!(!bus.blitter.busy);
        assert!(bus.copper.waiting().is_some());

        bus.advance_chipset(2);
        assert_eq!(bus.denise.palette[0], 0);

        // Once the blitter frees the bus, the Copper spends a dummy wake-up
        // cycle, then its MOVE fetch writes at hpos 0x2C.
        bus.advance_chipset(4);
        assert_eq!(bus.denise.palette[0], 0x0555);
        assert_eq!(bus.current_render_events()[0].hpos, 0x2C);
    }

    #[test]
    fn copper_wait_with_bfd_set_ignores_busy_blitter_after_position_match() {
        let mut bus = empty_bus();
        let cop1 = 0x0100usize;
        write_copper_wait_then_move(&mut bus, cop1, 0x5021, 0xFFFE, 0x0180, 0x0666);
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN | DMACON_BLTEN;
        bus.agnus.vpos = 0x50;
        bus.agnus.hpos = 0x1E;
        bus.copper.jump(cop1 as u32);
        // The WAIT instruction's two-word fetch now spans 3 color clocks.
        bus.advance_chipset(3);
        assert!(bus.copper.waiting().is_some());

        bus.blitter.bltcon0 = 0x0100;
        bus.blitter.start_scheduled((1 << 6) | 1, &bus.mem.chip_ram);
        assert_eq!(bus.next_copper_wakeup_cck(), Some(0));

        bus.advance_chipset(2);
        assert_eq!(bus.denise.palette[0], 0);

        // BFD set: the Copper ignores the busy blitter and resumes; it wakes on
        // its tail check at hpos 0x21, spends a dummy wake-up cycle, and the
        // MOVE write lands at hpos 0x26.
        bus.advance_chipset(4);
        assert_eq!(bus.denise.palette[0], 0x0666);
        assert_eq!(bus.current_render_events()[0].hpos, 0x26);
        assert!(bus.blitter.busy);
    }

    #[test]
    fn copper_wait_immediate_match_uses_free_cycle_before_next_fetch() {
        let mut bus = empty_bus();
        let cop1 = 0x0100usize;
        write_copper_wait_then_move(&mut bus, cop1, 0x0021, 0xFFFE, 0x0180, 0x0777);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN;
        bus.agnus.hpos = 0x20;
        bus.copper.jump(cop1 as u32);

        // WAIT fetch parks the Copper already past the target; it wakes on its
        // tail check at hpos 0x23, spends a dummy wake-up cycle, then the MOVE
        // write lands at hpos 0x28.
        bus.advance_chipset(6);
        assert_eq!(bus.denise.palette[0], 0);

        bus.advance_chipset(3);
        assert_eq!(bus.denise.palette[0], 0x0777);
        assert_eq!(bus.current_render_events()[0].hpos, 0x28);
    }

    #[test]
    fn copper_wait_wakeup_yields_free_cycle_after_late_match() {
        let mut bus = empty_bus();
        let cop1 = 0x0100usize;
        write_copper_wait_then_move(&mut bus, cop1, 0x0025, 0xFFFE, 0x0180, 0x0888);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN;
        bus.agnus.hpos = 0x20;
        bus.copper.jump(cop1 as u32);

        // The wait matches at hpos 0x24; the Copper yields that free cycle,
        // spends a dummy wake-up cycle, then writes at hpos 0x2A.
        bus.advance_chipset(8);
        assert_eq!(bus.denise.palette[0], 0);

        bus.advance_chipset(3);
        assert_eq!(bus.denise.palette[0], 0x0888);
        assert_eq!(bus.current_render_events()[0].hpos, 0x2A);
    }

    #[test]
    fn copper_e1_bus_lockout_defers_transfer_until_e2() {
        let mut bus = empty_bus();
        let cop1 = 0x0100usize;
        let start_vpos = 0x40;
        write_chip_word(&mut bus, cop1, 0x0180);
        write_chip_word(&mut bus, cop1 + 2, 0x0999);
        write_chip_word(&mut bus, cop1 + 4, 0xFFFF);
        write_chip_word(&mut bus, cop1 + 6, 0xFFFE);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN;
        bus.agnus.vpos = start_vpos;
        bus.agnus.hpos = COPPER_BUS_LOCKOUT_HPOS;
        bus.copper.jump(cop1 as u32);

        // E1 is odd, i.e. the Copper's idle half under beam-parity locking, and
        // it is also the end-of-line bus lockout, so the first-word fetch cannot
        // happen here; the slot is free.
        bus.advance_chipset(1);
        assert_eq!(bus.last_chip_bus_owner(), ChipBusOwner::Idle);
        assert_eq!(bus.copper.pc(), cop1 as u32);
        assert_eq!(bus.denise.palette[0], 0);

        // The first-word fetch lands on the next access-parity color clock, E2,
        // and the beam wraps to the next line.
        bus.advance_chipset(1);
        assert_eq!(bus.copper.pc(), cop1 as u32 + 2);
        assert_eq!(bus.agnus.vpos, start_vpos + 1);
        assert_eq!(bus.agnus.hpos, 0);
        assert_eq!(bus.denise.palette[0], 0);

        // With the hardware refresh model (4 slots at 0x004/6/8/A), hpos 0x00
        // is a free access-parity color clock, so the Copper fetches the second
        // word there immediately after the line wrap and its write lands at 0x00.
        bus.advance_chipset(1);
        assert_eq!(bus.denise.palette[0], 0x0999);
        assert_eq!(bus.current_render_events()[0].vpos, start_vpos + 1);
        assert_eq!(bus.current_render_events()[0].hpos, 0x00);
    }

    #[test]
    fn copper_skip_does_not_skip_wait_instruction() {
        let mut bus = empty_bus();
        let cop1 = 0x0100usize;
        write_chip_word(&mut bus, cop1, 0x0001);
        write_chip_word(&mut bus, cop1 + 2, 0xFFFF);
        write_chip_word(&mut bus, cop1 + 4, 0x5021);
        write_chip_word(&mut bus, cop1 + 6, 0xFFFE);
        write_chip_word(&mut bus, cop1 + 8, 0x0180);
        write_chip_word(&mut bus, cop1 + 10, 0x0555);
        write_chip_word(&mut bus, cop1 + 12, 0xFFFF);
        write_chip_word(&mut bus, cop1 + 14, 0xFFFE);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN;
        bus.agnus.vpos = 0x00;
        bus.agnus.hpos = 0x20;
        bus.copper.jump(cop1 as u32);
        // The SKIP runs the full hardware sequence: a 4-color-clock fetch plus
        // its two bus-free tail cycles (WAITSKIP1/WAITSKIP2), then the WAIT
        // spends its own 4-color-clock fetch, so the Copper reaches and parks on
        // the WAIT after 9 color clocks.
        bus.advance_chipset(9);

        assert!(bus.copper.waiting().is_some());
        assert_eq!(bus.denise.palette[0], 0);
    }

    #[test]
    fn copper_skip_over_move_consumes_move_fetch_slots_before_next_instruction() {
        let mut bus = empty_bus();
        let cop1 = 0x0100usize;
        write_chip_word(&mut bus, cop1, 0x0001);
        write_chip_word(&mut bus, cop1 + 2, 0xFFFF);
        write_chip_word(&mut bus, cop1 + 4, 0x0180);
        write_chip_word(&mut bus, cop1 + 6, 0x0111);
        write_chip_word(&mut bus, cop1 + 8, 0x0182);
        write_chip_word(&mut bus, cop1 + 10, 0x0222);
        write_chip_word(&mut bus, cop1 + 12, 0xFFFF);
        write_chip_word(&mut bus, cop1 + 14, 0xFFFE);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN;
        bus.agnus.hpos = 0x20;
        bus.copper.jump(cop1 as u32);
        // SKIP fetch is 4 color clocks and is still in its tail cycles here, so
        // neither MOVE has run yet.
        bus.advance_chipset(4);

        assert_eq!(bus.denise.palette[0], 0);
        assert_eq!(bus.denise.palette[1], 0);

        // The SKIP's two tail cycles elapse, the skipped MOVE still spends its
        // 4-color-clock fetch, then the second MOVE spends its own before
        // writing palette[1] at hpos 0x2C.
        bus.advance_chipset(9);

        assert_eq!(bus.denise.palette[0], 0);
        assert_eq!(bus.denise.palette[1], 0x0222);
    }

    #[test]
    fn copper_wait_wakeup_keeps_vp7_loop_switch_on_scanline_boundary() {
        let mut bus = empty_bus();
        let cop1 = 0x0100usize;
        let comb2 = cop1 + 4 + 42 * 4 + 4 + 4 + 8;
        let mut pc = cop1;

        write_chip_word(&mut bus, pc, 0x0033); // horizontal wait, VP7 low half
        write_chip_word(&mut bus, pc + 2, 0x80FE);
        pc += 4;
        for i in 0..42u16 {
            write_chip_word(&mut bus, pc, 0x0102); // BPLCON1
            write_chip_word(&mut bus, pc + 2, 0x0100 | i);
            pc += 4;
        }
        write_chip_word(&mut bus, pc, 0x7FE1); // skip COPJMP1 at vpos 127, hpos 224
        write_chip_word(&mut bus, pc + 2, 0xFFFF);
        pc += 4;
        write_chip_word(&mut bus, pc, 0x0088); // COPJMP1
        write_chip_word(&mut bus, pc + 2, 0x0000);
        pc += 4;
        write_chip_word(&mut bus, pc, 0x0080); // COP1LCH = comb2
        write_chip_word(&mut bus, pc + 2, ((comb2 >> 16) & 0x001F) as u16);
        pc += 4;
        write_chip_word(&mut bus, pc, 0x0082); // COP1LCL = comb2
        write_chip_word(&mut bus, pc + 2, (comb2 & 0xFFFE) as u16);

        pc = comb2;
        write_chip_word(&mut bus, pc, 0x8033); // horizontal wait, VP7 high half
        write_chip_word(&mut bus, pc + 2, 0x80FE);
        pc += 4;
        for i in 0..42u16 {
            write_chip_word(&mut bus, pc, 0x0102); // BPLCON1
            write_chip_word(&mut bus, pc + 2, 0x0200 | i);
            pc += 4;
        }
        write_chip_word(&mut bus, pc, 0xFFFF);
        write_chip_word(&mut bus, pc + 2, 0xFFFE);

        bus.agnus.cop1lc = cop1 as u32;
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN;
        bus.agnus.vpos = 126;
        bus.agnus.hpos = 0;
        bus.copper.jump(cop1 as u32);
        bus.advance_chipset(COLORCLOCKS_PER_LINE * 4);

        let bplcon1_writes_on = |vpos| {
            bus.current_render_events()
                .iter()
                .filter(|event| {
                    event.source == BeamWriteSource::Copper
                        && event.offset == 0x0102
                        && event.vpos == vpos
                })
                .count()
        };
        assert_eq!(bplcon1_writes_on(126), 42);
        assert_eq!(bplcon1_writes_on(127), 42);
        assert_eq!(bplcon1_writes_on(128), 42);

        let line_128_hpos: Vec<_> = bus
            .current_render_events()
            .iter()
            .filter(|event| {
                event.source == BeamWriteSource::Copper
                    && event.offset == 0x0102
                    && event.vpos == 128
            })
            .map(|event| event.hpos)
            .collect();
        assert_eq!(line_128_hpos.first(), Some(&0x38));
        assert_eq!(line_128_hpos.last(), Some(&0xDC));
    }

    #[test]
    fn copper_masked_end_of_list_stops_instead_of_waiting() {
        let mut bus = empty_bus();
        let cop1 = 0x0100usize;
        write_chip_word(&mut bus, cop1, 0xFFFF);
        write_chip_word(&mut bus, cop1 + 2, 0xFFFC);
        write_chip_word(&mut bus, cop1 + 4, 0x0180);
        write_chip_word(&mut bus, cop1 + 6, 0x0555);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN;
        bus.agnus.hpos = 0x20;
        bus.copper.jump(cop1 as u32);
        bus.advance_chipset(4);

        assert!(!bus.copper.is_running());
        assert!(bus.copper.waiting().is_none());
        assert_eq!(bus.denise.palette[0], 0);
    }

    #[test]
    fn blitter_completion_deadline_skips_fixed_dma_slots() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BLTEN | DMACON_BPLEN;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x00D0;
        bus.agnus.vpos = 0x40; // inside the default vertical display window
        bus.agnus.hpos = 0x03D;
        bus.blitter.bltcon0 = 0x09F0; // A -> D copy: every A/D slot is a bus access
        bus.blitter.bltafwm = 0xFFFF;
        bus.blitter.bltalwm = 0xFFFF;
        bus.blitter.bltapt = 0x10;
        bus.blitter.bltdpt = 0x20;
        write_chip_word(&mut bus, 0x10, 0x1234);
        bus.blitter.start_scheduled((1 << 6) | 1, &bus.mem.chip_ram);

        assert_eq!(bus.blitter.scheduled_slots_remaining(), Some(6));
        // Two internal lead-in cycles elapse at 0x3D/0x3E regardless of DMA,
        // then the A fetch must skip the plane-1 bitplane fetch slot at 0x3F
        // (granted at 0x40), D writes at 0x41, the internal E cycle passes at
        // 0x42, and the final F write lands at 0x43: 7 color clocks in all.
        assert_eq!(bus.next_blitter_completion_cck(), Some(7));

        // After the internal lead-in the blit's A access is pending and the
        // bitplane fetch owns its fixed slot.
        bus.advance_chipset(2);
        assert!(bus.blitter.busy);
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Bitplane);

        bus.advance_chipset(4);
        assert!(bus.blitter.busy);
        bus.advance_chipset(1);
        assert!(!bus.blitter.busy);
        assert_ne!(bus.paula.intreq & INT_BLIT, 0);
        assert_eq!(&bus.mem.chip_ram[0x20..0x22], &[0x12, 0x34]);
    }

    #[test]
    fn blitter_completion_deadline_accounts_for_copper_dma_slots() {
        let mut bus = empty_bus();
        let cop1 = 0x0100usize;
        write_chip_word(&mut bus, cop1, 0x0180);
        write_chip_word(&mut bus, cop1 + 2, 0x0123);
        write_chip_word(&mut bus, cop1 + 4, 0x0182);
        write_chip_word(&mut bus, cop1 + 6, 0x0456);
        write_chip_word(&mut bus, cop1 + 8, 0xFFFF);
        write_chip_word(&mut bus, cop1 + 10, 0xFFFE);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN | DMACON_BLTEN;
        bus.agnus.hpos = 0x20;
        bus.copper.jump(cop1 as u32);
        bus.blitter.bltcon0 = 0x09F0; // A -> D copy: every A/D slot is a bus access
        bus.blitter.bltafwm = 0xFFFF;
        bus.blitter.bltalwm = 0xFFFF;
        bus.blitter.bltapt = 0x10;
        bus.blitter.bltdpt = 0x20;
        write_chip_word(&mut bus, 0x10, 0x1234);
        bus.blitter.start_scheduled((1 << 6) | 1, &bus.mem.chip_ram);

        assert_eq!(bus.blitter.scheduled_slots_remaining(), Some(6));
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Copper);
        // Two internal lead-in cycles pass under the Copper's fetches at
        // 0x20/0x21, then the A and D accesses take the Copper's idle halves
        // (0x23, 0x25), the internal E cycle passes at 0x26, and the final F
        // write lands at 0x27: 8 color clocks in all.
        assert_eq!(bus.next_blitter_completion_cck(), Some(8));

        bus.advance_chipset(6);
        assert!(bus.blitter.busy);
        assert_eq!(bus.next_blitter_completion_cck(), Some(2));

        bus.advance_chipset(4);
        assert!(!bus.blitter.busy);
        assert_ne!(bus.paula.intreq & INT_BLIT, 0);
    }

    #[test]
    fn copper_move_writes_visible_registers_on_second_dma_slot() {
        let mut bus = empty_bus();
        run_copper_moves_at(
            &mut bus,
            0x0100,
            RENDER_VISIBLE_START_VPOS,
            RENDER_COPPER_WAIT_HPOS_FB0,
            &[(0x0182, 0x00F0), (0x0092, 0x0040), (0x0100, 0x1200)],
        );

        let events = bus.current_render_events();
        assert_eq!(events.len(), 3);
        assert_eq!(
            events
                .iter()
                .map(|event| (event.offset, event.value, event.hpos, event.source))
                .collect::<Vec<_>>(),
            vec![
                (
                    0x0182,
                    0x00F0,
                    RENDER_COPPER_WAIT_HPOS_FB0 + 2,
                    BeamWriteSource::Copper
                ),
                (
                    0x0092,
                    0x0040,
                    RENDER_COPPER_WAIT_HPOS_FB0 + 6,
                    BeamWriteSource::Copper
                ),
                (
                    0x0100,
                    0x1200,
                    RENDER_COPPER_WAIT_HPOS_FB0 + 10,
                    BeamWriteSource::Copper
                ),
            ]
        );
        assert_eq!(bus.denise.palette[1], 0x00F0);
        assert_eq!(bus.denise.ddfstrt, 0x0040);
        assert_eq!(bus.denise.bplcon0, 0x1200);
    }

    #[test]
    fn copper_move_palette_write_affects_pixels_after_second_dma_slot() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0038;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.palette.write_ocs(1, 0x0F00);
        bus.current_frame_render_base = bus.capture_render_snapshot();
        bus.agnus.dmacon = DMACON_DMAEN;
        run_copper_moves_at(
            &mut bus,
            0x0100,
            RENDER_VISIBLE_START_VPOS,
            RENDER_COPPER_WAIT_HPOS_FB0 + 30,
            &[(0x0182, 0x00F0)],
        );

        let event_hpos = bus.current_render_events()[0].hpos;
        // MOVE write lands on its second-word fetch, two color clocks into the
        // 4-color-clock cadence from the start hpos (+30).
        assert_eq!(event_hpos, RENDER_COPPER_WAIT_HPOS_FB0 + 32);
        let words_per_row = bitplane_words_per_row(
            bus.agnus.revision(),
            bus.denise.bplcon0,
            bus.agnus.fmode(),
            bus.denise.ddfstrt,
            bus.denise.ddfstop,
            bus.harddis_active(),
        );
        bus.current_frame_bitplane_rows[0] = Some(CapturedBitplaneRow {
            nplanes: 1,
            words_per_row,
            planes: [
                vec![0xFFFF; words_per_row],
                vec![0; words_per_row],
                vec![0; words_per_row],
                vec![0; words_per_row],
                vec![0; words_per_row],
                vec![0; words_per_row],
                Vec::new(),
                Vec::new(),
            ],
        });

        let mut fb = vec![0; FB_PIXELS];
        bitplane::render(&mut bus, &mut fb);

        let event_x = render_color_write_x(event_hpos);
        assert!(event_x > STANDARD_VISIBLE_X0);
        assert_eq!(fb[event_x - 1], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[event_x], rgb12_to_rgba8(0x00F0));
    }

    #[test]
    fn copper_write_restrictions_require_copcon_for_blitter_registers() {
        let mut bus = empty_bus();
        let cop1 = 0x0100usize;
        write_chip_word(&mut bus, cop1, 0x0040);
        write_chip_word(&mut bus, cop1 + 2, 0x1234);
        write_chip_word(&mut bus, cop1 + 4, 0xFFFF);
        write_chip_word(&mut bus, cop1 + 6, 0xFFFE);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN;
        bus.agnus.hpos = 0x20;
        assert!(!bus.custom_write(0x080, 2, 0x0000));
        assert!(!bus.custom_write(0x082, 2, cop1 as u64));
        assert!(!bus.custom_write(0x088, 2, 0xFFFF));
        bus.advance_chipset(4);
        assert_eq!(bus.blitter.bltcon0, 0);

        assert!(!bus.custom_write(0x02E, 2, 0x0002));
        assert!(!bus.custom_write(0x088, 2, 0xFFFF));
        bus.advance_chipset(4);
        assert_eq!(bus.blitter.bltcon0, 0x1234);
    }

    #[test]
    fn copper_forbidden_move_ranges_match_copcon_cdang_on_ocs_and_ecs() {
        let cases_by_revision = [
            (
                AgnusRevision::Ocs,
                [
                    (0x000, false, false),
                    (0x000, true, false),
                    (0x03E, true, false),
                    (0x040, false, false),
                    (0x040, true, true),
                    (0x07E, false, false),
                    (0x07E, true, true),
                    (0x080, false, true),
                    (0x180, false, true),
                ],
            ),
            (
                AgnusRevision::Ecs8372Rev4,
                [
                    (0x000, false, false),
                    (0x000, true, true),
                    (0x03E, true, true),
                    (0x040, false, false),
                    (0x040, true, true),
                    (0x07E, false, false),
                    (0x07E, true, true),
                    (0x080, false, true),
                    (0x180, false, true),
                ],
            ),
        ];

        for (revision, cases) in cases_by_revision {
            for (register, cdang, should_continue) in cases {
                let bus = run_copper_guarded_move(revision, register, cdang);
                assert_eq!(
                    bus.denise.palette[1] == 0x0ABC,
                    should_continue,
                    "revision={revision:?} register={register:#05X} cdang={cdang}"
                );
            }
        }
    }

    #[test]
    fn ecs_copper_can_clear_copcon_then_loses_lower_register_access() {
        let mut bus = empty_bus();
        bus.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        let cop1 = 0x0100usize;
        write_chip_word(&mut bus, cop1, 0x002E);
        write_chip_word(&mut bus, cop1 + 2, 0x0000);
        write_chip_word(&mut bus, cop1 + 4, 0x0040);
        write_chip_word(&mut bus, cop1 + 6, 0x1234);
        write_chip_word(&mut bus, cop1 + 8, 0x0182);
        write_chip_word(&mut bus, cop1 + 10, 0x0ABC);
        write_chip_word(&mut bus, cop1 + 12, 0xFFFF);
        write_chip_word(&mut bus, cop1 + 14, 0xFFFE);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN;
        bus.agnus.hpos = 0x20;
        assert!(!bus.custom_write(0x02E, 2, 0x0002));
        assert!(!bus.custom_write(0x080, 2, 0x0000));
        assert!(!bus.custom_write(0x082, 2, cop1 as u64));
        assert!(!bus.custom_write(0x088, 2, 0xFFFF));
        bus.advance_chipset(8);

        assert_eq!(bus.agnus.copcon, 0);
        assert_eq!(bus.blitter.bltcon0, 0);
        assert_eq!(bus.denise.palette[1], 0);
    }

    #[test]
    fn copper_halts_on_forbidden_register_move_until_next_strobe() {
        let mut bus = empty_bus();
        let illegal = 0x0100usize;
        let legal = 0x0200usize;
        write_chip_word(&mut bus, illegal, 0x0000);
        write_chip_word(&mut bus, illegal + 2, 0x0000);
        write_chip_word(&mut bus, illegal + 4, 0x0180);
        write_chip_word(&mut bus, illegal + 6, 0x0555);
        write_chip_word(&mut bus, legal, 0x0180);
        write_chip_word(&mut bus, legal + 2, 0x0666);
        write_chip_word(&mut bus, legal + 4, 0xFFFF);
        write_chip_word(&mut bus, legal + 6, 0xFFFE);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN;
        bus.agnus.hpos = 0x20;
        assert!(!bus.custom_write(0x080, 2, 0x0000));
        assert!(!bus.custom_write(0x082, 2, illegal as u64));
        assert!(!bus.custom_write(0x084, 2, 0x0000));
        assert!(!bus.custom_write(0x086, 2, legal as u64));
        assert!(!bus.custom_write(0x088, 2, 0xFFFF));

        bus.advance_chipset(8);
        assert_eq!(bus.denise.palette[0], 0);
        assert!(!bus.copper.is_running());

        assert!(!bus.custom_write(0x08A, 2, 0xFFFF));
        bus.advance_chipset(4);
        assert_eq!(bus.denise.palette[0], 0x0666);
    }

    #[test]
    fn forbidden_copper_move_recovers_at_start_of_frame_from_cop1lc() {
        let mut bus = empty_bus();
        let illegal = 0x0100usize;
        let legal = 0x0200usize;
        write_chip_word(&mut bus, illegal, 0x0000);
        write_chip_word(&mut bus, illegal + 2, 0x0000);
        write_chip_word(&mut bus, illegal + 4, 0x0180);
        write_chip_word(&mut bus, illegal + 6, 0x0555);
        write_chip_word(&mut bus, legal, 0x0180);
        write_chip_word(&mut bus, legal + 2, 0x0666);
        write_chip_word(&mut bus, legal + 4, 0xFFFF);
        write_chip_word(&mut bus, legal + 6, 0xFFFE);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN;
        bus.agnus.hpos = 0x20;
        assert!(!bus.custom_write(0x080, 2, 0x0000));
        assert!(!bus.custom_write(0x082, 2, legal as u64));
        assert!(!bus.custom_write(0x084, 2, 0x0000));
        assert!(!bus.custom_write(0x086, 2, illegal as u64));
        assert!(!bus.custom_write(0x08A, 2, 0xFFFF));
        bus.advance_chipset(8);

        assert_eq!(bus.denise.palette[0], 0);
        assert!(!bus.copper.is_running());

        bus.agnus.vpos = crate::chipset::agnus::PAL_LINES - 1;
        bus.agnus.hpos = COLORCLOCKS_PER_LINE - 1;
        bus.advance_chipset(1);
        // Restart is immediate at the top of the frame, recovering from the
        // forbidden MOVE via the live COP1LC.
        assert_eq!(bus.pending_copper_frame_start, None);
        // From hpos 0 the Copper waits out the refresh band and its idle-half
        // color clock, then its MOVE fetches on the next two even (access-parity)
        // color clocks: write at hpos 0x0C.
        bus.advance_chipset(13);

        assert_eq!(bus.denise.palette[0], 0x0666);
    }

    #[test]
    fn copper_move_updates_cop1lc_location_register() {
        let mut bus = empty_bus();
        let cop1 = 0x0100usize;
        write_chip_word(&mut bus, cop1, 0x0080);
        write_chip_word(&mut bus, cop1 + 2, 0x0003);
        write_chip_word(&mut bus, cop1 + 4, 0x0082);
        write_chip_word(&mut bus, cop1 + 6, 0x0200);
        write_chip_word(&mut bus, cop1 + 8, 0x0180);
        write_chip_word(&mut bus, cop1 + 10, 0x0555);
        write_chip_word(&mut bus, cop1 + 12, 0xFFFF);
        write_chip_word(&mut bus, cop1 + 14, 0xFFFE);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN;
        bus.agnus.hpos = 0x20;
        assert!(!bus.custom_write(0x080, 2, 0x0000));
        assert!(!bus.custom_write(0x082, 2, cop1 as u64));
        assert!(!bus.custom_write(0x088, 2, 0xFFFF));
        bus.advance_chipset(12);

        assert_eq!(bus.agnus.cop1lc, 0x030200);
        assert_eq!(bus.denise.palette[0], 0x0555);
    }

    #[test]
    fn copper_cannot_set_copcon_itself() {
        let mut bus = empty_bus();
        let cop1 = 0x0100usize;
        write_chip_word(&mut bus, cop1, 0x002E);
        write_chip_word(&mut bus, cop1 + 2, 0x0002);
        write_chip_word(&mut bus, cop1 + 4, 0x0040);
        write_chip_word(&mut bus, cop1 + 6, 0x4321);
        write_chip_word(&mut bus, cop1 + 8, 0xFFFF);
        write_chip_word(&mut bus, cop1 + 10, 0xFFFE);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN;
        bus.agnus.hpos = 0x20;
        assert!(!bus.custom_write(0x080, 2, 0x0000));
        assert!(!bus.custom_write(0x082, 2, cop1 as u64));
        assert!(!bus.custom_write(0x088, 2, 0xFFFF));
        bus.advance_chipset(8);

        assert_eq!(bus.agnus.copcon, 0);
        assert_eq!(bus.blitter.bltcon0, 0);
    }

    #[test]
    fn cpu_copjmp1_strobe_waits_for_target_instruction_dma_slots() {
        let mut bus = empty_bus();
        let cop1 = 0x0100usize;
        write_chip_word(&mut bus, cop1, 0x0180);
        write_chip_word(&mut bus, cop1 + 2, 0x0123);
        write_chip_word(&mut bus, cop1 + 4, 0xFFFF);
        write_chip_word(&mut bus, cop1 + 6, 0xFFFE);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN;
        bus.agnus.hpos = 0x20;
        assert!(!bus.custom_write(0x080, 2, 0x0000));
        assert!(!bus.custom_write(0x082, 2, cop1 as u64));
        assert!(!bus.custom_write(0x088, 2, 0xFFFF));

        bus.advance_chipset(1);
        assert_eq!(bus.denise.palette[0], 0);
        assert!(bus.current_render_events().is_empty());

        // First-word fetch at 0x20, idle half at 0x21, second-word fetch+write
        // at 0x22.
        bus.advance_chipset(2);
        assert_eq!(bus.denise.palette[0], 0x0123);
        let event = &bus.current_render_events()[0];
        assert_eq!(event.hpos, 0x22);
        assert_eq!(event.source, super::BeamWriteSource::Copper);
    }

    #[test]
    fn copper_move_to_copjmp2_loads_second_list() {
        let mut bus = empty_bus();
        let cop1 = 0x0100usize;
        let cop2 = 0x0200usize;
        write_chip_word(&mut bus, cop1, 0x008A);
        write_chip_word(&mut bus, cop1 + 2, 0x0000);
        write_chip_word(&mut bus, cop1 + 4, 0xFFFF);
        write_chip_word(&mut bus, cop1 + 6, 0xFFFE);
        write_chip_word(&mut bus, cop2, 0x0180);
        write_chip_word(&mut bus, cop2 + 2, 0x0456);
        write_chip_word(&mut bus, cop2 + 4, 0xFFFF);
        write_chip_word(&mut bus, cop2 + 6, 0xFFFE);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN;
        bus.agnus.hpos = 0x20;
        assert!(!bus.custom_write(0x080, 2, 0x0000));
        assert!(!bus.custom_write(0x082, 2, cop1 as u64));
        assert!(!bus.custom_write(0x084, 2, 0x0000));
        assert!(!bus.custom_write(0x086, 2, cop2 as u64));
        assert!(!bus.custom_write(0x088, 2, 0xFFFF));
        // COPJMP2 MOVE fetch (4 cck) then the cop2 MOVE fetch (4 cck): write at
        // hpos 0x26.
        bus.advance_chipset(7);

        assert_eq!(bus.denise.palette[0], 0x0456);
        assert_eq!(
            bus.frame_render_events()[0].source,
            super::BeamWriteSource::Copper
        );
    }

    #[test]
    fn copper_copjmp2_strobe_waits_for_target_instruction_dma_slots() {
        let mut bus = empty_bus();
        let cop1 = 0x0100usize;
        let cop2 = 0x0200usize;
        write_chip_word(&mut bus, cop1, 0x008A);
        write_chip_word(&mut bus, cop1 + 2, 0x0000);
        write_chip_word(&mut bus, cop1 + 4, 0xFFFF);
        write_chip_word(&mut bus, cop1 + 6, 0xFFFE);
        write_chip_word(&mut bus, cop2, 0x0180);
        write_chip_word(&mut bus, cop2 + 2, 0x0456);
        write_chip_word(&mut bus, cop2 + 4, 0xFFFF);
        write_chip_word(&mut bus, cop2 + 6, 0xFFFE);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN;
        bus.agnus.hpos = 0x20;
        assert!(!bus.custom_write(0x080, 2, 0x0000));
        assert!(!bus.custom_write(0x082, 2, cop1 as u64));
        assert!(!bus.custom_write(0x084, 2, 0x0000));
        assert!(!bus.custom_write(0x086, 2, cop2 as u64));
        assert!(!bus.custom_write(0x088, 2, 0xFFFF));

        bus.advance_chipset(2);
        assert_eq!(bus.denise.palette[0], 0);
        assert!(bus.current_render_events().is_empty());

        bus.advance_chipset(1);
        assert_eq!(bus.denise.palette[0], 0);
        assert!(bus.current_render_events().is_empty());

        // COPJMP2 strobe completes at 0x22; then idle half, the cop2 MOVE fetch
        // (4 cck), writing at hpos 0x26.
        bus.advance_chipset(4);
        assert_eq!(bus.denise.palette[0], 0x0456);
        let event = &bus.current_render_events()[0];
        assert_eq!(event.hpos, 0x26);
        assert_eq!(event.source, super::BeamWriteSource::Copper);
    }

    #[test]
    fn copper_can_program_cop2lc_before_copjmp2_loop_branch() {
        let mut bus = empty_bus();
        let cop1 = 0x0100usize;
        let stale_cop2 = 0x0200usize;
        let programmed_cop2 = 0x0300usize;
        write_chip_word(&mut bus, cop1, 0x0084);
        write_chip_word(&mut bus, cop1 + 2, 0x0000);
        write_chip_word(&mut bus, cop1 + 4, 0x0086);
        write_chip_word(&mut bus, cop1 + 6, programmed_cop2 as u16);
        write_chip_word(&mut bus, cop1 + 8, 0x008A);
        write_chip_word(&mut bus, cop1 + 10, 0x0000);
        write_chip_word(&mut bus, cop1 + 12, 0xFFFF);
        write_chip_word(&mut bus, cop1 + 14, 0xFFFE);
        write_chip_word(&mut bus, stale_cop2, 0x0180);
        write_chip_word(&mut bus, stale_cop2 + 2, 0x0111);
        write_chip_word(&mut bus, programmed_cop2, 0x0180);
        write_chip_word(&mut bus, programmed_cop2 + 2, 0x0789);
        write_chip_word(&mut bus, programmed_cop2 + 4, 0xFFFF);
        write_chip_word(&mut bus, programmed_cop2 + 6, 0xFFFE);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN;
        bus.agnus.hpos = 0x20;
        assert!(!bus.custom_write(0x080, 2, 0x0000));
        assert!(!bus.custom_write(0x082, 2, cop1 as u64));
        assert!(!bus.custom_write(0x084, 2, 0x0000));
        assert!(!bus.custom_write(0x086, 2, stale_cop2 as u64));
        assert!(!bus.custom_write(0x088, 2, 0xFFFF));
        // Three programming MOVEs plus the jumped-to MOVE, each a 4-color-clock
        // fetch: the final write lands at hpos 0x2E.
        bus.advance_chipset(15);

        assert_eq!(bus.agnus.cop2lc, programmed_cop2 as u32);
        assert_eq!(bus.denise.palette[0], 0x0789);
    }

    #[test]
    fn copper_can_program_cop1lc_before_copjmp1_loop_branch() {
        let mut bus = empty_bus();
        let cop1 = 0x0100usize;
        let programmed_cop1 = 0x0300usize;
        write_chip_word(&mut bus, cop1, 0x0080);
        write_chip_word(&mut bus, cop1 + 2, 0x0000);
        write_chip_word(&mut bus, cop1 + 4, 0x0082);
        write_chip_word(&mut bus, cop1 + 6, programmed_cop1 as u16);
        write_chip_word(&mut bus, cop1 + 8, 0x0088);
        write_chip_word(&mut bus, cop1 + 10, 0x0000);
        write_chip_word(&mut bus, cop1 + 12, 0xFFFF);
        write_chip_word(&mut bus, cop1 + 14, 0xFFFE);
        write_chip_word(&mut bus, programmed_cop1, 0x0180);
        write_chip_word(&mut bus, programmed_cop1 + 2, 0x0789);
        write_chip_word(&mut bus, programmed_cop1 + 4, 0xFFFF);
        write_chip_word(&mut bus, programmed_cop1 + 6, 0xFFFE);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN;
        bus.agnus.hpos = 0x20;
        assert!(!bus.custom_write(0x080, 2, 0x0000));
        assert!(!bus.custom_write(0x082, 2, cop1 as u64));
        assert!(!bus.custom_write(0x088, 2, 0xFFFF));
        // Two programming MOVEs and the COPJMP1 MOVE, then the jumped-to MOVE,
        // each a 4-color-clock fetch: the final write lands at hpos 0x2E.
        bus.advance_chipset(15);

        assert_eq!(bus.agnus.cop1lc, programmed_cop1 as u32);
        assert_eq!(bus.denise.palette[0], 0x0789);
    }

    #[test]
    fn copper_programmed_cop1lc_sets_automatic_frame_restart() {
        let mut bus = empty_bus();
        let cop1 = 0x0100usize;
        let programmed_cop1 = 0x0300usize;
        write_chip_word(&mut bus, cop1, 0x0080);
        write_chip_word(&mut bus, cop1 + 2, 0x0000);
        write_chip_word(&mut bus, cop1 + 4, 0x0082);
        write_chip_word(&mut bus, cop1 + 6, programmed_cop1 as u16);
        write_chip_word(&mut bus, cop1 + 8, 0xFFFF);
        write_chip_word(&mut bus, cop1 + 10, 0xFFFE);
        write_chip_word(&mut bus, programmed_cop1, 0x0180);
        write_chip_word(&mut bus, programmed_cop1 + 2, 0x0789);
        write_chip_word(&mut bus, programmed_cop1 + 4, 0xFFFF);
        write_chip_word(&mut bus, programmed_cop1 + 6, 0xFFFE);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN;
        bus.agnus.hpos = 0x20;
        assert!(!bus.custom_write(0x080, 2, 0x0000));
        assert!(!bus.custom_write(0x082, 2, cop1 as u64));
        assert!(!bus.custom_write(0x088, 2, 0xFFFF));
        bus.advance_chipset(8);

        assert_eq!(bus.agnus.cop1lc, programmed_cop1 as u32);

        bus.agnus.vpos = crate::chipset::agnus::PAL_LINES - 1;
        bus.agnus.hpos = COLORCLOCKS_PER_LINE - 1;
        bus.advance_chipset(1);

        // Restart is immediate at the top of the frame, picking up the
        // copper-programmed COP1LC straight away.
        assert_eq!(bus.pending_copper_frame_start, None);
        bus.advance_chipset(16);
        assert_eq!(bus.denise.palette[0], 0x0789);
    }

    #[test]
    fn automatic_vblank_reload_restarts_cop1_after_copjmp2_branch() {
        let mut bus = empty_bus();
        let cop1 = 0x0100usize;
        let cop2 = 0x0200usize;
        write_chip_word(&mut bus, cop1, 0x0180);
        write_chip_word(&mut bus, cop1 + 2, 0x0111);
        write_chip_word(&mut bus, cop1 + 4, 0x008A);
        write_chip_word(&mut bus, cop1 + 6, 0x0000);
        write_chip_word(&mut bus, cop1 + 8, 0xFFFF);
        write_chip_word(&mut bus, cop1 + 10, 0xFFFE);
        write_chip_word(&mut bus, cop2, 0x0180);
        write_chip_word(&mut bus, cop2 + 2, 0x0222);
        write_chip_word(&mut bus, cop2 + 4, 0xFFFF);
        write_chip_word(&mut bus, cop2 + 6, 0xFFFE);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN;
        bus.agnus.hpos = 0x20;
        assert!(!bus.custom_write(0x080, 2, 0x0000));
        assert!(!bus.custom_write(0x082, 2, cop1 as u64));
        assert!(!bus.custom_write(0x084, 2, 0x0000));
        assert!(!bus.custom_write(0x086, 2, cop2 as u64));
        assert!(!bus.custom_write(0x088, 2, 0xFFFF));
        // MOVE 0x0111 writes at 0x22; the COPJMP2 MOVE then the cop2 MOVE follow
        // at the 4-color-clock cadence, writing 0x0222 at hpos 0x2A.
        bus.advance_chipset(11);

        assert_eq!(bus.denise.palette[0], 0x0222);
        assert_eq!(bus.current_render_events()[0].hpos, 0x22);
        assert_eq!(bus.current_render_events()[1].hpos, 0x2A);

        bus.agnus.vpos = crate::chipset::agnus::PAL_LINES - 1;
        bus.agnus.hpos = COLORCLOCKS_PER_LINE - 1;
        bus.advance_chipset(1);

        // Restart is immediate at the top of the frame (vpos 0), not delayed to
        // the end of vblank: the live COP1LC reload happens as the beam wraps.
        assert_eq!(bus.pending_copper_frame_start, None);
        assert_eq!(bus.denise.palette[0], 0x0222);

        let event_count = bus.current_render_events().len();

        // From hpos 0 the restarted Copper's MOVE fetches on the first two free
        // access-parity color clocks. With the hardware refresh model (slots
        // 0x004/6/8/A) those are hpos 0x00 and 0x02, so the write lands at 0x02.
        bus.advance_chipset(3);
        assert_eq!(bus.denise.palette[0], 0x0111);
        let event = &bus.current_render_events()[event_count];
        assert_eq!(event.vpos, 0);
        assert_eq!(event.hpos, 0x02);
        assert_eq!(event.source, super::BeamWriteSource::Copper);
    }

    #[test]
    fn cpu_palette_writes_snapshot_top_and_status_palettes_by_interrupt_source() {
        let mut bus = empty_bus();

        assert!(!bus.custom_write(0x180, 2, 0x0123));
        assert_eq!(bus.beam_top_palette[0], 0x0123);
        assert!(!bus.beam_bottom_palette_valid);

        bus.agnus.vpos = 0xD4;
        bus.delivered_irq_pending = INT_COPER;
        assert!(!bus.custom_write(0x09C, 2, INT_COPER as u64));
        assert!(!bus.custom_write(0x182, 2, 0x0456));
        assert_eq!(bus.beam_top_palette[1], 0x0456);
        assert_eq!(bus.beam_bottom_palette[1], 0x0456);
        assert!(bus.beam_bottom_palette_valid);
        assert_eq!(
            bus.frame_render_events().last().map(|event| event.source),
            Some(super::BeamWriteSource::CpuCopperIrq)
        );

        bus.agnus.vpos = 0x10;
        assert!(!bus.custom_write(0x09C, 2, INT_VERTB as u64));
        assert!(!bus.custom_write(0x184, 2, 0x0789));
        assert_eq!(bus.beam_top_palette[2], 0x0789);
        assert_eq!(
            bus.frame_render_events().last().map(|event| event.source),
            Some(super::BeamWriteSource::Cpu)
        );
    }

    #[test]
    fn intreq_palette_target_uses_delivered_interrupt_when_coper_and_vertb_clear_together() {
        let mut bus = empty_bus();

        bus.delivered_irq_pending = INT_VERTB;
        assert!(!bus.custom_write(0x09C, 2, (INT_COPER | INT_VERTB) as u64));
        assert!(!bus.custom_write(0x180, 2, 0x0111));
        assert_eq!(bus.beam_top_palette[0], 0x0111);

        bus.agnus.vpos = 0xD4;
        bus.delivered_irq_pending = INT_COPER | INT_VERTB;
        assert!(!bus.custom_write(0x09C, 2, (INT_COPER | INT_VERTB) as u64));
        assert!(!bus.custom_write(0x182, 2, 0x0222));
        assert_eq!(bus.beam_top_palette[1], 0x0222);
        assert_eq!(bus.beam_bottom_palette[1], 0x0222);
        assert!(bus.beam_bottom_palette_valid);
    }

    #[test]
    fn cpu_copper_irq_palette_events_persist_with_bottom_palette_for_render_replay() {
        let mut bus = empty_bus();

        bus.agnus.vpos = 0xFA;
        bus.delivered_irq_pending = INT_COPER;
        bus.delivered_copper_irq_beam = Some((0xD4, 0x16));
        assert!(!bus.custom_write(0x09C, 2, INT_COPER as u64));
        for idx in 0..16 {
            let value = if idx == 0 {
                0x0222
            } else {
                (0x0200 + idx) & 0x0FFF
            };
            assert!(!bus.custom_write(0x180 + idx * 2, 2, value));
        }
        assert_eq!(bus.frame_bottom_palette_events().len(), 16);
        assert_eq!(bus.frame_bottom_palette_events()[0].vpos, 0xD4);
        assert_eq!(bus.frame_bottom_palette_events()[0].hpos, 0x16);
        assert_eq!(bus.beam_top_palette[0], 0x0222);
        assert_eq!(bus.beam_top_palette[15], 0x020F);
        assert_eq!(bus.beam_bottom_palette[0], 0x0222);
        assert_eq!(bus.beam_bottom_palette[15], 0x020F);

        bus.agnus.vpos = 0xFA;
        bus.delivered_irq_pending = INT_COPER;
        bus.delivered_copper_irq_beam = Some((0x86, 0x10));
        assert!(!bus.custom_write(0x09C, 2, INT_COPER as u64));
        for idx in 0..16 {
            assert!(!bus.custom_write(0x180 + idx * 2, 2, (0x0400 + idx) & 0x0FFF));
        }
        assert_eq!(bus.beam_bottom_palette[0], 0x0222);
        assert_eq!(bus.frame_bottom_palette_events().len(), 16);
        assert_eq!(bus.frame_bottom_palette_events()[0].vpos, 0xD4);

        bus.begin_new_beam_frame();
        assert_eq!(bus.frame_bottom_palette_events().len(), 16);
        assert_eq!(bus.frame_bottom_palette_events()[0].vpos, 0xD4);
        for _ in 0..256 {
            bus.begin_new_beam_frame();
        }
        assert_eq!(bus.frame_bottom_palette_events().len(), 16);
        assert_eq!(bus.frame_bottom_palette_events()[0].vpos, 0xD4);
    }

    #[test]
    fn cpu_copper_irq_high_palette_events_commit_bottom_replay_at_colour31() {
        let mut bus = empty_bus();

        bus.agnus.vpos = 0xFA;
        bus.delivered_irq_pending = INT_COPER;
        bus.delivered_copper_irq_beam = Some((0xD4, 0x16));
        assert!(!bus.custom_write(0x09C, 2, INT_COPER as u64));
        for idx in 17..32 {
            assert!(!bus.custom_write(0x180 + idx * 2, 2, (0x0300 + idx) & 0x0FFF));
        }

        assert_eq!(bus.frame_bottom_palette_events().len(), 15);
        assert_eq!(bus.frame_bottom_palette_events()[0].vpos, 0xD4);
        assert_eq!(bus.frame_bottom_palette_events()[0].hpos, 0x16);
        assert_eq!(bus.frame_bottom_palette_events()[0].offset, 0x1A2);
        assert_eq!(bus.frame_bottom_palette_events()[14].offset, 0x1BE);
        assert_eq!(bus.beam_bottom_palette[31], 0x031F);
    }

    #[test]
    fn cpu_copper_irq_render_event_uses_actual_write_beam() {
        let mut bus = empty_bus();

        bus.delivered_irq_pending = INT_COPER;
        bus.delivered_copper_irq_beam = Some((0xD4, 0x16));
        assert!(!bus.custom_write(0x09C, 2, INT_COPER as u64));
        bus.agnus.vpos = 0xFA;
        bus.agnus.hpos = 0x40;
        assert!(!bus.custom_write(0x180, 2, 0x0222));

        let render_event = bus.frame_render_events().last().unwrap();
        assert_eq!(render_event.vpos, 0xFA);
        assert_eq!(render_event.hpos, 0x40);
        assert_eq!(bus.pending_beam_bottom_palette_events[0].vpos, 0xD4);
        assert_eq!(bus.pending_beam_bottom_palette_events[0].hpos, 0x16);
    }

    #[test]
    fn frame_palette_split_uses_completed_frame_snapshot() {
        let mut bus = empty_bus();

        bus.beam_top_palette.write_ocs(0, 0x0111);
        bus.begin_new_beam_frame();
        bus.beam_top_palette.write_ocs(0, 0x0333);
        bus.capture_current_frame_display_start();
        bus.beam_top_palette.write_ocs(0, 0x0555);
        bus.beam_bottom_palette.write_ocs(0, 0x0222);
        bus.beam_bottom_palette_valid = true;
        bus.begin_new_beam_frame();

        bus.beam_top_palette.write_ocs(0, 0x0777);
        bus.beam_bottom_palette.write_ocs(0, 0x0444);
        let (top, bottom, valid) = bus.frame_palette_split();

        assert_eq!(top[0], 0x0333);
        assert_eq!(bottom[0], 0x0222);
        assert!(valid);
    }

    #[test]
    fn frame_chip_ram_uses_completed_frame_display_start_snapshot() {
        let mut bus = empty_bus();

        bus.mem.chip_ram[0] = 0x12;
        bus.begin_new_beam_frame();
        bus.mem.chip_ram[0] = 0x34;
        bus.capture_current_frame_display_start();
        bus.mem.chip_ram[0] = 0x56;
        bus.begin_new_beam_frame();
        bus.mem.chip_ram[0] = 0x78;

        assert_eq!(bus.frame_chip_ram()[0], 0x34);
    }

    #[test]
    fn bitplane_dma_capture_samples_words_at_fetch_time() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = 0x3E;
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0040;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.bplpt[0] = 0x0100;
        bus.display_dma_bplpt[0] = 0x0100;
        write_chip_word(&mut bus, 0x0100, 0x1111);
        write_chip_word(&mut bus, 0x0102, 0x2222);

        bus.advance_chipset(2);
        write_chip_word(&mut bus, 0x0100, 0xAAAA);
        write_chip_word(&mut bus, 0x0102, 0xBBBB);
        bus.advance_chipset(8);

        let row = bus.frame_captured_bitplane_rows()[0].as_ref().unwrap();
        assert_eq!(row.planes[0], vec![0x1111, 0xBBBB]);
        assert_eq!(bus.display_dma_bplpt[0], 0x0104);
    }

    #[test]
    fn bitplane_dma_capture_clips_ddfstart_to_hard_fetch_window() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = 0x16;
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0010;
        bus.denise.ddfstop = 0x0018;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.bplpt[0] = 0x0100;
        bus.display_dma_bplpt[0] = 0x0100;
        write_chip_word(&mut bus, 0x0100, 0x1111);
        write_chip_word(&mut bus, 0x0102, 0x2222);

        bus.advance_chipset(10);

        let row = bus.frame_captured_bitplane_rows()[0].as_ref().unwrap();
        assert_eq!(row.words_per_row, 1);
        assert_eq!(row.planes[0], vec![0x1111]);
        assert_eq!(bus.display_dma_bplpt[0], 0x0102);
    }

    #[test]
    fn ocs_bitplane_dma_capture_extends_equal_ddf_window_to_hard_stop() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = 0x3E;
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0038;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.bplpt[0] = 0x0100;
        bus.display_dma_bplpt[0] = 0x0100;
        for word in 0..21 {
            write_chip_word(&mut bus, 0x0100 + word * 2, 0x8000 | word as u16);
        }

        bus.advance_chipset(0x00E0 - 0x003E);

        let row = bus.frame_captured_bitplane_rows()[0].as_ref().unwrap();
        assert_eq!(row.words_per_row, 21);
        assert_eq!(row.planes[0].len(), 21);
        assert_eq!(row.planes[0][0], 0x8000);
        assert_eq!(row.planes[0][20], 0x8014);
        assert_eq!(bus.display_dma_bplpt[0], 0x012A);
    }

    #[test]
    fn wide_fmode_dma_capture_packs_lores_slots_in_fetch_units() {
        // Lores FMODE=3 (32-cck fetch units) with DDFSTRT $30 / DDFSTOP
        // $D0: Agnus runs six 32-cck units from the DDFSTRT comparator and
        // packs the lores plane slots into the first eight CCK of each unit.
        // The final unit still completes before the PAL line edge, so the row
        // modulo advances after all 24 fetched words.
        let mut bus = empty_bus();
        bus.set_chipset_revisions(AgnusRevision::AgaAlice, DeniseRevision::AgaLisa);
        bus.agnus.write_fmode(0x0003); // BPL32 | BPAGEM = 64-bit fetches
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = 0x16;
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0030;
        bus.denise.ddfstop = 0x00D0;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.bpl1mod = 0x0010;
        bus.denise.bplpt[0] = 0x0100;
        bus.display_dma_bplpt[0] = 0x0100;
        for word in 0..24 {
            write_chip_word(&mut bus, 0x0100 + word * 2, 0x9000 | word as u16);
        }

        // Advance through the whole line: every fetch unit lies inside it.
        bus.advance_chipset(0x00E3 - 0x16);

        let row = bus.frame_captured_bitplane_rows()[0].as_ref().unwrap();
        assert_eq!(row.words_per_row, 24);
        assert_eq!(row.planes[0][0], 0x9000);
        assert_eq!(row.planes[0][23], 0x9017);
        // The line completed, so the modulo advanced the pointer past the
        // 48 fetched bytes.
        assert_eq!(bus.display_dma_bplpt[0], 0x0100 + 48 + 0x10);
    }

    #[test]
    fn ecs_bitplane_dma_capture_stops_equal_ddf_window_after_one_fetch_cycle() {
        let mut bus = empty_bus();
        bus.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = 0x3E;
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0038;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.bplpt[0] = 0x0100;
        bus.display_dma_bplpt[0] = 0x0100;
        write_chip_word(&mut bus, 0x0100, 0xCAFE);
        write_chip_word(&mut bus, 0x0102, 0xBEEF);

        bus.advance_chipset(0x00E0 - 0x003E);

        let row = bus.frame_captured_bitplane_rows()[0].as_ref().unwrap();
        assert_eq!(row.words_per_row, 1);
        assert_eq!(row.planes[0], vec![0xCAFE]);
        assert_eq!(bus.display_dma_bplpt[0], 0x0102);
    }

    #[test]
    fn bitplane_dmacon_enable_reaches_fetcher_after_two_cck() {
        let mut bus = empty_bus();
        bus.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        bus.agnus.dmacon = DMACON_DMAEN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = 0x3E;
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0040;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.bplpt[0] = 0x0100;
        bus.display_dma_bplpt[0] = 0x0100;
        write_chip_word(&mut bus, 0x0100, 0x1111);
        write_chip_word(&mut bus, 0x0102, 0x2222);

        assert!(!bus.write_custom_word_from(0x096, 0x8000 | DMACON_BPLEN, BeamWriteSource::Cpu));
        bus.advance_chipset(0x0048 - 0x003E);

        let row = bus.frame_captured_bitplane_rows()[0].as_ref().unwrap();
        assert_eq!(row.words_per_row, 2);
        assert_eq!(row.planes[0], vec![0x0000, 0x1111]);
        assert_eq!(bus.display_dma_bplpt[0], 0x0102);
    }

    #[test]
    fn bitplane_dmacon_clear_reaches_fetcher_after_two_cck() {
        let mut bus = empty_bus();
        bus.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = 0x3E;
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0040;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.bplpt[0] = 0x0100;
        bus.display_dma_bplpt[0] = 0x0100;
        write_chip_word(&mut bus, 0x0100, 0x1111);
        write_chip_word(&mut bus, 0x0102, 0x2222);

        assert!(!bus.write_custom_word_from(0x096, DMACON_BPLEN, BeamWriteSource::Cpu));
        bus.advance_chipset(0x0048 - 0x003E);

        let row = bus.frame_captured_bitplane_rows()[0].as_ref().unwrap();
        assert_eq!(row.words_per_row, 2);
        assert_eq!(row.planes[0], vec![0x1111, 0x0000]);
        assert_eq!(bus.display_dma_bplpt[0], 0x0102);
    }

    #[test]
    fn bitplane_bplcon0_enable_reaches_fetcher_after_three_cck() {
        let mut bus = empty_bus();
        bus.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = 0x3D;
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0040;
        bus.denise.bplcon0 = 0x0000;
        bus.denise.bplpt[0] = 0x0100;
        bus.display_dma_bplpt[0] = 0x0100;
        write_chip_word(&mut bus, 0x0100, 0x1111);
        write_chip_word(&mut bus, 0x0102, 0x2222);

        assert!(!bus.write_custom_word_from(0x100, 0x1000, BeamWriteSource::Cpu));
        bus.advance_chipset(0x0048 - 0x003D);

        let row = bus.frame_captured_bitplane_rows()[0].as_ref().unwrap();
        assert_eq!(row.words_per_row, 2);
        assert_eq!(row.planes[0], vec![0x0000, 0x1111]);
        assert_eq!(bus.display_dma_bplpt[0], 0x0102);
    }

    #[test]
    fn bitplane_bplcon0_clear_reaches_fetcher_after_three_cck() {
        let mut bus = empty_bus();
        bus.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = 0x3D;
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0040;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.bplpt[0] = 0x0100;
        bus.display_dma_bplpt[0] = 0x0100;
        write_chip_word(&mut bus, 0x0100, 0x1111);
        write_chip_word(&mut bus, 0x0102, 0x2222);

        assert!(!bus.write_custom_word_from(0x100, 0x0000, BeamWriteSource::Cpu));
        bus.advance_chipset(0x0048 - 0x003D);

        let row = bus.frame_captured_bitplane_rows()[0].as_ref().unwrap();
        assert_eq!(row.words_per_row, 2);
        assert_eq!(row.planes[0], vec![0x1111, 0x0000]);
        assert_eq!(bus.display_dma_bplpt[0], 0x0102);
    }

    #[test]
    fn bitplane_dma_latches_plane_count_per_fetch_block() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = 0x30;
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0030;
        bus.denise.ddfstop = 0x0038;
        bus.denise.bplcon0 = 0x5200;
        bus.denise.bpl2mod = 4;
        for plane in 0..6 {
            let ptr = 0x0100 + plane * 0x0100;
            bus.denise.bplpt[plane] = ptr as u32;
            bus.display_dma_bplpt[plane] = ptr as u32;
            write_chip_word(&mut bus, ptr, 0x1000 | plane as u16);
            write_chip_word(&mut bus, ptr + 2, 0x2000 | plane as u16);
        }

        bus.advance_chipset(2);
        assert_eq!(bus.agnus.hpos, 0x32);
        assert!(!bus.write_custom_word_from(0x100, 0x6200, BeamWriteSource::Cpu));
        bus.advance_chipset(0x0040 - 0x0032);

        let row = bus.frame_captured_bitplane_rows()[0].as_ref().unwrap();
        assert_eq!(row.nplanes, 6);
        assert_eq!(row.words_per_row, 2);
        assert_eq!(row.planes[5], vec![0x0000, 0x1005]);
        assert_eq!(bus.display_dma_bplpt[0], 0x0104);
        assert_eq!(bus.display_dma_bplpt[4], 0x0504);
        assert_eq!(bus.display_dma_bplpt[5], 0x0606);
    }

    #[test]
    fn bitplane_ddfstrt_write_at_match_does_not_start_current_line() {
        let mut bus = empty_bus();
        bus.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = 0x38;
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0050;
        bus.denise.ddfstop = 0x0040;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.bplpt[0] = 0x0100;
        bus.display_dma_bplpt[0] = 0x0100;
        write_chip_word(&mut bus, 0x0100, 0x1111);
        write_chip_word(&mut bus, 0x0102, 0x2222);

        assert!(!bus.write_custom_word_from(0x092, 0x0038, BeamWriteSource::Cpu));
        bus.advance_chipset(0x0048 - 0x0038);

        assert!(bus.frame_captured_bitplane_rows()[0].is_none());
        assert_eq!(bus.display_dma_bplpt[0], 0x0100);
    }

    #[test]
    fn bitplane_ddfstrt_write_before_match_starts_current_line() {
        let mut bus = empty_bus();
        bus.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = 0x37;
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0050;
        bus.denise.ddfstop = 0x0040;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.bplpt[0] = 0x0100;
        bus.display_dma_bplpt[0] = 0x0100;
        write_chip_word(&mut bus, 0x0100, 0x1111);
        write_chip_word(&mut bus, 0x0102, 0x2222);

        assert!(!bus.write_custom_word_from(0x092, 0x0038, BeamWriteSource::Cpu));
        bus.advance_chipset(0x0048 - 0x0037);

        let row = bus.frame_captured_bitplane_rows()[0].as_ref().unwrap();
        assert_eq!(row.words_per_row, 2);
        assert_eq!(row.planes[0], vec![0x1111, 0x2222]);
        assert_eq!(bus.display_dma_bplpt[0], 0x0104);
    }

    #[test]
    fn bitplane_dma_capture_scans_fetch_window_independent_of_owner_hint() {
        let mut bus = empty_bus();
        bus.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = 0x3F;
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0038;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.bplpt[0] = 0x0100;
        bus.display_dma_bplpt[0] = 0x0100;
        write_chip_word(&mut bus, 0x0100, 0xCAFE);

        // Render capture is derived from the beam interval and DMA registers;
        // a coarse bus-owner hint must not suppress a due fetch.
        let (cck, tick) = bus.advance_one_chip_bus_quantum_limited(Some(ChipBusOwner::Idle), 2);

        assert_eq!(cck, 1);
        assert_eq!(tick.new_lines, 0);
        let row = bus.frame_captured_bitplane_rows()[0].as_ref().unwrap();
        assert_eq!(row.planes[0], vec![0xCAFE]);
        assert_eq!(bus.display_dma_bplpt[0], 0x0102);
    }

    #[test]
    fn bitplane_dma_capture_maps_early_vertical_overscan_to_first_framebuffer_row() {
        let mut bus = empty_bus();
        bus.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.agnus.vpos = RENDER_MIN_OVERSCAN_START_VPOS;
        bus.agnus.hpos = 0x3E;
        bus.denise.diwstrt = 0x1C81;
        bus.denise.diwstop = 0x1DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0038;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.bplpt[0] = 0x0100;
        bus.display_dma_bplpt[0] = 0x0100;
        write_chip_word(&mut bus, 0x0100, 0xCAFE);

        bus.capture_current_frame_display_start();
        bus.advance_chipset(2);

        assert_eq!(
            bus.current_frame_visible_start_vpos,
            RENDER_MIN_OVERSCAN_START_VPOS
        );
        let row = bus.frame_captured_bitplane_rows()[0].as_ref().unwrap();
        assert_eq!(row.planes[0], vec![0xCAFE]);
        assert_eq!(bus.display_dma_bplpt[0], 0x0102);
    }

    #[test]
    fn ecs_diwstrt_current_line_write_starts_live_bitplane_dma() {
        let mut bus = empty_bus();
        bus.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.agnus.vpos = RENDER_VISIBLE_START_VPOS;
        bus.agnus.hpos = 0x30;
        bus.current_frame_visible_start_vpos = RENDER_VISIBLE_START_VPOS + 1;
        bus.denise.diwstrt = ((RENDER_VISIBLE_START_VPOS + 1) as u16) << 8 | 0x0083;
        bus.denise.diwstop = ((RENDER_VISIBLE_START_VPOS + 2) as u16) << 8 | 0x00C1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0040;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.bplpt[0] = 0x0100;
        bus.display_dma_bplpt[0] = 0x0100;
        write_chip_word(&mut bus, 0x0100, 0x1111);
        write_chip_word(&mut bus, 0x0102, 0x2222);

        assert!(!bus.write_custom_word_from(
            0x08E,
            (RENDER_VISIBLE_START_VPOS as u16) << 8 | 0x0083,
            BeamWriteSource::Cpu
        ));
        bus.advance_chipset(0x20);

        assert!(bus.current_frame_display_snapshot_taken);
        assert_eq!(
            bus.current_frame_visible_start_vpos,
            RENDER_VISIBLE_START_VPOS
        );
        let row = bus.frame_captured_bitplane_rows()[0].as_ref().unwrap();
        assert_eq!(row.words_per_row, 2);
        assert_eq!(row.planes[0], vec![0x1111, 0x2222]);
    }

    #[test]
    fn ocs_diwstrt_current_line_write_does_not_start_live_bitplane_dma() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.agnus.vpos = RENDER_VISIBLE_START_VPOS;
        bus.agnus.hpos = 0x30;
        bus.current_frame_visible_start_vpos = RENDER_VISIBLE_START_VPOS + 1;
        bus.denise.diwstrt = ((RENDER_VISIBLE_START_VPOS + 1) as u16) << 8 | 0x0083;
        bus.denise.diwstop = ((RENDER_VISIBLE_START_VPOS + 2) as u16) << 8 | 0x00C1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0040;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.bplpt[0] = 0x0100;
        bus.display_dma_bplpt[0] = 0x0100;
        write_chip_word(&mut bus, 0x0100, 0x1111);
        write_chip_word(&mut bus, 0x0102, 0x2222);

        assert!(!bus.write_custom_word_from(
            0x08E,
            (RENDER_VISIBLE_START_VPOS as u16) << 8 | 0x0083,
            BeamWriteSource::Cpu
        ));
        bus.advance_chipset(0x20);

        assert!(!bus.current_frame_display_snapshot_taken);
        assert!(bus.frame_captured_bitplane_rows()[0].is_none());
        assert_eq!(bus.display_dma_bplpt[0], 0x0100);
    }

    #[test]
    fn ecs_diwstrt_diwstop_write_reverts_to_implicit_diwhigh() {
        // ECS DIWHIGH only supplies the window MSBs when written after
        // DIWSTRT/DIWSTOP. A later DIWSTRT or DIWSTOP write must revert to
        // implicit (OCS-complement) decoding so that a program which sets the
        // window without DIWHIGH after an ECS display wrote DIWHIGH is not
        // clipped by a stale DIWHIGH.
        let mut bus = empty_bus();
        bus.set_agnus_revision(AgnusRevision::Ecs8372Rev4);

        assert!(!bus.write_custom_word_from(0x1E4, 0x0100, BeamWriteSource::Cpu));
        assert!(bus.denise.diwhigh_written);

        // A later DIWSTOP write clears the DIWHIGH-active state.
        assert!(!bus.write_custom_word_from(0x090, 0x34D1, BeamWriteSource::Cpu));
        assert!(!bus.denise.diwhigh_written);

        // Re-arm DIWHIGH, then a DIWSTRT write clears it too.
        assert!(!bus.write_custom_word_from(0x1E4, 0x0100, BeamWriteSource::Cpu));
        assert!(bus.denise.diwhigh_written);
        assert!(!bus.write_custom_word_from(0x08E, 0x2C81, BeamWriteSource::Cpu));
        assert!(!bus.denise.diwhigh_written);
    }

    #[test]
    fn ocs_ignores_diwhigh_write_when_capturing_bitplane_dma() {
        let mut bus = empty_bus();
        assert!(!bus.write_custom_word_from(0x1E4, 0x0100, BeamWriteSource::Cpu));
        assert_eq!(bus.denise.diwhigh, 0);
        assert!(!bus.denise.diwhigh_written);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.agnus.vpos = RENDER_VISIBLE_START_VPOS;
        bus.agnus.hpos = 0x36;
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2CC1;
        bus.denise.ddfstrt = 0x0030;
        bus.denise.ddfstop = 0x0030;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.bplpt[0] = 0x0100;
        bus.display_dma_bplpt[0] = 0x0100;
        write_chip_word(&mut bus, 0x0100, 0xCAFE);

        bus.capture_current_frame_display_start();
        bus.advance_chipset(2);

        let row = bus.frame_captured_bitplane_rows()[0].as_ref().unwrap();
        assert_eq!(row.planes[0][0], 0xCAFE);
    }

    #[test]
    fn ocs_display_window_synthesizes_restricted_msb_ranges() {
        let implicit = DiwHigh::ocs_implicit();

        assert_eq!(diw_v_start(0xFC00, implicit), 0x00FC);
        assert_eq!(diw_h_start(0x00FC, implicit), 0x00FC);
        assert_eq!(diw_v_stop(0x7F00, implicit), 0x017F);
        assert_eq!(diw_v_stop(0x8000, implicit), 0x0080);
        assert_eq!(diw_h_stop(0x0001, implicit), 0x0101);
    }

    #[test]
    fn ocs_display_window_zero_start_opens_from_beam_zero() {
        let implicit = DiwHigh::ocs_implicit();

        assert_eq!(
            visible_start_vpos_for_diw(0x0000, 0x2CC1, implicit),
            RENDER_MIN_OVERSCAN_START_VPOS
        );
        assert_eq!(
            clipped_display_rows_before_visible(
                0x0000,
                0x2CC1,
                implicit,
                RENDER_MIN_OVERSCAN_START_VPOS
            ),
            RENDER_MIN_OVERSCAN_START_VPOS as usize
        );
        assert!(display_window_contains_vpos(
            0x0000,
            0x2CC1,
            implicit,
            RENDER_MIN_OVERSCAN_START_VPOS,
        ));
        assert_eq!(
            live_display_window_x(0x0000, 0x2CC1, implicit),
            (0, (0x01C1 - RENDER_DIW_HSTART_FB0) * 2)
        );
        assert_eq!(
            visible_start_vpos_for_diw(0x0000, 0x0000, implicit),
            RENDER_VISIBLE_START_VPOS
        );
    }

    #[test]
    fn ecs_diwhigh_write_zero_selects_direct_display_window_msbs() {
        let mut bus = empty_bus();
        bus.set_agnus_revision(AgnusRevision::Ecs8372Rev4);

        let before_diwhigh_write = bus.effective_diwhigh();
        assert_eq!(diw_v_stop(0x7F00, before_diwhigh_write), 0x017F);
        assert_eq!(diw_h_stop(0x0001, before_diwhigh_write), 0x0101);

        assert!(!bus.write_custom_word_from(0x1E4, 0x0000, BeamWriteSource::Cpu));
        let explicit_zero = bus.effective_diwhigh();
        assert_eq!(explicit_zero, DiwHigh::ecs_explicit(0));
        assert_eq!(diw_v_start(0xFC00, explicit_zero), 0x00FC);
        assert_eq!(diw_h_start(0x00FC, explicit_zero), 0x00FC);
        assert_eq!(diw_v_stop(0x7F00, explicit_zero), 0x007F);
        assert_eq!(diw_h_stop(0x0001, explicit_zero), 0x0001);
    }

    #[test]
    fn manual_bpl1dat_write_sets_sprite_display_enable_at_denise_hpos() {
        let mut bus = empty_bus();
        bus.agnus.vpos = RENDER_VISIBLE_START_VPOS;
        bus.agnus.hpos = RENDER_COPPER_WAIT_HPOS_FB0 + DENISE_HPOS_LAG_CCK + 2;

        assert!(!bus.write_custom_word_from(0x110, 0x8000, BeamWriteSource::Cpu));

        assert_eq!(bus.frame_sprite_display_enable_x_by_y()[0], Some(8));
    }

    #[test]
    fn bitplane_dma_bpl1dat_fetch_sets_sprite_display_enable_at_playfield_origin() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.agnus.vpos = RENDER_VISIBLE_START_VPOS;
        bus.agnus.hpos = 0x38;
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0038;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.bplpt[0] = 0x0100;
        bus.display_dma_bplpt[0] = 0x0100;
        write_chip_word(&mut bus, 0x0100, 0x4000);

        bus.advance_chipset(8);

        assert_eq!(
            bus.frame_sprite_display_enable_x_by_y()[0],
            Some(((0x81 - RENDER_DIW_HSTART_FB0) * 2) as usize)
        );
    }

    #[test]
    fn bitplane_dma_capture_keeps_pal_overscan_bottom_rows() {
        let mut bus = empty_bus();
        bus.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.denise.diwstrt = 0x1C81;
        bus.denise.diwstop = 0x3EC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0038;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.bplpt[0] = 0x0100;
        bus.display_dma_bplpt[0] = 0x0100;
        write_chip_word(&mut bus, 0x0100, 0xFACE);

        bus.capture_current_frame_display_start();
        let last_overscan_line = RENDER_VISIBLE_LINES - 1;
        bus.agnus.vpos = RENDER_MIN_OVERSCAN_START_VPOS + last_overscan_line as u32;
        bus.agnus.hpos = 0x3E;
        bus.advance_chipset(2);

        let row = bus.frame_captured_bitplane_rows()[last_overscan_line]
            .as_ref()
            .unwrap();
        assert_eq!(row.planes[0], vec![0xFACE]);
        assert_eq!(bus.display_dma_bplpt[0], 0x0102);
    }

    #[test]
    fn bitplane_dma_capture_preserves_words_when_ddfstop_extends_same_line() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = 0x3E;
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0038;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.bplpt[0] = 0x0100;
        bus.display_dma_bplpt[0] = 0x0100;
        write_chip_word(&mut bus, 0x0100, 0x1111);
        write_chip_word(&mut bus, 0x0102, 0x2222);

        bus.advance_chipset(2);
        bus.write_custom_word_from(0x094, 0x0040, BeamWriteSource::Cpu);
        bus.advance_chipset(8);

        let row = bus.frame_captured_bitplane_rows()[0].as_ref().unwrap();
        assert_eq!(row.words_per_row, 2);
        assert_eq!(row.planes[0], vec![0x1111, 0x2222]);
        assert_eq!(bus.display_dma_bplpt[0], 0x0104);
    }

    #[test]
    fn bitplane_dma_capture_leaves_unfetched_words_zero_when_ddfstop_shrinks_same_line() {
        let mut bus = empty_bus();
        bus.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = 0x3E;
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0040;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.bplpt[0] = 0x0100;
        bus.display_dma_bplpt[0] = 0x0100;
        write_chip_word(&mut bus, 0x0100, 0x1111);
        write_chip_word(&mut bus, 0x0102, 0x2222);

        bus.advance_chipset(2);
        bus.write_custom_word_from(0x094, 0x0038, BeamWriteSource::Cpu);
        bus.advance_chipset(8);

        let row = bus.frame_captured_bitplane_rows()[0].as_ref().unwrap();
        assert_eq!(row.words_per_row, 2);
        assert_eq!(row.planes[0], vec![0x1111, 0x0000]);
        assert_eq!(bus.display_dma_bplpt[0], 0x0102);
    }

    #[test]
    fn captured_bitplane_rows_render_after_later_dmacon_clears_bplen() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0038;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.palette.write_ocs(1, 0x0F00);
        bus.denise.bplpt[0] = 0x0100;
        bus.display_dma_bplpt[0] = 0x0100;
        write_chip_word(&mut bus, 0x0100, 0x4000);
        bus.current_frame_render_base = bus.capture_render_snapshot();

        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = 0x3E;
        bus.advance_chipset(2);
        assert!(bus.frame_captured_bitplane_rows()[0].is_some());

        bus.write_custom_word_from(0x096, DMACON_BPLEN, BeamWriteSource::Cpu);
        assert_eq!(bus.agnus.dmacon & DMACON_BPLEN, 0);

        let mut fb = vec![0; FB_PIXELS];
        bitplane::render(&mut bus, &mut fb);

        assert_eq!(fb[STANDARD_VISIBLE_X0], rgb12_to_rgba8(0x0F00));
    }

    #[test]
    fn beam_timed_display_window_changes_clip_later_bitplane_rows() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2EC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0048;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.palette.write_ocs(0, 0x0000);
        bus.denise.palette.write_ocs(1, 0x0F00);
        bus.denise.bplpt[0] = 0x0100;
        bus.current_frame_render_base = bus.capture_render_snapshot();
        for y in 0..2 {
            bus.current_frame_bitplane_rows[y] = Some(CapturedBitplaneRow {
                nplanes: 1,
                words_per_row: 3,
                planes: [
                    vec![0x4000, 0, 0],
                    vec![0; 3],
                    vec![0; 3],
                    vec![0; 3],
                    vec![0; 3],
                    vec![0; 3],
                    Vec::new(),
                    Vec::new(),
                ],
            });
        }
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS + 1,
            hpos: 0x38,
            offset: 0x08E,
            value: 0x2CA3,
            source: BeamWriteSource::Copper,
        });

        let mut fb = vec![0; FB_PIXELS];
        bitplane::render(&mut bus, &mut fb);

        assert_eq!(fb[STANDARD_VISIBLE_X0], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[FB_WIDTH + STANDARD_VISIBLE_X0], rgb12_to_rgba8(0x0000));
    }

    #[test]
    fn beam_timed_display_window_clips_later_bitplane_pixels_on_same_line() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0048;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.palette.write_ocs(0, 0x0000);
        bus.denise.palette.write_ocs(1, 0x0F00);
        bus.current_frame_render_base = bus.capture_render_snapshot();
        bus.current_frame_bitplane_rows[0] = Some(CapturedBitplaneRow {
            nplanes: 1,
            words_per_row: 3,
            planes: [
                vec![0xFFFF, 0xFFFF, 0xFFFF],
                vec![0; 3],
                vec![0; 3],
                vec![0; 3],
                vec![0; 3],
                vec![0; 3],
                Vec::new(),
                Vec::new(),
            ],
        });
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x40,
            offset: 0x08E,
            value: 0x2C97,
            source: BeamWriteSource::Copper,
        });

        let mut fb = vec![0; FB_PIXELS];
        bitplane::render(&mut bus, &mut fb);

        assert_eq!(fb[68], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[106], rgb12_to_rgba8(0x0000));
        assert_eq!(fb[108], rgb12_to_rgba8(0x0F00));
    }

    #[test]
    fn beam_timed_diwstrt_clips_hidden_bitplane_pixels_without_rebasing_fetch_origin() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.denise.diwstrt = 0x2C83;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0048;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.palette.write_ocs(0, 0x0000);
        bus.denise.palette.write_ocs(1, 0x0F00);
        bus.current_frame_render_base = bus.capture_render_snapshot();
        bus.current_frame_bitplane_rows[0] = Some(CapturedBitplaneRow {
            nplanes: 1,
            words_per_row: 3,
            planes: [
                vec![0x0400, 0, 0],
                vec![0; 3],
                vec![0; 3],
                vec![0; 3],
                vec![0; 3],
                vec![0; 3],
                Vec::new(),
                Vec::new(),
            ],
        });
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x39,
            offset: 0x08E,
            value: 0x2C84,
            source: BeamWriteSource::Copper,
        });

        let mut fb = vec![0; FB_PIXELS];
        bitplane::render(&mut bus, &mut fb);

        assert_eq!(fb[STANDARD_VISIBLE_X0 + 4], rgb12_to_rgba8(0x0000));
        assert_eq!(fb[STANDARD_VISIBLE_X0 + 6], rgb12_to_rgba8(0x0F00));
    }

    #[test]
    fn beam_timed_diwstrt_extends_later_bitplane_pixels_left_on_same_line() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.denise.diwstrt = 0x2CA1;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0048;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.palette.write_ocs(0, 0x0000);
        bus.denise.palette.write_ocs(1, 0x0F00);
        bus.current_frame_render_base = bus.capture_render_snapshot();
        bus.current_frame_bitplane_rows[0] = Some(CapturedBitplaneRow {
            nplanes: 1,
            words_per_row: 3,
            planes: [
                vec![0xFFFF, 0xFFFF, 0xFFFF],
                vec![0; 3],
                vec![0; 3],
                vec![0; 3],
                vec![0; 3],
                vec![0; 3],
                Vec::new(),
                Vec::new(),
            ],
        });
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x40,
            offset: 0x08E,
            value: 0x2C83,
            source: BeamWriteSource::Copper,
        });

        let mut fb = vec![0; FB_PIXELS];
        bitplane::render(&mut bus, &mut fb);

        assert_eq!(fb[94], rgb12_to_rgba8(0x0000));
        assert_eq!(fb[96], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[132], rgb12_to_rgba8(0x0F00));
    }

    #[test]
    fn beam_timed_diwstrt_can_enable_current_bitplane_line() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.denise.diwstrt = 0x2D93;
        bus.denise.diwstop = 0x2EC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0048;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.palette.write_ocs(0, 0x0000);
        bus.denise.palette.write_ocs(1, 0x0F00);
        bus.current_frame_render_base = bus.capture_render_snapshot();
        bus.current_frame_bitplane_rows[0] = Some(CapturedBitplaneRow {
            nplanes: 1,
            words_per_row: 3,
            planes: [
                vec![0xFFFF, 0xFFFF, 0xFFFF],
                vec![0; 3],
                vec![0; 3],
                vec![0; 3],
                vec![0; 3],
                vec![0; 3],
                Vec::new(),
                Vec::new(),
            ],
        });
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x3C,
            offset: 0x08E,
            value: 0x2C93,
            source: BeamWriteSource::Copper,
        });

        let mut fb = vec![0; FB_PIXELS];
        bitplane::render(&mut bus, &mut fb);

        assert_eq!(fb[STANDARD_VISIBLE_X0 + 34], rgb12_to_rgba8(0x0000));
        assert_eq!(fb[STANDARD_VISIBLE_X0 + 36], rgb12_to_rgba8(0x0F00));
    }

    #[test]
    fn beam_timed_diwstop_can_enable_current_bitplane_line() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.denise.diwstrt = 0x2C93;
        bus.denise.diwstop = 0x2C93;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0048;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.palette.write_ocs(0, 0x0000);
        bus.denise.palette.write_ocs(1, 0x0F00);
        bus.current_frame_render_base = bus.capture_render_snapshot();
        bus.current_frame_bitplane_rows[0] = Some(CapturedBitplaneRow {
            nplanes: 1,
            words_per_row: 3,
            planes: [
                vec![0xFFFF, 0xFFFF, 0xFFFF],
                vec![0; 3],
                vec![0; 3],
                vec![0; 3],
                vec![0; 3],
                vec![0; 3],
                Vec::new(),
                Vec::new(),
            ],
        });
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x3C,
            offset: 0x090,
            value: 0x2DC1,
            source: BeamWriteSource::Copper,
        });

        let mut fb = vec![0; FB_PIXELS];
        bitplane::render(&mut bus, &mut fb);

        assert_eq!(fb[STANDARD_VISIBLE_X0 + 34], rgb12_to_rgba8(0x0000));
        assert_eq!(fb[STANDARD_VISIBLE_X0 + 36], rgb12_to_rgba8(0x0F00));
    }

    #[test]
    fn beam_timed_diwstop_extends_later_bitplane_pixels_on_same_line() {
        let mut bus = empty_bus();
        bus.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.denise.diwstrt = 0x2C83;
        bus.denise.diwstop = 0x2D93;
        bus.denise.diwhigh = 0x0100;
        bus.denise.diwhigh_written = true;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0048;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.palette.write_ocs(0, 0x0000);
        bus.denise.palette.write_ocs(1, 0x0F00);
        bus.current_frame_render_base = bus.capture_render_snapshot();
        bus.current_frame_bitplane_rows[0] = Some(CapturedBitplaneRow {
            nplanes: 1,
            words_per_row: 3,
            planes: [
                vec![0xFFFF, 0xFFFF, 0xFFFF],
                vec![0; 3],
                vec![0; 3],
                vec![0; 3],
                vec![0; 3],
                vec![0; 3],
                Vec::new(),
                Vec::new(),
            ],
        });
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x3C,
            offset: 0x090,
            value: 0x2DC1,
            source: BeamWriteSource::Copper,
        });

        let mut fb = vec![0; FB_PIXELS];
        bitplane::render(&mut bus, &mut fb);

        assert_eq!(fb[STANDARD_VISIBLE_X0 + 30], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[STANDARD_VISIBLE_X0 + 64], rgb12_to_rgba8(0x0F00));
    }

    #[test]
    fn beam_timed_bitplane_pointer_changes_later_fallback_fetch_rows() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2EC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0038;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.palette.write_ocs(0, 0x0000);
        bus.denise.palette.write_ocs(1, 0x0F00);
        bus.denise.bplpt[0] = 0x0100;
        write_chip_word(&mut bus, 0x0100, 0x4000);
        write_chip_word(&mut bus, 0x0102, 0x4000);
        write_chip_word(&mut bus, 0x0200, 0x0000);
        bus.current_frame_render_base = bus.capture_render_snapshot();
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS + 1,
            hpos: 0x38,
            offset: 0x0E2,
            value: 0x0200,
            source: BeamWriteSource::Copper,
        });

        let mut fb = vec![0; FB_PIXELS];
        bitplane::render(&mut bus, &mut fb);

        assert_eq!(fb[STANDARD_VISIBLE_X0], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[FB_WIDTH + STANDARD_VISIBLE_X0], rgb12_to_rgba8(0x0000));
    }

    #[test]
    fn beam_timed_bitplane_pointer_changes_later_fallback_fetch_words() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.denise.diwstrt = 0x2CA1;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0050;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.palette.write_ocs(0, 0x0000);
        bus.denise.palette.write_ocs(1, 0x0F00);
        bus.denise.bplpt[0] = 0x0100;
        write_chip_word(&mut bus, 0x0100, 0x0000);
        write_chip_word(&mut bus, 0x0102, 0x0000);
        write_chip_word(&mut bus, 0x0104, 0x4000);
        write_chip_word(&mut bus, 0x0106, 0x4000);
        write_chip_word(&mut bus, 0x0200, 0x0000);
        bus.current_frame_render_base = bus.capture_render_snapshot();
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x50,
            offset: 0x0E2,
            value: 0x0200,
            source: BeamWriteSource::Copper,
        });

        let mut fb = vec![0; FB_PIXELS];
        bitplane::render(&mut bus, &mut fb);

        let x_start = STANDARD_VISIBLE_X0 + 64;
        assert_eq!(fb[x_start], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[x_start + 32], rgb12_to_rgba8(0x0000));
    }

    #[test]
    fn bplmod_write_before_last_lowres_fetch_slot_advances_next_line_pointer() {
        let pointer_after_mod_write = |write_hpos| {
            let mut bus = empty_bus();
            bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
            bus.agnus.vpos = RENDER_VISIBLE_START_VPOS;
            bus.agnus.hpos = 0x3E;
            bus.denise.diwstrt = 0x2C83;
            bus.denise.diwstop = 0x2DC1;
            bus.denise.ddfstrt = 0x0038;
            bus.denise.ddfstop = 0x0040;
            bus.denise.bplcon0 = 0x1000;
            bus.denise.bplpt[0] = 0x0100;
            bus.display_dma_bplpt[0] = 0x0100;

            bus.advance_chipset(write_hpos - bus.agnus.hpos);
            bus.write_custom_word_from(0x108, 0x0004, BeamWriteSource::Copper);
            bus.advance_chipset(0x48 - bus.agnus.hpos);
            bus.display_dma_bplpt[0]
        };

        assert_eq!(pointer_after_mod_write(0x46), 0x0108);
        assert_eq!(pointer_after_mod_write(0x48), 0x0104);
    }

    #[test]
    fn manual_bitplane_data_respects_display_window_clip() {
        let mut bus = empty_bus();
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2DD1;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.palette.write_ocs(0, 0x0000);
        bus.denise.palette.write_ocs(1, 0x0F00);
        bus.current_frame_render_base = bus.capture_render_snapshot();
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: RENDER_COPPER_WAIT_HPOS_FB0,
            offset: 0x0110,
            value: 0xFFFF,
            source: BeamWriteSource::Copper,
        });
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: RENDER_COPPER_WAIT_HPOS_FB0 + 16,
            offset: 0x0110,
            value: 0x8000,
            source: BeamWriteSource::Copper,
        });

        let mut fb = vec![0; FB_PIXELS];
        bitplane::render(&mut bus, &mut fb);

        assert_eq!(fb[0], rgb12_to_rgba8(0x0000));
        assert_eq!(fb[31], rgb12_to_rgba8(0x0000));
        assert_eq!(fb[64], rgb12_to_rgba8(0x0F00));
    }

    #[test]
    fn bpl1dat_write_triggers_output_while_bitplane_dma_enabled() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0040;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.palette.write_ocs(0, 0x0000);
        bus.denise.palette.write_ocs(1, 0x0F00);
        bus.current_frame_render_base = bus.capture_render_snapshot();
        bus.current_frame_bitplane_rows[0] = Some(CapturedBitplaneRow {
            nplanes: 1,
            words_per_row: 2,
            planes: [
                vec![0, 0],
                vec![0, 0],
                vec![0, 0],
                vec![0, 0],
                vec![0, 0],
                vec![0, 0],
                Vec::new(),
                Vec::new(),
            ],
        });
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x40,
            offset: 0x0110,
            value: 0x8000,
            source: BeamWriteSource::Copper,
        });

        let mut fb = vec![0; FB_PIXELS];
        bitplane::render(&mut bus, &mut fb);

        assert_eq!(fb[94], rgb12_to_rgba8(0x0000));
        assert_eq!(fb[96], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[97], rgb12_to_rgba8(0x0F00));
    }

    #[test]
    fn previsible_bpl1dat_write_does_not_draw_first_visible_line() {
        let mut bus = empty_bus();
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.palette.write_ocs(0, 0x0000);
        bus.denise.palette.write_ocs(1, 0x0F00);
        bus.current_frame_render_base = bus.capture_render_snapshot();
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS - 1,
            hpos: 0x40,
            offset: 0x0110,
            value: 0x8000,
            source: BeamWriteSource::Copper,
        });

        let mut fb = vec![0; FB_PIXELS];
        bitplane::render(&mut bus, &mut fb);

        assert_eq!(fb[32], rgb12_to_rgba8(0x0000));
    }

    #[test]
    fn same_line_diwstrt_extension_clips_later_manual_bitplane_pixels() {
        let mut bus = empty_bus();
        bus.denise.diwstrt = 0x2C93;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.palette.write_ocs(0, 0x0000);
        bus.denise.palette.write_ocs(1, 0x0F00);
        bus.current_frame_render_base = bus.capture_render_snapshot();
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x38,
            offset: 0x0110,
            value: 0x00FF,
            source: BeamWriteSource::Copper,
        });
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x3C,
            offset: 0x008E,
            value: 0x2C83,
            source: BeamWriteSource::Copper,
        });

        let mut fb = vec![0; FB_PIXELS];
        bitplane::render(&mut bus, &mut fb);

        assert_eq!(fb[68], rgb12_to_rgba8(0x0000));
        assert_eq!(fb[79], rgb12_to_rgba8(0x0000));
        assert_eq!(fb[80], rgb12_to_rgba8(0x0F00));
    }

    #[test]
    fn same_line_palette_write_colors_later_manual_bitplane_pixels() {
        let mut bus = empty_bus();
        bus.denise.diwstrt = 0x2C83;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.palette.write_ocs(0, 0x0000);
        bus.denise.palette.write_ocs(1, 0x0F00);
        bus.current_frame_render_base = bus.capture_render_snapshot();
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x38,
            offset: 0x0110,
            value: 0xFFFF,
            source: BeamWriteSource::Copper,
        });
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x4A,
            offset: 0x0182,
            value: 0x00F0,
            source: BeamWriteSource::Copper,
        });

        let mut fb = vec![0; FB_PIXELS];
        bitplane::render(&mut bus, &mut fb);

        let event_x = render_color_write_x(0x4A);
        assert_eq!(fb[event_x - 1], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[event_x], rgb12_to_rgba8(0x00F0));
    }

    #[test]
    fn same_line_bplcon0_plane_count_clips_later_manual_bitplane_pixels() {
        let mut bus = empty_bus();
        bus.denise.diwstrt = 0x2C83;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.palette.write_ocs(0, 0x0000);
        bus.denise.palette.write_ocs(1, 0x0F00);
        bus.current_frame_render_base = bus.capture_render_snapshot();
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x38,
            offset: 0x0110,
            value: 0xFFFF,
            source: BeamWriteSource::Copper,
        });
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x3C,
            offset: 0x0100,
            value: 0x0000,
            source: BeamWriteSource::Copper,
        });

        let mut fb = vec![0; FB_PIXELS];
        bitplane::render(&mut bus, &mut fb);

        assert_eq!(fb[79], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[80], rgb12_to_rgba8(0x0000));
    }

    #[test]
    fn same_line_bplcon0_hires_changes_later_manual_bitplane_pixel_repeat() {
        let mut bus = empty_bus();
        bus.denise.diwstrt = 0x2C83;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.palette.write_ocs(0, 0x0000);
        bus.denise.palette.write_ocs(1, 0x0F00);
        bus.current_frame_render_base = bus.capture_render_snapshot();
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x38,
            offset: 0x0110,
            value: 0x0080,
            source: BeamWriteSource::Copper,
        });
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x3C,
            offset: 0x0100,
            value: 0x9000,
            source: BeamWriteSource::Copper,
        });

        let mut fb = vec![0; FB_PIXELS];
        bitplane::render(&mut bus, &mut fb);

        assert_eq!(fb[79], rgb12_to_rgba8(0x0000));
        assert_eq!(fb[80], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[81], rgb12_to_rgba8(0x0000));
    }

    #[test]
    fn manual_ham_bitplane_words_carry_previous_pixel_color() {
        let mut bus = empty_bus();
        bus.denise.diwstrt = 0x2C83;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.bplcon0 = 0x6800;
        bus.denise.palette.write_ocs(0, 0x0000);
        bus.denise.palette.write_ocs(1, 0x0123);
        bus.current_frame_render_base = bus.capture_render_snapshot();
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x38,
            offset: 0x0110,
            value: 0xFFFF,
            source: BeamWriteSource::Copper,
        });
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x3B,
            offset: 0x0114,
            value: 0xFFFF,
            source: BeamWriteSource::Copper,
        });
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x3B,
            offset: 0x011A,
            value: 0xFFFF,
            source: BeamWriteSource::Copper,
        });
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x3C,
            offset: 0x0110,
            value: 0xFFFF,
            source: BeamWriteSource::Copper,
        });

        let mut fb = vec![0; FB_PIXELS];
        bitplane::render(&mut bus, &mut fb);

        assert_eq!(fb[79], rgb12_to_rgba8(0x0123));
        assert_eq!(fb[80], rgb12_to_rgba8(0x0123));
        assert_eq!(fb[82], rgb12_to_rgba8(0x0523));
    }

    #[test]
    fn same_line_ham_enable_does_not_retime_earlier_playfield_color() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.denise.diwstrt = 0x2C83;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0038;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.palette.write_ocs(0, 0x0000);
        bus.denise.palette.write_ocs(1, 0x0123);
        bus.current_frame_render_base = bus.capture_render_snapshot();
        bus.current_frame_bitplane_rows[0] = Some(CapturedBitplaneRow {
            nplanes: 6,
            words_per_row: 1,
            planes: [
                vec![0xC000],
                vec![0x0000],
                vec![0x2000],
                vec![0x0000],
                vec![0x2000],
                vec![0x0000],
                Vec::new(),
                Vec::new(),
            ],
        });
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x39,
            offset: 0x0100,
            value: 0x6800,
            source: BeamWriteSource::Copper,
        });

        let mut fb = vec![0; FB_PIXELS];
        bitplane::render(&mut bus, &mut fb);

        assert_eq!(fb[2], rgb12_to_rgba8(0x0000));
        assert_eq!(fb[4], rgb12_to_rgba8(0x0000));
    }

    #[test]
    fn same_line_ham_enable_modifies_previous_manual_bitplane_color() {
        let mut bus = empty_bus();
        bus.denise.diwstrt = 0x2C83;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.palette.write_ocs(0, 0x0000);
        bus.denise.palette.write_ocs(1, 0x0123);
        bus.current_frame_render_base = bus.capture_render_snapshot();
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x38,
            offset: 0x0110,
            value: 0xFFFF,
            source: BeamWriteSource::Copper,
        });
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x40,
            offset: 0x0100,
            value: 0x6800,
            source: BeamWriteSource::Copper,
        });
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x40,
            offset: 0x0114,
            value: 0xFFFF,
            source: BeamWriteSource::Copper,
        });
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x40,
            offset: 0x0118,
            value: 0xFFFF,
            source: BeamWriteSource::Copper,
        });
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x40,
            offset: 0x0110,
            value: 0x0000,
            source: BeamWriteSource::Copper,
        });

        let mut fb = vec![0; FB_PIXELS];
        bitplane::render(&mut bus, &mut fb);

        assert_eq!(fb[95], rgb12_to_rgba8(0x0123));
        assert_eq!(fb[96], rgb12_to_rgba8(0x0123));
        assert_eq!(fb[98], rgb12_to_rgba8(0x0124));
    }

    #[test]
    fn beam_timed_ddfstop_shrink_blanks_later_fallback_fetch_words_on_same_line() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.denise.diwstrt = 0x2C83;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0040;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.palette.write_ocs(0, 0x0000);
        bus.denise.palette.write_ocs(1, 0x0F00);
        bus.denise.bplpt[0] = 0x0100;
        write_chip_word(&mut bus, 0x0100, 0x0000);
        write_chip_word(&mut bus, 0x0102, 0x8000);
        bus.current_frame_render_base = bus.capture_render_snapshot();
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x40,
            offset: 0x094,
            value: 0x0038,
            source: BeamWriteSource::Copper,
        });

        let mut fb = vec![0; FB_PIXELS];
        bitplane::render(&mut bus, &mut fb);

        assert_eq!(fb[0], rgb12_to_rgba8(0x0000));
        assert_eq!(fb[32], rgb12_to_rgba8(0x0000));
    }

    #[test]
    fn beam_timed_bplcon1_scroll_changes_later_pixels_on_same_line() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0038;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.bplcon1 = 0;
        bus.denise.palette.write_ocs(0, 0x0000);
        bus.denise.palette.write_ocs(1, 0x0F00);
        bus.current_frame_render_base = bus.capture_render_snapshot();
        bus.current_frame_bitplane_rows[0] = Some(captured_row(1, 21, &[&[(0, 0x5000)]]));
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x39,
            offset: 0x0102,
            value: 0x0004,
            source: BeamWriteSource::Copper,
        });

        let mut fb = vec![0; FB_PIXELS];
        bitplane::render(&mut bus, &mut fb);

        // The mid-line BPLCON1 write (horizontal scroll = 4) shifts this
        // word's lit pixels right by four lores pixels (eight framebuffer
        // columns): 0xA000's set pixels land at columns 40 and 44 instead
        // of the unscrolled origin at 32 and 36.
        let red = rgb12_to_rgba8(0x0F00);
        let black = rgb12_to_rgba8(0x0000);
        assert_eq!(fb[72], red);
        assert_eq!(fb[76], red);
        assert_eq!(fb[64], black);
        assert_eq!(fb[68], black);
    }

    #[test]
    fn beam_timed_bplcon1_scroll_decrease_reveals_later_pixels_on_same_line() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0038;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.bplcon1 = 4;
        bus.denise.palette.write_ocs(0, 0x0000);
        bus.denise.palette.write_ocs(1, 0x0F00);
        bus.current_frame_render_base = bus.capture_render_snapshot();
        bus.current_frame_bitplane_rows[0] = Some(captured_row(1, 21, &[&[(0, 0x1000)]]));
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x39,
            offset: 0x0102,
            value: 0x0000,
            source: BeamWriteSource::Copper,
        });

        let mut fb = vec![0; FB_PIXELS];
        bitplane::render(&mut bus, &mut fb);

        // The mid-line BPLCON1 write drops the scroll from 4 back to 0,
        // so the lit lores pixel 2 is revealed at the unscrolled origin
        // (columns 36/37) instead of the scrolled-by-4 position (44/45).
        let red = rgb12_to_rgba8(0x0F00);
        let black = rgb12_to_rgba8(0x0000);
        assert_eq!(fb[68], red);
        assert_eq!(fb[69], red);
        assert_eq!(fb[76], black);
        assert_eq!(fb[64], black);
    }

    #[test]
    fn same_line_bplcon2_killehb_changes_later_extra_half_brite_pixels() {
        let mut bus = empty_bus();
        bus.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.denise.diwstrt = 0x2C83;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0038;
        bus.denise.bplcon0 = 0x6000;
        bus.denise.bplcon2 = 0;
        bus.denise.palette.write_ocs(1, 0x0E00);
        bus.current_frame_render_base = bus.capture_render_snapshot();
        bus.current_frame_bitplane_rows[0] = Some(CapturedBitplaneRow {
            nplanes: 6,
            words_per_row: 1,
            planes: [
                vec![0xFFFF],
                vec![0],
                vec![0],
                vec![0],
                vec![0],
                vec![0xFFFF],
                Vec::new(),
                Vec::new(),
            ],
        });
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x3C,
            offset: 0x0104,
            value: 0x0200,
            source: BeamWriteSource::Copper,
        });

        let mut fb = vec![0; FB_PIXELS];
        bitplane::render(&mut bus, &mut fb);

        assert_eq!(fb[68], rgb12_to_rgba8(0x0700));
        assert_eq!(fb[80], rgb12_to_rgba8(0x0E00));
    }

    #[test]
    fn beam_timed_bplcon0_hires_narrows_later_bitplane_pixels() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.denise.diwstrt = 0x2C83;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x003C;
        bus.denise.ddfstop = 0x0044;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.palette.write_ocs(0, 0x0000);
        bus.denise.palette.write_ocs(1, 0x0F00);
        bus.current_frame_render_base = bus.capture_render_snapshot();
        bus.current_frame_bitplane_rows[0] = Some(CapturedBitplaneRow {
            nplanes: 1,
            words_per_row: 4,
            planes: [
                vec![0x0000, 0x0000, 0x2000, 0x0000],
                vec![0; 4],
                vec![0; 4],
                vec![0; 4],
                vec![0; 4],
                vec![0; 4],
                Vec::new(),
                Vec::new(),
            ],
        });
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x40,
            offset: 0x0100,
            value: 0x9000,
            source: BeamWriteSource::Copper,
        });

        let mut fb = vec![0; FB_PIXELS];
        bitplane::render(&mut bus, &mut fb);

        assert_eq!(fb[STANDARD_VISIBLE_X0 + 36], rgb12_to_rgba8(0x0000));
        assert_eq!(fb[STANDARD_VISIBLE_X0 + 38], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[STANDARD_VISIBLE_X0 + 39], rgb12_to_rgba8(0x0000));
    }

    #[test]
    fn beam_timed_bplcon0_lowres_widens_later_bitplane_pixels() {
        let mut bus = empty_bus();
        bus.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.denise.diwstrt = 0x2C83;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x003C;
        bus.denise.ddfstop = 0x003C;
        bus.denise.bplcon0 = 0x9000;
        bus.denise.palette.write_ocs(0, 0x0000);
        bus.denise.palette.write_ocs(1, 0x0F00);
        bus.current_frame_render_base = bus.capture_render_snapshot();
        bus.current_frame_bitplane_rows[0] = Some(CapturedBitplaneRow {
            nplanes: 1,
            words_per_row: 2,
            planes: [
                vec![0x0000, 0x4000],
                vec![0; 2],
                vec![0; 2],
                vec![0; 2],
                vec![0; 2],
                vec![0; 2],
                Vec::new(),
                Vec::new(),
            ],
        });
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x40,
            offset: 0x0100,
            value: 0x1000,
            source: BeamWriteSource::Copper,
        });

        let mut fb = vec![0; FB_PIXELS];
        bitplane::render(&mut bus, &mut fb);

        assert_eq!(fb[STANDARD_VISIBLE_X0 + 30], rgb12_to_rgba8(0x0000));
        assert_eq!(fb[STANDARD_VISIBLE_X0 + 32], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[STANDARD_VISIBLE_X0 + 33], rgb12_to_rgba8(0x0F00));
    }

    #[test]
    fn same_line_bplcon2_priority_change_reveals_later_sprite_pixels() {
        let mut bus = empty_bus();
        bus.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN | DMACON_SPREN;
        bus.denise.diwstrt = 0x2C83;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0038;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.bplcon2 = 0;
        bus.denise.palette.write_ocs(1, 0x0F00);
        bus.denise.palette.write_ocs(17, 0x00F0);
        bus.current_frame_render_base = bus.capture_render_snapshot();
        bus.current_frame_bitplane_rows[0] = Some(CapturedBitplaneRow {
            nplanes: 1,
            words_per_row: 1,
            planes: [
                vec![0xFFFF],
                vec![0],
                vec![0],
                vec![0],
                vec![0],
                vec![0],
                Vec::new(),
                Vec::new(),
            ],
        });
        bus.current_frame_sprite_dma_observed = true;
        bus.current_frame_sprite_display_enable_x_by_y[0] = Some(0);
        bus.current_frame_sprite_lines.push(CapturedSpriteLine {
            sprite: 0,
            hstart: RENDER_DIW_HSTART_FB0 + 34,
            hsub_70ns: false,
            beam_y: RENDER_VISIBLE_START_VPOS as i32,
            data: 0xFFFF,
            datb: 0,
            attached: false,
            data_ext: [0; 3],
            datb_ext: [0; 3],
            width_words: 1,
        });
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x3C,
            offset: 0x0104,
            value: 0x0008,
            source: BeamWriteSource::Copper,
        });

        let mut fb = vec![0; FB_PIXELS];
        bitplane::render(&mut bus, &mut fb);

        assert_eq!(fb[68], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[80], rgb12_to_rgba8(0x00F0));
    }

    #[test]
    fn same_line_bplcon3_spres_narrows_later_sprite_pixels() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_SPREN;
        bus.denise.diwstrt = 0x2C83;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.bplcon0 = 0;
        bus.denise.bplcon3 = 0;
        bus.denise.palette.write_ocs(17, 0x0F00);
        bus.current_frame_render_base = bus.capture_render_snapshot();
        bus.current_frame_sprite_dma_observed = true;
        bus.current_frame_sprite_lines.push(CapturedSpriteLine {
            sprite: 0,
            hstart: RENDER_DIW_HSTART_FB0 + 34,
            hsub_70ns: false,
            beam_y: RENDER_VISIBLE_START_VPOS as i32,
            data: 0x0100,
            datb: 0,
            attached: false,
            data_ext: [0; 3],
            datb_ext: [0; 3],
            width_words: 1,
        });
        bus.current_frame_sprite_display_enable_x_by_y[0] = Some(0);
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x3C,
            offset: 0x0106,
            value: BPLCON3_SPRES_HIRES,
            source: BeamWriteSource::Copper,
        });

        let mut fb = vec![0; FB_PIXELS];
        bitplane::render(&mut bus, &mut fb);

        assert_eq!(fb[81], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[82], rgb12_to_rgba8(0x0000));
    }

    #[test]
    fn same_line_bplcon3_pf2of_changes_later_dual_playfield_pixels() {
        let mut bus = empty_bus();
        bus.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.denise.diwstrt = 0x2C83;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0038;
        bus.denise.bplcon0 = 0x2400;
        bus.denise.bplcon3 = 0;
        bus.denise.palette.write_ocs(1, 0x0F00);
        bus.denise.palette.write_ocs(17, 0x00F0);
        bus.current_frame_render_base = bus.capture_render_snapshot();
        bus.current_frame_bitplane_rows[0] = Some(CapturedBitplaneRow {
            nplanes: 2,
            words_per_row: 1,
            planes: [
                vec![0],
                vec![0xFFFF],
                vec![0],
                vec![0],
                vec![0],
                vec![0],
                Vec::new(),
                Vec::new(),
            ],
        });
        bus.current_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x3C,
            offset: 0x0106,
            value: 0x1000,
            source: BeamWriteSource::Copper,
        });

        let mut fb = vec![0; FB_PIXELS];
        bitplane::render(&mut bus, &mut fb);

        assert_eq!(fb[68], rgb12_to_rgba8(0x0F00));
        assert_eq!(fb[80], rgb12_to_rgba8(0x00F0));
    }

    #[test]
    fn lowres_fallback_fetches_ignore_chip_ram_writes_after_plane_slot() {
        let mut bus = empty_bus();
        let mut snapshot = RenderRegisterSnapshot {
            dmacon: DMACON_DMAEN | DMACON_BPLEN,
            bplcon0: 0x2000,
            diwstrt: 0x2C83,
            diwstop: 0x2DC1,
            ddfstrt: 0x0038,
            ddfstop: 0x0038,
            ..RenderRegisterSnapshot::default()
        };
        snapshot.palette.write_ocs(0, 0x0000);
        snapshot.palette.write_ocs(1, 0x0F00);
        snapshot.bplpt[0] = 0x0100;
        snapshot.bplpt[1] = 0x0200;
        bus.last_frame_render_base = Some(snapshot);
        bus.last_frame_chip_ram = vec![0; bus.mem.chip_ram.len()];
        bus.last_frame_chip_ram_writes
            .push(BeamChipRamWrite::from_bytes(
                RENDER_VISIBLE_START_VPOS,
                0x40,
                0x0100,
                &[0x80, 0x00],
            ));

        let mut fb = vec![0; FB_PIXELS];
        bitplane::render(&mut bus, &mut fb);

        assert_eq!(fb[0], rgb12_to_rgba8(0x0000));
    }

    #[test]
    fn lowres_fallback_fetches_ignore_pointer_writes_after_plane_slot() {
        let mut bus = empty_bus();
        let mut snapshot = RenderRegisterSnapshot {
            dmacon: DMACON_DMAEN | DMACON_BPLEN,
            bplcon0: 0x2000,
            diwstrt: 0x2C83,
            diwstop: 0x2DC1,
            ddfstrt: 0x0038,
            ddfstop: 0x0038,
            ..RenderRegisterSnapshot::default()
        };
        snapshot.palette.write_ocs(0, 0x0000);
        snapshot.palette.write_ocs(1, 0x0F00);
        snapshot.bplpt[0] = 0x0100;
        snapshot.bplpt[1] = 0x0200;
        bus.last_frame_render_base = Some(snapshot);
        bus.last_frame_chip_ram = vec![0; bus.mem.chip_ram.len()];
        bus.last_frame_chip_ram[0x0300] = 0x80;
        bus.last_frame_render_events.push(BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: 0x40,
            offset: 0x0E2,
            value: 0x0300,
            source: BeamWriteSource::Copper,
        });

        let mut fb = vec![0; FB_PIXELS];
        bitplane::render(&mut bus, &mut fb);

        assert_eq!(fb[0], rgb12_to_rgba8(0x0000));
    }

    #[test]
    fn sprite_dma_capture_samples_line_words_at_beam_time() {
        let mut bus = empty_bus();
        let sprite_ptr = 0x0100usize;
        let (pos, ctl) = sprite_control_words(0x2C, 0x2D, 0x0083);
        write_chip_word(&mut bus, sprite_ptr, pos);
        write_chip_word(&mut bus, sprite_ptr + 2, ctl);
        write_chip_word(&mut bus, sprite_ptr + 4, 0x1111);
        write_chip_word(&mut bus, sprite_ptr + 6, 0x2222);
        write_chip_word(&mut bus, sprite_ptr + 8, 0);
        write_chip_word(&mut bus, sprite_ptr + 10, 0);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_SPREN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
        bus.denise.sprpt[0] = sprite_ptr as u32;
        bus.display_dma_sprpt[0] = sprite_ptr as u32;

        bus.advance_chipset(1);
        write_chip_word(&mut bus, sprite_ptr + 4, 0xAAAA);
        write_chip_word(&mut bus, sprite_ptr + 6, 0xBBBB);
        bus.advance_chipset(1);
        write_chip_word(&mut bus, sprite_ptr + 4, 0xCCCC);
        write_chip_word(&mut bus, sprite_ptr + 6, 0xDDDD);

        let lines = bus.frame_captured_sprite_lines();
        assert!(bus.frame_sprite_dma_observed());
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].sprite, 0);
        assert_eq!(lines[0].beam_y, 0x2C);
        assert_eq!(lines[0].hstart, 0x0083);
        assert_eq!(lines[0].data, 0xAAAA);
        assert_eq!(lines[0].datb, 0xBBBB);
    }

    #[test]
    fn sprite_pointer_write_before_vstart_retargets_latched_control_data_fetch() {
        let mut bus = empty_bus();
        let control_ptr = 0x0100usize;
        let data_ptr = 0x0200usize;
        let (pos, ctl) = sprite_control_words(0x2C, 0x30, 0x0083);
        write_chip_word(&mut bus, control_ptr, pos);
        write_chip_word(&mut bus, control_ptr + 2, ctl);
        write_chip_word(&mut bus, control_ptr + 4, 0x1111);
        write_chip_word(&mut bus, control_ptr + 6, 0x2222);
        write_chip_word(&mut bus, data_ptr, 0xAAAA);
        write_chip_word(&mut bus, data_ptr + 2, 0xBBBB);
        write_chip_word(&mut bus, data_ptr + 4, 0xCCCC);
        write_chip_word(&mut bus, data_ptr + 6, 0xDDDD);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_SPREN;
        bus.denise.sprpt[0] = control_ptr as u32;
        bus.display_dma_sprpt[0] = control_ptr as u32;
        bus.current_frame_render_base.dmacon = DMACON_DMAEN | DMACON_SPREN;
        bus.current_frame_render_base.sprpt[0] = control_ptr as u32;

        bus.agnus.vpos = 0x24;
        bus.agnus.hpos = 0;
        let _ = bus.write_custom_word_from(0x120, (data_ptr >> 16) as u16, BeamWriteSource::Copper);
        let _ = bus.write_custom_word_from(0x122, data_ptr as u16, BeamWriteSource::Copper);

        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = 0;
        bus.capture_current_frame_display_start();
        bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
        bus.advance_chipset(2);

        let lines = bus.frame_captured_sprite_lines();
        assert!(bus.frame_sprite_dma_observed());
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].sprite, 0);
        assert_eq!(lines[0].beam_y, 0x2C);
        assert_eq!(lines[0].hstart, 0x0083);
        assert_eq!(lines[0].data, 0xAAAA);
        assert_eq!(lines[0].datb, 0xBBBB);
    }

    #[test]
    fn manual_sprite_control_write_fetches_data_from_sprpt() {
        let mut bus = empty_bus();
        let data_ptr = 0x0200usize;
        let (pos, ctl) = sprite_control_words(0x2C, 0x2E, 0x0083);
        write_chip_word(&mut bus, data_ptr, 0xAAAA);
        write_chip_word(&mut bus, data_ptr + 2, 0xBBBB);
        write_chip_word(&mut bus, data_ptr + 4, 0xCCCC);
        write_chip_word(&mut bus, data_ptr + 6, 0xDDDD);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_SPREN;
        bus.denise.sprpt[0] = data_ptr as u32;
        bus.display_dma_sprpt[0] = data_ptr as u32;

        bus.agnus.vpos = 0x28;
        bus.agnus.hpos = 0;
        assert!(!bus.write_custom_word_from(0x140, pos, BeamWriteSource::Copper));
        assert!(!bus.write_custom_word_from(0x142, ctl, BeamWriteSource::Copper));

        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
        bus.advance_chipset(2);
        bus.agnus.vpos = 0x2D;
        bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
        bus.advance_chipset(2);

        let lines = bus.frame_captured_sprite_lines();
        assert!(bus.frame_sprite_dma_observed());
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].sprite, 0);
        assert_eq!(lines[0].beam_y, 0x2C);
        assert_eq!(lines[0].hstart, 0x0083);
        assert_eq!(lines[0].data, 0xAAAA);
        assert_eq!(lines[0].datb, 0xBBBB);
        assert_eq!(lines[1].beam_y, 0x2D);
        assert_eq!(lines[1].data, 0xCCCC);
        assert_eq!(lines[1].datb, 0xDDDD);
    }

    #[test]
    fn active_sprite_control_rewrite_preserves_descriptor_data_origin() {
        let mut bus = empty_bus();
        let sprite_ptr = 0x0100usize;
        let (pos, ctl) = sprite_control_words(0x2C, 0x30, 0x0083);
        let (moved_pos, moved_ctl) = sprite_control_words(0x2D, 0x30, 0x0091);
        write_chip_word(&mut bus, sprite_ptr, pos);
        write_chip_word(&mut bus, sprite_ptr + 2, ctl);
        write_chip_word(&mut bus, sprite_ptr + 4, 0x1111);
        write_chip_word(&mut bus, sprite_ptr + 6, 0x2222);
        write_chip_word(&mut bus, sprite_ptr + 8, 0x3333);
        write_chip_word(&mut bus, sprite_ptr + 10, 0x4444);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_SPREN;
        bus.denise.sprpt[0] = sprite_ptr as u32;
        bus.display_dma_sprpt[0] = sprite_ptr as u32;

        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
        bus.advance_chipset(2);

        bus.agnus.vpos = 0x2D;
        bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 8;
        assert!(!bus.write_custom_word_from(0x140, moved_pos, BeamWriteSource::Copper));
        assert!(!bus.write_custom_word_from(0x142, moved_ctl, BeamWriteSource::Copper));

        bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
        bus.advance_chipset(2);

        let lines = bus.frame_captured_sprite_lines();
        assert!(bus.frame_sprite_dma_observed());
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].data, 0x1111);
        assert_eq!(lines[0].datb, 0x2222);
        assert_eq!(lines[1].beam_y, 0x2D);
        assert_eq!(lines[1].hstart, 0x0091);
        assert_eq!(lines[1].data, 0x3333);
        assert_eq!(lines[1].datb, 0x4444);
    }

    #[test]
    fn sprite_pointer_write_after_pair_slot_seeds_next_descriptor_fetch() {
        let mut bus = empty_bus();
        bus.set_chipset_revisions(AgnusRevision::AgaAlice, DeniseRevision::AgaLisa);
        bus.agnus.write_fmode(0x000C); // SPR32 | SPAGEM: 64-bit sprite fetches.

        let old_ptr = 0x0100usize;
        let new_ptr = 0x0200usize;
        let (old_pos, old_ctl) = sprite_control_words(0x2C, 0x30, 0x0083);
        let (new_pos, new_ctl) = sprite_control_words(0x2C, 0x30, 0x00C1);
        write_chip_word(&mut bus, old_ptr, old_pos);
        write_chip_word(&mut bus, old_ptr + 8, old_ctl);
        write_chip_word(&mut bus, old_ptr + 16, 0x1111);
        write_chip_word(&mut bus, old_ptr + 24, 0x2222);
        write_chip_word(&mut bus, new_ptr, new_pos);
        write_chip_word(&mut bus, new_ptr + 8, new_ctl);
        for w in 0..4 {
            write_chip_word(&mut bus, new_ptr + 16 + w * 2, 0xA000 + w as u16);
            write_chip_word(&mut bus, new_ptr + 24 + w * 2, 0xB000 + w as u16);
        }

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_SPREN;
        bus.denise.sprpt[2] = old_ptr as u32;
        bus.display_dma_sprpt[2] = old_ptr as u32;

        let slot = SPRITE_DMA_PAIR_CAPTURE_HPOS[1];
        bus.agnus.vpos = 0;
        bus.agnus.hpos = slot - 1;
        bus.advance_chipset(2);
        let _ = bus.write_custom_word_from(0x128, (new_ptr >> 16) as u16, BeamWriteSource::Copper);
        let _ = bus.write_custom_word_from(0x12A, new_ptr as u16, BeamWriteSource::Copper);

        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = slot - 1;
        bus.advance_chipset(2);

        let lines = bus.frame_captured_sprite_lines();
        assert!(bus.frame_sprite_dma_observed());
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].sprite, 2);
        assert_eq!(lines[0].beam_y, 0x2C);
        assert_eq!(lines[0].hstart, 0x00C1);
        assert_eq!(lines[0].data, 0xA000);
        assert_eq!(lines[0].data_ext, [0xA001, 0xA002, 0xA003]);
        assert_eq!(lines[0].datb, 0xB000);
        assert_eq!(lines[0].datb_ext, [0xB001, 0xB002, 0xB003]);
    }

    /// An inverted vertical pair (vstop < vstart) does not disable a sprite:
    /// Agnus arms it at vstart and, since the vstop comparator already passed,
    /// keeps fetching data to the bottom of the frame instead of terminating.
    /// Previously vstop<vstart killed sprites that deliberately reuse the same
    /// fetched strip across the remaining field.
    #[test]
    fn sprite_dma_inverted_vstop_runs_to_frame_bottom() {
        let mut bus = empty_bus();
        let sprite_ptr = 0x0100usize;
        let (pos, ctl) = sprite_control_words(0x2C, 0x20, 0x0083); // vstop < vstart
        write_chip_word(&mut bus, sprite_ptr, pos);
        write_chip_word(&mut bus, sprite_ptr + 2, ctl);
        write_chip_word(&mut bus, sprite_ptr + 4, 0xAAAA);
        write_chip_word(&mut bus, sprite_ptr + 6, 0xBBBB);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_SPREN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
        bus.denise.sprpt[0] = sprite_ptr as u32;
        bus.display_dma_sprpt[0] = sprite_ptr as u32;

        bus.advance_chipset(2);

        let lines = bus.frame_captured_sprite_lines();
        assert!(bus.frame_sprite_dma_observed());
        assert_eq!(lines.len(), 1, "inverted-window sprite must still display");
        assert_eq!(lines[0].beam_y, 0x2C);
        assert_eq!(lines[0].data, 0xAAAA);
        assert_eq!(lines[0].datb, 0xBBBB);
    }

    /// Plan 3.4: FMODE SPR32/SPAGEM widen the sprite fetch. The descriptor
    /// strides scale with the quantum (POS in the first word of the first
    /// wide fetch, CTL in the first word of the second) and each line
    /// carries 2/4 words per channel.
    #[test]
    fn fmode_wide_sprite_dma_captures_extension_words() {
        let mut bus = empty_bus();
        bus.set_chipset_revisions(AgnusRevision::AgaAlice, DeniseRevision::AgaLisa);
        bus.agnus.write_fmode(0x000C); // SPR32 | SPAGEM = 64-bit sprites

        let sprite_ptr = 0x0100usize;
        let (pos, ctl) = sprite_control_words(0x2C, 0x2D, 0x0083);
        // Control fetch pair: POS at +0, CTL at +8 (first word of the
        // second 64-bit fetch); line data starts at +16.
        write_chip_word(&mut bus, sprite_ptr, pos);
        write_chip_word(&mut bus, sprite_ptr + 8, ctl);
        for w in 0..4 {
            write_chip_word(&mut bus, sprite_ptr + 16 + w * 2, 0xA000 + w as u16);
            write_chip_word(&mut bus, sprite_ptr + 24 + w * 2, 0xB000 + w as u16);
        }

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_SPREN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
        bus.denise.sprpt[0] = sprite_ptr as u32;
        bus.display_dma_sprpt[0] = sprite_ptr as u32;

        bus.advance_chipset(2);

        let lines = bus.frame_captured_sprite_lines();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].width_words, 4);
        assert_eq!(lines[0].data, 0xA000);
        assert_eq!(lines[0].data_ext, [0xA001, 0xA002, 0xA003]);
        assert_eq!(lines[0].datb, 0xB000);
        assert_eq!(lines[0].datb_ext, [0xB001, 0xB002, 0xB003]);
    }

    /// FMODE SSCAN2 doubles each fetched sprite data line across two display
    /// lines, and a chained descriptor starts after the halved data block.
    #[test]
    fn fmode_sscan2_sprite_dma_doubles_each_data_line() {
        let mut bus = empty_bus();
        bus.set_chipset_revisions(AgnusRevision::AgaAlice, DeniseRevision::AgaLisa);
        bus.agnus.write_fmode(0x8000); // SSCAN2

        let sprite_ptr = 0x0100usize;
        // First descriptor: 4 display lines backed by 2 data lines.
        let (pos, ctl) = sprite_control_words(0x2C, 0x30, 0x0083);
        write_chip_word(&mut bus, sprite_ptr, pos);
        write_chip_word(&mut bus, sprite_ptr + 2, ctl);
        write_chip_word(&mut bus, sprite_ptr + 4, 0x1111);
        write_chip_word(&mut bus, sprite_ptr + 6, 0x2222);
        write_chip_word(&mut bus, sprite_ptr + 8, 0x3333);
        write_chip_word(&mut bus, sprite_ptr + 10, 0x4444);
        // Chained descriptor immediately after the two data lines.
        let (pos2, ctl2) = sprite_control_words(0x32, 0x34, 0x0091);
        write_chip_word(&mut bus, sprite_ptr + 12, pos2);
        write_chip_word(&mut bus, sprite_ptr + 14, ctl2);
        write_chip_word(&mut bus, sprite_ptr + 16, 0x5555);
        write_chip_word(&mut bus, sprite_ptr + 18, 0x6666);
        write_chip_word(&mut bus, sprite_ptr + 20, 0);
        write_chip_word(&mut bus, sprite_ptr + 22, 0);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_SPREN;
        bus.denise.sprpt[0] = sprite_ptr as u32;
        bus.display_dma_sprpt[0] = sprite_ptr as u32;

        for vpos in 0x2C..=0x32u32 {
            bus.agnus.vpos = vpos;
            bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
            bus.advance_chipset(2);
        }

        let lines = bus.frame_captured_sprite_lines();
        let words: Vec<(i32, u16, u16)> = lines
            .iter()
            .map(|line| (line.beam_y, line.data, line.datb))
            .collect();
        assert_eq!(
            words,
            vec![
                (0x2C, 0x1111, 0x2222),
                (0x2D, 0x1111, 0x2222),
                (0x2E, 0x3333, 0x4444),
                (0x2F, 0x3333, 0x4444),
                (0x32, 0x5555, 0x6666),
            ]
        );
    }

    /// Frame geometry latches at the frame wrap: a standard frame reports
    /// the fixed canvas (FB_HEIGHT rows, 227-cck lines); a VARBEAMEN frame
    /// reports the programmable scan derived from HTOTAL/VTOTAL and the
    /// programmable vertical blank. The renderer-facing accessor describes
    /// the completed frame, so the programmable values appear one wrap
    /// after the registers are programmed.
    #[test]
    fn frame_geometry_latches_programmable_scan_at_frame_wrap() {
        use crate::chipset::agnus::{BEAMCON0_PAL, BEAMCON0_VARBEAMEN, BEAMCON0_VARVBEN};
        use crate::video::FB_HEIGHT;

        let mut bus = empty_bus();
        bus.set_chipset_revisions(AgnusRevision::Ecs8372Rev4, DeniseRevision::Ecs8373);

        let standard = bus.frame_geometry();
        assert!(!standard.programmable);
        assert_eq!(standard.visible_lines, FB_HEIGHT);
        assert_eq!(standard.line_cck, 227);
        assert_eq!(standard.visible_start_vpos, RENDER_VISIBLE_START_VPOS);

        // Programmable 31 kHz scan.
        bus.agnus.write_htotal(113);
        bus.agnus.write_vtotal(625);
        bus.agnus
            .write_beamcon0(BEAMCON0_PAL | BEAMCON0_VARBEAMEN | BEAMCON0_VARVBEN);
        bus.agnus.write_vbstrt(613);
        bus.agnus.write_vbstop(44);

        // The frame in progress when the registers were written still
        // reports standard geometry...
        bus.begin_new_beam_frame();
        assert!(!bus.frame_geometry().programmable);

        // ...and the first frame that starts under the programmable beam
        // reports the programmable scan once it completes.
        bus.begin_new_beam_frame();
        let geometry = bus.frame_geometry();
        assert!(geometry.programmable);
        assert_eq!(geometry.visible_start_vpos, 44);
        assert_eq!(geometry.visible_lines, 569);
        assert_eq!(geometry.line_cck, 114);
    }

    /// FMODE BSCAN2 makes both plane groups share one end-of-line modulo,
    /// selected by line parity relative to DIWSTRT's vertical start.
    #[test]
    fn fmode_bscan2_selects_shared_modulo_by_line_parity() {
        let mut bus = empty_bus();
        bus.set_chipset_revisions(AgnusRevision::AgaAlice, DeniseRevision::AgaLisa);
        bus.denise.bpl1mod = -40;
        bus.denise.bpl2mod = 8;
        bus.denise.diwstrt = 0x2C81;

        bus.agnus.write_fmode(0x4000); // BSCAN2
        assert_eq!(bus.display_dma_modulo_for_plane(0, 0x2C), -40);
        assert_eq!(bus.display_dma_modulo_for_plane(1, 0x2C), -40);
        assert_eq!(bus.display_dma_modulo_for_plane(0, 0x2D), 8);
        assert_eq!(bus.display_dma_modulo_for_plane(1, 0x2D), 8);

        bus.agnus.write_fmode(0);
        assert_eq!(bus.display_dma_modulo_for_plane(0, 0x2D), -40);
        assert_eq!(bus.display_dma_modulo_for_plane(1, 0x2D), 8);
    }

    #[test]
    fn sprite_dma_capture_preserves_sprite_started_before_visible_area() {
        let mut bus = empty_bus();
        let sprite_ptr = 0x0100usize;
        let vstart = RENDER_VISIBLE_START_VPOS as u16 - 2;
        let vstop = RENDER_VISIBLE_START_VPOS as u16 + 1;
        let (pos, ctl) = sprite_control_words(vstart, vstop, 0x0083);
        write_chip_word(&mut bus, sprite_ptr, pos);
        write_chip_word(&mut bus, sprite_ptr + 2, ctl);
        write_chip_word(&mut bus, sprite_ptr + 4, 0x1111);
        write_chip_word(&mut bus, sprite_ptr + 6, 0x2222);
        write_chip_word(&mut bus, sprite_ptr + 8, 0x3333);
        write_chip_word(&mut bus, sprite_ptr + 10, 0x4444);
        write_chip_word(&mut bus, sprite_ptr + 12, 0x5555);
        write_chip_word(&mut bus, sprite_ptr + 14, 0x6666);
        write_chip_word(&mut bus, sprite_ptr + 16, 0);
        write_chip_word(&mut bus, sprite_ptr + 18, 0);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_SPREN;
        // The offscreen sprite-DMA replay seeds from the frame-start DMACON
        // and SPRxPT snapshot (and replays $096/$120..$13F writes across the
        // span); mirror what begin_new_beam_frame records so SPREN and the
        // sprite pointer are live for the offscreen lines this sprite starts on.
        bus.current_frame_render_base.dmacon = DMACON_DMAEN | DMACON_SPREN;
        bus.current_frame_render_base.sprpt[0] = sprite_ptr as u32;
        bus.agnus.vpos = RENDER_VISIBLE_START_VPOS;
        bus.agnus.hpos = 0;
        bus.denise.sprpt[0] = sprite_ptr as u32;
        bus.display_dma_sprpt[0] = sprite_ptr as u32;

        bus.capture_current_frame_display_start();
        bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
        bus.advance_chipset(2);

        let lines = bus.frame_captured_sprite_lines();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].sprite, 0);
        assert_eq!(lines[0].beam_y, RENDER_VISIBLE_START_VPOS as i32);
        assert_eq!(lines[0].hstart, 0x0083);
        assert_eq!(lines[0].data, 0x5555);
        assert_eq!(lines[0].datb, 0x6666);
    }

    #[test]
    fn sprite_dma_capture_treats_zero_pointer_as_chip_address() {
        let mut bus = empty_bus();
        let (pos, ctl) = sprite_control_words(0x2C, 0x2D, 0x0083);
        write_chip_word(&mut bus, 0, pos);
        write_chip_word(&mut bus, 2, ctl);
        write_chip_word(&mut bus, 4, 0x1111);
        write_chip_word(&mut bus, 6, 0x2222);
        write_chip_word(&mut bus, 8, 0);
        write_chip_word(&mut bus, 10, 0);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_SPREN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
        bus.denise.sprpt[0] = 0;
        bus.denise.sprpt[1] = 0x20;
        bus.display_dma_sprpt[0] = 0;
        bus.display_dma_sprpt[1] = 0x20;

        bus.advance_chipset(2);

        let lines = bus.frame_captured_sprite_lines();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].sprite, 0);
        assert_eq!(lines[0].hstart, 0x0083);
        assert_eq!(lines[0].data, 0x1111);
        assert_eq!(lines[0].datb, 0x2222);
    }

    #[test]
    fn state_load_resets_transient_video_latches() {
        let mut bus = empty_bus();
        bus.last_frame_render_base = Some(RenderRegisterSnapshot::default());
        bus.last_frame_render_events.push(BeamRegisterWrite {
            vpos: 0x2C,
            hpos: 0x40,
            offset: 0x180,
            value: 0x0FFF,
            source: BeamWriteSource::Copper,
        });
        bus.last_frame_chip_ram = vec![0xA5; bus.mem.chip_ram.len()];
        bus.last_frame_chip_ram_writes
            .push(BeamChipRamWrite::from_bytes(0x2C, 0x40, 0x0100, &[0x12]));
        bus.current_frame_chip_ram.clear();
        bus.current_frame_chip_ram_writes
            .push(BeamChipRamWrite::from_bytes(0x2C, 0x40, 0x0100, &[0x34]));
        bus.last_frame_bitplane_rows[0] = Some(CapturedBitplaneRow {
            nplanes: 1,
            words_per_row: 1,
            planes: std::array::from_fn(|_| vec![0xFFFF]),
        });
        bus.current_frame_sprite_lines.push(CapturedSpriteLine {
            sprite: 0,
            hstart: 0x80,
            hsub_70ns: false,
            beam_y: 0x2C,
            data: 0x1111,
            datb: 0x2222,
            data_ext: [0; 3],
            datb_ext: [0; 3],
            width_words: 1,
            attached: false,
        });
        bus.display_dma_sprite_state[0] = DisplaySpriteDmaState {
            control: Some(DisplaySpriteControl {
                vstart: 0x20,
                vstop: 0x40,
                hstart: 0x80,
                hsub_70ns: false,
                data_vstart: 0x20,
                data_base: 0x0100,
                next_ptr: 0x0200,
                attached: false,
            }),
            next_ptr: Some(0x0200),
            terminated: false,
            data_dma_active: true,
            last_line: Some(DisplaySpriteLineData {
                hstart: 0x80,
                hsub_70ns: false,
                data: 0x1111,
                datb: 0x2222,
                data_ext: [0; 3],
                datb_ext: [0; 3],
                width_words: 1,
                attached: false,
            }),
        };
        bus.display_dma_sprite_state[3].terminated = true;

        bus.reset_transient_video_after_state_load();

        assert!(bus.last_frame_render_base.is_none());
        assert!(bus.last_frame_render_events.is_empty());
        assert_eq!(bus.current_frame_chip_ram, bus.mem.chip_ram);
        assert!(bus.last_frame_chip_ram.is_empty());
        assert!(bus.current_frame_chip_ram_writes.is_empty());
        assert!(bus.last_frame_chip_ram_writes.is_empty());
        assert!(bus.current_frame_bitplane_rows.iter().all(Option::is_none));
        assert!(bus.last_frame_bitplane_rows.iter().all(Option::is_none));
        assert!(bus.current_frame_sprite_lines.is_empty());
        assert!(bus.last_frame_sprite_lines.is_empty());
        for state in bus.display_dma_sprite_state {
            assert!(state.control.is_none());
            assert!(state.next_ptr.is_none());
            assert!(!state.terminated);
            assert!(!state.data_dma_active);
            assert!(state.last_line.is_none());
        }
    }

    #[test]
    fn sprite_dma_zero_height_descriptor_terminates_stream() {
        let mut bus = empty_bus();
        let sprite_ptr = 0x0100usize;
        let (zero_pos, zero_ctl) = sprite_control_words(0x2C, 0x2C, 0x0083);
        let (pos, ctl) = sprite_control_words(0x2C, 0x2D, 0x0091);
        write_chip_word(&mut bus, sprite_ptr, zero_pos);
        write_chip_word(&mut bus, sprite_ptr + 2, zero_ctl);
        write_chip_word(&mut bus, sprite_ptr + 4, pos);
        write_chip_word(&mut bus, sprite_ptr + 6, ctl);
        write_chip_word(&mut bus, sprite_ptr + 8, 0x1111);
        write_chip_word(&mut bus, sprite_ptr + 10, 0x2222);
        write_chip_word(&mut bus, sprite_ptr + 12, 0);
        write_chip_word(&mut bus, sprite_ptr + 14, 0);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_SPREN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
        bus.denise.sprpt[0] = sprite_ptr as u32;
        bus.display_dma_sprpt[0] = sprite_ptr as u32;

        bus.advance_chipset(2);

        let lines = bus.frame_captured_sprite_lines();
        assert!(lines.is_empty());
    }

    #[test]
    fn sprite_dma_capture_wraps_control_words_at_chip_ram_end() {
        let mut bus = empty_bus();
        let sprite_ptr = bus.mem.chip_ram.len() - 2;
        let (pos, ctl) = sprite_control_words(0x2C, 0x2D, 0x0091);
        write_chip_word_wrapping(&mut bus, sprite_ptr, pos);
        write_chip_word_wrapping(&mut bus, sprite_ptr + 2, ctl);
        write_chip_word_wrapping(&mut bus, sprite_ptr + 4, 0x1111);
        write_chip_word_wrapping(&mut bus, sprite_ptr + 6, 0x2222);
        write_chip_word_wrapping(&mut bus, sprite_ptr + 8, 0);
        write_chip_word_wrapping(&mut bus, sprite_ptr + 10, 0);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_SPREN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
        bus.denise.sprpt[0] = sprite_ptr as u32;
        bus.display_dma_sprpt[0] = sprite_ptr as u32;

        bus.advance_chipset(2);

        let lines = bus.frame_captured_sprite_lines();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].hstart, 0x0091);
        assert_eq!(lines[0].data, 0x1111);
        assert_eq!(lines[0].datb, 0x2222);
    }

    #[test]
    fn sprite_dma_zero_height_descriptor_terminates_after_chip_address_wrap() {
        let mut bus = empty_bus();
        let sprite_ptr = bus.mem.chip_ram.len() - 2;
        let (zero_pos, zero_ctl) = sprite_control_words(0x2C, 0x2C, 0x0083);
        let (pos, ctl) = sprite_control_words(0x2C, 0x2D, 0x0091);
        write_chip_word_wrapping(&mut bus, sprite_ptr, zero_pos);
        write_chip_word_wrapping(&mut bus, sprite_ptr + 2, zero_ctl);
        let active_ptr = sprite_ptr + 4;
        write_chip_word_wrapping(&mut bus, active_ptr, pos);
        write_chip_word_wrapping(&mut bus, active_ptr + 2, ctl);
        write_chip_word_wrapping(&mut bus, active_ptr + 4, 0x1111);
        write_chip_word_wrapping(&mut bus, active_ptr + 6, 0x2222);
        write_chip_word_wrapping(&mut bus, active_ptr + 8, 0);
        write_chip_word_wrapping(&mut bus, active_ptr + 10, 0);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_SPREN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
        bus.denise.sprpt[0] = sprite_ptr as u32;
        bus.display_dma_sprpt[0] = sprite_ptr as u32;
        bus.denise.sprpt[1] = 0x0200;
        bus.display_dma_sprpt[1] = 0x0200;

        bus.advance_chipset(2);

        let lines = bus.frame_captured_sprite_lines();
        assert!(lines.is_empty());
    }

    #[test]
    fn sprite_dma_capture_latches_control_words_until_stop() {
        let mut bus = empty_bus();
        let sprite_ptr = 0x0100usize;
        let (pos, ctl) = sprite_control_words(0x2C, 0x2E, 0x0083);
        write_chip_word(&mut bus, sprite_ptr, pos);
        write_chip_word(&mut bus, sprite_ptr + 2, ctl);
        write_chip_word(&mut bus, sprite_ptr + 4, 0x1111);
        write_chip_word(&mut bus, sprite_ptr + 6, 0x2222);
        write_chip_word(&mut bus, sprite_ptr + 8, 0x3333);
        write_chip_word(&mut bus, sprite_ptr + 10, 0x4444);
        write_chip_word(&mut bus, sprite_ptr + 12, 0);
        write_chip_word(&mut bus, sprite_ptr + 14, 0);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_SPREN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
        bus.denise.sprpt[0] = sprite_ptr as u32;
        bus.display_dma_sprpt[0] = sprite_ptr as u32;

        bus.advance_chipset(2);

        let (rewritten_pos, rewritten_ctl) = sprite_control_words(0x2D, 0x2E, 0x0091);
        write_chip_word(&mut bus, sprite_ptr, rewritten_pos);
        write_chip_word(&mut bus, sprite_ptr + 2, rewritten_ctl);
        bus.agnus.vpos = 0x2D;
        bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
        bus.advance_chipset(2);

        let lines = bus.frame_captured_sprite_lines();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].hstart, 0x0083);
        assert_eq!(lines[0].data, 0x1111);
        assert_eq!(lines[1].beam_y, 0x2D);
        assert_eq!(lines[1].hstart, 0x0083);
        assert_eq!(lines[1].data, 0x3333);
        assert_eq!(lines[1].datb, 0x4444);
    }

    #[test]
    fn sprite_dma_capture_samples_later_pairs_at_their_fetch_slot() {
        let mut bus = empty_bus();
        let (pos, ctl) = sprite_control_words(0x2C, 0x2D, 0x0083);
        let sprite0_ptr = 0x0100usize;
        let sprite6_ptr = 0x0200usize;
        for ptr in [sprite0_ptr, sprite6_ptr] {
            write_chip_word(&mut bus, ptr, pos);
            write_chip_word(&mut bus, ptr + 2, ctl);
            write_chip_word(&mut bus, ptr + 8, 0);
            write_chip_word(&mut bus, ptr + 10, 0);
        }
        write_chip_word(&mut bus, sprite0_ptr + 4, 0x1111);
        write_chip_word(&mut bus, sprite0_ptr + 6, 0x2222);
        write_chip_word(&mut bus, sprite6_ptr + 4, 0x6666);
        write_chip_word(&mut bus, sprite6_ptr + 6, 0x7777);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_SPREN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
        bus.denise.sprpt[0] = sprite0_ptr as u32;
        bus.denise.sprpt[6] = sprite6_ptr as u32;
        bus.display_dma_sprpt[0] = sprite0_ptr as u32;
        bus.display_dma_sprpt[6] = sprite6_ptr as u32;

        bus.advance_chipset(2);
        write_chip_word(&mut bus, sprite0_ptr + 4, 0xAAAA);
        write_chip_word(&mut bus, sprite6_ptr + 4, 0xBBBB);
        let remaining = SPRITE_DMA_PAIR_CAPTURE_HPOS[3] + 1 - bus.agnus.hpos;
        bus.advance_chipset(remaining);

        let lines = bus.frame_captured_sprite_lines();
        let sprite0 = lines.iter().find(|line| line.sprite == 0).unwrap();
        let sprite6 = lines.iter().find(|line| line.sprite == 6).unwrap();
        assert_eq!(sprite0.data, 0x1111);
        assert_eq!(sprite6.data, 0xBBBB);
    }

    #[test]
    fn sprite_dma_capture_blocks_sprite_seven_when_ddfstrt_uses_early_fetch_slot() {
        let mut bus = empty_bus();
        let (pos, ctl) = sprite_control_words(0x2C, 0x2D, 0x0083);
        let sprite6_ptr = 0x0200usize;
        let sprite7_ptr = 0x0300usize;
        for ptr in [sprite6_ptr, sprite7_ptr] {
            write_chip_word(&mut bus, ptr, pos);
            write_chip_word(&mut bus, ptr + 2, ctl);
            write_chip_word(&mut bus, ptr + 8, 0);
            write_chip_word(&mut bus, ptr + 10, 0);
        }
        write_chip_word(&mut bus, sprite6_ptr + 4, 0x6666);
        write_chip_word(&mut bus, sprite6_ptr + 6, 0x7777);
        write_chip_word(&mut bus, sprite7_ptr + 4, 0x8888);
        write_chip_word(&mut bus, sprite7_ptr + 6, 0x9999);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN | DMACON_SPREN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[3] - 1;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.ddfstrt = 0x0028;
        bus.denise.ddfstop = 0x0038;
        bus.denise.sprpt[6] = sprite6_ptr as u32;
        bus.denise.sprpt[7] = sprite7_ptr as u32;
        bus.display_dma_sprpt[6] = sprite6_ptr as u32;
        bus.display_dma_sprpt[7] = sprite7_ptr as u32;

        bus.advance_chipset(2);

        let lines = bus.frame_captured_sprite_lines();
        assert!(lines.iter().any(|line| line.sprite == 6));
        assert!(!lines.iter().any(|line| line.sprite == 7));
    }

    #[test]
    fn sprite_dma_capture_repeats_last_fetched_line_after_dma_disable_until_vstop() {
        let mut bus = empty_bus();
        let sprite_ptr = 0x0100usize;
        let (pos, ctl) = sprite_control_words(0x2C, 0x2E, 0x0083);
        write_chip_word(&mut bus, sprite_ptr, pos);
        write_chip_word(&mut bus, sprite_ptr + 2, ctl);
        write_chip_word(&mut bus, sprite_ptr + 4, 0x1111);
        write_chip_word(&mut bus, sprite_ptr + 6, 0x2222);
        write_chip_word(&mut bus, sprite_ptr + 8, 0x3333);
        write_chip_word(&mut bus, sprite_ptr + 10, 0x4444);
        write_chip_word(&mut bus, sprite_ptr + 12, 0);
        write_chip_word(&mut bus, sprite_ptr + 14, 0);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_SPREN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
        bus.denise.sprpt[0] = sprite_ptr as u32;
        bus.display_dma_sprpt[0] = sprite_ptr as u32;

        bus.advance_chipset(2);
        bus.agnus.dmacon = DMACON_DMAEN;
        bus.agnus.vpos = 0x2D;
        bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
        bus.advance_chipset(2);
        bus.agnus.vpos = 0x2E;
        bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
        bus.advance_chipset(2);

        let lines = bus.frame_captured_sprite_lines();
        let first = lines
            .iter()
            .find(|line| line.sprite == 0 && line.beam_y == 0x2C)
            .unwrap();
        let repeated = lines
            .iter()
            .find(|line| line.sprite == 0 && line.beam_y == 0x2D)
            .unwrap();
        assert_eq!((first.data, first.datb), (0x1111, 0x2222));
        assert_eq!((repeated.data, repeated.datb), (0x1111, 0x2222));
        assert!(!lines
            .iter()
            .any(|line| line.sprite == 0 && line.beam_y == 0x2E));
    }

    #[test]
    fn sprite_dma_capture_does_not_start_descriptor_at_or_before_current_vpos() {
        let mut bus = empty_bus();
        let sprite_ptr = 0x0100usize;
        let (pos, ctl) = sprite_control_words(0x2C, 0x2E, 0x0083);
        write_chip_word(&mut bus, sprite_ptr, pos);
        write_chip_word(&mut bus, sprite_ptr + 2, ctl);
        write_chip_word(&mut bus, sprite_ptr + 4, 0x1111);
        write_chip_word(&mut bus, sprite_ptr + 6, 0x2222);
        write_chip_word(&mut bus, sprite_ptr + 8, 0x3333);
        write_chip_word(&mut bus, sprite_ptr + 10, 0x4444);
        write_chip_word(&mut bus, sprite_ptr + 12, 0);
        write_chip_word(&mut bus, sprite_ptr + 14, 0);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_SPREN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
        bus.denise.sprpt[0] = sprite_ptr as u32;
        bus.display_dma_sprite_state[0].next_ptr = Some(sprite_ptr as u32);

        bus.advance_chipset(2);
        bus.agnus.vpos = 0x2D;
        bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
        bus.advance_chipset(2);

        assert!(bus
            .frame_captured_sprite_lines()
            .iter()
            .all(|line| line.sprite != 0));
    }

    #[test]
    fn sprite_dma_reuse_skips_descriptor_with_vstart_before_current_vpos() {
        let mut bus = empty_bus();
        let sprite_ptr = 0x0100usize;
        let (first_pos, first_ctl) = sprite_control_words(0x2C, 0x2D, 0x0083);
        let (past_pos, past_ctl) = sprite_control_words(0x2C, 0x2F, 0x0091);
        write_chip_word(&mut bus, sprite_ptr, first_pos);
        write_chip_word(&mut bus, sprite_ptr + 2, first_ctl);
        write_chip_word(&mut bus, sprite_ptr + 4, 0x1111);
        write_chip_word(&mut bus, sprite_ptr + 6, 0x2222);
        write_chip_word(&mut bus, sprite_ptr + 8, past_pos);
        write_chip_word(&mut bus, sprite_ptr + 10, past_ctl);
        write_chip_word(&mut bus, sprite_ptr + 12, 0x3333);
        write_chip_word(&mut bus, sprite_ptr + 14, 0x4444);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_SPREN;
        bus.agnus.vpos = 0x2B;
        bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
        bus.denise.sprpt[0] = sprite_ptr as u32;
        bus.display_dma_sprpt[0] = sprite_ptr as u32;

        bus.advance_chipset(2);
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
        bus.advance_chipset(2);
        bus.agnus.vpos = 0x2D;
        bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
        bus.advance_chipset(2);

        let lines = bus.frame_captured_sprite_lines();
        assert!(lines
            .iter()
            .any(|line| line.sprite == 0 && line.beam_y == 0x2C));
        assert!(!lines
            .iter()
            .any(|line| line.sprite == 0 && line.beam_y == 0x2D));
    }

    #[test]
    fn visible_sprite_pixels_accumulate_live_sprite_sprite_clxdat() {
        let mut bus = empty_bus();
        let (pos, ctl) = sprite_control_words(0x2C, 0x2D, 0x0083);
        let sprite0_ptr = 0x0100usize;
        let sprite2_ptr = 0x0200usize;
        for ptr in [sprite0_ptr, sprite2_ptr] {
            write_chip_word(&mut bus, ptr, pos);
            write_chip_word(&mut bus, ptr + 2, ctl);
            write_chip_word(&mut bus, ptr + 4, 0x8000);
            write_chip_word(&mut bus, ptr + 6, 0);
            write_chip_word(&mut bus, ptr + 8, 0);
            write_chip_word(&mut bus, ptr + 10, 0);
        }

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_SPREN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
        bus.denise.sprpt[0] = sprite0_ptr as u32;
        bus.denise.sprpt[2] = sprite2_ptr as u32;
        bus.display_dma_sprpt[0] = sprite0_ptr as u32;
        bus.display_dma_sprpt[2] = sprite2_ptr as u32;
        bus.current_frame_sprite_display_enable_x_by_y[0] = Some(0);

        bus.advance_chipset(1);
        assert_eq!(bus.custom_read(0x00E, 2), 0x8000);

        let remaining = SPRITE_DMA_PAIR_CAPTURE_HPOS[1] + 1 - bus.agnus.hpos;
        bus.advance_chipset(remaining);
        assert_eq!(bus.custom_read(0x00E, 2), 0x8000);

        let remaining = 0x3A - bus.agnus.hpos;
        bus.advance_chipset(remaining);
        assert_eq!(bus.custom_read(0x00E, 2), 0x8200);
        assert_eq!(bus.custom_read(0x00E, 2), 0x8000);
    }

    #[test]
    fn palette_register_writes_do_not_feed_live_collision_replay() {
        let mut bus = empty_bus();

        bus.write_custom_word_from(0x180, 0x0ABC, BeamWriteSource::Copper);
        bus.write_custom_word_from(0x098, 1 << 12, BeamWriteSource::Copper);

        assert_eq!(bus.current_frame_render_events.len(), 2);
        assert_eq!(bus.current_frame_collision_events.len(), 1);
        assert_eq!(bus.current_frame_collision_events[0].offset, 0x098);
        assert_eq!(bus.current_frame_collision_control_events.len(), 1);
        assert_eq!(bus.current_frame_collision_control_events[0].offset, 0x098);
        assert!(bus.current_frame_collision_bpldat_events.is_empty());
        assert!(bus.current_frame_collision_sprite_events.is_empty());
    }

    #[test]
    fn sprite_data_writes_do_not_feed_live_collision_control_replay() {
        let mut bus = empty_bus();

        bus.write_custom_word_from(0x144, 0x8000, BeamWriteSource::Cpu);

        assert_eq!(bus.current_frame_collision_events.len(), 1);
        assert_eq!(bus.current_frame_collision_events[0].offset, 0x144);
        assert_eq!(bus.current_frame_collision_sprite_events.len(), 1);
        assert_eq!(bus.current_frame_collision_sprite_events[0].offset, 0x144);
        assert!(bus.current_frame_collision_control_events.is_empty());
        assert!(bus.current_frame_collision_bpldat_events.is_empty());
    }

    #[test]
    fn beam_timed_collision_indexes_are_reused_until_relevant_register_writes() {
        let mut bus = empty_bus();

        bus.write_custom_word_from(0x098, 1 << 12, BeamWriteSource::Copper);
        bus.ensure_current_collision_control_index();
        assert!(bus.current_frame_collision_control_index.is_some());

        bus.write_custom_word_from(0x180, 0x0ABC, BeamWriteSource::Copper);
        assert!(bus.current_frame_collision_control_index.is_some());

        bus.ensure_current_collision_sprite_index();
        assert!(bus.current_frame_collision_sprite_index.is_some());
        bus.write_custom_word_from(0x144, 0x8000, BeamWriteSource::Cpu);
        assert!(bus.current_frame_collision_control_index.is_some());
        assert!(bus.current_frame_collision_sprite_index.is_none());

        bus.ensure_current_collision_bpldat_index();
        assert!(bus.current_frame_collision_bpldat_index.is_some());
        bus.write_custom_word_from(0x110, 0xFFFF, BeamWriteSource::Copper);
        assert!(bus.current_frame_collision_control_index.is_none());
        assert!(bus.current_frame_collision_bpldat_index.is_none());
    }

    #[test]
    fn manual_sprite_data_writes_accumulate_live_sprite_sprite_clxdat() {
        let mut bus = empty_bus();
        let (pos, ctl) = sprite_control_words(0x2C, 0x2D, 0x0083);
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = 0x38;
        bus.denise.diwstrt = 0x2C83;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.sprpos[0] = pos;
        bus.denise.sprctl[0] = ctl;
        bus.denise.sprpos[2] = pos;
        bus.denise.sprctl[2] = ctl;
        bus.current_frame_sprite_display_enable_x_by_y[0] = Some(0);
        bus.current_frame_render_base = bus.capture_render_snapshot();

        bus.write_custom_word_from(0x144, 0x8000, BeamWriteSource::Cpu);
        bus.write_custom_word_from(0x154, 0x8000, BeamWriteSource::Cpu);
        bus.advance_chipset(2);

        assert_eq!(bus.custom_read(0x00E, 2), 0x8200);
        assert_eq!(bus.custom_read(0x00E, 2), 0x8000);
    }

    #[test]
    fn frame_end_completes_unread_live_sprite_sprite_clxdat() {
        let mut bus = empty_bus();
        let (pos, ctl) = sprite_control_words(0x2C, 0x2D, 0x0083);
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = 0x38;
        bus.denise.diwstrt = 0x2C83;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.sprpos[0] = pos;
        bus.denise.sprctl[0] = ctl;
        bus.denise.sprpos[2] = pos;
        bus.denise.sprctl[2] = ctl;
        bus.current_frame_sprite_display_enable_x_by_y[0] = Some(0);
        bus.current_frame_render_base = bus.capture_render_snapshot();

        bus.write_custom_word_from(0x144, 0x8000, BeamWriteSource::Cpu);
        bus.write_custom_word_from(0x154, 0x8000, BeamWriteSource::Cpu);
        bus.accumulate_live_collisions_to_frame_end();

        assert_eq!(bus.custom_read(0x00E, 2), 0x8200);
        assert_eq!(bus.custom_read(0x00E, 2), 0x8000);
    }

    #[test]
    fn attached_manual_sprite_data_writes_accumulate_live_sprite_sprite_clxdat() {
        let mut bus = empty_bus();
        let (pos, ctl) = sprite_control_words(0x2C, 0x2D, 0x0083);
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = 0x38;
        bus.denise.diwstrt = 0x2C83;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.sprpos[0] = pos;
        bus.denise.sprctl[0] = ctl;
        bus.denise.sprpos[1] = pos;
        bus.denise.sprctl[1] = ctl | 0x0080;
        bus.denise.sprpos[2] = pos;
        bus.denise.sprctl[2] = ctl;
        bus.denise.clxcon = 1 << 12;
        bus.current_frame_sprite_display_enable_x_by_y[0] = Some(0);
        bus.current_frame_render_base = bus.capture_render_snapshot();

        bus.write_custom_word_from(0x144, 0x0000, BeamWriteSource::Cpu);
        bus.write_custom_word_from(0x14C, 0x8000, BeamWriteSource::Cpu);
        bus.write_custom_word_from(0x154, 0x8000, BeamWriteSource::Cpu);
        bus.advance_chipset(2);

        assert_eq!(bus.custom_read(0x00E, 2), 0x8200);
        assert_eq!(bus.custom_read(0x00E, 2), 0x8000);
    }

    #[test]
    fn attached_manual_sprite_odd_data_writes_accumulate_later_live_sprite_sprite_clxdat() {
        let mut bus = empty_bus();
        let (pos, ctl) = sprite_control_words(0x2C, 0x2D, 0x0083);
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = 0x38;
        bus.denise.diwstrt = 0x2C83;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.sprpos[0] = pos;
        bus.denise.sprctl[0] = ctl;
        bus.denise.sprpos[1] = pos;
        bus.denise.sprctl[1] = ctl | 0x0080;
        bus.denise.sprpos[2] = pos;
        bus.denise.sprctl[2] = ctl;
        bus.denise.clxcon = 1 << 12;
        bus.current_frame_sprite_display_enable_x_by_y[0] = Some(0);
        bus.current_frame_render_base = bus.capture_render_snapshot();

        bus.write_custom_word_from(0x144, 0x0000, BeamWriteSource::Cpu);
        bus.write_custom_word_from(0x14C, 0x8000, BeamWriteSource::Cpu);
        bus.write_custom_word_from(0x154, 0x2000, BeamWriteSource::Cpu);
        bus.advance_chipset(2);
        assert_eq!(bus.custom_read(0x00E, 2), 0x8000);

        bus.write_custom_word_from(0x14C, 0x2000, BeamWriteSource::Cpu);
        bus.advance_chipset(1);

        assert_eq!(bus.custom_read(0x00E, 2), 0x8200);
        assert_eq!(bus.custom_read(0x00E, 2), 0x8000);
    }

    #[test]
    fn attached_manual_sprite_sources_preserve_even_intervals_outside_odd_attachment() {
        let mut bus = empty_bus();
        let (pos, ctl) = sprite_control_words(0x2C, 0x2D, 0x0083);
        bus.denise.sprpos[0] = pos;
        bus.denise.sprctl[0] = ctl;
        bus.denise.sprdata[0] = 0xA000;
        bus.denise.spr_armed[0] = true;
        bus.denise.sprpos[1] = pos;
        bus.denise.sprctl[1] = ctl | 0x0080;
        let frame_base = bus.capture_render_snapshot();
        let events = [BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: RENDER_COPPER_WAIT_HPOS_FB0 + 1,
            offset: 0x014C,
            value: 0x2000,
            source: BeamWriteSource::Cpu,
        }];

        let event_index = BeamEventIndex::from_register_writes(&events);
        let sources = live_manual_sprite_collision_sources(frame_base, &event_index, 0x2C, 0, 8);

        assert!(sources.iter().any(|source| {
            source.sprite == 0
                && source.x_start == 0
                && source.x_stop == 8
                && source.source.words == [0xA000, 0, 0, 0]
                && !source.source.requires_odd_enable
        }));
        assert!(sources.iter().any(|source| {
            source.sprite == 1
                && source.x_start == 4
                && source.x_stop == 8
                && source.source.words == [0x2000, 0, 0, 0]
                && source.source.requires_odd_enable
        }));
    }

    #[test]
    fn manual_sprite_position_write_uses_sprite_compare_domain_for_live_sources() {
        let event_hpos = 96;
        let event = BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: event_hpos,
            offset: 0x0150,
            value: 0,
            source: BeamWriteSource::Cpu,
        };
        let sprite_compare_hpos = event_hpos.saturating_sub(DENISE_HPOS_LAG_CCK);
        let sprite_compare_x = (sprite_compare_hpos as i32 * 2 - RENDER_DIW_HSTART_FB0) * 2;
        let colour_output_x = (event_hpos as i32 - RENDER_COPPER_WAIT_HPOS_FB0 as i32) * 4;

        assert_eq!(super::live_manual_sprite_event_x(event), sprite_compare_x);
        assert_ne!(super::live_manual_sprite_event_x(event), colour_output_x);
    }

    #[test]
    fn manual_sprite_position_write_on_compare_boundary_preserves_live_source() {
        let mut bus = empty_bus();
        let first_hstart = 0x007E;
        let second_hstart = 0x008E;
        let (first_pos, ctl) = sprite_control_words(0x2C, 0x2D, first_hstart);
        let (second_pos, _) = sprite_control_words(0x2C, 0x2D, second_hstart);
        bus.denise.sprpos[0] = first_pos;
        bus.denise.sprctl[0] = ctl;
        bus.denise.sprdata[0] = 0xFFFF;
        bus.denise.spr_armed[0] = true;
        let frame_base = bus.capture_render_snapshot();
        let boundary_hpos = u32::from(first_hstart / 2) + DENISE_HPOS_LAG_CCK;
        let event = BeamRegisterWrite {
            vpos: RENDER_VISIBLE_START_VPOS,
            hpos: boundary_hpos,
            offset: 0x0140,
            value: second_pos,
            source: BeamWriteSource::Cpu,
        };
        let event_index = BeamEventIndex::from_register_writes(&[event]);
        let first_base_x = (i32::from(first_hstart) - RENDER_DIW_HSTART_FB0) * 2;

        let sources = live_manual_sprite_collision_sources(
            frame_base,
            &event_index,
            0x2C,
            first_base_x,
            first_base_x + 2,
        );

        assert!(sources.iter().any(|source| {
            source.sprite == 0
                && source.x_start == first_base_x
                && source.x_stop == first_base_x + 2
                && source.source.words == [0xFFFF, 0, 0, 0]
        }));
    }

    #[test]
    fn manual_sprite_data_writes_accumulate_live_sprite_playfield_clxdat() {
        let mut bus = empty_bus();
        let (pos, ctl) = sprite_control_words(0x2C, 0x2D, 0x0081);
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = 0x38;
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0038;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.sprpos[0] = pos;
        bus.denise.sprctl[0] = ctl;
        bus.current_frame_sprite_display_enable_x_by_y[0] = Some(0);
        bus.current_frame_render_base = bus.capture_render_snapshot();
        bus.current_frame_bitplane_rows[0] = Some(CapturedBitplaneRow {
            nplanes: 1,
            words_per_row: 1,
            planes: [
                vec![0x4000],
                vec![0],
                vec![0],
                vec![0],
                vec![0],
                vec![0],
                Vec::new(),
                Vec::new(),
            ],
        });

        bus.write_custom_word_from(0x144, 0x8000, BeamWriteSource::Cpu);
        bus.advance_chipset(2);

        assert_eq!(bus.custom_read(0x00E, 2), 0x8022);
        assert_eq!(bus.custom_read(0x00E, 2), 0x8000);
    }

    #[test]
    fn attached_manual_sprite_data_writes_accumulate_live_sprite_playfield_clxdat() {
        let mut bus = empty_bus();
        let (pos, ctl) = sprite_control_words(0x2C, 0x2D, 0x0083);
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = 0x38;
        bus.denise.diwstrt = 0x2C83;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0038;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.sprpos[0] = pos;
        bus.denise.sprctl[0] = ctl;
        bus.denise.sprpos[1] = pos;
        bus.denise.sprctl[1] = ctl | 0x0080;
        bus.denise.clxcon = 1 << 12;
        bus.current_frame_sprite_display_enable_x_by_y[0] = Some(0);
        bus.current_frame_render_base = bus.capture_render_snapshot();
        bus.current_frame_bitplane_rows[0] = Some(CapturedBitplaneRow {
            nplanes: 1,
            words_per_row: 1,
            planes: [
                vec![0x8000],
                vec![0],
                vec![0],
                vec![0],
                vec![0],
                vec![0],
                Vec::new(),
                Vec::new(),
            ],
        });

        bus.write_custom_word_from(0x144, 0x0000, BeamWriteSource::Cpu);
        bus.write_custom_word_from(0x14C, 0x8000, BeamWriteSource::Cpu);
        bus.advance_chipset(2);

        assert_eq!(bus.custom_read(0x00E, 2), 0x8022);
        assert_eq!(bus.custom_read(0x00E, 2), 0x8000);
    }

    #[test]
    fn bplcon3_spres_hires_narrows_live_sprite_sprite_clxdat() {
        let clxdat_after_visible_sprite_pixels = |bplcon3| {
            let mut bus = empty_bus();
            let (pos0, ctl0) = sprite_control_words(0x2C, 0x2D, 0x0083);
            let (pos2, ctl2) = sprite_control_words(0x2C, 0x2D, 0x0084);
            let sprite0_ptr = 0x0100usize;
            let sprite2_ptr = 0x0200usize;

            write_chip_word(&mut bus, sprite0_ptr, pos0);
            write_chip_word(&mut bus, sprite0_ptr + 2, ctl0);
            write_chip_word(&mut bus, sprite0_ptr + 4, 0x4000);
            write_chip_word(&mut bus, sprite0_ptr + 6, 0);
            write_chip_word(&mut bus, sprite0_ptr + 8, 0);
            write_chip_word(&mut bus, sprite0_ptr + 10, 0);
            write_chip_word(&mut bus, sprite2_ptr, pos2);
            write_chip_word(&mut bus, sprite2_ptr + 2, ctl2);
            write_chip_word(&mut bus, sprite2_ptr + 4, 0x8000);
            write_chip_word(&mut bus, sprite2_ptr + 6, 0);
            write_chip_word(&mut bus, sprite2_ptr + 8, 0);
            write_chip_word(&mut bus, sprite2_ptr + 10, 0);

            bus.agnus.dmacon = DMACON_DMAEN | DMACON_SPREN;
            bus.agnus.vpos = 0x2C;
            bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
            bus.denise.bplcon0 = 0x8000;
            bus.denise.bplcon3 = bplcon3;
            bus.denise.sprpt[0] = sprite0_ptr as u32;
            bus.denise.sprpt[2] = sprite2_ptr as u32;
            bus.display_dma_sprpt[0] = sprite0_ptr as u32;
            bus.display_dma_sprpt[2] = sprite2_ptr as u32;
            bus.current_frame_sprite_display_enable_x_by_y[0] = Some(0);

            let remaining = 0x3A - bus.agnus.hpos;
            bus.advance_chipset(remaining);

            bus.custom_read(0x00E, 2)
        };

        assert_eq!(clxdat_after_visible_sprite_pixels(0), 0x8200);
        assert_eq!(
            clxdat_after_visible_sprite_pixels(BPLCON3_SPRES_HIRES),
            0x8000
        );
    }

    #[test]
    fn same_line_clxcon_odd_sprite_enable_does_not_retime_earlier_live_sprite_sprite_clxdat() {
        let clxdat_after_visible_sprite_pixels = |initial_clxcon, enable_hpos: Option<u32>| {
            let mut bus = empty_bus();
            let (pos, ctl) = sprite_control_words(0x2C, 0x2D, 0x0083);
            let sprite1_ptr = 0x0100usize;
            let sprite2_ptr = 0x0200usize;
            for ptr in [sprite1_ptr, sprite2_ptr] {
                write_chip_word(&mut bus, ptr, pos);
                write_chip_word(&mut bus, ptr + 2, ctl);
                write_chip_word(&mut bus, ptr + 4, 0x8000);
                write_chip_word(&mut bus, ptr + 6, 0);
                write_chip_word(&mut bus, ptr + 8, 0);
                write_chip_word(&mut bus, ptr + 10, 0);
            }

            bus.agnus.dmacon = DMACON_DMAEN | DMACON_SPREN;
            bus.agnus.vpos = 0x2C;
            bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
            bus.denise.clxcon = initial_clxcon;
            bus.denise.sprpt[1] = sprite1_ptr as u32;
            bus.denise.sprpt[2] = sprite2_ptr as u32;
            bus.display_dma_sprpt[1] = sprite1_ptr as u32;
            bus.display_dma_sprpt[2] = sprite2_ptr as u32;
            bus.current_frame_sprite_display_enable_x_by_y[0] = Some(0);
            bus.current_frame_render_base = bus.capture_render_snapshot();

            let after_pair_capture = SPRITE_DMA_PAIR_CAPTURE_HPOS[1] + 1 - bus.agnus.hpos;
            bus.advance_chipset(after_pair_capture);

            if let Some(enable_hpos) = enable_hpos {
                let before_enable = enable_hpos - bus.agnus.hpos;
                bus.advance_chipset(before_enable);
                bus.write_custom_word_from(0x098, 1 << 12, BeamWriteSource::Cpu);
            }
            let remaining = 0x3A - bus.agnus.hpos;
            bus.advance_chipset(remaining);

            bus.custom_read(0x00E, 2)
        };

        assert_eq!(clxdat_after_visible_sprite_pixels(1 << 12, None), 0x8200);
        assert_eq!(clxdat_after_visible_sprite_pixels(0, Some(0x38)), 0x8200);
        assert_eq!(clxdat_after_visible_sprite_pixels(0, Some(0x3A)), 0x8000);
    }

    #[test]
    fn ocs_lowres_bitplane_dma_fetches_plane_two_before_plane_one() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = 0x3A;
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0038;
        bus.denise.bplcon0 = 0x2000;
        bus.denise.bplpt[0] = 0x0100;
        bus.denise.bplpt[1] = 0x0200;
        bus.display_dma_bplpt[0] = 0x0100;
        bus.display_dma_bplpt[1] = 0x0200;
        write_chip_word(&mut bus, 0x0100, 0x1111);
        write_chip_word(&mut bus, 0x0200, 0x2222);

        bus.advance_chipset(2);
        write_chip_word(&mut bus, 0x0100, 0xAAAA);
        write_chip_word(&mut bus, 0x0200, 0xBBBB);
        bus.advance_chipset(4);

        let row = bus.frame_captured_bitplane_rows()[0].as_ref().unwrap();
        assert_eq!(row.planes[0][0], 0xAAAA);
        assert_eq!(row.planes[1][0], 0x2222);
        assert_eq!(bus.display_dma_bplpt[0], 0x0102);
        assert_eq!(bus.display_dma_bplpt[1], 0x0202);
    }

    #[test]
    fn bitplane_dma_fetch_loads_bpldat_latch() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = 0x3A;
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0038;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.bplpt[0] = 0x0100;
        bus.display_dma_bplpt[0] = 0x0100;
        write_chip_word(&mut bus, 0x0100, 0x8001);

        bus.advance_chipset(6);

        assert_eq!(bus.denise.bpldat[0], 0x8001);
    }

    #[test]
    fn bitplane_dma_capture_accumulates_live_dual_playfield_clxdat() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = 0x3A;
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0038;
        bus.denise.bplcon0 = 0x2400;
        bus.denise.bplpt[0] = 0x0100;
        bus.denise.bplpt[1] = 0x0200;
        bus.display_dma_bplpt[0] = 0x0100;
        bus.display_dma_bplpt[1] = 0x0200;
        write_chip_word(&mut bus, 0x0100, 0x4000);
        write_chip_word(&mut bus, 0x0200, 0x4000);

        bus.advance_chipset(6);

        assert_eq!(bus.custom_read(0x00E, 2), 0x8001);
        assert_eq!(bus.custom_read(0x00E, 2), 0x8000);
    }

    #[test]
    fn latched_playfield_playfield_clxdat_bit_skips_completed_row_scan() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = 0x3A;
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0038;
        bus.denise.bplcon0 = 0x2400;
        bus.denise.clxdat = 1;
        bus.denise.bplpt[0] = 0x0100;
        bus.denise.bplpt[1] = 0x0200;
        bus.display_dma_bplpt[0] = 0x0100;
        bus.display_dma_bplpt[1] = 0x0200;
        write_chip_word(&mut bus, 0x0100, 0x4000);
        write_chip_word(&mut bus, 0x0200, 0x8000);

        bus.advance_chipset(6);

        assert_eq!(bus.video_pipeline_stats.collision_calls, 0);
        assert_eq!(bus.custom_read(0x00E, 2), 0x8001);
    }

    #[test]
    fn horizontal_diw_clips_live_playfield_clxdat() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = 0x3A;
        bus.denise.diwstrt = 0x2C84;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0038;
        bus.denise.bplcon0 = 0x2400;
        bus.denise.bplpt[0] = 0x0100;
        bus.denise.bplpt[1] = 0x0200;
        bus.display_dma_bplpt[0] = 0x0100;
        bus.display_dma_bplpt[1] = 0x0200;
        write_chip_word(&mut bus, 0x0100, 0x4000);
        write_chip_word(&mut bus, 0x0200, 0x8000);

        bus.advance_chipset(6);

        assert_eq!(bus.custom_read(0x00E, 2), 0x8000);
    }

    #[test]
    fn shifted_horizontal_diw_offsets_live_playfield_clxdat_fetch_origin() {
        let row = CapturedBitplaneRow {
            nplanes: 2,
            words_per_row: 2,
            planes: [
                vec![0, 0x1000],
                vec![0, 0x1000],
                vec![0; 2],
                vec![0; 2],
                vec![0; 2],
                vec![0; 2],
                Vec::new(),
                Vec::new(),
            ],
        };
        let control = LiveCollisionControl::from_current(
            0x2400,
            0,
            0,
            0,
            0x2C93,
            0x2DC1,
            DiwHigh::ocs_implicit(),
            0x0038,
            [0; 8],
        );

        assert_eq!(
            live_bitplane_collision_bits(
                &row,
                &LiveCollisionLineReplay::from_index(
                    control,
                    RenderRegisterSnapshot::default(),
                    &BeamEventIndex::from_register_writes(&[]),
                    RENDER_VISIBLE_START_VPOS as i32,
                ),
                RENDER_VISIBLE_START_VPOS as i32,
            ),
            1
        );
    }

    #[test]
    fn denise_horizontal_delay_aligns_sprite_playfield_collision_domain() {
        let display_x = live_display_window_x(0x2C81, 0x2DC1, DiwHigh::ocs_implicit()).0;
        let copper_hpos = RENDER_COPPER_WAIT_HPOS_FB0 + (display_x as u32 / 4);
        assert_eq!(display_x, STANDARD_VISIBLE_X0 as i32);
        assert_eq!(
            framebuffer_x_for_live_collision_hpos(copper_hpos),
            display_x
        );

        let row = CapturedBitplaneRow {
            nplanes: 1,
            words_per_row: 1,
            planes: [
                vec![0x8000],
                vec![0],
                vec![0],
                vec![0],
                vec![0],
                vec![0],
                Vec::new(),
                Vec::new(),
            ],
        };
        let control = LiveCollisionControl::from_current(
            0x1000,
            0,
            0,
            0,
            0x2C81,
            0x2DC1,
            DiwHigh::ocs_implicit(),
            0x0038,
            [0; 8],
        );
        let replay = LiveCollisionLineReplay {
            line_start: control,
            segments: Vec::new(),
        };
        let source = LiveSpriteCollisionSource {
            group: 0,
            hstart: 0x81,
            hsub_70ns: false,
            words: [0x8000, 0, 0, 0],
            requires_odd_enable: false,
        };

        assert_eq!(
            live_sprite_playfield_collision_bits_in_range(
                &row,
                &[source],
                &replay,
                &replay,
                RENDER_VISIBLE_START_VPOS as i32,
                display_x - 2,
                display_x,
                Some(0),
            ),
            0
        );
        assert_ne!(
            live_sprite_playfield_collision_bits_in_range(
                &row,
                &[source],
                &replay,
                &replay,
                RENDER_VISIBLE_START_VPOS as i32,
                display_x,
                display_x + 2,
                Some(0),
            ) & (1 << 5),
            0
        );
    }

    #[test]
    fn sprite_sprite_clxdat_waits_for_bpl1dat_display_enable() {
        let control = LiveCollisionControl::from_current(
            0x1000,
            0,
            0,
            0,
            ((RENDER_VISIBLE_START_VPOS as u16) << 8) | RENDER_DIW_HSTART_FB0 as u16,
            ((RENDER_VISIBLE_START_VPOS as u16 + 1) << 8) | 0x00C1,
            DiwHigh::ocs_implicit(),
            0x0038,
            [0; 8],
        );
        let replay = LiveCollisionLineReplay {
            line_start: control,
            segments: Vec::new(),
        };
        let sources = [
            LiveSpriteCollisionSource {
                group: 0,
                hstart: RENDER_DIW_HSTART_FB0,
                hsub_70ns: false,
                words: [0x8000, 0, 0, 0],
                requires_odd_enable: false,
            },
            LiveSpriteCollisionSource {
                group: 1,
                hstart: RENDER_DIW_HSTART_FB0,
                hsub_70ns: false,
                words: [0x8000, 0, 0, 0],
                requires_odd_enable: false,
            },
        ];

        assert_eq!(
            live_sprite_sprite_collision_bits(
                &sources,
                &replay,
                RENDER_VISIBLE_START_VPOS as i32,
                0,
                2,
                None,
            ),
            0
        );
        assert_eq!(
            live_sprite_sprite_collision_bits(
                &sources,
                &replay,
                RENDER_VISIBLE_START_VPOS as i32,
                0,
                2,
                Some(2),
            ),
            0
        );
        assert_eq!(
            live_sprite_sprite_collision_bits(
                &sources,
                &replay,
                RENDER_VISIBLE_START_VPOS as i32,
                0,
                2,
                Some(0),
            ) & (1 << 9),
            1 << 9
        );
    }

    #[test]
    fn brdsprt_bypasses_bpl1dat_display_enable_for_live_sprite_clxdat() {
        let control = LiveCollisionControl::from_current(
            BPLCON0_ECSENA | 0x1000,
            0,
            BPLCON3_BRDSPRT,
            0,
            ((RENDER_VISIBLE_START_VPOS as u16) << 8) | RENDER_DIW_HSTART_FB0 as u16,
            ((RENDER_VISIBLE_START_VPOS as u16 + 1) << 8) | 0x00C1,
            DiwHigh::ocs_implicit(),
            0x0038,
            [0; 8],
        );
        let replay = LiveCollisionLineReplay {
            line_start: control,
            segments: Vec::new(),
        };
        let sources = [
            LiveSpriteCollisionSource {
                group: 0,
                hstart: RENDER_DIW_HSTART_FB0,
                hsub_70ns: false,
                words: [0x8000, 0, 0, 0],
                requires_odd_enable: false,
            },
            LiveSpriteCollisionSource {
                group: 1,
                hstart: RENDER_DIW_HSTART_FB0,
                hsub_70ns: false,
                words: [0x8000, 0, 0, 0],
                requires_odd_enable: false,
            },
        ];

        assert_eq!(
            live_sprite_sprite_collision_bits(
                &sources,
                &replay,
                RENDER_VISIBLE_START_VPOS as i32,
                0,
                2,
                None,
            ) & (1 << 9),
            1 << 9
        );
    }

    #[test]
    fn bpldat_writes_update_latched_planes_for_live_playfield_clxdat() {
        let clxdat_after_row_capture = |bpldat_hpos: Option<u32>| {
            let mut bus = empty_bus();
            bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
            bus.agnus.vpos = 0x2C;
            bus.agnus.hpos = 0x3A;
            bus.denise.diwstrt = 0x2C83;
            bus.denise.diwstop = 0x2DC1;
            bus.denise.ddfstrt = 0x0038;
            bus.denise.ddfstop = 0x0040;
            bus.denise.bplcon0 = 0x7400;
            for plane in 0..4 {
                let ptr = 0x0100 + plane * 0x40;
                bus.denise.bplpt[plane] = ptr as u32;
                bus.display_dma_bplpt[plane] = ptr as u32;
            }
            bus.current_frame_render_base = bus.capture_render_snapshot();
            write_chip_word(&mut bus, 0x0100, 0);
            write_chip_word(&mut bus, 0x0102, 0x8000);

            if let Some(bpldat_hpos) = bpldat_hpos {
                let before_bpldat = bpldat_hpos - bus.agnus.hpos;
                bus.advance_chipset(before_bpldat);
                bus.write_custom_word_from(0x11A, 0x8000, BeamWriteSource::Cpu);
            }
            let remaining = 0x48 - bus.agnus.hpos;
            bus.advance_chipset(remaining);

            bus.custom_read(0x00E, 2)
        };

        assert_eq!(clxdat_after_row_capture(None), 0x8000);
        assert_eq!(clxdat_after_row_capture(Some(0x3E)), 0x8001);
    }

    #[test]
    fn same_line_bplcon0_dual_playfield_enable_does_not_retime_earlier_live_clxdat() {
        let clxdat_after_row_capture = |initial_bplcon0, enable_hpos: Option<u32>| {
            let mut bus = empty_bus();
            bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
            bus.agnus.vpos = 0x2C;
            bus.agnus.hpos = 0x3A;
            bus.denise.diwstrt = 0x2C81;
            bus.denise.diwstop = 0x2DC1;
            bus.denise.ddfstrt = 0x0038;
            bus.denise.ddfstop = 0x0040;
            bus.denise.bplcon0 = initial_bplcon0;
            bus.denise.bplpt[0] = 0x0100;
            bus.denise.bplpt[1] = 0x0200;
            bus.display_dma_bplpt[0] = 0x0100;
            bus.display_dma_bplpt[1] = 0x0200;
            bus.current_frame_render_base = bus.capture_render_snapshot();
            write_chip_word(&mut bus, 0x0100, 0x4000);
            write_chip_word(&mut bus, 0x0102, 0);
            write_chip_word(&mut bus, 0x0200, 0x4000);
            write_chip_word(&mut bus, 0x0202, 0);

            if let Some(enable_hpos) = enable_hpos {
                let before_enable = enable_hpos - bus.agnus.hpos;
                bus.advance_chipset(before_enable);
                bus.write_custom_word_from(0x100, 0x2400, BeamWriteSource::Cpu);
            }
            let remaining = 0x48 - bus.agnus.hpos;
            bus.advance_chipset(remaining);

            bus.custom_read(0x00E, 2)
        };

        assert_eq!(clxdat_after_row_capture(0x2400, None), 0x8001);
        assert_eq!(clxdat_after_row_capture(0x2000, Some(0x40)), 0x8000);
    }

    #[test]
    fn captured_sprite_and_bitplane_rows_accumulate_live_sprite_playfield_clxdat() {
        let mut bus = empty_bus();
        let sprite_ptr = 0x0300usize;
        let (pos, ctl) = sprite_control_words(0x2C, 0x2D, 0x0081);
        write_chip_word(&mut bus, sprite_ptr, pos);
        write_chip_word(&mut bus, sprite_ptr + 2, ctl);
        write_chip_word(&mut bus, sprite_ptr + 4, 0x8000);
        write_chip_word(&mut bus, sprite_ptr + 6, 0);
        write_chip_word(&mut bus, sprite_ptr + 8, 0);
        write_chip_word(&mut bus, sprite_ptr + 10, 0);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_SPREN | DMACON_BPLEN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0038;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.sprpt[0] = sprite_ptr as u32;
        bus.display_dma_sprpt[0] = sprite_ptr as u32;
        bus.denise.bplpt[0] = 0x0100;
        bus.display_dma_bplpt[0] = 0x0100;
        bus.current_frame_sprite_display_enable_x_by_y[0] = Some(0);
        write_chip_word(&mut bus, 0x0100, 0x4000);

        bus.advance_chipset(1);
        assert_eq!(bus.custom_read(0x00E, 2), 0x8000);
        let remaining = 0x40 - bus.agnus.hpos;
        bus.advance_chipset(remaining);

        assert_eq!(bus.custom_read(0x00E, 2), 0x8022);
        assert_eq!(bus.custom_read(0x00E, 2), 0x8000);
    }

    #[test]
    fn explicit_bpl1dat_output_accumulates_live_sprite_playfield_clxdat() {
        let mut bus = empty_bus();
        let sprite_ptr = 0x0300usize;
        let (pos, ctl) = sprite_control_words(0x2C, 0x2D, 0x0083);
        write_chip_word(&mut bus, sprite_ptr, pos);
        write_chip_word(&mut bus, sprite_ptr + 2, ctl);
        write_chip_word(&mut bus, sprite_ptr + 4, 0x8000);
        write_chip_word(&mut bus, sprite_ptr + 6, 0);
        write_chip_word(&mut bus, sprite_ptr + 8, 0);
        write_chip_word(&mut bus, sprite_ptr + 10, 0);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_SPREN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
        bus.denise.diwstrt = 0x2C83;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.sprpt[0] = sprite_ptr as u32;
        bus.display_dma_sprpt[0] = sprite_ptr as u32;
        bus.current_frame_render_base = bus.capture_render_snapshot();

        bus.advance_chipset(2);
        assert_eq!(bus.custom_read(0x00E, 2), 0x8000);

        let before_bpl1dat = 0x38 - bus.agnus.hpos;
        bus.advance_chipset(before_bpl1dat);
        bus.write_custom_word_from(0x110, 0x8000, BeamWriteSource::Cpu);
        bus.advance_chipset(2);

        assert_eq!(bus.custom_read(0x00E, 2), 0x8020);
        assert_eq!(bus.custom_read(0x00E, 2), 0x8000);
    }

    #[test]
    fn manual_sprite_and_bpl1dat_writes_accumulate_live_sprite_playfield_clxdat() {
        let mut bus = empty_bus();
        let (pos, ctl) = sprite_control_words(0x2C, 0x2D, 0x0083);
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = 0x38;
        bus.denise.diwstrt = 0x2C83;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.sprpos[0] = pos;
        bus.denise.sprctl[0] = ctl;
        bus.current_frame_render_base = bus.capture_render_snapshot();

        bus.write_custom_word_from(0x144, 0x8000, BeamWriteSource::Cpu);
        bus.write_custom_word_from(0x110, 0x8000, BeamWriteSource::Cpu);
        bus.advance_chipset(2);

        assert_eq!(bus.custom_read(0x00E, 2), 0x8020);
        assert_eq!(bus.custom_read(0x00E, 2), 0x8000);
    }

    #[test]
    fn same_line_bplcon1_scroll_increase_latches_later_live_sprite_playfield_clxdat() {
        let mut bus = empty_bus();
        let sprite_ptr = 0x0300usize;
        let (pos, ctl) = sprite_control_words(0x2C, 0x2D, 0x0093);
        write_chip_word(&mut bus, sprite_ptr, pos);
        write_chip_word(&mut bus, sprite_ptr + 2, ctl);
        write_chip_word(&mut bus, sprite_ptr + 4, 0x8000);
        write_chip_word(&mut bus, sprite_ptr + 6, 0);
        write_chip_word(&mut bus, sprite_ptr + 8, 0);
        write_chip_word(&mut bus, sprite_ptr + 10, 0);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_SPREN | DMACON_BPLEN;
        bus.agnus.vpos = 0x2C;
        bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
        bus.denise.diwstrt = 0x2C83;
        bus.denise.diwstop = 0x2DC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0038;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.bplcon1 = 0;
        bus.denise.sprpt[0] = sprite_ptr as u32;
        bus.display_dma_sprpt[0] = sprite_ptr as u32;
        bus.denise.bplpt[0] = 0x0100;
        bus.display_dma_bplpt[0] = 0x0100;
        bus.current_frame_render_base = bus.capture_render_snapshot();
        write_chip_word(&mut bus, 0x0100, 0x0001);

        let before_scroll_write = 0x40 - bus.agnus.hpos;
        bus.advance_chipset(before_scroll_write);
        assert_eq!(bus.custom_read(0x00E, 2), 0x8000);

        bus.write_custom_word_from(0x102, 0x0004, BeamWriteSource::Cpu);
        let remaining = 0x48 - bus.agnus.hpos;
        bus.advance_chipset(remaining);

        assert_eq!(bus.custom_read(0x00E, 2), 0x8022);
        assert_eq!(bus.custom_read(0x00E, 2), 0x8000);
    }

    #[test]
    fn same_line_clxcon_odd_sprite_enable_does_not_retime_earlier_live_sprite_playfield_clxdat() {
        let clxdat_after_row_capture = |initial_clxcon, enable_hpos: Option<u32>| {
            let mut bus = empty_bus();
            let sprite_ptr = 0x0300usize;
            let (pos, ctl) = sprite_control_words(0x2C, 0x2D, 0x0083);
            write_chip_word(&mut bus, sprite_ptr, pos);
            write_chip_word(&mut bus, sprite_ptr + 2, ctl);
            write_chip_word(&mut bus, sprite_ptr + 4, 0x8000);
            write_chip_word(&mut bus, sprite_ptr + 6, 0);
            write_chip_word(&mut bus, sprite_ptr + 8, 0);
            write_chip_word(&mut bus, sprite_ptr + 10, 0);

            bus.agnus.dmacon = DMACON_DMAEN | DMACON_SPREN | DMACON_BPLEN;
            bus.agnus.vpos = 0x2C;
            bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
            bus.denise.diwstrt = 0x2C83;
            bus.denise.diwstop = 0x2DC1;
            bus.denise.ddfstrt = 0x0038;
            bus.denise.ddfstop = 0x0040;
            bus.denise.bplcon0 = 0x1000;
            bus.denise.clxcon = initial_clxcon;
            bus.denise.sprpt[1] = sprite_ptr as u32;
            bus.display_dma_sprpt[1] = sprite_ptr as u32;
            bus.denise.bplpt[0] = 0x0100;
            bus.display_dma_bplpt[0] = 0x0100;
            bus.current_frame_sprite_display_enable_x_by_y[0] = Some(0);
            bus.current_frame_render_base = bus.capture_render_snapshot();
            write_chip_word(&mut bus, 0x0100, 0x8000);
            write_chip_word(&mut bus, 0x0102, 0);

            bus.advance_chipset(1);
            assert_eq!(bus.custom_read(0x00E, 2), 0x8000);

            if let Some(enable_hpos) = enable_hpos {
                let before_enable = enable_hpos - bus.agnus.hpos;
                bus.advance_chipset(before_enable);
                bus.write_custom_word_from(0x098, 1 << 12, BeamWriteSource::Cpu);
            }
            let remaining = 0x48 - bus.agnus.hpos;
            bus.advance_chipset(remaining);

            bus.custom_read(0x00E, 2)
        };

        assert_eq!(clxdat_after_row_capture(1 << 12, None), 0x8022);
        assert_eq!(clxdat_after_row_capture(0, Some(0x40)), 0x8000);
    }

    #[test]
    fn bplcon3_spres_hires_narrows_live_sprite_playfield_clxdat() {
        let clxdat_after_bitplane_row_capture = |bplcon3| {
            let mut bus = empty_bus();
            let sprite_ptr = 0x0300usize;
            let (pos, ctl) = sprite_control_words(0x2C, 0x2D, 0x0081);
            write_chip_word(&mut bus, sprite_ptr, pos);
            write_chip_word(&mut bus, sprite_ptr + 2, ctl);
            write_chip_word(&mut bus, sprite_ptr + 4, 0x8000);
            write_chip_word(&mut bus, sprite_ptr + 6, 0);
            write_chip_word(&mut bus, sprite_ptr + 8, 0);
            write_chip_word(&mut bus, sprite_ptr + 10, 0);

            bus.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
            bus.agnus.dmacon = DMACON_DMAEN | DMACON_SPREN | DMACON_BPLEN;
            bus.agnus.vpos = 0x2C;
            bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
            bus.denise.diwstrt = 0x2C81;
            bus.denise.diwstop = 0x2DC1;
            bus.denise.ddfstrt = 0x0038;
            bus.denise.ddfstop = 0x0038;
            // ECSENA/ENBPLCN3 set so the live SPRES write below latches.
            bus.denise.bplcon0 = 0x9000 | BPLCON0_ECSENA;
            bus.denise.bplcon3 = bplcon3;
            bus.denise.sprpt[0] = sprite_ptr as u32;
            bus.display_dma_sprpt[0] = sprite_ptr as u32;
            bus.denise.bplpt[0] = 0x0100;
            bus.display_dma_bplpt[0] = 0x0100;
            write_chip_word(&mut bus, 0x0100, 0x0004);
            write_chip_word(&mut bus, 0x0102, 0);

            bus.advance_chipset(1);
            let remaining = 0x40 - bus.agnus.hpos;
            bus.advance_chipset(remaining);

            bus.custom_read(0x00E, 2)
        };

        assert_eq!(clxdat_after_bitplane_row_capture(0), 0x8022);
        assert_eq!(
            clxdat_after_bitplane_row_capture(BPLCON3_SPRES_HIRES),
            0x8020
        );
    }

    #[test]
    fn same_line_bplcon3_spres_write_does_not_retime_earlier_live_sprite_playfield_clxdat() {
        let clxdat_after_bitplane_row_capture = |spres_hpos: Option<u32>| {
            let mut bus = empty_bus();
            let sprite_ptr = 0x0300usize;
            let (pos, ctl) = sprite_control_words(0x2C, 0x2D, 0x0081);
            write_chip_word(&mut bus, sprite_ptr, pos);
            write_chip_word(&mut bus, sprite_ptr + 2, ctl);
            write_chip_word(&mut bus, sprite_ptr + 4, 0x8000);
            write_chip_word(&mut bus, sprite_ptr + 6, 0);
            write_chip_word(&mut bus, sprite_ptr + 8, 0);
            write_chip_word(&mut bus, sprite_ptr + 10, 0);

            bus.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
            bus.agnus.dmacon = DMACON_DMAEN | DMACON_SPREN | DMACON_BPLEN;
            bus.agnus.vpos = 0x2C;
            bus.agnus.hpos = SPRITE_DMA_PAIR_CAPTURE_HPOS[0] - 1;
            bus.denise.diwstrt = 0x2C81;
            bus.denise.diwstop = 0x2DC1;
            bus.denise.ddfstrt = 0x0038;
            bus.denise.ddfstop = 0x0038;
            // ECSENA/ENBPLCN3 set so the live SPRES write below latches.
            bus.denise.bplcon0 = 0x9000 | BPLCON0_ECSENA;
            bus.denise.bplcon3 = 0;
            bus.denise.sprpt[0] = sprite_ptr as u32;
            bus.display_dma_sprpt[0] = sprite_ptr as u32;
            bus.denise.bplpt[0] = 0x0100;
            bus.display_dma_bplpt[0] = 0x0100;
            bus.current_frame_render_base = bus.capture_render_snapshot();
            write_chip_word(&mut bus, 0x0100, 0x0004);
            write_chip_word(&mut bus, 0x0102, 0);

            bus.advance_chipset(1);
            if let Some(spres_hpos) = spres_hpos {
                let before_spres = spres_hpos - bus.agnus.hpos;
                bus.advance_chipset(before_spres);
                bus.write_custom_word_from(0x106, BPLCON3_SPRES_HIRES, BeamWriteSource::Cpu);
            }
            let remaining = 0x40 - bus.agnus.hpos;
            bus.advance_chipset(remaining);

            bus.custom_read(0x00E, 2)
        };

        assert_eq!(clxdat_after_bitplane_row_capture(None), 0x8022);
        assert_eq!(clxdat_after_bitplane_row_capture(Some(0x38)), 0x8020);
        assert_eq!(clxdat_after_bitplane_row_capture(Some(0x3A)), 0x8022);
    }

    #[test]
    fn bltsize_starts_dma_and_preempts_cpu_slice_without_irq_preempt() {
        let mut bus = empty_bus();
        bus.paula.intena = INT_BLIT;
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BLTEN;
        bus.agnus.hpos = 0x20;
        bus.blitter.bltcon0 = 0x0100;
        bus.begin_cpu_slice();

        let preempt = bus.custom_write(0x058, 2, (1 << 6) | 1);

        assert!(!preempt);
        assert!(bus.slice_preempted);
        assert_eq!(bus.paula.intreq & INT_BLIT, 0);
        assert!(bus.blitter.busy);
        assert_eq!(bus.next_blitter_completion_cck(), Some(6));
        bus.advance_chipset(1);
        assert_eq!(bus.paula.intreq & INT_BLIT, 0);
        assert!(bus.blitter.busy);
        assert_eq!(bus.next_blitter_completion_cck(), Some(5));
        bus.advance_chipset(5);
        assert_ne!(bus.paula.intreq & INT_BLIT, 0);
        assert!(!bus.blitter.busy);
        assert_eq!(bus.next_blitter_completion_cck(), None);
    }

    #[test]
    fn bltsize_stale_blit_clear_rearms_interrupt_recognition_for_next_completion() {
        let mut bus = empty_bus();
        bus.irq_latency_setting = 65;
        bus.paula.intena = INT_MASTER | INT_BLIT;
        bus.paula.intreq = INT_BLIT;
        bus.arm_irq_recognition_latency();
        assert_eq!(bus.irq_latency_last_pending & INT_BLIT, INT_BLIT);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BLTEN;
        bus.agnus.hpos = 0x20;
        bus.blitter.bltcon0 = 0x0100;
        bus.begin_cpu_slice();

        assert!(!bus.custom_write(0x058, 2, (1 << 6) | 1));
        assert_eq!(bus.paula.intreq & INT_BLIT, 0);
        assert_eq!(bus.irq_latency_last_pending & INT_BLIT, 0);
        assert_eq!(bus.irq_latency_mask & INT_BLIT, 0);

        let completion_cck = bus.next_blitter_completion_cck().unwrap();
        bus.advance_chipset(completion_cck);
        assert_ne!(bus.paula.intreq & INT_BLIT, 0);
        assert_eq!(bus.irq_latency_mask & INT_BLIT, INT_BLIT);
        assert_eq!(bus.cpu_visible_intreq() & INT_BLIT, 0);

        bus.advance_chipset(65);
        assert_ne!(bus.cpu_visible_intreq() & INT_BLIT, 0);
    }

    #[test]
    fn busy_blitter_register_writes_finish_current_blit_before_latching_next_state() {
        assert_busy_blitter_register_write_drains_current_blit(0x044, 0x0FF0, |bus| {
            assert_eq!(bus.blitter.bltafwm, 0x0FF0);
        });
        assert_busy_blitter_register_write_drains_current_blit(0x046, 0xF0F0, |bus| {
            assert_eq!(bus.blitter.bltalwm, 0xF0F0);
        });
        assert_busy_blitter_register_write_drains_current_blit(0x04A, 0x0060, |bus| {
            assert_eq!(bus.blitter.bltcpt, 0x0060);
        });
        assert_busy_blitter_register_write_drains_current_blit(0x04E, 0x0070, |bus| {
            assert_eq!(bus.blitter.bltbpt, 0x0070);
        });
        assert_busy_blitter_register_write_drains_current_blit(0x052, 0x0080, |bus| {
            assert_eq!(bus.blitter.bltapt, 0x0080);
        });
        assert_busy_blitter_register_write_drains_current_blit(0x056, 0x0090, |bus| {
            assert_eq!(bus.blitter.bltdpt, 0x0090);
        });
        assert_busy_blitter_register_write_drains_current_blit(0x060, 0x0012, |bus| {
            assert_eq!(bus.blitter.bltcmod, 0x0012);
        });
        assert_busy_blitter_register_write_drains_current_blit(0x062, 0x0022, |bus| {
            assert_eq!(bus.blitter.bltbmod, 0x0022);
        });
        assert_busy_blitter_register_write_drains_current_blit(0x064, 0x0032, |bus| {
            assert_eq!(bus.blitter.bltamod, 0x0032);
        });
        assert_busy_blitter_register_write_drains_current_blit(0x066, 0x0042, |bus| {
            assert_eq!(bus.blitter.bltdmod, 0x0042);
        });
    }

    #[test]
    fn busy_bltcon0_write_disables_remaining_d_output_without_draining_blit() {
        let mut bus = bus_with_pending_two_word_a_to_d_blit();
        bus.advance_chipset(4);
        assert!(bus.blitter.busy);
        assert_eq!(&bus.mem.chip_ram[0x20..0x24], &[0, 0, 0, 0]);
        assert_eq!(bus.paula.intreq & INT_BLIT, 0);

        assert!(!bus.custom_write(0x040, 2, 0x0000));

        assert!(bus.blitter.busy);
        assert_eq!(bus.blitter.bltcon0, 0x0000);
        assert_eq!(&bus.mem.chip_ram[0x20..0x24], &[0, 0, 0, 0]);
        assert_eq!(bus.paula.intreq & INT_BLIT, 0);

        bus.advance_chipset(8);
        assert!(!bus.blitter.busy);
        assert_eq!(&bus.mem.chip_ram[0x20..0x24], &[0, 0, 0, 0]);
        assert_ne!(bus.paula.intreq & INT_BLIT, 0);
    }

    #[test]
    fn busy_bltcon1_line_bit_write_updates_register_without_reinterpreting_pipeline_snapshot() {
        let mut bus = bus_with_pending_two_word_a_to_d_blit();

        assert!(!bus.custom_write(0x042, 2, 0x0001));

        assert!(bus.blitter.busy);
        assert_eq!(bus.blitter.bltcon1, 0x0001);
        assert_eq!(bus.paula.intreq & INT_BLIT, 0);

        bus.advance_chipset(8);
        assert!(!bus.blitter.busy);
        assert_eq!(&bus.mem.chip_ram[0x20..0x24], &[0x11, 0x11, 0x22, 0x22]);
        assert_ne!(bus.paula.intreq & INT_BLIT, 0);
    }

    #[test]
    fn ecs_busy_bltcon0l_write_finishes_current_blit_before_latching_minterm() {
        let mut bus = bus_with_pending_two_word_a_to_d_blit();
        bus.set_agnus_revision(AgnusRevision::Ecs8372Rev4);

        assert!(!bus.custom_write(0x05A, 2, 0x00A5));

        assert!(!bus.blitter.busy);
        assert_eq!(&bus.mem.chip_ram[0x20..0x24], &[0x11, 0x11, 0x22, 0x22]);
        assert_ne!(bus.paula.intreq & INT_BLIT, 0);
        assert_eq!(bus.blitter.bltcon0, 0x09A5);
    }

    #[test]
    fn busy_bltsize_write_finishes_current_blit_then_starts_replacement() {
        let mut bus = bus_with_pending_two_word_a_to_d_blit();

        assert!(!bus.custom_write(0x058, 2, ((1 << 6) | 1) as u64));

        assert!(bus.blitter.busy);
        assert_eq!(&bus.mem.chip_ram[0x20..0x24], &[0x11, 0x11, 0x22, 0x22]);
        // Starting the replacement blit consumes the finished blit's pending
        // interrupt request: INTREQ.BLIT means "the last started blit has
        // finished", and the replacement has not finished yet.
        assert_eq!(bus.paula.intreq & INT_BLIT, 0);
        assert_eq!(bus.next_blitter_completion_cck(), Some(6));

        bus.advance_chipset(6);
        assert!(!bus.blitter.busy);
        assert_eq!(&bus.mem.chip_ram[0x24..0x26], &[0x33, 0x33]);
        // The replacement blit's completion raises the request.
        assert_ne!(bus.paula.intreq & INT_BLIT, 0);
    }

    #[test]
    fn busy_blitter_dmacon_clear_gates_dma_without_finishing_pending_blit() {
        let mut bus = bus_with_pending_two_word_a_to_d_blit();

        assert!(!bus.custom_write(0x096, 2, DMACON_BLTEN as u64));

        assert!(bus.blitter.busy);
        assert_eq!(bus.agnus.dmacon & DMACON_BLTEN, 0);
        assert_eq!(bus.paula.intreq & INT_BLIT, 0);
        assert_eq!(bus.next_blitter_completion_cck(), None);
        assert_ne!(bus.custom_read(0x002, 2) as u16 & (1 << 14), 0);

        bus.advance_chipset(8);
        assert!(bus.blitter.busy);
        assert_eq!(&bus.mem.chip_ram[0x20..0x24], &[0, 0, 0, 0]);

        assert!(!bus.custom_write(0x096, 2, (0x8000 | DMACON_BLTEN) as u64));
        assert_eq!(bus.next_blitter_completion_cck(), Some(8));
        bus.advance_chipset(8);

        assert!(!bus.blitter.busy);
        assert_eq!(&bus.mem.chip_ram[0x20..0x24], &[0x11, 0x11, 0x22, 0x22]);
        assert_ne!(bus.paula.intreq & INT_BLIT, 0);
    }

    #[test]
    fn blithog_clear_busy_blitter_yields_to_cpu_only_after_starvation() {
        // With BLTPRI=0 the blitter is "nice" but still holds the chip bus while
        // it has work: it does not hand the CPU a regular alternate slot. A
        // BLITWAIT-ing CPU is made to wait BLITTER_SLOWDOWN_CPU_MISS_LIMIT cycles
        // (the blitter advancing its scheduled slots) before the blitter yields
        // and the CPU gets its access. This matches real OCS giving a busy
        // blitter roughly 2:1 over the CPU.
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BLTEN;
        bus.agnus.hpos = 0x20;
        // ABC source-only blit: after the lead-in every pipeline phase needs
        // the bus, so this directly exercises the nice-blitter starvation
        // yield rather than the D-output pipeline bubble.
        bus.blitter.bltcon0 = 0x0E00;
        bus.blitter.bltcon1 = 0;
        bus.blitter.bltafwm = 0xFFFF;
        bus.blitter.bltalwm = 0xFFFF;
        bus.blitter.bltapt = 0x10;
        bus.blitter.bltbpt = 0x20;
        bus.blitter.bltcpt = 0x30;
        write_chip_word(&mut bus, 0x10, 0x1111);
        write_chip_word(&mut bus, 0x12, 0x2222);
        write_chip_word(&mut bus, 0x14, 0x3333);
        write_chip_word(&mut bus, 0x16, 0x4444);
        write_chip_word(&mut bus, 0x20, 0xAAAA);
        write_chip_word(&mut bus, 0x22, 0xBBBB);
        write_chip_word(&mut bus, 0x24, 0xCCCC);
        write_chip_word(&mut bus, 0x26, 0xDDDD);
        write_chip_word(&mut bus, 0x30, 0x5555);
        write_chip_word(&mut bus, 0x32, 0x6666);
        write_chip_word(&mut bus, 0x34, 0x7777);
        write_chip_word(&mut bus, 0x36, 0x8888);
        bus.blitter.start_scheduled((1 << 6) | 4, &bus.mem.chip_ram);
        // Walk the blit past its two internal lead-in cycles (those are
        // CPU-available) so its pending slot is an A-channel bus access.
        bus.advance_chipset(2);
        let initial_slots = bus.blitter.scheduled_slots_remaining();
        bus.set_cpu_bus_arbitration_enabled(true);
        bus.begin_cpu_slice();

        let dmaconr = bus.custom_read(0x002, 2) as u16;
        let (poll_cck, poll_tick) = bus.take_slice_bus_advance();
        // The CPU was starved for BLITTER_SLOWDOWN_CPU_MISS_LIMIT cycles, then
        // granted its access (one slot) plus the bus-free tail cck -- one access
        // takes limit + 2 color clocks.
        assert_eq!(poll_cck, u32::from(BLITTER_SLOWDOWN_CPU_MISS_LIMIT) + 2);
        assert_eq!(poll_tick.new_lines, 0);
        assert_ne!(dmaconr & (1 << 14), 0);
        // The blitter kept running through the CPU's wait (it did not yield its
        // regular slots), so it advanced its scheduled work.
        assert!(bus.blitter.scheduled_slots_remaining() < initial_slots);
        assert_eq!(bus.paula.intreq & INT_BLIT, 0);
        assert_eq!(
            bus.agnus.hpos,
            0x22 + u32::from(BLITTER_SLOWDOWN_CPU_MISS_LIMIT) + 2
        );
        // The granted slot was the CPU's; the trailing bus-free tail cck is
        // reclaimed by the still-busy blitter, so it is the last chip-bus owner.
        assert_eq!(bus.last_chip_bus_owner(), ChipBusOwner::Blitter);
    }

    #[test]
    fn blithog_clear_bls_count_yields_blitter_priority_slot_to_cpu() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BLTEN;
        bus.agnus.hpos = 0x22;
        bus.blitter.bltcon0 = 0x09F0;
        bus.blitter.start_scheduled((1 << 6) | 1, &bus.mem.chip_ram);
        // Walk the blit past its two internal lead-in cycles (those are
        // CPU-available) so its pending slot is an A-channel bus access.
        bus.advance_chipset(2);

        assert_eq!(bus.scheduled_dma_owner(true), ChipBusOwner::Blitter);

        bus.blitter_slowdown_cpu_misses = BLITTER_SLOWDOWN_CPU_MISS_LIMIT - 1;
        assert_eq!(bus.scheduled_dma_owner(true), ChipBusOwner::Blitter);

        bus.blitter_slowdown_cpu_misses = BLITTER_SLOWDOWN_CPU_MISS_LIMIT;
        assert_eq!(bus.scheduled_dma_owner(true), ChipBusOwner::Idle);
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Blitter);
    }

    #[test]
    fn bltcon0l_updates_only_minterm_byte() {
        let mut bus = empty_bus();
        bus.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        bus.blitter.bltcon0 = 0xABCD;

        assert!(!bus.custom_write(0x05A, 2, 0x0012));

        assert_eq!(bus.blitter.bltcon0, 0xAB12);
    }

    #[test]
    fn ocs_ignores_ecs_blitter_extension_registers() {
        let mut bus = empty_bus();
        bus.blitter.bltcon0 = 0xABCD;

        assert!(!bus.custom_write(0x05A, 2, 0x0012));
        assert!(!bus.custom_write(0x05C, 2, 0x1234));
        assert!(!bus.custom_write(0x05E, 2, 0x0001));

        assert_eq!(bus.blitter.bltcon0, 0xABCD);
        assert_eq!(bus.blitter.bltsizv, 0);
        assert!(!bus.blitter.busy);
        assert_eq!(bus.custom_read(0x05A, 2), 0);
        assert_eq!(bus.custom_read(0x05C, 2), 0);
    }

    #[test]
    fn ecs_bltcon1_doff_suppresses_destination_writes_but_advances_pointer() {
        let mut bus = empty_bus();
        bus.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BLTEN;
        bus.agnus.hpos = 0x20;
        bus.blitter.bltcon0 = 0x09F0;
        bus.blitter.bltafwm = 0xFFFF;
        bus.blitter.bltalwm = 0xFFFF;
        bus.blitter.bltapt = 0x10;
        bus.blitter.bltdpt = 0x20;
        write_chip_word(&mut bus, 0x10, 0x1234);
        write_chip_word(&mut bus, 0x20, 0xAAAA);

        assert!(!bus.custom_write(0x042, 2, BLTCON1_DOFF as u64));
        assert!(!bus.custom_write(0x058, 2, ((1 << 6) | 1) as u64));

        bus.advance_chipset(2);
        assert!(bus.blitter.busy);
        assert_eq!(&bus.mem.chip_ram[0x20..0x22], &[0xAA, 0xAA]);
        assert_eq!(bus.blitter.bltdpt, 0x20);

        bus.advance_chipset(2);
        assert!(bus.blitter.busy);
        assert!(!bus.blitter.bzero);
        assert_eq!(&bus.mem.chip_ram[0x20..0x22], &[0xAA, 0xAA]);
        assert_eq!(bus.blitter.bltdpt, 0x20);

        bus.advance_chipset(2);

        assert!(!bus.blitter.busy);
        assert_eq!(&bus.mem.chip_ram[0x20..0x22], &[0xAA, 0xAA]);
        assert_eq!(bus.blitter.bltdpt, 0x22);
        assert!(!bus.blitter.bzero);
    }

    #[test]
    fn ocs_masks_ecs_bltcon1_doff_bit() {
        let mut bus = empty_bus();

        assert!(!bus.custom_write(0x042, 2, BLTCON1_DOFF as u64));
        assert_eq!(bus.blitter.bltcon1 & BLTCON1_DOFF, 0);

        bus.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        assert!(!bus.custom_write(0x042, 2, BLTCON1_DOFF as u64));
        assert_eq!(bus.blitter.bltcon1 & BLTCON1_DOFF, BLTCON1_DOFF);
    }

    #[test]
    fn ecs_bltsizv_bltsizh_start_extended_blit() {
        let mut bus = empty_bus();
        bus.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        bus.paula.intena = INT_BLIT;
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BLTEN;
        bus.agnus.hpos = 0x20;
        bus.blitter.bltcon0 = 0x09F0;
        bus.blitter.bltafwm = 0xFFFF;
        bus.blitter.bltalwm = 0xFFFF;
        bus.blitter.bltapt = 0x10;
        bus.blitter.bltdpt = 0x20;
        write_chip_word(&mut bus, 0x10, 0x1234);
        write_chip_word(&mut bus, 0x12, 0x5678);

        assert!(!bus.custom_write(0x05C, 2, 0x0002));
        assert!(!bus.blitter.busy);
        assert!(!bus.custom_write(0x05E, 2, 0x0001));

        assert!(bus.blitter.busy);
        bus.advance_chipset(16);
        assert!(!bus.blitter.busy);
        assert_ne!(bus.paula.intreq & INT_BLIT, 0);
        assert_eq!(&bus.mem.chip_ram[0x20..0x24], &[0x12, 0x34, 0x56, 0x78]);
    }

    #[test]
    fn cpu_chip_access_waits_through_refresh_slots() {
        let mut bus = empty_bus();
        bus.set_cpu_bus_arbitration_enabled(true);
        // Start on a refresh slot (hardware model: refresh occupies the odd
        // color clocks 0x001/0x003/0x005/0x007). The CPU misses that slot and
        // is granted the following free color clock.
        bus.agnus.hpos = 0x003;

        bus.grant_cpu_bus_access(2, CpuBusAccessKind::Read);

        // The CPU waits one cck through the refresh slot (0x003), is granted the
        // following free color clock (0x004 = CPU slot), then spends one bus-free
        // "tail" cck (0x005): wait + slot + tail = three color clocks.
        let (cck, tick) = bus.take_slice_bus_advance();
        assert_eq!(cck, 3);
        assert_eq!(tick.new_lines, 0);
        assert_eq!(bus.agnus.hpos, 0x006);
        assert_eq!(bus.last_chip_bus_owner(), ChipBusOwner::Refresh);
    }

    #[test]
    fn cpu_chip_access_uses_two_color_clocks_slot_plus_bus_free_tail() {
        let mut bus = empty_bus();
        bus.agnus.hpos = 0x21;
        bus.set_cpu_bus_arbitration_enabled(true);

        bus.grant_cpu_bus_access(2, CpuBusAccessKind::Read);

        // A single-word CPU chip access now costs two color clocks: one granted
        // chip-bus slot (the CPU owns hpos 0x21) plus one bus-free "tail" cck
        // (hpos 0x22), modelling the 68000's 4-clock bus cycle. The tail cck is
        // not a CPU bus slot, so the bus is free for whatever the chipset gives
        // (here Idle, since no DMA channel is active).
        let (cck, tick) = bus.take_slice_bus_advance();
        assert_eq!(cck, 2);
        assert_eq!(tick.new_lines, 0);
        assert_eq!(bus.agnus.hpos, 0x23);
        assert_eq!(bus.last_chip_bus_owner(), ChipBusOwner::Idle);
    }

    #[test]
    fn aga_68020_chip_and_custom_reads_use_alice_slot_without_extra_wait() {
        let mut bus = empty_bus();
        bus.set_chipset_revisions(AgnusRevision::AgaAlice, DeniseRevision::AgaLisa);
        bus.set_cpu_clocks_per_cck(4);
        bus.set_cpu_short_bus_cycle(true);
        bus.set_cpu_bus_arbitration_enabled(true);

        bus.agnus.hpos = 0x20;
        bus.grant_cpu_bus_access_at(Some(0x0002_0000), 2, CpuBusAccessKind::Read);
        let (chip_read_cck, _) = bus.take_slice_bus_advance();
        assert_eq!(chip_read_cck, 1);
        assert_eq!(bus.last_chip_bus_owner(), ChipBusOwner::Cpu);

        bus.grant_cpu_bus_access_at(Some(0x0002_0000), 2, CpuBusAccessKind::Write);
        let (chip_write_cck, _) = bus.take_slice_bus_advance();
        assert_eq!(chip_write_cck, 1);
        assert_eq!(bus.last_chip_bus_owner(), ChipBusOwner::Cpu);

        let _ = bus.custom_read(0x002, 2);
        let (custom_read_cck, _) = bus.take_slice_bus_advance();
        assert_eq!(custom_read_cck, 1);
        assert_eq!(bus.last_chip_bus_owner(), ChipBusOwner::Cpu);
    }

    #[test]
    fn ecs_68020_chip_and_custom_reads_wait_for_16_bit_data_return() {
        let mut bus = empty_bus();
        bus.set_chipset_revisions(AgnusRevision::Ecs8375, DeniseRevision::Ecs8373);
        bus.set_cpu_clocks_per_cck(4);
        bus.set_cpu_short_bus_cycle(true);
        bus.set_cpu_bus_arbitration_enabled(true);

        bus.agnus.hpos = 0x20;
        bus.grant_cpu_bus_access_at(Some(0x0002_0000), 2, CpuBusAccessKind::Read);
        let (chip_read_cck, _) = bus.take_slice_bus_advance();
        assert_eq!(chip_read_cck, 2);
        assert_eq!(bus.last_chip_bus_owner(), ChipBusOwner::Idle);

        bus.grant_cpu_bus_access_at(Some(0x0002_0000), 2, CpuBusAccessKind::Write);
        let (chip_write_cck, _) = bus.take_slice_bus_advance();
        assert_eq!(chip_write_cck, 1);
        assert_eq!(bus.last_chip_bus_owner(), ChipBusOwner::Cpu);

        let _ = bus.custom_read(0x002, 2);
        let (custom_read_cck, _) = bus.take_slice_bus_advance();
        assert_eq!(custom_read_cck, 2);
        assert_eq!(bus.last_chip_bus_owner(), ChipBusOwner::Idle);
    }

    #[test]
    fn running_copper_yields_alternate_chip_bus_slot_to_waiting_cpu() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN;
        bus.agnus.hpos = 0x20;
        // A list of back-to-back MOVEs keeps the Copper in the running state
        // so it competes for every contended slot.
        write_chip_word(&mut bus, 0x100, 0x0180); // MOVE COLOR00, $0000
        write_chip_word(&mut bus, 0x102, 0x0000);
        write_chip_word(&mut bus, 0x104, 0x0180);
        write_chip_word(&mut bus, 0x106, 0x0000);
        bus.copper.jump(0x100);
        assert!(bus.copper.is_running());
        bus.set_cpu_bus_arbitration_enabled(true);

        bus.grant_cpu_bus_access(2, CpuBusAccessKind::Read);

        // The Copper takes one slot, then yields the next color clock to the
        // waiting CPU instead of monopolising the bus. The single-word access
        // then costs a third color clock for its bus-free "tail": Copper +
        // CPU slot + tail = three color clocks, not a full Copper run. This
        // models the OCS Copper's 4-color-clock MOVE cadence, which leaves the
        // alternate cycles free for the CPU. The bus-free tail cck is not a CPU
        // slot, so the still-running Copper reclaims the bus on it.
        let (cck, _) = bus.take_slice_bus_advance();
        assert_eq!(cck, 3);
        assert_eq!(bus.last_chip_bus_owner(), ChipBusOwner::Copper);
    }

    // Audit: drive the CPU across one display line under 6-plane lores bitplane
    // DMA and print the per-access contention, to localise the fractional
    // over-charge the timing-test ROM measured (Copperline ~0.25 cck/line/plane high
    // vs FS-UAE/vAmiga). Run with: cargo test audit_six_plane_cpu_contention -- --nocapture
    #[test]
    fn audit_six_plane_cpu_contention() {
        let mut bus = empty_bus();
        bus.set_cpu_bus_arbitration_enabled(true);
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.agnus.vpos = RENDER_VISIBLE_START_VPOS + 20;
        bus.agnus.hpos = 0;
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0x2CC1;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x00D0;
        bus.denise.bplcon0 = 0x6000; // 6 bitplanes, lores
        for i in 0..6 {
            bus.denise.bplpt[i] = 0x0002_0000;
            bus.display_dma_bplpt[i] = 0x0002_0000;
        }
        let line_cck = bus.agnus.current_line_cck();
        let mut total = 0u32;
        let mut accesses = 0u32;
        eprintln!("=== 6-plane lores: CPU access cost by start hpos (DDF $38..$D0) ===");
        while bus.agnus.hpos + 4 < line_cck {
            let h0 = bus.agnus.hpos;
            bus.grant_cpu_bus_access(2, CpuBusAccessKind::Write);
            let owner = bus.last_chip_bus_owner();
            let (cck, _) = bus.take_slice_bus_advance();
            total += cck;
            accesses += 1;
            let in_ddf = (0x38..=0xD7).contains(&h0);
            if in_ddf {
                eprintln!("  h={h0:#04X} cost={cck} last_owner={owner:?}");
            }
        }
        eprintln!("total cck for {accesses} accesses across line = {total}");
    }

    #[test]
    fn copper_move_spends_four_color_clocks_leaving_alternate_cycles_free() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN;
        bus.agnus.hpos = 0x30;
        write_chip_word(&mut bus, 0x100, 0x0180); // MOVE COLOR00, $0ABC
        write_chip_word(&mut bus, 0x102, 0x0ABC);
        write_chip_word(&mut bus, 0x104, 0xFFFF);
        write_chip_word(&mut bus, 0x106, 0xFFFE);
        bus.copper.jump(0x100);

        // A MOVE fetches its two words on alternate color clocks (Copper, free,
        // Copper, free), spanning four color clocks, with the register write on
        // the second fetch and the idle halves left for the blitter/CPU.
        bus.advance_chipset(1);
        assert_eq!(bus.last_chip_bus_owner(), ChipBusOwner::Copper);
        assert_eq!(bus.denise.palette[0], 0);

        bus.advance_chipset(1);
        assert_eq!(bus.last_chip_bus_owner(), ChipBusOwner::Idle);
        assert_eq!(bus.denise.palette[0], 0);

        bus.advance_chipset(1);
        assert_eq!(bus.last_chip_bus_owner(), ChipBusOwner::Copper);
        assert_eq!(bus.denise.palette[0], 0x0ABC);

        bus.advance_chipset(1);
        assert_eq!(bus.last_chip_bus_owner(), ChipBusOwner::Idle);
    }

    #[test]
    fn blitter_completion_prediction_matches_actual_with_running_copper() {
        let mut bus = empty_bus();
        let cop1 = 0x0100usize;
        // A long run of back-to-back MOVEs keeps the Copper contending for the
        // bus the whole time the blitter is running.
        for i in 0..8usize {
            write_chip_word(&mut bus, cop1 + i * 4, 0x0180);
            write_chip_word(&mut bus, cop1 + i * 4 + 2, 0x0000);
        }
        write_chip_word(&mut bus, cop1 + 32, 0xFFFF);
        write_chip_word(&mut bus, cop1 + 34, 0xFFFE);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN | DMACON_BLTEN;
        bus.agnus.hpos = 0x20;
        bus.copper.jump(cop1 as u32);
        bus.blitter.bltcon0 = 0;
        bus.blitter.start_scheduled((1 << 6) | 3, &bus.mem.chip_ram);

        // The predicted completion (which simulates the Copper's cadence on a
        // clone via the shared step primitive) must match when the blitter
        // actually finishes once executed, or wake-up scheduling would drift.
        let predicted = bus.next_blitter_completion_cck().expect("blitter deadline");
        assert!(predicted > 1);
        bus.advance_chipset(predicted - 1);
        assert!(bus.blitter.busy);
        bus.advance_chipset(1);
        assert!(!bus.blitter.busy);
    }

    #[test]
    fn fixed_agnus_dma_slot_bands_drive_owner_selection() {
        let mut bus = empty_bus();

        bus.agnus.dmacon = DMACON_DMAEN;
        bus.agnus.hpos = 0x007;
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Refresh);
        bus.agnus.hpos = 0x008;
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Idle);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_AUD_MASK;
        let _ = bus.paula.advance_audio(0, bus.agnus.dmacon);
        bus.agnus.hpos = 0x00F;
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Audio);
        bus.agnus.hpos = 0x016;
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Idle);

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_SPREN;
        // A sprite's slot is reserved only while that sprite is actually
        // fetching (data_dma_active); a parked sprite frees it. Sprite slots
        // sit on odd color clocks (0x019/0x01B for pair 0).
        bus.agnus.hpos = 0x019;
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Idle);
        bus.display_dma_sprite_state[0].data_dma_active = true;
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Sprite);
        // The even color clock inside the band stays free for the Copper/CPU.
        bus.agnus.hpos = 0x018;
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Idle);
        bus.agnus.hpos = 0x037;
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Idle);
        bus.display_dma_sprite_state[0].data_dma_active = false;

        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x00D0;
        bus.agnus.vpos = 0x40; // inside the default vertical display window
        bus.agnus.hpos = 0x036;
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Idle);
        bus.agnus.hpos = 0x038;
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Idle);
        bus.agnus.hpos = 0x03A;
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Idle);
        bus.agnus.hpos = 0x03E;
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Idle);
        bus.agnus.hpos = 0x03F;
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Bitplane);
        bus.agnus.hpos = 0x040;
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Idle);
        bus.agnus.hpos = 0x0E4;
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Idle);
    }

    #[test]
    fn bitplane_dma_ownership_gated_to_vertical_display_window() {
        // Bitplane DMA only runs inside the vertical display window. The same
        // fetch hpos that the arbiter hands to the bitplane on a display line
        // must be left free (Idle, available to a busy blitter) on a
        // top-border / vertical-blank line. Guards against over-reserving
        // display DMA outside the vertical window. See docs/internals/timing.md.
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x00D0;
        // Explicit vertical display window: lines 0x2C..0xF4.
        bus.denise.diwstrt = 0x2C81;
        bus.denise.diwstop = 0xF4C1;
        bus.agnus.hpos = 0x03F;

        bus.agnus.vpos = 0x40; // inside the window -> bitplane owns the slot
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Bitplane);

        bus.agnus.vpos = 0x10; // top border, before vstart -> slot is free
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Idle);

        bus.agnus.vpos = 0xF8; // past vstop -> slot is free
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Idle);
    }

    #[test]
    fn bitplane_dma_ownership_clips_ddfstart_to_hard_fetch_window() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.ddfstrt = 0x0010;
        bus.denise.ddfstop = 0x0018;
        bus.agnus.vpos = 0x40; // inside the default vertical display window

        bus.agnus.hpos = 0x017;
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Idle);
        bus.agnus.hpos = 0x01F;
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Bitplane);
        bus.agnus.hpos = 0x020;
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Idle);
    }

    #[test]
    fn bitplane_dma_ownership_clips_ddfstop_to_hard_fetch_window() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.denise.bplcon0 = 0x1000;
        bus.denise.ddfstrt = 0x00D8;
        bus.denise.ddfstop = 0x00E0;
        bus.agnus.vpos = 0x40; // inside the default vertical display window

        bus.agnus.hpos = 0x0DF;
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Bitplane);
        bus.agnus.hpos = 0x0E0;
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Idle);
    }

    #[test]
    fn bitplane_dma_ownership_matches_revision_for_equal_ddf_window() {
        let mut ocs = empty_bus();
        ocs.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        ocs.denise.bplcon0 = 0x1000;
        ocs.denise.ddfstrt = 0x0038;
        ocs.denise.ddfstop = 0x0038;
        ocs.agnus.vpos = 0x40; // inside the default vertical display window

        ocs.agnus.hpos = 0x047;
        assert_eq!(ocs.scheduled_dma_owner(false), ChipBusOwner::Bitplane);
        ocs.agnus.hpos = 0x0DF;
        assert_eq!(ocs.scheduled_dma_owner(false), ChipBusOwner::Bitplane);
        ocs.agnus.hpos = 0x0E0;
        assert_eq!(ocs.scheduled_dma_owner(false), ChipBusOwner::Idle);

        let mut ecs = empty_bus();
        ecs.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        ecs.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        ecs.denise.bplcon0 = 0x1000;
        ecs.denise.ddfstrt = 0x0038;
        ecs.denise.ddfstop = 0x0038;
        ecs.agnus.vpos = 0x40; // inside the default vertical display window

        ecs.agnus.hpos = 0x047;
        assert_eq!(ecs.scheduled_dma_owner(false), ChipBusOwner::Idle);
    }

    #[test]
    fn hires_bitplane_dma_ownership_uses_four_cck_fetch_cadence() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BPLEN;
        bus.denise.bplcon0 = 0x9000;
        bus.denise.ddfstrt = 0x0038;
        bus.denise.ddfstop = 0x0040;
        bus.agnus.vpos = 0x40; // inside the default vertical display window

        bus.agnus.hpos = 0x038;
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Bitplane);
        bus.agnus.hpos = 0x03A;
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Idle);
        bus.agnus.hpos = 0x03C;
        assert_eq!(bus.scheduled_dma_owner(false), ChipBusOwner::Bitplane);
    }

    #[test]
    fn bltpri_stalls_cpu_chip_access_through_blitter_access_cycles() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BLTEN | DMACON_BLTPRI;
        bus.agnus.hpos = 0x20;
        bus.blitter.bltcon0 = 0x09F0;
        bus.blitter.bltafwm = 0xFFFF;
        bus.blitter.bltalwm = 0xFFFF;
        bus.blitter.bltapt = 0x10;
        bus.blitter.bltdpt = 0x20;
        bus.mem.chip_ram[0x10] = 0x12;
        bus.mem.chip_ram[0x11] = 0x34;
        bus.blitter.start_scheduled((1 << 6) | 1, &bus.mem.chip_ram);
        // Walk the blit past its two internal lead-in cycles so its pending
        // slot is the A-channel access.
        bus.advance_chipset(2);
        bus.set_cpu_bus_arbitration_enabled(true);

        bus.grant_cpu_bus_access(2, CpuBusAccessKind::Read);

        // With BLTPRI set there is no starvation yield, but the empty D phase
        // after A is an idle pipeline bubble and remains CPU-available. With
        // the bus-free tail cck added after the granted slot the access costs
        // 3 color clocks (A wait + D bubble + tail through E).
        let (cck, _) = bus.take_slice_bus_advance();
        assert_eq!(cck, 3);
        assert!(bus.blitter.busy);
        assert_eq!(bus.paula.intreq & INT_BLIT, 0);
        assert_eq!(&bus.mem.chip_ram[0x20..0x22], &[0x00, 0x00]);

        // The final F slot is still owned by the blitter and writes the queued
        // word once the CPU's bus cycle is over.
        bus.advance_chipset(1);
        assert!(!bus.blitter.busy);
        assert_ne!(bus.paula.intreq & INT_BLIT, 0);
        assert_eq!(&bus.mem.chip_ram[0x20..0x22], &[0x12, 0x34]);
    }

    #[test]
    fn blithog_set_blocks_cpu_slowdown_back_pressure_until_blitter_finishes() {
        let mut bus = empty_bus();
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_BLTEN | DMACON_BLTPRI;
        bus.agnus.hpos = 0x20;
        // Use an ABC source-only blit so there are no idle destination bubbles
        // before completion; BLTPRI should block the nice-blitter starvation
        // yield for the whole remaining source cadence.
        bus.blitter.bltcon0 = 0x0E00;
        bus.blitter.bltafwm = 0xFFFF;
        bus.blitter.bltalwm = 0xFFFF;
        bus.blitter.bltapt = 0x10;
        bus.blitter.bltbpt = 0x20;
        bus.blitter.bltcpt = 0x30;
        write_chip_word(&mut bus, 0x10, 0x1111);
        write_chip_word(&mut bus, 0x12, 0x2222);
        write_chip_word(&mut bus, 0x14, 0x3333);
        write_chip_word(&mut bus, 0x16, 0x4444);
        write_chip_word(&mut bus, 0x20, 0xAAAA);
        write_chip_word(&mut bus, 0x22, 0xBBBB);
        write_chip_word(&mut bus, 0x24, 0xCCCC);
        write_chip_word(&mut bus, 0x26, 0xDDDD);
        write_chip_word(&mut bus, 0x30, 0x5555);
        write_chip_word(&mut bus, 0x32, 0x6666);
        write_chip_word(&mut bus, 0x34, 0x7777);
        write_chip_word(&mut bus, 0x36, 0x8888);
        bus.blitter.start_scheduled((1 << 6) | 4, &bus.mem.chip_ram);
        // Walk the blit past its two internal lead-in cycles so its pending
        // slot is the first A-channel access.
        bus.advance_chipset(2);
        bus.set_cpu_bus_arbitration_enabled(true);

        bus.grant_cpu_bus_access(2, CpuBusAccessKind::Read);

        // With BLTPRI set the CPU gets no starvation yield: it waits through
        // all twelve A/B/C accesses of the four words, then spends its granted
        // slot plus the bus-free tail cck, costing 14 color clocks.
        let (cck, _) = bus.take_slice_bus_advance();
        assert_eq!(cck, 14);
        assert!(!bus.blitter.busy);
        assert_ne!(bus.paula.intreq & INT_BLIT, 0);
    }

    #[test]
    fn front_panel_power_led_follows_cia_a_led_bit() {
        let mut bus = empty_bus();
        assert!(bus.front_panel_status().power_led_on);

        let pra = (REG_PRA as u64) << 8;
        let _ = bus.cia_a_write(pra, 1, 0xC3);
        assert!(!bus.front_panel_status().power_led_on);

        let _ = bus.cia_a_write(pra, 1, 0xC1);
        assert!(bus.front_panel_status().power_led_on);
    }

    #[test]
    fn cd32_pad_serial_protocol_shifts_button_bits() {
        use crate::chipset::cia::REG_DDRA;
        let mut bus = empty_bus();
        bus.input.cd32_pad_port2 = true;
        // Pressed: Red (fire line), Green, Play. Released: Blue, Yellow,
        // FFW, RWD.
        bus.input.lmb_port2 = true;
        bus.input.cd32_green_port2 = true;
        bus.input.cd32_play_port2 = true;

        // lowlevel.library's read: drive /FIR1 as output high, put the
        // pad into serial mode by driving POT1X low through POTGO, then
        // sample POT1Y and clock with /FIR1 falling/rising edges.
        let ddra = (REG_DDRA as u64) << 8;
        let pra = (REG_PRA as u64) << 8;
        let _ = bus.cia_a_write(ddra, 1, 0x80);
        let _ = bus.cia_a_write(pra, 1, 0x80);
        bus.custom_write(0x034, 2, 0x2000); // POTGO: OUTRX, DATRX=0

        // Shift order from 8: Blue, Red, Yellow, Green, FFW, RWD, Play,
        // then the pad-present bit, then zeros. Active low.
        let expected = [
            true,  // 8 Blue released
            false, // 7 Red pressed
            true,  // 6 Yellow released
            false, // 5 Green pressed
            true,  // 4 FFW released
            true,  // 3 RWD released
            false, // 2 Play pressed
            true,  // 1 pad-present
            false, // 0 zeros
            false, // stays zero
        ];
        for (step, want) in expected.iter().enumerate() {
            let potgor = bus.custom_read(0x016, 2) as u16;
            assert_eq!(potgor & (1 << 14) != 0, *want, "serial bit at step {step}");
            let _ = bus.cia_a_write(pra, 1, 0x00); // falling edge: shift
            let _ = bus.cia_a_write(pra, 1, 0x80); // rising edge
        }

        // Leaving serial mode reloads the shifter: Blue again first.
        bus.custom_write(0x034, 2, 0x3000); // POT1X driven high
        let _ = bus.custom_read(0x016, 2);
        bus.custom_write(0x034, 2, 0x2000);
        let potgor = bus.custom_read(0x016, 2) as u16;
        assert!(potgor & (1 << 14) != 0, "Blue (released) after reload");
        // And P5 reads low while in serial mode.
        assert_eq!(potgor & (1 << 12), 0);
    }

    #[test]
    fn front_panel_hdd_led_follows_gayle_activity_with_hold() {
        let mut bus = empty_bus();
        // No Gayle: no HDD LED at all.
        assert_eq!(bus.front_panel_status().hdd_led, None);

        bus.attach_gayle(crate::gayle::Gayle::new(0xD1));
        assert_eq!(bus.front_panel_status().hdd_led, Some(false));

        bus.note_hdd_activity();
        assert_eq!(bus.front_panel_status().hdd_led, Some(true));

        // The hold expires once emulated time passes the deadline.
        bus.emulated_cck += u64::from(PAULA_CLOCK_HZ);
        assert_eq!(bus.front_panel_status().hdd_led, Some(false));
    }

    #[test]
    fn front_panel_reports_host_output_volume() {
        let mut bus = empty_bus();

        assert_eq!(bus.front_panel_status().output_volume_percent, 100);
        bus.set_output_volume_percent(35);
        assert_eq!(bus.front_panel_status().output_volume_percent, 35);
        bus.adjust_output_volume_percent(-50);
        assert_eq!(bus.front_panel_status().output_volume_percent, 0);
        bus.adjust_output_volume_percent(150);
        assert_eq!(bus.front_panel_status().output_volume_percent, 100);
    }

    #[test]
    fn joy0dat_reports_wrapping_mouse_counters() {
        let mut bus = empty_bus();

        bus.input.add_mouse_delta_port1(5, -2);
        assert_eq!(bus.custom_read(0x00A, 2), 0xFE05);

        bus.input.add_mouse_delta_port1(-6, 4);
        assert_eq!(bus.custom_read(0x00A, 2), 0x02FF);
    }

    #[test]
    fn joy1dat_reports_second_port_mouse_counters() {
        let mut bus = empty_bus();

        bus.input.mouse_x_port2 = 0xFF;
        bus.input.mouse_y_port2 = 0x03;

        assert_eq!(bus.custom_read(0x00C, 2), 0x03FF);
    }

    /// The decode an Amiga game (or AmigaTestKit) applies to a JOYxDAT word to
    /// recover digital-joystick directions, per the HRM: right = bit1,
    /// down = bit1 ^ bit0, left = bit9, up = bit9 ^ bit8. The encoding must
    /// round-trip through it.
    fn decode_digital_joydat(joy: u16) -> (bool, bool, bool, bool) {
        let right = joy & 0x0002 != 0;
        let down = (joy ^ (joy >> 1)) & 0x0001 != 0;
        let left = joy & 0x0200 != 0;
        let up = (joy ^ (joy >> 1)) & 0x0100 != 0;
        (up, down, left, right)
    }

    #[test]
    fn digital_joystick_directions_round_trip_through_joydat_on_both_ports() {
        // Every direction combination read back through JOY0DAT/JOY1DAT must
        // decode to the same directions a game would read.
        for bits in 0u8..16 {
            let (up, down, left, right) =
                (bits & 1 != 0, bits & 2 != 0, bits & 4 != 0, bits & 8 != 0);

            let mut bus = empty_bus();
            bus.input.joystick_port1 = true;
            bus.input.joy_up_port1 = up;
            bus.input.joy_down_port1 = down;
            bus.input.joy_left_port1 = left;
            bus.input.joy_right_port1 = right;
            bus.input
                .set_joystick_port2(up, down, left, right, false, false);

            let p1 = bus.custom_read(0x00A, 2) as u16;
            let p2 = bus.custom_read(0x00C, 2) as u16;
            assert_eq!(
                decode_digital_joydat(p1),
                (up, down, left, right),
                "port1 {bits:#04b}"
            );
            assert_eq!(
                decode_digital_joydat(p2),
                (up, down, left, right),
                "port2 {bits:#04b}"
            );
        }
    }

    #[test]
    fn joydat_reports_mouse_until_joystick_engaged() {
        let mut bus = empty_bus();
        bus.input.mouse_x_port2 = 0x12;
        bus.input.mouse_y_port2 = 0x34;
        // Direction lines set but the port is still a mouse: JOY1DAT must keep
        // reporting the quadrature counters.
        bus.input.joy_right_port2 = true;
        assert_eq!(bus.custom_read(0x00C, 2), 0x3412);

        // Engaging the joystick switches JOY1DAT to the direction encoding.
        bus.input
            .set_joystick_port2(false, false, false, true, false, false);
        assert_eq!(
            bus.custom_read(0x00C, 2) as u16,
            super::digital_joydat(false, false, false, true)
        );
    }

    #[test]
    fn joystick_fire_drives_cia_a_pra_fir1() {
        let mut bus = empty_bus();
        // Released: /FIR1 (PRA bit 7) reads high.
        bus.input
            .set_joystick_port2(false, false, false, false, false, false);
        assert_ne!(bus.cia_a_read((REG_PRA as u64) * 256, 1) & 0x80, 0);
        // Pressed: /FIR1 reads low (active-low).
        bus.input
            .set_joystick_port2(false, false, false, false, true, false);
        assert_eq!(bus.cia_a_read((REG_PRA as u64) * 256, 1) & 0x80, 0);
    }

    #[test]
    fn joystick_button2_drives_pot1y_through_potgor() {
        let mut bus = empty_bus();
        // Released: POT1Y (POTGOR bit 14) reads high (input pin, button up).
        bus.input
            .set_joystick_port2(false, false, false, false, false, false);
        assert_ne!(bus.custom_read(0x016, 2) & 0x4000, 0);
        // Pressed: POT1Y reads low.
        bus.input
            .set_joystick_port2(false, false, false, false, false, true);
        assert_eq!(bus.custom_read(0x016, 2) & 0x4000, 0);
    }

    #[test]
    fn fire_buttons_read_through_potgo_pullup_mode() {
        // Software reads fire 2/3 by enabling the pot pin pull-ups and then
        // sampling POTGOR; e.g. AmigaTestKit writes POTGO = 0x0f00 << (port*4),
        // which sets output-enable + data 1 on POT0X/Y (port 1) or POT1X/Y
        // (port 2). A pressed button still pulls its pull-up pin low, so the
        // button must remain visible despite output being enabled.
        let mut bus = empty_bus();

        // Port 2 pull-ups (POT1X bits 12/13, POT1Y bits 14/15).
        assert!(!bus.custom_write(0x034, 2, 0xF000));
        bus.input
            .set_joystick_port2(false, false, false, false, false, false);
        assert_ne!(bus.custom_read(0x016, 2) & 0x4000, 0); // button 2 up -> high
        bus.input
            .set_joystick_port2(false, false, false, false, false, true);
        assert_eq!(bus.custom_read(0x016, 2) & 0x4000, 0); // button 2 down -> low

        // Port 1 pull-ups (POT0X bits 8/9, POT0Y bits 10/11): the right mouse
        // button reads through POT0Y the same way.
        assert!(!bus.custom_write(0x034, 2, 0x0F00));
        bus.input.rmb_port1 = false;
        assert_ne!(bus.custom_read(0x016, 2) & 0x0400, 0); // RMB up -> high
        bus.input.rmb_port1 = true;
        assert_eq!(bus.custom_read(0x016, 2) & 0x0400, 0); // RMB down -> low
    }

    #[test]
    fn vposw_and_vhposw_update_beam_register_reads() {
        let mut bus = empty_bus();

        assert!(!bus.custom_write(0x02A, 2, 0x8001));
        assert!(!bus.custom_write(0x02C, 2, 0x2034));

        assert_eq!(bus.custom_read(0x004, 2), 0x8001);
        assert_eq!(bus.custom_read(0x006, 2), 0x2034);
    }

    #[test]
    fn joytest_sets_both_mouse_counter_pairs() {
        let mut bus = empty_bus();

        assert!(!bus.custom_write(0x036, 2, 0x1234));

        assert_eq!(bus.custom_read(0x00A, 2), 0x1234);
        assert_eq!(bus.custom_read(0x00C, 2), 0x1234);
    }

    #[test]
    fn potgo_starts_counters_and_potgor_reflects_button_pins() {
        let mut bus = empty_bus();
        bus.input.rmb_port1 = true;

        assert!(!bus.custom_write(0x034, 2, 0x0001));
        assert_eq!(bus.next_pot_event_cck(), Some(512));
        assert_eq!(bus.custom_read(0x012, 2), 0);
        bus.advance_devices(512);

        assert_eq!(bus.custom_read(0x012, 2), 0x0101);
        assert_eq!(bus.next_pot_event_cck(), Some(512));
        assert_eq!(bus.custom_read(0x016, 2) & (1 << 10), 0);
    }

    #[test]
    fn clxdat_reads_and_clears_collision_latch() {
        let mut bus = empty_bus();

        bus.denise.or_clxdat(0x1234);

        assert_eq!(bus.custom_read(0x00E, 2), 0x9234);
        assert_eq!(bus.custom_read(0x00E, 2), 0x8000);
    }

    #[test]
    fn denise_write_only_reads_use_zero_bus_approximation() {
        let mut bus = empty_bus();

        assert!(!bus.custom_write(0x098, 2, 0x0A5A));
        assert!(!bus.custom_write(0x104, 2, 0x1234));
        assert!(!bus.custom_write(0x180, 2, u64::from(COLOR_TRANSPARENCY_BIT | 0x0BCD)));
        assert!(!bus.custom_write(0x1BE, 2, 0x0FED));

        assert_eq!(bus.denise.clxcon, 0x0A5A);
        assert_eq!(bus.denise.bplcon2, 0x1234);
        assert_eq!(bus.denise.palette[0], COLOR_TRANSPARENCY_BIT | 0x0BCD);
        assert_eq!(bus.denise.palette[31], 0x0FED);
        assert_eq!(bus.custom_read(0x098, 2), 0);
        assert_eq!(bus.custom_read(0x104, 2), 0);
        assert_eq!(bus.custom_read(0x180, 2), 0);
        assert_eq!(bus.custom_read(0x1BE, 2), 0);
        assert_eq!(bus.custom_read(0x099, 1), 0);
        assert_eq!(bus.custom_read(0x181, 1), 0);
    }

    #[test]
    fn custom_writes_latch_sprite_pointer_and_data_registers() {
        let mut bus = empty_bus();

        assert!(!bus.custom_write(0x120, 2, 0x0002));
        assert!(!bus.custom_write(0x122, 2, 0x3456));
        assert_eq!(bus.denise.sprpt[0], 0x0002_3456);

        assert!(!bus.custom_write(0x140, 2, 0x2C40));
        assert!(!bus.custom_write(0x142, 2, 0x3000));
        assert!(!bus.custom_write(0x144, 2, 0x8000));
        assert!(!bus.custom_write(0x146, 2, 0x0000));
        assert_eq!(bus.denise.sprpos[0], 0x2C40);
        assert_eq!(bus.denise.sprctl[0], 0x3000);
        assert_eq!(bus.denise.sprdata[0], 0x8000);
        assert_eq!(bus.denise.sprdatb[0], 0x0000);
    }

    #[test]
    fn manual_sprite_data_arm_persists_across_frame_start() {
        let mut bus = empty_bus();

        assert!(!bus.custom_write(0x140, 2, 0x2C40));
        assert!(!bus.custom_write(0x142, 2, 0x2D00));
        assert!(!bus.custom_write(0x146, 2, 0x4000));
        assert!(!bus.custom_write(0x144, 2, 0x8000));

        bus.begin_new_beam_frame();

        assert!(bus.denise.spr_armed[0]);
        assert_eq!(bus.denise.sprdata[0], 0x8000);
        assert_eq!(bus.denise.sprdatb[0], 0x4000);
        assert!(bus.current_frame_render_base.spr_armed[0]);
        assert_eq!(bus.current_frame_render_base.sprdata[0], 0x8000);
        assert_eq!(bus.current_frame_render_base.sprdatb[0], 0x4000);
    }

    /// One emulated keyboard byte takes 8 bits x 3 phases x ~20 us.
    const KEYBOARD_BYTE_CCK: u32 = 8 * 3 * 71;

    fn unmask_cia_a_sp(bus: &mut Bus) {
        let icr = (crate::chipset::cia::REG_ICR as u64) << 8;
        let _ = bus.cia_a_write(icr, 1, 0x88); // set SP mask bit
    }

    fn read_cia_a_sdr_and_ack(bus: &mut Bus) -> u8 {
        let sdr = bus.cia_a.read(crate::chipset::cia::REG_SDR);
        bus.cia_a.read(crate::chipset::cia::REG_ICR);
        bus.paula.intreq &= !INT_PORTS;
        sdr
    }

    /// Walk the keyboard MCU through its post-power-on flow at the bus
    /// level: self-test, lone-bit sync, then the $FD/$FE stream, each
    /// handshaked the way keyboard.device does. Leaves the MCU idle.
    fn complete_keyboard_power_up(bus: &mut Bus) {
        unmask_cia_a_sp(bus);
        // Self-test (50 ms) plus the first sync bit.
        bus.advance_devices(180_000);
        keyboard_handshake(bus);
        // $FD then $FE, each handshaked.
        for _ in 0..2 {
            bus.advance_devices(2 * KEYBOARD_BYTE_CCK);
            read_cia_a_sdr_and_ack(bus);
            keyboard_handshake(bus);
        }
        bus.advance_devices(2 * KEYBOARD_BYTE_CCK);
        assert_eq!(bus.next_keyboard_event_cck(), None, "MCU should be idle");
    }

    #[test]
    fn keyboard_power_up_streams_fd_fe_over_the_bit_path() {
        let mut bus = empty_bus();
        unmask_cia_a_sp(&mut bus);
        // Self-test, then the lone sync bit; handshake it.
        bus.advance_devices(180_000);
        keyboard_handshake(&mut bus);
        // $FD ("initiate power-up key stream"), rotated on the wire.
        bus.advance_devices(2 * KEYBOARD_BYTE_CCK);
        assert_eq!(read_cia_a_sdr_and_ack(&mut bus), !0xFDu8.rotate_left(1));
        keyboard_handshake(&mut bus);
        // $FE ("terminate key stream").
        bus.advance_devices(2 * KEYBOARD_BYTE_CCK);
        assert_eq!(read_cia_a_sdr_and_ack(&mut bus), !0xFEu8.rotate_left(1));
    }

    #[test]
    fn keyboard_chord_runs_reset_protocol_and_requests_machine_reset() {
        let mut bus = empty_bus();
        complete_keyboard_power_up(&mut bus);
        bus.enqueue_key_event(0x63, true);
        bus.enqueue_key_event(0x66, true);
        bus.enqueue_key_event(0x67, true);

        // Nobody handshakes the $78 warnings; the keyboard still pulls
        // KCLK low and, 500 ms later, requests the system reset.
        // ($78 byte + 143 ms window + 500 ms hold, with margin.)
        assert!(!bus.keyboard_system_reset_pending);
        bus.advance_devices(4_000_000);
        assert!(bus.keyboard_system_reset_pending);
    }

    #[test]
    fn keyboard_bit_stream_delivers_encoded_transition_to_sdr() {
        let mut bus = empty_bus();
        complete_keyboard_power_up(&mut bus);
        bus.enqueue_key_event(0x01, true);

        // Half a byte: no SP interrupt yet.
        bus.advance_devices(KEYBOARD_BYTE_CCK / 2);
        assert_eq!(bus.paula.intreq & INT_PORTS, 0);
        // The full byte arrives bit by bit over KCLK/KDAT.
        bus.advance_devices(2 * KEYBOARD_BYTE_CCK);
        assert_ne!(bus.paula.intreq & INT_PORTS, 0);
        assert_eq!(bus.cia_a.read(crate::chipset::cia::REG_SDR), 0xFD);
    }

    #[test]
    fn keyboard_second_byte_waits_for_a_timed_handshake() {
        let mut bus = empty_bus();
        complete_keyboard_power_up(&mut bus);
        bus.enqueue_key_event(0x01, true);
        bus.enqueue_key_event(0x02, true);
        bus.advance_devices(2 * KEYBOARD_BYTE_CCK);
        assert_eq!(bus.cia_a.read(crate::chipset::cia::REG_SDR), 0xFD);

        // No handshake: the second byte must not transmit.
        bus.advance_devices(2 * KEYBOARD_BYTE_CCK);
        assert_eq!(bus.cia_a.read(crate::chipset::cia::REG_SDR), 0xFD);

        // A too-short KDAT pulse (~28 us) is not a handshake.
        let cra = (REG_CRA as u64) << 8;
        let _ = bus.cia_a_write(cra, 1, 0x40);
        bus.advance_devices(100);
        let _ = bus.cia_a_write(cra, 1, 0x00);
        bus.advance_devices(2 * KEYBOARD_BYTE_CCK);
        assert_eq!(bus.cia_a.read(crate::chipset::cia::REG_SDR), 0xFD);

        // A proper >= 85 us pulse releases the next byte.
        keyboard_handshake(&mut bus);
        bus.advance_devices(2 * KEYBOARD_BYTE_CCK);
        assert_eq!(bus.cia_a.read(crate::chipset::cia::REG_SDR), 0xFB);
    }

    #[test]
    fn keyboard_burst_requires_a_handshake_between_each_byte() {
        let mut bus = empty_bus();
        complete_keyboard_power_up(&mut bus);
        bus.enqueue_key_event(0x01, true);
        bus.enqueue_key_event(0x02, true);
        bus.enqueue_key_event(0x03, true);

        for expected in [0xFDu8, 0xFB, 0xF9] {
            bus.advance_devices(2 * KEYBOARD_BYTE_CCK);
            assert_eq!(bus.cia_a.read(crate::chipset::cia::REG_SDR), expected);
            keyboard_handshake(&mut bus);
        }
    }

    #[test]
    fn keyboard_event_deadline_caps_idle_fast_forward_while_active() {
        let mut bus = empty_bus();
        // During power-up the MCU always has a deadline.
        assert!(bus.next_keyboard_event_cck().is_some());
        complete_keyboard_power_up(&mut bus);
        assert_eq!(bus.next_keyboard_event_cck(), None);
        bus.enqueue_key_event(0x01, true);
        assert_eq!(bus.next_keyboard_event_cck(), Some(1));
        bus.advance_devices(400);
        // Mid-transmission: the next KCLK edge bounds the fast-forward.
        let deadline = bus.next_keyboard_event_cck().expect("edge deadline");
        assert!(deadline <= 71, "deadline {deadline} cck");
        // After the byte, the resync timeout still provides a deadline.
        bus.advance_devices(2 * KEYBOARD_BYTE_CCK);
        assert!(bus.next_keyboard_event_cck().is_some());
    }

    // ------------------------------------------------------------------
    // Custom-register cross-check (HRM Appendix A/B drift guard).
    //
    // These tests are the machine-checked replacement for the
    // hand-maintained custom-register audit prose: they enumerate the
    // $DFFxxx map as a single source of truth and assert the bus's
    // read/latch/write dispatch matches it. A register that is added,
    // removed, or mis-classified fails here -- not silently in a demo.
    // When a previously-unmodeled register is implemented, the matching
    // table below must be updated, which is exactly the conscious audit
    // step we want to force.
    // ------------------------------------------------------------------

    /// Every CPU-readable custom register (the `read_custom_word` arms),
    /// HRM Appendix A read column. Everything else returns the undriven
    /// custom-bus fallback (0). The 4-channel audio block ($0A0-$0DE) is
    /// dispatched to Paula as well but is handled separately below because
    /// HRM marks those write-only and Copperline returns the latch as an
    /// approximation.
    const CPU_READABLE_CUSTOM_REGS: &[(u16, &str)] = &[
        (0x002, "DMACONR"),
        (0x004, "VPOSR"),
        (0x006, "VHPOSR"),
        (0x008, "DSKDATR"),
        (0x00A, "JOY0DAT"),
        (0x00C, "JOY1DAT"),
        (0x00E, "CLXDAT"),
        (0x010, "ADKCONR"),
        (0x012, "POT0DAT"),
        (0x014, "POT1DAT"),
        (0x016, "POTGOR"),
        (0x018, "SERDATR"),
        (0x01A, "DSKBYTR"),
        (0x01C, "INTENAR"),
        (0x01E, "INTREQR"),
    ];

    fn is_readable_custom_off(off: u16) -> bool {
        CPU_READABLE_CUSTOM_REGS.iter().any(|&(o, _)| o == off)
            // Paula audio block reads back its latches in this model.
            || (0x0A0..=0x0DE).contains(&off)
    }

    #[test]
    fn custom_register_read_map_matches_dispatch() {
        // Drive distinctive state through the write side and read it back
        // through the read side, pinning both halves of the dispatch for a
        // representative readable register from each chip.
        let mut bus = empty_bus();
        bus.custom_write(0x096, 2, u64::from(0x8000 | DMACON_DMAEN)); // DMACON SET
        assert_ne!(
            bus.custom_read(0x002, 2) & u64::from(DMACON_DMAEN),
            0,
            "DMACONR must reflect a DMACON SET write"
        );
        bus.custom_write(0x09E, 2, 0x8000 | 0x0100); // ADKCON SET bit 8
        assert_ne!(bus.custom_read(0x010, 2) & 0x0100, 0, "ADKCONR readback");
        bus.custom_write(0x09A, 2, u64::from(0x8000 | INT_BLIT)); // INTENA SET
        assert_ne!(
            bus.custom_read(0x01C, 2) & u64::from(INT_BLIT),
            0,
            "INTENAR readback"
        );
        bus.custom_write(0x09C, 2, u64::from(0x8000 | INT_BLIT)); // INTREQ SET
        assert_ne!(
            bus.custom_read(0x01E, 2) & u64::from(INT_BLIT),
            0,
            "INTREQR readback"
        );

        // Every offset NOT in the readable map must return the undriven-bus
        // fallback (0). A fresh bus has no driven state, so a stray new read
        // arm (or a removed write-only fallback) is caught here.
        let mut fresh = empty_bus();
        let mut off = 0x000u16;
        while off <= 0x1FE {
            if !is_readable_custom_off(off) {
                assert_eq!(
                    fresh.custom_read(u64::from(off), 2),
                    0,
                    "write-only/unmodeled custom register {off:#05X} must read as the bus fallback"
                );
            }
            off += 2;
        }
    }

    /// ECS-only registers whose byte-write latch (and dispatch) appears
    /// only on ECS Agnus/Denise. The bus gates these on the configured
    /// revision; this list is the single place that fact is asserted.
    const ECS_ONLY_LATCHED_REGS: &[(u16, &str)] = &[
        (0x05A, "BLTCON0L"),
        (0x05C, "BLTSIZV"),
        (0x078, "SPRHDAT"),
        (0x1C0, "HTOTAL"),
        (0x1C2, "HSSTOP"),
        (0x1C4, "HBSTRT"),
        (0x1C6, "HBSTOP"),
        (0x1C8, "VTOTAL"),
        (0x1CA, "VSSTOP"),
        (0x1CC, "VBSTRT"),
        (0x1CE, "VBSTOP"),
        (0x1DC, "BEAMCON0"),
        (0x1DE, "HSSTRT"),
        (0x1E0, "VSSTRT"),
        (0x1E2, "HCENTER"),
        (0x1E4, "DIWHIGH"),
    ];

    #[test]
    fn custom_register_ecs_latches_follow_agnus_revision() {
        // OCS: the ECS-only latches must report "unmodeled" so byte writes
        // do not invent state for registers the chipset does not have.
        let ocs = empty_bus();
        for &(off, name) in ECS_ONLY_LATCHED_REGS {
            assert!(
                ocs.custom_byte_write_latch(off).is_none(),
                "{name} ({off:#05X}) must not latch on OCS"
            );
        }

        // ECS: the same registers gain a byte-write latch.
        let mut ecs = empty_bus();
        ecs.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        for &(off, name) in ECS_ONLY_LATCHED_REGS {
            assert!(
                ecs.custom_byte_write_latch(off).is_some(),
                "{name} ({off:#05X}) must latch on ECS"
            );
        }

        // Revision-independent anchors: a few always-latched registers and a
        // few never-latched offsets, so a wholesale latch-map change is
        // caught regardless of revision.
        for bus in [&ocs, &ecs] {
            // COPCON, BLTCON0, BPLCON0, BPL1PTH, BPL1DAT, SPR0PTH, SPR0POS,
            // COLOR00 -- a spread across the byte-latched register groups.
            for off in [0x02E, 0x040, 0x100, 0x0E0, 0x110, 0x120, 0x140, 0x180] {
                assert!(
                    bus.custom_byte_write_latch(off).is_some(),
                    "always-latched register {off:#05X}"
                );
            }
            // DMACONR (read-only), COPJMP1 and DMACON (strobes): no byte latch.
            for off in [0x002, 0x088, 0x096] {
                assert!(
                    bus.custom_byte_write_latch(off).is_none(),
                    "read-only/strobe register {off:#05X} must not latch"
                );
            }
        }
    }

    #[test]
    fn ecs_scan_registers_latch_only_on_ecs_agnus() {
        // ECS: each programmable sync/blank latch (and the UHRES SPRHDAT)
        // stores the written value, masked to its register width. OCS ignores
        // the write. No scan-rate geometry is derived from them yet.
        let mut ecs = empty_bus();
        ecs.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        ecs.custom_write(0x1DE, 2, 0x1234); // HSSTRT (9-bit)
        ecs.custom_write(0x1C2, 2, 0x1235); // HSSTOP
        ecs.custom_write(0x1C4, 2, 0x1236); // HBSTRT
        ecs.custom_write(0x1C6, 2, 0x1237); // HBSTOP
        ecs.custom_write(0x1E2, 2, 0x1238); // HCENTER
        ecs.custom_write(0x1C8, 2, 0x1244); // VTOTAL (11-bit)
        ecs.custom_write(0x1E0, 2, 0x1245); // VSSTRT
        ecs.custom_write(0x1CA, 2, 0x1246); // VSSTOP
        ecs.custom_write(0x1CC, 2, 0x1247); // VBSTRT
        ecs.custom_write(0x1CE, 2, 0x1248); // VBSTOP
        ecs.custom_write(0x078, 2, 0x1249); // SPRHDAT (raw u16)

        assert_eq!(ecs.agnus.hsstrt(), 0x1234 & 0x01FF);
        assert_eq!(ecs.agnus.hsstop(), 0x1235 & 0x01FF);
        assert_eq!(ecs.agnus.hbstrt(), 0x1236 & 0x01FF);
        assert_eq!(ecs.agnus.hbstop(), 0x1237 & 0x01FF);
        assert_eq!(ecs.agnus.hcenter(), 0x1238 & 0x01FF);
        assert_eq!(ecs.agnus.vtotal(), 0x1244 & 0x07FF);
        assert_eq!(ecs.agnus.vsstrt(), 0x1245 & 0x07FF);
        assert_eq!(ecs.agnus.vsstop(), 0x1246 & 0x07FF);
        assert_eq!(ecs.agnus.vbstrt(), 0x1247 & 0x07FF);
        assert_eq!(ecs.agnus.vbstop(), 0x1248 & 0x07FF);
        assert_eq!(ecs.agnus.sprhdat(), 0x1249);

        // OCS Agnus has none of these; writes are dropped and the fields stay
        // at their reset 0.
        let mut ocs = empty_bus();
        for off in [
            0x1DEu64, 0x1C2, 0x1C4, 0x1C6, 0x1E2, 0x1C8, 0x1E0, 0x1CA, 0x1CC, 0x1CE, 0x078,
        ] {
            ocs.custom_write(off, 2, 0x1FFF);
        }
        assert_eq!(ocs.agnus.hsstrt(), 0);
        assert_eq!(ocs.agnus.hcenter(), 0);
        assert_eq!(ocs.agnus.vtotal(), 0);
        assert_eq!(ocs.agnus.vbstop(), 0);
        assert_eq!(ocs.agnus.sprhdat(), 0);
    }

    #[test]
    fn deniseid_reads_ecs_denise_id_on_ecs_only() {
        // ECS Denise (8373) drives DENISEID = 0xFFFC; the low byte 0xFC is how
        // software detects ECS. OCS Denise has no such register and reads the
        // undriven-bus fallback (0).
        let mut ecs = empty_bus();
        ecs.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        let id = ecs.custom_read(0x07C, 2) as u16;
        assert_eq!(id, 0xFFFC, "ECS Denise DENISEID");
        assert_eq!(id & 0x00FF, 0x00FC, "ECS detection low byte");

        let mut ocs = empty_bus();
        assert_eq!(ocs.custom_read(0x07C, 2), 0, "OCS Denise has no DENISEID");
    }

    /// Plan 1.3: the chip revisions are independent. Late A500s shipped an
    /// ECS Agnus with an OCS Denise; software must see the split ids.
    #[test]
    fn chip_revisions_split_deniseid_from_vposr_id() {
        let mut mixed = empty_bus();
        mixed.set_chipset_revisions(AgnusRevision::Ecs8372Rev4, DeniseRevision::Ocs);
        assert_eq!(mixed.custom_read(0x07C, 2), 0, "OCS Denise stays silent");
        assert_eq!(
            mixed.custom_read(0x004, 2) & 0x7F00,
            0x2000,
            "ECS Agnus VPOSR id"
        );

        let mut mixed = empty_bus();
        mixed.set_chipset_revisions(AgnusRevision::Ocs, DeniseRevision::Ecs8373);
        assert_eq!(mixed.custom_read(0x07C, 2), 0xFFFC, "ECS Denise id");
        assert_eq!(
            mixed.custom_read(0x004, 2) & 0x7F00,
            0x0000,
            "OCS Agnus VPOSR id"
        );
    }

    /// Plan 3.1: AGA identification and register latches, gated on the
    /// Alice/Lisa revisions (not selectable from config until the AGA
    /// display path lands).
    #[test]
    fn aga_ids_and_register_latches_gate_on_alice_lisa() {
        let mut aga = empty_bus();
        aga.set_chipset_revisions(AgnusRevision::AgaAlice, DeniseRevision::AgaLisa);
        assert_eq!(aga.custom_read(0x004, 2) & 0x7F00, 0x2300, "Alice PAL id");
        assert_eq!(aga.custom_read(0x07C, 2), 0x00F8, "Lisa DENISEID");

        assert!(!aga.custom_write(0x1FC, 2, 0xFFFF));
        assert_eq!(aga.agnus.fmode(), 0xC00F, "FMODE defined bits latch");
        assert!(!aga.custom_write(0x10C, 2, 0x1234));
        assert_eq!(aga.denise.bplcon4, 0x1234);
        assert!(!aga.custom_write(0x10E, 2, 0xFFFF));
        assert_eq!(aga.denise.clxcon2, 0x0FFF);

        // ECS machines ignore the AGA registers entirely.
        let mut ecs = empty_bus();
        ecs.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        assert!(!ecs.custom_write(0x1FC, 2, 0xFFFF));
        assert_eq!(ecs.agnus.fmode(), 0);
        assert!(!ecs.custom_write(0x10C, 2, 0x1234));
        assert_eq!(ecs.denise.bplcon4, 0x0011, "BPLCON4 keeps its reset value");
        assert!(!ecs.custom_write(0x10E, 2, 0x0FFF));
        assert_eq!(ecs.denise.clxcon2, 0);
    }

    /// Plan 3.2: Lisa routes COLORxx writes through BPLCON3 BANK/LOCT into
    /// the 256-entry store; bank 0 with LOCT clear stays OCS-compatible.
    #[test]
    fn lisa_palette_writes_follow_bplcon3_bank_and_loct() {
        let mut aga = empty_bus();
        aga.set_chipset_revisions(AgnusRevision::AgaAlice, DeniseRevision::AgaLisa);
        // ENBPLCN3 so BPLCON3 writes latch.
        assert!(!aga.custom_write(0x100, 2, 0x0001));
        assert!(!aga.custom_write(0x106, 2, 1 << 13)); // BANK = 1
        assert!(!aga.custom_write(0x180, 2, 0x0123)); // COLOR00 -> entry 32
        assert_eq!(aga.denise.palette.rgb24(32), 0x0011_2233);
        assert_eq!(aga.denise.palette[0], 0, "bank 0 untouched");

        // LOCT set: low nibbles only.
        assert!(!aga.custom_write(0x106, 2, (1 << 13) | 0x0200));
        assert!(!aga.custom_write(0x180, 2, 0x0FFF));
        assert_eq!(aga.denise.palette.rgb24(32), 0x001F_2F3F);

        // Bank 0, LOCT clear: classic OCS write, visible in the render view.
        assert!(!aga.custom_write(0x106, 2, 0));
        assert!(!aga.custom_write(0x182, 2, 0x0ABC)); // COLOR01
        assert_eq!(aga.denise.palette[1], 0x0ABC);
    }

    #[test]
    fn hhposr_reads_hhposw_latch_on_ecs_agnus_only() {
        let mut ocs = empty_bus();
        assert!(!ocs.custom_write(0x1D8, 2, 0x0155));
        assert_eq!(ocs.custom_read(0x1DA, 2), 0, "no HHPOSR on OCS");

        let mut ecs = empty_bus();
        ecs.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        assert!(!ecs.custom_write(0x1D8, 2, 0x0155));
        assert_eq!(ecs.custom_read(0x1DA, 2), 0x0155);
    }

    #[test]
    fn gayle_int2_level_keeps_setting_paula_ports_intreq() {
        use crate::gayle::{Gayle, IdeDrive};

        let image =
            std::env::temp_dir().join(format!("copperline-bus-gayle-{}.hdf", std::process::id()));
        std::fs::write(&image, vec![0u8; 512 * 16]).unwrap();

        let mut bus = empty_bus();
        let mut gayle = Gayle::new(0xD0);
        gayle.attach_drive(0, IdeDrive::open(&image, 0).unwrap());
        bus.attach_gayle(gayle);

        // Enable the IDE interrupt at $DAA000 and issue a READ SECTORS via
        // the memory-mapped interface.
        let g = bus.gayle.as_mut().unwrap();
        g.write(0x00DA_A000, 1, 0x80); // INTENA.IDE
        g.write(0x00DA_2018, 1, 0x40); // LBA, drive 0
        g.write(0x00DA_200C, 1, 0); // LBA 0
        g.write(0x00DA_2010, 1, 0);
        g.write(0x00DA_2014, 1, 0);
        g.write(0x00DA_2008, 1, 1); // one sector
        g.write(0x00DA_201C, 1, 0x20); // READ SECTORS
        assert!(bus.gayle.as_ref().unwrap().int2_line());

        // The timed-device tick re-latches INTREQ.PORTS while the line holds.
        bus.advance_devices(4);
        assert_ne!(bus.paula.intreq & INT_PORTS, 0);
        bus.paula.intreq &= !INT_PORTS;
        bus.advance_devices(4);
        assert_ne!(
            bus.paula.intreq & INT_PORTS,
            0,
            "level interrupt re-latches after a clear"
        );

        // Acknowledging at Gayle (write-to-clear) drops the line.
        let g = bus.gayle.as_mut().unwrap();
        g.write(0x00DA_9000, 1, 0x7F);
        assert!(!bus.gayle.as_ref().unwrap().int2_line());
        bus.paula.intreq &= !INT_PORTS;
        bus.advance_devices(4);
        assert_eq!(bus.paula.intreq & INT_PORTS, 0);
        std::fs::remove_file(image).ok();
    }

    #[test]
    fn bplcon0_lpen_enables_light_pen_latch_via_bus() {
        let mut bus = empty_bus();
        assert!(!bus.custom_write(0x100, 2, 0x0008));
        bus.advance_chipset(5 * 227 + 0x40);
        bus.light_pen_pulse();
        bus.advance_chipset(20);
        assert_eq!(bus.custom_read(0x006, 2), (5 << 8) | (0x40 >> 1));
    }

    #[test]
    fn custom_register_space_sweep_is_panic_free_and_unused_offsets_inert() {
        // Sweeping every register with byte/word/long reads and writes on
        // both revisions must never panic -- the dispatch has total coverage
        // (an explicit arm or the silent `_ => false` / `=> 0` fallbacks).
        for ecs in [false, true] {
            let mut bus = empty_bus();
            if ecs {
                bus.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
            }
            let mut off = 0x000u16;
            while off <= 0x1FE {
                let addr = u64::from(off);
                bus.custom_write(addr, 2, 0xFFFF);
                bus.custom_write(addr, 1, 0xAA);
                bus.custom_write(addr | 1, 1, 0x55);
                bus.custom_write(addr, 4, 0x1234_5678);
                let _ = bus.custom_read(addr, 1);
                let _ = bus.custom_read(addr, 2);
                let _ = bus.custom_read(addr, 4);
                off += 2;
            }
        }

        // Offsets with no modelled register stay inert: a write leaves no
        // readable state behind. Catches a reserved offset accidentally
        // gaining a read arm or a stored latch.
        let mut bus = empty_bus();
        for off in [0x1F0u16, 0x1F2, 0x1F4, 0x1F6, 0x1F8, 0x1FA, 0x1FC, 0x1FE] {
            assert!(bus.custom_byte_write_latch(off).is_none());
            bus.custom_write(u64::from(off), 2, 0xFFFF);
            assert_eq!(
                bus.custom_read(u64::from(off), 2),
                0,
                "unused custom offset {off:#05X} must stay inert"
            );
        }
    }
}
