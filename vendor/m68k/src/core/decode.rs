//! Instruction decoding and dispatch.
//!
//! Decodes opcodes and dispatches to appropriate handlers.

use super::cpu::CpuCore;
use super::ea::{AddressingMode, EaResult};
use super::execute::RUN_MODE_BERR_AERR_RESET;
use super::memory::AddressBus;
use super::types::{CpuType, InternalStepResult, Size};

// ============================================================================
// Trap Interception Sentinels
// ============================================================================
//
// SAFETY: Sentinel values are used to signal trap interception to the caller.
// The fallback exception methods (e.g., `take_trap_exception()`) rewind the PC
// to `ppc` before taking the hardware exception.
//
// This is ONLY safe if the instruction that returned the sentinel has NOT:
// 1. Read any extension words (PC must only have advanced by 2 for the opcode)
// 2. Modified any registers or memory
// 3. Performed any side effects
//
// Currently safe instructions:
// - A-line (0xAxxx): Detected immediately by group dispatch
// - F-line (0xFxxx): Detected immediately by group dispatch (68000/68010)
// - TRAP #n: Pattern match on opcode bits only, no EA decoding
// - BKPT #n: Pattern match on opcode bits only, no EA decoding
// - ILLEGAL (0x4AFC): Explicit early match, no EA decoding
//
// If adding new interceptable instructions, verify they meet these criteria!
// ============================================================================

/// Sentinel value for A-line traps (0xAxxx opcodes).
pub(crate) const ALINE_TRAP_SENTINEL: i32 = -1_000_000;

/// Sentinel value for F-line traps (0xFxxx opcodes on 68000/68010).
pub(crate) const FLINE_TRAP_SENTINEL: i32 = -1_000_001;

/// Sentinel base for TRAP #n instructions (n in 0..15).
pub(crate) const TRAP_SENTINEL_BASE: i32 = -1_000_100;

/// Sentinel base for BKPT #n instructions (n in 0..7).
pub(crate) const BKPT_SENTINEL_BASE: i32 = -1_000_200;

/// Sentinel for ILLEGAL instruction (0x4AFC).
pub(crate) const ILLEGAL_SENTINEL: i32 = -1_000_300;

// ============================================================================
// Main Dispatch
// ============================================================================

/// Dispatch an instruction based on its opcode.
///
/// Returns an `InternalStepResult` which includes trap variants for internal handling.
pub(crate) fn dispatch_instruction<B: AddressBus>(
    cpu: &mut CpuCore,
    bus: &mut B,
    opcode: u16,
) -> InternalStepResult {
    // Get the top 4 bits for group dispatch
    let group = (opcode >> 12) & 0xF;

    // Dispatch by group
    let cycles = match group {
        0x0 => dispatch_group_0(cpu, bus, opcode), // Bit ops, MOVEP, Imm
        0x1 => dispatch_move(cpu, bus, opcode, Size::Byte),
        0x2 => dispatch_move(cpu, bus, opcode, Size::Long),
        0x3 => dispatch_move(cpu, bus, opcode, Size::Word),
        0x4 => dispatch_group_4(cpu, bus, opcode), // Misc (LEA, TRAP, etc.)
        0x5 => dispatch_group_5(cpu, bus, opcode), // ADDQ/SUBQ/Scc/DBcc
        0x6 => dispatch_group_6(cpu, bus, opcode), // Bcc/BSR
        0x7 => dispatch_moveq(cpu, opcode),
        0x8 => dispatch_group_8(cpu, bus, opcode), // OR/DIV/SBCD
        0x9 => dispatch_group_9(cpu, bus, opcode), // SUB/SUBX
        0xA => exception_1010(cpu, opcode),
        0xB => dispatch_group_b(cpu, bus, opcode), // CMP/EOR
        0xC => dispatch_group_c(cpu, bus, opcode), // AND/MUL/ABCD/EXG
        0xD => dispatch_group_d(cpu, bus, opcode), // ADD/ADDX
        0xE => dispatch_group_e(cpu, bus, opcode), // Shift/Rotate
        0xF => dispatch_group_f(cpu, bus, opcode),
        _ => unreachable!(),
    };

    // Fast path: normal instructions return small non-negative cycle counts;
    // sentinels are large negative values.
    if cycles >= 0 {
        return InternalStepResult::Ok { cycles };
    }

    // Rare path: sentinel values (trap, illegal, etc.).
    if cycles == ALINE_TRAP_SENTINEL {
        return InternalStepResult::AlineTrap { opcode };
    }
    if cycles == FLINE_TRAP_SENTINEL {
        return InternalStepResult::FlineTrap { opcode };
    }
    if (TRAP_SENTINEL_BASE..TRAP_SENTINEL_BASE + 16).contains(&cycles) {
        let trap_num = (cycles - TRAP_SENTINEL_BASE) as u8;
        return InternalStepResult::TrapInstruction { trap_num };
    }
    if (BKPT_SENTINEL_BASE..BKPT_SENTINEL_BASE + 8).contains(&cycles) {
        let bp_num = (cycles - BKPT_SENTINEL_BASE) as u8;
        return InternalStepResult::Breakpoint { bp_num };
    }
    if cycles == ILLEGAL_SENTINEL {
        return InternalStepResult::IllegalInstruction { opcode };
    }

    // Fallback: should not happen (all negative cycles should match a
    // sentinel), but return Ok to match previous behaviour.
    InternalStepResult::Ok { cycles }
}

// ============================================================================
// Group F: Coprocessor / FPU (68040: 0xF2xx/0xF3xx)
// ============================================================================

fn dispatch_group_f<B: AddressBus>(cpu: &mut CpuCore, bus: &mut B, opcode: u16) -> i32 {
    // Musashi patterns:
    // - 040fpu0: 1111 0010 ........  (0xF2xx) -> m68040_fpu_op0
    // - 040fpu1: 1111 0011 ........  (0xF3xx) -> m68040_fpu_op1
    //
    // FPU coprocessor interface is available on 68020+ (via external 68881/82 or integrated 68040 FPU).
    // 68000/68010/SCC68070 don't have the coprocessor interface, so all F-line opcodes are Line-F exceptions.
    let has_coproc_interface = !cpu.is_pre_68020;

    if !has_coproc_interface {
        return exception_1111(cpu, opcode);
    }

    let sub = (opcode >> 8) & 0xF;

    // MOVE16 (68030/68040): 16-byte aligned block transfer
    // Pattern: 1111 0110 0010 0yyy (0xF620-0xF627) for (Ax)+,(Ay)+
    if (opcode & 0xFFF8) == 0xF620 {
        let supports_move16 = matches!(
            cpu.cpu_type,
            CpuType::M68EC030
                | CpuType::M68030
                | CpuType::M68EC040
                | CpuType::M68LC040
                | CpuType::M68040
        );
        if !supports_move16 {
            return illegal_instruction(cpu, bus);
        }
        return cpu.exec_move16(bus, opcode);
    }

    // 68030/68040 Cache Instructions: CINV and CPUSH (F-line, privileged)
    // CINVA/CPUSHA: 1111 0100 x1x1 1000 (0xF418, 0xF438, 0xF458, 0xF478, etc.)
    // CINV/CPUSH line/page: 1111 010x xxxx xaaa
    // We treat these as NOPs since there's no cache to invalidate/push.
    let is_cache_cpu = matches!(
        cpu.cpu_type,
        CpuType::M68EC030
            | CpuType::M68030
            | CpuType::M68EC040
            | CpuType::M68LC040
            | CpuType::M68040
    );
    if is_cache_cpu && (opcode >> 8) & 0xF == 4 {
        // Check for supervisor mode (cache ops are privileged)
        if !cpu.is_supervisor() {
            return cpu.take_exception(bus, 8); // Privilege violation
        }
        // All CINV/CPUSH variants are NOPs for us
        return 4;
    }

    // 68030/68040 PFLUSH instructions (F-line, privileged): 0xF5xx
    // PFLUSHA: 1111 0101 0001 1000 (0xF518)
    // PFLUSHN: 1111 0101 0000 0xxx (0xF500-0xF507)
    // PFLUSH: 1111 0101 0010 0xxx (0xF520-0xF527)
    // We treat these as NOPs since there's no TLB to flush.
    if is_cache_cpu && (opcode >> 8) & 0xF == 5 {
        if !cpu.is_supervisor() {
            return cpu.take_exception(bus, 8); // Privilege violation
        }
        // All PFLUSH variants are NOPs for us
        return 4;
    }

    // PMMU/COP0 opcodes are in the 0xF0xx/0xF1xx range (1111 000? .... ....) and are further
    // subdivided by (opcode>>9)&7. Group 0 carries PMOVE/PFLUSH/PTEST/etc with an extension word.
    if ((opcode >> 9) & 0x7) == 0 {
        let cycles = cpu.exec_mmu_op0(bus, opcode);
        if cycles != 0 {
            return cycles;
        }
    }

    // The 0xF240-0xF27F block splits on the EA-mode field:
    //   mode 001          -> FDBcc Dn,disp
    //   mode 111, reg 2-4 -> FTRAPcc (.W / .L / no operand)
    //   everything else   -> FScc <ea>
    if (opcode & 0xFFC0) == 0xF240 {
        let ea_mode = ((opcode >> 3) & 7) as u8;
        let ea_reg = (opcode & 7) as usize;
        let w2 = cpu.read_imm_16(bus);
        let cond = (w2 & 0x3F) as u8;
        if ea_mode == 1 {
            return cpu.exec_fdbcc(bus, ea_reg, cond);
        }
        if ea_mode == 7 && (2..=4).contains(&ea_reg) {
            let imm_words = match ea_reg {
                2 => 1,
                3 => 2,
                _ => 0,
            };
            return cpu.exec_ftrapcc(bus, cond, imm_words);
        }
        return cpu.exec_fscc(bus, ea_mode, ea_reg, cond);
    }

    // FBcc.W: 1111 0010 10cc cccc (0xF280-0xF2BF)
    // FBcc.L: 1111 0010 11cc cccc (0xF2C0-0xF2FF)
    if (opcode & 0xFFC0) == 0xF280 {
        // FBcc.W - 16-bit displacement
        let cond = (opcode & 0x3F) as u8;
        let disp = cpu.read_imm_16(bus) as i16 as i32;
        return cpu.exec_fbcc(cond, disp);
    }
    if (opcode & 0xFFC0) == 0xF2C0 {
        // FBcc.L - 32-bit displacement
        let cond = (opcode & 0x3F) as u8;
        let disp = cpu.read_imm_32(bus) as i32;
        return cpu.exec_fbcc(cond, disp);
    }

    let cycles = match sub {
        0x2 => cpu.exec_fpu_op0(bus, opcode),
        0x3 => cpu.exec_fpu_op1(bus, opcode),
        _ => 0,
    };
    if cycles != 0 {
        return cycles;
    }

    // Unknown/unsupported coprocessor instruction on a CPU with coprocessor interface:
    // Return FLINE_TRAP_SENTINEL for interception. This allows HLE to handle FPU probes
    // on FPU-less CPUs like 68LC040 without looping in the exception handler.
    // If the HleHandler returns false, step_with_hle_handler will take the exception.
    FLINE_TRAP_SENTINEL
}

// ============================================================================
// MOVE (Groups 1, 2, 3)
// ============================================================================

fn dispatch_move<B: AddressBus>(cpu: &mut CpuCore, bus: &mut B, opcode: u16, size: Size) -> i32 {
    // MOVE encoding: 00ss ddd DDD sss SSS
    // ss = size (01=B, 11=W, 10=L)
    // DDD ddd = destination mode, register
    // SSS sss = source mode, register
    let src_reg = (opcode & 7) as u8;
    let src_mode = ((opcode >> 3) & 7) as u8;
    let dst_reg = ((opcode >> 9) & 7) as u8;
    let dst_mode = ((opcode >> 6) & 7) as u8;

    let src = AddressingMode::decode(src_mode, src_reg);
    let dst = AddressingMode::decode(dst_mode, dst_reg);

    match (src, dst) {
        (Some(src_ea), Some(dst_ea)) => {
            // MOVEA to address register (byte size is illegal)
            if dst_mode == 1 {
                if size == Size::Byte {
                    illegal_instruction(cpu, bus)
                } else {
                    cpu.exec_movea(bus, size, src_ea, dst_reg as usize)
                }
            } else {
                cpu.exec_move(bus, size, src_ea, dst_ea)
            }
        }
        _ => illegal_instruction(cpu, bus),
    }
}

fn dispatch_moveq(cpu: &mut CpuCore, opcode: u16) -> i32 {
    let reg = ((opcode >> 9) & 7) as usize;
    let data = (opcode & 0xFF) as i8 as i32 as u32;
    cpu.set_d(reg, data);
    cpu.n_flag = if (data as i32) < 0 { 0x80 } else { 0 };
    cpu.not_z_flag = data;
    cpu.v_flag = 0;
    cpu.c_flag = 0;
    4
}

// ============================================================================
// Group 0: Bit manipulation, MOVEP, Immediate
// ============================================================================

fn dispatch_group_0<B: AddressBus>(cpu: &mut CpuCore, bus: &mut B, opcode: u16) -> i32 {
    // 68020+ CAS / CAS2 (compare-and-swap)
    // CAS2: 0000 1ss0 1111 1100 with two extension words
    if opcode == 0x0EFC || opcode == 0x0CFC || opcode == 0x0AFC {
        if cpu.cpu_type == CpuType::M68000
            || cpu.cpu_type == CpuType::M68010
            || cpu.cpu_type == CpuType::SCC68070
        {
            return illegal_instruction(cpu, bus);
        }
        return cpu.exec_cas2(bus, opcode);
    }
    // CAS: 0000 1ss0 11 mmm rrr with extension word (Du/Dc)
    // ss encodes size (A=byte, C=word, E=long) in bits 11..9.
    if (opcode & 0x0FC0) == 0x0AC0 || (opcode & 0x0FC0) == 0x0CC0 || (opcode & 0x0FC0) == 0x0EC0 {
        if cpu.cpu_type == CpuType::M68000
            || cpu.cpu_type == CpuType::M68010
            || cpu.cpu_type == CpuType::SCC68070
        {
            return illegal_instruction(cpu, bus);
        }
        return cpu.exec_cas(bus, opcode);
    }

    // 68010+ MOVES - Move to/from address space
    // Pattern: 0000 1110 ssmm mrrr (0x0E00-0x0EFF)
    if (opcode & 0xFF00) == 0x0E00 {
        if cpu.cpu_type == CpuType::M68000 {
            return illegal_instruction(cpu, bus);
        }
        return cpu.exec_moves(bus, opcode);
    }

    // 68020-only CALLM/RTM instructions
    // CALLM: 0000 0110 11 mmm rrr (0x06C0-0x06FF)
    // RTM:   0000 0110 1100 xrrr  (0x06C0-0x06CF, where x=0 for Dn, x=1 for An)
    // RTM is a subset of CALLM's encoding; we check RTM first (mode=1, reg<8)
    if (opcode & 0xFFF0) == 0x06C0 {
        if !matches!(
            cpu.cpu_type,
            CpuType::M68EC020
                | CpuType::M68020
                | CpuType::M68EC030
                | CpuType::M68030
                | CpuType::M68EC040
                | CpuType::M68LC040
                | CpuType::M68040
        ) {
            return illegal_instruction(cpu, bus);
        }
        // RTM Dn/An - mode 1, reg 0-7 with x bit
        return cpu.exec_rtm(bus, opcode);
    }
    if (opcode & 0xFFC0) == 0x06C0 {
        if !matches!(
            cpu.cpu_type,
            CpuType::M68EC020
                | CpuType::M68020
                | CpuType::M68EC030
                | CpuType::M68030
                | CpuType::M68EC040
                | CpuType::M68LC040
                | CpuType::M68040
        ) {
            return illegal_instruction(cpu, bus);
        }
        // CALLM #<data>, <ea>
        return cpu.exec_callm(bus, opcode);
    }

    // 68020+ CMP2 / CHK2 (bounds compare/check)
    // Pattern: 0000 0ss0 11 mmm rrr
    // Key disambiguator vs 68000 bit ops: bit11 must be 0 (bit ops are 0000 1xxx ....).
    if (opcode & 0x0800) == 0
        && (opcode & 0x0100) == 0
        && (opcode & 0x00C0) == 0x00C0
        && ((opcode >> 9) & 3) != 3
    {
        if cpu.is_pre_68020 {
            return illegal_instruction(cpu, bus);
        }
        return cpu.exec_cmp2_chk2(bus, opcode);
    }

    // MOVEP (68000):
    // 0000 ddd 1 s 0 0 1 aaa  with extension word = displacement (d16,An)
    // s: 0=word, 1=long. direction: bit7 (0=mem->reg, 1=reg->mem)
    if (opcode & 0xF138) == 0x0108 {
        let dreg = ((opcode >> 9) & 7) as usize;
        let areg = (opcode & 7) as usize;
        let is_long = (opcode & 0x0040) != 0;
        let reg_to_mem = (opcode & 0x0080) != 0;

        let disp = cpu.read_imm_16(bus) as i16 as i32;
        let base = cpu.a(areg);
        let addr = (base as i32).wrapping_add(disp) as u32;

        if is_long {
            if reg_to_mem {
                let v = cpu.d(dreg);
                cpu.write_8(bus, addr, ((v >> 24) & 0xFF) as u8);
                cpu.write_8(bus, addr.wrapping_add(2), ((v >> 16) & 0xFF) as u8);
                cpu.write_8(bus, addr.wrapping_add(4), ((v >> 8) & 0xFF) as u8);
                cpu.write_8(bus, addr.wrapping_add(6), (v & 0xFF) as u8);
            } else {
                let b0 = cpu.read_8(bus, addr) as u32;
                let b1 = cpu.read_8(bus, addr.wrapping_add(2)) as u32;
                let b2 = cpu.read_8(bus, addr.wrapping_add(4)) as u32;
                let b3 = cpu.read_8(bus, addr.wrapping_add(6)) as u32;
                let v = (b0 << 24) | (b1 << 16) | (b2 << 8) | b3;
                cpu.set_d(dreg, v);
            }
        } else if reg_to_mem {
            let v = cpu.d(dreg) & 0xFFFF;
            cpu.write_8(bus, addr, ((v >> 8) & 0xFF) as u8);
            cpu.write_8(bus, addr.wrapping_add(2), (v & 0xFF) as u8);
        } else {
            let hi = cpu.read_8(bus, addr) as u32;
            let lo = cpu.read_8(bus, addr.wrapping_add(2)) as u32;
            let v = (hi << 8) | lo;
            cpu.set_d(dreg, (cpu.d(dreg) & 0xFFFF0000) | v);
        }

        // MOVEP does not affect condition codes.
        return if is_long { 24 } else { 16 };
    }

    let subop = (opcode >> 8) & 0xF;
    let ea_mode = ((opcode >> 3) & 7) as u8;
    let ea_reg = (opcode & 7) as u8;

    match subop {
        // ORI to CCR / SR are distinguished by size:
        // - ORI.B #<data>,CCR : 0x003C  (size=byte, mode=111 reg=100)
        // - ORI.W #<data>,SR  : 0x007C  (size=word, mode=111 reg=100)
        0x0 if ea_mode == 7 && ea_reg == 4 && ((opcode >> 6) & 3) == 0 => cpu.exec_ori_ccr(bus),
        0x0 if ea_mode == 7 && ea_reg == 4 && ((opcode >> 6) & 3) == 1 => cpu.exec_ori_sr(bus),
        0x0 => {
            if let Some(mode) = AddressingMode::decode(ea_mode, ea_reg) {
                let size = decode_size_00((opcode >> 6) & 3);
                let legacy = cpu.exec_ori(bus, size, mode);
                if cpu.cpu_type == CpuType::M68000 {
                    cpu.immediate_alu_cycles(mode, size)
                } else {
                    legacy
                }
            } else {
                illegal_instruction(cpu, bus)
            }
        }
        // ANDI to CCR: 0x023C
        0x2 if ea_mode == 7 && ea_reg == 4 && ((opcode >> 6) & 3) == 0 => cpu.exec_andi_ccr(bus),
        // ANDI to SR: 0x027C
        0x2 if ea_mode == 7 && ea_reg == 4 && ((opcode >> 6) & 3) == 1 => cpu.exec_andi_sr(bus),
        0x2 => {
            // ANDI
            if let Some(mode) = AddressingMode::decode(ea_mode, ea_reg) {
                let size = decode_size_00((opcode >> 6) & 3);
                let legacy = cpu.exec_andi(bus, size, mode);
                if cpu.cpu_type == CpuType::M68000 {
                    cpu.immediate_alu_cycles(mode, size)
                } else {
                    legacy
                }
            } else {
                illegal_instruction(cpu, bus)
            }
        }
        0x4 => {
            // SUBI: 0000 0100 ss eee eee
            let size_bits = (opcode >> 6) & 3;
            if size_bits == 3 {
                return 4; // Invalid size
            }
            let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
            let size = decode_size_00(size_bits);
            // SUBI manual implementation (mirroring ADDI)
            let imm = read_immediate(cpu, bus, size);
            let ea = cpu.resolve_ea(bus, mode, size);
            let dst = cpu.read_resolved_ea(bus, ea, size);
            let (result, _) = cpu.exec_sub(bus, size, imm, dst);
            cpu.write_resolved_ea(bus, ea, size, result);
            if cpu.cpu_type == CpuType::M68000 {
                cpu.immediate_alu_cycles(mode, size)
            } else if size == Size::Long {
                16
            } else {
                8
            }
        }
        0x6 => {
            // ADDI: 0000 0110 ss eee eee
            // CALLM / RTM are handled by early checks in dispatch_group_0
            let size_bits = (opcode >> 6) & 3;
            if size_bits == 3 {
                // Should be unreachable if CALLM/RTM checks are correct
                // But strictly speaking, if size=11, it is invalid for ADDI.
                return 4;
            }

            let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
            let size = decode_size_00(size_bits);
            // ADDI manual implementation
            let imm = read_immediate(cpu, bus, size);

            let ea = cpu.resolve_ea(bus, mode, size);
            let dst = cpu.read_resolved_ea(bus, ea, size);
            let (result, _cycles) = cpu.exec_add(bus, size, imm, dst);
            cpu.write_resolved_ea(bus, ea, size, result);
            if cpu.cpu_type == CpuType::M68000 {
                cpu.immediate_alu_cycles(mode, size)
            } else {
                _cycles + if size == Size::Long { 8 } else { 4 }
            }
        }
        // EORI.B #<data>,CCR : 0x0A3C
        // EORI.W #<data>,SR  : 0x0A7C
        0xA if ea_mode == 7 && ea_reg == 4 && ((opcode >> 6) & 3) == 0 => cpu.exec_eori_ccr(bus),
        0xA if ea_mode == 7 && ea_reg == 4 && ((opcode >> 6) & 3) == 1 => cpu.exec_eori_sr(bus),
        0xA => {
            let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
            let size = decode_size_00((opcode >> 6) & 3);
            let legacy = cpu.exec_eori(bus, size, mode);
            if cpu.cpu_type == CpuType::M68000 {
                cpu.immediate_alu_cycles(mode, size)
            } else {
                legacy
            }
        }
        0xC => {
            // CMPI
            let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
            let size = decode_size_00((opcode >> 6) & 3);
            let imm = read_immediate(cpu, bus, size);
            let dst = cpu.read_ea(bus, mode, size);
            if cpu.run_mode == RUN_MODE_BERR_AERR_RESET {
                return 50;
            }
            let legacy = cpu.exec_cmp(size, imm, dst);
            if cpu.cpu_type == CpuType::M68000 {
                cpu.cmpi_cycles(mode, size)
            } else {
                legacy
            }
        }
        _ => {
            // Bit operations: BTST, BCHG, BCLR, BSET
            let bit_op = (opcode >> 6) & 3;
            let mode = AddressingMode::decode(ea_mode, ea_reg);

            if let Some(ea) = mode {
                let bit_num = if opcode & 0x100 != 0 {
                    // Dynamic: bit number in Dn
                    let reg = ((opcode >> 9) & 7) as usize;
                    cpu.d(reg)
                } else {
                    // Static: bit number in extension word
                    cpu.read_imm_16(bus) as u32
                };

                let legacy = match bit_op {
                    0 => cpu.exec_btst(bus, bit_num, ea),
                    1 => cpu.exec_bchg(bus, bit_num, ea),
                    2 => cpu.exec_bclr(bus, bit_num, ea),
                    3 => cpu.exec_bset(bus, bit_num, ea),
                    _ => return illegal_instruction(cpu, bus),
                };
                if cpu.cpu_type == CpuType::M68000 {
                    cpu.bitop_cycles(ea, bit_op, opcode & 0x100 == 0)
                } else {
                    legacy
                }
            } else {
                illegal_instruction(cpu, bus)
            }
        }
    }
}

// ============================================================================
// Group 4: Miscellaneous
// ============================================================================

fn dispatch_group_4<B: AddressBus>(cpu: &mut CpuCore, bus: &mut B, opcode: u16) -> i32 {
    let subop = (opcode >> 8) & 0xF;
    let ea_mode = ((opcode >> 3) & 7) as u8;
    let ea_reg = (opcode & 7) as u8;
    let opmode = (opcode >> 6) & 7;

    // 68020+ LINK.L: 0100 1000 0000 1rrr (0x4808..0x480F)
    if (opcode & 0xFFF8) == 0x4808 {
        if cpu.is_pre_68020 {
            return illegal_instruction(cpu, bus);
        }
        return cpu.exec_link_long(bus, ea_reg as usize);
    }

    // 68020+ long multiply/divide (MULL/MULS/MULU, DIVL/DIVS/DIVU, and remainder forms).
    // These share opcode space with MOVEM and must be decoded before MOVEM heuristics.
    if (opcode & 0xFFC0) == 0x4C00 {
        if cpu.is_pre_68020 {
            return illegal_instruction(cpu, bus);
        }
        return cpu.exec_mull(bus, opcode);
    }
    if (opcode & 0xFFC0) == 0x4C40 {
        if cpu.is_pre_68020 {
            return illegal_instruction(cpu, bus);
        }
        return cpu.exec_divl(bus, opcode);
    }

    // MOVE from SR: 0100 0000 11 mmm rrr (0x40C0..0x40FF)
    // Writes SR (word) to <ea>. Does not affect flags.
    if (opcode & 0xFFC0) == 0x40C0 {
        let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
        let sr = cpu.get_sr() as u32;
        // 68000 quirk: like CLR, MOVE from SR reads its destination before
        // writing (removed on the 68010+).
        let ea = cpu.resolve_ea(bus, mode, Size::Word);
        if cpu.run_mode == RUN_MODE_BERR_AERR_RESET {
            return 50;
        }
        if cpu.cpu_type == CpuType::M68000 && !mode.is_register_direct() {
            let _ = cpu.read_resolved_ea(bus, ea, Size::Word);
            if cpu.run_mode == RUN_MODE_BERR_AERR_RESET {
                return 50;
            }
        }
        cpu.write_resolved_ea(bus, ea, Size::Word, sr);
        return if mode.is_register_direct() {
            6
        } else if cpu.cpu_type == CpuType::M68000 {
            8 + cpu.ea_source_cycles(mode, Size::Word)
        } else {
            8
        };
    }

    // 68010+ MOVE from CCR: 0100 0010 11 mmm rrr (0x42C0..0x42FF)
    // Writes CCR (word) to <ea>. Does not affect flags.
    if (opcode & 0xFFC0) == 0x42C0 {
        if cpu.cpu_type == CpuType::M68000 {
            return illegal_instruction(cpu, bus);
        }
        let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
        let ccr = cpu.get_ccr() as u32;
        cpu.write_ea(bus, mode, Size::Word, ccr);
        return if mode.is_register_direct() { 6 } else { 8 };
    }

    // CHK (68000: opmode=110 for CHK.W). Note: opmode=111 overlaps with LEA on 68000.
    if opmode == 0b110 {
        let dst_reg = ((opcode >> 9) & 7) as usize;
        if let Some(mode) = AddressingMode::decode(ea_mode, ea_reg) {
            let size = Size::Word;
            let bound = cpu.read_ea(bus, mode, size);
            return cpu.exec_chk(bus, size, bound, dst_reg);
        } else {
            return illegal_instruction(cpu, bus);
        }
    }

    match opcode {
        0x4E70 => {
            // RESET is privileged.
            if cpu.is_supervisor() {
                // The RESET line is asserted for 124 internal clocks (plus 4
                // decision clocks) before the final prefetch.
                cpu.internal_cycles(128);
                bus.reset_devices();
                132
            } else {
                cpu.exception_privilege(bus)
            }
        } // RESET
        0x4E71 => 4, // NOP
        0x4E72 => {
            // STOP
            if cpu.is_supervisor() {
                // The SR operand is consumed without a prefetch: the CPU
                // stops and performs no further bus activity.
                cpu.consume_without_prefetch = true;
                let sr = cpu.read_imm_16(bus);
                cpu.consume_without_prefetch = false;
                cpu.stop(sr);
                4
            } else {
                cpu.exception_privilege(bus)
            }
        }
        0x4E73 => {
            // RTE
            if cpu.is_supervisor() {
                match cpu.cpu_type {
                    CpuType::M68000 => {
                        let sr = cpu.pull_16(bus);
                        cpu.pc = cpu.pull_32(bus);
                        cpu.set_sr(sr);
                        cpu.full_prefetch(bus);
                        20
                    }
                    CpuType::M68010 | CpuType::SCC68070 => {
                        // Musashi m68k_in.c: format word at (SP+6) >> 12
                        let sp = cpu.a(7);
                        let format = cpu.read_16(bus, sp.wrapping_add(6)) >> 12;
                        if format != 0 {
                            return cpu.take_exception(bus, 14); // format error
                        }
                        let sr = cpu.pull_16(bus);
                        cpu.pc = cpu.pull_32(bus);
                        let _ = cpu.pull_16(bus); // vector offset word
                        cpu.set_sr(sr);
                        20
                    }
                    _ => {
                        // 68020+ RTE loop (Musashi m68k_in.c)
                        loop {
                            let sp = cpu.a(7);
                            let format = cpu.read_16(bus, sp.wrapping_add(6)) >> 12;
                            match format {
                                0 => {
                                    // Normal (format 0)
                                    let sr = cpu.pull_16(bus);
                                    cpu.pc = cpu.pull_32(bus);
                                    let _ = cpu.pull_16(bus); // vector offset word
                                    cpu.set_sr(sr);
                                    return 20;
                                }
                                1 => {
                                    // Throwaway (format 1): discard PC+format, restore SR, then loop.
                                    let sr = cpu.pull_16(bus);
                                    // fake pull 32-bit PC + 16-bit format word
                                    cpu.dar[15] = cpu.dar[15].wrapping_add(4 + 2);
                                    cpu.set_sr(sr);
                                    continue;
                                }
                                2 => {
                                    // Trap (format 2): discard format + address long.
                                    let sr = cpu.pull_16(bus);
                                    cpu.pc = cpu.pull_32(bus);
                                    let _ = cpu.pull_16(bus); // format word
                                    cpu.dar[15] = cpu.dar[15].wrapping_add(4); // address long
                                    cpu.set_sr(sr);
                                    return 20;
                                }
                                _ => {
                                    return cpu.take_exception(bus, 14); // format error
                                }
                            }
                        }
                    }
                }
            } else {
                cpu.exception_privilege(bus)
            }
        }
        0x4E74 => {
            // RTD (68010+): return and deallocate stack arguments.
            // Pop return PC, then add signed word displacement to SP.
            if cpu.cpu_type == CpuType::M68000 {
                illegal_instruction(cpu, bus)
            } else {
                let disp = cpu.read_imm_16(bus) as i16 as i32;
                cpu.pc = cpu.pull_32(bus);
                cpu.dar[15] = (cpu.dar[15] as i32).wrapping_add(disp) as u32;
                20
            }
        }
        0x4E75 => {
            // RTS
            cpu.change_of_flow = true;
            cpu.pc = cpu.pull_32(bus);
            cpu.full_prefetch(bus);
            16
        }
        0x4E76 => {
            // TRAPV
            if cpu.flag_v() {
                cpu.take_exception(bus, 7)
            } else {
                4
            }
        }
        0x4E77 => {
            // RTR
            let ccr = cpu.pull_16(bus) as u8;
            cpu.set_ccr(ccr);
            cpu.change_of_flow = true;
            cpu.pc = cpu.pull_32(bus);
            cpu.full_prefetch(bus);
            20
        }
        0x4E7A => {
            // MOVEC Rc,Rn - Move from control register (68010+)
            if cpu.cpu_type == CpuType::M68000 {
                return illegal_instruction(cpu, bus);
            }
            let ext = bus.read_word(cpu.pc);
            cpu.pc += 2;
            let reg_type = (ext >> 15) & 1; // 0=Dn, 1=An
            let reg_num = ((ext >> 12) & 7) as usize;
            let ctrl_reg = ext & 0xFFF;
            if matches!(cpu.cpu_type, CpuType::M68010 | CpuType::SCC68070)
                && !matches!(ctrl_reg, 0x000 | 0x001 | 0x800 | 0x801)
            {
                return illegal_instruction(cpu, bus);
            }
            if matches!(cpu.cpu_type, CpuType::M68EC020 | CpuType::M68020)
                && !matches!(
                    ctrl_reg,
                    0x000 | 0x001 | 0x002 | 0x800 | 0x801 | 0x802 | 0x803 | 0x804
                )
            {
                return illegal_instruction(cpu, bus);
            }
            if matches!(cpu.cpu_type, CpuType::M68EC030 | CpuType::M68030)
                && !matches!(
                    ctrl_reg,
                    0x000 | 0x001 | 0x002 | 0x800 | 0x801 | 0x802 | 0x803 | 0x804
                )
            {
                return illegal_instruction(cpu, bus);
            }
            if !cpu.is_supervisor() {
                return cpu.take_exception(bus, 8); // Privilege violation
            }
            let value = cpu.read_control_register(ctrl_reg);
            if reg_type == 0 {
                cpu.set_d(reg_num, value);
            } else {
                cpu.set_a(reg_num, value);
            }
            12
        }
        0x4E7B => {
            // MOVEC Rn,Rc - Move to control register (68010+)
            if cpu.cpu_type == CpuType::M68000 {
                return illegal_instruction(cpu, bus);
            }
            let ext = bus.read_word(cpu.pc);
            cpu.pc += 2;
            let reg_type = (ext >> 15) & 1; // 0=Dn, 1=An
            let reg_num = ((ext >> 12) & 7) as usize;
            let ctrl_reg = ext & 0xFFF;
            if matches!(cpu.cpu_type, CpuType::M68010 | CpuType::SCC68070)
                && !matches!(ctrl_reg, 0x000 | 0x001 | 0x800 | 0x801)
            {
                return illegal_instruction(cpu, bus);
            }
            if matches!(cpu.cpu_type, CpuType::M68EC020 | CpuType::M68020)
                && !matches!(
                    ctrl_reg,
                    0x000 | 0x001 | 0x002 | 0x800 | 0x801 | 0x802 | 0x803 | 0x804
                )
            {
                return illegal_instruction(cpu, bus);
            }
            if matches!(cpu.cpu_type, CpuType::M68EC030 | CpuType::M68030)
                && !matches!(
                    ctrl_reg,
                    0x000 | 0x001 | 0x002 | 0x800 | 0x801 | 0x802 | 0x803 | 0x804
                )
            {
                return illegal_instruction(cpu, bus);
            }
            if !cpu.is_supervisor() {
                return cpu.take_exception(bus, 8); // Privilege violation
            }
            let value = if reg_type == 0 {
                cpu.d(reg_num)
            } else {
                cpu.a(reg_num)
            };
            cpu.write_control_register(ctrl_reg, value);
            12
        }
        _ => {
            match subop {
                0x0 => {
                    // NEGX
                    let size = decode_size_00((opcode >> 6) & 3);
                    let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
                    let legacy = cpu.exec_negx(bus, size, mode);
                    if cpu.cpu_type == CpuType::M68000 {
                        cpu.unary_rmw_cycles(mode, size)
                    } else {
                        legacy
                    }
                }
                0x2 => {
                    // CLR
                    let size = decode_size_00((opcode >> 6) & 3);
                    let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
                    let legacy = cpu.exec_clr(bus, size, mode);
                    if cpu.cpu_type == CpuType::M68000 {
                        cpu.unary_rmw_cycles(mode, size)
                    } else {
                        legacy
                    }
                }
                0x4 if (opcode >> 6) & 3 == 3 => {
                    // MOVE to CCR: 0100 0100 11xx xxxx
                    let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
                    let value = cpu.read_ea(bus, mode, Size::Word) as u8;
                    if cpu.run_mode == RUN_MODE_BERR_AERR_RESET {
                        return 50;
                    }
                    cpu.set_ccr(value);
                    // Status modification spends 4 internal clocks before
                    // discarding and refilling the prefetch queue.
                    cpu.internal_cycles(4);
                    cpu.full_prefetch(bus);
                    if cpu.cpu_type == CpuType::M68000 {
                        12 + cpu.ea_source_cycles(mode, Size::Word)
                    } else {
                        12
                    }
                }
                0x4 => {
                    // NEG
                    let size = decode_size_00((opcode >> 6) & 3);
                    let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
                    let legacy = cpu.exec_neg(bus, size, mode);
                    if cpu.cpu_type == CpuType::M68000 {
                        cpu.unary_rmw_cycles(mode, size)
                    } else {
                        legacy
                    }
                }
                0x6 if (opcode >> 6) & 3 == 3 => {
                    // MOVE to SR: 0100 0110 11xx xxxx
                    if !cpu.is_supervisor() {
                        return cpu.exception_privilege(bus);
                    }
                    let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
                    let value = cpu.read_ea(bus, mode, Size::Word);
                    if cpu.run_mode == RUN_MODE_BERR_AERR_RESET {
                        return 50;
                    }
                    cpu.set_sr(value as u16);
                    // Status modification spends 4 internal clocks before
                    // discarding and refilling the prefetch queue.
                    cpu.internal_cycles(4);
                    cpu.full_prefetch(bus);
                    if cpu.cpu_type == CpuType::M68000 {
                        12 + cpu.ea_source_cycles(mode, Size::Word)
                    } else {
                        12
                    }
                }
                0x6 => {
                    // NOT
                    let size = decode_size_00((opcode >> 6) & 3);
                    let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
                    let legacy = cpu.exec_not(bus, size, mode);
                    if cpu.cpu_type == CpuType::M68000 {
                        cpu.unary_rmw_cycles(mode, size)
                    } else {
                        legacy
                    }
                }
                0x8 if (opcode >> 6) & 3 == 1 && ea_mode == 0 => {
                    // SWAP
                    cpu.exec_swap(ea_reg as usize)
                }
                0x8 if (opcode >> 6) & 3 == 1 && ea_mode == 1 => {
                    // BKPT #n (68010+): 0100 1000 0100 1nnn (0x4848..0x484F)
                    // Return sentinel for interception
                    let bp_num = (opcode & 7) as u8;
                    BKPT_SENTINEL_BASE + bp_num as i32
                }
                0x8 if (opcode >> 6) & 3 == 0 => {
                    // NBCD: 0100 1000 00 mmm rrr
                    let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
                    let legacy = cpu.exec_nbcd(bus, mode);
                    if cpu.cpu_type == CpuType::M68000 {
                        if mode.is_register_direct() {
                            6
                        } else {
                            8 + cpu.ea_source_cycles(mode, Size::Byte)
                        }
                    } else {
                        legacy
                    }
                }
                0x8 if (opcode >> 6) & 3 == 2 && ea_mode == 0 => {
                    // EXT.W
                    cpu.exec_ext(Size::Word, ea_reg as usize)
                }
                0x8 if (opcode >> 6) & 3 == 3 && ea_mode == 0 => {
                    // EXT.L
                    cpu.exec_ext(Size::Long, ea_reg as usize)
                }
                0x9 if (opcode >> 6) & 3 == 3 && ea_mode == 0 => {
                    // EXTB.L (68020+) - sign extend byte to long
                    if cpu.is_pre_68020 {
                        illegal_instruction(cpu, bus)
                    } else {
                        cpu.exec_extb(ea_reg as usize)
                    }
                }
                0xA if opcode == 0x4AFC => {
                    // ILLEGAL instruction - return sentinel for interception
                    ILLEGAL_SENTINEL
                }
                0xA if (opcode >> 6) & 3 == 3 => {
                    // TAS
                    let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
                    let legacy = cpu.exec_tas(bus, mode);
                    if cpu.cpu_type == CpuType::M68000 {
                        if mode.is_register_direct() {
                            4
                        } else {
                            // RMW with the indivisible TAS cycle: 10 + EA.
                            10 + cpu.ea_source_cycles(mode, Size::Byte)
                        }
                    } else {
                        legacy
                    }
                }
                0xA => {
                    // TST
                    let size = decode_size_00((opcode >> 6) & 3);
                    let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
                    let legacy = cpu.exec_tst(bus, size, mode);
                    if cpu.cpu_type == CpuType::M68000 {
                        cpu.tst_cycles(mode, size)
                    } else {
                        legacy
                    }
                }
                0xE if (opcode >> 4) & 0xF == 4 => {
                    // TRAP #n - return sentinel for interception
                    let trap_num = (opcode & 0xF) as u8;
                    TRAP_SENTINEL_BASE + trap_num as i32
                }
                0xE if (opcode & 0xFFF8) == 0x4E50 => {
                    // LINK: 0100 1110 0101 0rrr
                    cpu.exec_link(bus, ea_reg as usize)
                }
                0xE if (opcode & 0xFFF8) == 0x4E58 => {
                    // UNLK: 0100 1110 0101 1rrr
                    cpu.exec_unlk(bus, ea_reg as usize)
                }
                _ if (opcode & 0xFFF8) == 0x4E60 => {
                    // MOVE to USP: 0100 1110 0110 0rrr
                    if cpu.is_supervisor() {
                        let reg = (opcode & 7) as usize;
                        cpu.set_usp(cpu.a(reg));
                        4
                    } else {
                        cpu.exception_privilege(bus)
                    }
                }
                _ if (opcode & 0xFFF8) == 0x4E68 => {
                    // MOVE from USP: 0100 1110 0110 1rrr
                    if cpu.is_supervisor() {
                        let reg = (opcode & 7) as usize;
                        let usp = cpu.get_usp();
                        cpu.set_a(reg, usp);
                        4
                    } else {
                        cpu.exception_privilege(bus)
                    }
                }
                // JSR/JMP/LEA/PEA must be checked BEFORE MOVEM due to bit pattern overlap
                _ if (opcode & 0xFFC0) == 0x4E80 => {
                    // JSR: 0100 1110 10 mmm rrr
                    let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
                    // EA extension words are consumed without prefetching
                    // ahead: the stream is about to be abandoned.
                    cpu.consume_without_prefetch = true;
                    let addr = cpu.get_ea_address(bus, mode, Size::Long);
                    cpu.consume_without_prefetch = false;
                    // Control-flow EA internal clocks before the target
                    // refill (resolve_ea charged 2 for indexed modes already).
                    cpu.internal_cycles(match mode {
                        AddressingMode::Displacement(_)
                        | AddressingMode::AbsoluteShort
                        | AddressingMode::PcDisplacement => 2,
                        AddressingMode::Index(_) | AddressingMode::PcIndex => 4,
                        _ => 0,
                    });
                    cpu.change_of_flow = true;
                    let return_pc = cpu.pc;
                    cpu.pc = addr;
                    // 68000 JSR bus order: first prefetch from the target,
                    // then the return-address push, then the second prefetch.
                    cpu.prefetch_first(bus);
                    cpu.push_32(bus, return_pc);
                    cpu.prefetch_second(bus);
                    if cpu.cpu_type == CpuType::M68000 {
                        16 + cpu.jump_addr_calc_cycles(mode)
                    } else {
                        16
                    }
                }
                _ if (opcode & 0xFFC0) == 0x4EC0 => {
                    // JMP: 0100 1110 11 mmm rrr
                    let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
                    cpu.change_of_flow = true;
                    // EA extension words are consumed without prefetching
                    // ahead: the stream is about to be abandoned.
                    cpu.consume_without_prefetch = true;
                    cpu.pc = cpu.get_ea_address(bus, mode, Size::Long);
                    cpu.consume_without_prefetch = false;
                    // Control-flow EA internal clocks before the target
                    // refill (resolve_ea charged 2 for indexed modes already).
                    cpu.internal_cycles(match mode {
                        AddressingMode::Displacement(_)
                        | AddressingMode::AbsoluteShort
                        | AddressingMode::PcDisplacement => 2,
                        AddressingMode::Index(_) | AddressingMode::PcIndex => 4,
                        _ => 0,
                    });
                    cpu.full_prefetch(bus);
                    if cpu.cpu_type == CpuType::M68000 {
                        8 + cpu.jump_addr_calc_cycles(mode)
                    } else {
                        8
                    }
                }
                _ if (opcode & 0xF1C0) == 0x41C0 => {
                    // LEA: 0100 rrr 111 mmm rrr
                    let reg = ((opcode >> 9) & 7) as usize;
                    let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
                    if mode.is_register_direct() || matches!(mode, AddressingMode::Immediate) {
                        illegal_instruction(cpu, bus)
                    } else {
                        let legacy = cpu.exec_lea(bus, mode, reg);
                        if cpu.cpu_type == CpuType::M68000 {
                            4 + cpu.control_addr_calc_cycles(mode)
                        } else {
                            legacy
                        }
                    }
                }
                _ if (opcode & 0xFFC0) == 0x4840 => {
                    // PEA: 0100 1000 010 mmm rrr
                    let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
                    if mode.is_register_direct() || matches!(mode, AddressingMode::Immediate) {
                        illegal_instruction(cpu, bus)
                    } else {
                        let legacy = cpu.exec_pea(bus, mode);
                        if cpu.cpu_type == CpuType::M68000 {
                            12 + cpu.control_addr_calc_cycles(mode)
                        } else {
                            legacy
                        }
                    }
                }
                // MOVEM after JSR/JMP checks
                // Direction bit is 10: 0=register->memory, 1=memory->register
                _ if (opcode & 0x0400) == 0 && (opcode >> 6) & 3 == 2 && ea_mode >= 2 => {
                    // MOVEM register to memory (word)
                    let mask = cpu.read_imm_16(bus);
                    let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
                    cpu.exec_movem_to_mem(bus, Size::Word, mode, mask)
                }
                _ if (opcode & 0x0400) == 0 && (opcode >> 6) & 3 == 3 && ea_mode >= 2 => {
                    // MOVEM register to memory (long)
                    let mask = cpu.read_imm_16(bus);
                    let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
                    cpu.exec_movem_to_mem(bus, Size::Long, mode, mask)
                }
                _ if (opcode & 0x0400) != 0 && (opcode >> 10) & 3 == 3 && ea_mode >= 2 => {
                    // MOVEM memory to register
                    let mask = cpu.read_imm_16(bus);
                    let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
                    let size = if (opcode >> 6) & 1 == 0 {
                        Size::Word
                    } else {
                        Size::Long
                    };
                    cpu.exec_movem_to_reg(bus, size, mode, mask)
                }
                _ => illegal_instruction(cpu, bus),
            }
        }
    }
}

// ============================================================================
// Group 5: ADDQ/SUBQ/Scc/DBcc
// ============================================================================

fn dispatch_group_5<B: AddressBus>(cpu: &mut CpuCore, bus: &mut B, opcode: u16) -> i32 {
    let size_bits = (opcode >> 6) & 3;
    let ea_mode = ((opcode >> 3) & 7) as u8;
    let ea_reg = (opcode & 7) as u8;

    if size_bits == 3 {
        // 68020+ TRAPcc (conditional trap).
        //
        // Encoding overlaps with Scc when mmm=111, but TRAPcc uses the otherwise-non-alterable
        // PC-relative/immediate submodes in the low 3 bits:
        // - ..FA: TRAPcc.W #<data>  (consume 16-bit operand)
        // - ..FB: TRAPcc.L #<data>  (consume 32-bit operand)
        // - ..FC: TRAPcc            (no operand)
        //
        // Musashi's mc68040 `trapcc.bin` fixture expects TRAPcc to take exception vector 7
        // (same as TRAPV) when the condition is true.
        let is_020_plus = matches!(
            cpu.cpu_type,
            CpuType::M68EC020
                | CpuType::M68020
                | CpuType::M68EC030
                | CpuType::M68030
                | CpuType::M68EC040
                | CpuType::M68LC040
                | CpuType::M68040
        );
        if is_020_plus && ea_mode == 7 && (ea_reg == 2 || ea_reg == 3 || ea_reg == 4) {
            let condition = ((opcode >> 8) & 0xF) as u8;

            // Consume optional operand (reg field encodes size for TRAPcc).
            match ea_reg {
                2 => {
                    let _ = cpu.read_imm_16(bus);
                }
                3 => {
                    let _ = cpu.read_imm_32(bus);
                }
                4 => {}
                _ => {}
            }

            if cpu.test_condition(condition) {
                return cpu.take_exception(bus, 7);
            } else {
                // If not trapping, TRAPcc is effectively a NOP (aside from operand fetch).
                return 4;
            }
        }

        // Scc or DBcc
        let condition = ((opcode >> 8) & 0xF) as u8;
        if ea_mode == 1 {
            // DBcc
            let counter = cpu.d(ea_reg as usize) as u16;
            // The 68000 evaluates the condition and counter before consuming
            // the displacement: on branching paths (cc false) the consume does
            // not prefetch ahead of the to-be-discarded stream.
            let cc_true = cpu.test_condition(condition);
            // Condition/counter-evaluation internal clocks before any bus
            // activity: 4 when the condition is true ("nn np np"), 2 on the
            // branching paths ("n np np").
            cpu.internal_cycles(if cc_true { 4 } else { 2 });
            cpu.consume_without_prefetch = !cc_true;
            // Always fetch the displacement word (even if the branch is not taken) to match
            // 68000 behavior and to correctly trigger address errors on misaligned PC.
            let disp = cpu.read_imm_16(bus) as i16;
            cpu.consume_without_prefetch = false;
            if cpu.run_mode == RUN_MODE_BERR_AERR_RESET {
                return 50;
            }
            if !cc_true {
                let new_counter = counter.wrapping_sub(1);
                cpu.set_d(
                    ea_reg as usize,
                    (cpu.d(ea_reg as usize) & 0xFFFF0000) | new_counter as u32,
                );
                // DBcc displacement is relative to the displacement word (i.e. the PC value
                // *before* reading it). `read_imm_16` advanced PC, so compensate by -2.
                let target = (cpu.pc as i32).wrapping_add(disp as i32 - 2) as u32;
                if new_counter != 0xFFFF {
                    cpu.pc = target;
                    cpu.full_prefetch(bus);
                    // 68000 DBcc taken = 10. On 020+ a taken branch refills the
                    // pipeline; the flat scale alone lands the chip-RAM dbra
                    // loop at 7 clocks/iter where the cycle-exact A1200/FS-UAE
                    // reference measures 8, so pre-scale to 12 (-> 8 after
                    // scale_cycles_for_cpu_type) for the post-020 parts.
                    if cpu.is_pre_68020 { 10 } else { 12 }
                } else {
                    // Counter expired: the 68000 has already begun the branch;
                    // it reads one word at the target (discarded) before
                    // refilling the queue from the fall-through path.
                    if target & 1 == 0 {
                        let masked = cpu.address(target);
                        let _ = bus.read_word(masked);
                    }
                    cpu.full_prefetch(bus);
                    14
                }
            } else {
                12
            }
        } else {
            // Scc
            let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
            let value = if cpu.test_condition(condition) {
                0xFF
            } else {
                0x00
            };
            // 68000 quirk: like CLR, Scc on a memory destination reads the
            // operand before writing.
            let ea = cpu.resolve_ea(bus, mode, Size::Byte);
            if cpu.run_mode == RUN_MODE_BERR_AERR_RESET {
                return 50;
            }
            if cpu.cpu_type == CpuType::M68000 && !mode.is_register_direct() {
                let _ = cpu.read_resolved_ea(bus, ea, Size::Byte);
                if cpu.run_mode == RUN_MODE_BERR_AERR_RESET {
                    return 50;
                }
            }
            cpu.write_resolved_ea(bus, ea, Size::Byte, value);
            if cpu.cpu_type == CpuType::M68000 {
                if mode.is_register_direct() {
                    // Data register: 4 if condition false, 6 if true.
                    if value != 0 { 6 } else { 4 }
                } else {
                    8 + cpu.ea_source_cycles(mode, Size::Byte)
                }
            } else if mode.is_register_direct() {
                4
            } else {
                8
            }
        }
    } else {
        // ADDQ or SUBQ
        let data = ((opcode >> 9) & 7) as u32;
        let data = if data == 0 { 8 } else { data };
        let size = decode_size_00(size_bits);
        let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();

        let legacy = if opcode & 0x100 == 0 {
            cpu.exec_addq(bus, size, data, mode)
        } else {
            cpu.exec_subq(bus, size, data, mode)
        };
        if cpu.cpu_type == CpuType::M68000 {
            cpu.addq_subq_cycles(mode, size)
        } else {
            legacy
        }
    }
}

// ============================================================================
// Group 6: Bcc/BSR/BRA
// ============================================================================

fn dispatch_group_6<B: AddressBus>(cpu: &mut CpuCore, bus: &mut B, opcode: u16) -> i32 {
    let condition = ((opcode >> 8) & 0xF) as u8;
    let displacement = (opcode & 0xFF) as u8;
    // Base is the PC *after the opcode word* (i.e. address of the extension word for .w/.l).
    // This matches 68k branch semantics and keeps short/word/long displacements consistent.
    let base_pc = cpu.pc;

    // The 68000 evaluates the branch condition before consuming the
    // displacement extension word: on taken paths the displacement consume
    // does NOT prefetch ahead (the stream is about to be discarded and
    // refilled from the target); on the not-taken path it prefetches
    // normally. BRA/BSR are always taken.
    let taken = condition < 2 || cpu.test_condition(condition);
    // Condition-evaluation internal clocks before any bus activity:
    // 2 on taken paths ("n np np"), 4 on not-taken ("nn np ...").
    cpu.internal_cycles(if taken { 2 } else { 4 });
    cpu.consume_without_prefetch = taken;
    let disp: i32 = if displacement == 0 {
        cpu.read_imm_16(bus) as i16 as i32
    } else if displacement == 0xFF {
        cpu.read_imm_32(bus) as i32
    } else {
        displacement as i8 as i32
    };
    cpu.consume_without_prefetch = false;

    match condition {
        0 => {
            // BRA
            cpu.change_of_flow = true;
            cpu.pc = (base_pc as i32).wrapping_add(disp) as u32;
            cpu.full_prefetch(bus);
            10
        }
        1 => {
            // BSR
            // Return address is after the displacement extension (cpu.pc already advanced by reads above).
            cpu.change_of_flow = true;
            let return_pc = cpu.pc;
            cpu.pc = (base_pc as i32).wrapping_add(disp) as u32;
            // 68000 BSR bus order: the return-address push happens first,
            // then the two-word refill from the branch target (unlike JSR,
            // which interleaves the push between the two target prefetches).
            cpu.push_32(bus, return_pc);
            cpu.full_prefetch(bus);
            18
        }
        _ => {
            // Bcc
            if taken {
                cpu.change_of_flow = true;
                cpu.pc = (base_pc as i32).wrapping_add(disp) as u32;
                cpu.full_prefetch(bus);
                10
            } else if displacement == 0 {
                12
            } else {
                8
            }
        }
    }
}

// ============================================================================
// Groups 8, 9, B, C, D: Arithmetic/Logic
// ============================================================================

fn dispatch_group_8<B: AddressBus>(cpu: &mut CpuCore, bus: &mut B, opcode: u16) -> i32 {
    let reg = ((opcode >> 9) & 7) as usize;
    let ea_mode = ((opcode >> 3) & 7) as u8;
    let ea_reg = (opcode & 7) as u8;
    let op_mode = (opcode >> 6) & 7;

    match op_mode {
        0..=2 => {
            // OR Dn, <ea>
            let size = decode_size_012(op_mode);
            let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
            let src = cpu.read_ea(bus, mode, size);
            if cpu.run_mode == RUN_MODE_BERR_AERR_RESET {
                return 50;
            }
            let (result, _) = cpu.exec_or(bus, size, src, cpu.d(reg));
            cpu.set_d(reg, (cpu.d(reg) & !size.mask()) | result);
            if cpu.cpu_type == CpuType::M68000 {
                cpu.alu_ea_dn_cycles(mode, size)
            } else {
                4
            }
        }
        4..=6 => {
            // Check for SBCD first (pattern: 1000 xxx1 0000 0yyy for Dn or 1000 xxx1 0000 1yyy for -(An))
            if op_mode == 4 && ea_mode == 0 {
                // SBCD Dy, Dx (register to register)
                cpu.exec_sbcd_rr(ea_reg as usize, reg)
            } else if op_mode == 4 && ea_mode == 1 {
                // SBCD -(Ay), -(Ax) (memory to memory)
                cpu.exec_sbcd_mm(bus, ea_reg as usize, reg)
            } else if op_mode == 5 && (ea_mode == 0 || ea_mode == 1) {
                // PACK (68020+): 1000 xxx1 0100 yrrr
                // y=0: PACK Ds, Dd, #adj  y=1: PACK -(As), -(Ad), #adj
                if cpu.is_pre_68020 {
                    return illegal_instruction(cpu, bus);
                }
                let adj = cpu.read_imm_16(bus);
                if ea_mode == 0 {
                    cpu.exec_pack_rr(ea_reg as usize, reg, adj)
                } else {
                    cpu.exec_pack_mm(bus, ea_reg as usize, reg, adj)
                }
            } else if op_mode == 6 && (ea_mode == 0 || ea_mode == 1) {
                // UNPK (68020+): 1000 xxx1 1000 yrrr
                if cpu.is_pre_68020 {
                    return illegal_instruction(cpu, bus);
                }
                let adj = cpu.read_imm_16(bus);
                if ea_mode == 0 {
                    cpu.exec_unpk_rr(ea_reg as usize, reg, adj)
                } else {
                    cpu.exec_unpk_mm(bus, ea_reg as usize, reg, adj)
                }
            } else {
                // OR Dn, <ea>
                let size = decode_size_012(op_mode - 4);
                let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
                let ea = cpu.resolve_ea(bus, mode, size);
                let dst = cpu.read_resolved_ea(bus, ea, size);
                if cpu.run_mode == RUN_MODE_BERR_AERR_RESET {
                    return 50;
                }
                let (result, _) = cpu.exec_or(bus, size, cpu.d(reg), dst);
                cpu.write_resolved_ea(bus, ea, size, result);
                if cpu.cpu_type == CpuType::M68000 {
                    cpu.alu_dn_ea_cycles(mode, size)
                } else {
                    8
                }
            }
        }
        3 => {
            // DIVU <ea>, Dn
            let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
            cpu.exec_divu(bus, mode, reg)
        }
        7 => {
            // DIVS <ea>, Dn
            let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
            cpu.exec_divs(bus, mode, reg)
        }
        _ => illegal_instruction(cpu, bus),
    }
}

fn dispatch_group_9<B: AddressBus>(cpu: &mut CpuCore, bus: &mut B, opcode: u16) -> i32 {
    let reg = ((opcode >> 9) & 7) as usize;
    let ea_mode = ((opcode >> 3) & 7) as u8;
    let ea_reg = (opcode & 7) as u8;
    let op_mode = (opcode >> 6) & 7;

    match op_mode {
        0..=2 => {
            // SUB <ea>, Dn
            let size = decode_size_012(op_mode);
            let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
            let src = cpu.read_ea(bus, mode, size);
            let dst = cpu.d(reg) & size.mask(); // Mask to operation size
            let (result, _) = cpu.exec_sub(bus, size, src, dst);
            cpu.set_d(reg, (cpu.d(reg) & !size.mask()) | result);
            if cpu.cpu_type == CpuType::M68000 {
                cpu.alu_ea_dn_cycles(mode, size)
            } else {
                4
            }
        }
        3 | 7 => {
            // SUBA
            let size = if op_mode == 3 { Size::Word } else { Size::Long };
            let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
            let src = cpu.read_ea(bus, mode, size);
            let legacy = cpu.exec_suba(bus, size, src, reg);
            if cpu.cpu_type == CpuType::M68000 {
                cpu.adda_suba_cycles(mode, size)
            } else {
                legacy
            }
        }
        4..=6 => {
            // SUB Dn, <ea> or SUBX
            let size = decode_size_012(op_mode - 4);
            if ea_mode == 0 {
                // SUBX Dm, Dn
                let src = cpu.d(ea_reg as usize) & size.mask();
                let dst = cpu.d(reg) & size.mask();
                let result = cpu.exec_subx(size, src, dst);
                cpu.set_d(reg, (cpu.d(reg) & !size.mask()) | result);
                if cpu.cpu_type == CpuType::M68000 && size == Size::Long {
                    8
                } else {
                    4
                }
            } else if ea_mode == 1 {
                // SUBX -(Am), -(An) - predecrement
                // Use proper predecrement semantics (A7 byte alignment) by resolving as -(An).
                let src_ea = cpu.resolve_ea(bus, AddressingMode::PreDecrement(ea_reg), size);
                let dst_ea = cpu.resolve_ea(bus, AddressingMode::PreDecrement(reg as u8), size);

                // The memory-to-memory form's leading internal period is 2
                // clocks total (the two predecrements overlap in microcode);
                // override the per-EA predecrement charges from resolve_ea.
                cpu.pending_sync_clocks = 0;
                cpu.internal_cycles(2);

                // 68000 long memory-to-memory form: predecrement reads go low
                // word first, and the writeback interleaves the final
                // prefetch between the low and high result writes.
                let long_mm_68000 = cpu.cpu_type == CpuType::M68000 && size == Size::Long;

                let src = if long_mm_68000
                    && let EaResult::Memory(sa) = src_ea
                {
                    cpu.read_long_predec_68000(bus, sa)
                } else {
                    cpu.read_resolved_ea(bus, src_ea, size)
                };
                if cpu.run_mode == RUN_MODE_BERR_AERR_RESET {
                    return 50;
                }
                let dst = if long_mm_68000
                    && let EaResult::Memory(da) = dst_ea
                {
                    cpu.read_long_predec_68000(bus, da)
                } else {
                    cpu.read_resolved_ea(bus, dst_ea, size)
                };
                if cpu.run_mode == RUN_MODE_BERR_AERR_RESET {
                    return 50;
                }

                // If the store faults (misaligned word/long), the instruction should not update
                // flags; pre-check alignment to avoid mutating flags before the fault.
                if cpu.cpu_type == CpuType::M68000
                    && size != Size::Byte
                    && let EaResult::Memory(addr) = dst_ea
                    && (addr & 1) != 0
                {
                    cpu.trigger_address_error(bus, addr, true, false);
                    return 50;
                }

                let result = cpu.exec_subx(size, src, dst);
                if long_mm_68000
                    && let EaResult::Memory(da) = dst_ea
                {
                    cpu.write_long_mm_interleaved_68000(bus, da, result);
                } else {
                    cpu.write_resolved_ea(bus, dst_ea, size, result);
                }
                if cpu.run_mode == RUN_MODE_BERR_AERR_RESET {
                    return 50;
                }
                if cpu.cpu_type == CpuType::M68000 && size == Size::Long {
                    30
                } else {
                    18
                }
            } else {
                // SUB Dn, <ea>
                let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
                let src = cpu.d(reg) & size.mask(); // Mask to operation size
                let ea = cpu.resolve_ea(bus, mode, size);
                let dst = cpu.read_resolved_ea(bus, ea, size);
                let (result, _) = cpu.exec_sub(bus, size, src, dst);
                cpu.write_resolved_ea(bus, ea, size, result);
                if cpu.cpu_type == CpuType::M68000 {
                    cpu.alu_dn_ea_cycles(mode, size)
                } else {
                    8
                }
            }
        }
        _ => illegal_instruction(cpu, bus),
    }
}

fn dispatch_group_b<B: AddressBus>(cpu: &mut CpuCore, bus: &mut B, opcode: u16) -> i32 {
    let reg = ((opcode >> 9) & 7) as usize;
    let ea_mode = ((opcode >> 3) & 7) as u8;
    let ea_reg = (opcode & 7) as u8;
    let op_mode = (opcode >> 6) & 7;

    match op_mode {
        0..=2 => {
            // CMP
            let size = decode_size_012(op_mode);
            let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
            let src = cpu.read_ea(bus, mode, size);
            if cpu.run_mode == RUN_MODE_BERR_AERR_RESET {
                return 50;
            }
            let legacy = cpu.exec_cmp(size, src, cpu.d(reg));
            if cpu.cpu_type == CpuType::M68000 {
                cpu.cmp_ea_dn_cycles(mode, size)
            } else {
                legacy
            }
        }
        3 | 7 => {
            // CMPA
            let size = if op_mode == 3 { Size::Word } else { Size::Long };
            let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
            let src = cpu.read_ea(bus, mode, size);
            if cpu.run_mode == RUN_MODE_BERR_AERR_RESET {
                return 50;
            }
            let legacy = cpu.exec_cmpa(size, src, reg);
            if cpu.cpu_type == CpuType::M68000 {
                cpu.cmpa_cycles(mode, size)
            } else {
                legacy
            }
        }
        4..=6 => {
            // EOR or CMPM
            let size = decode_size_012(op_mode - 4);
            if ea_mode == 1 {
                // CMPM (An)+, (Am)+
                // Must read + postincrement in-order (Ay then Ax) so that overlapping regs
                // behave correctly, and so A7 byte inc uses the special +2 rule.
                let src_ea = cpu.resolve_ea(bus, AddressingMode::PostIncrement(ea_reg), size);
                let src_val = cpu.read_resolved_ea(bus, src_ea, size);
                if cpu.run_mode == RUN_MODE_BERR_AERR_RESET {
                    return 50;
                }
                let dst_ea = cpu.resolve_ea(bus, AddressingMode::PostIncrement(reg as u8), size);
                let dst_val = cpu.read_resolved_ea(bus, dst_ea, size);
                if cpu.run_mode == RUN_MODE_BERR_AERR_RESET {
                    return 50;
                }
                let legacy = cpu.exec_cmp(size, src_val, dst_val);
                if cpu.cpu_type == CpuType::M68000 {
                    // CMPM (An)+,(Am)+: 12 byte/word, 20 long.
                    if size == Size::Long {
                        20
                    } else {
                        12
                    }
                } else {
                    legacy
                }
            } else {
                // EOR Dn, <ea>
                let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
                let ea = cpu.resolve_ea(bus, mode, size);
                let dst = cpu.read_resolved_ea(bus, ea, size);
                if cpu.run_mode == RUN_MODE_BERR_AERR_RESET {
                    return 50;
                }
                let result = (cpu.d(reg) ^ dst) & size.mask();
                cpu.write_resolved_ea(bus, ea, size, result);
                if cpu.run_mode == RUN_MODE_BERR_AERR_RESET {
                    return 50;
                }
                cpu.set_logic_flags(result, size);
                if cpu.cpu_type == CpuType::M68000 {
                    cpu.eor_cycles(mode, size)
                } else {
                    8
                }
            }
        }
        _ => illegal_instruction(cpu, bus),
    }
}

fn dispatch_group_c<B: AddressBus>(cpu: &mut CpuCore, bus: &mut B, opcode: u16) -> i32 {
    let reg = ((opcode >> 9) & 7) as usize;
    let ea_mode = ((opcode >> 3) & 7) as u8;
    let ea_reg = (opcode & 7) as u8;
    let op_mode = (opcode >> 6) & 7;

    match op_mode {
        0..=2 => {
            // AND <ea>, Dn
            let size = decode_size_012(op_mode);
            let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
            let src = cpu.read_ea(bus, mode, size);
            let (result, _) = cpu.exec_and(bus, size, src, cpu.d(reg));
            cpu.set_d(reg, (cpu.d(reg) & !size.mask()) | result);
            if cpu.cpu_type == CpuType::M68000 {
                cpu.alu_ea_dn_cycles(mode, size)
            } else {
                4
            }
        }
        4..=6 => {
            // Check for ABCD first (pattern: 1100 xxx1 0000 0yyy for Dn or 1100 xxx1 0000 1yyy for -(An))
            if op_mode == 4 && (ea_mode == 0 || ea_mode == 1) {
                // ABCD
                if ea_mode == 0 {
                    // ABCD Dy, Dx (register to register)
                    cpu.exec_abcd_rr(ea_reg as usize, reg)
                } else {
                    // ABCD -(Ay), -(Ax) (memory to memory)
                    cpu.exec_abcd_mm(bus, ea_reg as usize, reg)
                }
            } else {
                // Check for EXG: mode field (bits 3-7) encodes the exchange type
                let mode_field = (opcode >> 3) & 0x1F;
                if mode_field == 0x08 || mode_field == 0x09 || mode_field == 0x11 {
                    // EXG: 0x08=Dx/Dy, 0x09=Ax/Ay, 0x11=Dx/Ay
                    cpu.exec_exg(opcode)
                } else {
                    // AND Dn, <ea>
                    let size = decode_size_012(op_mode - 4);
                    let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
                    let ea = cpu.resolve_ea(bus, mode, size);
                    let dst = cpu.read_resolved_ea(bus, ea, size);
                    let (result, _) = cpu.exec_and(bus, size, cpu.d(reg), dst);
                    cpu.write_resolved_ea(bus, ea, size, result);
                    if cpu.cpu_type == CpuType::M68000 {
                        cpu.alu_dn_ea_cycles(mode, size)
                    } else {
                        8
                    }
                }
            }
        }
        3 => {
            // MULU <ea>, Dn
            let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
            cpu.exec_mulu(bus, mode, reg)
        }
        7 => {
            // MULS <ea>, Dn
            let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
            cpu.exec_muls(bus, mode, reg)
        }
        _ => illegal_instruction(cpu, bus),
    }
}

fn dispatch_group_d<B: AddressBus>(cpu: &mut CpuCore, bus: &mut B, opcode: u16) -> i32 {
    let reg = ((opcode >> 9) & 7) as usize;
    let ea_mode = ((opcode >> 3) & 7) as u8;
    let ea_reg = (opcode & 7) as u8;
    let op_mode = (opcode >> 6) & 7;

    match op_mode {
        0..=2 => {
            // ADD <ea>, Dn
            let size = decode_size_012(op_mode);
            let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
            let src = cpu.read_ea(bus, mode, size);
            let dst = cpu.d(reg) & size.mask(); // Mask to operation size
            let (result, _) = cpu.exec_add(bus, size, src, dst);
            cpu.set_d(reg, (cpu.d(reg) & !size.mask()) | result);
            if cpu.cpu_type == CpuType::M68000 {
                cpu.alu_ea_dn_cycles(mode, size)
            } else {
                4
            }
        }
        3 | 7 => {
            // ADDA
            let size = if op_mode == 3 { Size::Word } else { Size::Long };
            let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
            let src = cpu.read_ea(bus, mode, size);
            let legacy = cpu.exec_adda(bus, size, src, reg);
            if cpu.cpu_type == CpuType::M68000 {
                cpu.adda_suba_cycles(mode, size)
            } else {
                legacy
            }
        }
        4..=6 => {
            // ADD Dn, <ea> or ADDX
            let size = decode_size_012(op_mode - 4);
            if ea_mode == 0 {
                // ADDX Dm, Dn
                let src = cpu.d(ea_reg as usize) & size.mask();
                let dst = cpu.d(reg) & size.mask();
                let result = cpu.exec_addx(size, src, dst);
                cpu.set_d(reg, (cpu.d(reg) & !size.mask()) | result);
                if cpu.cpu_type == CpuType::M68000 && size == Size::Long {
                    8
                } else {
                    4
                }
            } else if ea_mode == 1 {
                // ADDX -(Am), -(An)
                // Use proper predecrement semantics (A7 byte alignment) by resolving as -(An).
                let src_ea = cpu.resolve_ea(bus, AddressingMode::PreDecrement(ea_reg), size);
                let dst_ea = cpu.resolve_ea(bus, AddressingMode::PreDecrement(reg as u8), size);

                // The memory-to-memory form's leading internal period is 2
                // clocks total (the two predecrements overlap in microcode);
                // override the per-EA predecrement charges from resolve_ea.
                cpu.pending_sync_clocks = 0;
                cpu.internal_cycles(2);

                // 68000 long memory-to-memory form: predecrement reads go low
                // word first, and the writeback interleaves the final
                // prefetch between the low and high result writes.
                let long_mm_68000 = cpu.cpu_type == CpuType::M68000 && size == Size::Long;

                let src = if long_mm_68000
                    && let EaResult::Memory(sa) = src_ea
                {
                    cpu.read_long_predec_68000(bus, sa)
                } else {
                    cpu.read_resolved_ea(bus, src_ea, size)
                };
                if cpu.run_mode == RUN_MODE_BERR_AERR_RESET {
                    return 50;
                }
                let dst = if long_mm_68000
                    && let EaResult::Memory(da) = dst_ea
                {
                    cpu.read_long_predec_68000(bus, da)
                } else {
                    cpu.read_resolved_ea(bus, dst_ea, size)
                };
                if cpu.run_mode == RUN_MODE_BERR_AERR_RESET {
                    return 50;
                }

                // If the store faults (misaligned word/long), the instruction should not update
                // flags; pre-check alignment to avoid mutating flags before the fault.
                if cpu.cpu_type == CpuType::M68000
                    && size != Size::Byte
                    && let EaResult::Memory(addr) = dst_ea
                    && (addr & 1) != 0
                {
                    cpu.trigger_address_error(bus, addr, true, false);
                    return 50;
                }

                let result = cpu.exec_addx(size, src, dst);
                if long_mm_68000
                    && let EaResult::Memory(da) = dst_ea
                {
                    cpu.write_long_mm_interleaved_68000(bus, da, result);
                } else {
                    cpu.write_resolved_ea(bus, dst_ea, size, result);
                }
                if cpu.run_mode == RUN_MODE_BERR_AERR_RESET {
                    return 50;
                }
                if cpu.cpu_type == CpuType::M68000 && size == Size::Long {
                    30
                } else {
                    18
                }
            } else {
                // ADD Dn, <ea>
                let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
                let src = cpu.d(reg) & size.mask(); // Mask to operation size
                let ea = cpu.resolve_ea(bus, mode, size);
                let dst = cpu.read_resolved_ea(bus, ea, size);
                let (result, _) = cpu.exec_add(bus, size, src, dst);
                cpu.write_resolved_ea(bus, ea, size, result);
                if cpu.cpu_type == CpuType::M68000 {
                    cpu.alu_dn_ea_cycles(mode, size)
                } else {
                    8
                }
            }
        }
        _ => illegal_instruction(cpu, bus),
    }
}

// ============================================================================
// Group E: Shift/Rotate
// ============================================================================

fn dispatch_group_e<B: AddressBus>(cpu: &mut CpuCore, bus: &mut B, opcode: u16) -> i32 {
    let ea_mode = ((opcode >> 3) & 7) as u8;
    let ea_reg = (opcode & 7) as u8;

    // 68020+ bitfield instructions live in group E with bits 7..6 == 11 and op selector 0x8..0xF.
    // Example: BFCHG (0xEAF9), BFTST (0xE8F9), BFINS (0xEFF9), etc.
    if (opcode & 0x00C0) == 0x00C0 && ((opcode >> 8) & 0xF) >= 0x8 {
        if cpu.is_pre_68020 {
            return illegal_instruction(cpu, bus);
        }
        return cpu.exec_bitfield(bus, opcode);
    }

    if (opcode >> 6) & 3 == 3 {
        // Memory shift/rotate (always word size)
        let mode = AddressingMode::decode(ea_mode, ea_reg).unwrap();
        // Resolve EA once: postinc/predec have side effects and must not be applied twice.
        let ea = cpu.resolve_ea(bus, mode, Size::Word);
        let value = cpu.read_resolved_ea(bus, ea, Size::Word);
        if cpu.run_mode == RUN_MODE_BERR_AERR_RESET {
            // Address/bus error while fetching the operand: exception has been taken.
            return 50;
        }
        let op = (opcode >> 9) & 7;
        let direction = (opcode >> 8) & 1;

        let (result, cycles) = match (op, direction) {
            (0, 0) => cpu.exec_asr(Size::Word, 1, value),
            (0, 1) => cpu.exec_asl(Size::Word, 1, value),
            (1, 0) => cpu.exec_lsr(Size::Word, 1, value),
            (1, 1) => cpu.exec_lsl(Size::Word, 1, value),
            (2, 0) => cpu.exec_roxr(Size::Word, 1, value),
            (2, 1) => cpu.exec_roxl(Size::Word, 1, value),
            (3, 0) => cpu.exec_ror(Size::Word, 1, value),
            (3, 1) => cpu.exec_rol(Size::Word, 1, value),
            _ => return illegal_instruction(cpu, bus),
        };
        cpu.write_resolved_ea(bus, ea, Size::Word, result);
        // MC68000 memory shift/rotate (always 1 bit, word): 8 + EA.
        if cpu.cpu_type == CpuType::M68000 {
            8 + cpu.ea_source_cycles(mode, Size::Word)
        } else {
            cycles + 4
        }
    } else {
        // Register shift/rotate
        let size = decode_size_00((opcode >> 6) & 3);
        let count_or_reg = ((opcode >> 9) & 7) as usize;
        let shift = if opcode & 0x20 != 0 {
            cpu.d(count_or_reg) & 63
        } else {
            let c = count_or_reg as u32;
            if c == 0 { 8 } else { c }
        };
        let reg = ea_reg as usize;
        let value = cpu.d(reg) & size.mask();
        let direction = (opcode >> 8) & 1;
        let op = (opcode >> 3) & 3;

        let (result, cycles) = match (op, direction) {
            (0, 0) => cpu.exec_asr(size, shift, value),
            (0, 1) => cpu.exec_asl(size, shift, value),
            (1, 0) => cpu.exec_lsr(size, shift, value),
            (1, 1) => cpu.exec_lsl(size, shift, value),
            (2, 0) => cpu.exec_roxr(size, shift, value),
            (2, 1) => cpu.exec_roxl(size, shift, value),
            (3, 0) => cpu.exec_ror(size, shift, value),
            (3, 1) => cpu.exec_rol(size, shift, value),
            _ => return illegal_instruction(cpu, bus),
        };
        cpu.set_d(reg, (cpu.d(reg) & !size.mask()) | result);
        cycles
    }
}

// ============================================================================
// Helpers
// ============================================================================

fn decode_size_00(bits: u16) -> Size {
    match bits {
        0 => Size::Byte,
        1 => Size::Word,
        2 => Size::Long,
        _ => Size::Byte,
    }
}

fn decode_size_012(bits: u16) -> Size {
    match bits {
        0 => Size::Byte,
        1 => Size::Word,
        2 => Size::Long,
        _ => Size::Long,
    }
}

fn read_immediate<B: AddressBus>(cpu: &mut CpuCore, bus: &mut B, size: Size) -> u32 {
    match size {
        Size::Byte => cpu.read_imm_16(bus) as u32 & 0xFF,
        Size::Word => cpu.read_imm_16(bus) as u32,
        Size::Long => cpu.read_imm_32(bus),
    }
}

/// Return sentinel for illegal instruction interception.
/// This function is called for undefined opcodes that don't match any pattern.
fn illegal_instruction<B: AddressBus>(_cpu: &mut CpuCore, _bus: &mut B) -> i32 {
    ILLEGAL_SENTINEL
}

/// Return sentinel value for A-line trap interception.
/// The caller (dispatch_instruction) converts this to StepResult::AlineTrap.
fn exception_1010(_cpu: &mut CpuCore, _opcode: u16) -> i32 {
    // Return sentinel to signal A-line interception
    super::decode::ALINE_TRAP_SENTINEL
}

/// Return sentinel value for F-line trap interception.
/// The caller (dispatch_instruction) converts this to StepResult::FlineTrap.
fn exception_1111(_cpu: &mut CpuCore, _opcode: u16) -> i32 {
    // Return sentinel to signal F-line interception
    super::decode::FLINE_TRAP_SENTINEL
}
