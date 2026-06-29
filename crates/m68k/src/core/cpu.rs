//! CPU core state structure.
//!
//! Mirrors Musashi's `m68ki_cpu_core` for complete M68000 family emulation.

use super::execute::RUN_MODE_BERR_AERR_RESET;
use super::memory::{AddressBus, BusFaultKind};
use super::types::CpuType;
use crate::fpu::FloatX80;

/// Flag constants for SR bits.
pub const XFLAG_SET: u32 = 0x100;
pub const NFLAG_SET: u32 = 0x80;
pub const VFLAG_SET: u32 = 0x80;
pub const CFLAG_SET: u32 = 0x100;
pub const SFLAG_SET: u32 = 4;
pub const MFLAG_SET: u32 = 2;

/// Function codes for memory access.
pub const FC_USER_DATA: u32 = 1;
pub const FC_USER_PROGRAM: u32 = 2;
pub const FC_SUPERVISOR_DATA: u32 = 5;
pub const FC_SUPERVISOR_PROGRAM: u32 = 6;

/// The main CPU state structure.
///
/// Matches Musashi's `m68ki_cpu_core` layout for compatibility.
///
/// Every field is plain runtime or configuration state (there are no
/// lazily built decode tables), so the whole struct round-trips through
/// serde for host save states.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct CpuCore {
    // ========== Registers ==========
    /// Data and Address registers (D0-D7, A0-A7)
    pub dar: [u32; 16],
    /// Saved registers for bus/address error recovery
    pub dar_save: [u32; 16],
    /// Saved SR for bus/address error recovery (captured at start of instruction).
    pub sr_save: u16,
    /// Previous program counter
    pub ppc: u32,
    /// Program counter
    pub pc: u32,
    /// Stack pointers: [USP, _, _, _, ISP, _, MSP, _]
    /// Index: s_flag | ((s_flag >> 1) & m_flag)
    pub sp: [u32; 8],
    /// Vector Base Register (68010+)
    pub vbr: u32,
    /// Source Function Code (68010+)
    pub sfc: u32,
    /// Destination Function Code (68010+)
    pub dfc: u32,
    /// Cache Control Register (68020+). Only the persistent control bits
    /// are stored; the write-only clear strobes land in
    /// `cacr_pending_ops` for the host to act on.
    pub cacr: u32,
    /// Cache Address Register (68020+)
    pub caar: u32,
    /// CACR clear strobes (CI/CEI, and CD/CED on the 68030) accumulated
    /// since the host last drained them. These bits trigger a cache
    /// invalidation when written and always read back as zero, so they
    /// are latched here instead of in `cacr`.
    pub cacr_pending_ops: u32,
    /// Instruction Transparent Translation 0 (68040)
    pub itt0: u32,
    /// Instruction Transparent Translation 1 (68040)
    pub itt1: u32,
    /// Data Transparent Translation 0 (68040)
    pub dtt0: u32,
    /// Data Transparent Translation 1 (68040)
    pub dtt1: u32,
    /// Instruction Register (current opcode)
    pub ir: u32,

    // ========== FPU Registers (68881/68882/68040) ==========
    /// FPU Data Registers (FP0-FP7) - 80-bit extended precision.
    pub fpr: [FloatX80; 8],
    /// FPU Instruction Address Register
    pub fpiar: u32,
    /// FPU Status Register
    pub fpsr: u32,
    /// FPU Control Register
    pub fpcr: u32,

    // ========== Flags (stored separately for speed) ==========
    /// Trace 1 flag (T1 bit of SR)
    pub t1_flag: u32,
    /// Trace 0 flag (T0 bit of SR, 68020+)
    pub t0_flag: u32,
    /// Supervisor flag (0 or SFLAG_SET=4)
    pub s_flag: u32,
    /// Master/Interrupt state (0 or MFLAG_SET=2, 68020+)
    pub m_flag: u32,
    /// Extend flag (X)
    pub x_flag: u32,
    /// Negative flag (N)
    pub n_flag: u32,
    /// Zero flag (inverted: 0 = Z set, non-zero = Z clear)
    pub not_z_flag: u32,
    /// Overflow flag (V)
    pub v_flag: u32,
    /// Carry flag (C)
    pub c_flag: u32,
    /// Interrupt mask (I0-I2)
    pub int_mask: u32,

    // ========== Interrupt State ==========
    /// Current interrupt level
    pub int_level: u32,
    /// Stopped state (STOP instruction)
    pub stopped: u32,
    /// Change-of-flow flag for T0 trace (set by BRA, JMP, JSR, RTS, etc.)
    pub change_of_flow: bool,

    // ========== Prefetch (Part E.1, 68000 only) ==========
    /// The 68000's two-word instruction prefetch queue (IRD/IRC model).
    ///
    /// `prefetch_queue[0..prefetch_count]` hold the words at `pc`,
    /// `pc + 2`. Consuming a word (see `read_imm_16`) takes from the queue
    /// without a bus access; when the queue is empty the word is fetched
    /// directly. At the end of every instruction the queue is topped back up
    /// to two words (`top_up_prefetch`) -- the prefetch bus reads real
    /// hardware performs during instruction execution. Flow-change
    /// instructions skip the top-up: they discard the queue and refill it
    /// from the branch target instead (`full_prefetch`), which is why taken
    /// branches cost two bus reads at the target and why words prefetched
    /// past a taken branch are discarded.
    pub prefetch_queue: [u16; 2],
    /// Number of valid words in `prefetch_queue` (0..=2), starting at `pc`.
    pub prefetch_count: u8,
    /// Microcode mode: when set, instruction-stream consumes skip their
    /// accompanying prefetch ("np") bus read. Flow-change instructions set
    /// this while consuming displacement/address words on paths that will
    /// discard the queue and refill from the branch target -- real hardware
    /// never prefetches ahead of a stream it is about to abandon.
    pub consume_without_prefetch: bool,
    /// Internal (non-bus) CPU clocks accumulated since the last bus access
    /// (Part E.2 precise timing). Reported to the host via
    /// `AddressBus::sync` immediately before the next access.
    pub pending_sync_clocks: u32,

    // ========== CPU Configuration ==========
    /// CPU type
    pub cpu_type: CpuType,
    /// Address mask (24-bit for 68000, 32-bit for 68020+)
    pub address_mask: u32,
    /// SR mask (implemented bits)
    pub sr_mask: u32,
    /// Instruction mode
    pub instr_mode: u32,
    /// Run mode (normal, bus error, address error)
    pub run_mode: u32,
    /// True while processing an exception (for double-fault detection)
    pub exception_processing: bool,

    // ========== MMU State ==========
    /// Has PMMU
    pub has_pmmu: bool,
    /// PMMU enabled
    pub pmmu_enabled: bool,
    /// Cached `cpu_type` is pre-68020. Updated in `set_cpu_type`.
    pub is_pre_68020: bool,
    /// FPU just reset
    pub fpu_just_reset: bool,
    /// Reset cycles counter
    pub reset_cycles: u32,

    // ========== Cycle Timing ==========
    /// Cycles for Bcc not taken (byte)
    pub cyc_bcc_notake_b: i32,
    /// Cycles for Bcc not taken (word)
    pub cyc_bcc_notake_w: i32,
    /// Cycles for DBcc false, no expiration
    pub cyc_dbcc_f_noexp: i32,
    /// Cycles for DBcc false, expiration
    pub cyc_dbcc_f_exp: i32,
    /// Cycles for Scc register true
    pub cyc_scc_r_true: i32,
    /// Cycles per word for MOVEM
    pub cyc_movem_w: i32,
    /// Cycles per long for MOVEM
    pub cyc_movem_l: i32,
    /// Cycles per shift count
    pub cyc_shift: i32,
    /// Cycles for RESET instruction
    pub cyc_reset: i32,

    // ========== Virtual IRQ ==========
    pub virq_state: u32,
    pub nmi_pending: u32,

    // ========== MMU Registers ==========
    // One canonical register set is shared by the 68030 and 68040 paths so the
    // page-table walker and the register writes can never desync. The 68040
    // root pointers (URP/SRP) overload the CRP/SRP address slots: `mmu_crp_aptr`
    // is the CRP on the 030 and the URP on the 040, `mmu_srp_aptr` is the SRP on
    // both. The two formats never coexist (the walker dispatches on `cpu_type`),
    // and the 040 root pointers carry no limit longword, so the 040 ignores the
    // `*_limit` fields.
    pub mmu_crp_aptr: u32, // CRP (030) / URP (040) address pointer
    pub mmu_crp_limit: u32, // CRP limit/mode (030 only)
    pub mmu_srp_aptr: u32, // SRP address pointer (030 + 040)
    pub mmu_srp_limit: u32, // SRP limit/mode (030 only)
    pub mmu_tc: u32,       // Translation Control (030 PMOVE / 040 MOVEC 0x003)
    pub mmu_sr: u32,       // MMU Status Register (030 + 040; layout per cpu_type)
    // 68030 Transparent Translation Registers
    pub mmu_tt0: u32,
    pub mmu_tt1: u32,
    // 68040-specific MMU registers
    pub dacr0: u32, // Data Access Control 0 (0x008)
    pub dacr1: u32, // Data Access Control 1 (0x009)
    pub iacr0: u32, // Instruction Access Control 0 (0x00A)
    pub iacr1: u32, // Instruction Access Control 1 (0x00B)

    // ========== Execution State ==========
    /// Remaining cycles in current timeslice
    pub cycles_remaining: i32,
    /// Initial cycles for timeslice
    pub initial_cycles: i32,

    /// When enabled, use SingleStepTests/MAME-derived semantics for a few edge cases where
    /// Musashi and MAME fixtures intentionally differ (notably BCD "invalid digit" behavior and
    pub sst_m68000_compat: bool,
}

// CACR bit assignments (68020/68030). The 68040 redefines CACR (IE/DE
// enables only, with CINV/CPUSH instructions doing invalidation); its bits
// are below (CACR_040_*).
/// Enable instruction cache.
pub const CACR_EI: u32 = 1 << 0;
/// Freeze instruction cache (hits served, misses do not allocate).
pub const CACR_FI: u32 = 1 << 1;
/// Clear instruction cache entry indexed by CAAR (write-only strobe).
pub const CACR_CEI: u32 = 1 << 2;
/// Clear instruction cache (write-only strobe).
pub const CACR_CI: u32 = 1 << 3;
/// Instruction burst enable (68030; stored, no timing effect here).
pub const CACR_IBE: u32 = 1 << 4;
/// Enable data cache (68030).
pub const CACR_ED: u32 = 1 << 8;
/// Freeze data cache (68030).
pub const CACR_FD: u32 = 1 << 9;
/// Clear data cache entry indexed by CAAR (68030; write-only strobe).
pub const CACR_CED: u32 = 1 << 10;
/// Clear data cache (68030; write-only strobe).
pub const CACR_CD: u32 = 1 << 11;
/// Data burst enable (68030; stored, no timing effect here).
pub const CACR_DBE: u32 = 1 << 12;
/// Write allocate (68030; stored, the host model is write-through).
pub const CACR_WA: u32 = 1 << 13;

// CACR bit assignments (68040). The 68040 CACR has only two defined bits -
// the cache enables - and no freeze/clear strobes: invalidation is done with
// the CINV/CPUSH instructions instead (see decode.rs). All other bits are
// reserved and read back as zero.
/// Enable instruction cache (68040).
pub const CACR_040_IE: u32 = 1 << 15;
/// Enable data cache (68040).
pub const CACR_040_DE: u32 = 1 << 31;

impl Default for CpuCore {
    fn default() -> Self {
        Self::new()
    }
}

impl CpuCore {
    /// Approximate 68020/030/040 instruction timing from the 68000 cycle
    /// counts the instruction handlers produce: the 020's three-stage
    /// pipeline and instruction cache make most instructions cost roughly
    /// half their 68000 cycles (memory-bound work is additionally dominated
    /// by the host bus model), with a two-cycle floor. Calibrated against a
    /// cycle-exact A1200 reference (FS-UAE) using the Copperline
    /// timing-test ADF: 68000 MULU.W #imm with 8 set bits = 54 cycles
    /// scales to the measured 27, and the chip-RAM dbra loop lands on the
    /// measured 8 clocks per iteration. The 68000/68010 paths (validated
    /// against SingleStepTests) are untouched; see the "68020+ timing"
    /// section of docs/internals/cpu.md for why no per-instruction 020
    /// reference exists.
    #[inline]
    pub(crate) fn scale_cycles_for_cpu_type(&self, cycles: i32) -> i32 {
        use crate::core::types::CpuType;
        match self.cpu_type {
            CpuType::M68000 | CpuType::M68010 | CpuType::Invalid => cycles,
            _ => ((cycles * 5 + 7) / 8).max(2),
        }
    }

    /// Create a new CPU with M68000 defaults.
    pub fn new() -> Self {
        let mut cpu = Self {
            dar: [0; 16],
            dar_save: [0; 16],
            sr_save: 0,
            ppc: 0,
            pc: 0,
            sp: [0; 8],
            vbr: 0,
            sfc: 0,
            dfc: 0,
            cacr: 0,
            caar: 0,
            cacr_pending_ops: 0,
            itt0: 0,
            itt1: 0,
            dtt0: 0,
            dtt1: 0,
            ir: 0,
            fpr: [FloatX80::default(); 8],
            fpiar: 0,
            fpsr: 0,
            fpcr: 0,
            t1_flag: 0,
            t0_flag: 0,
            s_flag: SFLAG_SET, // Start in supervisor mode
            m_flag: 0,
            x_flag: 0,
            n_flag: 0,
            not_z_flag: 1, // Z = 0 (not set)
            v_flag: 0,
            c_flag: 0,
            int_mask: 0x0700, // Mask all interrupts
            int_level: 0,
            stopped: 0,
            change_of_flow: false,
            prefetch_queue: [0; 2],
            prefetch_count: 0,
            consume_without_prefetch: false,
            pending_sync_clocks: 0,
            cpu_type: CpuType::M68000,
            address_mask: 0x00FFFFFF, // 24-bit for 68000
            sr_mask: 0xA71F,          // T1 -- S -- -- I2 I1 I0 -- -- -- X N Z V C
            instr_mode: 0,
            run_mode: 0,
            exception_processing: false,
            has_pmmu: false,
            pmmu_enabled: false,
            is_pre_68020: true,
            // A real 6888x comes out of reset in the NULL state: FSAVE
            // writes a NULL frame until the first FPU instruction runs.
            fpu_just_reset: true,
            reset_cycles: 0,
            cyc_bcc_notake_b: -2,
            cyc_bcc_notake_w: 2,
            cyc_dbcc_f_noexp: -2,
            cyc_dbcc_f_exp: 2,
            cyc_scc_r_true: 2,
            cyc_movem_w: 2,
            cyc_movem_l: 3,
            cyc_shift: 1,
            cyc_reset: 132,
            virq_state: 0,
            nmi_pending: 0,
            mmu_crp_aptr: 0,
            mmu_crp_limit: 0,
            mmu_srp_aptr: 0,
            mmu_srp_limit: 0,
            mmu_tc: 0,
            mmu_sr: 0,
            mmu_tt0: 0,
            mmu_tt1: 0,
            dacr0: 0,
            dacr1: 0,
            iacr0: 0,
            iacr1: 0,
            cycles_remaining: 0,
            initial_cycles: 0,
            sst_m68000_compat: false,
        };
        cpu.set_cpu_type(CpuType::M68000);
        cpu
    }

    /// Enable/disable SingleStepTests (MAME) fixture compatibility behavior.
    #[inline]
    pub fn set_sst_m68000_compat(&mut self, on: bool) {
        self.sst_m68000_compat = on;
    }

    /// Set CPU type and configure appropriate masks/timing.
    pub fn set_cpu_type(&mut self, cpu_type: CpuType) {
        self.cpu_type = cpu_type;
        self.is_pre_68020 = matches!(
            cpu_type,
            CpuType::M68000 | CpuType::M68010 | CpuType::SCC68070
        );
        match cpu_type {
            CpuType::M68000 => {
                self.address_mask = 0x00FFFFFF;
                self.sr_mask = 0xA71F;
                self.has_pmmu = false;
            }
            CpuType::M68010 => {
                self.address_mask = 0x00FFFFFF;
                self.sr_mask = 0xA71F;
                self.has_pmmu = false;
            }
            CpuType::SCC68070 => {
                // SCC68070 is a 68010 with a 32-bit data bus.
                // Instruction set is 68010-compatible, but the address bus is 32-bit.
                self.address_mask = 0xFFFFFFFF;
                self.sr_mask = 0xA71F;
                self.has_pmmu = false;
            }
            CpuType::M68EC020 | CpuType::M68020 => {
                self.address_mask = 0xFFFFFFFF;
                self.sr_mask = 0xF71F;
                self.has_pmmu = false;
            }
            CpuType::M68EC030 => {
                self.address_mask = 0xFFFFFFFF;
                self.sr_mask = 0xF71F;
                self.has_pmmu = false;
            }
            CpuType::M68030 => {
                self.address_mask = 0xFFFFFFFF;
                self.sr_mask = 0xF71F;
                self.has_pmmu = true;
            }
            CpuType::M68EC040 | CpuType::M68LC040 => {
                self.address_mask = 0xFFFFFFFF;
                self.sr_mask = 0xF71F;
                self.has_pmmu = false;
            }
            CpuType::M68040 => {
                self.address_mask = 0xFFFFFFFF;
                self.sr_mask = 0xF71F;
                self.has_pmmu = true;
            }
            _ => {}
        }
    }

    // ========== Stack Pointer Banking ==========
    // Musashi formula: sp[s_flag | ((s_flag >> 1) & m_flag)]
    // s_flag = 0 (user) or 4 (supervisor)
    // m_flag = 0 (interrupt) or 2 (master)
    // Results: USP=0, ISP=4, MSP=6

    /// Get the current stack pointer bank index.
    #[inline]
    fn sp_index(&self) -> usize {
        (self.s_flag | ((self.s_flag >> 1) & self.m_flag)) as usize
    }

    /// Backup current SP to banked storage.
    fn backup_sp(&mut self) {
        let idx = self.sp_index();
        self.sp[idx] = self.dar[15];
    }

    /// Restore SP from banked storage.
    fn restore_sp(&mut self) {
        let idx = self.sp_index();
        self.dar[15] = self.sp[idx];
    }

    /// Set the S flag with stack pointer banking.
    /// Value must be 0 (user) or SFLAG_SET (supervisor).
    pub fn set_s_flag(&mut self, value: u32) {
        self.backup_sp();
        self.s_flag = value;
        self.restore_sp();
    }

    /// Set both S and M flags with stack pointer banking.
    /// Value: bit 2 = S, bit 1 = M (0, 2, 4, or 6).
    pub fn set_sm_flag(&mut self, value: u32) {
        self.backup_sp();
        self.s_flag = value & SFLAG_SET;
        self.m_flag = value & MFLAG_SET;
        self.restore_sp();
    }

    /// Set S and M flags without touching the stack pointer.
    pub fn set_sm_flag_nosp(&mut self, value: u32) {
        self.s_flag = value & SFLAG_SET;
        self.m_flag = value & MFLAG_SET;
    }

    // ========== Reset ==========

    /// Pulse reset (initialize CPU state without loading vectors).
    pub fn pulse_reset(&mut self) {
        self.stopped = 0;
        self.t1_flag = 0;
        self.t0_flag = 0;
        self.m_flag = 0;
        self.run_mode = 0;
        self.instr_mode = 0;
        self.vbr = 0;
        // Reset disables and clears the on-chip caches (68020+): CACR
        // enable/freeze bits drop and the host model must invalidate.
        self.cacr = 0;
        self.cacr_pending_ops |= CACR_CI | CACR_CD;
        self.prefetch_queue = [0; 2];
        self.prefetch_count = 0;
        self.consume_without_prefetch = false;
        self.pending_sync_clocks = 0;

        // Condition codes after reset: clear X/N/V/C, set Z (Musashi-compatible default).
        self.x_flag = 0;
        self.n_flag = 0;
        self.v_flag = 0;
        self.c_flag = 0;
        self.not_z_flag = 0; // Z set

        // Enter supervisor mode
        self.set_s_flag(SFLAG_SET);
        self.int_mask = 0x0700; // Mask all interrupts
    }

    // ========== Prefetch queue (Part E.1, 68000 only) ==========

    /// Whether the two-word prefetch queue models instruction fetching.
    /// Only the 68000 path is prefetch-modeled; other CPU types keep the
    /// direct fetch-at-PC behavior.
    #[inline]
    pub fn prefetch_enabled(&self) -> bool {
        self.cpu_type == CpuType::M68000
    }

    // ========== Precise per-access timing (Part E.2, 68000 only) ==========

    /// Record `clocks` CPU clocks of internal (non-bus) processing. They are
    /// reported to the host (via `AddressBus::sync`) immediately before the
    /// next bus access, so that access lands at its hardware-exact offset.
    #[inline]
    pub fn internal_cycles(&mut self, clocks: u32) {
        if self.prefetch_enabled() {
            self.pending_sync_clocks = self.pending_sync_clocks.wrapping_add(clocks);
        }
    }

    /// Flush accumulated internal clocks to the host right before a bus
    /// access. Every bus-access helper calls this first.
    #[inline]
    pub(crate) fn flush_sync<B: AddressBus>(&mut self, bus: &mut B) {
        if self.pending_sync_clocks > 0 {
            let clocks = std::mem::take(&mut self.pending_sync_clocks);
            bus.sync(clocks);
        }
    }

    /// Mark the prefetch queue as stale (after an externally forced PC
    /// change). The next instruction-stream read fetches directly and the
    /// next instruction-end top-up restores the queue.
    #[inline]
    pub fn invalidate_prefetch(&mut self) {
        self.prefetch_count = 0;
    }

    /// Read one program-space word for the prefetch queue. Returns None when
    /// the read faulted (bus error already triggered).
    fn prefetch_read<B: AddressBus>(&mut self, bus: &mut B, addr: u32) -> Option<u16> {
        // Part E.2: report internal clocks elapsed before this bus access.
        self.flush_sync(bus);
        let addr = self.address(addr);
        match bus.try_read_word(addr) {
            Ok(v) => Some(v),
            Err(_) => {
                self.trigger_bus_error(bus, addr, false, true);
                None
            }
        }
    }

    /// Refill the prefetch queue from the current PC: two program-space word
    /// reads at `pc` and `pc + 2`, discarding whatever was queued. This is
    /// what the 68000 does after every change of instruction flow (taken
    /// branches, jumps, returns, exception entry), which is why those
    /// instructions cost two extra bus reads at the target and why words
    /// prefetched past a taken branch never appear on the bus as re-reads.
    ///
    /// An odd PC leaves the queue empty; the address error fires at the next
    /// instruction-stream read (same point as the non-prefetch path).
    pub fn full_prefetch<B: AddressBus>(&mut self, bus: &mut B) {
        self.prefetch_first(bus);
        self.prefetch_second(bus);
    }

    /// First half of a flow-change prefetch: read the word at the new PC into
    /// the front of the queue. Some instructions (JSR/BSR) interleave other
    /// bus accesses between the two refill reads; they call this, do their
    /// accesses, then call `prefetch_second`.
    pub fn prefetch_first<B: AddressBus>(&mut self, bus: &mut B) {
        if !self.prefetch_enabled() {
            return;
        }
        self.prefetch_count = 0;
        if self.pc & 1 != 0 {
            return;
        }
        if let Some(w) = self.prefetch_read(bus, self.pc) {
            self.prefetch_queue[0] = w;
            self.prefetch_count = 1;
        }
    }

    /// Second half of a flow-change prefetch: read the word at PC + 2 into the
    /// back of the queue.
    pub fn prefetch_second<B: AddressBus>(&mut self, bus: &mut B) {
        if !self.prefetch_enabled() || self.prefetch_count != 1 {
            return;
        }
        if self.pc & 1 != 0 || self.run_mode == super::execute::RUN_MODE_BERR_AERR_RESET {
            return;
        }
        if let Some(w) = self.prefetch_read(bus, self.pc.wrapping_add(2)) {
            self.prefetch_queue[1] = w;
            self.prefetch_count = 2;
        }
    }

    /// Fetch one word into the back of the prefetch queue (the "np" prefetch
    /// micro-operation). The fetch address is the first instruction-stream
    /// word the queue does not yet hold.
    pub fn top_up_prefetch_one<B: AddressBus>(&mut self, bus: &mut B) {
        if !self.prefetch_enabled() || self.pc & 1 != 0 || self.prefetch_count >= 2 {
            return;
        }
        if self.run_mode == super::execute::RUN_MODE_BERR_AERR_RESET {
            return;
        }
        let slot = self.prefetch_count as usize;
        let addr = self.pc.wrapping_add(2 * slot as u32);
        if let Some(w) = self.prefetch_read(bus, addr) {
            self.prefetch_queue[slot] = w;
            self.prefetch_count += 1;
        }
    }

    /// Top the prefetch queue back up to two words at the end of an
    /// instruction -- the final prefetch the 68000 performs after its
    /// writes. A no-op after flow-change instructions (their refill
    /// already filled the queue), on a stopped CPU (STOP performs no
    /// further bus activity), and on non-prefetch CPU types.
    pub fn top_up_prefetch<B: AddressBus>(&mut self, bus: &mut B) {
        if !self.prefetch_enabled() || self.pc & 1 != 0 || self.stopped != 0 {
            return;
        }
        if self.run_mode == super::execute::RUN_MODE_BERR_AERR_RESET {
            return;
        }
        while self.prefetch_count < 2 {
            let before = self.prefetch_count;
            self.top_up_prefetch_one(bus);
            if self.prefetch_count == before {
                return;
            }
        }
    }

    /// 68000 -(An) long operand read: the two predecrement micro-steps read
    /// the LOW word first (at addr + 2), then the HIGH word (at addr).
    pub fn read_long_predec_68000<B: AddressBus>(&mut self, bus: &mut B, addr: u32) -> u32 {
        let lo = self.read_16(bus, addr.wrapping_add(2)) as u32;
        let hi = self.read_16(bus, addr) as u32;
        (hi << 16) | lo
    }

    /// 68000 long writeback for the -(Ax),-(Ay) extended-arithmetic forms
    /// (ADDX.L/SUBX.L memory-to-memory): write the low word, perform the
    /// final prefetch, then write the high word.
    pub fn write_long_mm_interleaved_68000<B: AddressBus>(
        &mut self,
        bus: &mut B,
        addr: u32,
        value: u32,
    ) {
        self.write_16(bus, addr.wrapping_add(2), (value & 0xFFFF) as u16);
        self.top_up_prefetch(bus);
        self.write_16(bus, addr, (value >> 16) as u16);
    }

    /// Consume the next instruction-stream word from the prefetch queue
    /// WITHOUT a prefetch refill. Used for the opcode itself (its refill was
    /// the previous instruction's final prefetch) and for words consumed on
    /// flow-change paths (the microcode goes straight to the jump refill
    /// instead of prefetching ahead of a discarded stream).
    ///
    /// Falls back to a direct fetch when the queue is empty.
    pub fn consume_imm_16_no_prefetch<B: AddressBus>(&mut self, bus: &mut B) -> u16 {
        if self.prefetch_count > 0 {
            let word = self.prefetch_queue[0];
            self.prefetch_queue[0] = self.prefetch_queue[1];
            self.prefetch_count -= 1;
            self.pc = self.pc.wrapping_add(2);
            return word;
        }
        match self.prefetch_read(bus, self.pc) {
            Some(w) => {
                self.pc = self.pc.wrapping_add(2);
                w
            }
            None => 0,
        }
    }

    /// Full reset: pulse reset + load SP and PC from vectors.
    pub fn reset<B: AddressBus>(&mut self, bus: &mut B) {
        self.pulse_reset();

        // Read initial SSP from vector 0
        let ssp = bus.read_long(0);
        self.dar[15] = ssp;
        self.sp[SFLAG_SET as usize] = ssp; // ISP bank
        // Initialize MSP too (for 68020+ MSP/ISP banking). Harmless on 68000.
        self.sp[(SFLAG_SET | MFLAG_SET) as usize] = ssp;

        // Read initial PC from vector 1
        self.pc = bus.read_long(4);

        // The queue holds nothing valid for the new PC; the first instruction
        // fetch refills it (matching the 68000's post-reset double prefetch).
        self.invalidate_prefetch();

        // Use reset cycles
        self.cycles_remaining -= self.cyc_reset;
    }

    /// Soft reset (compatible with old API - no bus access).
    pub fn reset_soft(&mut self) {
        self.pulse_reset();
    }

    // ========== Register Accessors ==========

    /// Get data register.
    #[inline]
    pub fn d(&self, reg: usize) -> u32 {
        self.dar[reg & 7]
    }

    /// Set data register.
    #[inline]
    pub fn set_d(&mut self, reg: usize, value: u32) {
        self.dar[reg & 7] = value;
    }

    /// Returns true if the CPU is stopped via STOP.
    #[inline]
    pub fn is_stopped(&self) -> bool {
        self.stopped != 0 && self.run_mode != RUN_MODE_BERR_AERR_RESET
    }

    /// Returns true if the CPU halted due to a double-fault/bus-error reset condition.
    #[inline]
    pub fn is_halted(&self) -> bool {
        self.stopped != 0 && self.run_mode == RUN_MODE_BERR_AERR_RESET
    }

    /// Get address register.
    #[inline]
    pub fn a(&self, reg: usize) -> u32 {
        self.dar[8 + (reg & 7)]
    }

    /// Set address register.
    #[inline]
    pub fn set_a(&mut self, reg: usize, value: u32) {
        self.dar[8 + (reg & 7)] = value;
    }

    /// Get stack pointer (A7).
    #[inline]
    pub fn sp(&self) -> u32 {
        self.dar[15]
    }

    /// Set stack pointer (A7).
    #[inline]
    pub fn set_sp(&mut self, value: u32) {
        self.dar[15] = value;
    }

    /// Get User Stack Pointer.
    pub fn get_usp(&self) -> u32 {
        if self.s_flag == 0 {
            self.dar[15]
        } else {
            self.sp[0]
        }
    }

    /// Set User Stack Pointer.
    pub fn set_usp(&mut self, value: u32) {
        if self.s_flag == 0 {
            self.dar[15] = value;
        } else {
            self.sp[0] = value;
        }
    }

    // ========== Control Register Access (MOVEC) ==========

    /// Read control register for MOVEC instruction.
    /// Control register codes:
    /// 0x000 = SFC, 0x001 = DFC, 0x002 = CACR, 0x003 = TC (68040)
    /// 0x004-0x007 = ITT0/ITT1/DTT0/DTT1, 0x008-0x00B = DACR0/DACR1/IACR0/IACR1
    /// 0x800 = USP, 0x801 = VBR, 0x802 = CAAR, 0x803 = MSP, 0x804 = ISP
    /// 0x805 = MMUSR, 0x806 = URP, 0x807 = SRP
    pub fn read_control_register(&self, reg: u16) -> u32 {
        match reg {
            0x000 => self.sfc,   // Source Function Code
            0x001 => self.dfc,   // Destination Function Code
            0x002 => self.cacr,  // Cache Control Register
            0x003 => self.mmu_tc, // Translation Control (68040)
            0x004 => self.itt0,  // Instruction TTR 0 (68040)
            0x005 => self.itt1,  // Instruction TTR 1 (68040)
            0x006 => self.dtt0,  // Data TTR 0 (68040)
            0x007 => self.dtt1,  // Data TTR 1 (68040)
            0x008 => self.dacr0, // Data Access Control 0 (68040)
            0x009 => self.dacr1, // Data Access Control 1 (68040)
            0x00A => self.iacr0, // Instruction Access Control 0 (68040)
            0x00B => self.iacr1, // Instruction Access Control 1 (68040)
            0x800 => {
                // USP
                if self.s_flag == 0 {
                    self.dar[15]
                } else {
                    self.sp[0]
                }
            }
            0x801 => self.vbr,  // Vector Base Register
            0x802 => self.caar, // Cache Address Register
            0x803 => {
                // MSP (Master Stack Pointer)
                if self.s_flag != 0 && self.m_flag != 0 {
                    self.dar[15]
                } else {
                    self.sp[6]
                }
            }
            0x804 => {
                // ISP (Interrupt Stack Pointer)
                if self.s_flag != 0 && self.m_flag == 0 {
                    self.dar[15]
                } else {
                    self.sp[4]
                }
            }
            0x805 => self.mmu_sr,        // MMU Status Register (68040)
            0x806 => self.mmu_crp_aptr,  // User Root Pointer (68040; URP)
            0x807 => self.mmu_srp_aptr,  // Supervisor Root Pointer (68040)
            _ => 0,                      // Unknown register
        }
    }

    /// Write control register for MOVEC instruction.
    pub fn write_control_register(&mut self, reg: u16, value: u32) {
        match reg {
            0x000 => self.sfc = value & 7, // SFC (3 bits)
            0x001 => self.dfc = value & 7, // DFC (3 bits)
            0x002 => {
                // CACR. The clear bits (CEI/CI, and CED/CD on the 68030)
                // are write-only strobes: they trigger an invalidation,
                // never store, and read back as zero. Persistent bits are
                // masked to what the CPU type implements.
                use crate::core::types::CpuType;
                let (persist, strobes) = match self.cpu_type {
                    CpuType::M68EC020 | CpuType::M68020 => (CACR_EI | CACR_FI, CACR_CEI | CACR_CI),
                    CpuType::M68EC030 | CpuType::M68030 => (
                        CACR_EI | CACR_FI | CACR_IBE | CACR_ED | CACR_FD | CACR_DBE | CACR_WA,
                        CACR_CEI | CACR_CI | CACR_CED | CACR_CD,
                    ),
                    // 68040 CACR defines only the two cache-enable bits and
                    // has no clear strobes - invalidation is done with the
                    // CINV/CPUSH instructions (see decode.rs). Reserved bits
                    // read back as zero.
                    _ => (CACR_040_IE | CACR_040_DE, 0),
                };
                self.cacr = value & persist;
                self.cacr_pending_ops |= value & strobes;
            }
            0x003 => {
                // Translation Control (68040). MOVEC must update pmmu_enabled
                // (the 040 enable bit is TC[15], unlike the 030's TC[31]).
                self.mmu_tc = value;
                self.pmmu_enabled = self.tc_enable();
            }
            0x004 => self.itt0 = value,    // Instruction TTR 0 (68040)
            0x005 => self.itt1 = value,    // Instruction TTR 1 (68040)
            0x006 => self.dtt0 = value,    // Data TTR 0 (68040)
            0x007 => self.dtt1 = value,    // Data TTR 1 (68040)
            0x008 => self.dacr0 = value,   // Data Access Control 0 (68040)
            0x009 => self.dacr1 = value,   // Data Access Control 1 (68040)
            0x00A => self.iacr0 = value,   // Instruction Access Control 0 (68040)
            0x00B => self.iacr1 = value,   // Instruction Access Control 1 (68040)
            0x800 => {
                // USP
                if self.s_flag == 0 {
                    self.dar[15] = value;
                } else {
                    self.sp[0] = value;
                }
            }
            0x801 => self.vbr = value,  // VBR
            0x802 => self.caar = value, // CAAR
            0x803 => {
                // MSP
                if self.s_flag != 0 && self.m_flag != 0 {
                    self.dar[15] = value;
                } else {
                    self.sp[6] = value;
                }
            }
            0x804 => {
                // ISP
                if self.s_flag != 0 && self.m_flag == 0 {
                    self.dar[15] = value;
                } else {
                    self.sp[4] = value;
                }
            }
            0x805 => self.mmu_sr = value,        // MMU Status Register (68040)
            0x806 => self.mmu_crp_aptr = value,  // User Root Pointer (68040; URP)
            0x807 => self.mmu_srp_aptr = value,  // Supervisor Root Pointer (68040)
            _ => {}                              // Unknown register - ignore
        }
    }

    /// True for any 68040-family part (full / LC / EC). The 040 MMU differs
    /// from the 030 in register layout, TC enable bit, and table format.
    #[inline]
    pub fn is_040(&self) -> bool {
        matches!(
            self.cpu_type,
            CpuType::M68EC040 | CpuType::M68LC040 | CpuType::M68040
        )
    }

    /// Whether TC's translation-enable bit is set. The bit position differs by
    /// part: the 68040 uses TC[15], the 68030 uses TC[31].
    #[inline]
    pub fn tc_enable(&self) -> bool {
        if self.is_040() {
            self.mmu_tc & 0x0000_8000 != 0
        } else {
            self.mmu_tc & 0x8000_0000 != 0
        }
    }

    // ========== Memory Access Helpers ==========

    /// Mask address according to CPU type.
    #[inline]
    pub fn address(&self, addr: u32) -> u32 {
        addr & self.address_mask
    }

    #[inline]
    fn faulted(&self) -> bool {
        self.run_mode == RUN_MODE_BERR_AERR_RESET
    }

    /// Trigger a 68000-style address error and mark the current instruction as faulted so that
    /// subsequent EA resolution/memory operations become no-ops.
    pub(crate) fn trigger_address_error<B: AddressBus>(
        &mut self,
        bus: &mut B,
        address: u32,
        write: bool,
        instruction: bool,
    ) {
        if self.faulted() {
            return;
        }
        if std::env::var_os("M68K_DIAG_ADDRESS_ERROR").is_some() {
            eprintln!(
                "m68k address error: addr={address:#010X} write={write} instr={instruction} \
                 pc={:#010X} ppc={:#010X} sp={:#010X} sr={:#06X}",
                self.pc,
                self.ppc,
                self.sp(),
                self.get_sr(),
            );
            let sp = self.sp();
            let mut words = Vec::new();
            for i in -8i32..8 {
                let a = sp.wrapping_add((i * 2) as u32);
                words.push(format!("{:04X}", bus.read_word(a)));
            }
            eprintln!(
                "m68k stack around sp ({:#010X}-16..+16): {}",
                sp,
                words.join(" ")
            );
            eprintln!(
                "m68k regs d0-d7={:08X?} a0-a7={:08X?} vbr={:#010X}",
                &self.dar[0..8],
                &self.dar[8..16],
                self.vbr
            );
        }

        // Roll back any partially-applied register side effects from the faulting instruction.
        // The execute loop saved a snapshot at the start of the instruction.
        self.set_sr_noint_nosp(self.sr_save);
        self.dar = self.dar_save;
        let _ = self.exception_address_error(bus, address, write, instruction);
        self.run_mode = RUN_MODE_BERR_AERR_RESET;
    }

    /// Trigger a bus error and mark the current instruction as faulted so that subsequent EA
    /// resolution/memory operations become no-ops.
    pub(crate) fn trigger_bus_error<B: AddressBus>(
        &mut self,
        bus: &mut B,
        address: u32,
        write: bool,
        instruction: bool,
    ) {
        if self.faulted() {
            return;
        }

        // Roll back any partially-applied register side effects from the faulting instruction.
        self.set_sr_noint_nosp(self.sr_save);
        self.dar = self.dar_save;
        let _ = self.exception_bus_error(bus, address, write, instruction);
        self.run_mode = RUN_MODE_BERR_AERR_RESET;
    }

    /// Read byte from memory (data space).
    #[inline]
    pub fn read_8<B: AddressBus>(&mut self, bus: &mut B, addr: u32) -> u8 {
        if self.faulted() {
            return 0;
        }
        // Part E.2: report internal clocks elapsed before this bus access.
        self.flush_sync(bus);
        let mut addr = self.address(addr);
        {
            if self.has_pmmu && self.pmmu_enabled {
                match crate::mmu::translate_address(
                    self,
                    bus,
                    addr,
                    /*write=*/ false,
                    self.is_supervisor(),
                    /*instruction=*/ false,
                ) {
                    Ok(p) => addr = self.address(p),
                    Err(f) => {
                        self.handle_mmu_fault(
                            bus, f, /*write=*/ false, /*instruction=*/ false,
                        );
                        return 0;
                    }
                }
            }
        }
        match bus.try_read_byte(addr) {
            Ok(v) => v,
            Err(f) => {
                if matches!(f.kind, BusFaultKind::BusError) {
                    self.trigger_bus_error(bus, addr, false, false);
                }
                0
            }
        }
    }

    /// Read word from memory (data space).
    #[inline]
    pub fn read_16<B: AddressBus>(&mut self, bus: &mut B, addr: u32) -> u16 {
        if self.faulted() {
            return 0;
        }
        // Part E.2: report internal clocks elapsed before this bus access.
        self.flush_sync(bus);
        let mut addr = self.address(addr);
        if matches!(
            self.cpu_type,
            CpuType::M68000 | CpuType::M68010 | CpuType::SCC68070
        ) && (addr & 1) != 0
        {
            self.trigger_address_error(bus, addr, false, false);
            return 0;
        }
        {
            if self.has_pmmu && self.pmmu_enabled {
                match crate::mmu::translate_address(
                    self,
                    bus,
                    addr,
                    /*write=*/ false,
                    self.is_supervisor(),
                    /*instruction=*/ false,
                ) {
                    Ok(p) => addr = self.address(p),
                    Err(f) => {
                        self.handle_mmu_fault(
                            bus, f, /*write=*/ false, /*instruction=*/ false,
                        );
                        return 0;
                    }
                }
            }
        }
        match bus.try_read_word(addr) {
            Ok(v) => v,
            Err(f) => {
                if matches!(f.kind, BusFaultKind::BusError) {
                    self.trigger_bus_error(bus, addr, false, false);
                }
                0
            }
        }
    }

    /// Read long from memory (data space).
    #[inline]
    pub fn read_32<B: AddressBus>(&mut self, bus: &mut B, addr: u32) -> u32 {
        if self.faulted() {
            return 0;
        }
        // Part E.2: report internal clocks elapsed before this bus access.
        self.flush_sync(bus);
        let mut addr = self.address(addr);
        if matches!(
            self.cpu_type,
            CpuType::M68000 | CpuType::M68010 | CpuType::SCC68070
        ) && (addr & 1) != 0
        {
            self.trigger_address_error(bus, addr, false, false);
            return 0;
        }
        {
            if self.has_pmmu && self.pmmu_enabled {
                match crate::mmu::translate_address(
                    self,
                    bus,
                    addr,
                    /*write=*/ false,
                    self.is_supervisor(),
                    /*instruction=*/ false,
                ) {
                    Ok(p) => addr = self.address(p),
                    Err(f) => {
                        self.handle_mmu_fault(
                            bus, f, /*write=*/ false, /*instruction=*/ false,
                        );
                        return 0;
                    }
                }
            }
        }
        match bus.try_read_long(addr) {
            Ok(v) => v,
            Err(f) => {
                if matches!(f.kind, BusFaultKind::BusError) {
                    self.trigger_bus_error(bus, addr, false, false);
                }
                0
            }
        }
    }

    /// Write byte to memory (data space).
    #[inline]
    pub fn write_8<B: AddressBus>(&mut self, bus: &mut B, addr: u32, value: u8) {
        if self.faulted() {
            return;
        }
        // Part E.2: report internal clocks elapsed before this bus access.
        self.flush_sync(bus);
        let mut addr = self.address(addr);
        {
            if self.has_pmmu && self.pmmu_enabled {
                match crate::mmu::translate_address(
                    self,
                    bus,
                    addr,
                    /*write=*/ true,
                    self.is_supervisor(),
                    /*instruction=*/ false,
                ) {
                    Ok(p) => addr = self.address(p),
                    Err(f) => {
                        self.handle_mmu_fault(
                            bus, f, /*write=*/ true, /*instruction=*/ false,
                        );
                        return;
                    }
                }
            }
        }
        if let Err(f) = bus.try_write_byte(addr, value)
            && matches!(f.kind, BusFaultKind::BusError)
        {
            self.trigger_bus_error(bus, addr, true, false);
        }
    }

    /// Write word to memory (data space).
    #[inline]
    pub fn write_16<B: AddressBus>(&mut self, bus: &mut B, addr: u32, value: u16) {
        if self.faulted() {
            return;
        }
        // Part E.2: report internal clocks elapsed before this bus access.
        self.flush_sync(bus);
        let mut addr = self.address(addr);
        if matches!(
            self.cpu_type,
            CpuType::M68000 | CpuType::M68010 | CpuType::SCC68070
        ) && (addr & 1) != 0
        {
            self.trigger_address_error(bus, addr, true, false);
            return;
        }
        {
            if self.has_pmmu && self.pmmu_enabled {
                match crate::mmu::translate_address(
                    self,
                    bus,
                    addr,
                    /*write=*/ true,
                    self.is_supervisor(),
                    /*instruction=*/ false,
                ) {
                    Ok(p) => addr = self.address(p),
                    Err(f) => {
                        self.handle_mmu_fault(
                            bus, f, /*write=*/ true, /*instruction=*/ false,
                        );
                        return;
                    }
                }
            }
        }
        if let Err(f) = bus.try_write_word(addr, value)
            && matches!(f.kind, BusFaultKind::BusError)
        {
            self.trigger_bus_error(bus, addr, true, false);
        }
    }

    /// Write long to memory (data space).
    #[inline]
    pub fn write_32<B: AddressBus>(&mut self, bus: &mut B, addr: u32, value: u32) {
        if self.faulted() {
            return;
        }
        // Part E.2: report internal clocks elapsed before this bus access.
        self.flush_sync(bus);
        let mut addr = self.address(addr);
        if matches!(
            self.cpu_type,
            CpuType::M68000 | CpuType::M68010 | CpuType::SCC68070
        ) && (addr & 1) != 0
        {
            self.trigger_address_error(bus, addr, true, false);
            return;
        }
        {
            if self.has_pmmu && self.pmmu_enabled {
                match crate::mmu::translate_address(
                    self,
                    bus,
                    addr,
                    /*write=*/ true,
                    self.is_supervisor(),
                    /*instruction=*/ false,
                ) {
                    Ok(p) => addr = self.address(p),
                    Err(f) => {
                        self.handle_mmu_fault(
                            bus, f, /*write=*/ true, /*instruction=*/ false,
                        );
                        return;
                    }
                }
            }
        }
        if let Err(f) = bus.try_write_long(addr, value)
            && matches!(f.kind, BusFaultKind::BusError)
        {
            self.trigger_bus_error(bus, addr, true, false);
        }
    }

    pub(crate) fn handle_mmu_fault<B: AddressBus>(
        &mut self,
        bus: &mut B,
        fault: crate::mmu::MmuFault,
        write: bool,
        instruction: bool,
    ) {
        use crate::core::exceptions::vector;
        use crate::mmu::MmuFaultKind;

        // Note: Infinite recursion prevention is handled by:
        // 1. exception_processing flag in translate() bypasses MMU during exception handling
        // 2. Double-fault detection in take_exception() halts CPU on recursive faults

        match fault.kind {
            MmuFaultKind::BusError => {
                self.trigger_bus_error(bus, fault.address, write, instruction)
            }
            MmuFaultKind::ConfigurationError => {
                let _ = self.take_exception(bus, vector::MMU_CONFIGURATION_ERROR);
                self.run_mode = RUN_MODE_BERR_AERR_RESET;
            }
            MmuFaultKind::IllegalOperation => {
                let _ = self.take_exception(bus, vector::MMU_ILLEGAL_OPERATION_ERROR);
                self.run_mode = RUN_MODE_BERR_AERR_RESET;
            }
            MmuFaultKind::AccessLevelViolation => {
                let _ = self.take_exception(bus, vector::MMU_ACCESS_LEVEL_VIOLATION_ERROR);
                self.run_mode = RUN_MODE_BERR_AERR_RESET;
            }
        }
    }

    /// Execute COP0 / PMMU op0 (0xF0xx) style instructions.
    ///
    /// Currently supports only PMOVE to/from a subset of PMMU registers:
    /// - TC (32-bit)
    /// - SRP (64-bit) (limit:aptr)
    /// - CRP (64-bit) (limit:aptr)
    ///
    /// Returns 0 if not recognized/supported (caller should treat as LINE 1111).
    pub fn exec_mmu_op0<B: AddressBus>(&mut self, bus: &mut B, opcode: u16) -> i32 {
        use super::ea::AddressingMode;
        use super::types::Size;

        // MMU ops require PMMU-capable CPU (68030/68040).
        if !self.has_pmmu {
            return 0;
        }
        if !self.is_supervisor() {
            return self.exception_privilege(bus);
        }

        // Extension word immediately after opcode.
        let modes = self.read_imm_16(bus);

        // Only handle PMOVE-family encodings for now.
        // Reject known-but-unimplemented ops (PLOAD/PFLUSH/PTEST/etc).
        // However, on 68040 treat PTEST as NOP since we don't have real MMU.
        let is_ptest = (modes & 0xE000) == 0x8000;
        let is_040 = matches!(
            self.cpu_type,
            super::types::CpuType::M68EC040
                | super::types::CpuType::M68LC040
                | super::types::CpuType::M68040
        );
        if is_ptest && is_040 {
            // PTEST on 68040 - treat as NOP
            return 4;
        }
        // PLOAD / PFLUSH / PFLUSHA / PTEST (68030 forms): recognized but not yet
        // fully modelled. Treat them as NOPs rather than returning 0, which the
        // decoder would turn into a LINE-1111 trap -- AROS and the 68040.library
        // issue these during MMU setup, so trapping crashes the boot. PTEST
        // reports "no fault" via a benign MMUSR; an ATC to flush (PFLUSH/PLOAD)
        // and precise MMUSR bits (PTEST) come with later phases.
        if (modes & 0xFDE0) == 0x2000   // PLOAD
            || (modes & 0xE200) == 0x2000
            || modes == 0xA000          // PFLUSHA
            || modes == 0x2800
            || (modes & 0xFFF8) == 0x2C00 // PFLUSH
            || is_ptest
        {
            if is_ptest {
                self.mmu_sr = 0;
            }
            return 8;
        }

        // Decode effective address from opcode.
        let ea_mode = ((opcode >> 3) & 0x7) as u8;
        let ea_reg = (opcode & 0x7) as u8;
        let Some(am) = AddressingMode::decode(ea_mode, ea_reg) else {
            return 0;
        };

        // Determine whether this is PMOVE <reg> -> <ea> or <ea> -> <reg>.
        // Musashi uses bit 9 (0x0200): if set, it writes EA from MMU reg.
        let to_ea = (modes & 0x0200) != 0;
        let preg = ((modes >> 10) & 0x1F) as u8;

        // Helper: resolve EA and require memory for 64-bit transfers.
        let ea = self.resolve_ea(bus, am, Size::Long);

        fn ea_addr_only(ea: super::ea::EaResult) -> Option<u32> {
            match ea {
                super::ea::EaResult::Memory(a) => Some(a),
                _ => None,
            }
        }

        // 68030 PMOVE preg encoding (5-bit):
        //   0x10 = TC    (32-bit)
        //   0x12 = SRP   (64-bit)
        //   0x13 = CRP   (64-bit)
        //   0x02 = TT0   (32-bit)
        //   0x03 = TT1   (32-bit)
        if to_ea {
            match preg {
                0x10 => {
                    // TC (32)
                    self.write_resolved_ea(bus, ea, Size::Long, self.mmu_tc);
                    4
                }
                0x12 => {
                    // SRP (64): [limit, aptr]
                    let Some(a) = ea_addr_only(ea) else { return 0 };
                    self.write_32(bus, a, self.mmu_srp_limit);
                    self.write_32(bus, a.wrapping_add(4), self.mmu_srp_aptr);
                    8
                }
                0x13 => {
                    // CRP (64)
                    let Some(a) = ea_addr_only(ea) else { return 0 };
                    self.write_32(bus, a, self.mmu_crp_limit);
                    self.write_32(bus, a.wrapping_add(4), self.mmu_crp_aptr);
                    8
                }
                0x02 => {
                    // TT0 (32)
                    self.write_resolved_ea(bus, ea, Size::Long, self.mmu_tt0);
                    4
                }
                0x03 => {
                    // TT1 (32)
                    self.write_resolved_ea(bus, ea, Size::Long, self.mmu_tt1);
                    4
                }
                _ => 0,
            }
        } else {
            match preg {
                0x10 => {
                    // TC (32)
                    let v = self.read_resolved_ea(bus, ea, Size::Long);
                    self.mmu_tc = v;
                    // PMOVE is 68030-only, so tc_enable() reads TC[31] here.
                    self.pmmu_enabled = self.tc_enable();
                    4
                }
                0x12 => {
                    // SRP (64)
                    let Some(a) = ea_addr_only(ea) else { return 0 };
                    let limit = self.read_32(bus, a);
                    let aptr = self.read_32(bus, a.wrapping_add(4));
                    self.mmu_srp_limit = limit;
                    self.mmu_srp_aptr = aptr;
                    8
                }
                0x13 => {
                    // CRP (64)
                    let Some(a) = ea_addr_only(ea) else { return 0 };
                    let limit = self.read_32(bus, a);
                    let aptr = self.read_32(bus, a.wrapping_add(4));
                    self.mmu_crp_limit = limit;
                    self.mmu_crp_aptr = aptr;
                    8
                }
                0x02 => {
                    // TT0 (32)
                    let v = self.read_resolved_ea(bus, ea, Size::Long);
                    self.mmu_tt0 = v;
                    4
                }
                0x03 => {
                    // TT1 (32)
                    let v = self.read_resolved_ea(bus, ea, Size::Long);
                    self.mmu_tt1 = v;
                    4
                }
                _ => 0,
            }
        }
    }

    // ========== SR/CCR Access ==========

    /// Get Status Register (composed from flags).
    pub fn get_sr(&self) -> u16 {
        let mut sr = 0u16;
        sr |= (self.t1_flag & 0x8000) as u16;
        sr |= (self.t0_flag & 0x4000) as u16;
        sr |= ((self.s_flag & SFLAG_SET) << 11) as u16;
        sr |= ((self.m_flag & MFLAG_SET) << 11) as u16;
        sr |= (self.int_mask & 0x0700) as u16;
        sr |= ((self.x_flag & XFLAG_SET) >> 4) as u16;
        sr |= ((self.n_flag & NFLAG_SET) >> 4) as u16;
        sr |= if self.not_z_flag == 0 { 0x04 } else { 0x00 };
        sr |= ((self.v_flag & VFLAG_SET) >> 6) as u16;
        sr |= ((self.c_flag & CFLAG_SET) >> 8) as u16;
        sr
    }

    /// Set Status Register (decomposes to flags) with stack banking.
    pub fn set_sr(&mut self, sr: u16) {
        let sr = sr & self.sr_mask as u16;
        self.t1_flag = (sr as u32) & 0x8000;
        self.t0_flag = (sr as u32) & 0x4000;
        self.int_mask = (sr as u32) & 0x0700;
        self.set_ccr_internal(sr as u8);
        // Set S and M with banking (M must be 0 when S=0)
        let mut sm = ((sr >> 11) & 6) as u32;
        if (sm & SFLAG_SET) == 0 {
            sm &= !MFLAG_SET;
        }
        self.set_sm_flag(sm);
    }

    /// Set SR without interrupt check or stack pointer change.
    pub fn set_sr_noint_nosp(&mut self, sr: u16) {
        let sr = sr & self.sr_mask as u16;
        self.t1_flag = (sr as u32) & 0x8000;
        self.t0_flag = (sr as u32) & 0x4000;
        self.int_mask = (sr as u32) & 0x0700;
        self.set_ccr_internal(sr as u8);
        let mut sm = ((sr >> 11) & 6) as u32;
        if (sm & SFLAG_SET) == 0 {
            sm &= !MFLAG_SET;
        }
        self.set_sm_flag_nosp(sm);
    }

    /// Internal CCR setter.
    fn set_ccr_internal(&mut self, ccr: u8) {
        self.x_flag = if ccr & 0x10 != 0 { XFLAG_SET } else { 0 };
        self.n_flag = if ccr & 0x08 != 0 { NFLAG_SET } else { 0 };
        self.not_z_flag = if ccr & 0x04 != 0 { 0 } else { 1 };
        self.v_flag = if ccr & 0x02 != 0 { VFLAG_SET } else { 0 };
        self.c_flag = if ccr & 0x01 != 0 { CFLAG_SET } else { 0 };
    }

    /// Get Condition Code Register (low byte of SR).
    pub fn get_ccr(&self) -> u8 {
        (self.get_sr() & 0xFF) as u8
    }

    /// Set Condition Code Register.
    pub fn set_ccr(&mut self, ccr: u8) {
        self.set_ccr_internal(ccr);
    }

    // ========== Flag Helpers ==========

    #[inline]
    pub fn flag_x(&self) -> bool {
        self.x_flag != 0
    }
    #[inline]
    pub fn flag_n(&self) -> bool {
        self.n_flag != 0
    }
    #[inline]
    pub fn flag_z(&self) -> bool {
        self.not_z_flag == 0
    }
    #[inline]
    pub fn flag_v(&self) -> bool {
        self.v_flag != 0
    }
    #[inline]
    pub fn flag_c(&self) -> bool {
        self.c_flag != 0
    }
    #[inline]
    pub fn is_supervisor(&self) -> bool {
        self.s_flag != 0
    }

    // ========== Condition Tests ==========

    /// Evaluate condition code.
    pub fn test_condition(&self, cond: u8) -> bool {
        match cond & 0x0F {
            0x0 => true,                                               // T
            0x1 => false,                                              // F
            0x2 => !self.flag_c() && !self.flag_z(),                   // HI
            0x3 => self.flag_c() || self.flag_z(),                     // LS
            0x4 => !self.flag_c(),                                     // CC/HS
            0x5 => self.flag_c(),                                      // CS/LO
            0x6 => !self.flag_z(),                                     // NE
            0x7 => self.flag_z(),                                      // EQ
            0x8 => !self.flag_v(),                                     // VC
            0x9 => self.flag_v(),                                      // VS
            0xA => !self.flag_n(),                                     // PL
            0xB => self.flag_n(),                                      // MI
            0xC => self.flag_n() == self.flag_v(),                     // GE
            0xD => self.flag_n() != self.flag_v(),                     // LT
            0xE => !self.flag_z() && (self.flag_n() == self.flag_v()), // GT
            0xF => self.flag_z() || (self.flag_n() != self.flag_v()),  // LE
            _ => true,
        }
    }
}
