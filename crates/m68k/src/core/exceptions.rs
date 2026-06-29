//! Exception handling.
//!
//! Defines exception vectors and processing.

use super::cpu::{CpuCore, SFLAG_SET};
use super::execute::RUN_MODE_BERR_AERR_RESET;
use super::memory::AddressBus;
use super::types::CpuType;

/// Exception vector numbers.
pub mod vector {
    pub const RESET_SSP: u32 = 0;
    pub const RESET_PC: u32 = 1;
    pub const BUS_ERROR: u32 = 2;
    pub const ADDRESS_ERROR: u32 = 3;
    pub const ILLEGAL_INSTRUCTION: u32 = 4;
    pub const ZERO_DIVIDE: u32 = 5;
    pub const CHK: u32 = 6;
    pub const TRAPV: u32 = 7;
    pub const PRIVILEGE_VIOLATION: u32 = 8;
    pub const TRACE: u32 = 9;
    pub const LINE_1010: u32 = 10;
    pub const LINE_1111: u32 = 11;
    pub const FORMAT_ERROR: u32 = 14;
    pub const UNINITIALIZED_INTERRUPT: u32 = 15;
    pub const SPURIOUS_INTERRUPT: u32 = 24;
    pub const TRAP_BASE: u32 = 32;

    // 68020+ MMU exceptions (vector numbers per 68k docs; used by 68030/68040 PMMU).
    pub const MMU_CONFIGURATION_ERROR: u32 = 56;
    pub const MMU_ILLEGAL_OPERATION_ERROR: u32 = 57;
    pub const MMU_ACCESS_LEVEL_VIOLATION_ERROR: u32 = 58;
}

/// Function code bits for exception stack frames.
pub mod fc {
    pub const USER_DATA: u16 = 1;
    pub const USER_PROGRAM: u16 = 2;
    pub const SUPERVISOR_DATA: u16 = 5;
    pub const SUPERVISOR_PROGRAM: u16 = 6;
}

impl CpuCore {
    #[inline]
    fn push_16_raw<B: AddressBus>(&mut self, bus: &mut B, value: u16) {
        self.dar[15] = self.dar[15].wrapping_sub(2);
        bus.write_word(self.address(self.dar[15]), value);
    }

    #[inline]
    fn push_32_raw<B: AddressBus>(&mut self, bus: &mut B, value: u32) {
        self.dar[15] = self.dar[15].wrapping_sub(4);
        bus.write_long(self.address(self.dar[15]), value);
    }

    #[inline]
    fn fake_push_16_raw(&mut self) {
        self.dar[15] = self.dar[15].wrapping_sub(2);
    }

    #[inline]
    fn fake_push_32_raw(&mut self) {
        self.dar[15] = self.dar[15].wrapping_sub(4);
    }

    /// Push the 68000's 3-word exception frame (SR + PC) in the bus order the
    /// hardware uses: PC low word first, then SR, then PC high word.
    pub(crate) fn push_exception_frame_68000<B: AddressBus>(
        &mut self,
        bus: &mut B,
        stacked_pc: u32,
        sr: u16,
    ) {
        let sp = self.dar[15].wrapping_sub(6);
        self.dar[15] = sp;
        self.write_16(bus, sp.wrapping_add(4), (stacked_pc & 0xFFFF) as u16);
        self.write_16(bus, sp, sr);
        self.write_16(bus, sp.wrapping_add(2), (stacked_pc >> 16) as u16);
    }

    /// Process TRAP #n instruction.
    pub fn trap<B: AddressBus>(&mut self, bus: &mut B, trap_num: u8) -> i32 {
        let vector = vector::TRAP_BASE + (trap_num & 0xF) as u32;

        // Musashi 68020/68030 uses "format 2" stack frame for TRAP exceptions.
        // 68040+ uses format 0 (same as simple exceptions).
        let uses_format_2 = matches!(
            self.cpu_type,
            super::types::CpuType::M68EC020
                | super::types::CpuType::M68020
                | super::types::CpuType::M68EC030
                | super::types::CpuType::M68030
        );
        if uses_format_2 {
            let old_sr = self.get_sr();
            // Match Musashi m68ki_init_exception: enter supervisor, clear trace.
            self.set_s_flag(SFLAG_SET);
            self.t1_flag = 0;
            self.t0_flag = 0;

            // Stacked PC for TRAP is the next instruction.
            let stacked_pc = self.pc;
            let vec_word = (vector as u16) << 2;

            // Musashi m68ki_stack_frame_0010:
            // push PPC (long), then 0x2000|(vector<<2) (word), then PC (long), then SR (word)
            self.push_32(bus, self.ppc);
            self.push_16(bus, 0x2000 | (vec_word & 0x0FFF));
            self.push_32(bus, stacked_pc);
            self.push_16(bus, old_sr);

            self.jump_vector(bus, vector);
            return self.exception_cycles(vector);
        }

        self.take_exception(bus, vector)
    }

    /// Process CHK exception.
    ///
    /// The caller (exec_chk) reports the comparison's internal clocks before
    /// calling this (8 for trap-on-too-big, 10 for trap-on-negative).
    pub fn exception_chk<B: AddressBus>(&mut self, bus: &mut B) -> i32 {
        let old_sr = self.get_sr();

        // Enter supervisor, clear trace
        self.set_s_flag(SFLAG_SET);
        self.t1_flag = 0;
        self.t0_flag = 0;

        // Musashi treats CHK as a group-2 exception:
        // - 68000: 3-word frame (PC, SR)
        // - 68010: format-0 frame (vector<<2, PC, SR)
        // - 68020+: format-2 frame (PPC, 0x2000|vector<<2, PC, SR)
        //
        // CHK stacks the next PC (self.pc) and includes PPC in the 020+ format-2 frame.
        match self.cpu_type {
            super::types::CpuType::M68000 => {
                let pc = self.pc;
                self.push_exception_frame_68000(bus, pc, old_sr);
            }
            super::types::CpuType::M68010 | super::types::CpuType::SCC68070 => {
                self.push_16(bus, (vector::CHK as u16) << 2);
                self.push_32(bus, self.pc);
                self.push_16(bus, old_sr);
            }
            _ => {
                let vec_word = (vector::CHK as u16) << 2;
                self.push_32(bus, self.ppc);
                self.push_16(bus, 0x2000 | (vec_word & 0x0FFF));
                self.push_32(bus, self.pc);
                self.push_16(bus, old_sr);
            }
        }

        // Jump to vector
        self.jump_vector(bus, vector::CHK);

        self.exception_cycles(vector::CHK)
    }

    /// Process zero divide exception.
    pub fn exception_zero_divide<B: AddressBus>(&mut self, bus: &mut B) -> i32 {
        self.take_exception(bus, vector::ZERO_DIVIDE)
    }

    /// Process privilege violation exception.
    pub fn exception_privilege<B: AddressBus>(&mut self, bus: &mut B) -> i32 {
        self.take_exception(bus, vector::PRIVILEGE_VIOLATION)
    }

    /// Process trace exception.
    pub fn exception_trace<B: AddressBus>(&mut self, bus: &mut B) -> i32 {
        self.take_exception(bus, vector::TRACE)
    }

    /// Process address error exception.
    ///
    /// 68000 pushes additional info: access address, instruction register, status.
    pub fn exception_address_error<B: AddressBus>(
        &mut self,
        bus: &mut B,
        address: u32,
        write: bool,
        instruction: bool,
    ) -> i32 {
        let old_sr = self.get_sr();
        let was_supervisor = (old_sr & 0x2000) != 0;

        // Enter supervisor mode, clear trace
        self.set_s_flag(SFLAG_SET);
        self.t1_flag = 0;
        self.t0_flag = 0;

        // Build function code / status word
        // Bits: R/W (4), I/N (3), Function Code (2:0)
        let fc = if was_supervisor {
            if instruction {
                fc::SUPERVISOR_PROGRAM
            } else {
                fc::SUPERVISOR_DATA
            }
        } else if instruction {
            fc::USER_PROGRAM
        } else {
            fc::USER_DATA
        };
        let status_word = fc | if write { 0 } else { 0x10 } | if instruction { 0 } else { 0x08 };

        match self.cpu_type {
            CpuType::M68000 => {
                // 68000 address error frame (14 bytes):
                // Push: PC (4), SR (2), IR (2), Access Address (4), Status Word (2)
                //
                // Use raw bus writes (no alignment/address-error checks) to avoid recursive
                // address-error exceptions if the stack pointer is itself misaligned.
                self.push_16_raw(bus, status_word);
                self.push_32_raw(bus, address);
                self.push_16_raw(bus, self.ir as u16);
                self.push_16_raw(bus, old_sr);
                self.push_32_raw(bus, self.ppc);
            }
            CpuType::M68010 | CpuType::SCC68070 => {
                // 68010 uses the "format 8" (0x8) bus/address error stack frame (29 words).
                // We intentionally mirror Musashi's placeholder implementation here: most internal
                // words are zero/undefined and we primarily preserve the format/vector word, PC, SR.
                //
                // Layout (from Musashi m68kcpu.h m68ki_stack_frame_1000):
                // - lots of internal words (mostly not written)
                // - fault address (long) = 0
                // - special status word = 0
                // - format/vector word = 0x8000 | (vector<<2)
                // - stacked PC (long)
                // - stacked SR (word)
                for _ in 0..8 {
                    self.fake_push_32_raw();
                }
                self.push_16_raw(bus, 0); // instruction input buffer
                self.fake_push_16_raw();
                self.push_16_raw(bus, 0); // data input buffer
                self.fake_push_16_raw();
                self.push_16_raw(bus, 0); // data output buffer
                self.fake_push_16_raw();
                self.push_32_raw(bus, 0); // fault address
                self.push_16_raw(bus, 0); // special status word
                self.push_16_raw(bus, 0x8000 | ((vector::ADDRESS_ERROR as u16) << 2));
                self.push_32_raw(bus, self.ppc);
                self.push_16_raw(bus, old_sr);
            }
            _ => {
                // TODO: 68020+ address error stack frames (format A/B/7 variants) are not yet
                // implemented. Use a minimal 68010+ format-0-like frame to avoid totally losing
                // control flow, but this is not architecturally accurate.
                self.push_16_raw(bus, (vector::ADDRESS_ERROR as u16) << 2);
                self.push_32_raw(bus, self.ppc);
                self.push_16_raw(bus, old_sr);
                let _ = (status_word, address); // currently unused in this fallback
            }
        }

        // Jump to vector
        self.jump_vector(bus, vector::ADDRESS_ERROR);

        50 // Cycles for address error
    }

    /// Process bus error exception.
    pub fn exception_bus_error<B: AddressBus>(
        &mut self,
        bus: &mut B,
        address: u32,
        write: bool,
        instruction: bool,
    ) -> i32 {
        let old_sr = self.get_sr();
        let was_supervisor = (old_sr & 0x2000) != 0;

        // Enter supervisor mode, clear trace
        self.set_s_flag(SFLAG_SET);
        self.t1_flag = 0;
        self.t0_flag = 0;

        // Build function code / status word
        let fc = if was_supervisor {
            if instruction {
                fc::SUPERVISOR_PROGRAM
            } else {
                fc::SUPERVISOR_DATA
            }
        } else if instruction {
            fc::USER_PROGRAM
        } else {
            fc::USER_DATA
        };
        let status_word = fc | if write { 0 } else { 0x10 } | if instruction { 0 } else { 0x08 };

        match self.cpu_type {
            CpuType::M68000 => {
                // 68000 bus error frame (same as address error)
                self.push_16_raw(bus, status_word);
                self.push_32_raw(bus, address);
                self.push_16_raw(bus, self.ir as u16);
                self.push_16_raw(bus, old_sr);
                self.push_32_raw(bus, self.ppc);
            }
            CpuType::M68010 | CpuType::SCC68070 => {
                // 68010 format 8 (0x8) bus error frame (placeholder, matching Musashi).
                for _ in 0..8 {
                    self.fake_push_32_raw();
                }
                self.push_16_raw(bus, 0); // instruction input buffer
                self.fake_push_16_raw();
                self.push_16_raw(bus, 0); // data input buffer
                self.fake_push_16_raw();
                self.push_16_raw(bus, 0); // data output buffer
                self.fake_push_16_raw();
                self.push_32_raw(bus, 0); // fault address
                self.push_16_raw(bus, 0); // special status word
                self.push_16_raw(bus, 0x8000 | ((vector::BUS_ERROR as u16) << 2));
                self.push_32_raw(bus, self.ppc);
                self.push_16_raw(bus, old_sr);
                let _ = (status_word, address); // currently unused in this placeholder
            }
            _ if self.is_040() => {
                // 68040 access-error stack frame (format 7, 30 words). The
                // caller (trigger_bus_error) has already rolled the instruction
                // back, so we stack PPC and leave the writeback/continuation
                // fields clear: RTE then restarts the faulting instruction
                // (demand-paging / Enforcer fix-and-retry model). The SSW
                // reports R/W and the function code; the size field is left at
                // its long default since the restart re-runs the real access.
                // Layout (Musashi m68ki_stack_frame_0111), pushed high->low.
                let rw = if write { 0 } else { 0x0100 }; // SSW bit 8: 1 = read
                let ssw = rw | (fc as u16 & 0x7); // TM = function code
                let fmt_vec = 0x7000 | ((vector::BUS_ERROR as u16) << 2);
                // PD2/PD1/PD0 and the three writeback entries are all unused.
                for _ in 0..3 {
                    self.push_32_raw(bus, 0); // PD2, PD1, PD0
                }
                self.push_32_raw(bus, 0); // WB1D
                self.push_32_raw(bus, 0); // WB1A
                self.push_32_raw(bus, 0); // WB2D
                self.push_32_raw(bus, 0); // WB2A
                self.push_32_raw(bus, 0); // WB3D
                self.push_32_raw(bus, 0); // WB3A
                self.push_32_raw(bus, address); // fault address
                self.push_16_raw(bus, 0); // WB1S (status: invalid -> no writeback)
                self.push_16_raw(bus, 0); // WB2S
                self.push_16_raw(bus, 0); // WB3S
                self.push_16_raw(bus, ssw); // special status word
                self.push_32_raw(bus, address); // effective address
                self.push_16_raw(bus, fmt_vec); // format 7 / vector offset
                self.push_32_raw(bus, self.ppc); // restart PC
                self.push_16_raw(bus, old_sr);
            }
            _ => {
                // TODO: 68020/68030 bus-error stack frames (format A / long
                // format B) are not yet implemented. Minimal fallback.
                self.push_16_raw(bus, (vector::BUS_ERROR as u16) << 2);
                self.push_32_raw(bus, self.ppc);
                self.push_16_raw(bus, old_sr);
                let _ = (status_word, address);
            }
        }

        // Jump to vector
        self.jump_vector(bus, vector::BUS_ERROR);

        50 // Cycles for bus error
    }

    /// Common exception processing (simple frame: SR, PC).
    ///
    /// Implements double-fault detection: if an exception occurs while already
    /// processing an exception, the CPU halts (similar to x86 triple fault).
    pub fn take_exception<B: AddressBus>(&mut self, bus: &mut B, vector: u32) -> i32 {
        // Double-fault detection: if we're already processing an exception and
        // another exception occurs, halt the CPU. This prevents infinite recursion.
        if self.exception_processing {
            // Double fault - halt the CPU
            self.stopped = 1;
            self.run_mode = RUN_MODE_BERR_AERR_RESET;
            return 0;
        }

        // Mark that we're processing an exception. This flag is checked by translate()
        // to bypass MMU translation during exception frame writes.
        self.exception_processing = true;

        // Exception entry spends 4 internal clocks (vector number / state
        // capture) before the first stack write.
        self.internal_cycles(4);

        let old_sr = self.get_sr();

        // Match Musashi `m68ki_init_exception`: enter supervisor mode but do not modify M.
        self.set_s_flag(SFLAG_SET);

        // Clear trace flags
        self.t1_flag = 0;
        self.t0_flag = 0;

        // Select stacked PC (Musashi-style: traps/interrupts stack the next PC; faults stack PPC).
        let stacked_pc = if vector == vector::TRAPV
            || vector == vector::TRACE
            || vector == vector::ZERO_DIVIDE
            || (vector::TRAP_BASE..vector::TRAP_BASE + 16).contains(&vector)
            || (24..=31).contains(&vector)
        {
            self.pc
        } else {
            self.ppc
        };

        // Match Musashi `m68ki_stack_frame_0000`:
        // - 68000: push PC, then SR (3-word frame)
        // - 68010+: push vector offset word (vector<<2), then PC, then SR (format 0)
        if self.cpu_type == super::types::CpuType::M68000 {
            self.push_exception_frame_68000(bus, stacked_pc, old_sr);
        } else {
            self.push_16(bus, (vector as u16) << 2);
            self.push_32(bus, stacked_pc);
            self.push_16(bus, old_sr);
        }

        // Read vector and jump
        self.jump_vector(bus, vector);

        // Done processing exception
        self.exception_processing = false;

        self.exception_cycles(vector)
    }

    /// Get cycles for exception processing.
    fn exception_cycles(&self, vector: u32) -> i32 {
        match vector {
            vector::RESET_SSP | vector::RESET_PC => 40,
            vector::BUS_ERROR | vector::ADDRESS_ERROR => 50,
            vector::ILLEGAL_INSTRUCTION => 34,
            vector::ZERO_DIVIDE => 38,
            vector::CHK => 40,
            vector::TRAPV => 34,
            vector::PRIVILEGE_VIOLATION => 34,
            vector::TRACE => 34,
            vector::LINE_1010 | vector::LINE_1111 => 34,
            24..=31 => 44, // Autovector interrupts
            _ => 34,       // TRAPs and user vectors
        }
    }

    /// Check for trace exception after instruction execution.
    pub fn check_trace(&mut self) -> bool {
        // T1 trace: trace after every instruction
        // T0 trace: trace only on change-of-flow (68020+)
        // We check the T1/T0 bits from sr_save (SR BEFORE instruction), not current SR.
        // This is important for RTE: if RTE restores T1=1, we don't take trace immediately.
        let t1_before = (self.sr_save & 0x8000) != 0;
        let t0_before = (self.sr_save & 0x4000) != 0;
        let should_trace = t1_before || (t0_before && self.change_of_flow);
        // Reset change_of_flow flag after checking
        self.change_of_flow = false;
        should_trace
    }

    // ========== Fallback Exception Methods for Unhandled Traps ==========

    /// Take A-line exception for an unhandled trap.
    ///
    /// Call this to manually take an A-line exception (vector 10).
    /// Note: With `step()`, this is called automatically. This method is
    /// primarily used internally by `step_with_hle_handler()` when the
    /// handler returns `false`.
    ///
    /// This rewinds the PC to the trap instruction before taking the exception.
    pub fn take_aline_exception<B: AddressBus>(&mut self, bus: &mut B) -> i32 {
        self.pc = self.ppc; // Rewind PC to the trap instruction
        self.take_exception(bus, vector::LINE_1010)
    }

    /// Take F-line exception for an unhandled trap.
    ///
    /// Call this after receiving `StepResult::FlineTrap` if you cannot handle the trap.
    /// This rewinds the PC and takes the real hardware exception (vector 11).
    pub fn take_fline_exception<B: AddressBus>(&mut self, bus: &mut B) -> i32 {
        self.pc = self.ppc; // Rewind PC to the trap instruction
        self.take_exception(bus, vector::LINE_1111)
    }

    /// Take TRAP #n exception for an unhandled trap.
    ///
    /// Call this after receiving `StepResult::TrapInstruction` if you cannot handle the trap.
    /// This takes the real hardware exception (vector 32+n).
    ///
    /// Note: Unlike A-line/F-line exceptions, TRAP exceptions stack the PC of the
    /// instruction AFTER the TRAP, so we do NOT rewind PC here.
    pub fn take_trap_exception<B: AddressBus>(&mut self, bus: &mut B, trap_num: u8) -> i32 {
        // Don't rewind PC - TRAP stacks the NEXT instruction address
        self.trap(bus, trap_num)
    }

    /// Take BKPT exception for an unhandled breakpoint.
    ///
    /// Call this after receiving `StepResult::Breakpoint` if you cannot handle it.
    /// This rewinds the PC and takes the illegal instruction exception (vector 4).
    pub fn take_bkpt_exception<B: AddressBus>(&mut self, bus: &mut B) -> i32 {
        self.pc = self.ppc; // Rewind PC to the breakpoint instruction
        self.take_exception(bus, vector::ILLEGAL_INSTRUCTION)
    }

    /// Take illegal instruction exception for an unhandled illegal opcode.
    ///
    /// Call this after receiving `StepResult::IllegalInstruction` if you cannot handle it.
    /// This rewinds the PC and takes the real hardware exception (vector 4).
    pub fn take_illegal_exception<B: AddressBus>(&mut self, bus: &mut B) -> i32 {
        self.pc = self.ppc; // Rewind PC to the illegal instruction
        self.take_exception(bus, vector::ILLEGAL_INSTRUCTION)
    }
}
