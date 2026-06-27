//! FPU memory/immediate-source operation tests (68040 / 6888x).
//!
//! Regression coverage for the general-instruction `Fcc <ea>,FPn` form
//! (extension-word R/M = 1). The monadic ops (FNEG/FABS/FTST/FINT/FINTRZ/
//! FSQRT and the transcendentals) used to be implemented only for the
//! register-source form, so a memory- or immediate-source operand fell
//! through to the `_ => 0` opmode arm and raised a Line-F exception. The
//! bebbo `m68k-amigaos-gcc -m68881` toolchain emits these forms routinely
//! (`fneg.d (a5),fp0`, `ftst.d (a5)`, `fintrz.d d0,fp0`, ...), so a
//! `-m68881` binary faulted at startup. Both source forms now share one
//! opmode-dispatch table.

use m68k::NoOpHleHandler;
use m68k::core::cpu::CpuCore;
use m68k::core::memory::AddressBus;
use m68k::core::types::CpuType;
use m68k::fpu::FloatX80;

/// Full 32-bit address space backed by a flat 16MB low region (mirrors the
/// local harness in scc68070_tests.rs; the FPU programs all live below 16MB).
struct FlatBus {
    mem: Vec<u8>,
}

impl FlatBus {
    fn new() -> Self {
        Self {
            mem: vec![0; 0x0100_0000], // 16MB
        }
    }
}

impl AddressBus for FlatBus {
    fn read_byte(&mut self, address: u32) -> u8 {
        let a = address as usize;
        if a < self.mem.len() { self.mem[a] } else { 0 }
    }

    fn read_word(&mut self, address: u32) -> u16 {
        let hi = self.read_byte(address) as u16;
        let lo = self.read_byte(address.wrapping_add(1)) as u16;
        (hi << 8) | lo
    }

    fn read_long(&mut self, address: u32) -> u32 {
        let hi = self.read_word(address) as u32;
        let lo = self.read_word(address.wrapping_add(2)) as u32;
        (hi << 16) | lo
    }

    fn write_byte(&mut self, address: u32, value: u8) {
        let a = address as usize;
        if a < self.mem.len() {
            self.mem[a] = value;
        }
    }

    fn write_word(&mut self, address: u32, value: u16) {
        self.write_byte(address, (value >> 8) as u8);
        self.write_byte(address.wrapping_add(1), value as u8);
    }

    fn write_long(&mut self, address: u32, value: u32) {
        self.write_word(address, (value >> 16) as u16);
        self.write_word(address.wrapping_add(2), value as u16);
    }
}

const CODE: u32 = 0x0000_2000;
const DATA: u32 = 0x0000_3000;
const STACK: u32 = 0x0000_8000;
const FLINE_HANDLER: u32 = 0x0000_4000;
/// Marker the Line-F handler drops into D7 if the instruction was rejected.
const FLINE_SENTINEL: u32 = 0x0BAD_F00D;

/// FPSR condition-code bits.
const FPCC_N: u32 = 0x0800_0000;
const FPCC_Z: u32 = 0x0400_0000;

/// Build a reset 68040 with a Line-F handler installed. The handler writes
/// FLINE_SENTINEL into D7 and STOPs, so a rejected FPU opcode is detectable.
fn new_machine() -> (CpuCore, FlatBus) {
    let mut cpu = CpuCore::new();
    cpu.set_cpu_type(CpuType::M68040);
    let mut bus = FlatBus::new();

    bus.write_long(0, STACK); // SSP
    bus.write_long(4, CODE); // initial PC
    bus.write_long(11 * 4, FLINE_HANDLER); // Line-F vector (11)

    // Line-F handler: MOVE.L #FLINE_SENTINEL,D7 ; STOP #$2700
    bus.write_word(FLINE_HANDLER, 0x2E3C);
    bus.write_long(FLINE_HANDLER.wrapping_add(2), FLINE_SENTINEL);
    bus.write_word(FLINE_HANDLER.wrapping_add(6), 0x4E72);
    bus.write_word(FLINE_HANDLER.wrapping_add(8), 0x2700);

    cpu.reset(&mut bus);
    (cpu, bus)
}

fn write_f64(bus: &mut FlatBus, addr: u32, value: f64) {
    let bits = value.to_bits();
    bus.write_long(addr, (bits >> 32) as u32);
    bus.write_long(addr.wrapping_add(4), bits as u32);
}

/// Run until the CPU STOPs (or a step budget is exhausted).
fn run(cpu: &mut CpuCore, bus: &mut FlatBus) {
    let mut hle = NoOpHleHandler;
    for _ in 0..64 {
        if cpu.is_stopped() {
            break;
        }
        cpu.step_with_hle_handler(bus, &mut hle);
    }
}

fn assert_no_fline(cpu: &CpuCore) {
    assert_ne!(
        cpu.dar[7], FLINE_SENTINEL,
        "FPU instruction wrongly raised a Line-F exception"
    );
}

/// Compare an extended FPU register against an exactly-representable f64.
fn assert_fpr_eq(actual: FloatX80, expected: f64) {
    assert_eq!(
        actual.to_f64(),
        expected,
        "fpr {actual:?} (= {}) != {expected}",
        actual.to_f64()
    );
}

/// Build the extension word for a memory/immediate-source general FPU op:
/// R/M = 1, source format `fmt` (bits 12-10), dest FP reg (bits 9-7), opmode.
fn ext(fmt: u16, dst: u16, opmode: u16) -> u16 {
    0x4000 | (fmt << 10) | (dst << 7) | opmode
}

const FMT_LONG: u16 = 0;
const FMT_DOUBLE: u16 = 5;

/// Place `FOP.D (A0),FPn` at CODE followed by STOP, with A0 -> DATA and the
/// source double preloaded, then run.
fn run_double_mem_op(opmode: u16, dst: u16, operand: f64) -> (CpuCore, FlatBus) {
    let (mut cpu, mut bus) = new_machine();
    // F-line opcode: 1111 001 000 mmm rrr; (A0) = mode 2, reg 0.
    bus.write_word(CODE, 0xF200 | (2 << 3));
    bus.write_word(CODE.wrapping_add(2), ext(FMT_DOUBLE, dst, opmode));
    bus.write_word(CODE.wrapping_add(4), 0x4E72); // STOP #$2700
    bus.write_word(CODE.wrapping_add(6), 0x2700);

    write_f64(&mut bus, DATA, operand);
    cpu.dar[8] = DATA; // A0
    run(&mut cpu, &mut bus);
    (cpu, bus)
}

#[test]
fn fpu_mem_source_fneg_double() {
    // fneg.d (a0),fp0 : -3.0 -> 3.0
    let (cpu, _) = run_double_mem_op(0x1A, 0, -3.0);
    assert_no_fline(&cpu);
    assert_fpr_eq(cpu.fpr[0], 3.0);
}

#[test]
fn fpu_mem_source_fabs_double() {
    // fabs.d (a0),fp0 : -3.0 -> 3.0
    let (cpu, _) = run_double_mem_op(0x18, 0, -3.0);
    assert_no_fline(&cpu);
    assert_fpr_eq(cpu.fpr[0], 3.0);
}

#[test]
fn fpu_mem_source_ftst_double_sets_zero() {
    // ftst.d (a0) on 0.0 sets the FPSR Z bit and leaves FP0 untouched.
    let (mut cpu, mut bus) = new_machine();
    bus.write_word(CODE, 0xF200 | (2 << 3));
    bus.write_word(CODE.wrapping_add(2), ext(FMT_DOUBLE, 0, 0x3A));
    bus.write_word(CODE.wrapping_add(4), 0x4E72);
    bus.write_word(CODE.wrapping_add(6), 0x2700);
    write_f64(&mut bus, DATA, 0.0);
    cpu.dar[8] = DATA;
    cpu.fpr[0] = FloatX80::from_f64(123.0); // FTST must not write the destination.
    run(&mut cpu, &mut bus);

    assert_no_fline(&cpu);
    assert_fpr_eq(cpu.fpr[0], 123.0); // FTST must not modify FP0
    assert_ne!(cpu.fpsr & FPCC_Z, 0, "FPSR Z should be set for 0.0");
    assert_eq!(cpu.fpsr & FPCC_N, 0, "FPSR N should be clear for +0.0");
}

#[test]
fn fpu_mem_source_fintrz_double() {
    // fintrz.d (a0),fp0 : 3.7 -> 3.0 (round toward zero)
    let (cpu, _) = run_double_mem_op(0x03, 0, 3.7);
    assert_no_fline(&cpu);
    assert_fpr_eq(cpu.fpr[0], 3.0);
}

#[test]
fn fpu_mem_source_fsqrt_double() {
    // fsqrt.d (a0),fp0 : 16.0 -> 4.0
    let (cpu, _) = run_double_mem_op(0x04, 0, 16.0);
    assert_no_fline(&cpu);
    assert_fpr_eq(cpu.fpr[0], 4.0);
}

#[test]
fn fpu_mem_source_fmove_double_still_works() {
    // Regression guard: the previously-handled fmove.d (a0),fp0 still works
    // now that it routes through the shared dispatch helper.
    let (cpu, _) = run_double_mem_op(0x00, 0, 2.5);
    assert_no_fline(&cpu);
    assert_fpr_eq(cpu.fpr[0], 2.5);
}

#[test]
fn fpu_imm_source_monadic() {
    // fneg.l #5,fp0 : exercises the immediate (#<data>) source path through
    // the unified helper. Long immediate 5 -> 5.0 -> negate -> -5.0.
    let (mut cpu, mut bus) = new_machine();
    // F-line opcode with immediate EA: mode 7, reg 4.
    bus.write_word(CODE, 0xF200 | (7 << 3) | 4);
    bus.write_word(CODE.wrapping_add(2), ext(FMT_LONG, 0, 0x1A));
    bus.write_long(CODE.wrapping_add(4), 5); // immediate long operand
    bus.write_word(CODE.wrapping_add(8), 0x4E72); // STOP #$2700
    bus.write_word(CODE.wrapping_add(10), 0x2700);
    run(&mut cpu, &mut bus);

    assert_no_fline(&cpu);
    assert_fpr_eq(cpu.fpr[0], -5.0);
}
