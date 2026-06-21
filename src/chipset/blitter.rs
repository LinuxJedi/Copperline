// SPDX-License-Identifier: GPL-3.0-or-later

//! Amiga blitter ($DFF040-$DFF074). The blitter is Agnus's general-purpose
//! 16-bit-wide DMA engine; software programs A/B/C/D channels and triggers
//! by writing BLTSIZE ($058). It supports three modes:
//!
//! - **Normal mode** (BLTCON1.LINEMODE=0): rectangular block transfer,
//!   `D = LF(A, B, C)` where `LF` is an 8-bit minterm lookup table.
//!   Optional first/last-word masks on A, independent shifts on A and B,
//!   per-channel modulos, ascending or descending direction.
//! - **Line mode** (BLTCON1.LINEMODE=1): Bresenham single-pixel-wide line
//!   into a bitplane via the A/C/D channels. BLTAPT holds the Bresenham
//!   accumulator; BLTCON1[4:2] = (SUD, SUL, AUL) encodes the octant.
//! - **Area fill** (BLTCON1.IFE/EFE): a post-minterm transform applied
//!   row-by-row in descending bit order, used by intuition/gadtools for
//!   filling closed shapes.
//!
//! `execute()` is still available for focused unit tests, but the bus
//! normally starts a scheduled blit from BLTSIZE and lets chip-bus grants
//! retire it over time. This keeps DMACONR.BBUSY observable while
//! software waits with the standard `VBLT` macro.

const BLTCON0_USE_A: u16 = 1 << 11;
const BLTCON0_USE_B: u16 = 1 << 10;
const BLTCON0_USE_C: u16 = 1 << 9;
const BLTCON0_USE_D: u16 = 1 << 8;

const BLTCON1_SIGN: u16 = 1 << 6;
const BLTCON1_DOFF: u16 = 1 << 7;
const BLTCON1_EFE: u16 = 1 << 4;
const BLTCON1_IFE: u16 = 1 << 3;
const BLTCON1_FCI: u16 = 1 << 2;
const BLTCON1_DESC: u16 = 1 << 1;
const BLTCON1_SING: u16 = 1 << 1;
const BLTCON1_LINE: u16 = 1 << 0;
const CHIP_DMA_ADDR_MASK: u32 = 0x001F_FFFF;
const CHIP_DMA_HIGH_MASK: u32 = 0x001F_0000;

/// The blitter can only DMA from populated chip RAM. Addresses outside
/// the configured chip range do not mirror into low RAM; they hit
/// unpopulated space.
fn chip_off(ptr: u32, ram_len: usize) -> Option<usize> {
    let populated_mask = ram_len.next_power_of_two().saturating_sub(1) as u32;
    let off = (ptr & CHIP_DMA_ADDR_MASK & populated_mask) as usize;
    (off + 1 < ram_len).then_some(off)
}

fn read_word(ram: &[u8], ptr: u32) -> u16 {
    let Some(off) = chip_off(ptr, ram.len()) else {
        return 0;
    };
    let hi = ram[off];
    let lo = ram[off + 1];
    u16::from_be_bytes([hi, lo])
}

fn write_word(ram: &mut [u8], ptr: u32, val: u16) {
    let len = ram.len();
    let Some(off) = chip_off(ptr, len) else {
        return;
    };
    let bytes = val.to_be_bytes();
    ram[off] = bytes[0];
    ram[off + 1] = bytes[1];
}

/// All-bits-parallel evaluation of the 8-bit minterm `lf` on the three
/// 16-bit inputs. For each output bit position, the (a,b,c) triple
/// indexes `lf`. The standard formulation enumerates all eight LF
/// nibbles and ORs in the matching (A,B,C) AND product.
fn minterm(lf: u8, a: u16, b: u16, c: u16) -> u16 {
    let na = !a;
    let nb = !b;
    let nc = !c;
    let mut d = 0u16;
    if lf & 0x80 != 0 {
        d |= a & b & c;
    }
    if lf & 0x40 != 0 {
        d |= a & b & nc;
    }
    if lf & 0x20 != 0 {
        d |= a & nb & c;
    }
    if lf & 0x10 != 0 {
        d |= a & nb & nc;
    }
    if lf & 0x08 != 0 {
        d |= na & b & c;
    }
    if lf & 0x04 != 0 {
        d |= na & b & nc;
    }
    if lf & 0x02 != 0 {
        d |= na & nb & c;
    }
    if lf & 0x01 != 0 {
        d |= na & nb & nc;
    }
    d
}

/// Barrel-shifter combining the previously-processed source word with the
/// current one. Imagine the source words laid out as pixels MSB-first
/// across memory: a 4-bit right-shift means the leftmost 4 pixels of
/// the second word come from the rightmost 4 pixels of the first word.
///
/// Ascending mode produces (prev:cur) >> n (low 16 bits): the bottom
/// `n` bits of the previous word fill the top `n` bits of the new
/// shifted current.
///
/// Descending mode produces (cur:prev) << n (high 16 bits): the top
/// `n` bits of the previous word fill the bottom `n` bits of the new
/// shifted current. `prev` here is the word processed at the higher
/// address (which descending mode visited first).
fn shift_combine(prev: u16, cur: u16, n: u32, desc: bool) -> u16 {
    if n == 0 {
        return cur;
    }
    if desc {
        let combined = ((cur as u32) << 16) | (prev as u32);
        let shifted = combined << n;
        (shifted >> 16) as u16
    } else {
        let combined = ((prev as u32) << 16) | (cur as u32);
        let shifted = combined >> n;
        shifted as u16
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Blitter {
    pub bltcon0: u16,
    pub bltcon1: u16,
    pub bltafwm: u16,
    pub bltalwm: u16,

    pub bltapt: u32,
    pub bltbpt: u32,
    pub bltcpt: u32,
    pub bltdpt: u32,

    pub bltamod: i16,
    pub bltbmod: i16,
    pub bltcmod: i16,
    pub bltdmod: i16,

    pub bltadat: u16,
    pub bltbdat: u16,
    pub bltcdat: u16,
    pub bltsizv: u16,
    bltbold: u16,
    bltbold_init: bool,
    line_bdat: u16,
    line_bdat_valid: bool,

    /// Set to true during `execute()`; cleared on exit. We snapshot it
    /// for DMACONR even though normally the CPU only observes the
    /// cleared state.
    pub busy: bool,
    /// Set to true at the start of `execute()` and cleared on the first
    /// non-zero D word. Surfaces as DMACONR bit 13.
    pub bzero: bool,

    pending: Option<PendingBlit>,
    dma_addr_mask: u32,
    #[serde(skip, default)]
    bltaold: u16,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
enum PendingBlit {
    Line(LineBlitState),
    Normal(NormalBlitState),
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct LineBlitState {
    phase: LineBlitPhase,
    slots_remaining: u32,
    npixels_remaining: u32,
    con0: u16,
    con1: u16,
    lf: u8,
    use_c: bool,
    sing: bool,
    bplmod: i32,
    amod_step: u16,
    bmod_step: u16,
    cpt: u32,
    dpt: u32,
    ash_now: i32,
    acc: u16,
    sign: bool,
    one_dot: bool,
    bdat: u16,
    bsh: u16,
    a_word: u16,
    bltcdat: u16,
    cur_c: u16,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct NormalBlitState {
    phase: NormalBlitPhase,
    slots_remaining: u32,
    h_remaining: u32,
    w: u32,
    word_idx: u32,

    lf: u8,
    use_a: bool,
    use_b: bool,
    use_c: bool,
    use_d: bool,
    write_d: bool,
    ash: u32,
    bsh: u32,
    desc: bool,
    ife: bool,
    efe: bool,
    fci: u16,

    step: i32,
    amod: i32,
    bmod: i32,
    cmod: i32,
    dmod: i32,

    bltafwm: u16,
    bltalwm: u16,
    bltadat: u16,
    bltbdat: u16,
    bltcdat: u16,

    apt: u32,
    bpt: u32,
    cpt: u32,
    dpt: u32,

    a_prev: u16,
    b_prev: u16,
    cur_a: u16,
    cur_b: u16,
    cur_c: u16,
    fill_state: u16,
    pipeline_full: bool,
    d_hold: u16,
    d_hold_pt: u32,

    // Source words (A and B channels) snapshotted from chip RAM at BLTSIZE.
    // On real hardware the blitter owns the chip bus for the whole blit and
    // consumes its source before the CPU can write those addresses again;
    // code that reuses a scratch buffer for back-to-back blits relies on this.
    // We read the source
    // up front so a CPU overwrite mid-blit cannot corrupt it, while still
    // computing and writing D progressively (so mid-blit BLTCON0/DMACON/DOFF
    // changes and beam timing keep working). C stays live: it is the
    // destination read-modify-write channel, so a self-overlapping blit must
    // still see its own freshly written D words.
    //
    // The snapshot, on its own, breaks self-overlapping blits that feed D back
    // through the A or B channel (not just C), such as a vertical XOR fill with
    // D = A ^ B where B points one row above D (apt==dpt, bpt==dpt-rowbytes).
    // Each output row must read the row this same blit just wrote. To keep both
    // behaviours, A/B read the snapshot EXCEPT at addresses this blit has
    // already written via D, where they read the freshly written word
    // (`d_overlay`). D writes to a separate buffer never land in the overlay, so
    // that CPU-overwrite protection is unaffected.
    snap_a: Vec<u16>,
    snap_b: Vec<u16>,
    snap_a_idx: usize,
    snap_b_idx: usize,
    track_overlay: bool,
    d_overlay: std::collections::HashMap<usize, u16>,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
enum NormalBlitPhase {
    StartDelay,
    Init,
    A,
    B,
    C,
    D,
    E,
    F,
    Done,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
enum LineBlitPhase {
    StartDelay,
    Init,
    L1,
    L2,
    L3,
    L4,
    Done,
}

impl Blitter {
    pub fn new() -> Self {
        Self {
            bltcon0: 0,
            bltcon1: 0,
            bltafwm: 0,
            bltalwm: 0,
            bltapt: 0,
            bltbpt: 0,
            bltcpt: 0,
            bltdpt: 0,
            bltamod: 0,
            bltbmod: 0,
            bltcmod: 0,
            bltdmod: 0,
            bltadat: 0,
            bltbdat: 0,
            bltcdat: 0,
            bltsizv: 0,
            bltaold: 0,
            bltbold: 0,
            bltbold_init: true,
            line_bdat: 0,
            line_bdat_valid: false,
            busy: false,
            bzero: true,
            pending: None,
            dma_addr_mask: CHIP_DMA_ADDR_MASK,
        }
    }

    pub fn set_dma_addr_mask(&mut self, mask: u32) {
        self.dma_addr_mask = mask | 1;
        let ptr_mask = self.dma_ptr_mask();
        self.bltapt &= ptr_mask;
        self.bltbpt &= ptr_mask;
        self.bltcpt &= ptr_mask;
        self.bltdpt &= ptr_mask;
    }

    pub fn set_apt_high(&mut self, val: u16) {
        self.bltapt =
            ((self.bltapt & 0x0000_FFFF) | (((val as u32) & 0x001F) << 16)) & self.dma_ptr_mask();
    }
    pub fn set_apt_low(&mut self, val: u16) {
        self.bltapt =
            ((self.bltapt & CHIP_DMA_HIGH_MASK) | ((val as u32) & 0xFFFE)) & self.dma_ptr_mask();
    }
    pub fn set_bpt_high(&mut self, val: u16) {
        self.bltbpt =
            ((self.bltbpt & 0x0000_FFFF) | (((val as u32) & 0x001F) << 16)) & self.dma_ptr_mask();
    }
    pub fn set_bpt_low(&mut self, val: u16) {
        self.bltbpt =
            ((self.bltbpt & CHIP_DMA_HIGH_MASK) | ((val as u32) & 0xFFFE)) & self.dma_ptr_mask();
    }
    pub fn set_cpt_high(&mut self, val: u16) {
        self.bltcpt =
            ((self.bltcpt & 0x0000_FFFF) | (((val as u32) & 0x001F) << 16)) & self.dma_ptr_mask();
    }
    pub fn set_cpt_low(&mut self, val: u16) {
        self.bltcpt =
            ((self.bltcpt & CHIP_DMA_HIGH_MASK) | ((val as u32) & 0xFFFE)) & self.dma_ptr_mask();
    }
    pub fn set_dpt_high(&mut self, val: u16) {
        self.bltdpt =
            ((self.bltdpt & 0x0000_FFFF) | (((val as u32) & 0x001F) << 16)) & self.dma_ptr_mask();
    }
    pub fn set_dpt_low(&mut self, val: u16) {
        self.bltdpt =
            ((self.bltdpt & CHIP_DMA_HIGH_MASK) | ((val as u32) & 0xFFFE)) & self.dma_ptr_mask();
    }

    fn dma_ptr_mask(&self) -> u32 {
        self.dma_addr_mask & !1
    }

    pub fn write_bltcon0(&mut self, val: u16) {
        if self.busy {
            self.disable_pending_d_output();
        }
        self.bltcon0 = val;
    }

    pub fn write_bltcon1(&mut self, val: u16) {
        if val & BLTCON1_LINE == 0 {
            self.line_bdat_valid = false;
        }
        self.bltcon1 = val;
    }

    pub fn write_bltadat(&mut self, val: u16) {
        self.bltadat = val;
    }

    pub fn write_bltbdat(&mut self, val: u16) {
        self.bltbold = if self.bltbold_init { 0 } else { self.bltbdat };
        self.bltbold_init = false;
        self.bltbdat = val;
        if self.bltcon1 & BLTCON1_LINE != 0 {
            let bsh = ((self.bltcon1 >> 12) & 0x0F) as u32;
            self.line_bdat = self.bltbdat.rotate_right(bsh);
            self.line_bdat_valid = true;
        }
    }

    pub fn write_bltcdat(&mut self, val: u16) {
        self.bltcdat = val;
    }

    fn disable_pending_d_output(&mut self) {
        if let Some(PendingBlit::Normal(state)) = self.pending.as_mut() {
            state.disable_d_output();
        }
    }

    fn finish_blit(&mut self) {
        self.busy = false;
        self.bltbold_init = true;
    }

    /// Triggered by a BLTSIZE write. Runs the entire blit synchronously
    /// against `ram`, updates pointers, sets `bzero` from the OR-of-all-D
    /// across the run.
    /// Apply a whole blit's memory effect in one shot. Production issues
    /// blits through `start_scheduled` (progressive timing with the source
    /// snapshotted at BLTSIZE); this immediate form is kept for unit tests
    /// that check the blit math directly.
    #[cfg(test)]
    pub fn execute(&mut self, bltsize: u16, ram: &mut [u8]) {
        let (h, w) = decode_bltsize(bltsize);
        self.execute_dims(h, w, ram);
    }

    #[cfg(test)]
    fn execute_dims(&mut self, h: u32, w: u32, ram: &mut [u8]) {
        self.pending = None;
        if ram.is_empty() {
            self.busy = false;
            return;
        }
        self.busy = true;
        self.bzero = true;
        if self.bltcon1 & BLTCON1_LINE != 0 {
            self.execute_line(h, ram);
        } else {
            self.execute_normal(h, w, ram);
        }
        self.finish_blit();
    }

    pub fn start_scheduled(&mut self, bltsize: u16, ram: &[u8]) {
        let (h, w) = decode_bltsize(bltsize);
        self.start_scheduled_dims(h, w, ram);
    }

    pub fn start_scheduled_ecs(&mut self, bltsizh: u16, ram: &[u8]) {
        let (h, w) = decode_ecs_bltsize(self.bltsizv, bltsizh);
        self.start_scheduled_dims(h, w, ram);
    }

    fn start_scheduled_dims(&mut self, h: u32, w: u32, ram: &[u8]) {
        self.busy = true;
        self.bzero = true;
        if self.bltcon1 & BLTCON1_LINE != 0 {
            self.pending = Some(PendingBlit::Line(LineBlitState::new(self, h)));
        } else {
            self.pending = Some(PendingBlit::Normal(NormalBlitState::new(self, h, w, ram)));
        }
    }

    pub fn tick_scheduled_slot(&mut self, ram: &mut [u8]) -> bool {
        if self.pending.is_none() || !self.busy {
            return false;
        }
        let mut pending = self.pending.take().unwrap();
        match &mut pending {
            PendingBlit::Normal(state) => {
                if state.tick_slot(ram, &mut self.bzero) {
                    state.write_back(self);
                    self.finish_blit();
                    true
                } else {
                    self.pending = Some(pending);
                    false
                }
            }
            PendingBlit::Line(state) => {
                if state.tick_slot(ram, &mut self.bzero) {
                    state.write_back(self);
                    self.finish_blit();
                    true
                } else {
                    self.pending = Some(pending);
                    false
                }
            }
        }
    }

    pub fn scheduled_slots_remaining(&self) -> Option<u32> {
        if !self.busy {
            return None;
        }
        match self.pending.as_ref()? {
            PendingBlit::Line(state) => Some(state.slots_remaining().max(1)),
            PendingBlit::Normal(state) => Some(state.slots_remaining().max(1)),
        }
    }

    /// Whether the blit pipeline cycle that the next `tick_scheduled_slot`
    /// will process performs a chip-bus access. Idle pipeline cycles (the "-"
    /// slots in the HRM blitter cycle diagrams, e.g. the non-write half of a
    /// D-only clear or a line blit's two internal cycles) do not use the bus:
    /// per the HRM they "are available to the other DMA channels or the 68000",
    /// and the MiniMig RTL only asserts the blitter's dma_req on channel-access
    /// states. The bus still advances the pipeline through these cycles (they
    /// elapse in real time), it just does not reserve the slot for them.
    pub fn current_slot_needs_bus(&self) -> bool {
        if !self.busy {
            return false;
        }
        match self.pending.as_ref() {
            Some(PendingBlit::Normal(state)) => state.current_slot_needs_bus(),
            Some(PendingBlit::Line(state)) => state.current_slot_needs_bus(),
            None => false,
        }
    }

    /// Access pattern of the next scheduled pipeline slots, as a bitmask: bit k
    /// set means slot k (k=0 is the slot the next tick processes) performs a
    /// chip-bus access; clear means it is an internal cycle that leaves the bus
    /// free. Returns (mask, count) with count = min(slots remaining, limit, 64).
    /// Used by the completion-deadline prediction so it walks the same
    /// access/idle sequence the live bus arbitration sees.
    pub fn scheduled_slot_access_pattern(&self, limit: u32) -> Option<(u64, u32)> {
        if !self.busy {
            return None;
        }
        let limit = limit.min(64);
        match self.pending.as_ref()? {
            PendingBlit::Normal(state) => Some(state.slot_access_pattern(limit)),
            PendingBlit::Line(state) => Some(state.slot_access_pattern(limit)),
        }
    }

    /// Whether the currently scheduled (pending) blit is a line blit. None
    /// when no blit is pending. Used by per-frame bus accounting.
    pub fn pending_is_line(&self) -> Option<bool> {
        match self.pending.as_ref()? {
            PendingBlit::Line(_) => Some(true),
            PendingBlit::Normal(_) => Some(false),
        }
    }

    pub fn finish_scheduled_now(&mut self, ram: &mut [u8]) -> bool {
        let Some(mut pending) = self.pending.take() else {
            return false;
        };
        match &mut pending {
            PendingBlit::Normal(state) => {
                state.run_to_completion(ram, &mut self.bzero);
                state.write_back(self);
                self.finish_blit();
            }
            PendingBlit::Line(state) => {
                state.run_to_completion(ram, &mut self.bzero);
                state.write_back(self);
                self.finish_blit();
            }
        }
        true
    }

    #[cfg(test)]
    fn execute_normal(&mut self, h: u32, w: u32, ram: &mut [u8]) {
        let con0 = self.bltcon0;
        let con1 = self.bltcon1;
        let use_a = con0 & BLTCON0_USE_A != 0;
        let use_b = con0 & BLTCON0_USE_B != 0;
        let use_c = con0 & BLTCON0_USE_C != 0;
        let use_d = con0 & BLTCON0_USE_D != 0;
        let write_d = use_d && con1 & BLTCON1_DOFF == 0;
        let ash = ((con0 >> 12) & 0x0F) as u32;
        let bsh = ((con1 >> 12) & 0x0F) as u32;
        let desc = con1 & BLTCON1_DESC != 0;
        let lf = (con0 & 0xFF) as u8;
        let ife = con1 & BLTCON1_IFE != 0;
        let efe = con1 & BLTCON1_EFE != 0;
        let fci = if con1 & BLTCON1_FCI != 0 { 1u16 } else { 0u16 };
        let fill = desc && (ife || efe);

        // Pointer step per word. In descending mode pointers count
        // downwards by 2. Use wrapping_add with the wrapped i32 to keep
        // the math identical for both directions.
        let step: i32 = if desc { -2 } else { 2 };
        let amod = if desc {
            -(self.bltamod as i32)
        } else {
            self.bltamod as i32
        };
        let bmod = if desc {
            -(self.bltbmod as i32)
        } else {
            self.bltbmod as i32
        };
        let cmod = if desc {
            -(self.bltcmod as i32)
        } else {
            self.bltcmod as i32
        };
        let dmod = if desc {
            -(self.bltdmod as i32)
        } else {
            self.bltdmod as i32
        };

        let mut apt = self.bltapt;
        let mut bpt = self.bltbpt;
        let mut cpt = self.bltcpt;
        let mut dpt = self.bltdpt;

        let mut a_prev: u16 = self.bltaold;
        let mut b_prev: u16 = self.bltbold;
        for _row in 0..h {
            let mut fill_state: u16 = fci;
            // Buffer this row's D words so fill mode can process them
            // in descending bit-order with carry across word boundaries.
            let mut row_d = Vec::with_capacity(w as usize);
            let mut row_dpt = Vec::with_capacity(w as usize);

            for word_idx in 0..w {
                let first = word_idx == 0;
                let last = word_idx == w - 1;

                let a_raw = if use_a {
                    let v = read_word(ram, apt);
                    apt = apt.wrapping_add(step as u32);
                    v
                } else {
                    self.bltadat
                };
                let mut a_masked = a_raw;
                if first {
                    a_masked &= self.bltafwm;
                }
                if last {
                    a_masked &= self.bltalwm;
                }
                let a = shift_combine(a_prev, a_masked, ash, desc);
                a_prev = a_masked;

                let b_raw = if use_b {
                    let v = read_word(ram, bpt);
                    bpt = bpt.wrapping_add(step as u32);
                    v
                } else {
                    self.bltbdat
                };
                let b = shift_combine(b_prev, b_raw, bsh, desc);
                b_prev = b_raw;

                let c = if use_c {
                    let v = read_word(ram, cpt);
                    cpt = cpt.wrapping_add(step as u32);
                    v
                } else {
                    self.bltcdat
                };

                let mut d = minterm(lf, a, b, c);
                if fill {
                    d = apply_fill(d, &mut fill_state, ife, efe);
                }

                if d != 0 {
                    self.bzero = false;
                }

                if use_d {
                    if write_d {
                        row_d.push(d);
                    }
                    row_dpt.push(dpt);
                    dpt = dpt.wrapping_add(step as u32);
                }
            }

            // Write D words for this row. Done after the row loop so a
            // future change can buffer for some other purpose; the
            // single-pass version above already does fill in the same
            // loop, so we just flush.
            for (pt, d) in row_dpt.iter().zip(row_d.iter()) {
                write_word(ram, *pt, *d);
            }

            // End of row: apply modulos to every enabled pointer.
            if use_a {
                apt = apt.wrapping_add(amod as u32);
            }
            if use_b {
                bpt = bpt.wrapping_add(bmod as u32);
            }
            if use_c {
                cpt = cpt.wrapping_add(cmod as u32);
            }
            if use_d {
                dpt = dpt.wrapping_add(dmod as u32);
            }
        }

        let ptr_mask = self.dma_ptr_mask();
        self.bltapt = apt & ptr_mask;
        self.bltbpt = bpt & ptr_mask;
        self.bltcpt = cpt & ptr_mask;
        self.bltdpt = dpt & ptr_mask;
        self.bltaold = a_prev;
        if use_b {
            self.bltbold = b_prev;
        }
    }

    /// Bresenham single-pixel line, BLTCON1.LINEMODE=1.
    ///
    /// Channel usage:
    /// - A: the single-bit texture word in BLTADAT (typically `$8000`),
    ///   masked by BLTAFWM and shifted by BLTCON0[15:12] = the start
    ///   pixel's column-within-word.
    /// - B: BLTBDAT carries the line-pattern mask; rotated left by 1
    ///   each pixel so dashed/dotted lines work.
    /// - C/D: C points at the destination word (read-modify-write);
    ///   line mode ignores the D enable bit and writes through C timing.
    /// - BLTAPT: signed Bresenham accumulator. Updated by BLTAMOD when
    ///   the minor step is taken, by BLTBMOD when it isn't.
    /// - BLTCMOD / BLTDMOD: bytes per bitplane row, for Y stepping.
    /// - BLTSIZE.H: number of pixels in the line (W is fixed to 2 by
    ///   software).
    ///
    /// Octant decoding (BLTCON1[4:2] = (SUD, SUL, AUL)):
    /// - SUD ("Sometimes Up/Down"): 0 = minor axis is X, major is Y.
    ///   1 = minor is Y, major is X.
    /// - SUL ("Sometimes Up/Left"): direction of the minor (sometimes-
    ///   stepped) axis. 0 = down/right (+), 1 = up/left (-).
    /// - AUL ("Always Up/Left"): direction of the major axis. 0 = +, 1 = -.
    ///
    /// SIGN semantics: the hardware uses BLTCON1.SIGN for the current
    /// pixel's sometimes step, then updates SIGN from BLTAPT for the next
    /// pixel after applying BLTAMOD/BLTBMOD.
    #[cfg(test)]
    fn execute_line(&mut self, npixels: u32, ram: &mut [u8]) {
        let con0 = self.bltcon0;
        let lf = (con0 & 0xFF) as u8;
        let ash = ((con0 >> 12) & 0x0F) as u32;
        let use_c = con0 & BLTCON0_USE_C != 0;

        let con1 = self.bltcon1;
        let mut bsh = (con1 >> 12) & 0x0F;
        let sing = con1 & BLTCON1_SING != 0;

        let bplmod = self.bltcmod as i32; // bytes per bitplane row
        let amod_step = self.bltamod as u16; // added when minor IS stepped
        let bmod_step = self.bltbmod as u16; // added when minor NOT stepped

        // We track X position purely through ASH (the within-word bit
        // position of the line pixel) and BLTCPT (the byte address of
        // the word that contains the pixel). Y position is implicit in
        // BLTCPT (one step = +/- bplmod).
        let mut cpt = self.bltcpt;
        let mut dpt = self.bltdpt;
        let mut ash_now = ash as i32;
        // Software stores the signed 16-bit error term in the low word
        // of BLTAPT. BLTCON1.SIGN supplies the first step decision; after
        // that, the low word's signed state drives the hardware state.
        let mut acc = self.bltapt as u16;
        let mut sign = con1 & BLTCON1_SIGN != 0;
        let mut one_dot = false;

        let mut bdat = self.line_initial_bdat(bsh);
        let a_word = self.bltadat & self.bltafwm;

        for _ in 0..npixels {
            let line_pixel = !sing || !one_dot;
            let a_shifted = if line_pixel {
                a_word >> (ash_now as u32)
            } else {
                0
            };
            one_dot = true;
            let b_shifted = if bdat & 1 != 0 { 0xFFFF } else { 0 };
            let c = if use_c {
                read_word(ram, cpt)
            } else {
                self.bltcdat
            };
            let d = minterm(lf, a_shifted, b_shifted, c);

            if !sign {
                ash_now = line_step_sometimes(con1, ash_now, bplmod, &mut cpt, &mut one_dot);
                acc = acc.wrapping_add(amod_step);
            } else {
                acc = acc.wrapping_add(bmod_step);
            }
            ash_now = line_step_always(con1, ash_now, bplmod, &mut cpt, &mut one_dot);
            sign = (acc as i16) < 0;

            if d != 0 {
                self.bzero = false;
            }
            if use_c && line_pixel {
                write_word(ram, dpt, d);
            }
            dpt = cpt;
            bdat = bdat.rotate_left(1);
            bsh = bsh.wrapping_sub(1) & 0x000F;
        }

        // Write back final state. Real hardware reflects the
        // accumulator's sign in BLTCON1.SIGN as a status bit; we do
        // the same for completeness even though software re-sets
        // BLTCON1 before each line.
        let mut con1 = self.bltcon1 & !BLTCON1_SIGN;
        if (acc as i16) < 0 {
            con1 |= BLTCON1_SIGN;
        }
        con1 = (con1 & 0x0FFF) | (bsh << 12);
        self.bltcon1 = con1;
        self.bltcon0 = (self.bltcon0 & 0x0FFF) | ((ash_now as u16 & 0x000F) << 12);
        let ptr_mask = self.dma_ptr_mask();
        self.bltcpt = cpt & ptr_mask;
        self.bltdpt = dpt & ptr_mask;
        self.bltapt = ((self.bltapt & CHIP_DMA_HIGH_MASK) | acc as u32) & ptr_mask;
    }

    fn line_initial_bdat(&self, bsh: u16) -> u16 {
        if self.line_bdat_valid {
            self.line_bdat
        } else {
            self.bltbdat.rotate_right(bsh as u32)
        }
    }
}

impl LineBlitState {
    fn new(blitter: &Blitter, npixels: u32) -> Self {
        let con0 = blitter.bltcon0;
        let con1 = blitter.bltcon1;
        let bsh = (con1 >> 12) & 0x0F;
        let bdat = blitter.line_initial_bdat(bsh);

        Self {
            phase: LineBlitPhase::StartDelay,
            slots_remaining: line_total_slots(npixels),
            npixels_remaining: npixels,
            con0,
            con1,
            lf: (con0 & 0xFF) as u8,
            use_c: con0 & BLTCON0_USE_C != 0,
            sing: con1 & BLTCON1_SING != 0,
            bplmod: blitter.bltcmod as i32,
            amod_step: blitter.bltamod as u16,
            bmod_step: blitter.bltbmod as u16,
            cpt: blitter.bltcpt,
            dpt: blitter.bltdpt,
            ash_now: ((con0 >> 12) & 0x0F) as i32,
            acc: blitter.bltapt as u16,
            sign: con1 & BLTCON1_SIGN != 0,
            one_dot: false,
            bdat,
            bsh,
            a_word: blitter.bltadat & blitter.bltafwm,
            bltcdat: blitter.bltcdat,
            cur_c: blitter.bltcdat,
        }
    }

    fn tick_slot(&mut self, ram: &mut [u8], bzero: &mut bool) -> bool {
        if self.slots_remaining == 0 {
            return true;
        }
        self.slots_remaining = self.slots_remaining.saturating_sub(1);
        self.process_phase(ram, bzero);
        self.slots_remaining == 0 || matches!(self.phase, LineBlitPhase::Done)
    }

    /// Whether the phase the next tick_slot will process is a chip-bus access.
    /// Per pixel the line engine reads C (L2) and writes D (L4); L1/L3 are
    /// internal Bresenham cycles that leave the bus free.
    fn current_slot_needs_bus(&self) -> bool {
        match self.phase {
            LineBlitPhase::L2 => self.use_c,
            LineBlitPhase::L4 => true,
            LineBlitPhase::StartDelay
            | LineBlitPhase::Init
            | LineBlitPhase::L1
            | LineBlitPhase::L3
            | LineBlitPhase::Done => false,
        }
    }

    /// Access pattern of the next `limit` scheduled slots (bit k = slot k needs
    /// the bus): 2 lead-in slots then the [L1, L2, L3, L4] cadence per pixel.
    fn slot_access_pattern(&self, limit: u32) -> (u64, u32) {
        let count = self.slots_remaining.min(limit).min(64);
        let cadence = [false, self.use_c, false, true];
        let (lead_in, cadence_idx) = match self.phase {
            LineBlitPhase::StartDelay => (2u32, 0usize),
            LineBlitPhase::Init => (1, 0),
            LineBlitPhase::L1 => (0, 0),
            LineBlitPhase::L2 => (0, 1),
            LineBlitPhase::L3 => (0, 2),
            LineBlitPhase::L4 | LineBlitPhase::Done => (0, 3),
        };
        let mut mask = 0u64;
        for k in 0..count {
            let needs = if k < lead_in {
                false
            } else {
                cadence[(cadence_idx + (k - lead_in) as usize) % 4]
            };
            if needs {
                mask |= 1u64 << k;
            }
        }
        (mask, count)
    }

    fn slots_remaining(&self) -> u32 {
        self.slots_remaining
    }

    fn run_to_completion(&mut self, ram: &mut [u8], bzero: &mut bool) {
        while self.slots_remaining != 0 {
            self.slots_remaining = self.slots_remaining.saturating_sub(1);
            self.process_phase(ram, bzero);
        }
    }

    fn process_phase(&mut self, ram: &mut [u8], bzero: &mut bool) {
        match self.phase {
            LineBlitPhase::StartDelay => self.phase = LineBlitPhase::Init,
            LineBlitPhase::Init => self.phase = LineBlitPhase::L1,
            LineBlitPhase::L1 => self.phase = LineBlitPhase::L2,
            LineBlitPhase::L2 => {
                self.cur_c = if self.use_c {
                    read_word(ram, self.cpt)
                } else {
                    self.bltcdat
                };
                self.phase = LineBlitPhase::L3;
            }
            LineBlitPhase::L3 => self.phase = LineBlitPhase::L4,
            LineBlitPhase::L4 => {
                self.process_latched_pixel(ram, bzero);
                self.phase = if self.npixels_remaining == 0 {
                    LineBlitPhase::Done
                } else {
                    LineBlitPhase::L1
                };
            }
            LineBlitPhase::Done => {}
        }
    }

    fn process_latched_pixel(&mut self, ram: &mut [u8], bzero: &mut bool) {
        let line_pixel = !self.sing || !self.one_dot;
        let a_shifted = if line_pixel {
            self.a_word >> (self.ash_now as u32)
        } else {
            0
        };
        self.one_dot = true;
        let b_shifted = if self.bdat & 1 != 0 { 0xFFFF } else { 0 };
        let d = minterm(self.lf, a_shifted, b_shifted, self.cur_c);

        if !self.sign {
            self.ash_now = line_step_sometimes(
                self.con1,
                self.ash_now,
                self.bplmod,
                &mut self.cpt,
                &mut self.one_dot,
            );
            self.acc = self.acc.wrapping_add(self.amod_step);
        } else {
            self.acc = self.acc.wrapping_add(self.bmod_step);
        }
        self.ash_now = line_step_always(
            self.con1,
            self.ash_now,
            self.bplmod,
            &mut self.cpt,
            &mut self.one_dot,
        );
        self.sign = (self.acc as i16) < 0;

        if d != 0 {
            *bzero = false;
        }
        if self.use_c && line_pixel {
            write_word(ram, self.dpt, d);
        }
        self.dpt = self.cpt;
        self.bdat = self.bdat.rotate_left(1);
        self.bsh = self.bsh.wrapping_sub(1) & 0x000F;
        self.npixels_remaining = self.npixels_remaining.saturating_sub(1);
    }

    fn write_back(&self, blitter: &mut Blitter) {
        let mut con1 = self.con1 & !BLTCON1_SIGN;
        if (self.acc as i16) < 0 {
            con1 |= BLTCON1_SIGN;
        }
        con1 = (con1 & 0x0FFF) | (self.bsh << 12);
        blitter.bltcon1 = con1;
        blitter.bltcon0 = (self.con0 & 0x0FFF) | ((self.ash_now as u16 & 0x000F) << 12);
        let ptr_mask = blitter.dma_ptr_mask();
        blitter.bltcpt = self.cpt & ptr_mask;
        blitter.bltdpt = self.dpt & ptr_mask;
        blitter.bltapt = ((blitter.bltapt & CHIP_DMA_HIGH_MASK) | self.acc as u32) & ptr_mask;
    }
}

impl NormalBlitState {
    fn new(blitter: &Blitter, h: u32, w: u32, ram: &[u8]) -> Self {
        let con0 = blitter.bltcon0;
        let con1 = blitter.bltcon1;
        let desc = con1 & BLTCON1_DESC != 0;
        let step: i32 = if desc { -2 } else { 2 };
        let mod_sign = if desc { -1 } else { 1 };
        let fci = if con1 & BLTCON1_FCI != 0 { 1u16 } else { 0u16 };
        let use_a = con0 & BLTCON0_USE_A != 0;
        let use_b = con0 & BLTCON0_USE_B != 0;
        let use_c = con0 & BLTCON0_USE_C != 0;
        let use_d = con0 & BLTCON0_USE_D != 0;
        let fill = desc && con1 & (BLTCON1_IFE | BLTCON1_EFE) != 0;

        // Pre-read the A and B source words in the exact order and at the
        // exact addresses the pipeline will consume them (one per word, w
        // words per row, advancing by `step` per word and the channel modulo
        // per row). See the snap_a/snap_b field comment for why.
        let snap_source = |enabled: bool, base: u32, modulo: i32| -> Vec<u16> {
            if !enabled || ram.is_empty() {
                return Vec::new();
            }
            let mut out = Vec::with_capacity((h * w) as usize);
            let mut ptr = base;
            for _row in 0..h {
                for _word in 0..w {
                    out.push(read_word(ram, ptr));
                    ptr = ptr.wrapping_add(step as u32);
                }
                ptr = ptr.wrapping_add(modulo as u32);
            }
            out
        };
        let snap_a = snap_source(use_a, blitter.bltapt, mod_sign * blitter.bltamod as i32);
        let snap_b = snap_source(use_b, blitter.bltbpt, mod_sign * blitter.bltbmod as i32);

        Self {
            phase: NormalBlitPhase::StartDelay,
            slots_remaining: normal_total_slots(h, w, con0, con1),
            h_remaining: h,
            w,
            word_idx: 0,
            lf: (con0 & 0xFF) as u8,
            use_a,
            use_b,
            use_c,
            use_d,
            write_d: use_d && con1 & BLTCON1_DOFF == 0,
            ash: ((con0 >> 12) & 0x0F) as u32,
            bsh: ((con1 >> 12) & 0x0F) as u32,
            desc,
            ife: fill && con1 & BLTCON1_IFE != 0,
            efe: fill && con1 & BLTCON1_EFE != 0,
            fci,
            step,
            amod: mod_sign * blitter.bltamod as i32,
            bmod: mod_sign * blitter.bltbmod as i32,
            cmod: mod_sign * blitter.bltcmod as i32,
            dmod: mod_sign * blitter.bltdmod as i32,
            bltafwm: blitter.bltafwm,
            bltalwm: blitter.bltalwm,
            bltadat: blitter.bltadat,
            bltbdat: blitter.bltbdat,
            bltcdat: blitter.bltcdat,
            apt: blitter.bltapt,
            bpt: blitter.bltbpt,
            cpt: blitter.bltcpt,
            dpt: blitter.bltdpt,
            a_prev: blitter.bltaold,
            b_prev: blitter.bltbold,
            cur_a: 0,
            cur_b: 0,
            cur_c: 0,
            fill_state: fci,
            pipeline_full: false,
            d_hold: 0,
            d_hold_pt: blitter.bltdpt,
            snap_a,
            snap_b,
            snap_a_idx: 0,
            snap_b_idx: 0,
            // Only self-overlap-capable blits (D plus a snapshotted source
            // channel) need the overlay; D-only or C-only blits skip it.
            track_overlay: use_d && (use_a || use_b),
            d_overlay: std::collections::HashMap::new(),
        }
    }

    fn disable_d_output(&mut self) {
        self.use_d = false;
        self.write_d = false;
        self.pipeline_full = false;
    }

    fn tick_slot(&mut self, ram: &mut [u8], bzero: &mut bool) -> bool {
        if self.slots_remaining == 0 {
            return true;
        }
        self.slots_remaining = self.slots_remaining.saturating_sub(1);
        self.process_phase(ram, bzero);
        self.slots_remaining == 0 || matches!(self.phase, NormalBlitPhase::Done)
    }

    /// Whether the phase the next tick_slot will process is a chip-bus access.
    /// The A/D phases exist in every blit's per-word cadence but only access
    /// memory when their channel is enabled; B/C phases are only entered when
    /// their channel is enabled. StartDelay/Init/E are internal cycles.
    fn current_slot_needs_bus(&self) -> bool {
        match self.phase {
            NormalBlitPhase::A => self.use_a,
            NormalBlitPhase::B => true,
            // A real C fetch uses the bus; fill mode's C slot is idle.
            NormalBlitPhase::C => self.use_c,
            NormalBlitPhase::D => self.d_phase_needs_bus_with(self.pipeline_full),
            NormalBlitPhase::F => self.use_d && self.pipeline_full,
            NormalBlitPhase::StartDelay
            | NormalBlitPhase::Init
            | NormalBlitPhase::E
            | NormalBlitPhase::Done => false,
        }
    }

    fn d_phase_needs_bus_with(&self, pipeline_full: bool) -> bool {
        self.use_d && (pipeline_full || !self.d_output_waits_for_source_pipeline())
    }

    fn d_output_waits_for_source_pipeline(&self) -> bool {
        self.use_a || self.use_b || self.has_c_phase()
    }

    /// Access pattern of the next `limit` scheduled slots (bit k = slot k needs
    /// the bus). Mirrors process_phase, including the delayed D holding
    /// register: the first D phase after source fetches is the HRM "-" bubble
    /// until a destination word has been queued.
    fn slot_access_pattern(&self, limit: u32) -> (u64, u32) {
        let count = self.slots_remaining.min(limit).min(64);
        let mut mask = 0u64;

        let mut phase = self.phase;
        let mut pipeline_full = self.pipeline_full;
        let mut word_idx = self.word_idx;
        let mut h_remaining = self.h_remaining;

        for k in 0..count {
            let needs = match phase {
                NormalBlitPhase::A => self.use_a,
                NormalBlitPhase::B => true,
                NormalBlitPhase::C => self.use_c,
                NormalBlitPhase::D => self.d_phase_needs_bus_with(pipeline_full),
                NormalBlitPhase::F => self.use_d && pipeline_full,
                NormalBlitPhase::StartDelay
                | NormalBlitPhase::Init
                | NormalBlitPhase::E
                | NormalBlitPhase::Done => false,
            };
            if needs {
                mask |= 1u64 << k;
            }

            match phase {
                NormalBlitPhase::StartDelay => phase = NormalBlitPhase::Init,
                NormalBlitPhase::Init => phase = NormalBlitPhase::A,
                NormalBlitPhase::A => {
                    phase = if self.use_b {
                        NormalBlitPhase::B
                    } else if self.has_c_phase() {
                        NormalBlitPhase::C
                    } else {
                        NormalBlitPhase::D
                    };
                }
                NormalBlitPhase::B => {
                    phase = if self.has_c_phase() {
                        NormalBlitPhase::C
                    } else {
                        NormalBlitPhase::D
                    };
                }
                NormalBlitPhase::C => {
                    if self.use_d {
                        phase = NormalBlitPhase::D;
                    } else {
                        let done =
                            Self::advance_pattern_word(self.w, &mut word_idx, &mut h_remaining);
                        phase = if done {
                            NormalBlitPhase::Done
                        } else {
                            NormalBlitPhase::A
                        };
                    }
                }
                NormalBlitPhase::D => {
                    pipeline_full = self.use_d;
                    let done = Self::advance_pattern_word(self.w, &mut word_idx, &mut h_remaining);
                    phase = if done {
                        if self.use_d {
                            NormalBlitPhase::E
                        } else {
                            NormalBlitPhase::Done
                        }
                    } else {
                        NormalBlitPhase::A
                    };
                }
                NormalBlitPhase::E => phase = NormalBlitPhase::F,
                NormalBlitPhase::F => {
                    pipeline_full = false;
                    phase = NormalBlitPhase::Done;
                }
                NormalBlitPhase::Done => {}
            }
        }
        (mask, count)
    }

    fn advance_pattern_word(w: u32, word_idx: &mut u32, h_remaining: &mut u32) -> bool {
        *word_idx += 1;
        if *word_idx == w {
            *word_idx = 0;
            *h_remaining = h_remaining.saturating_sub(1);
        }
        *h_remaining == 0
    }

    fn slots_remaining(&self) -> u32 {
        self.slots_remaining
    }

    fn run_to_completion(&mut self, ram: &mut [u8], bzero: &mut bool) {
        while self.slots_remaining != 0 {
            self.slots_remaining = self.slots_remaining.saturating_sub(1);
            self.process_phase(ram, bzero);
        }
    }

    fn process_phase(&mut self, ram: &mut [u8], bzero: &mut bool) {
        match self.phase {
            NormalBlitPhase::StartDelay => self.phase = NormalBlitPhase::Init,
            NormalBlitPhase::Init => self.phase = NormalBlitPhase::A,
            NormalBlitPhase::A => {
                self.begin_word();
                self.fetch_a(ram);
                self.phase = if self.use_b {
                    NormalBlitPhase::B
                } else if self.has_c_phase() {
                    NormalBlitPhase::C
                } else {
                    NormalBlitPhase::D
                };
            }
            NormalBlitPhase::B => {
                self.fetch_b(ram);
                self.phase = if self.has_c_phase() {
                    NormalBlitPhase::C
                } else {
                    NormalBlitPhase::D
                };
            }
            NormalBlitPhase::C => {
                // Fill mode's C slot is idle (USEC clear): begin_word already
                // set cur_c = bltcdat, so do not fetch from BLTCPT.
                if self.use_c {
                    self.fetch_c(ram);
                }
                if self.use_d {
                    self.phase = NormalBlitPhase::D;
                } else {
                    let done = self.finish_source_word(bzero);
                    self.phase = if done {
                        NormalBlitPhase::Done
                    } else {
                        NormalBlitPhase::A
                    };
                }
            }
            NormalBlitPhase::D => {
                self.write_queued_d(ram);
                let done = self.finish_source_word(bzero);
                self.phase = if done {
                    if self.use_d {
                        NormalBlitPhase::E
                    } else {
                        NormalBlitPhase::Done
                    }
                } else {
                    NormalBlitPhase::A
                };
            }
            NormalBlitPhase::E => self.phase = NormalBlitPhase::F,
            NormalBlitPhase::F => {
                self.write_queued_d(ram);
                self.phase = NormalBlitPhase::Done;
            }
            NormalBlitPhase::Done => {}
        }
    }

    fn begin_word(&mut self) {
        // Channels whose pipeline phase is skipped this word still latch
        // their data-register value here. The A channel always has its
        // phase slot (fetch_a handles both the fetched and the
        // BLTADAT-driven case), so it must NOT be computed here as well:
        // doing so advanced the A barrel shifter twice per word, which
        // mis-shifted BLTADAT window masks whenever ASH was non-zero
        // (the CD32 boot intro's cookie-cut letter-rotation blits).
        if !self.use_b {
            let b_raw = self.bltbdat;
            self.cur_b = shift_combine(self.b_prev, b_raw, self.bsh, self.desc);
            self.b_prev = b_raw;
        }
        if !self.use_c {
            self.cur_c = self.bltcdat;
        }
    }

    fn has_c_phase(&self) -> bool {
        // A real C bus cycle (USEC), or fill mode's idle C slot. Fill consumes
        // the C cycle even with USEC clear (begin_word already supplied cur_c =
        // bltcdat); the slot is idle, not a fetch (see process_phase C and
        // current_slot_needs_bus). Real hardware times it -- see
        // normal_source_slots_per_word and docs/internals/timing.md.
        self.use_c || self.ife || self.efe
    }

    fn mask_a_word(&self, raw: u16) -> u16 {
        let first = self.word_idx == 0;
        let last = self.word_idx == self.w - 1;

        let mut masked = raw;
        if first {
            masked &= self.bltafwm;
        }
        if last {
            masked &= self.bltalwm;
        }
        masked
    }

    // Read a snapshotted source word, but prefer a word this blit has already
    // written through D at the same address (self-overlap; see d_overlay).
    fn overlay_read(&self, addr: u32, snap: u16, ram_len: usize) -> u16 {
        if self.track_overlay {
            if let Some(off) = chip_off(addr, ram_len) {
                if let Some(&v) = self.d_overlay.get(&off) {
                    return v;
                }
            }
        }
        snap
    }

    fn fetch_a(&mut self, ram: &[u8]) {
        let a_raw = if self.use_a {
            // Source was snapshotted at BLTSIZE; pointer still advances so the
            // post-blit BLTAPT write-back matches hardware.
            let addr = self.apt;
            let snap = self.snap_a.get(self.snap_a_idx).copied().unwrap_or(0);
            self.snap_a_idx += 1;
            self.apt = self.apt.wrapping_add(self.step as u32);
            self.overlay_read(addr, snap, ram.len())
        } else {
            self.bltadat
        };
        let a_masked = self.mask_a_word(a_raw);
        let a = shift_combine(self.a_prev, a_masked, self.ash, self.desc);
        self.a_prev = a_masked;
        self.cur_a = a;
    }

    fn fetch_b(&mut self, ram: &[u8]) {
        let b_raw = if self.use_b {
            let addr = self.bpt;
            let snap = self.snap_b.get(self.snap_b_idx).copied().unwrap_or(0);
            self.snap_b_idx += 1;
            self.bpt = self.bpt.wrapping_add(self.step as u32);
            self.overlay_read(addr, snap, ram.len())
        } else {
            self.bltbdat
        };
        let b = shift_combine(self.b_prev, b_raw, self.bsh, self.desc);
        self.b_prev = b_raw;
        self.cur_b = b;
    }

    fn fetch_c(&mut self, ram: &[u8]) {
        self.cur_c = if self.use_c {
            let v = read_word(ram, self.cpt);
            self.cpt = self.cpt.wrapping_add(self.step as u32);
            v
        } else {
            self.bltcdat
        };
    }

    fn write_queued_d(&mut self, ram: &mut [u8]) {
        if !self.pipeline_full {
            return;
        }
        if self.write_d {
            write_word(ram, self.d_hold_pt, self.d_hold);
            if self.track_overlay {
                if let Some(off) = chip_off(self.d_hold_pt, ram.len()) {
                    self.d_overlay.insert(off, self.d_hold);
                }
            }
        }
        self.pipeline_full = false;
    }

    fn finish_source_word(&mut self, bzero: &mut bool) -> bool {
        let mut d = minterm(self.lf, self.cur_a, self.cur_b, self.cur_c);
        if self.ife || self.efe {
            d = apply_fill(d, &mut self.fill_state, self.ife, self.efe);
        }
        if d != 0 {
            *bzero = false;
        }
        if self.use_d {
            self.d_hold = d;
            self.d_hold_pt = self.dpt;
            self.pipeline_full = true;
            self.dpt = self.dpt.wrapping_add(self.step as u32);
        }
        self.advance_word()
    }

    fn advance_word(&mut self) -> bool {
        self.word_idx += 1;
        if self.word_idx == self.w {
            self.end_row();
        }
        self.h_remaining == 0
    }

    fn end_row(&mut self) {
        if self.use_a {
            self.apt = self.apt.wrapping_add(self.amod as u32);
        }
        if self.use_b {
            self.bpt = self.bpt.wrapping_add(self.bmod as u32);
        }
        if self.use_c {
            self.cpt = self.cpt.wrapping_add(self.cmod as u32);
        }
        if self.use_d {
            self.dpt = self.dpt.wrapping_add(self.dmod as u32);
        }
        self.h_remaining = self.h_remaining.saturating_sub(1);
        self.word_idx = 0;
        self.fill_state = self.fci;
    }

    fn write_back(&self, blitter: &mut Blitter) {
        let ptr_mask = blitter.dma_ptr_mask();
        blitter.bltapt = self.apt & ptr_mask;
        blitter.bltbpt = self.bpt & ptr_mask;
        blitter.bltcpt = self.cpt & ptr_mask;
        blitter.bltdpt = self.dpt & ptr_mask;
        blitter.bltaold = self.a_prev;
        if self.use_b {
            blitter.bltbold = self.b_prev;
        }
    }
}

fn decode_bltsize(bltsize: u16) -> (u32, u32) {
    let mut h = ((bltsize >> 6) & 0x3FF) as u32;
    if h == 0 {
        h = 1024;
    }
    let mut w = (bltsize & 0x3F) as u32;
    if w == 0 {
        w = 64;
    }
    (h, w)
}

fn decode_ecs_bltsize(bltsizv: u16, bltsizh: u16) -> (u32, u32) {
    let mut h = (bltsizv & 0x7FFF) as u32;
    if h == 0 {
        h = 32_768;
    }
    let mut w = (bltsizh & 0x07FF) as u32;
    if w == 0 {
        w = 2_048;
    }
    (h, w)
}

fn normal_source_slots_per_word(con0: u16, con1: u16) -> u32 {
    // Normal mode always enters the A state, then conditionally visits
    // B and C before the D/next-word state. The D result itself is
    // pipeline-delayed; this count is just the repeating source cadence.
    //
    // Per-word cost is the number of channel slots A/B/C/D, EXCEPT that area
    // fill (IFE/EFE) consumes the C slot even when USEC is clear: that slot is
    // an idle cycle (no bus access -- see current_slot_needs_bus), the "-" in
    // the HRM "A - D" fill cadence. Cross-emulator timing (FS-UAE and vAmiga
    // both report an A->D area fill at 3 cck/word vs 2 for an A->D copy --
    // timing-test rows 23/24/26) confirms the slot is real, not phantom.
    // (A previous change dropped it to speed one frame-budget regression; that
    // masked a separate timing bug. See docs/internals/timing.md.)
    let use_b = con0 & BLTCON0_USE_B != 0;
    let use_c = con0 & BLTCON0_USE_C != 0;
    let use_d = con0 & BLTCON0_USE_D != 0;
    let desc = con1 & BLTCON1_DESC != 0;
    let fill = desc && con1 & (BLTCON1_IFE | BLTCON1_EFE) != 0;
    let c_phase = use_c || fill;
    let d_phase = use_d || !c_phase;
    1 + u32::from(use_b) + u32::from(c_phase) + u32::from(d_phase)
}

fn normal_total_slots(h: u32, w: u32, con0: u16, con1: u16) -> u32 {
    let words = h.saturating_mul(w);
    if words == 0 {
        return 1;
    }
    let use_d = con0 & BLTCON0_USE_D != 0;
    2 + words.saturating_mul(normal_source_slots_per_word(con0, con1)) + if use_d { 2 } else { 0 }
}

fn line_total_slots(npixels: u32) -> u32 {
    2 + npixels.saturating_mul(4)
}

fn line_step_sometimes(
    bltcon1: u16,
    ash: i32,
    bplmod: i32,
    cpt: &mut u32,
    one_dot: &mut bool,
) -> i32 {
    if bltcon1 & 0x0010 != 0 {
        if bltcon1 & 0x0008 != 0 {
            line_step_y(-1, bplmod, cpt, one_dot);
        } else {
            line_step_y(1, bplmod, cpt, one_dot);
        }
        ash
    } else if bltcon1 & 0x0008 != 0 {
        line_step_x(ash, -1, cpt)
    } else {
        line_step_x(ash, 1, cpt)
    }
}

fn line_step_always(bltcon1: u16, ash: i32, bplmod: i32, cpt: &mut u32, one_dot: &mut bool) -> i32 {
    if bltcon1 & 0x0010 != 0 {
        if bltcon1 & 0x0004 != 0 {
            line_step_x(ash, -1, cpt)
        } else {
            line_step_x(ash, 1, cpt)
        }
    } else {
        if bltcon1 & 0x0004 != 0 {
            line_step_y(-1, bplmod, cpt, one_dot);
        } else {
            line_step_y(1, bplmod, cpt, one_dot);
        }
        ash
    }
}

fn line_step_x(ash: i32, dx: i32, cpt: &mut u32) -> i32 {
    if dx > 0 {
        let mut n = ash + 1;
        if n > 15 {
            n = 0;
            *cpt = cpt.wrapping_add(2);
        }
        n
    } else {
        let mut n = ash - 1;
        if n < 0 {
            n = 15;
            *cpt = cpt.wrapping_sub(2);
        }
        n
    }
}

fn line_step_y(dy: i32, bplmod: i32, cpt: &mut u32, one_dot: &mut bool) {
    let delta = if dy > 0 { bplmod } else { -bplmod };
    *cpt = cpt.wrapping_add(delta as u32);
    *one_dot = false;
}

/// Apply area-fill (inclusive or exclusive) to a single D word in
/// descending bit-order, carrying `fill_state` across calls within a
/// row.
fn apply_fill(d: u16, fill_state: &mut u16, ife: bool, efe: bool) -> u16 {
    let mut out = d;
    for bit in 0..16 {
        let mask = 1 << bit;
        if *fill_state != 0 {
            if ife {
                out |= mask;
            } else if efe {
                out ^= mask;
            }
        }
        if d & mask != 0 {
            *fill_state ^= 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A->D copy with the source pre-loaded into chip RAM. BLTCON0 =
    /// `0x09F0` (USE A + USE D + minterm $F0 == D := A), no shift, no
    /// masks excluded. Verifies the normal-mode pipeline end-to-end.
    /// A disabled-A blit shifts the BLTADAT window mask through the A
    /// barrel shifter exactly once per word. The scheduled pipeline used
    /// to compute the A channel twice per word (begin_word and fetch_a),
    /// double-advancing the shifter and mis-shifting BLTADAT cookie-cut
    /// windows whenever ASH was non-zero - the CD32 boot intro's
    /// letter-rotation blits scattered sprite strips because of it.
    #[test]
    fn scheduled_disabled_a_window_mask_shifts_once_per_word() {
        let mut ram = vec![0u8; 256];
        let snapshot = ram.clone();
        let mut b = Blitter::new();
        b.bltcon0 = 0x41F0; // ASH=4, USED only, minterm $F0 (D=A)
        b.bltcon1 = 0x0000;
        b.bltafwm = 0xFC00;
        b.bltalwm = 0x001F;
        b.bltadat = 0xFFFF;
        b.bltdpt = 0x20;
        b.start_scheduled((1 << 6) | 3, &snapshot);
        while !b.tick_scheduled_slot(&mut ram) {}
        // The shifted window: word0 = (0:FC00)>>4, word1 = (FC00:FFFF)>>4,
        // word2 = (FFFF:001F)>>4.
        assert_eq!(&ram[0x20..0x26], &[0x0F, 0xC0, 0x0F, 0xFF, 0xF0, 0x01]);
    }

    #[test]
    fn normal_mode_copy() {
        let mut ram = vec![0u8; 256];
        // Source bytes at offset 0x10: 0x11 0x22 0x33 0x44
        ram[0x10] = 0x11;
        ram[0x11] = 0x22;
        ram[0x12] = 0x33;
        ram[0x13] = 0x44;
        let mut b = Blitter::new();
        b.bltcon0 = 0x09F0; // USEA|USED, minterm=$F0 (D=A), ASH=0
        b.bltcon1 = 0x0000;
        b.bltafwm = 0xFFFF;
        b.bltalwm = 0xFFFF;
        b.bltapt = 0x10;
        b.bltdpt = 0x20;
        // 1 row, 2 words
        let bltsize = (1u16 << 6) | 2;
        b.execute(bltsize, &mut ram);
        assert_eq!(&ram[0x20..0x24], &[0x11, 0x22, 0x33, 0x44]);
        assert!(!b.bzero);
        assert!(!b.busy);
    }

    #[test]
    fn normal_mode_wraps_to_configured_chip_ram_window() {
        let mut ram = vec![0u8; 512 * 1024];
        ram[0x5F70] = 0xAA;
        ram[0x5F71] = 0x55;

        let mut b = Blitter::new();
        b.bltcon0 = 0x0100; // USE D, minterm=$00 clears destination.
        b.bltdpt = 0x085F70;
        b.execute(0x0001, &mut ram);

        assert_eq!(&ram[0x5F70..0x5F72], &[0x00, 0x00]);
    }

    /// A->D copy with BLTAFWM = $00FF (zero the high byte of the first
    /// word) and BLTALWM = $FF00 (zero the low byte of the last word).
    #[test]
    fn normal_mode_masks() {
        let mut ram = vec![0u8; 256];
        ram[0x10] = 0xAA;
        ram[0x11] = 0xBB;
        ram[0x12] = 0xCC;
        ram[0x13] = 0xDD;
        let mut b = Blitter::new();
        b.bltcon0 = 0x09F0;
        b.bltcon1 = 0x0000;
        b.bltafwm = 0x00FF;
        b.bltalwm = 0xFF00;
        b.bltapt = 0x10;
        b.bltdpt = 0x20;
        let bltsize = (1u16 << 6) | 2;
        b.execute(bltsize, &mut ram);
        assert_eq!(&ram[0x20..0x24], &[0x00, 0xBB, 0xCC, 0x00]);
    }

    /// 4-bit right shift of a single source word into the destination.
    /// The barrel shifter feeds the previous source word in as the new
    /// high bits; for the first word (prev = 0) we get a clean shift.
    #[test]
    fn normal_mode_shift_a() {
        let mut ram = vec![0u8; 256];
        ram[0x10] = 0xF0; // 0xF000 source word
        ram[0x11] = 0x00;
        let mut b = Blitter::new();
        b.bltcon0 = (4 << 12) | 0x09F0; // ASH=4, USEA|USED, D=A
        b.bltcon1 = 0;
        b.bltafwm = 0xFFFF;
        b.bltalwm = 0xFFFF;
        b.bltapt = 0x10;
        b.bltdpt = 0x20;
        let bltsize = (1u16 << 6) | 1;
        b.execute(bltsize, &mut ram);
        // $F000 >> 4 = $0F00, prev = 0 so no carry-in.
        assert_eq!(&ram[0x20..0x22], &[0x0F, 0x00]);
    }

    /// Two-word span with non-zero prev: bits shifted out of the first
    /// word reappear in the high bits of the second.
    #[test]
    fn normal_mode_shift_carry() {
        let mut ram = vec![0u8; 256];
        // Source: $1111 $2222
        ram[0x10] = 0x11;
        ram[0x11] = 0x11;
        ram[0x12] = 0x22;
        ram[0x13] = 0x22;
        let mut b = Blitter::new();
        b.bltcon0 = (4 << 12) | 0x09F0;
        b.bltcon1 = 0;
        b.bltafwm = 0xFFFF;
        b.bltalwm = 0xFFFF;
        b.bltapt = 0x10;
        b.bltdpt = 0x20;
        let bltsize = (1u16 << 6) | 2;
        b.execute(bltsize, &mut ram);
        // Word 0: $1111 >> 4 = $0111. Word 1: ($1111 << 12) | ($2222 >> 4)
        //         = $1000 | $0222 = $1222.
        assert_eq!(&ram[0x20..0x24], &[0x01, 0x11, 0x12, 0x22]);
    }

    /// Normal-mode A/B barrel shifters are not cleared by the BLTSIZE row
    /// counter. Masks and modulos still apply per row, but the shifter's
    /// previous-word latch carries from the last word of one row into the
    /// first word of the next.
    #[test]
    fn scheduled_shift_carry_crosses_normal_mode_row_boundary() {
        let mut ram = vec![0u8; 256];
        for (addr, word) in [
            (0x10, 0x1111),
            (0x12, 0x2222),
            (0x14, 0x3333),
            (0x16, 0x4444),
        ] {
            write_word(&mut ram, addr, word);
        }

        let mut b = Blitter::new();
        b.bltcon0 = (4 << 12) | 0x09F0; // ASH=4, USEA|USED, D=A
        b.bltcon1 = 0;
        b.bltafwm = 0xFFFF;
        b.bltalwm = 0xFFFF;
        b.bltapt = 0x10;
        b.bltdpt = 0x20;
        b.start_scheduled((2u16 << 6) | 2, &ram);
        while !b.tick_scheduled_slot(&mut ram) {}

        assert_eq!(
            &ram[0x20..0x28],
            &[0x01, 0x11, 0x12, 0x22, 0x23, 0x33, 0x34, 0x44]
        );
    }

    /// The A-channel barrel shifter's old-word latch is not cleared by
    /// BLTSIZE. Split shifted blits can carry a boundary bit through BLTAOLD
    /// when software starts the next blit without writing BLTADAT.
    #[test]
    fn scheduled_a_shift_carry_survives_bltsize_restart() {
        let mut ram = vec![0u8; 256];
        write_word(&mut ram, 0x10, 0x0001);
        write_word(&mut ram, 0x12, 0x8000);

        let mut b = Blitter::new();
        b.bltcon0 = (1 << 12) | BLTCON0_USE_A | BLTCON0_USE_D | 0x00F0;
        b.bltcon1 = 0;
        b.bltafwm = 0xFFFF;
        b.bltalwm = 0xFFFF;
        b.bltapt = 0x10;
        b.bltdpt = 0x20;
        b.start_scheduled((1u16 << 6) | 1, &ram);
        while !b.tick_scheduled_slot(&mut ram) {}
        assert_eq!(read_word(&ram, 0x20), 0x0000);

        b.bltapt = 0x12;
        b.bltdpt = 0x22;
        b.start_scheduled((1u16 << 6) | 1, &ram);
        while !b.tick_scheduled_slot(&mut ram) {}

        assert_eq!(read_word(&ram, 0x22), 0xC000);
    }

    /// Verify BZERO surfaces correctly when all output bits are zero.
    #[test]
    fn bzero_when_d_all_zero() {
        let mut ram = vec![0u8; 256];
        // Source is all zeros; D = A = 0.
        let mut b = Blitter::new();
        b.bltcon0 = 0x09F0;
        b.bltcon1 = 0;
        b.bltafwm = 0xFFFF;
        b.bltalwm = 0xFFFF;
        b.bltapt = 0x10;
        b.bltdpt = 0x20;
        let bltsize = (1u16 << 6) | 2;
        b.execute(bltsize, &mut ram);
        assert!(b.bzero);
    }

    #[test]
    fn four_by_four_c2p_blitter_chain_matches_direct_planar_conversion() {
        assert_c2p_4x4_blitter_chain_matches_direct_planar_conversion(49);
        assert_c2p_4x4_blitter_chain_matches_direct_planar_conversion(102);
    }

    fn assert_c2p_4x4_blitter_chain_matches_direct_planar_conversion(chunky_h: usize) {
        const CHUNKY_W: usize = 80;
        let chunky_words = CHUNKY_W * chunky_h;
        let screen_bpl_words = chunky_words / 4;
        let screen_bpl_bytes = screen_bpl_words * 2;
        let screen_bytes = screen_bpl_bytes * 4;
        const CHUNKY: usize = 0x1000;
        const TMP: usize = 0x6000;
        const DRAW: usize = 0xB000;

        let mut ram = vec![0u8; 0x10000];
        let mut chunky = Vec::with_capacity(chunky_words);
        for i in 0..chunky_words {
            let word = ((i as u16).wrapping_mul(0x4D3B)).rotate_left((i & 15) as u32);
            chunky.push(word);
            write_word(&mut ram, (CHUNKY + i * 2) as u32, word);
        }

        run_c2p_4x4_blits(
            &mut ram,
            CHUNKY as u32,
            TMP as u32,
            DRAW as u32,
            chunky_h as u16,
        );

        let mut expected = vec![0u8; screen_bytes];
        for group in 0..screen_bpl_words {
            let a = chunky[group * 4];
            let b = chunky[group * 4 + 1];
            let c = chunky[group * 4 + 2];
            let d = chunky[group * 4 + 3];
            for plane in 0..4 {
                let shift = plane * 4;
                let out = (((a >> shift) & 0x000F) << 12)
                    | (((b >> shift) & 0x000F) << 8)
                    | (((c >> shift) & 0x000F) << 4)
                    | ((d >> shift) & 0x000F);
                write_word(
                    &mut expected,
                    (plane * screen_bpl_bytes + group * 2) as u32,
                    out,
                );
            }
        }

        let actual = &ram[DRAW..DRAW + screen_bytes];
        if actual != expected.as_slice() {
            let mismatch = actual
                .iter()
                .zip(expected.iter())
                .position(|(actual, expected)| actual != expected)
                .unwrap_or(0);
            panic!(
                "4x4 C2P mismatch for chunky_h={chunky_h} at output byte {mismatch}: actual={:#04X} expected={:#04X}",
                actual[mismatch], expected[mismatch]
            );
        }
    }

    fn run_c2p_4x4_blits(ram: &mut [u8], chunky: u32, tmp: u32, draw: u32, chunky_h: u16) {
        const CHUNKY_W: u16 = 80;
        let c2p_bpl = (CHUNKY_W / 2) * chunky_h;
        let c2p_bpl3 = c2p_bpl * 3;
        let c2p_screen_size = c2p_bpl * 4;
        let c2p_blit_size = ((c2p_screen_size >> 4) << 6) + 1;

        let mut b = Blitter::new();
        b.bltafwm = 0xFFFF;
        b.bltalwm = 0xFFFF;

        b.bltbmod = 4;
        b.bltamod = 4;
        b.bltdmod = 4;
        b.bltcdat = 0x00FF;
        b.bltcon0 = (0x0DE4 | (8 << 12)) as u16;
        b.bltcon1 = 0;
        b.bltbpt = chunky;
        b.bltapt = chunky + 4;
        b.bltdpt = tmp;
        b.execute(c2p_blit_size + 1, ram);
        b.execute(c2p_blit_size + 1, ram);

        b.bltcon0 = (0x0DD8 | (8 << 12)) as u16;
        b.bltcon1 = BLTCON1_DESC;
        b.bltapt = chunky + c2p_screen_size as u32 - 6;
        b.bltbpt = chunky + c2p_screen_size as u32 - 2;
        b.bltdpt = tmp + c2p_screen_size as u32 - 2;
        b.execute(c2p_blit_size + 1, ram);
        b.execute(c2p_blit_size + 1, ram);

        b.bltbmod = 6;
        b.bltamod = 6;
        b.bltdmod = 0;
        b.bltcdat = 0x0F0F;
        b.bltcon0 = (0x0DE4 | (4 << 12)) as u16;
        b.bltcon1 = 0;
        b.bltbpt = tmp;
        b.bltapt = tmp + 2;
        b.bltdpt = draw + c2p_bpl3 as u32;
        b.execute(c2p_blit_size, ram);
        b.execute(c2p_blit_size, ram);

        b.bltbpt = tmp + 4;
        b.bltapt = tmp + 6;
        b.bltdpt = draw + c2p_bpl as u32;
        b.execute(c2p_blit_size, ram);
        b.execute(c2p_blit_size, ram);

        b.bltcon0 = (0x0DD8 | (4 << 12)) as u16;
        b.bltcon1 = BLTCON1_DESC;
        b.bltapt = tmp + c2p_screen_size as u32 - 8;
        b.bltbpt = tmp + c2p_screen_size as u32 - 6;
        b.bltdpt = draw + c2p_bpl3 as u32 - 2;
        b.execute(c2p_blit_size, ram);
        b.execute(c2p_blit_size, ram);

        b.bltapt = tmp + c2p_screen_size as u32 - 4;
        b.bltbpt = tmp + c2p_screen_size as u32 - 2;
        b.bltdpt = draw + c2p_bpl as u32 - 2;
        b.execute(c2p_blit_size, ram);
        b.execute(c2p_blit_size, ram);
    }

    #[test]
    fn scheduled_normal_clear_writes_progressively() {
        let mut ram = vec![0xAAu8; 256];
        let mut b = Blitter::new();
        b.bltcon0 = 0x0100; // USE D, minterm $00 clears destination.
        b.bltdpt = 0x20;
        let bltsize = (1u16 << 6) | 2;

        b.start_scheduled(bltsize, &ram);

        assert!(b.busy);
        assert_eq!(b.scheduled_slots_remaining(), Some(8));
        assert_eq!(&ram[0x20..0x24], &[0xAA, 0xAA, 0xAA, 0xAA]);
        assert!(!b.tick_scheduled_slot(&mut ram));
        assert_eq!(b.scheduled_slots_remaining(), Some(7));
        assert!(b.busy);
        assert_eq!(&ram[0x20..0x24], &[0xAA, 0xAA, 0xAA, 0xAA]);
        assert!(!b.tick_scheduled_slot(&mut ram));
        assert_eq!(b.scheduled_slots_remaining(), Some(6));
        assert!(b.busy);
        assert_eq!(&ram[0x20..0x24], &[0xAA, 0xAA, 0xAA, 0xAA]);
        assert!(!b.tick_scheduled_slot(&mut ram));
        assert_eq!(b.scheduled_slots_remaining(), Some(5));
        assert!(b.busy);
        assert_eq!(&ram[0x20..0x24], &[0xAA, 0xAA, 0xAA, 0xAA]);
        assert!(!b.tick_scheduled_slot(&mut ram));
        assert_eq!(&ram[0x20..0x24], &[0xAA, 0xAA, 0xAA, 0xAA]);
        assert!(!b.tick_scheduled_slot(&mut ram));
        assert_eq!(&ram[0x20..0x24], &[0xAA, 0xAA, 0xAA, 0xAA]);
        assert!(!b.tick_scheduled_slot(&mut ram));
        assert_eq!(&ram[0x20..0x24], &[0x00, 0x00, 0xAA, 0xAA]);
        assert!(!b.tick_scheduled_slot(&mut ram));
        assert_eq!(&ram[0x20..0x24], &[0x00, 0x00, 0xAA, 0xAA]);
        assert!(b.tick_scheduled_slot(&mut ram));
        assert!(!b.busy);
        assert_eq!(b.scheduled_slots_remaining(), None);
        assert_eq!(&ram[0x20..0x24], &[0x00, 0x00, 0x00, 0x00]);
    }

    /// Maps each scheduled pipeline slot to whether it performs a chip-bus
    /// access, per the HRM blitter cycle diagrams. The idle slots ("-" in the
    /// HRM diagrams) are available to the CPU/other DMA on real hardware;
    /// current_slot_needs_bus is the hook for the bus to model that.
    #[test]
    fn blit_pipeline_identifies_idle_cycles_per_hrm_diagrams() {
        fn needs_bus_walk(b: &mut Blitter, ram: &mut [u8]) -> Vec<bool> {
            let mut pattern = Vec::new();
            loop {
                pattern.push(b.current_slot_needs_bus());
                if b.tick_scheduled_slot(ram) {
                    break;
                }
            }
            pattern
        }

        // D-only clear, 1 row x 2 words: StartDelay, Init, [A D] x2, E, F.
        // Only the D write slots and the final F flush access the bus; the A
        // slots are idle because A DMA is disabled (HRM: "- D0" per word).
        let mut ram = vec![0xAAu8; 256];
        let mut b = Blitter::new();
        b.bltcon0 = BLTCON0_USE_D; // minterm $00 clears
        b.bltdpt = 0x20;
        b.start_scheduled((1u16 << 6) | 2, &ram);
        assert_eq!(
            needs_bus_walk(&mut b, &mut ram),
            [false, false, false, true, false, true, false, true]
        );

        // A->D copy, 1 row x 2 words: the first D phase is the delayed-D
        // pipeline bubble, then the steady state writes the previous word
        // (HRM: "A0 -, A1 D0").
        let mut ram = vec![0u8; 256];
        let mut b = Blitter::new();
        b.bltcon0 = BLTCON0_USE_A | BLTCON0_USE_D | 0x00F0; // D := A
        b.bltafwm = 0xFFFF;
        b.bltalwm = 0xFFFF;
        b.bltapt = 0x10;
        b.bltdpt = 0x20;
        b.start_scheduled((1u16 << 6) | 2, &ram);
        assert_eq!(
            needs_bus_walk(&mut b, &mut ram),
            [false, false, true, false, true, true, false, true]
        );

        // ABCD cookie-cut, 1 row x 2 words: A/B/C fetch first, then the
        // empty D bubble; the next D/F phases store the queued words.
        let mut ram = vec![0u8; 256];
        let mut b = Blitter::new();
        b.bltcon0 = BLTCON0_USE_A | BLTCON0_USE_B | BLTCON0_USE_C | BLTCON0_USE_D | 0x00E4;
        b.bltafwm = 0xFFFF;
        b.bltalwm = 0xFFFF;
        b.bltapt = 0x10;
        b.bltbpt = 0x20;
        b.bltcpt = 0x30;
        b.bltdpt = 0x40;
        b.start_scheduled((1u16 << 6) | 2, &ram);
        assert_eq!(
            needs_bus_walk(&mut b, &mut ram),
            [false, false, true, true, true, false, true, true, true, true, false, true]
        );

        // Line blit, 2 pixels: StartDelay, Init, then [L1 L2 L3 L4] per pixel.
        // Only L2 (C read) and L4 (D write) access the bus; L1/L3 are internal
        // Bresenham cycles.
        let mut ram = vec![0u8; 256];
        let mut b = Blitter::new();
        b.bltcon0 = BLTCON0_USE_A | BLTCON0_USE_C | BLTCON0_USE_D | 0x004A;
        b.bltcon1 = BLTCON1_LINE;
        b.bltafwm = 0xFFFF;
        b.bltalwm = 0xFFFF;
        b.bltadat = 0x8000;
        b.bltcpt = 0x20;
        b.bltdpt = 0x20;
        b.bltcmod = 4;
        b.bltdmod = 4;
        b.start_scheduled((2u16 << 6) | 2, &ram);
        assert_eq!(
            needs_bus_walk(&mut b, &mut ram),
            [false, false, false, true, false, true, false, true, false, true]
        );
    }

    #[test]
    fn scheduled_normal_mode_latches_b_source_before_d_write_slot() {
        let mut ram = vec![0u8; 256];
        write_word(&mut ram, 0x10, 0x1234);
        write_word(&mut ram, 0x20, 0x0000);

        let mut b = Blitter::new();
        b.bltcon0 = BLTCON0_USE_B | BLTCON0_USE_C | BLTCON0_USE_D | 0x00CC; // D := B
        b.bltcon1 = 0;
        b.bltbpt = 0x10;
        b.bltcpt = 0x20;
        b.bltdpt = 0x20;
        b.start_scheduled((1u16 << 6) | 1, &ram);

        assert!(!b.tick_scheduled_slot(&mut ram)); // BBUSY start delay.
        assert!(!b.tick_scheduled_slot(&mut ram)); // INIT.
        assert!(!b.tick_scheduled_slot(&mut ram)); // A slot is idle when A DMA is disabled.
        assert!(!b.tick_scheduled_slot(&mut ram)); // B source is fetched here.
        write_word(&mut ram, 0x10, 0xABCD);
        assert!(!b.tick_scheduled_slot(&mut ram)); // C source.
        assert!(!b.tick_scheduled_slot(&mut ram)); // D queues the result.
        assert!(!b.tick_scheduled_slot(&mut ram)); // E pipeline flush.
        assert!(b.tick_scheduled_slot(&mut ram)); // F writes the queued D word.

        assert_eq!(read_word(&ram, 0x20), 0x1234);
    }

    #[test]
    fn scheduled_normal_mode_bbusy_start_delay_precedes_first_source_slot() {
        let mut ram = vec![0u8; 256];
        write_word(&mut ram, 0x10, 0xCAFE);

        let mut b = Blitter::new();
        b.bltcon0 = BLTCON0_USE_A | BLTCON0_USE_D | 0x00F0;
        b.bltafwm = 0xFFFF;
        b.bltalwm = 0xFFFF;
        b.bltapt = 0x10;
        b.bltdpt = 0x20;
        b.start_scheduled((1u16 << 6) | 1, &ram);

        assert_eq!(b.scheduled_slots_remaining(), Some(6));
        assert!(!b.tick_scheduled_slot(&mut ram));

        assert_eq!(b.bltapt, 0x10);
        assert_eq!(read_word(&ram, 0x20), 0);
        assert_eq!(b.scheduled_slots_remaining(), Some(5));
    }

    #[test]
    fn scheduled_normal_c_without_d_completes_after_c_state_without_d_flush() {
        let mut ram = vec![0u8; 256];
        write_word(&mut ram, 0x10, 0x8000);
        write_word(&mut ram, 0x20, 0x5555);

        let mut b = Blitter::new();
        b.bltcon0 = BLTCON0_USE_C | 0x00AA; // Minterm C, but D DMA disabled.
        b.bltcpt = 0x10;
        b.bltdpt = 0x20;
        b.start_scheduled((1u16 << 6) | 1, &ram);

        assert_eq!(b.scheduled_slots_remaining(), Some(4));
        assert!(!b.tick_scheduled_slot(&mut ram)); // BBUSY start delay.
        assert!(!b.tick_scheduled_slot(&mut ram)); // INIT.
        assert!(!b.tick_scheduled_slot(&mut ram)); // A state, empty when A is disabled.
        assert!(b.tick_scheduled_slot(&mut ram)); // C state is the next-word state.

        assert!(!b.busy);
        assert!(!b.bzero);
        assert_eq!(read_word(&ram, 0x20), 0x5555);
    }

    #[test]
    fn scheduled_normal_snapshots_source_at_start_against_later_overwrite() {
        // A scheduled blit must consume the A/B source as it was at BLTSIZE,
        // even if the CPU overwrites that buffer before the blit ticks. This
        // mirrors real hardware (the blitter owns the bus and reads its source
        // before the CPU can touch it) and is what makes back-to-back blits
        // through a shared scratch buffer correct.
        let mut ram = vec![0u8; 256];
        write_word(&mut ram, 0x10, 0xABCD); // B source word, two rows.
        write_word(&mut ram, 0x12, 0x1234);

        let mut b = Blitter::new();
        b.bltcon0 = BLTCON0_USE_B | BLTCON0_USE_D | 0x00CC; // D := B.
        b.bltcon1 = 0;
        b.bltbpt = 0x10;
        b.bltdpt = 0x20;
        b.start_scheduled((2u16 << 6) | 1, &ram); // h=2, w=1.

        // CPU clobbers the source buffer after BLTSIZE but before the blit
        // ticks; the snapshot must shield the blit from this.
        write_word(&mut ram, 0x10, 0x0000);
        write_word(&mut ram, 0x12, 0x0000);

        while !b.tick_scheduled_slot(&mut ram) {}

        assert_eq!(read_word(&ram, 0x20), 0xABCD);
        assert_eq!(read_word(&ram, 0x22), 0x1234);
    }

    #[test]
    fn scheduled_line_mode_latches_c_source_before_store_phase() {
        let mut ram = vec![0u8; 256];

        let mut b = Blitter::new();
        b.bltcon0 = BLTCON0_USE_C | 0x00AA; // Minterm C.
        b.bltcon1 = BLTCON1_LINE;
        b.bltcpt = 0;
        b.bltdpt = 0;
        b.start_scheduled((1u16 << 6) | 2, &ram);

        assert_eq!(b.scheduled_slots_remaining(), Some(6));
        assert!(!b.tick_scheduled_slot(&mut ram)); // BBUSY start delay.
        assert!(!b.tick_scheduled_slot(&mut ram)); // INIT.
        assert!(!b.tick_scheduled_slot(&mut ram)); // L1 accumulator state.
        assert!(!b.tick_scheduled_slot(&mut ram)); // L2 fetches C.
        write_word(&mut ram, 0, 0xFFFF);
        assert!(!b.tick_scheduled_slot(&mut ram)); // L3 propagation state.
        assert!(b.tick_scheduled_slot(&mut ram)); // L4 stores the latched C result.

        assert_eq!(read_word(&ram, 0), 0);
    }

    #[test]
    fn bltbdat_first_write_after_done_zeros_b_old_shift_register() {
        let mut ram = vec![0u8; 256];

        let mut b = Blitter::new();
        b.bltcon0 = BLTCON0_USE_D | 0x00CC; // Minterm B from BLTBDAT.
        b.bltcon1 = 4 << 12;
        b.bltdpt = 0x20;
        b.write_bltbdat(0x000F);
        b.write_bltbdat(0x0000);
        b.execute((1u16 << 6) | 1, &mut ram);
        assert_eq!(read_word(&ram, 0x20), 0xF000);

        write_word(&mut ram, 0x20, 0xFFFF);
        b.bltdpt = 0x20;
        b.write_bltbdat(0x0000);
        b.execute((1u16 << 6) | 1, &mut ram);

        assert_eq!(read_word(&ram, 0x20), 0x0000);
    }

    /// Inclusive fill on a pair of bits should set everything between
    /// (and including) them. Input pattern 0b00100010 -> 0b00111110.
    #[test]
    fn area_fill_inclusive() {
        // d=0x0022 has bits 1 and 5 set, IFE should produce 0x003E
        // (bits 1..5 inclusive).
        let mut state: u16 = 0;
        let out = apply_fill(0x0022, &mut state, true, false);
        assert_eq!(out, 0x003E);
        // After processing, bits 1 and 5 toggled the state twice, so
        // state ends at 0 again.
        assert_eq!(state, 0);
    }

    /// Exclusive fill same input: 0b00100010 -> 0b00011110.
    /// The right edge remains intact, the span fills, and the left edge
    /// is deleted, matching the hardware manual's edge convention.
    #[test]
    fn area_fill_exclusive() {
        let mut state: u16 = 0;
        let out = apply_fill(0x0022, &mut state, false, true);
        assert_eq!(out, 0x001E);
    }

    #[test]
    fn area_fill_matches_hardware_manual_example() {
        let mut state = 0;
        assert_eq!(apply_fill(0x2418, &mut state, true, false), 0x3C18);
        assert_eq!(state, 0);

        let mut state = 0;
        assert_eq!(apply_fill(0x2418, &mut state, false, true), 0x1C08);
        assert_eq!(state, 0);
    }

    #[test]
    fn area_fill_requires_descending_mode_for_blit_output() {
        let mut ram = vec![0; 64];
        write_word(&mut ram, 0x10, 0x0022);

        let mut ascending = Blitter::new();
        ascending.bltcon0 = BLTCON0_USE_A | BLTCON0_USE_D | 0x00F0;
        ascending.bltcon1 = BLTCON1_IFE;
        ascending.bltafwm = 0xFFFF;
        ascending.bltalwm = 0xFFFF;
        ascending.bltapt = 0x10;
        ascending.bltdpt = 0x20;
        ascending.execute((1u16 << 6) | 1, &mut ram);

        assert_eq!(read_word(&ram, 0x20), 0x0022);

        write_word(&mut ram, 0x20, 0);
        let mut descending = Blitter::new();
        descending.bltcon0 = BLTCON0_USE_A | BLTCON0_USE_D | 0x00F0;
        descending.bltcon1 = BLTCON1_DESC | BLTCON1_IFE;
        descending.bltafwm = 0xFFFF;
        descending.bltalwm = 0xFFFF;
        descending.bltapt = 0x10;
        descending.bltdpt = 0x20;
        descending.execute((1u16 << 6) | 1, &mut ram);

        assert_eq!(read_word(&ram, 0x20), 0x003E);
    }

    #[test]
    fn scheduled_area_fill_requires_descending_mode() {
        let mut ram = vec![0; 64];
        write_word(&mut ram, 0x10, 0x0022);

        let mut b = Blitter::new();
        b.bltcon0 = BLTCON0_USE_A | BLTCON0_USE_D | 0x00F0;
        b.bltcon1 = BLTCON1_IFE;
        b.bltafwm = 0xFFFF;
        b.bltalwm = 0xFFFF;
        b.bltapt = 0x10;
        b.bltdpt = 0x20;
        b.start_scheduled((1u16 << 6) | 1, &ram);
        while b.busy {
            let _ = b.tick_scheduled_slot(&mut ram);
        }

        assert_eq!(read_word(&ram, 0x20), 0x0022);
        assert_eq!(b.scheduled_slots_remaining(), None);
    }

    #[test]
    fn descending_area_fill_costs_one_extra_idle_cycle_per_word() {
        // An A->D area fill (USEA|USED, USEC clear) costs ONE more cycle/word
        // than an A->D copy: the fill consumes the C slot, but as an IDLE cycle
        // (no bus access), the "-" in the HRM "A - D" fill cadence. Validated
        // cross-emulator -- FS-UAE and vAmiga both time an A->D fill at
        // 3 cck/word vs 2 for a copy (timing-test rows 23/24/26). (A previous
        // change collapsed fill to the copy cost to speed one frame-budget
        // regression; that masked a separate timing bug. See docs/internals/timing.md.)
        let mut ram = vec![0u8; 256];
        write_word(&mut ram, 0x10, 0x0022);
        write_word(&mut ram, 0x12, 0x0044);

        let walk_bus = |b: &mut Blitter, ram: &mut Vec<u8>| -> (usize, usize) {
            let total = b.scheduled_slots_remaining().unwrap() as usize;
            let mut bus = 0usize;
            while b.busy {
                if b.current_slot_needs_bus() {
                    bus += 1;
                }
                let _ = b.tick_scheduled_slot(ram);
            }
            (total, bus)
        };

        let mut copy = Blitter::new();
        copy.bltcon0 = BLTCON0_USE_A | BLTCON0_USE_D | 0x00F0;
        copy.bltcon1 = BLTCON1_DESC;
        copy.bltafwm = 0xFFFF;
        copy.bltalwm = 0xFFFF;
        copy.bltapt = 0x12;
        copy.bltdpt = 0x22;
        copy.start_scheduled((1u16 << 6) | 2, &ram);
        let (copy_total, copy_bus) = walk_bus(&mut copy, &mut ram);

        let mut fill = Blitter::new();
        fill.bltcon0 = BLTCON0_USE_A | BLTCON0_USE_D | 0x00F0;
        fill.bltcon1 = BLTCON1_DESC | BLTCON1_EFE;
        fill.bltafwm = 0xFFFF;
        fill.bltalwm = 0xFFFF;
        fill.bltapt = 0x12;
        fill.bltdpt = 0x22;
        fill.start_scheduled((1u16 << 6) | 2, &ram);
        let fill_total = fill.scheduled_slots_remaining();
        let (fill_total_walked, fill_bus) = walk_bus(&mut fill, &mut ram);

        // Copy: 2 (start delay + init) + 2 words * 2 cyc/word + 2 (D flush) = 8.
        assert_eq!(copy_total, 8);
        // Fill: the same, but 3 cyc/word -> 2 + 2*3 + 2 = 10 (two extra slots).
        assert_eq!(fill_total, Some(10));
        assert_eq!(fill_total_walked, 10);
        // The extra fill slots are IDLE: the fill performs the same number of
        // bus accesses as the copy (the A reads and D writes), just spread over
        // two more idle cycles.
        assert_eq!(fill_bus, copy_bus);

        // And the fill still produces filled output (carry datapath intact).
        assert_ne!(read_word(&ram, 0x22), 0);
    }

    #[test]
    fn ecs_bltsizv_bltsizh_decode_full_big_blit_ranges() {
        assert_eq!(decode_ecs_bltsize(0x0001, 0x0001), (1, 1));
        assert_eq!(decode_ecs_bltsize(0x7FFF, 0x07FF), (32_767, 2_047));
        assert_eq!(decode_ecs_bltsize(0x0000, 0x0000), (32_768, 2_048));
        assert_eq!(decode_ecs_bltsize(0xFFFF, 0xFFFF), (32_767, 2_047));
    }

    /// Line mode: draw a 16-pixel diagonal from (0,0) to (15,15) into a
    /// 32-byte-wide bitplane. Octant 0 has SUD=0 SUL=0 AUL=0, i.e.
    /// major=Y+, minor=X+. For a 45-degree dx=dy=15 line the accumulator
    /// stays >= 0 so the minor step is taken every iteration, producing
    /// the (y, y) diagonal.
    #[test]
    fn line_mode_diagonal_octant0() {
        let mut ram = vec![0u8; 1024];
        let mut b = Blitter::new();
        // Bitplane: 32 bytes/row, 16 rows. Pixel (x, y) lives at byte
        // (y * 32 + x/8), bit (7 - x%8).
        b.bltcon0 = 0x0BCA; // ASH=0, USEA|USEC|USED, minterm $CA
        b.bltcon1 = BLTCON1_LINE; // LINEMODE, octant 0 (SUD=0 SUL=0 AUL=0)
        b.bltafwm = 0xFFFF;
        b.bltalwm = 0xFFFF;
        b.bltadat = 0x8000;
        b.bltbdat = 0xFFFF;
        b.bltcpt = 0;
        b.bltdpt = 0;
        b.bltcmod = 32;
        // For dx=dy=15: deltaX-deltaY = 0, so DiagROM-style setup gives
        // bltapt = 0, bltamod = 0, bltbmod = 30. acc starts at 0 (>=0)
        // so minor steps every iter, and amod=0 keeps acc at 0.
        b.bltamod = 0;
        b.bltbmod = 30;
        b.bltapt = 0;
        let bltsize = (16u16 << 6) | 2;
        b.execute(bltsize, &mut ram);
        for y in 0..=15 {
            let byte_off = y * 32 + y / 8;
            let bit = 7 - (y % 8);
            assert!(
                ram[byte_off] & (1 << bit) != 0,
                "diagonal pixel ({y}, {y}) byte={byte_off:#X} bit={bit} ram={:#X}",
                ram[byte_off]
            );
        }
        assert_eq!(ram[5 * 32] & 0x80, 0);
    }

    /// Pure vertical line in octant 0: dx=0, dy=15. The accumulator
    /// stays negative throughout so the minor (X) axis never steps;
    /// the result is a single-column line at x=0.
    #[test]
    fn line_mode_vertical_octant0() {
        let mut ram = vec![0u8; 1024];
        let mut b = Blitter::new();
        b.bltcon0 = 0x0BCA;
        b.bltcon1 = BLTCON1_LINE | BLTCON1_SIGN;
        b.bltafwm = 0xFFFF;
        b.bltalwm = 0xFFFF;
        b.bltadat = 0x8000;
        b.bltbdat = 0xFFFF;
        b.bltcpt = 0;
        b.bltdpt = 0;
        b.bltcmod = 32;
        // dx=0, dy=15: deltaX-deltaY = -15, 2*deltaX = 0.
        b.bltamod = -15;
        b.bltbmod = 0;
        b.bltapt = (-15i16) as u16 as u32;
        let bltsize = (15u16 << 6) | 2;
        b.execute(bltsize, &mut ram);
        // Every row 0..=14 should have bit 7 of byte 0 set (column 0).
        for y in 0..15 {
            assert!(
                ram[y * 32] & 0x80 != 0,
                "vertical pixel (0, {y}) ram[{:#X}]={:#X}",
                y * 32,
                ram[y * 32]
            );
        }
        // No pixel set at column 1 anywhere.
        for y in 0..16 {
            assert_eq!(
                ram[y * 32] & 0x40,
                0,
                "unexpected pixel at (1, {y}) ram[{:#X}]={:#X}",
                y * 32,
                ram[y * 32]
            );
        }
    }

    #[test]
    fn line_mode_accumulator_wraps_as_signed_16_bit() {
        let mut ram = vec![0u8; 1024];
        let mut b = Blitter::new();
        b.bltcon0 = 0x0BCA;
        b.bltcon1 = BLTCON1_LINE;
        b.bltafwm = 0xFFFF;
        b.bltalwm = 0xFFFF;
        b.bltadat = 0x8000;
        b.bltbdat = 0xFFFF;
        b.bltcpt = 0;
        b.bltdpt = 0;
        b.bltcmod = 32;
        b.bltamod = 20;
        b.bltbmod = 0;
        b.bltapt = 0x7FF8;

        b.execute((3u16 << 6) | 2, &mut ram);

        assert_ne!(ram[2 * 32] & 0x40, 0, "expected pixel at (1, 2)");
        assert_eq!(ram[2 * 32] & 0x20, 0, "unexpected pixel at (2, 2)");
    }

    #[test]
    fn line_mode_initial_sign_comes_from_bltcon1() {
        let mut b = Blitter::new();
        b.bltcon1 = BLTCON1_LINE | BLTCON1_SIGN;
        b.bltapt = 0x0000;
        assert!(LineBlitState::new(&b, 1).sign);

        b.bltcon1 = BLTCON1_LINE;
        b.bltapt = 0x8000;
        assert!(!LineBlitState::new(&b, 1).sign);
    }

    #[test]
    fn line_mode_bltbdat_load_uses_bsh_at_write_time() {
        let mut ram = vec![0u8; 64];
        let mut b = Blitter::new();
        b.bltcon0 = BLTCON0_USE_C | 0x00CC; // Minterm B.
        b.write_bltcon1(BLTCON1_LINE);
        b.write_bltbdat(0x0001);
        b.write_bltcon1(BLTCON1_LINE | (1 << 12));
        b.bltcpt = 0;
        b.bltdpt = 0;

        b.execute((1u16 << 6) | 2, &mut ram);

        assert_eq!(read_word(&ram, 0), 0xFFFF);
    }

    #[test]
    fn normal_mode_bltbdat_write_does_not_load_line_texture_shifter() {
        let mut ram = vec![0u8; 64];
        let mut b = Blitter::new();
        b.bltcon0 = BLTCON0_USE_C | 0x00CC; // Minterm B.
        b.write_bltcon1(0);
        b.write_bltbdat(0x0001);
        b.write_bltcon1(BLTCON1_LINE | (1 << 12));
        b.bltcpt = 0;
        b.bltdpt = 0;

        b.execute((1u16 << 6) | 2, &mut ram);

        assert_eq!(read_word(&ram, 0), 0x0000);
    }

    #[test]
    fn line_mode_writes_back_shift_sign_and_accumulator_registers() {
        let mut ram = vec![0u8; 128];
        let mut b = Blitter::new();
        b.bltcon0 = (14 << 12) | BLTCON0_USE_C | 0x00AA; // Minterm C.
        b.bltcon1 = (2 << 12) | BLTCON1_LINE | 0x0010;
        b.bltcpt = 0;
        b.bltdpt = 0;
        b.bltcmod = 32;
        b.bltamod = 1;
        b.bltbmod = 0;
        b.bltapt = 0;

        b.execute((2u16 << 6) | 2, &mut ram);

        assert_eq!((b.bltcon0 >> 12) & 0x000F, 0);
        assert_eq!((b.bltcon1 >> 12) & 0x000F, 0);
        assert_eq!(b.bltapt & 0x0000_FFFF, 2);
        assert_eq!(b.bltcon1 & BLTCON1_SIGN, 0);
    }

    #[test]
    fn line_mode_sing_limits_horizontal_line_to_one_dot() {
        let mut ram = vec![0u8; 1024];
        let mut b = Blitter::new();
        b.bltcon0 = 0x0BCA;
        b.bltcon1 = BLTCON1_LINE | BLTCON1_SIGN | BLTCON1_SING | 0x0010;
        b.bltafwm = 0xFFFF;
        b.bltalwm = 0xFFFF;
        b.bltadat = 0x8000;
        b.bltbdat = 0xFFFF;
        b.bltcpt = 0;
        b.bltdpt = 0;
        b.bltcmod = 32;
        b.bltdmod = 32;
        b.bltamod = -60;
        b.bltbmod = 0;
        b.bltapt = (-30i16) as u16 as u32;
        let bltsize = (16u16 << 6) | 2;

        b.execute(bltsize, &mut ram);

        let set_bits: u32 = ram[..32].iter().map(|byte| byte.count_ones()).sum();
        assert_eq!(set_bits, 1);
        assert_ne!(ram[0] & 0x80, 0);
    }
}
