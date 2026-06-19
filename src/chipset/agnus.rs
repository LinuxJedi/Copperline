// SPDX-License-Identifier: GPL-3.0-or-later

//! Agnus state we care about: the raster beam position counters and
//! DMACON. VPOSR/VHPOSR are read frequently by ROM polling loops; if
//! they never change, DiagROM hangs in its delay routines.

pub const PAL_LINES: u32 = 313;
pub const NTSC_LINES: u32 = 263;
pub const COLORCLOCKS_PER_LINE: u32 = 227;
pub const NTSC_LONG_COLORCLOCKS_PER_LINE: u32 = 228;
pub const DMACON_BPLEN: u16 = 1 << 8;
pub const DMACON_DMAEN: u16 = 1 << 9;
// BEAMCON0 ($DFF1DC) control bits (ECS Agnus). PAL, VARBEAMEN, LOLDIS,
// HARDDIS, DUAL, LPENDIS, VARVBEN, and BLANKEN are interpreted; the
// sync-shape bits (VARHSYEN/VARVSYEN/VARCSYEN/CSCBEN and the xSYTRUE
// polarities) affect monitor sync pulses only, which the emulated display
// always locks to, so they are decoded for completeness.
#[allow(dead_code)]
pub const BEAMCON0_BLANKEN: u16 = 1 << 3;
pub const BEAMCON0_PAL: u16 = 1 << 5;
pub const BEAMCON0_DUAL: u16 = 1 << 6;
pub const BEAMCON0_VARBEAMEN: u16 = 1 << 7;
#[allow(dead_code)]
pub const BEAMCON0_VARHSYEN: u16 = 1 << 8;
#[allow(dead_code)]
pub const BEAMCON0_VARVSYEN: u16 = 1 << 9;
#[allow(dead_code)]
pub const BEAMCON0_CSCBEN: u16 = 1 << 10;
pub const BEAMCON0_LOLDIS: u16 = 1 << 11;
#[allow(dead_code)]
pub const BEAMCON0_VARVBEN: u16 = 1 << 12;
#[allow(dead_code)]
pub const BEAMCON0_LPENDIS: u16 = 1 << 13;
pub const BEAMCON0_HARDDIS: u16 = 1 << 14;

const BITPLANE_DDF_HARD_START: u16 = 0x0018;
const BITPLANE_DDF_HARD_STOP: u16 = 0x00D8;
/// With BEAMCON0.HARDDIS set, the hardwired DDF stop ceiling is relaxed from
/// 0xD8 to 0xE0 -- the widest fetch that still lands inside the 704px (hpos
/// 0x30..0xE0) framebuffer. The start floor is left at 0x18.
const BITPLANE_DDF_HARD_STOP_RELAXED: u16 = 0x00E0;

/// Resolve the (start_floor, stop_ceiling) DDF hard-window bounds, honoring
/// BEAMCON0.HARDDIS. Single source of truth shared by every effective-DDF
/// window helper (agnus, bus, bitplane) so they cannot drift.
pub fn ddf_hard_bounds(harddis: bool) -> (u16, u16) {
    (
        BITPLANE_DDF_HARD_START,
        if harddis {
            BITPLANE_DDF_HARD_STOP_RELAXED
        } else {
            BITPLANE_DDF_HARD_STOP
        },
    )
}
const DEFAULT_HTOTAL: u16 = (COLORCLOCKS_PER_LINE - 1) as u16;

#[cfg_attr(not(test), allow(dead_code))]
const OCS_LORES_BPL_SEQUENCE: [usize; 8] = [8, 4, 6, 2, 7, 3, 5, 1];

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum VideoStandard {
    Pal,
    Ntsc,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AgnusRevision {
    #[default]
    Ocs,
    Ecs8372Rev4,
    /// The 2 MB ECS Agnus (8375, aka 8372B / "8372 rev 5" in the HRM
    /// identification table).
    Ecs8375,
    /// AGA Alice (8374 rev 3/4).
    AgaAlice,
}

impl AgnusRevision {
    fn chipset_id(self, video_standard: VideoStandard) -> u16 {
        match (self, video_standard) {
            (Self::Ocs, _) => 0x00,
            (Self::Ecs8372Rev4, VideoStandard::Pal) => 0x20,
            (Self::Ecs8372Rev4, VideoStandard::Ntsc) => 0x30,
            // HRM table: "8372 (Fat-hr) rev 5" = $22 PAL / $31 NTSC.
            (Self::Ecs8375, VideoStandard::Pal) => 0x22,
            (Self::Ecs8375, VideoStandard::Ntsc) => 0x31,
            // HRM table: "8374 (Alice) rev 3 thru rev 4" = $23 PAL / $33 NTSC.
            (Self::AgaAlice, VideoStandard::Pal) => 0x23,
            (Self::AgaAlice, VideoStandard::Ntsc) => 0x33,
        }
    }

    /// ECS Agnus behaviour (BEAMCON0, DIWHIGH, ECS blitter) is a subset of
    /// Alice's, so AGA reports true here.
    pub fn is_ecs(self) -> bool {
        !matches!(self, Self::Ocs)
    }

    /// Highest chip address this Agnus can drive: the writable DMA pointer
    /// high bits follow the chip's address bus, not the installed RAM.
    /// OCS (8370/8371) stops at 512 KiB, the 8372A at 1 MiB, and the
    /// 8375/8372B and Alice at 2 MiB.
    pub fn dma_addr_capability_mask(self) -> u32 {
        match self {
            Self::Ocs => 0x0007_FFFF,
            Self::Ecs8372Rev4 => 0x000F_FFFF,
            Self::Ecs8375 | Self::AgaAlice => 0x001F_FFFF,
        }
    }
}

impl VideoStandard {
    fn short_frame_lines(self) -> u32 {
        match self {
            Self::Pal => PAL_LINES - 1,
            Self::Ntsc => NTSC_LINES - 1,
        }
    }

    fn long_frame_lines(self) -> u32 {
        match self {
            Self::Pal => PAL_LINES,
            Self::Ntsc => NTSC_LINES,
        }
    }
}

fn beamcon0_reset_value(video_standard: VideoStandard) -> u16 {
    match video_standard {
        VideoStandard::Pal => 1 << 5,
        VideoStandard::Ntsc => 0,
    }
}

/// Beam-counter snapshot taken when BPLCON0.ERSY is set with no genlock:
/// VPOSR/VHPOSR read these frozen values until ERSY is cleared.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
struct ErsyFreeze {
    vpos: u32,
    hpos: u32,
    lof: bool,
    lol: bool,
}

#[derive(Default, Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct AgnusTick {
    pub new_lines: u32,
    pub new_frames: u32,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Agnus {
    pub vpos: u32,
    pub hpos: u32,
    pub dmacon: u16,
    pub copcon: u16,
    pub lof: bool,
    pub lol: bool,
    /// Copper list 1 address. Programs write the halves via COP1LCH
    /// ($DFF080) and COP1LCL ($DFF082), then trigger with COPJMP1.
    pub cop1lc: u32,
    /// Copper list 2 address. System copper lists can jump here with
    /// COPJMP2; boot ROM display code uses this path for the insert-disk view.
    pub cop2lc: u32,
    /// Color clocks consumed via nudge_hpos that should be deducted
    /// from the next call to advance_by_cck() so total emulated time stays
    /// proportional to instructions executed.
    nudge_cck: u32,
    /// Line/frame crossings accumulated by both nudge_hpos and
    /// advance_by_cck(); returned and reset by advance_by_cck().
    pending_tick: AgnusTick,
    video_standard: VideoStandard,
    revision: AgnusRevision,
    lace: bool,
    beamcon0: u16,
    htotal: u16,
    // ECS Agnus programmable horizontal/vertical sync/blank latches plus the
    // UHRES sprite identifier. All are latched (ECS-gated); the blank windows
    // (HBSTRT/HBSTOP under BLANKEN, VBSTRT/VBSTOP under VARVBEN) and the
    // VARBEAMEN totals are interpreted, while the sync-position latches only
    // affect monitor sync pulses the emulated display always locks to.
    hsstrt: u16,
    hsstop: u16,
    hbstrt: u16,
    hbstop: u16,
    hcenter: u16,
    vtotal: u16,
    vsstrt: u16,
    vsstop: u16,
    vbstrt: u16,
    vbstop: u16,
    sprhdat: u16,
    dma_addr_mask: u32,
    vpos_read_delay: Option<u32>,
    /// BPLCON0.LPEN (bit 3), mirrored in from the bus on BPLCON0 writes:
    /// BPLCON0.ERSY (bit 1) turns the HSYNC/VSYNC pins into inputs for
    /// genlock resynchronization. With no genlock driving the pins the beam
    /// counters stop, so VPOSR/VHPOSR freeze at the position where ERSY was
    /// set. The boot ROM genlock probe relies on this: it sets ERSY,
    /// reads VHPOSR twice and concludes a genlock is present if the counter
    /// still advances (which would wrongly hide the non-genlock programmable
    /// 31 kHz display modes). Copperline has no genlock, so
    /// the readback is frozen while ERSY is set; the internal timing chain
    /// keeps running. TODO: a full model would halt the sync generators and
    /// every beam-derived process, not just the CPU-visible counters.
    ersy_freeze: Option<ErsyFreeze>,
    /// the light-pen beam latch lives in Agnus but its enable is a Denise
    /// register bit Agnus snoops off the shared register bus.
    lpen_enabled: bool,
    /// Latched (vpos, hpos) the moment the light-pen pulse arrived. While
    /// LPEN is enabled and a latch is held, VPOSR/VHPOSR read the latch
    /// instead of the live beam.
    lpen_latch: Option<(u32, u32)>,
    /// Whether a pen pulse latched during the current field; re-armed at
    /// every field wrap.
    lpen_triggered_this_field: bool,
    /// HHPOSW latch (ECS). The UHRES dual-mode horizontal counter itself is
    /// not emulated (BEAMCON0.DUAL logs a one-time warning), so HHPOSR reads
    /// back the last written value.
    hhpos: u16,
    /// AGA FMODE ($1FC): bitplane/sprite fetch width. Latched only
    /// (Alice-gated); the wide-fetch DMA interpretation lands with plan 3.3
    /// and must keep FMODE=0 byte-identical to the OCS/ECS slot timing.
    fmode: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub struct BitplaneDmaFetchConfig {
    pub revision: AgnusRevision,
    pub dmacon: u16,
    pub bplcon0: u16,
    /// AGA FMODE latch; 0 on OCS/ECS.
    pub fmode: u16,
    pub ddfstrt: u16,
    pub ddfstop: u16,
    /// Bitplane pointer state at `old_hpos`; callers that plan a partial
    /// scanline range should pass pointers already advanced by earlier slots.
    pub bplpt: [u32; 8],
    pub bpl1mod: i16,
    pub bpl2mod: i16,
    pub addr_mask: u32,
    /// BEAMCON0.HARDDIS active: relax the hardwired DDF stop ceiling.
    pub harddis: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub struct BitplaneDmaFetchSlot {
    pub hpos: u32,
    pub order: u32,
    pub word_idx: usize,
    pub plane: usize,
    pub addr: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub struct BitplaneDmaFetchPlan {
    pub dma_planes: usize,
    pub words_per_row: usize,
    pub slots: Vec<BitplaneDmaFetchSlot>,
    pub ptrs_after_range: [u32; 8],
    pub line_complete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
struct BitplaneDmaFetchTiming {
    hpos: u32,
    order: u32,
    word_idx: usize,
    plane: usize,
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn plan_bitplane_dma_fetches(
    config: BitplaneDmaFetchConfig,
    old_hpos: u32,
    new_hpos: u32,
) -> Option<BitplaneDmaFetchPlan> {
    if config.dmacon & (DMACON_DMAEN | DMACON_BPLEN) != (DMACON_DMAEN | DMACON_BPLEN) {
        return None;
    }
    let dma_planes = bitplane_dma_planes(
        config.bplcon0,
        matches!(config.revision, AgnusRevision::AgaAlice),
    );
    if dma_planes == 0 {
        return None;
    }
    let (ddfstart, _ddfstop) = effective_bitplane_ddf_window(
        config.revision,
        config.bplcon0,
        config.ddfstrt,
        config.ddfstop,
        config.harddis,
    )?;
    let ddfstart = u32::from(anchor_bitplane_fetch_start(
        ddfstart,
        bitplane_fetch_unit(config.bplcon0, config.fmode),
    ));
    if new_hpos <= ddfstart {
        return None;
    }

    let words_per_row = bitplane_words_per_row(
        config.revision,
        config.bplcon0,
        config.fmode,
        config.ddfstrt,
        config.ddfstop,
        config.harddis,
    );
    let mut ptrs = config.bplpt;
    let mut slots = Vec::new();
    let timings = bitplane_dma_fetch_timings(
        config.bplcon0,
        config.fmode,
        ddfstart,
        words_per_row,
        dma_planes,
    );
    let last_fetch = timings.last().copied();
    let mut line_complete = false;

    for timing in timings {
        if timing.hpos < old_hpos {
            continue;
        }
        if timing.hpos >= new_hpos {
            break;
        }
        let plane = timing.plane;
        slots.push(BitplaneDmaFetchSlot {
            hpos: timing.hpos,
            order: timing.order,
            word_idx: timing.word_idx,
            plane,
            addr: ptrs[plane] & config.addr_mask,
        });
        ptrs[plane] = ptrs[plane].wrapping_add(2) & config.addr_mask;
        if Some(timing) == last_fetch {
            line_complete = true;
        }
    }

    if line_complete {
        apply_bitplane_modulos(
            &mut ptrs,
            dma_planes,
            config.bpl1mod,
            config.bpl2mod,
            config.addr_mask,
        );
    }

    Some(BitplaneDmaFetchPlan {
        dma_planes,
        words_per_row,
        slots,
        ptrs_after_range: ptrs,
        line_complete,
    })
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn bitplane_words_per_row(
    revision: AgnusRevision,
    bplcon0: u16,
    fmode: u16,
    ddfstrt: u16,
    ddfstop: u16,
    harddis: bool,
) -> usize {
    let fallback = if bitplane_shres(bplcon0) {
        1280
    } else if bitplane_hires(bplcon0) {
        640
    } else {
        320
    } / 16;
    let Some((start, stop)) =
        effective_bitplane_ddf_window(revision, bplcon0, ddfstrt, ddfstop, harddis)
    else {
        return fallback;
    };
    let unit = bitplane_fetch_unit(bplcon0, fmode);
    let start = anchor_bitplane_fetch_start(start, unit);
    let blocks = bitplane_fetch_blocks(u32::from(stop - start), unit);
    let words = blocks * (unit / bitplane_fetch_cck_per_word(bplcon0)) as usize;
    words.max(1)
}

/// Whole fetch units run for a DDF window spanning `rel` colour clocks.
/// A DDFSTOP inside a unit stops the fetch only after the unit starting
/// at-or-after it completes, so the partial tail rounds up (CDTV trademark
/// screen, hires $64/$A8 with $28 modulos: 10 units = 20 words/row;
/// truncating fetched 18 and sheared every row). Wide-FMODE units use the
/// same rule; their 16/32-cck unit is longer, but the active lores plane
/// slots are still packed into the first eight cycles of the unit.
pub fn bitplane_fetch_blocks(rel: u32, unit: u32) -> usize {
    (rel.div_ceil(unit) + 1) as usize
}

/// Bitplane fetch starts from the DDFSTRT comparator. The wide-FMODE unit
/// length controls how far the sequencer runs before the next group, but it
/// does not move the first active group back to an absolute unit boundary.
pub fn anchor_bitplane_fetch_start(start: u16, unit: u32) -> u16 {
    let _ = unit;
    start
}

fn bitplane_shres(bplcon0: u16) -> bool {
    bplcon0 & 0x0040 != 0
}

fn bitplane_hires(bplcon0: u16) -> bool {
    bplcon0 & 0x8000 != 0 && !bitplane_shres(bplcon0)
}

fn bitplane_hires_like_ddf(bplcon0: u16) -> bool {
    bitplane_hires(bplcon0) || bitplane_shres(bplcon0)
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

/// AGA FMODE: 16-bit words moved per bitplane fetch (BPL32/BPAGEM).
pub fn bitplane_fetch_quantum(fmode: u16) -> u32 {
    match fmode & 0x0003 {
        0 => 1,
        3 => 4,
        _ => 2,
    }
}

/// Colour clocks between successive fetches of one plane.
fn bitplane_fetch_period(bplcon0: u16, fmode: u16) -> u32 {
    bitplane_fetch_cck_per_word(bplcon0) * bitplane_fetch_quantum(fmode)
}

/// The DDF block quantum in colour clocks: the fetch sequencer always
/// completes a whole unit. 8 cck at FMODE=0 (the classic block), growing
/// with the fetch width when the per-plane period exceeds it (lores 16/32,
/// hires 16 at FMODE=3).
fn bitplane_fetch_unit(bplcon0: u16, fmode: u16) -> u32 {
    bitplane_fetch_period(bplcon0, fmode).max(8)
}

pub fn effective_bitplane_ddf_start_hpos(bplcon0: u16, raw: u16) -> u16 {
    if bitplane_hires_like_ddf(bplcon0) {
        raw & 0x00FC
    } else {
        raw & 0x00F8
    }
}

pub fn effective_bitplane_ddf_stop_hpos(_bplcon0: u16, raw: u16) -> u16 {
    raw & 0x00FC
}

pub fn effective_bitplane_ddf_window(
    revision: AgnusRevision,
    bplcon0: u16,
    ddfstrt: u16,
    ddfstop: u16,
    harddis: bool,
) -> Option<(u16, u16)> {
    let (hard_start, hard_stop) = ddf_hard_bounds(harddis);
    let start = effective_bitplane_ddf_start_hpos(bplcon0, ddfstrt);
    let mut stop = effective_bitplane_ddf_stop_hpos(bplcon0, ddfstop);
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

pub fn sprite_dma_disabled_by_bitplane_ddf(
    sprite: usize,
    revision: AgnusRevision,
    bplcon0: u16,
    fmode: u16,
    dmacon: u16,
    ddfstrt: u16,
    ddfstop: u16,
    harddis: bool,
) -> bool {
    if sprite != 7 || dmacon & (DMACON_DMAEN | DMACON_BPLEN) != (DMACON_DMAEN | DMACON_BPLEN) {
        return false;
    }
    if bitplane_dma_planes(bplcon0, matches!(revision, AgnusRevision::AgaAlice)) == 0 {
        return false;
    }
    let Some((ddfstart, ddfstop)) =
        effective_bitplane_ddf_window(revision, bplcon0, ddfstrt, ddfstop, harddis)
    else {
        return false;
    };
    let unit = bitplane_fetch_unit(bplcon0, fmode);
    let ddfstart = u32::from(anchor_bitplane_fetch_start(ddfstart, unit));
    let blocks = bitplane_fetch_blocks(u32::from(ddfstop) - ddfstart, unit) as u32;
    let last_block_start = ddfstart + blocks.saturating_sub(1) * unit;
    let sprite7_block_start = 0x0030;
    ddfstart <= sprite7_block_start
        && last_block_start >= sprite7_block_start
        && (sprite7_block_start - ddfstart).is_multiple_of(unit)
}

pub fn bitplane_dma_planes(bplcon0: u16, aga: bool) -> usize {
    let code = ((bplcon0 >> 12) & 0x0007) as usize;
    if aga {
        // Alice: BPU3 (bit 4) requests 8 planes, overriding BPU2-0; the
        // OCS lowres BPU=7 overfetch quirk does not apply.
        return if bplcon0 & 0x0010 != 0 { 8 } else { code };
    }
    let display_planes = if bitplane_shres(bplcon0) {
        code.min(2)
    } else {
        code.min(6)
    };
    if code == 7 && !bitplane_hires(bplcon0) && !bitplane_shres(bplcon0) {
        4
    } else {
        display_planes
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn bitplane_dma_fetch_timings(
    bplcon0: u16,
    fmode: u16,
    ddfstart: u32,
    words_per_row: usize,
    dma_planes: usize,
) -> Vec<BitplaneDmaFetchTiming> {
    // FMODE>0 moves `quantum` consecutive words per fetch slot; the words of
    // one group share the slot's hpos. Lores keeps the OCS slot sequence
    // packed into the first eight cycles of the wider fetch unit.
    let quantum = bitplane_fetch_quantum(fmode);
    let period = bitplane_fetch_period(bplcon0, fmode);
    let unit = bitplane_fetch_unit(bplcon0, fmode);
    let mut timings = Vec::new();
    for word_idx in 0..words_per_row {
        let group = word_idx as u32 / quantum;
        for plane in 0..dma_planes.min(8) {
            let order = bitplane_fetch_order(bplcon0, plane);
            let hpos = if bitplane_hires(bplcon0) || bitplane_shres(bplcon0) {
                ddfstart + group * period
            } else {
                ddfstart + group * unit + order
            };
            timings.push(BitplaneDmaFetchTiming {
                hpos,
                order,
                word_idx,
                plane,
            });
        }
    }
    timings.sort_by_key(|timing| (timing.hpos, timing.order, timing.word_idx));
    timings
}

#[cfg_attr(not(test), allow(dead_code))]
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

#[cfg_attr(not(test), allow(dead_code))]
fn apply_bitplane_modulos(
    ptrs: &mut [u32; 8],
    dma_planes: usize,
    bpl1mod: i16,
    bpl2mod: i16,
    addr_mask: u32,
) {
    for (plane, ptr) in ptrs.iter_mut().enumerate().take(dma_planes.min(8)) {
        let modulo = if plane & 1 == 0 { bpl1mod } else { bpl2mod };
        *ptr = ((*ptr as i64).wrapping_add(modulo as i64) as u32) & addr_mask;
    }
}

impl Agnus {
    pub fn new() -> Self {
        Self::with_video_standard_and_revision(VideoStandard::Pal, AgnusRevision::Ocs)
    }

    #[cfg(test)]
    pub fn with_video_standard(video_standard: VideoStandard) -> Self {
        Self::with_video_standard_and_revision(video_standard, AgnusRevision::Ocs)
    }

    pub fn with_video_standard_and_revision(
        video_standard: VideoStandard,
        revision: AgnusRevision,
    ) -> Self {
        Self {
            vpos: 0,
            hpos: 0,
            dmacon: 0,
            copcon: 0,
            lof: true,
            lol: false,
            cop1lc: 0,
            cop2lc: 0,
            nudge_cck: 0,
            pending_tick: AgnusTick::default(),
            video_standard,
            revision,
            lace: false,
            beamcon0: beamcon0_reset_value(video_standard),
            htotal: DEFAULT_HTOTAL,
            hsstrt: 0,
            hsstop: 0,
            hbstrt: 0,
            hbstop: 0,
            hcenter: 0,
            vtotal: 0,
            vsstrt: 0,
            vsstop: 0,
            vbstrt: 0,
            vbstop: 0,
            sprhdat: 0,
            dma_addr_mask: 0x001F_FFFF,
            vpos_read_delay: None,
            ersy_freeze: None,
            lpen_enabled: false,
            lpen_latch: None,
            lpen_triggered_this_field: false,
            hhpos: 0,
            fmode: 0,
        }
    }

    pub fn video_standard(&self) -> VideoStandard {
        self.video_standard
    }

    pub fn revision(&self) -> AgnusRevision {
        self.revision
    }

    pub fn set_revision(&mut self, revision: AgnusRevision) {
        self.revision = revision;
    }

    pub fn set_dma_addr_mask(&mut self, mask: u32) {
        self.dma_addr_mask = mask | 1;
        let ptr_mask = self.dma_ptr_mask();
        self.cop1lc &= ptr_mask;
        self.cop2lc &= ptr_mask;
    }

    /// Host/config entry point: authoritatively select the standard, which
    /// also rewrites the BEAMCON0 PAL bit to match.
    pub fn set_video_standard(&mut self, video_standard: VideoStandard) {
        self.beamcon0 =
            (self.beamcon0 & !(1 << 5)) | (beamcon0_reset_value(video_standard) & (1 << 5));
        self.apply_video_standard(video_standard);
    }

    /// Switch the active standard and re-clamp the beam, without touching the
    /// BEAMCON0 PAL bit. Shared by the host path and the ECS BEAMCON0 write
    /// path (where the program's just-written PAL bit must be preserved).
    fn apply_video_standard(&mut self, video_standard: VideoStandard) {
        self.video_standard = video_standard;
        if !matches!(video_standard, VideoStandard::Ntsc) {
            self.lol = false;
        }
        self.vpos = self.vpos.min(self.current_frame_lines().saturating_sub(1));
        self.hpos = self.hpos.min(self.current_line_cck().saturating_sub(1));
        self.vpos_read_delay = self
            .vpos_read_delay
            .map(|vpos| vpos.min(self.current_frame_lines().saturating_sub(1)));
    }

    pub fn set_lace(&mut self, lace: bool) {
        self.lace = lace;
        self.vpos = self.vpos.min(self.current_frame_lines().saturating_sub(1));
        self.vpos_read_delay = self
            .vpos_read_delay
            .map(|vpos| vpos.min(self.current_frame_lines().saturating_sub(1)));
    }

    pub fn current_frame_lines(&self) -> u32 {
        // VARBEAMEN programmable totals (VTOTAL+1) drive the beam entirely and
        // override the fixed PAL/NTSC short/long-frame selection.
        if let Some(lines) = self.programmable_frame_lines() {
            return lines;
        }
        if self.lace && !self.lof {
            self.video_standard.short_frame_lines()
        } else {
            self.video_standard.long_frame_lines()
        }
    }

    pub fn nominal_frame_lines(&self) -> u32 {
        if let Some(lines) = self.programmable_frame_lines() {
            return lines;
        }
        self.video_standard.long_frame_lines()
    }

    pub fn current_line_cck(&self) -> u32 {
        self.line_cck_for(self.lol)
    }

    /// Color clocks for a line given the long-line (`lol`) state. Single owner
    /// of the programmable-vs-227/228 selection; every beam-walk path (live
    /// advance, the frame/line deadlines, and the bus Copper-prediction clone)
    /// routes through this so they cannot drift.
    pub(crate) fn line_cck_for(&self, lol: bool) -> u32 {
        if let Some(line_cck) = self.programmable_line_cck() {
            return line_cck;
        }
        if matches!(self.video_standard, VideoStandard::Ntsc) && lol {
            NTSC_LONG_COLORCLOCKS_PER_LINE
        } else {
            COLORCLOCKS_PER_LINE
        }
    }

    /// Whether lines alternate long/short. Only NTSC does, and only when the
    /// beam is not programmable (VARBEAMEN fixes the line length) and LOLDIS is
    /// clear.
    pub(crate) fn long_line_toggles(&self) -> bool {
        matches!(self.video_standard, VideoStandard::Ntsc)
            && self.programmable_line_cck().is_none()
            && !self.loldis_active()
    }

    fn loldis_active(&self) -> bool {
        self.revision.is_ecs() && self.beamcon0 & BEAMCON0_LOLDIS != 0
    }

    pub fn programmable_line_cck(&self) -> Option<u32> {
        if !self.revision.is_ecs() || self.beamcon0 & BEAMCON0_VARBEAMEN == 0 {
            return None;
        }
        Some(u32::from(self.htotal).saturating_add(1).max(1))
    }

    /// Programmable frame height (VTOTAL+1) when ECS and VARBEAMEN are set;
    /// otherwise None and the fixed PAL/NTSC frame counts apply.
    pub fn programmable_frame_lines(&self) -> Option<u32> {
        if !self.revision.is_ecs() || self.beamcon0 & BEAMCON0_VARBEAMEN == 0 {
            return None;
        }
        Some(u32::from(self.vtotal).saturating_add(1).max(1))
    }

    /// ECS programmable vertical blanking: with VARVBEN set the composite
    /// vertical blank runs from VBSTRT to VBSTOP (comparator semantics: blank
    /// asserts on the VBSTRT line and clears on the VBSTOP line, wrapping
    /// through the frame top when VBSTRT >= VBSTOP). A degenerate equal pair
    /// produces no programmable blanking.
    pub fn programmable_vertical_blank(&self) -> Option<(u32, u32)> {
        if !self.revision.is_ecs()
            || self.beamcon0 & BEAMCON0_VARVBEN == 0
            || self.vbstrt == self.vbstop
        {
            return None;
        }
        Some((u32::from(self.vbstrt), u32::from(self.vbstop)))
    }

    /// The visible window of a programmable (VARBEAMEN) frame as
    /// (first visible line, visible line count). The programmable vertical
    /// blank asserts on the VBSTRT line and clears on VBSTOP, so the
    /// visible region runs from VBSTOP to VBSTRT, wrapping through the
    /// frame top; the count is the wrapped distance. With VARVBEN off (or
    /// a degenerate window) the whole frame is visible. None on
    /// fixed-geometry (non-VARBEAMEN) frames.
    pub fn programmable_visible_window(&self) -> Option<(u32, u32)> {
        let frame_lines = self.programmable_frame_lines()?;
        let Some((vbstrt, vbstop)) = self.programmable_vertical_blank() else {
            return Some((0, frame_lines));
        };
        if vbstop >= frame_lines {
            // Blank never clears inside the frame; treat as no usable
            // programmable blank rather than an empty display.
            return Some((0, frame_lines));
        }
        let lines = if vbstrt > vbstop {
            vbstrt - vbstop
        } else {
            frame_lines - vbstop + vbstrt
        };
        Some((vbstop, lines.clamp(1, frame_lines)))
    }

    /// ECS programmable horizontal blanking: with BLANKEN set the blanking
    /// output follows HBSTRT..HBSTOP (in colour clocks, same comparator and
    /// wrap semantics as the vertical window).
    pub fn programmable_horizontal_blank(&self) -> Option<(u32, u32)> {
        if !self.revision.is_ecs()
            || self.beamcon0 & BEAMCON0_BLANKEN == 0
            || self.hbstrt == self.hbstop
        {
            return None;
        }
        Some((u32::from(self.hbstrt), u32::from(self.hbstop)))
    }

    pub fn cck_until_next_frame(&self) -> u32 {
        let mut cck = self.nudge_cck;
        let mut hpos = self.hpos;
        let mut vpos = self.vpos;
        let mut lol = self.lol;
        let frame_lines = self.current_frame_lines();

        loop {
            let line_cck = self.line_cck_for(lol);
            let remaining_line = line_cck.saturating_sub(hpos).max(1);
            cck = cck.saturating_add(remaining_line);
            vpos += 1;
            if self.long_line_toggles() {
                lol = !lol;
            }
            if vpos >= frame_lines {
                return cck;
            }
            hpos = 0;
        }
    }

    pub fn cck_until_line_start(&self, target_vpos: u32) -> Option<u32> {
        if target_vpos >= self.current_frame_lines() || self.vpos >= target_vpos {
            return None;
        }

        self.cck_until_line_ticks(target_vpos - self.vpos)
    }

    pub fn cck_until_line_ticks(&self, lines: u32) -> Option<u32> {
        if lines == 0 {
            return None;
        }
        let current_line = self.current_line_cck();
        let first_line = current_line.saturating_sub(self.hpos).max(1);
        let remaining_lines = lines.saturating_sub(1);
        let following = if self.long_line_toggles() {
            let starts_long = !self.lol;
            let long_lines = if starts_long {
                remaining_lines.div_ceil(2)
            } else {
                remaining_lines / 2
            };
            u64::from(remaining_lines)
                .saturating_mul(u64::from(COLORCLOCKS_PER_LINE))
                .saturating_add(u64::from(long_lines).saturating_mul(u64::from(
                    NTSC_LONG_COLORCLOCKS_PER_LINE - COLORCLOCKS_PER_LINE,
                )))
        } else {
            // Uniform line length: PAL, LOLDIS, or programmable VARBEAMEN.
            u64::from(remaining_lines).saturating_mul(u64::from(current_line))
        };
        let cck = u64::from(self.nudge_cck)
            .saturating_add(u64::from(first_line))
            .saturating_add(following);
        Some(cck.min(u64::from(u32::MAX)) as u32)
    }

    pub fn set_cop1lc_high(&mut self, val: u16) {
        self.cop1lc =
            ((self.cop1lc & 0x0000_FFFF) | ((val as u32 & 0x001F) << 16)) & self.dma_ptr_mask();
    }
    pub fn set_cop1lc_low(&mut self, val: u16) {
        self.cop1lc = ((self.cop1lc & 0x00FF_0000) | (val as u32 & 0xFFFE)) & self.dma_ptr_mask();
    }
    pub fn set_cop2lc_high(&mut self, val: u16) {
        self.cop2lc =
            ((self.cop2lc & 0x0000_FFFF) | ((val as u32 & 0x001F) << 16)) & self.dma_ptr_mask();
    }
    pub fn set_cop2lc_low(&mut self, val: u16) {
        self.cop2lc = ((self.cop2lc & 0x00FF_0000) | (val as u32 & 0xFFFE)) & self.dma_ptr_mask();
    }

    fn dma_ptr_mask(&self) -> u32 {
        self.dma_addr_mask & !1
    }

    /// Advance the raster beam by the given number of color clocks.
    /// Returns counts of how many scanlines and frames crossed during
    /// the advance, so the caller can clock CIA-B TOD per HSYNC and
    /// CIA-A TOD per VSYNC.
    pub fn advance_by_cck(&mut self, target_cck: u32) -> AgnusTick {
        let remaining = if self.nudge_cck > target_cck {
            self.nudge_cck -= target_cck;
            0
        } else {
            let remaining = target_cck - self.nudge_cck;
            self.nudge_cck = 0;
            remaining
        };
        self.advance_cck(remaining);
        std::mem::take(&mut self.pending_tick)
    }

    /// Bump hpos by ~one MMIO-read's worth of color clocks. Kept for
    /// the nudge-debt unit test; live CPU/custom access timing now
    /// advances through the chip-bus arbiter instead.
    #[cfg(test)]
    pub fn nudge_hpos(&mut self) {
        const NUDGE_CCK: u32 = 2;
        self.advance_cck(NUDGE_CCK);
        self.nudge_cck = self.nudge_cck.saturating_add(NUDGE_CCK);
    }

    fn advance_cck(&mut self, mut cck: u32) {
        while cck > 0 {
            let line_cck = self.current_line_cck();
            let remaining_line = line_cck.saturating_sub(self.hpos).max(1);
            if cck < remaining_line {
                self.hpos += cck;
                if self.hpos >= 2 {
                    self.vpos_read_delay = None;
                }
                return;
            }

            cck -= remaining_line;
            self.hpos = 0;
            let previous_vpos = self.vpos;
            self.vpos += 1;
            self.advance_long_line_state();
            self.pending_tick.new_lines += 1;
            if self.vpos >= self.current_frame_lines() {
                self.vpos = 0;
                self.pending_tick.new_frames += 1;
                // Field wrap re-arms the light-pen latch. If the pen was
                // enabled but never pulsed, the latch freezes the end-of-field
                // position so polling software reads a stable out-of-range
                // value instead of the live beam (HRM "no light pen pulse"
                // behaviour).
                if self.lpen_enabled && !self.lpen_triggered_this_field {
                    self.lpen_latch = Some((previous_vpos, line_cck.saturating_sub(1)));
                }
                self.lpen_triggered_this_field = false;
            }
            self.vpos_read_delay = Some(previous_vpos);
        }
    }

    pub fn update_interlace_long_frame(&mut self, lace: bool) {
        self.set_lace(lace);
        if lace {
            self.lof = !self.lof;
        } else {
            self.lof = false;
        }
    }

    fn advance_long_line_state(&mut self) {
        if self.long_line_toggles() {
            self.lol = !self.lol;
        } else {
            self.lol = false;
        }
    }

    /// Bit 15 of DMACON writes is the SET/CLR flag. 1 = set the bits
    /// listed in low 15, 0 = clear them. Bits above BLTPRI are derived
    /// status or unused bits and do not latch in the control register.
    pub fn write_dmacon(&mut self, val: u16) {
        let bits = val & 0x07FF;
        if val & 0x8000 != 0 {
            self.dmacon |= bits;
        } else {
            self.dmacon &= !bits;
        }
    }

    /// Read VPOSR as exposed by the replacement Agnus: top byte is
    /// {LOF, chipset_id}, low byte is {LOL, 6'b0, V8}. With the light pen
    /// enabled and a position latched, V8 comes from the latch (LPENV high
    /// bit); LOF/LOL and the chipset id stay live.
    pub fn read_vposr(&self) -> u16 {
        let (vpos, lof, lol) = match (self.lpen_latch_active(), self.ersy_freeze) {
            (Some((v, _)), _) => (v, self.lof, self.lol),
            (None, Some(freeze)) => (freeze.vpos, freeze.lof, freeze.lol),
            (None, None) => (self.read_visible_vpos(), self.lof, self.lol),
        };
        let v8 = ((vpos >> 8) & 0x01) as u16;
        (u16::from(lof) << 15)
            | (self.revision.chipset_id(self.video_standard) << 8)
            | (u16::from(lol) << 7)
            | v8
    }

    /// Read VHPOSR: top byte = V[7:0], bottom byte = H[8:1]. With the light
    /// pen enabled and a position latched, both halves come from the
    /// LPENV/LPENH latches. With ERSY set and no genlock, the frozen beam
    /// counters are read back instead of the live beam.
    pub fn read_vhposr(&self) -> u16 {
        let (v, h) = match (self.lpen_latch_active(), self.ersy_freeze) {
            (Some((v, h)), _) => (v, h),
            (None, Some(freeze)) => (freeze.vpos, freeze.hpos),
            (None, None) => (self.read_visible_vpos(), self.hpos),
        };
        (((v & 0xFF) as u16) << 8) | (((h >> 1) & 0xFF) as u16)
    }

    /// Mirror BPLCON0.ERSY (bit 1). Setting it with no genlock attached
    /// stops the beam counters: the readback latches the position at the
    /// write. Clearing ERSY releases the counters back to the live beam.
    pub fn set_ersy(&mut self, enabled: bool) {
        match (enabled, self.ersy_freeze) {
            (true, None) => {
                self.ersy_freeze = Some(ErsyFreeze {
                    vpos: self.read_visible_vpos(),
                    hpos: self.hpos,
                    lof: self.lof,
                    lol: self.lol,
                });
            }
            (false, Some(_)) => self.ersy_freeze = None,
            _ => {}
        }
    }

    /// The internal LPENV/LPENH latches, exposed through VPOSR/VHPOSR while
    /// BPLCON0.LPEN is set.
    fn lpen_latch_active(&self) -> Option<(u32, u32)> {
        if self.lpen_enabled {
            self.lpen_latch
        } else {
            None
        }
    }

    /// Mirror BPLCON0.LPEN. Disabling the pen drops the latch so reads go
    /// back to the live beam counters.
    pub fn set_lpen(&mut self, enabled: bool) {
        if self.lpen_enabled != enabled {
            self.lpen_enabled = enabled;
            self.lpen_latch = None;
            self.lpen_triggered_this_field = false;
        }
    }

    /// ECS BEAMCON0.LPENDIS suppresses the light-pen latch entirely.
    fn lpen_latch_disabled(&self) -> bool {
        self.revision.is_ecs() && self.beamcon0 & BEAMCON0_LPENDIS != 0
    }

    /// A light-pen pulse at the current beam position. The first pulse of a
    /// field wins; later pulses in the same field are ignored, matching the
    /// latch staying frozen until it is re-armed at the field wrap.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn trigger_light_pen(&mut self) {
        if !self.lpen_enabled || self.lpen_latch_disabled() || self.lpen_triggered_this_field {
            return;
        }
        self.lpen_latch = Some((self.vpos, self.hpos));
        self.lpen_triggered_this_field = true;
    }

    pub fn hhpos(&self) -> u16 {
        self.hhpos
    }

    pub fn fmode(&self) -> u16 {
        self.fmode
    }

    /// FMODE ($1FC, AGA Alice only). The defined bits are BPL32/BPAGEM
    /// (0-1), SPR32/SPAGEM (2-3), and BSCAN2/SSCAN2 (14-15); undefined bits
    /// read back zero.
    pub fn write_fmode(&mut self, val: u16) {
        if matches!(self.revision, AgnusRevision::AgaAlice) {
            self.fmode = val & 0xC00F;
        }
    }

    /// HHPOSW ($1D8): write the UHRES horizontal beam counter. The counter
    /// itself is outside the emulated model (UHRES/DUAL is explicitly out of
    /// scope), so the value is latched for HHPOSR readback only.
    pub fn write_hhposw(&mut self, val: u16) {
        if self.revision.is_ecs() {
            self.hhpos = val & 0x01FF;
        }
    }

    pub fn beamcon0(&self) -> u16 {
        self.beamcon0
    }

    pub fn htotal(&self) -> u16 {
        self.htotal
    }

    pub fn write_beamcon0(&mut self, val: u16) {
        if self.revision.is_ecs() {
            self.beamcon0 = val;
            // The PAL bit is authoritative on ECS: a program flipping it
            // selects PAL vs NTSC geometry at runtime. apply_video_standard
            // re-clamps the beam (and is given the just-written value, so it
            // must not rewrite the PAL bit). It also re-clamps hpos for any
            // VARBEAMEN/HTOTAL line-length change.
            let new_std = if val & BEAMCON0_PAL != 0 {
                VideoStandard::Pal
            } else {
                VideoStandard::Ntsc
            };
            self.apply_video_standard(new_std);
        }
    }

    pub fn write_htotal(&mut self, val: u16) {
        if self.revision.is_ecs() {
            self.htotal = val & 0x01FF;
            self.hpos = self.hpos.min(self.current_line_cck() - 1);
        }
    }

    // ECS programmable sync/blank latches and the UHRES sprite identifier.
    // Writes are gated on ECS Agnus. The blank windows are interpreted (see
    // programmable_vertical_blank / programmable_horizontal_blank); the sync
    // position latches are stored for readback only. Horizontal positions
    // are 9-bit (& 0x01FF, like HTOTAL); vertical line counts are 11-bit
    // (& 0x07FF). The getters back the byte-write reconstruction latch.
    pub fn hsstrt(&self) -> u16 {
        self.hsstrt
    }
    pub fn write_hsstrt(&mut self, val: u16) {
        if self.revision.is_ecs() {
            self.hsstrt = val & 0x01FF;
        }
    }

    pub fn hsstop(&self) -> u16 {
        self.hsstop
    }
    pub fn write_hsstop(&mut self, val: u16) {
        if self.revision.is_ecs() {
            self.hsstop = val & 0x01FF;
        }
    }

    pub fn hbstrt(&self) -> u16 {
        self.hbstrt
    }
    pub fn write_hbstrt(&mut self, val: u16) {
        if self.revision.is_ecs() {
            self.hbstrt = val & 0x01FF;
        }
    }

    pub fn hbstop(&self) -> u16 {
        self.hbstop
    }
    pub fn write_hbstop(&mut self, val: u16) {
        if self.revision.is_ecs() {
            self.hbstop = val & 0x01FF;
        }
    }

    pub fn hcenter(&self) -> u16 {
        self.hcenter
    }
    pub fn write_hcenter(&mut self, val: u16) {
        if self.revision.is_ecs() {
            self.hcenter = val & 0x01FF;
        }
    }

    pub fn vtotal(&self) -> u16 {
        self.vtotal
    }
    pub fn write_vtotal(&mut self, val: u16) {
        if self.revision.is_ecs() {
            self.vtotal = val & 0x07FF;
        }
    }

    pub fn vsstrt(&self) -> u16 {
        self.vsstrt
    }
    pub fn write_vsstrt(&mut self, val: u16) {
        if self.revision.is_ecs() {
            self.vsstrt = val & 0x07FF;
        }
    }

    pub fn vsstop(&self) -> u16 {
        self.vsstop
    }
    pub fn write_vsstop(&mut self, val: u16) {
        if self.revision.is_ecs() {
            self.vsstop = val & 0x07FF;
        }
    }

    pub fn vbstrt(&self) -> u16 {
        self.vbstrt
    }
    pub fn write_vbstrt(&mut self, val: u16) {
        if self.revision.is_ecs() {
            self.vbstrt = val & 0x07FF;
        }
    }

    pub fn vbstop(&self) -> u16 {
        self.vbstop
    }
    pub fn write_vbstop(&mut self, val: u16) {
        if self.revision.is_ecs() {
            self.vbstop = val & 0x07FF;
        }
    }

    pub fn sprhdat(&self) -> u16 {
        self.sprhdat
    }
    pub fn write_sprhdat(&mut self, val: u16) {
        if self.revision.is_ecs() {
            self.sprhdat = val;
        }
    }

    pub fn write_vposw(&mut self, val: u16) {
        self.vpos_read_delay = None;
        self.lof = val & 0x8000 != 0;
        let low = self.vpos & 0xFF;
        let high = u32::from(val & 0x0001) << 8;
        self.vpos = (high | low).min(self.current_frame_lines() - 1);
    }

    pub fn write_vhposw(&mut self, val: u16) {
        self.vpos_read_delay = None;
        let high = self.vpos & !0xFF;
        self.vpos = (high | u32::from((val >> 8) & 0xFF)).min(self.current_frame_lines() - 1);
        self.hpos = (u32::from(val & 0xFF) << 1).min(self.current_line_cck() - 1);
    }

    fn read_visible_vpos(&self) -> u32 {
        if self.hpos < 2 {
            self.vpos_read_delay.unwrap_or(self.vpos)
        } else {
            self.vpos
        }
    }

    pub fn write_copcon(&mut self, val: u16) {
        self.copcon = val & 0x0002;
    }

    pub fn reset_copcon(&mut self) {
        self.copcon = 0;
    }

    pub fn copper_danger_enabled(&self) -> bool {
        self.copcon & 0x0002 != 0
    }
}

#[cfg(test)]
mod tests {
    use super::{
        bitplane_words_per_row, effective_bitplane_ddf_window, plan_bitplane_dma_fetches,
        sprite_dma_disabled_by_bitplane_ddf, Agnus, AgnusRevision, BitplaneDmaFetchConfig,
        BitplaneDmaFetchSlot, VideoStandard, BEAMCON0_LOLDIS, BEAMCON0_LPENDIS, BEAMCON0_PAL,
        BEAMCON0_VARBEAMEN, BEAMCON0_VARVBEN, COLORCLOCKS_PER_LINE, DMACON_BPLEN, DMACON_DMAEN,
        NTSC_LINES, NTSC_LONG_COLORCLOCKS_PER_LINE, PAL_LINES,
    };

    #[test]
    fn nudge_debt_carries_across_small_advances() {
        let mut agnus = Agnus::new();

        agnus.nudge_hpos();
        agnus.nudge_hpos();
        assert_eq!(agnus.hpos, 4);

        agnus.advance_by_cck(2);
        assert_eq!(agnus.hpos, 4);

        agnus.advance_by_cck(2);
        assert_eq!(agnus.hpos, 4);

        agnus.advance_by_cck(1);
        assert_eq!(agnus.hpos, 5);
    }

    /// BPLCON0.ERSY with no genlock stops the beam counters: VPOSR/VHPOSR
    /// freeze at the set position until ERSY is cleared. The boot ROM genlock
    /// probe (set ERSY, read VHPOSR twice, "genlock present" if the counter
    /// advanced) is the regression example: a still-moving counter makes the
    /// OS hide non-genlock programmable 31 kHz display modes.
    #[test]
    fn ersy_without_genlock_freezes_beam_counter_readback() {
        let mut agnus =
            Agnus::with_video_standard_and_revision(VideoStandard::Pal, AgnusRevision::Ocs);
        agnus.vpos = 0x50;
        agnus.hpos = 0x40;
        agnus.set_ersy(true);
        let frozen = agnus.read_vhposr();

        agnus.vpos = 0x90;
        agnus.hpos = 0x10;
        assert_eq!(agnus.read_vhposr(), frozen);
        assert_eq!(agnus.read_vposr() & 0x0001, 0);

        // Re-asserting ERSY (e.g. another BPLCON0 write with the bit still
        // set) must not re-latch at the new position.
        agnus.set_ersy(true);
        assert_eq!(agnus.read_vhposr(), frozen);

        agnus.set_ersy(false);
        assert_eq!(agnus.read_vhposr(), (0x90 << 8) | (0x10 >> 1));
    }

    #[test]
    fn vposr_reports_chipset_long_frame_long_line_and_v8() {
        let mut pal =
            Agnus::with_video_standard_and_revision(VideoStandard::Pal, AgnusRevision::Ecs8372Rev4);
        assert_eq!(pal.read_vposr(), 0xA000);

        pal.lof = true;
        pal.vpos = 0x100;
        assert_eq!(pal.read_vposr(), 0xA001);

        let mut ntsc = Agnus::with_video_standard_and_revision(
            VideoStandard::Ntsc,
            AgnusRevision::Ecs8372Rev4,
        );
        assert_eq!(ntsc.read_vposr(), 0xB000);
        ntsc.lol = true;
        assert_eq!(ntsc.read_vposr(), 0xB080);
    }

    #[test]
    fn ocs_vposr_keeps_chipset_id_clear() {
        let mut agnus = Agnus::new();

        assert_eq!(agnus.read_vposr(), 0x8000);
        agnus.lof = true;
        agnus.vpos = 0x100;
        assert_eq!(agnus.read_vposr(), 0x8001);
    }

    #[test]
    fn ntsc_timing_has_shorter_frame_and_visible_long_line_state() {
        let mut agnus = Agnus::with_video_standard(VideoStandard::Ntsc);

        agnus.advance_by_cck(COLORCLOCKS_PER_LINE);

        assert_eq!(agnus.vpos, 1);
        assert!(agnus.lol);
        assert_eq!(agnus.current_line_cck(), NTSC_LONG_COLORCLOCKS_PER_LINE);

        let remaining_frame_cck = (NTSC_LINES - 1) * COLORCLOCKS_PER_LINE
            + ((NTSC_LINES - 1) / 2) * (NTSC_LONG_COLORCLOCKS_PER_LINE - COLORCLOCKS_PER_LINE);
        let tick = agnus.advance_by_cck(remaining_frame_cck);

        assert_eq!(tick.new_frames, 1);
        assert_eq!(agnus.vpos, 0);
    }

    #[test]
    fn vpos_register_reads_increment_after_hpos_two() {
        let mut agnus = Agnus::new();
        agnus.vpos = 0x20;
        agnus.hpos = COLORCLOCKS_PER_LINE - 1;

        agnus.advance_by_cck(1);
        assert_eq!(agnus.vpos, 0x21);
        assert_eq!(agnus.hpos, 0);
        assert_eq!(agnus.read_vhposr(), 0x2000);

        agnus.advance_by_cck(1);
        assert_eq!(agnus.hpos, 1);
        assert_eq!(agnus.read_vhposr(), 0x2000);

        agnus.advance_by_cck(1);
        assert_eq!(agnus.hpos, 2);
        assert_eq!(agnus.read_vhposr(), 0x2101);
    }

    #[test]
    fn vposr_v8_bit_observes_line_start_increment_delay() {
        let mut agnus =
            Agnus::with_video_standard_and_revision(VideoStandard::Pal, AgnusRevision::Ecs8372Rev4);
        agnus.vpos = 0xFF;
        agnus.hpos = COLORCLOCKS_PER_LINE - 1;

        agnus.advance_by_cck(1);
        assert_eq!(agnus.vpos, 0x100);
        assert_eq!(agnus.read_vposr(), 0xA000);

        agnus.advance_by_cck(1);
        assert_eq!(agnus.read_vposr(), 0xA000);

        agnus.advance_by_cck(1);
        assert_eq!(agnus.read_vposr(), 0xA001);
    }

    #[test]
    fn ntsc_long_line_state_changes_before_vpos_read_increment() {
        let mut agnus = Agnus::with_video_standard(VideoStandard::Ntsc);

        agnus.advance_by_cck(COLORCLOCKS_PER_LINE);
        assert_eq!(agnus.vpos, 1);
        assert_eq!(agnus.hpos, 0);
        assert!(agnus.lol);
        assert_eq!(agnus.current_line_cck(), NTSC_LONG_COLORCLOCKS_PER_LINE);
        assert_eq!(agnus.read_vhposr(), 0x0000);

        agnus.advance_by_cck(2);
        assert_eq!(agnus.read_vhposr(), 0x0101);
    }

    #[test]
    fn frame_deadline_counts_remaining_beam_clocks() {
        let mut agnus = Agnus::new();
        agnus.vpos = PAL_LINES - 1;
        agnus.hpos = COLORCLOCKS_PER_LINE - 8;
        assert_eq!(agnus.cck_until_next_frame(), 8);

        agnus.nudge_hpos();
        assert_eq!(agnus.cck_until_next_frame(), 8);
    }

    #[test]
    fn ntsc_frame_deadline_accounts_for_long_lines() {
        let mut agnus = Agnus::with_video_standard(VideoStandard::Ntsc);

        assert_eq!(
            agnus.cck_until_next_frame(),
            NTSC_LINES * COLORCLOCKS_PER_LINE
                + (NTSC_LINES / 2) * (NTSC_LONG_COLORCLOCKS_PER_LINE - COLORCLOCKS_PER_LINE)
        );

        agnus.advance_by_cck(COLORCLOCKS_PER_LINE);
        assert_eq!(
            agnus.cck_until_next_frame(),
            (NTSC_LINES - 1) * COLORCLOCKS_PER_LINE
                + ((NTSC_LINES - 1) / 2) * (NTSC_LONG_COLORCLOCKS_PER_LINE - COLORCLOCKS_PER_LINE)
        );
    }

    #[test]
    fn line_start_deadline_counts_to_future_vpos() {
        let mut agnus = Agnus::new();
        agnus.vpos = 0x2B;
        agnus.hpos = COLORCLOCKS_PER_LINE - 6;

        assert_eq!(agnus.cck_until_line_start(0x2C), Some(6));
        assert_eq!(agnus.cck_until_line_start(0x2B), None);
    }

    #[test]
    fn line_tick_deadline_counts_multiple_hsyncs() {
        let mut agnus = Agnus::new();
        agnus.hpos = COLORCLOCKS_PER_LINE - 5;

        assert_eq!(
            agnus.cck_until_line_ticks(3),
            Some(5 + 2 * COLORCLOCKS_PER_LINE)
        );
        assert_eq!(agnus.cck_until_line_ticks(0), None);
    }

    #[test]
    fn ntsc_line_start_deadline_accounts_for_long_lines() {
        let agnus = Agnus::with_video_standard(VideoStandard::Ntsc);

        assert_eq!(
            agnus.cck_until_line_start(2),
            Some(COLORCLOCKS_PER_LINE + NTSC_LONG_COLORCLOCKS_PER_LINE)
        );
    }

    #[test]
    fn interlace_uses_short_and_long_frame_counts() {
        let mut agnus = Agnus::new();
        agnus.set_lace(true);

        assert_eq!(agnus.current_frame_lines(), 313);
        agnus.update_interlace_long_frame(true);
        assert_eq!(agnus.current_frame_lines(), 312);
        agnus.update_interlace_long_frame(true);
        assert_eq!(agnus.current_frame_lines(), 313);
        agnus.update_interlace_long_frame(false);
        assert_eq!(agnus.current_frame_lines(), 313);
        assert!(!agnus.lof);
    }

    #[test]
    fn bitplane_dma_plan_orders_lowres_fetch_slots_and_applies_modulos() {
        let config = BitplaneDmaFetchConfig {
            revision: AgnusRevision::Ecs8372Rev4,
            dmacon: DMACON_DMAEN | DMACON_BPLEN,
            fmode: 0,
            bplcon0: 0x2000,
            ddfstrt: 0x0038,
            ddfstop: 0x0038,
            bplpt: [0x0100, 0x0200, 0, 0, 0, 0, 0, 0],
            bpl1mod: 4,
            bpl2mod: 6,
            addr_mask: 0x001F_FFFF,
            harddis: false,
        };

        let plan = plan_bitplane_dma_fetches(config, 0x0038, 0x0040).unwrap();

        assert_eq!(plan.words_per_row, 1);
        assert_eq!(plan.dma_planes, 2);
        assert_eq!(
            plan.slots,
            vec![
                BitplaneDmaFetchSlot {
                    hpos: 0x003B,
                    order: 3,
                    word_idx: 0,
                    plane: 1,
                    addr: 0x0200,
                },
                BitplaneDmaFetchSlot {
                    hpos: 0x003F,
                    order: 7,
                    word_idx: 0,
                    plane: 0,
                    addr: 0x0100,
                },
            ]
        );
        assert!(plan.line_complete);
        assert_eq!(plan.ptrs_after_range[0], 0x0106);
        assert_eq!(plan.ptrs_after_range[1], 0x0208);
    }

    #[test]
    fn bitplane_dma_plan_clips_ddfstart_to_hard_fetch_window() {
        let config = BitplaneDmaFetchConfig {
            revision: AgnusRevision::Ocs,
            dmacon: DMACON_DMAEN | DMACON_BPLEN,
            fmode: 0,
            bplcon0: 0x1000,
            ddfstrt: 0x0010,
            ddfstop: 0x0018,
            bplpt: [0x0100, 0, 0, 0, 0, 0, 0, 0],
            bpl1mod: 0,
            bpl2mod: 0,
            addr_mask: 0x001F_FFFF,
            harddis: false,
        };

        let plan = plan_bitplane_dma_fetches(config, 0x0010, 0x0020).unwrap();

        assert_eq!(plan.words_per_row, 1);
        assert_eq!(
            plan.slots,
            vec![BitplaneDmaFetchSlot {
                hpos: 0x001F,
                order: 7,
                word_idx: 0,
                plane: 0,
                addr: 0x0100,
            }]
        );
        assert!(plan.line_complete);
        assert_eq!(plan.ptrs_after_range[0], 0x0102);
    }

    #[test]
    fn early_bitplane_ddfstart_disables_only_sprite_seven_dma() {
        assert!(sprite_dma_disabled_by_bitplane_ddf(
            7,
            AgnusRevision::Ocs,
            0x1000,
            0,
            DMACON_DMAEN | DMACON_BPLEN,
            0x0030,
            0x0038,
            false,
        ));
        assert!(!sprite_dma_disabled_by_bitplane_ddf(
            6,
            AgnusRevision::Ocs,
            0x1000,
            0,
            DMACON_DMAEN | DMACON_BPLEN,
            0x0030,
            0x0038,
            false,
        ));
        assert!(sprite_dma_disabled_by_bitplane_ddf(
            7,
            AgnusRevision::Ocs,
            0x1000,
            0,
            DMACON_DMAEN | DMACON_BPLEN,
            0x0028,
            0x0028,
            false,
        ));
        assert!(!sprite_dma_disabled_by_bitplane_ddf(
            7,
            AgnusRevision::Ocs,
            0x1000,
            0,
            DMACON_DMAEN | DMACON_BPLEN,
            0x0038,
            0x0038,
            false,
        ));
        assert!(!sprite_dma_disabled_by_bitplane_ddf(
            7,
            AgnusRevision::Ocs,
            0x0000,
            0,
            DMACON_DMAEN | DMACON_BPLEN,
            0x0030,
            0x0038,
            false,
        ));
        assert!(!sprite_dma_disabled_by_bitplane_ddf(
            7,
            AgnusRevision::Ocs,
            0x1000,
            0,
            DMACON_DMAEN,
            0x0030,
            0x0038,
            false,
        ));
    }

    #[test]
    fn bitplane_dma_plan_clips_ddfstop_to_hard_fetch_window() {
        let config = BitplaneDmaFetchConfig {
            revision: AgnusRevision::Ocs,
            dmacon: DMACON_DMAEN | DMACON_BPLEN,
            fmode: 0,
            bplcon0: 0x1000,
            ddfstrt: 0x00D8,
            ddfstop: 0x00E0,
            bplpt: [0x0100, 0, 0, 0, 0, 0, 0, 0],
            bpl1mod: 0,
            bpl2mod: 0,
            addr_mask: 0x001F_FFFF,
            harddis: false,
        };

        let plan = plan_bitplane_dma_fetches(config, 0x00D8, 0x00E8).unwrap();

        assert_eq!(plan.words_per_row, 1);
        assert_eq!(
            plan.slots,
            vec![BitplaneDmaFetchSlot {
                hpos: 0x00DF,
                order: 7,
                word_idx: 0,
                plane: 0,
                addr: 0x0100,
            }]
        );
        assert!(plan.line_complete);
        assert_eq!(plan.ptrs_after_range[0], 0x0102);
    }

    #[test]
    fn bitplane_dma_plan_rejects_ddfstart_after_hard_fetch_window() {
        let config = BitplaneDmaFetchConfig {
            revision: AgnusRevision::Ocs,
            dmacon: DMACON_DMAEN | DMACON_BPLEN,
            fmode: 0,
            bplcon0: 0x1000,
            ddfstrt: 0x00E0,
            ddfstop: 0x00E0,
            bplpt: [0x0100, 0, 0, 0, 0, 0, 0, 0],
            bpl1mod: 0,
            bpl2mod: 0,
            addr_mask: 0x001F_FFFF,
            harddis: false,
        };

        assert!(plan_bitplane_dma_fetches(config, 0x00D8, 0x00E8).is_none());
    }

    #[test]
    fn ocs_bitplane_dma_plan_extends_equal_ddf_window_to_hard_stop() {
        let config = BitplaneDmaFetchConfig {
            revision: AgnusRevision::Ocs,
            dmacon: DMACON_DMAEN | DMACON_BPLEN,
            fmode: 0,
            bplcon0: 0x1000,
            ddfstrt: 0x0038,
            ddfstop: 0x0038,
            bplpt: [0x0100, 0, 0, 0, 0, 0, 0, 0],
            bpl1mod: 0,
            bpl2mod: 0,
            addr_mask: 0x001F_FFFF,
            harddis: false,
        };

        let plan = plan_bitplane_dma_fetches(config, 0x0038, 0x00E0).unwrap();

        assert_eq!(plan.words_per_row, 21);
        assert_eq!(plan.slots.len(), 21);
        assert_eq!(plan.slots.first().unwrap().hpos, 0x003F);
        assert_eq!(plan.slots.last().unwrap().hpos, 0x00DF);
        assert!(plan.line_complete);
        assert_eq!(plan.ptrs_after_range[0], 0x012A);
    }

    #[test]
    fn lores_ddfstop_bit_two_extends_partial_fetch_unit() {
        assert_eq!(
            bitplane_words_per_row(AgnusRevision::Ocs, 0x0000, 0, 0x004A, 0x00B6, false),
            15
        );
    }

    #[test]
    fn ecs_bitplane_dma_plan_stops_equal_ddf_window_after_one_fetch_cycle() {
        let config = BitplaneDmaFetchConfig {
            revision: AgnusRevision::Ecs8372Rev4,
            dmacon: DMACON_DMAEN | DMACON_BPLEN,
            fmode: 0,
            bplcon0: 0x1000,
            ddfstrt: 0x0038,
            ddfstop: 0x0038,
            bplpt: [0x0100, 0, 0, 0, 0, 0, 0, 0],
            bpl1mod: 0,
            bpl2mod: 0,
            addr_mask: 0x001F_FFFF,
            harddis: false,
        };

        let plan = plan_bitplane_dma_fetches(config, 0x0038, 0x0040).unwrap();

        assert_eq!(plan.words_per_row, 1);
        assert_eq!(plan.slots.len(), 1);
        assert_eq!(plan.slots[0].hpos, 0x003F);
        assert!(plan.line_complete);
        assert_eq!(plan.ptrs_after_range[0], 0x0102);
    }

    #[test]
    fn bitplane_words_per_row_obeys_hard_fetch_window_limits() {
        // ddfstop 0xE0 clamps to the 0xD8 hard stop: (0xD8-0x18)/8 + 1 = 25.
        assert_eq!(
            bitplane_words_per_row(AgnusRevision::Ocs, 0x0000, 0, 0x0010, 0x00E0, false),
            25
        );
        // HARDDIS relaxes the stop ceiling to 0xE0: one more fetch slot -> 26.
        assert_eq!(
            bitplane_words_per_row(AgnusRevision::Ocs, 0x0000, 0, 0x0010, 0x00E0, true),
            26
        );
    }

    #[test]
    fn harddis_widens_effective_ddf_window() {
        // Without HARDDIS the stop is clamped to 0xD8; with it, to 0xE0.
        assert_eq!(
            effective_bitplane_ddf_window(
                AgnusRevision::Ecs8372Rev4,
                0x1000,
                0x0038,
                0x00E0,
                false
            ),
            Some((0x0038, 0x00D8))
        );
        assert_eq!(
            effective_bitplane_ddf_window(AgnusRevision::Ecs8372Rev4, 0x1000, 0x0038, 0x00E0, true),
            Some((0x0038, 0x00E0))
        );
    }

    #[test]
    fn beamcon0_pal_bit_switches_video_standard_on_ecs() {
        let mut a =
            Agnus::with_video_standard_and_revision(VideoStandard::Pal, AgnusRevision::Ecs8372Rev4);
        assert_eq!(a.video_standard(), VideoStandard::Pal);
        assert_eq!(a.current_frame_lines(), PAL_LINES);

        // Clearing the PAL bit selects NTSC geometry at runtime.
        a.write_beamcon0(0);
        assert_eq!(a.video_standard(), VideoStandard::Ntsc);
        assert_eq!(a.current_frame_lines(), NTSC_LINES);

        // Setting it again returns to PAL.
        a.write_beamcon0(BEAMCON0_PAL);
        assert_eq!(a.video_standard(), VideoStandard::Pal);
        assert_eq!(a.current_frame_lines(), PAL_LINES);

        // OCS Agnus has no BEAMCON0; the standard stays put.
        let mut ocs =
            Agnus::with_video_standard_and_revision(VideoStandard::Pal, AgnusRevision::Ocs);
        ocs.write_beamcon0(0);
        assert_eq!(ocs.video_standard(), VideoStandard::Pal);
    }

    #[test]
    fn beamcon0_standard_switch_clamps_vpos() {
        let mut a =
            Agnus::with_video_standard_and_revision(VideoStandard::Pal, AgnusRevision::Ecs8372Rev4);
        a.vpos = 300; // valid PAL line, beyond the NTSC frame
        a.write_beamcon0(0); // -> NTSC (263 lines)
        assert!(a.vpos < NTSC_LINES);
    }

    #[test]
    fn loldis_stops_ntsc_long_line_toggle() {
        let mut a = Agnus::with_video_standard_and_revision(
            VideoStandard::Ntsc,
            AgnusRevision::Ecs8372Rev4,
        );
        // Plain NTSC alternates: after one short line, lol becomes true.
        a.advance_by_cck(COLORCLOCKS_PER_LINE);
        assert!(a.lol);

        // LOLDIS (PAL bit clear keeps NTSC) freezes the long-line toggle.
        a.write_beamcon0(BEAMCON0_LOLDIS);
        assert!(!a.long_line_toggles());
        a.advance_by_cck(a.current_line_cck());
        assert!(!a.lol);
        assert_eq!(a.current_line_cck(), COLORCLOCKS_PER_LINE);
    }

    #[test]
    fn vtotal_sets_programmable_frame_height() {
        let mut a =
            Agnus::with_video_standard_and_revision(VideoStandard::Pal, AgnusRevision::Ecs8372Rev4);
        a.write_vtotal(199); // last line 199 -> 200-line frame
                             // Without VARBEAMEN the fixed PAL count still applies.
        assert_eq!(a.current_frame_lines(), PAL_LINES);

        a.write_beamcon0(BEAMCON0_PAL | BEAMCON0_VARBEAMEN);
        assert_eq!(a.current_frame_lines(), 200);
        assert_eq!(a.nominal_frame_lines(), 200);

        // Clearing VARBEAMEN reverts to the fixed standard count.
        a.write_beamcon0(BEAMCON0_PAL);
        assert_eq!(a.current_frame_lines(), PAL_LINES);
    }

    /// The visible window of a programmable scan runs from VBSTOP to
    /// VBSTRT (blank asserts at VBSTRT, clears at VBSTOP), wrapping
    /// through the frame top; without a usable programmable blank the
    /// whole frame is visible.
    #[test]
    fn programmable_visible_window_follows_vertical_blank() {
        let mut a =
            Agnus::with_video_standard_and_revision(VideoStandard::Pal, AgnusRevision::Ecs8372Rev4);
        assert_eq!(a.programmable_visible_window(), None);

        // Programmable 31 kHz scan: 626 lines, blank wrapping through the top.
        a.write_vtotal(625);
        a.write_beamcon0(BEAMCON0_PAL | BEAMCON0_VARBEAMEN);
        assert_eq!(a.programmable_visible_window(), Some((0, 626)));

        a.write_beamcon0(BEAMCON0_PAL | BEAMCON0_VARBEAMEN | BEAMCON0_VARVBEN);
        a.write_vbstrt(613);
        a.write_vbstop(44);
        assert_eq!(a.programmable_visible_window(), Some((44, 569)));

        // Blank wholly below the frame top: visible wraps through it.
        a.write_vbstrt(10);
        a.write_vbstop(44);
        assert_eq!(a.programmable_visible_window(), Some((44, 626 - 44 + 10)));

        // Degenerate pairs fall back to the full frame.
        a.write_vbstrt(44);
        a.write_vbstop(44);
        assert_eq!(a.programmable_visible_window(), Some((0, 626)));
        a.write_vbstrt(10);
        a.write_vbstop(700); // clears past the last line
        assert_eq!(a.programmable_visible_window(), Some((0, 626)));
    }

    #[test]
    fn vtotal_overrides_lace_frame_selection() {
        let mut a =
            Agnus::with_video_standard_and_revision(VideoStandard::Pal, AgnusRevision::Ecs8372Rev4);
        a.write_vtotal(149); // 150-line frame
        a.write_beamcon0(BEAMCON0_PAL | BEAMCON0_VARBEAMEN);
        a.set_lace(true);
        a.update_interlace_long_frame(true);
        // Programmable totals win over the lace short/long selection.
        assert_eq!(a.current_frame_lines(), 150);
    }

    #[test]
    fn programmable_frame_does_not_toggle_long_line() {
        let mut a = Agnus::with_video_standard_and_revision(
            VideoStandard::Ntsc,
            AgnusRevision::Ecs8372Rev4,
        );
        a.write_htotal(140); // programmable line length 141
        a.write_beamcon0(BEAMCON0_VARBEAMEN); // NTSC + programmable beam
        assert_eq!(a.current_line_cck(), 141);
        assert!(!a.long_line_toggles());
        a.advance_by_cck(141 * 3);
        assert_eq!(a.current_line_cck(), 141);
    }

    #[test]
    fn vtotal_zero_does_not_underflow() {
        let mut a =
            Agnus::with_video_standard_and_revision(VideoStandard::Pal, AgnusRevision::Ecs8372Rev4);
        a.write_vtotal(0); // 1-line frame
        a.write_beamcon0(BEAMCON0_PAL | BEAMCON0_VARBEAMEN);
        assert_eq!(a.current_frame_lines(), 1);
        // These exercise the `current_frame_lines() - 1` clamp sites.
        a.set_video_standard(VideoStandard::Ntsc);
        a.set_lace(true);
        a.advance_by_cck(COLORCLOCKS_PER_LINE * 2);
    }

    #[test]
    fn bitplane_dma_plan_uses_current_pointers_for_partial_line_ranges() {
        let config = BitplaneDmaFetchConfig {
            revision: AgnusRevision::Ecs8372Rev4,
            dmacon: DMACON_DMAEN | DMACON_BPLEN,
            fmode: 0,
            bplcon0: 0x2000,
            ddfstrt: 0x0038,
            ddfstop: 0x0038,
            bplpt: [0x0100, 0x0202, 0, 0, 0, 0, 0, 0],
            bpl1mod: 4,
            bpl2mod: 6,
            addr_mask: 0x001F_FFFF,
            harddis: false,
        };

        let plan = plan_bitplane_dma_fetches(config, 0x003C, 0x0040).unwrap();

        assert_eq!(
            plan.slots,
            vec![BitplaneDmaFetchSlot {
                hpos: 0x003F,
                order: 7,
                word_idx: 0,
                plane: 0,
                addr: 0x0100,
            }]
        );
        assert!(plan.line_complete);
        assert_eq!(plan.ptrs_after_range[0], 0x0106);
        assert_eq!(plan.ptrs_after_range[1], 0x0208);
    }

    #[test]
    fn bitplane_dma_plan_requires_master_dma_enable() {
        let config = BitplaneDmaFetchConfig {
            revision: AgnusRevision::Ocs,
            dmacon: DMACON_BPLEN,
            fmode: 0,
            bplcon0: 0x1000,
            ddfstrt: 0x0038,
            ddfstop: 0x0038,
            bplpt: [0x0100, 0, 0, 0, 0, 0, 0, 0],
            bpl1mod: 0,
            bpl2mod: 0,
            addr_mask: 0x001F_FFFF,
            harddis: false,
        };

        assert!(plan_bitplane_dma_fetches(config, 0x0038, 0x0040).is_none());
    }

    /// Plan 3.3: FMODE widens the fetch. The DDF window completes whole
    /// fetch units from the DDFSTRT comparator and each active slot moves
    /// 1/2/4 consecutive words.
    #[test]
    fn fmode_scales_words_per_row_and_fetch_plan() {
        let aga = AgnusRevision::AgaAlice;
        // Standard lores window $38..$D0: 20 blocks at FMODE=0.
        assert_eq!(
            bitplane_words_per_row(aga, 0x0000, 0, 0x0038, 0x00D0, false),
            20
        );
        // FMODE=3 lores: 32-cck units, 4 words per unit. The raw $38..$D0
        // DDF span rounds up to 6 units -> 24 words.
        assert_eq!(
            bitplane_words_per_row(aga, 0x0000, 3, 0x0038, 0x00D0, false),
            24
        );
        // A window that does not divide evenly still completes the last
        // unit: from the raw $38, ($60-$38) rounds up to 2 units, then the
        // tail unit is completed -> 12 words.
        assert_eq!(
            bitplane_words_per_row(aga, 0x0000, 3, 0x0038, 0x0060, false),
            12
        );
        // Hires FMODE=1: 8-cck units of one 32-bit fetch (2 words).
        assert_eq!(
            bitplane_words_per_row(aga, 0x8000, 1, 0x003C, 0x00D4, false),
            40
        );

        // FMODE=3 lores plan: groups of 4 consecutive words share one slot
        // hpos; the lores slot sequence is packed into the first 8 cck of the
        // 32-cck unit; pointers advance 2 bytes per word.
        let config = BitplaneDmaFetchConfig {
            revision: aga,
            dmacon: DMACON_DMAEN | DMACON_BPLEN,
            bplcon0: 0x1000, // 1 plane lores
            fmode: 3,
            ddfstrt: 0x0038,
            ddfstop: 0x0060,
            bplpt: [0x0100, 0, 0, 0, 0, 0, 0, 0],
            bpl1mod: 0,
            bpl2mod: 0,
            addr_mask: 0x001F_FFFF,
            harddis: false,
        };
        let plan = plan_bitplane_dma_fetches(config, 0x0038, 0x00A0).expect("plan");
        // DDFSTRT $38 arms the sequencer directly; DDFSTOP $60 still rounds
        // through the final 32-cck unit, so the row is 12 words.
        assert_eq!(plan.words_per_row, 12);
        assert_eq!(plan.slots.len(), 12, "12 words for the single plane");
        // Plane 1's lores order-7 slot is in the first 8 cck of the unit.
        let unit_offset = 7;
        assert_eq!(plan.slots[0].hpos, 0x38 + unit_offset);
        assert_eq!(
            plan.slots[3].hpos,
            0x38 + unit_offset,
            "words share the slot"
        );
        assert_eq!(plan.slots[4].hpos, 0x38 + 32 + unit_offset, "next unit");
        let addrs: Vec<u32> = plan.slots.iter().map(|slot| slot.addr).collect();
        assert_eq!(
            addrs,
            (0..12).map(|w| 0x0100 + w * 2).collect::<Vec<_>>(),
            "consecutive words"
        );
        assert!(plan.line_complete);
        assert_eq!(plan.ptrs_after_range[0], 0x0100 + 24);
    }

    #[test]
    fn wide_fmode_counts_units_from_raw_ddfstart() {
        let aga = AgnusRevision::AgaAlice;
        // Hi-res FMODE=3 has 16-CCK fetch units. A lo-res-style DDFSTRT $38
        // starts the sequencer at $38, not at an absolute $30 boundary, while
        // DDFSTOP $C0 still rounds through the final unit:
        // ceil(($C0-$38)/16)+1 = 10 units * 4 words = 40 words/row. This is
        // the width expected by interleaved 8-plane effects using BPLxMOD for
        // seven skipped planes per row.
        assert_eq!(
            bitplane_words_per_row(aga, 0x8000, 3, 0x0038, 0x00C0, false),
            40
        );
        // A grid-aligned DDFSTRT gives the same count for this DDFSTOP.
        assert_eq!(
            bitplane_words_per_row(aga, 0x8000, 3, 0x0030, 0x00C0, false),
            40
        );
        // FMODE=0 has 8-CCK units, which already divide the standard hi-res
        // DDFSTRT $3C, so it is never anchored: $3C/$D4 keeps its 40 words.
        assert_eq!(
            bitplane_words_per_row(aga, 0x8000, 0, 0x003C, 0x00D4, false),
            40
        );
    }

    #[test]
    fn vposr_reports_8375_chipset_id() {
        // HRM identification table: 8375 ("8372 rev 5") = $22 PAL, $31 NTSC.
        let pal =
            Agnus::with_video_standard_and_revision(VideoStandard::Pal, AgnusRevision::Ecs8375);
        assert_eq!(pal.read_vposr() & 0x7F00, 0x2200);
        let ntsc =
            Agnus::with_video_standard_and_revision(VideoStandard::Ntsc, AgnusRevision::Ecs8375);
        assert_eq!(ntsc.read_vposr() & 0x7F00, 0x3100);
    }

    #[test]
    fn light_pen_trigger_latches_beam_for_vposr_vhposr() {
        let mut agnus = Agnus::with_video_standard(VideoStandard::Pal);
        agnus.set_lpen(true);
        agnus.advance_by_cck(100 * COLORCLOCKS_PER_LINE + 0x40);
        assert_eq!(agnus.vpos, 100);
        assert_eq!(agnus.hpos, 0x40);
        agnus.trigger_light_pen();

        // The latch freezes the read while the beam moves on.
        agnus.advance_by_cck(50);
        assert_eq!(agnus.read_vhposr(), (100 << 8) | (0x40 >> 1));
        assert_eq!(agnus.read_vposr() & 0x0001, 0);

        // A second pulse in the same field is ignored.
        agnus.advance_by_cck(COLORCLOCKS_PER_LINE);
        agnus.trigger_light_pen();
        assert_eq!(agnus.read_vhposr(), (100 << 8) | (0x40 >> 1));

        // Disabling LPEN returns the live counters.
        agnus.set_lpen(false);
        assert_eq!(agnus.read_vhposr() >> 8, agnus.vpos as u16 & 0xFF);
    }

    #[test]
    fn light_pen_without_pulse_latches_end_of_field() {
        let mut agnus = Agnus::with_video_standard(VideoStandard::Pal);
        agnus.set_lpen(true);
        // A full field passes with no pen pulse: the latch freezes the
        // end-of-field position (last line, end of line).
        agnus.advance_by_cck(PAL_LINES * COLORCLOCKS_PER_LINE + 10);
        let expect_v = ((PAL_LINES - 1) & 0xFF) as u16;
        let expect_h = ((COLORCLOCKS_PER_LINE - 1) >> 1) as u16;
        assert_eq!(agnus.read_vhposr(), (expect_v << 8) | expect_h);

        // The new field re-arms the latch: a pulse overwrites it.
        agnus.advance_by_cck(5 * COLORCLOCKS_PER_LINE - 10 + 0x20);
        agnus.trigger_light_pen();
        assert_eq!(agnus.read_vhposr(), (5 << 8) | (0x20 >> 1));
    }

    #[test]
    fn ecs_lpendis_suppresses_light_pen_latch() {
        let mut agnus =
            Agnus::with_video_standard_and_revision(VideoStandard::Pal, AgnusRevision::Ecs8372Rev4);
        agnus.set_lpen(true);
        agnus.write_beamcon0(BEAMCON0_PAL | BEAMCON0_LPENDIS);
        agnus.advance_by_cck(10 * COLORCLOCKS_PER_LINE + 0x30);
        agnus.trigger_light_pen();
        // No latch: reads stay live.
        assert_eq!(agnus.read_vhposr(), (10 << 8) | (0x30 >> 1));
        agnus.advance_by_cck(2);
        assert_eq!(agnus.read_vhposr(), (10 << 8) | (0x32 >> 1));
    }

    #[test]
    fn hhposw_latches_on_ecs_only() {
        let mut ocs = Agnus::with_video_standard(VideoStandard::Pal);
        ocs.write_hhposw(0x0123);
        assert_eq!(ocs.hhpos(), 0);

        let mut ecs =
            Agnus::with_video_standard_and_revision(VideoStandard::Pal, AgnusRevision::Ecs8372Rev4);
        ecs.write_hhposw(0x3123);
        assert_eq!(ecs.hhpos(), 0x0123, "9-bit horizontal latch");
    }
}
