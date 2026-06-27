//! FPU operations (68040/68881-class).
//!
//! Note: This is currently a **minimal bring-up** focused on plumbing + a few
//! OS-critical operations. Expect expansion over time.

use crate::core::cpu::CpuCore;
use crate::core::ea::{AddressingMode, EaResult};
use crate::core::types::Size;
use crate::core::memory::AddressBus;
use crate::fpu::FloatX80;
use super::softfloat::{self, ExcFlags, FpCmp, Precision, RoundCtx, RoundMode};

/// Where a resolved FPU operand lives.
enum FpuEa {
    /// Data register (formats of 4 bytes or fewer only).
    DataReg(usize),
    /// Memory address.
    Memory(u32),
    /// Immediate data, to be consumed from the instruction stream.
    Immediate,
}

/// Round an f64 to an integer using the FPCR rounding mode and saturate
/// into the destination integer width, as the 6888x does for FMOVE to an
/// integer format (out-of-range and NaN produce the most negative value
/// and would set OPERR, which the emulated FPU does not raise).
fn f64_to_int_saturating(value: f64, fpcr: u32, min: i64, max: i64) -> i64 {
    if value.is_nan() {
        return min;
    }
    // FPCR rounding mode, bits 4-5: 0=nearest, 1=zero, 2=minus, 3=plus.
    let rounded = match (fpcr >> 4) & 3 {
        1 => value.trunc(),
        2 => value.floor(),
        3 => value.ceil(),
        _ => {
            // Round to nearest, ties to even.
            let r = value.round();
            if (value - value.trunc()).abs() == 0.5 && r % 2.0 != 0.0 {
                r - value.signum()
            } else {
                r
            }
        }
    };
    if rounded < min as f64 {
        min
    } else if rounded > max as f64 {
        max
    } else {
        rounded as i64
    }
}

/// Load a constant from the 6888x on-chip ROM by `offset` (the ROM-index
/// field of an FMOVECR instruction). Shared by both FMOVECR encodings.
fn fpu_const_rom(offset: usize) -> f64 {
    match offset {
        0x00 => std::f64::consts::PI,      // Pi
        0x0B => std::f64::consts::LOG10_2, // log10(2)
        0x0C => std::f64::consts::E,       // e
        0x0D => std::f64::consts::LN_2,    // log_e(2) = ln(2)
        0x0E => std::f64::consts::LN_10,   // log_e(10) = ln(10)
        0x0F => 0.0,                       // Zero
        0x30 => std::f64::consts::LN_2,    // ln(2)
        0x31 => std::f64::consts::LN_10,   // ln(10)
        0x32 => 1.0,                       // 1.0
        0x33 => 10.0,                      // 10.0
        0x34 => 100.0,                     // 10^2
        0x35 => 1.0e4,                     // 10^4
        0x36 => 1.0e8,                     // 10^8
        0x37 => 1.0e16,                    // 10^16
        0x38 => 1.0e32,                    // 10^32
        0x39 => 1.0e64,                    // 10^64
        0x3A => 1.0e128,                   // 10^128
        0x3B => 1.0e256,                   // 10^256
        // Higher powers would overflow, return infinity
        0x3C..=0x3F => f64::INFINITY,
        _ => 0.0, // Unknown constant, return 0
    }
}

impl CpuCore {
    /// 68040 FPU "op0" entrypoint (opcode pattern 0xF2xx in Musashi: `040fpu0`).
    ///
    /// Handles the implemented 6888x/68040 ALU, FMOVE, control, and condition-code subset.
    /// Unsupported encodings return 0 so the caller can raise the Line-F exception.
    pub fn exec_fpu_op0<B: AddressBus>(&mut self, bus: &mut B, opcode: u16) -> i32 {
        use crate::core::types::CpuType;

        // LC040 and EC040 don't have integrated FPUs - must trap as Line-F
        if matches!(self.cpu_type, CpuType::M68LC040 | CpuType::M68EC040) {
            return 0;
        }

        // IMPORTANT:
        // - PC currently points at the first extension word (w2).
        // - We must NOT consume w2 (or any EA extension) unless we handle the instruction.

        let w2 = self.read_16(bus, self.pc);
        let subop = (w2 >> 13) & 0x7;

        // Executing any general FPU instruction takes the 6888x out of
        // its reset NULL state (FSAVE then produces a real frame).
        self.fpu_just_reset = false;

        match subop {
            0x2 => {
                // FPU ALU <ea>, FPn -- the memory/immediate-source form of
                // the full opmode set (FMOVE, the monadic ops, the
                // transcendentals, and the dyadic arithmetic). Shares the
                // fpu_apply_op dispatch table with the register-source path
                // below so the two cannot drift apart.
                let src_fmt = (w2 >> 10) & 0x7;
                let dst = ((w2 >> 7) & 7) as usize;
                let opmode = w2 & 0x7f;

                // Consume w2 now that we're committed.
                let _w2 = self.read_imm_16(bus);

                if src_fmt == 7 {
                    // FMOVECR - load constant from ROM. The opmode field is
                    // the ROM index; there is no <ea> source operand.
                    self.fpr[dst] = FloatX80::from_f64(fpu_const_rom(opmode as usize));
                    self.fpu_set_cc(self.fpr[dst]);
                    return 4;
                }

                let Some(src) = self.fpu_read_source(bus, opcode, src_fmt) else {
                    return 0;
                };
                self.fpu_apply_op(opmode, dst, src)
            }
            0x3 => {
                // FMOVE FP, <ea> - move FP register to memory/integer register
                let dst_fmt = (w2 >> 10) & 0x7;
                let src = ((w2 >> 7) & 7) as usize;

                // Consume w2 now that we're committed.
                let _w2 = self.read_imm_16(bus);

                let ea = (opcode & 0x3f) as u8;
                let ea_mode = (ea >> 3) & 7;
                let ea_reg = (ea & 7) as usize;

                if self.fpu_write_dest(bus, ea_mode, ea_reg, dst_fmt, self.fpr[src]) {
                    4
                } else {
                    0
                }
            }
            0x0 => {
                // FP register-to-register operations (FMOVE FPm,FPn, FADD, FSUB, FMUL, FDIV, FCMP, etc.)
                let src = ((w2 >> 10) & 7) as usize;
                let dst = ((w2 >> 7) & 7) as usize;
                let opmode = w2 & 0x7f;

                // Consume w2
                let _ = self.read_imm_16(bus);

                if opmode == 0x17 {
                    // FMOVECR - load constant from ROM. In this register-form
                    // encoding the source-register field carries the ROM index.
                    self.fpr[dst] = FloatX80::from_f64(fpu_const_rom(src));
                    self.fpu_set_cc(self.fpr[dst]);
                    return 4;
                }
                self.fpu_apply_op(opmode, dst, self.fpr[src])
            }
            0x6 | 0x7 => {
                // FMOVEM - move multiple FP registers to/from memory
                // subop 0x6: memory to FP registers (restore)
                // subop 0x7: FP registers to memory (save)
                let direction = subop;
                let reg_list = w2 & 0xFF;
                let mode_bits = (w2 >> 11) & 0x3;

                // Consume w2
                let _w2 = self.read_imm_16(bus);

                let ea = (opcode & 0x3f) as u8;
                let ea_mode = (ea >> 3) & 7;
                let ea_reg = (ea & 7) as usize;

                // MODE field (w2 bits 12-11): bit 11 selects a dynamic
                // register list (named by w2 bits 4-6), bit 12 selects the
                // postincrement/control mask order.
                let reg_list = if mode_bits & 0x1 != 0 {
                    (self.d(((w2 >> 4) & 7) as usize) & 0xFF) as u16
                } else {
                    reg_list
                };
                let reg_count = reg_list.count_ones();

                // Resolve the base. Pre-decrement applies the whole block;
                // every other mode (including indexed) resolves normally
                // and post-increment advances afterwards.
                let mut addr = if ea_mode == 4 {
                    let a = self.a(ea_reg).wrapping_sub(reg_count * 12);
                    self.set_a(ea_reg, a);
                    a
                } else {
                    match self.fpu_ea(bus, ea_mode, ea_reg, 0) {
                        Some(FpuEa::Memory(a)) => a,
                        _ => return 0,
                    }
                };

                // Registers transfer in 96-bit extended format, FP0 at
                // the lowest address. Mask order: predecrement lists carry
                // FPn in bit n; postincrement/control lists carry FPn in
                // bit 7-n -- so a -(An) save and an (An)+ restore with the
                // assembler's natural masks are mirror images.
                if direction == 0x6 {
                    // Memory to FP registers
                    for i in 0..8 {
                        let bit = if mode_bits & 0x2 != 0 {
                            1 << (7 - i)
                        } else {
                            1 << i
                        };
                        if reg_list & bit != 0 {
                            let exp_word = (self.read_32(bus, addr) >> 16) as u16;
                            let hi = self.read_32(bus, addr.wrapping_add(4)) as u64;
                            let lo = self.read_32(bus, addr.wrapping_add(8)) as u64;
                            self.fpr[i] = FloatX80::from_extended(exp_word, (hi << 32) | lo);
                            addr = addr.wrapping_add(12);
                        }
                    }
                } else {
                    // FP registers to memory
                    for i in 0..8 {
                        let bit = if mode_bits & 0x2 != 0 {
                            1 << (7 - i)
                        } else {
                            1 << i
                        };
                        if reg_list & bit != 0 {
                            let (exp_word, mantissa) = self.fpr[i].to_extended();
                            self.write_32(bus, addr, (exp_word as u32) << 16);
                            self.write_32(bus, addr.wrapping_add(4), (mantissa >> 32) as u32);
                            self.write_32(bus, addr.wrapping_add(8), mantissa as u32);
                            addr = addr.wrapping_add(12);
                        }
                    }
                }

                // Handle post-increment
                if ea_mode == 3 {
                    self.set_a(ea_reg, addr);
                }

                8
            }
            0x4 => {
                // FMOVE <ea>, control register (FPCR, FPSR, FPIAR)
                // or FMOVEM <ea>, control register list
                let ctrl_sel = (w2 >> 10) & 0x7;
                let _w2 = self.read_imm_16(bus);

                let ea = (opcode & 0x3f) as u8;
                let ea_mode = (ea >> 3) & 7;
                let ea_reg = (ea & 7) as usize;

                // Multi-register lists read consecutive longs in the
                // order FPCR, FPSR, FPIAR; a single Dn/immediate source
                // can only feed a single-register list.
                let count = (ctrl_sel & 4 != 0) as u32
                    + (ctrl_sel & 2 != 0) as u32
                    + (ctrl_sel & 1 != 0) as u32;
                let mut values = [0u32; 3];
                match self.fpu_ea(bus, ea_mode, ea_reg, 4 * count) {
                    Some(FpuEa::DataReg(r)) if count == 1 => values[0] = self.d(r),
                    Some(FpuEa::Immediate) => {
                        for v in values.iter_mut().take(count as usize) {
                            *v = self.read_imm_32(bus);
                        }
                    }
                    Some(FpuEa::Memory(addr)) => {
                        for (i, v) in values.iter_mut().enumerate().take(count as usize) {
                            *v = self.read_32(bus, addr.wrapping_add(4 * i as u32));
                        }
                    }
                    _ => return 0,
                }
                let mut next = values.iter();
                if ctrl_sel & 0x4 != 0 {
                    self.fpcr = *next.next().unwrap();
                }
                if ctrl_sel & 0x2 != 0 {
                    self.fpsr = *next.next().unwrap();
                }
                if ctrl_sel & 0x1 != 0 {
                    self.fpiar = *next.next().unwrap();
                }

                4
            }
            0x5 => {
                // FMOVE control register, <ea> (FPCR, FPSR, FPIAR)
                let ctrl_sel = (w2 >> 10) & 0x7;
                let _w2 = self.read_imm_16(bus);

                let ea = (opcode & 0x3f) as u8;
                let ea_mode = (ea >> 3) & 7;
                let ea_reg = (ea & 7) as usize;

                let count = (ctrl_sel & 4 != 0) as u32
                    + (ctrl_sel & 2 != 0) as u32
                    + (ctrl_sel & 1 != 0) as u32;
                match self.fpu_ea(bus, ea_mode, ea_reg, 4 * count) {
                    Some(FpuEa::DataReg(r)) if count == 1 => {
                        let value = if ctrl_sel & 0x4 != 0 {
                            self.fpcr
                        } else if ctrl_sel & 0x2 != 0 {
                            self.fpsr
                        } else {
                            self.fpiar
                        };
                        self.set_d(r, value);
                    }
                    Some(FpuEa::Memory(addr)) => {
                        // Multi-register lists write consecutive longs in
                        // the order FPCR, FPSR, FPIAR.
                        let mut cur_addr = addr;
                        if ctrl_sel & 0x4 != 0 {
                            self.write_32(bus, cur_addr, self.fpcr);
                            cur_addr = cur_addr.wrapping_add(4);
                        }
                        if ctrl_sel & 0x2 != 0 {
                            self.write_32(bus, cur_addr, self.fpsr);
                            cur_addr = cur_addr.wrapping_add(4);
                        }
                        if ctrl_sel & 0x1 != 0 {
                            self.write_32(bus, cur_addr, self.fpiar);
                        }
                    }
                    _ => return 0,
                }

                4
            }
            _ => 0,
        }
    }

    /// FBcc - FPU conditional branch.
    ///
    /// Note: The PC has already been advanced past the displacement when this is called.
    pub fn exec_fbcc(&mut self, condition: u8, disp: i32) -> i32 {
        let take_branch = self.fpu_condition(condition);

        if take_branch {
            self.change_of_flow = true;
            // PC was already advanced past displacement; adjust relative to that position
            // Compute target: (PC - disp_size) + disp
            // Since PC is after displacement, we compute: base_pc + disp
            // where base_pc is the address of the first extension word
            let base_pc = self.ppc.wrapping_add(2); // ppc is opcode, +2 is extension word
            self.pc = (base_pc as i32).wrapping_add(disp) as u32;
        }

        8
    }

    /// FScc - Set byte on FPU condition.
    pub fn exec_fscc<B: AddressBus>(
        &mut self,
        bus: &mut B,
        ea_mode: u8,
        ea_reg: usize,
        condition: u8,
    ) -> i32 {
        let value = if self.fpu_condition(condition) {
            0xFFu8
        } else {
            0x00u8
        };

        match self.fpu_ea(bus, ea_mode, ea_reg, 1) {
            Some(FpuEa::DataReg(r)) => {
                self.set_d(r, (self.d(r) & 0xFFFF_FF00) | value as u32);
            }
            Some(FpuEa::Memory(addr)) => {
                self.write_8(bus, addr, value);
            }
            _ => return 0,
        }

        4
    }

    /// FDBcc - decrement and branch on FPU condition false. The
    /// displacement is relative to its own extension word's address.
    pub fn exec_fdbcc<B: AddressBus>(&mut self, bus: &mut B, reg: usize, condition: u8) -> i32 {
        let disp_base = self.pc;
        let disp = self.read_imm_16(bus) as i16 as i32;
        if !self.fpu_condition(condition) {
            let count = (self.d(reg) as u16).wrapping_sub(1);
            self.set_d(reg, (self.d(reg) & 0xFFFF_0000) | u32::from(count));
            if count != 0xFFFF {
                self.change_of_flow = true;
                self.pc = (disp_base as i32).wrapping_add(disp) as u32;
            }
        }
        8
    }

    /// FTRAPcc - trap on FPU condition (with optional ignored operand
    /// word(s) already addressed by `imm_words`).
    pub fn exec_ftrapcc<B: AddressBus>(
        &mut self,
        bus: &mut B,
        condition: u8,
        imm_words: u32,
    ) -> i32 {
        for _ in 0..imm_words {
            let _ = self.read_imm_16(bus);
        }
        if self.fpu_condition(condition) {
            // FTRAPcc takes the TRAPcc/TRAPV exception vector (7).
            return self.take_exception(bus, 7);
        }
        4
    }

    /// 68040 FPU "op1" entrypoint (opcode pattern 0xF3xx in Musashi: `040fpu1`).
    ///
    /// Implements a minimal subset: `FSAVE <ea>` and `FRESTORE <ea>` for a NULL/IDLE frame.
    pub fn exec_fpu_op1<B: AddressBus>(&mut self, bus: &mut B, opcode: u16) -> i32 {
        let ea_mode = ((opcode >> 3) & 7) as u8;
        let ea_reg = (opcode & 7) as usize;
        let op = ((opcode >> 6) & 3) as u8;

        match op {
            0 => self.exec_fsave(bus, ea_mode, ea_reg),
            1 => self.exec_frestore(bus, ea_mode, ea_reg),
            _ => 0, // unsupported -> let caller raise LINE1111 without consuming extensions
        }
    }

    fn exec_fsave<B: AddressBus>(&mut self, bus: &mut B, ea_mode: u8, ea_reg: usize) -> i32 {
        // Musashi supports only (An)+ and -(An) here for 68040.
        match ea_mode {
            3 => {
                // (An)+
                let addr = self.a(ea_reg);
                self.set_a(ea_reg, addr.wrapping_add(4));

                if self.fpu_just_reset {
                    self.write_32(bus, addr, 0);
                } else {
                    // Total frame size is 7 longs (28 bytes). EA increment already did +4.
                    self.set_a(ea_reg, self.a(ea_reg).wrapping_add(6 * 4));
                    perform_fsave(bus, self, addr, true);
                }
                8
            }
            4 => {
                // -(An)
                let addr_hi = self.a(ea_reg).wrapping_sub(4);
                self.set_a(ea_reg, addr_hi);

                if self.fpu_just_reset {
                    self.write_32(bus, addr_hi, 0);
                } else {
                    // Total frame size is 28 bytes; one predecrement already happened (-4).
                    self.set_a(ea_reg, self.a(ea_reg).wrapping_sub(6 * 4));
                    perform_fsave(bus, self, addr_hi, false);
                }
                8
            }
            _ => 0,
        }
    }

    fn exec_frestore<B: AddressBus>(&mut self, bus: &mut B, ea_mode: u8, ea_reg: usize) -> i32 {
        match ea_mode {
            2 => {
                // (An)
                let addr = self.a(ea_reg);
                let header = self.read_32(bus, addr);
                if (header & 0xFF00_0000) == 0 {
                    self.do_frestore_null();
                } else {
                    self.fpu_just_reset = false;
                }
                8
            }
            3 => {
                // (An)+
                let addr = self.a(ea_reg);
                self.set_a(ea_reg, addr.wrapping_add(4));
                let header = self.read_32(bus, addr);

                if (header & 0xFF00_0000) == 0 {
                    self.do_frestore_null();
                } else {
                    self.fpu_just_reset = false;

                    // Musashi adjusts A-reg by additional bytes based on frame type.
                    // (EA macro already did +4.)
                    let kind = header & 0x00FF_0000;
                    let extra = match kind {
                        0x0018_0000 => 6 * 4,  // IDLE
                        0x0038_0000 => 14 * 4, // UNIMP
                        0x00B4_0000 => 45 * 4, // BUSY
                        _ => 0,
                    };
                    self.set_a(ea_reg, self.a(ea_reg).wrapping_add(extra));
                }
                8
            }
            _ => 0,
        }
    }

    fn do_frestore_null(&mut self) {
        self.fpcr = 0;
        self.fpsr = 0;
        self.fpiar = 0;
        self.fpr = [FloatX80::default_nan(); 8];
        self.fpu_just_reset = true;
    }

    /// Set the FPSR condition-code byte (bits 24-27) from an extended value.
    /// N reflects the sign bit for every class (so -0 and -NaN set N).
    fn fpu_set_cc(&mut self, value: FloatX80) {
        const FPCC_N: u32 = 0x0800_0000;
        const FPCC_Z: u32 = 0x0400_0000;
        const FPCC_I: u32 = 0x0200_0000;
        const FPCC_NAN: u32 = 0x0100_0000;

        self.fpsr &= !(FPCC_N | FPCC_Z | FPCC_I | FPCC_NAN);
        if value.is_nan() {
            self.fpsr |= FPCC_NAN;
        } else if value.is_inf() {
            self.fpsr |= FPCC_I;
        } else if value.is_zero() {
            self.fpsr |= FPCC_Z;
        }
        if value.sign() {
            self.fpsr |= FPCC_N;
        }
    }

    /// Set the FPSR condition codes from an ordered comparison result.
    fn fpu_set_cc_cmp(&mut self, cmp: FpCmp) {
        const FPCC_N: u32 = 0x0800_0000;
        const FPCC_Z: u32 = 0x0400_0000;
        const FPCC_I: u32 = 0x0200_0000;
        const FPCC_NAN: u32 = 0x0100_0000;

        self.fpsr &= !(FPCC_N | FPCC_Z | FPCC_I | FPCC_NAN);
        match cmp {
            FpCmp::Less => self.fpsr |= FPCC_N,
            FpCmp::Equal => self.fpsr |= FPCC_Z,
            FpCmp::Greater => {}
            FpCmp::Unordered => self.fpsr |= FPCC_NAN,
        }
    }

    /// Build the rounding context from FPCR for a base-precision operation.
    fn fpu_ctx(&self) -> RoundCtx {
        RoundCtx {
            mode: RoundMode::from_fpcr(self.fpcr),
            prec: Precision::Extended,
        }
    }

    /// Phase 1 exception commit: map the engine's OPERR/DZ onto the existing
    /// crude FPSR bits. Phase 2 replaces this with full EXC/AEXC modeling.
    fn fpu_commit(&mut self, f: ExcFlags) {
        if f.has(ExcFlags::OPERR) {
            self.fpsr |= 0x20;
        }
        if f.has(ExcFlags::DZ) {
            self.fpsr |= 0x10;
        }
    }

    /// Evaluate a 6888x conditional predicate against FPSR. The upper
    /// half of the condition space (bit 5 (actually bit 4 of the 5-bit
    /// field)) only differs by signalling BSUN on NaN, which the emulated
    /// FPU does not raise, so it folds onto the lower half.
    fn fpu_condition(&self, condition: u8) -> bool {
        const FPCC_N: u32 = 0x0800_0000;
        const FPCC_Z: u32 = 0x0400_0000;
        const FPCC_NAN: u32 = 0x0100_0000;

        let n = (self.fpsr & FPCC_N) != 0;
        let z = (self.fpsr & FPCC_Z) != 0;
        let nan = (self.fpsr & FPCC_NAN) != 0;

        match condition & 0x0F {
            0x0 => false,             // F
            0x1 => z,                 // EQ
            0x2 => !(nan || z || n),  // OGT
            0x3 => z || !(nan || n),  // OGE
            0x4 => n && !(nan || z),  // OLT
            0x5 => z || (n && !nan),  // OLE
            0x6 => !(nan || z),       // OGL
            0x7 => !nan,              // OR
            0x8 => nan,               // UN
            0x9 => nan || z,          // UEQ
            0xA => nan || !(n || z),  // UGT
            0xB => nan || z || !n,    // UGE
            0xC => nan || (n && !z),  // ULT
            0xD => nan || n || z,     // ULE
            0xE => !z,                // NE
            _ => true,                // T
        }
    }

    /// Apply a general FPU ALU operation `opmode` to destination register
    /// `dst` using the source operand `src` (an FPm register or an
    /// <ea>/immediate operand). This is the single dispatch table shared by
    /// the register-source (`subop 0x0`) and memory/immediate-source (`subop
    /// 0x2`) paths so the two cannot drift apart. FMOVECR has no source
    /// operand and is handled by the callers.
    ///
    /// Phase 0: arithmetic is still computed via an f64 bridge
    /// (`src.to_f64()` ... `FloatX80::from_f64(result)`) so results match the
    /// previous core exactly while the register file is the extended type;
    /// Phase 1 swaps each arm onto the softfloat engine. The 6888x
    /// single/double rounding-precision variants (FSxxx/FDxxx) still fold
    /// onto their base op here.
    fn fpu_apply_op(&mut self, opmode: u16, dst: usize, src: FloatX80) -> i32 {
        let s = src.to_f64();
        match opmode {
            0x00 | 0x40 | 0x44 => {
                // FMOVE / FSMOVE / FDMOVE (lossless copy)
                self.fpr[dst] = src;
                self.fpu_set_cc(src);
                4
            }
            0x01 => {
                // FINT - round to integer using the FPCR rounding mode
                let mode = RoundMode::from_fpcr(self.fpcr);
                let mut f = ExcFlags::default();
                self.fpr[dst] = softfloat::round_to_int(src, mode, &mut f);
                self.fpu_set_cc(self.fpr[dst]);
                self.fpu_commit(f);
                4
            }
            0x03 => {
                // FINTRZ - round to integer toward zero
                let mut f = ExcFlags::default();
                self.fpr[dst] = softfloat::round_to_int(src, RoundMode::Zero, &mut f);
                self.fpu_set_cc(self.fpr[dst]);
                self.fpu_commit(f);
                4
            }
            0x04 | 0x41 | 0x45 => {
                // FSQRT / FSSQRT / FDSQRT
                let ctx = self.fpu_ctx();
                let mut f = ExcFlags::default();
                self.fpr[dst] = softfloat::sqrt(src, ctx, &mut f);
                self.fpu_set_cc(self.fpr[dst]);
                self.fpu_commit(f);
                4
            }
            0x18 | 0x58 | 0x5C => {
                // FABS / FSABS / FDABS
                self.fpr[dst] = softfloat::abs(src);
                self.fpu_set_cc(self.fpr[dst]);
                4
            }
            0x1A | 0x5A | 0x5E => {
                // FNEG / FSNEG / FDNEG
                self.fpr[dst] = softfloat::neg(src);
                self.fpu_set_cc(self.fpr[dst]);
                4
            }
            0x20 | 0x60 | 0x64 => {
                // FDIV / FSDIV / FDDIV - dst = dst / src
                let ctx = self.fpu_ctx();
                let mut f = ExcFlags::default();
                self.fpr[dst] = softfloat::div(self.fpr[dst], src, ctx, &mut f);
                self.fpu_set_cc(self.fpr[dst]);
                self.fpu_commit(f);
                4
            }
            0x22 | 0x62 | 0x66 => {
                // FADD / FSADD / FDADD
                let ctx = self.fpu_ctx();
                let mut f = ExcFlags::default();
                self.fpr[dst] = softfloat::add(self.fpr[dst], src, ctx, &mut f);
                self.fpu_set_cc(self.fpr[dst]);
                self.fpu_commit(f);
                4
            }
            0x23 | 0x63 | 0x67 => {
                // FMUL / FSMUL / FDMUL
                let ctx = self.fpu_ctx();
                let mut f = ExcFlags::default();
                self.fpr[dst] = softfloat::mul(self.fpr[dst], src, ctx, &mut f);
                self.fpu_set_cc(self.fpr[dst]);
                self.fpu_commit(f);
                4
            }
            0x28 | 0x68 | 0x6C => {
                // FSUB / FSSUB / FDSUB
                let ctx = self.fpu_ctx();
                let mut f = ExcFlags::default();
                self.fpr[dst] = softfloat::sub(self.fpr[dst], src, ctx, &mut f);
                self.fpu_set_cc(self.fpr[dst]);
                self.fpu_commit(f);
                4
            }
            0x38 => {
                // FCMP - ordered compare of dst with src, set condition codes only
                let mut f = ExcFlags::default();
                let cmp = softfloat::compare(self.fpr[dst], src, &mut f);
                self.fpu_set_cc_cmp(cmp);
                self.fpu_commit(f);
                4
            }
            0x3A => {
                // FTST - test src, set condition codes only (no dst write)
                self.fpu_set_cc(src);
                4
            }
            // ========== Transcendental Functions ==========
            0x0E => {
                // FSIN
                self.fpr[dst] = FloatX80::from_f64(s.sin());
                self.fpu_set_cc(self.fpr[dst]);
                4
            }
            0x1D => {
                // FCOS
                self.fpr[dst] = FloatX80::from_f64(s.cos());
                self.fpu_set_cc(self.fpr[dst]);
                4
            }
            0x0F => {
                // FTAN
                self.fpr[dst] = FloatX80::from_f64(s.tan());
                self.fpu_set_cc(self.fpr[dst]);
                4
            }
            0x0C => {
                // FASIN
                self.fpr[dst] = FloatX80::from_f64(s.asin());
                self.fpu_set_cc(self.fpr[dst]);
                4
            }
            0x1C => {
                // FACOS
                self.fpr[dst] = FloatX80::from_f64(s.acos());
                self.fpu_set_cc(self.fpr[dst]);
                4
            }
            0x0A => {
                // FATAN
                self.fpr[dst] = FloatX80::from_f64(s.atan());
                self.fpu_set_cc(self.fpr[dst]);
                4
            }
            0x02 => {
                // FSINH
                self.fpr[dst] = FloatX80::from_f64(s.sinh());
                self.fpu_set_cc(self.fpr[dst]);
                4
            }
            0x19 => {
                // FCOSH
                self.fpr[dst] = FloatX80::from_f64(s.cosh());
                self.fpu_set_cc(self.fpr[dst]);
                4
            }
            0x09 => {
                // FTANH
                self.fpr[dst] = FloatX80::from_f64(s.tanh());
                self.fpu_set_cc(self.fpr[dst]);
                4
            }
            0x0D => {
                // FATANH
                self.fpr[dst] = FloatX80::from_f64(s.atanh());
                self.fpu_set_cc(self.fpr[dst]);
                4
            }
            0x10 => {
                // FETOX (e^x)
                self.fpr[dst] = FloatX80::from_f64(s.exp());
                self.fpu_set_cc(self.fpr[dst]);
                4
            }
            0x08 => {
                // FETOXM1 (e^x - 1)
                self.fpr[dst] = FloatX80::from_f64(s.exp_m1());
                self.fpu_set_cc(self.fpr[dst]);
                4
            }
            0x11 => {
                // FTWOTOX (2^x)
                self.fpr[dst] = FloatX80::from_f64((2.0_f64).powf(s));
                self.fpu_set_cc(self.fpr[dst]);
                4
            }
            0x12 => {
                // FTENTOX (10^x)
                self.fpr[dst] = FloatX80::from_f64((10.0_f64).powf(s));
                self.fpu_set_cc(self.fpr[dst]);
                4
            }
            0x14 => {
                // FLOGN (ln(x))
                self.fpr[dst] = FloatX80::from_f64(s.ln());
                self.fpu_set_cc(self.fpr[dst]);
                4
            }
            0x06 => {
                // FLOGNP1 (ln(1+x))
                self.fpr[dst] = FloatX80::from_f64(s.ln_1p());
                self.fpu_set_cc(self.fpr[dst]);
                4
            }
            0x15 => {
                // FLOG10 (log10(x))
                self.fpr[dst] = FloatX80::from_f64(s.log10());
                self.fpu_set_cc(self.fpr[dst]);
                4
            }
            0x16 => {
                // FLOG2 (log2(x))
                self.fpr[dst] = FloatX80::from_f64(s.log2());
                self.fpu_set_cc(self.fpr[dst]);
                4
            }
            0x1E => {
                // FGETEXP - extract the unbiased exponent
                let mut f = ExcFlags::default();
                self.fpr[dst] = softfloat::getexp(src, &mut f);
                self.fpu_set_cc(self.fpr[dst]);
                self.fpu_commit(f);
                4
            }
            0x1F => {
                // FGETMAN - extract the mantissa as a value in [1, 2)
                let mut f = ExcFlags::default();
                self.fpr[dst] = softfloat::getman(src, &mut f);
                self.fpu_set_cc(self.fpr[dst]);
                self.fpu_commit(f);
                4
            }
            0x21 => {
                // FMOD
                self.fpr[dst] = FloatX80::from_f64(self.fpr[dst].to_f64() % s);
                self.fpu_set_cc(self.fpr[dst]);
                4
            }
            0x25 => {
                // FREM (IEEE remainder): r = x - y*round(x/y)
                let mut d = self.fpr[dst].to_f64();
                if s != 0.0 {
                    let n = (d / s).round();
                    d -= s * n;
                }
                self.fpr[dst] = FloatX80::from_f64(d);
                self.fpu_set_cc(self.fpr[dst]);
                4
            }
            0x26 => {
                // FSCALE - dst = dst * 2^trunc(src)
                let ctx = self.fpu_ctx();
                let mut f = ExcFlags::default();
                let n = s as i32;
                self.fpr[dst] = softfloat::scale(self.fpr[dst], n, ctx, &mut f);
                self.fpu_set_cc(self.fpr[dst]);
                self.fpu_commit(f);
                4
            }
            0x30..=0x37 => {
                // FSINCOS - compute sin and cos simultaneously.
                // Bottom 3 bits of opmode = cos destination register.
                let cos_dst = (opmode & 7) as usize;
                self.fpr[dst] = FloatX80::from_f64(s.sin());
                self.fpr[cos_dst] = FloatX80::from_f64(s.cos());
                self.fpu_set_cc(self.fpr[dst]);
                4
            }
            _ => 0, // Unimplemented opmode
        }
    }

    /// Resolve the effective address for an FPU operand of `bytes`
    /// (0 = control mode, no auto-adjust). Post-increment/pre-decrement
    /// apply the FPU operand width; every other mode -- including the
    /// 68020 indexed and full-extension forms -- goes through the core
    /// resolver. Address-register direct is not a legal FPU operand.
    fn fpu_ea<B: AddressBus>(
        &mut self,
        bus: &mut B,
        ea_mode: u8,
        ea_reg: usize,
        bytes: u32,
    ) -> Option<FpuEa> {
        let mode = AddressingMode::decode(ea_mode, ea_reg as u8)?;
        match mode {
            AddressingMode::DataDirect(r) => Some(FpuEa::DataReg(r as usize)),
            AddressingMode::AddressDirect(_) => None,
            AddressingMode::PostIncrement(r) => {
                let r = r as usize;
                // A7 stays word-aligned for byte operands.
                let inc = if bytes == 1 && r == 7 { 2 } else { bytes };
                let addr = self.a(r);
                self.set_a(r, addr.wrapping_add(inc));
                Some(FpuEa::Memory(addr))
            }
            AddressingMode::PreDecrement(r) => {
                let r = r as usize;
                let dec = if bytes == 1 && r == 7 { 2 } else { bytes };
                let addr = self.a(r).wrapping_sub(dec);
                self.set_a(r, addr);
                Some(FpuEa::Memory(addr))
            }
            AddressingMode::Immediate => Some(FpuEa::Immediate),
            other => match self.resolve_ea(bus, other, Size::Long) {
                EaResult::Memory(addr) => Some(FpuEa::Memory(addr)),
                _ => None,
            },
        }
    }

    /// Read the ALU/FMOVE source operand in `fmt` (0=long, 1=single,
    /// 2=extended, 4=word, 6=byte, 5=double) as an extended value. The
    /// extended format (2) is read losslessly; the others go through an f64
    /// bridge for now (Phase 1 replaces these with exact widening). Packed
    /// decimal (3) is unimplemented and reports as unhandled (F-line).
    fn fpu_read_source<B: AddressBus>(
        &mut self,
        bus: &mut B,
        opcode: u16,
        fmt: u16,
    ) -> Option<FloatX80> {
        let ea_mode = ((opcode >> 3) & 7) as u8;
        let ea_reg = (opcode & 7) as usize;
        let bytes: u32 = match fmt {
            6 => 1,
            4 => 2,
            0 | 1 => 4,
            5 => 8,
            2 => 12,
            _ => return None, // packed decimal (3) unimplemented
        };
        match self.fpu_ea(bus, ea_mode, ea_reg, bytes)? {
            FpuEa::DataReg(r) => {
                let v = self.d(r);
                match fmt {
                    0 => Some(FloatX80::from_f64(v as i32 as f64)),
                    1 => Some(FloatX80::from_f64(f32::from_bits(v) as f64)),
                    4 => Some(FloatX80::from_f64(v as u16 as i16 as f64)),
                    6 => Some(FloatX80::from_f64(v as u8 as i8 as f64)),
                    _ => None, // 8/12-byte operands cannot live in Dn
                }
            }
            FpuEa::Immediate => match fmt {
                0 => Some(FloatX80::from_f64(self.read_imm_32(bus) as i32 as f64)),
                1 => Some(FloatX80::from_f64(f32::from_bits(self.read_imm_32(bus)) as f64)),
                4 => Some(FloatX80::from_f64(self.read_imm_16(bus) as i16 as f64)),
                6 => Some(FloatX80::from_f64((self.read_imm_16(bus) & 0xFF) as u8 as i8 as f64)),
                5 => {
                    let hi = self.read_imm_32(bus) as u64;
                    let lo = self.read_imm_32(bus) as u64;
                    Some(FloatX80::from_f64(f64::from_bits((hi << 32) | lo)))
                }
                2 => {
                    let exp_word = (self.read_imm_32(bus) >> 16) as u16;
                    let hi = self.read_imm_32(bus) as u64;
                    let lo = self.read_imm_32(bus) as u64;
                    Some(FloatX80::from_extended(exp_word, (hi << 32) | lo))
                }
                _ => None,
            },
            FpuEa::Memory(addr) => match fmt {
                0 => Some(FloatX80::from_f64(self.read_32(bus, addr) as i32 as f64)),
                1 => Some(FloatX80::from_f64(f32::from_bits(self.read_32(bus, addr)) as f64)),
                4 => Some(FloatX80::from_f64(self.read_16(bus, addr) as i16 as f64)),
                6 => Some(FloatX80::from_f64(self.read_8(bus, addr) as i8 as f64)),
                5 => {
                    let hi = self.read_32(bus, addr) as u64;
                    let lo = self.read_32(bus, addr.wrapping_add(4)) as u64;
                    Some(FloatX80::from_f64(f64::from_bits((hi << 32) | lo)))
                }
                2 => {
                    let exp_word = self.read_16(bus, addr);
                    let hi = self.read_32(bus, addr.wrapping_add(4)) as u64;
                    let lo = self.read_32(bus, addr.wrapping_add(8)) as u64;
                    Some(FloatX80::from_extended(exp_word, (hi << 32) | lo))
                }
                _ => None,
            },
        }
    }

    /// Write `value` to the FMOVE destination in `fmt`. Integer formats
    /// round per FPCR and saturate; packed decimal (3 and 7) is
    /// unimplemented and reports as unhandled (F-line). Returns false if
    /// the format/EA combination cannot be carried out.
    fn fpu_write_dest<B: AddressBus>(
        &mut self,
        bus: &mut B,
        ea_mode: u8,
        ea_reg: usize,
        fmt: u16,
        value: FloatX80,
    ) -> bool {
        let bytes: u32 = match fmt {
            6 => 1,
            4 => 2,
            0 | 1 => 4,
            5 => 8,
            2 => 12,
            _ => return false, // packed decimal unimplemented
        };
        let Some(ea) = self.fpu_ea(bus, ea_mode, ea_reg, bytes) else {
            return false;
        };
        // Bridge: integer/single/double formats round through f64 for now;
        // the extended format is written losslessly. Phase 1 replaces the
        // bridge with exact softfloat conversions.
        let fv = value.to_f64();
        match ea {
            FpuEa::DataReg(r) => match fmt {
                0 => {
                    let v = f64_to_int_saturating(fv, self.fpcr, i32::MIN as i64, i32::MAX as i64);
                    self.set_d(r, v as u32);
                    true
                }
                1 => {
                    self.set_d(r, (fv as f32).to_bits());
                    true
                }
                4 => {
                    let v = f64_to_int_saturating(fv, self.fpcr, i16::MIN as i64, i16::MAX as i64);
                    self.set_d(r, (self.d(r) & 0xFFFF_0000) | (v as u16 as u32));
                    true
                }
                6 => {
                    let v = f64_to_int_saturating(fv, self.fpcr, i8::MIN as i64, i8::MAX as i64);
                    self.set_d(r, (self.d(r) & 0xFFFF_FF00) | (v as u8 as u32));
                    true
                }
                _ => false,
            },
            FpuEa::Memory(addr) => match fmt {
                0 => {
                    let v = f64_to_int_saturating(fv, self.fpcr, i32::MIN as i64, i32::MAX as i64);
                    self.write_32(bus, addr, v as u32);
                    true
                }
                1 => {
                    self.write_32(bus, addr, (fv as f32).to_bits());
                    true
                }
                4 => {
                    let v = f64_to_int_saturating(fv, self.fpcr, i16::MIN as i64, i16::MAX as i64);
                    self.write_16(bus, addr, v as u16);
                    true
                }
                6 => {
                    let v = f64_to_int_saturating(fv, self.fpcr, i8::MIN as i64, i8::MAX as i64);
                    self.write_8(bus, addr, v as u8);
                    true
                }
                5 => {
                    let bits = fv.to_bits();
                    self.write_32(bus, addr, (bits >> 32) as u32);
                    self.write_32(bus, addr.wrapping_add(4), bits as u32);
                    true
                }
                2 => {
                    let (exp_word, mantissa) = value.to_extended();
                    self.write_32(bus, addr, (exp_word as u32) << 16);
                    self.write_32(bus, addr.wrapping_add(4), (mantissa >> 32) as u32);
                    self.write_32(bus, addr.wrapping_add(8), mantissa as u32);
                    true
                }
                _ => false,
            },
            FpuEa::Immediate => false,
        }
    }
}

fn perform_fsave<B: AddressBus>(bus: &mut B, cpu: &mut CpuCore, addr: u32, inc: bool) {
    // Generate a 68881-style "IDLE" frame as Musashi does for 68040 FSAVE.
    // This is sufficient for many OSes that only probe save/restore behavior.
    if inc {
        cpu.write_32(bus, addr, 0x1F18_0000);
        cpu.write_32(bus, addr.wrapping_add(4), 0);
        cpu.write_32(bus, addr.wrapping_add(8), 0);
        cpu.write_32(bus, addr.wrapping_add(12), 0);
        cpu.write_32(bus, addr.wrapping_add(16), 0);
        cpu.write_32(bus, addr.wrapping_add(20), 0);
        cpu.write_32(bus, addr.wrapping_add(24), 0x7000_0000);
    } else {
        cpu.write_32(bus, addr, 0x7000_0000);
        cpu.write_32(bus, addr.wrapping_sub(4), 0);
        cpu.write_32(bus, addr.wrapping_sub(8), 0);
        cpu.write_32(bus, addr.wrapping_sub(12), 0);
        cpu.write_32(bus, addr.wrapping_sub(16), 0);
        cpu.write_32(bus, addr.wrapping_sub(20), 0);
        cpu.write_32(bus, addr.wrapping_sub(24), 0x1F18_0000);
    }
}

