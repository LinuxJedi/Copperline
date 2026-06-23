// SPDX-License-Identifier: GPL-3.0-or-later

//! OCS Copper instruction decode and persistent execution state.
//!
//! The Copper runs from Agnus beam time and consumes grants from the
//! pragmatic chip-bus arbiter. The arbiter is slot-ordered, but it is
//! still not an exact 68000-cycle model.

#[cfg(test)]
use super::agnus::{COLORCLOCKS_PER_LINE, PAL_LINES};

pub const DMACON_COPEN: u16 = 1 << 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopperInstruction {
    Move { register: u16, value: u16 },
    Wait(CopperWait),
    Skip(CopperWait),
}

impl CopperInstruction {
    pub fn decode(first: u16, second: u16) -> Self {
        if first & 1 == 0 {
            return Self::Move {
                register: first & 0x01FE,
                value: second,
            };
        }

        let wait = CopperWait::new(first, second);
        if second & 1 == 0 {
            Self::Wait(wait)
        } else {
            Self::Skip(wait)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopperFetch {
    Idle,
    FirstWord {
        pc: u32,
        word: u16,
    },
    Instruction {
        pc: u32,
        first: u16,
        second: u16,
        instruction: CopperInstruction,
    },
    SkippedMove {
        pc: u32,
        first: u16,
        second: u16,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CopperWait {
    first: u16,
    second: u16,
}

impl CopperWait {
    pub fn new(first: u16, second: u16) -> Self {
        Self { first, second }
    }

    pub fn is_end_of_list(self) -> bool {
        if self.second & 1 != 0 {
            return false;
        }
        if self.first == 0xFFFF && self.second == 0xFFFE {
            return true;
        }

        let mask = self.compare_mask();
        let target = self.position_bits() & mask;
        let vertical_mask = mask & 0xFF00;
        if vertical_mask != 0xFF00 || target & vertical_mask != vertical_mask {
            return false;
        }

        let horizontal_mask = mask & 0x00FE;
        let target_h = target & horizontal_mask;
        let last_reachable_h = 0x00E2 & horizontal_mask;
        target_h > last_reachable_h
    }

    pub fn blitter_wait_enabled(self) -> bool {
        self.second & 0x8000 == 0
    }

    pub fn position_bits(self) -> u16 {
        self.first & 0xFFFE
    }

    pub fn compare_mask(self) -> u16 {
        // IR2 bit 15 is BFD, not a vertical mask bit. VP7 is always
        // included in the position comparison; bits 14..1 are VE/HE.
        (self.second & 0x7FFE) | 0x8000
    }

    pub fn is_satisfied(self, vpos: u32, hpos: u32) -> bool {
        if self.is_end_of_list() {
            return false;
        }
        let mask = self.compare_mask();
        let target = self.position_bits() & mask;
        if mask == 0xFFFE {
            let target_v = ((target >> 8) & 0x00FF) as u32;
            let target_h = (target & 0x00FE) as u32;
            let vlow = vpos & 0x00FF;
            // Near the end of line 255, low-half full-mask waits can
            // target the short post-rollover tail of the field by
            // waiting for V[7:0] to wrap back through zero. High-half
            // targets have already been reached and remain satisfied.
            if vpos == 0x00FF && target_v < 0x80 {
                return false;
            }
            return vlow > target_v || (vlow == target_v && hpos >= target_h);
        }

        let beam = (((vpos as u16) & 0x00FF) << 8) | ((hpos as u16) & 0x00FE);
        let beam = beam & mask;
        if beam == target {
            true
        } else if mask == 0x80FE && target & 0x8000 != 0 && vpos >= 0x100 {
            // VP7-only waits can be fetched just after the 8-bit
            // vertical phase rolls from 255 to 256; the high phase has
            // already passed even though VP7 is low again.
            true
        } else {
            // A copper wait fires once the beam reaches OR passes the
            // masked compare position, not only on an exact match. VP7
            // occupies bit 15, so comparing the masked beam against the
            // masked target as a combined value preserves raster order
            // (vertical phase first, then horizontal). Without the
            // greater-than case, a horizontal-only wait (vertical bits
            // masked out) that the chip-bus scheduler does not land on at
            // the exact target colorclock would never be satisfied on that
            // line and would stall until VP7 flipped at vpos 128.
            beam > target
        }
    }

    #[cfg(test)]
    pub fn cck_until_satisfied(self, start_vpos: u32, start_hpos: u32) -> Option<u32> {
        if self.is_end_of_list() {
            return None;
        }
        if self.is_satisfied(start_vpos, start_hpos) {
            return Some(0);
        }
        if self.compare_mask() == 0xFFFE {
            return self.cck_until_full_mask_satisfied(start_vpos, start_hpos);
        }

        let mut vpos = start_vpos;
        let mut hpos = start_hpos;
        let frame_cck = PAL_LINES * COLORCLOCKS_PER_LINE;
        for delta in 1..=frame_cck {
            hpos += 1;
            if hpos >= COLORCLOCKS_PER_LINE {
                hpos = 0;
                vpos += 1;
                if vpos >= PAL_LINES {
                    vpos = 0;
                }
            }
            if self.is_satisfied(vpos, hpos) {
                return Some(delta);
            }
        }
        None
    }

    #[cfg(test)]
    fn cck_until_full_mask_satisfied(self, start_vpos: u32, start_hpos: u32) -> Option<u32> {
        let target_h = (self.position_bits() & 0x00FE) as u32;

        for line_delta in 0..=PAL_LINES {
            let vpos = (start_vpos + line_delta) % PAL_LINES;
            let line_start_delta = if line_delta == 0 {
                0
            } else {
                COLORCLOCKS_PER_LINE
                    .saturating_sub(start_hpos)
                    .saturating_add((line_delta - 1).saturating_mul(COLORCLOCKS_PER_LINE))
            };

            if line_delta == 0 {
                if target_h < COLORCLOCKS_PER_LINE
                    && start_hpos <= target_h
                    && self.is_satisfied(vpos, target_h)
                {
                    return Some(target_h - start_hpos);
                }
            } else if self.is_satisfied(vpos, 0) {
                return Some(line_start_delta);
            } else if target_h < COLORCLOCKS_PER_LINE && self.is_satisfied(vpos, target_h) {
                return Some(line_start_delta + target_h);
            }
        }

        None
    }
}

/// The Copper's WAIT comparator cannot release a wait during the last color
/// clocks of a scanline. On hardware the comparison acts on the position of
/// the next color clock (which wraps to 0 at the line end) and a WAIT spends
/// dead cycles after its second instruction-word fetch before the comparator
/// output can take effect, so a wait whose horizontal condition is already
/// true near the line end does not release there; it releases on a following
/// line when its comparison is true at that line's positions. This matches
/// WinUAE/FS-UAE and the documented "WAIT for HP >= $E0 never matches on
/// that line" quirk.
///
/// Evidence: a dense 42-MOVE copper blast separated by `WAIT $8033,$80FE`
/// (VP7 set, h >= $32) can reach the wait at hpos ~220. On hardware each
/// blast therefore starts at h=$32 of the next line, landing interleaved
/// COLOR00 and BPLCON1 writes at the same hpos on every line. Releasing the
/// wait at the line end instead starts the next blast ~54 color clocks early,
/// so same-line scroll and color writes land at the wrong positions.
const WAIT_RELEASE_LINE_END_BLACKOUT_CCK: u32 = 4;

/// Whether a WAIT comparator that is satisfied at `(vpos, hpos)` must still be
/// held off because the beam is in the last few color clocks of the line.
///
/// The blackout models a wait whose horizontal condition became true *earlier*
/// on the line: the copper reaches such an already-true WAIT near the line end,
/// and on hardware the comparison acts on the next color clock's position
/// (which wraps to 0), so the wait releases at its target position on a
/// following line rather than at the line end. It must NOT, however, hold off a
/// WAIT whose compare position lies *inside* the blackout itself (for example
/// `WAIT $80E1,$80FE`, h >= $E0 on a 227-cck line):
/// that position is the only color clock where the wait is ever true, so
/// blacking it out would stall the copper forever. Such a wait -- not yet
/// satisfied at the color clock just before the blackout begins -- releases at
/// its target.
fn wait_release_blocked_at_line_end(wait: CopperWait, vpos: u32, hpos: u32, line_cck: u32) -> bool {
    if hpos.saturating_add(WAIT_RELEASE_LINE_END_BLACKOUT_CCK) < line_cck {
        return false;
    }
    // In the blackout. Block only if the comparator was already satisfied at
    // the color clock immediately before the blackout (an already-true wait
    // reached late); a wait that first becomes true inside the blackout
    // releases there.
    let pre_blackout_hpos = line_cck.saturating_sub(WAIT_RELEASE_LINE_END_BLACKOUT_CCK + 1);
    wait.is_satisfied(vpos, pre_blackout_hpos)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum CopperState {
    Running,
    Waiting {
        wait: CopperWait,
        phase: CopperWaitPhase,
    },
    /// A SKIP that has been fetched and is spending its two non-bus tail cycles
    /// before the comparator output takes effect. On real Agnus a SKIP runs the
    /// identical 4-cycle FETCH1/FETCH2/WAITSKIP1/WAITSKIP2 sequence as a WAIT
    /// (Minimig Copper.v): a dummy cycle then the compare cycle, both bus-free.
    /// The skip condition is evaluated on the final (compare) cycle.
    Skipping {
        skip: CopperWait,
        phase: CopperSkipPhase,
    },
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum CopperWaitPhase {
    InstructionTail,
    Waiting,
    Wakeup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum CopperSkipPhase {
    /// WAITSKIP1: the dummy cycle that requests no DMA.
    Dummy,
    /// WAITSKIP2: the comparator cycle that decides whether the next MOVE is
    /// skipped, after which the Copper fetches the next instruction.
    Compare,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
struct CopperFirstWord {
    pc: u32,
    word: u16,
}

/// What the Copper did with one Copper-eligible chip-bus color clock, as
/// reported by [`Copper::step_eligible_slot`]. The bus owner and any register
/// write are applied by the caller (the bus owns custom-register state and the
/// COPCON gate), so this is the single primitive shared by the live execution
/// path and the blitter-deadline predictor's cloned simulation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopperSlotAction {
    /// The Copper did not use the bus this color clock (its idle half-cycle, an
    /// asleep WAIT comparator check, or stopped). The slot is free for the
    /// blitter/CPU.
    Idle,
    /// The Copper used the bus this color clock but produced no register write
    /// the caller must apply (a first-word fetch, a skipped MOVE, an applied
    /// COPJMP/WAIT/SKIP).
    BusUsed,
    /// The Copper used the bus and decoded a `MOVE`; the caller applies the
    /// custom-register write (subject to COPCON) or stops the Copper.
    Move { register: u16, value: u16 },
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Copper {
    pc: u32,
    state: CopperState,
    pending_first: Option<CopperFirstWord>,
    skip_next_move: bool,
}

impl Default for Copper {
    fn default() -> Self {
        Self::new()
    }
}

impl Copper {
    pub fn new() -> Self {
        Self {
            pc: 0,
            state: CopperState::Stopped,
            pending_first: None,
            skip_next_move: false,
        }
    }

    pub fn frame_start(&mut self, cop1lc: u32) {
        self.jump(cop1lc);
    }

    pub fn jump(&mut self, address: u32) {
        self.pc = address & !1;
        self.pending_first = None;
        self.skip_next_move = false;
        self.state = CopperState::Running;
    }

    pub fn stop(&mut self) {
        self.pending_first = None;
        self.skip_next_move = false;
        self.state = CopperState::Stopped;
    }

    #[cfg(test)]
    pub fn wait(&mut self, wait: CopperWait) {
        self.pending_first = None;
        self.skip_next_move = false;
        self.state = CopperState::Waiting {
            wait,
            phase: CopperWaitPhase::Waiting,
        };
    }

    pub fn start_wait_instruction(&mut self, wait: CopperWait) {
        self.pending_first = None;
        self.skip_next_move = false;
        self.state = CopperState::Waiting {
            wait,
            phase: CopperWaitPhase::InstructionTail,
        };
    }

    /// Begin a SKIP's two bus-free tail cycles (WAITSKIP1/WAITSKIP2). The skip
    /// decision is deferred to the compare cycle so a SKIP occupies the same
    /// four Copper cycles as a WAIT, matching real Agnus timing.
    pub fn start_skip_instruction(&mut self, skip: CopperWait) {
        self.pending_first = None;
        self.skip_next_move = false;
        self.state = CopperState::Skipping {
            skip,
            phase: CopperSkipPhase::Dummy,
        };
    }

    pub fn waiting(&self) -> Option<CopperWait> {
        match self.state {
            CopperState::Waiting { wait, .. } => Some(wait),
            _ => None,
        }
    }

    pub fn advance_wait_free_cycle(
        &mut self,
        vpos: u32,
        hpos: u32,
        blitter_busy: bool,
        line_cck: u32,
    ) {
        let CopperState::Waiting { wait, phase } = self.state else {
            return;
        };

        match phase {
            CopperWaitPhase::Wakeup => {
                if Self::hpos_is_access_cycle(hpos) {
                    self.state = CopperState::Running;
                }
            }
            CopperWaitPhase::InstructionTail => {
                // The comparator cannot release a wait in the line-end blackout
                // (see WAIT_RELEASE_LINE_END_BLACKOUT_CCK); the wait keeps
                // sleeping and is re-evaluated at the next line's positions.
                let released = !wait_release_blocked_at_line_end(wait, vpos, hpos, line_cck)
                    && wait_is_satisfied_with_blitter(wait, vpos, hpos, blitter_busy);
                if released {
                    self.state = CopperState::Waiting {
                        wait,
                        phase: CopperWaitPhase::Wakeup,
                    };
                } else {
                    self.state = CopperState::Waiting {
                        wait,
                        phase: CopperWaitPhase::Waiting,
                    };
                }
            }
            CopperWaitPhase::Waiting => {
                let released = !wait_release_blocked_at_line_end(wait, vpos, hpos, line_cck)
                    && wait_is_satisfied_with_blitter(wait, vpos, hpos, blitter_busy);
                if released {
                    self.state = CopperState::Waiting {
                        wait,
                        phase: CopperWaitPhase::Wakeup,
                    };
                }
            }
        }
    }

    /// Advance a SKIP through its two bus-free tail cycles. The dummy cycle
    /// elapses first; on the following compare cycle (a Copper access cycle) the
    /// skip condition is evaluated against the live beam position and, if true,
    /// the next MOVE is marked for discard. The Copper then resumes fetching.
    pub fn advance_skip_free_cycle(&mut self, vpos: u32, hpos: u32, blitter_busy: bool) {
        let CopperState::Skipping { skip, phase } = self.state else {
            return;
        };

        match phase {
            CopperSkipPhase::Dummy => {
                self.state = CopperState::Skipping {
                    skip,
                    phase: CopperSkipPhase::Compare,
                };
            }
            CopperSkipPhase::Compare => {
                if Self::hpos_is_access_cycle(hpos) {
                    if wait_is_satisfied_with_blitter(skip, vpos, hpos, blitter_busy) {
                        self.skip_next_move = true;
                    }
                    self.state = CopperState::Running;
                }
            }
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self.state, CopperState::Running)
    }

    /// The OCS Copper is a two-cycle processor: it can take a chip-bus memory
    /// cycle only on every other color clock, and that cadence is locked to the
    /// beam's horizontal parity rather than to a free-running internal phase.
    /// The Copper accesses on even in-line color clocks; the odd ones are its
    /// mandatory idle halves, left free for the blitter/CPU. Anchoring to the
    /// beam (not a carried-over flip-flop) is what makes a back-to-back colour
    /// MOVE list land its writes at the same hpos on every line, so a
    /// Copper-driven horizontal gradient produces vertically aligned bands
    /// instead of per-line shimmer.
    pub fn hpos_is_access_cycle(hpos: u32) -> bool {
        hpos & 1 == 0
    }

    pub fn pc(&self) -> u32 {
        self.pc
    }

    #[cfg(test)]
    fn skip_next_move(&mut self) {
        self.skip_next_move = true;
    }

    pub fn fetch_decode(&mut self, chip_ram: &[u8]) -> CopperFetch {
        if !self.is_running() {
            return CopperFetch::Idle;
        }
        let pc = (self.pc as usize) & !1;
        if pc + 2 > chip_ram.len() {
            self.stop();
            return CopperFetch::Idle;
        }

        let word_pc = self.pc & !1;
        let word = u16::from_be_bytes([chip_ram[pc], chip_ram[pc + 1]]);
        self.pc = self.pc.wrapping_add(2) & !1;

        if let Some(first) = self.pending_first.take() {
            let instruction = CopperInstruction::decode(first.word, word);
            if self.skip_next_move {
                self.skip_next_move = false;
                if matches!(instruction, CopperInstruction::Move { .. }) {
                    return CopperFetch::SkippedMove {
                        pc: first.pc,
                        first: first.word,
                        second: word,
                    };
                }
            }
            return CopperFetch::Instruction {
                pc: first.pc,
                first: first.word,
                second: word,
                instruction,
            };
        }

        self.pending_first = Some(CopperFirstWord { pc: word_pc, word });
        CopperFetch::FirstWord { pc: word_pc, word }
    }

    /// Advance the Copper by one Copper-eligible chip-bus color clock and report
    /// what it did with the slot. This is the single cadence primitive used by
    /// both the live bus path and the blitter-deadline predictor's cloned
    /// simulation, so they cannot drift apart.
    ///
    /// The OCS Copper accesses the bus on every *other* color clock, locked to
    /// the beam's horizontal parity (see [`Copper::hpos_is_access_cycle`]): it
    /// fetches on even in-line color clocks and idles on the odd ones, so a
    /// MOVE/SKIP spans 4 color clocks and a WAIT 6 (its load tail is the third
    /// memory cycle), with the alternate cycles left free for the blitter/CPU.
    /// Because the cadence follows the beam rather than a carried-over flip-flop,
    /// a back-to-back MOVE list writes at the same hpos on every line.
    ///
    /// `cop1lc`/`cop2lc` resolve COPJMP strobes; `allow_fetch` is false when a
    /// forced owner (a granted CPU access) already holds this color clock, so
    /// the Copper only consumes its idle half here, never a fetch.
    pub fn step_eligible_slot(
        &mut self,
        chip_ram: &[u8],
        vpos: u32,
        hpos: u32,
        blitter_busy: bool,
        cop1lc: u32,
        cop2lc: u32,
        allow_fetch: bool,
        line_cck: u32,
    ) -> CopperSlotAction {
        match self.state {
            CopperState::Stopped => CopperSlotAction::Idle,
            CopperState::Waiting { .. } => {
                // A waiting Copper releases the bus: it spends each eligible
                // color clock checking the comparator. After a match, real
                // Agnus spends a dummy wake-up Copper cycle with no DMA request
                // before fetching the next instruction, so the blitter/CPU keep
                // that cycle too.
                self.advance_wait_free_cycle(vpos, hpos, blitter_busy, line_cck);
                CopperSlotAction::Idle
            }
            CopperState::Skipping { .. } => {
                // A SKIP's two tail cycles request no DMA, exactly like a WAIT's
                // tail, so the blitter/CPU keep them. The skip decision lands on
                // the compare cycle.
                self.advance_skip_free_cycle(vpos, hpos, blitter_busy);
                CopperSlotAction::Idle
            }
            CopperState::Running => {
                // The Copper can only take a memory cycle on its access-parity
                // color clock; the odd ones are its mandatory idle halves. A
                // forced owner (a granted CPU access) likewise blocks the fetch.
                if !allow_fetch || !Self::hpos_is_access_cycle(hpos) {
                    return CopperSlotAction::Idle;
                }
                match self.fetch_decode(chip_ram) {
                    CopperFetch::Idle => CopperSlotAction::Idle,
                    CopperFetch::FirstWord { .. } | CopperFetch::SkippedMove { .. } => {
                        CopperSlotAction::BusUsed
                    }
                    CopperFetch::Instruction { instruction, .. } => {
                        match instruction {
                            CopperInstruction::Move {
                                register: 0x088, ..
                            } => {
                                self.jump(cop1lc);
                                CopperSlotAction::BusUsed
                            }
                            CopperInstruction::Move {
                                register: 0x08A, ..
                            } => {
                                self.jump(cop2lc);
                                CopperSlotAction::BusUsed
                            }
                            CopperInstruction::Move { register, value } => {
                                CopperSlotAction::Move { register, value }
                            }
                            CopperInstruction::Wait(wait) => {
                                if wait.is_end_of_list() {
                                    self.stop();
                                } else {
                                    self.start_wait_instruction(wait);
                                }
                                CopperSlotAction::BusUsed
                            }
                            CopperInstruction::Skip(skip) => {
                                // The SKIP comparator does not act immediately:
                                // the Copper spends two more bus-free cycles
                                // (WAITSKIP1/WAITSKIP2) before the decision takes
                                // effect, matching a WAIT's timing.
                                self.start_skip_instruction(skip);
                                CopperSlotAction::BusUsed
                            }
                        }
                    }
                }
            }
        }
    }
}

fn wait_is_satisfied_with_blitter(
    wait: CopperWait,
    vpos: u32,
    hpos: u32,
    blitter_busy: bool,
) -> bool {
    wait.is_satisfied(vpos, hpos) && (!wait.blitter_wait_enabled() || !blitter_busy)
}

#[cfg(test)]
mod tests {
    use super::{Copper, CopperFetch, CopperInstruction, CopperWait};
    use crate::chipset::agnus::COLORCLOCKS_PER_LINE;

    #[test]
    fn decodes_move_wait_skip_and_end_wait() {
        assert_eq!(
            CopperInstruction::decode(0x0180, 0x0ABC),
            CopperInstruction::Move {
                register: 0x0180,
                value: 0x0ABC
            }
        );

        let wait = CopperWait::new(0x5007, 0xFFFE);
        assert_eq!(
            CopperInstruction::decode(0x5007, 0xFFFE),
            CopperInstruction::Wait(wait)
        );
        assert_eq!(
            CopperInstruction::decode(0x5007, 0xFFFF),
            CopperInstruction::Skip(CopperWait::new(0x5007, 0xFFFF))
        );
        assert!(CopperWait::new(0xFFFF, 0xFFFE).is_end_of_list());
    }

    #[test]
    fn masked_unreachable_waits_are_end_of_list() {
        assert!(CopperWait::new(0xFFFF, 0xFFFC).is_end_of_list());
        assert!(CopperWait::new(0xFFFF, 0xFFF8).is_end_of_list());
        assert!(!CopperWait::new(0xFFFF, 0xFFFF).is_end_of_list());

        let reachable_last_visible_slot = CopperWait::new(0xFFE1, 0xFFFE);
        assert!(!reachable_last_visible_slot.is_end_of_list());
        assert!(reachable_last_visible_slot.is_satisfied(0xFF, 0xE0));
    }

    #[test]
    fn wait_uses_masks_and_keeps_bfd_out_of_position_compare() {
        let wait = CopperWait::new(0x8001, 0x7FFE);
        assert!(wait.blitter_wait_enabled());
        assert_eq!(wait.compare_mask() & 0x8000, 0x8000);
        assert!(!wait.is_satisfied(0x7F, 0));
        assert!(wait.is_satisfied(0x80, 0));

        let masked_h = CopperWait::new(0x5021, 0xFFF0);
        assert!(!masked_h.is_satisfied(0x50, 0x10));
        assert!(masked_h.is_satisfied(0x50, 0x20));
    }

    #[test]
    fn partial_mask_wait_is_satisfied_once_beam_reaches_or_passes_position() {
        // A copper wait fires when the beam reaches OR passes the masked
        // compare position (Amiga "wait until beam >= position"), not only on
        // an exact match. With the vertical phase masked out (mask 0x80FE),
        // any horizontal position at or beyond the target satisfies the wait
        // while VP7 still matches the target.
        let wait_next_line = CopperWait::new(0x0029, 0x80FE);
        assert!(wait_next_line.is_satisfied(0x00, 0xE0));
        assert!(wait_next_line.is_satisfied(0x00, 0x40));
        // Still unsatisfied before the target horizontal position is reached.
        assert!(!wait_next_line.is_satisfied(0x00, 0x10));
        assert_eq!(wait_next_line.cck_until_satisfied(0x49, 0x0E), Some(0x1A));
        assert!(wait_next_line.is_satisfied(0x100, 0x28));
        assert!(wait_next_line.is_satisfied(0x80, 0xE0));

        let wait_high_half = CopperWait::new(0x8029, 0x80FE);
        assert!(!wait_high_half.is_satisfied(0x00, 0x28));
        assert!(wait_high_half.is_satisfied(0x80, 0x28));
        assert!(wait_high_half.is_satisfied(0x100, 0x00));

        let wait_low_nibble = CopperWait::new(0x0F01, 0x8F00);
        assert!(wait_low_nibble.is_satisfied(0x0F, 0));
        assert!(!wait_low_nibble.is_satisfied(0x10, 0));
        assert!(wait_low_nibble.is_satisfied(0x80, 0));
    }

    #[test]
    fn full_mask_waits_stay_satisfied_after_target_beam_position() {
        let wait = CopperWait::new(0x5021, 0xFFFE);

        assert_eq!(wait.cck_until_satisfied(0x50, 0x10), Some(0x10));
        assert_eq!(wait.cck_until_satisfied(0x50, 0x20), Some(0));
        assert_eq!(wait.cck_until_satisfied(0x50, 0x22), Some(0));
        assert_eq!(wait.cck_until_satisfied(0x51, 0x00), Some(0));
    }

    #[test]
    fn full_mask_wait_after_line_255_waits_for_vertical_low_byte_rollover() {
        let wait = CopperWait::new(0x1B01, 0xFFFE);

        assert!(!wait.is_satisfied(0xFF, 0xDF));
        assert!(!wait.is_satisfied(0x100, 0));
        assert!(!wait.is_satisfied(0x11A, COLORCLOCKS_PER_LINE - 1));
        assert!(wait.is_satisfied(0x11B, 0));
        assert!(wait.is_satisfied(0x11C, 0));
        assert_eq!(
            wait.cck_until_satisfied(0xFF, 0xDF),
            Some((COLORCLOCKS_PER_LINE - 0xDF) + 0x1B * COLORCLOCKS_PER_LINE)
        );
        assert_eq!(
            wait.cck_until_satisfied(0x100, 0),
            Some(0x1B * COLORCLOCKS_PER_LINE)
        );
    }

    #[test]
    fn full_mask_high_half_wait_remains_satisfied_late_on_line_255() {
        let wait = CopperWait::new(0xFC01, 0xFFFE);

        assert!(wait.is_satisfied(0xFC, 0x04));
        assert!(wait.is_satisfied(0xFF, COLORCLOCKS_PER_LINE - 1));
        assert_eq!(
            wait.cck_until_satisfied(0xFF, COLORCLOCKS_PER_LINE - 1),
            Some(0)
        );
    }

    #[test]
    fn high_half_wait_remains_satisfied_after_line_255_rollover() {
        let wait = CopperWait::new(0x8021, 0x80FE);

        assert!(!wait.is_satisfied(0x7F, 0x20));
        assert!(wait.is_satisfied(0x80, 0x20));
        assert!(wait.is_satisfied(0x100, 0x00));
        // At the end of line 255 the high-half (VP7=1) target has already been
        // reached, so the wait is satisfied immediately (reach-or-pass) rather
        // than one colorclock later.
        assert_eq!(
            wait.cck_until_satisfied(0xFF, COLORCLOCKS_PER_LINE - 1),
            Some(0)
        );
    }

    #[test]
    fn wait_release_is_blocked_in_line_end_blackout() {
        // A VP7-masked horizontal wait (`WAIT $8033,$80FE`: VP7 set, h >= $32)
        // whose condition is already true late in a line must
        // not release in the last color clocks of that line; it releases at
        // the target hpos of the next line instead.
        let wait = CopperWait::new(0x8033, 0x80FE);
        let mut copper = Copper::new();

        copper.start_wait_instruction(wait);
        // Line-end positions: comparator blocked even though h >= 0x32.
        for hpos in (COLORCLOCKS_PER_LINE - 4)..COLORCLOCKS_PER_LINE {
            copper.advance_wait_free_cycle(136, hpos, false, COLORCLOCKS_PER_LINE);
            assert!(
                !copper.is_running(),
                "wait must not release at line-end hpos {hpos}"
            );
        }
        // Next line, before the horizontal target: still waiting.
        copper.advance_wait_free_cycle(137, 0x10, false, COLORCLOCKS_PER_LINE);
        assert!(!copper.is_running());
        // Next line, at the horizontal target: comparator matches, then the
        // Copper spends one bus-free wake-up cycle before running.
        copper.advance_wait_free_cycle(137, 0x32, false, COLORCLOCKS_PER_LINE);
        assert!(!copper.is_running());
        copper.advance_wait_free_cycle(137, 0x34, false, COLORCLOCKS_PER_LINE);
        assert!(copper.is_running());

        // Away from the line end the same satisfied wait still pays the wake-up
        // cycle before the next fetch can run.
        let mut copper = Copper::new();
        copper.start_wait_instruction(wait);
        copper.advance_wait_free_cycle(136, 0x80, false, COLORCLOCKS_PER_LINE);
        assert!(!copper.is_running());
        copper.advance_wait_free_cycle(136, 0x82, false, COLORCLOCKS_PER_LINE);
        assert!(copper.is_running());
    }

    #[test]
    fn wait_whose_target_is_inside_the_line_end_blackout_still_releases_there() {
        // A right-edge raster bar can use `WAIT $80E1,$80FE` (VP7 set,
        // h >= $E0 = 224) on a 227-cck line, so the wait's only true positions
        // ($E0..$E2) lie inside the line-end blackout. The blackout only defers
        // a wait that was already true *before* the blackout, so this wait must
        // still release at its target rather than stalling forever.
        let wait = CopperWait::new(0x80E1, 0x80FE);
        assert!(wait.is_satisfied(200, 0xE0));
        // Just before the blackout the wait is not yet satisfied, so it is not
        // deferred; it releases at its target hpos inside the blackout.
        let mut copper = Copper::new();
        copper.start_wait_instruction(wait);
        copper.advance_wait_free_cycle(200, 0xDE, false, COLORCLOCKS_PER_LINE);
        assert!(!copper.is_running());
        copper.advance_wait_free_cycle(200, 0xE0, false, COLORCLOCKS_PER_LINE);
        assert!(!copper.is_running(), "match cycle is bus-free wake-up");
        copper.advance_wait_free_cycle(200, 0xE2, false, COLORCLOCKS_PER_LINE);
        assert!(
            copper.is_running(),
            "wait whose target is in the blackout must release at that target"
        );
    }

    #[test]
    fn wait_wakeup_spends_dummy_access_cycle_before_running() {
        let wait = CopperWait::new(0x0033, 0x80FE);
        let mut copper = Copper::new();

        copper.start_wait_instruction(wait);
        copper.advance_wait_free_cycle(0, 0x34, false, COLORCLOCKS_PER_LINE);
        assert!(!copper.is_running());
        copper.advance_wait_free_cycle(0, 0x35, false, COLORCLOCKS_PER_LINE);
        assert!(!copper.is_running());
        copper.advance_wait_free_cycle(0, 0x36, false, COLORCLOCKS_PER_LINE);
        assert!(copper.is_running());
    }

    #[test]
    fn skip_condition_matches_wait_condition() {
        let skip = CopperWait::new(0x7F01, 0xFF01);
        assert!(!skip.is_satisfied(0x7E, 0));
        assert!(skip.is_satisfied(0x7F, 0));
    }

    #[test]
    fn dma_fetches_one_instruction_word_per_slot() {
        let mut copper = Copper::new();
        let chip_ram = [0x01, 0x80, 0x0A, 0xBC, 0x01, 0x82, 0x04, 0x56];
        copper.jump(0);
        assert_eq!(
            copper.fetch_decode(&chip_ram),
            CopperFetch::FirstWord {
                pc: 0,
                word: 0x0180
            }
        );
        assert_eq!(copper.pc(), 2);
        assert_eq!(
            copper.fetch_decode(&chip_ram),
            CopperFetch::Instruction {
                pc: 0,
                first: 0x0180,
                second: 0x0ABC,
                instruction: CopperInstruction::Move {
                    register: 0x0180,
                    value: 0x0ABC
                }
            }
        );
        assert_eq!(copper.pc(), 4);

        copper.jump(4);
        assert_eq!(
            copper.fetch_decode(&chip_ram),
            CopperFetch::FirstWord {
                pc: 4,
                word: 0x0182
            }
        );
        assert_eq!(copper.pc(), 6);
        assert_eq!(
            copper.fetch_decode(&chip_ram),
            CopperFetch::Instruction {
                pc: 4,
                first: 0x0182,
                second: 0x0456,
                instruction: CopperInstruction::Move {
                    register: 0x0182,
                    value: 0x0456
                }
            }
        );
        assert_eq!(copper.pc(), 8);
    }

    #[test]
    fn skip_latch_discards_only_a_fetched_move_instruction() {
        let mut copper = Copper::new();
        let chip_ram = [0x00, 0x00, 0x01, 0x80, 0x0A, 0xBC, 0x01, 0x82, 0x04, 0x56];
        copper.jump(2);
        copper.skip_next_move();

        assert!(matches!(
            copper.fetch_decode(&chip_ram),
            CopperFetch::FirstWord {
                pc: 2,
                word: 0x0180
            }
        ));
        assert_eq!(
            copper.fetch_decode(&chip_ram),
            CopperFetch::SkippedMove {
                pc: 2,
                first: 0x0180,
                second: 0x0ABC
            }
        );
        assert_eq!(copper.pc(), 6);
        assert!(matches!(
            copper.fetch_decode(&chip_ram),
            CopperFetch::FirstWord {
                pc: 6,
                word: 0x0182
            }
        ));
    }

    #[test]
    fn skip_latch_does_not_discard_wait_or_skip_instruction() {
        let mut copper = Copper::new();
        let chip_ram = [0x00, 0x00, 0x50, 0x21, 0xFF, 0xFE, 0x01, 0x82, 0x04, 0x56];
        copper.jump(2);
        copper.skip_next_move();

        assert!(matches!(
            copper.fetch_decode(&chip_ram),
            CopperFetch::FirstWord {
                pc: 2,
                word: 0x5021
            }
        ));
        assert_eq!(
            copper.fetch_decode(&chip_ram),
            CopperFetch::Instruction {
                pc: 2,
                first: 0x5021,
                second: 0xFFFE,
                instruction: CopperInstruction::Wait(CopperWait::new(0x5021, 0xFFFE))
            }
        );
    }

    #[test]
    fn jump_clears_partially_fetched_instruction() {
        let mut copper = Copper::new();
        let chip_ram = [0x01, 0x80, 0x0A, 0xBC, 0x01, 0x82, 0x04, 0x56];
        copper.jump(0x0002);
        assert!(matches!(
            copper.fetch_decode(&chip_ram),
            CopperFetch::FirstWord { .. }
        ));

        copper.jump(0x0004);
        assert_eq!(
            copper.fetch_decode(&chip_ram),
            CopperFetch::FirstWord {
                pc: 4,
                word: 0x0182
            }
        );
        assert_eq!(
            copper.fetch_decode(&chip_ram),
            CopperFetch::Instruction {
                pc: 4,
                first: 0x0182,
                second: 0x0456,
                instruction: CopperInstruction::Move {
                    register: 0x0182,
                    value: 0x0456
                }
            }
        );
    }

    #[test]
    fn finds_future_beam_time_for_waits() {
        let same_line = CopperWait::new(0x5021, 0xFFFE);
        assert_eq!(same_line.cck_until_satisfied(0x50, 0x10), Some(0x10));

        let later_line = CopperWait::new(0x5101, 0xFFFE);
        assert_eq!(
            later_line.cck_until_satisfied(0x50, COLORCLOCKS_PER_LINE - 2),
            Some(2)
        );

        assert_eq!(
            CopperWait::new(0xFFFF, 0xFFFE).cck_until_satisfied(0, 0),
            None
        );
    }
}
