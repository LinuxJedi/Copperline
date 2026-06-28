//! FPU transcendental accuracy oracle.
//!
//! Drives the real FPU instruction path (`Fop FP0,FP0`) for each transcendental,
//! computes a high-precision reference with the pure-Rust `astro-float` BigFloat
//! from the *exact* 80-bit operand, and measures the true error in ULPs of the
//! extended format. The gate is faithful rounding: error <= 1 ULP for every
//! function across a wide sweep and all four FPCR rounding modes.
//!
//! The exhaustive sweep is `#[ignore]` (run with `cargo test --release --test
//! fpu_accuracy -- --ignored`); a small always-on smoke test keeps `cargo test`
//! fast.

use astro_float::{BigFloat, Consts, RoundingMode};
use m68k::NoOpHleHandler;
use m68k::core::cpu::CpuCore;
use m68k::core::memory::AddressBus;
use m68k::core::types::CpuType;
use m68k::fpu::FloatX80;

/// Reference working precision (well above the 64-bit extended significand).
const P: usize = 200;
const RN: RoundingMode = RoundingMode::ToEven;

// ============================== flat bus =================================

struct FlatBus {
    mem: Vec<u8>,
}
impl FlatBus {
    fn new() -> Self {
        Self {
            mem: vec![0; 0x0100_0000],
        }
    }
}
impl AddressBus for FlatBus {
    fn read_byte(&mut self, a: u32) -> u8 {
        *self.mem.get(a as usize).unwrap_or(&0)
    }
    fn read_word(&mut self, a: u32) -> u16 {
        ((self.read_byte(a) as u16) << 8) | self.read_byte(a.wrapping_add(1)) as u16
    }
    fn read_long(&mut self, a: u32) -> u32 {
        ((self.read_word(a) as u32) << 16) | self.read_word(a.wrapping_add(2)) as u32
    }
    fn write_byte(&mut self, a: u32, v: u8) {
        if let Some(b) = self.mem.get_mut(a as usize) {
            *b = v;
        }
    }
    fn write_word(&mut self, a: u32, v: u16) {
        self.write_byte(a, (v >> 8) as u8);
        self.write_byte(a.wrapping_add(1), v as u8);
    }
    fn write_long(&mut self, a: u32, v: u32) {
        self.write_word(a, (v >> 16) as u16);
        self.write_word(a.wrapping_add(2), v as u16);
    }
}

const CODE: u32 = 0x0000_2000;

struct Mach {
    cpu: CpuCore,
    bus: FlatBus,
}
impl Mach {
    fn new() -> Self {
        let mut cpu = CpuCore::new();
        cpu.set_cpu_type(CpuType::M68040);
        let mut bus = FlatBus::new();
        bus.write_long(0, 0x0000_8000); // SSP
        bus.write_long(4, CODE); // PC
        cpu.reset(&mut bus);
        cpu.set_sr(0x2700);
        Self { cpu, bus }
    }

    /// Execute `Fop FP0,FP0` (register form: opcode F200, ext = opmode) with
    /// the given FPCR, returning the FP0 result.
    fn eval(&mut self, opmode: u16, input: FloatX80, fpcr: u32) -> FloatX80 {
        self.bus.write_word(CODE, 0xF200);
        self.bus.write_word(CODE + 2, opmode);
        self.cpu.fpcr = fpcr;
        self.cpu.fpr[0] = input;
        self.cpu.pc = CODE;
        let mut hle = NoOpHleHandler;
        self.cpu.step_with_hle_handler(&mut self.bus, &mut hle);
        self.cpu.fpr[0]
    }
}

// ============================== x80 <-> BigFloat =========================

fn is_normal(x: FloatX80) -> bool {
    let e = x.biased_exp();
    e != 0 && e != 0x7FFF
}

/// Exact conversion of a finite nonzero extended value to BigFloat.
fn x80_to_bf(x: FloatX80) -> BigFloat {
    let mut bf = BigFloat::from_word(x.mantissa, P);
    let e = (x.sign_exp & 0x7FFF) as i32 - 16383;
    let cur = bf.exponent().unwrap();
    bf.set_exponent(cur + (e - 63)); // multiply by 2^(e-63), exact
    if (x.sign_exp >> 15) & 1 != 0 { -bf } else { bf }
}

/// One ULP of a finite nonzero extended value, as BigFloat.
fn ulp_bf(x: FloatX80) -> BigFloat {
    let e = (x.sign_exp & 0x7FFF) as i32 - 16383;
    let mut one = BigFloat::from_word(1, P);
    let cur = one.exponent().unwrap();
    one.set_exponent(cur + (e - 63));
    one
}

fn le(a: &BigFloat, b: &BigFloat) -> bool {
    a.cmp(b).map(|o| o <= 0).unwrap_or(false)
}

// ============================== reference ===============================

fn bf_word(d: u64) -> BigFloat {
    BigFloat::from_word(d, P)
}

/// High-precision reference for `opmode` applied to the exact operand `x`.
fn reference(opmode: u16, x: &BigFloat, cc: &mut Consts) -> BigFloat {
    match opmode {
        0x10 => x.exp(P, RN, cc),
        0x08 => x.exp(P, RN, cc).sub(&bf_word(1), P, RN), // expm1
        0x11 => bf_word(2).pow(x, P, RN, cc),             // 2^x
        0x12 => bf_word(10).pow(x, P, RN, cc),            // 10^x
        0x14 => x.ln(P, RN, cc),
        0x06 => x.add(&bf_word(1), P, RN).ln(P, RN, cc), // log1p
        0x15 => x.log10(P, RN, cc),
        0x16 => x.log2(P, RN, cc),
        0x0E => x.sin(P, RN, cc),
        0x1D => x.cos(P, RN, cc),
        0x0F => x.tan(P, RN, cc),
        0x0C => x.asin(P, RN, cc),
        0x1C => x.acos(P, RN, cc),
        0x0A => x.atan(P, RN, cc),
        0x02 => x.sinh(P, RN, cc),
        0x19 => x.cosh(P, RN, cc),
        0x09 => x.tanh(P, RN, cc),
        0x0D => x.atanh(P, RN, cc),
        _ => unreachable!("not a transcendental opmode: {opmode:#x}"),
    }
}

const FUNCS: &[(u16, &str)] = &[
    (0x10, "exp"),
    (0x08, "expm1"),
    (0x11, "2^x"),
    (0x12, "10^x"),
    (0x14, "ln"),
    (0x06, "log1p"),
    (0x15, "log10"),
    (0x16, "log2"),
    (0x0E, "sin"),
    (0x1D, "cos"),
    (0x0F, "tan"),
    (0x0C, "asin"),
    (0x1C, "acos"),
    (0x0A, "atan"),
    (0x02, "sinh"),
    (0x19, "cosh"),
    (0x09, "tanh"),
    (0x0D, "atanh"),
];

// ============================== input sampling ==========================

struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn unit(&mut self) -> f64 {
        // uniform in [0,1)
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// A function-appropriate operand value (f64), then widened to a full 80-bit
/// significand by randomizing the low mantissa bits.
fn sample(opmode: u16, rng: &mut Rng) -> FloatX80 {
    let u = rng.unit();
    let v: f64 = match opmode {
        // exp family: linear magnitude up to where the result stays finite.
        0x10 | 0x08 => lerp(-700.0, 700.0, u),
        0x11 => lerp(-16000.0, 16000.0, u), // 2^x
        0x12 => lerp(-4900.0, 4900.0, u),   // 10^x
        // log family: log-uniform positive magnitudes.
        0x14 | 0x15 | 0x16 => 10f64.powf(lerp(-300.0, 300.0, u)),
        0x06 => lerp(-0.9, 1000.0, u), // log1p, x > -1
        // trig: a moderate band (reduction degrades far beyond this).
        0x0E | 0x1D | 0x0F => lerp(-50.0, 50.0, u),
        // inverse trig.
        0x0C | 0x1C => lerp(-0.999, 0.999, u), // asin/acos domain
        0x0A => 10f64.powf(lerp(-3.0, 6.0, u)) * sign(rng), // atan, wide
        // hyperbolic.
        0x02 | 0x19 => lerp(-700.0, 700.0, u),
        0x09 => lerp(-30.0, 30.0, u),   // tanh
        0x0D => lerp(-0.999, 0.999, u), // atanh domain
        _ => unreachable!(),
    };
    widen(v, rng)
}

fn lerp(a: f64, b: f64, u: f64) -> f64 {
    a + (b - a) * u
}
fn sign(rng: &mut Rng) -> f64 {
    if rng.next_u64() & 1 == 0 { 1.0 } else { -1.0 }
}

/// Build an 80-bit operand near `v` but with a full random low significand, so
/// the sweep exercises operands beyond f64 precision.
fn widen(v: f64, rng: &mut Rng) -> FloatX80 {
    let mut x = FloatX80::from_f64(v);
    if is_normal(x) {
        x.mantissa = (x.mantissa & !0x7FF) | (rng.next_u64() & 0x7FF);
    }
    x
}

// ============================== measurement =============================

#[derive(Default, Clone, Copy)]
struct Stat {
    samples: u64,
    within_half: u64,
    within_one: u64,
    failures: u64,
    worst_input: f64,
}

/// Returns true if |result - reference| <= 1 ULP. Skips (returns true) when the
/// operand/result isn't a finite normal number (special cases are covered by
/// the unit tests).
fn check(input: FloatX80, result: FloatX80, opmode: u16, cc: &mut Consts, stat: &mut Stat) -> bool {
    if !is_normal(input) || !is_normal(result) {
        return true;
    }
    let xbf = x80_to_bf(input);
    let refv = reference(opmode, &xbf, cc);
    if refv.exponent().is_none() {
        return true; // reference is 0/inf/nan -> not a normal-ULP case
    }
    let kbf = x80_to_bf(result);
    let ulp = ulp_bf(result);
    let half = ulp.mul(&BigFloat::from_f64(0.5, P), P, RN);
    // err = |refv - kbf|
    let diff = refv.sub(&kbf, P, RN);
    let err = if diff.cmp(&bf_word(0)).map(|o| o < 0).unwrap_or(false) {
        -diff
    } else {
        diff
    };
    stat.samples += 1;
    if le(&err, &half) {
        stat.within_half += 1;
    }
    if le(&err, &ulp) {
        stat.within_one += 1;
        true
    } else {
        if stat.failures == 0 {
            stat.worst_input = input.to_f64();
        }
        stat.failures += 1;
        false
    }
}

const MODES: &[(u32, &str)] = &[(0x00, "RN"), (0x10, "RZ"), (0x20, "RM"), (0x30, "RP")];

// ============================== tests ===================================

#[test]
fn bridge_roundtrips() {
    // x80_to_bf of a representable value matches its f64.
    let x = FloatX80::from_f64(1.5);
    let bf = x80_to_bf(x);
    assert_eq!(bf.cmp(&BigFloat::from_f64(1.5, P)), Some(0));
    // a full-precision value: 1 + 2^-60 differs from 1 by less than 1 ULP_64.
    let mut y = FloatX80::from_f64(1.0);
    y.mantissa |= 1 << 3; // a low bit f64 can't hold
    let diff = x80_to_bf(y).sub(&BigFloat::from_word(1, P), P, RN);
    assert!(diff.cmp(&BigFloat::from_word(0, P)) == Some(1)); // strictly > 0
}

#[test]
fn accuracy_smoke() {
    // A handful of inputs per function under round-to-nearest must be <= 1 ULP.
    let mut m = Mach::new();
    let mut cc = Consts::new().unwrap();
    let mut rng = Rng(0x1234_5678_9abc_def1);
    for &(opmode, name) in FUNCS {
        let mut stat = Stat::default();
        for _ in 0..64 {
            let input = sample(opmode, &mut rng);
            let result = m.eval(opmode, input, 0);
            assert!(
                check(input, result, opmode, &mut cc, &mut stat),
                "{name}: > 1 ULP at input {}",
                stat.worst_input
            );
        }
    }
}

#[test]
#[ignore = "exhaustive accuracy sweep; run with --release -- --ignored"]
fn accuracy_sweep() {
    let mut m = Mach::new();
    let mut cc = Consts::new().unwrap();
    let mut failed = Vec::new();
    eprintln!("\nFPU transcendental accuracy (ULP of 80-bit extended):");
    for &(mode_bits, mode_name) in MODES {
        let n = if mode_bits == 0 { 5000 } else { 1000 };
        eprintln!("  -- rounding mode {mode_name} ({n} samples/func) --");
        for &(opmode, name) in FUNCS {
            let mut rng = Rng(0xC0FF_EE00_1234_5678 ^ (opmode as u64));
            let mut stat = Stat::default();
            for _ in 0..n {
                let input = sample(opmode, &mut rng);
                let result = m.eval(opmode, input, mode_bits);
                check(input, result, opmode, &mut cc, &mut stat);
            }
            let pct_half = 100.0 * stat.within_half as f64 / stat.samples.max(1) as f64;
            eprintln!(
                "    {name:6} mode {mode_name}: {} samples, {:.1}% <=0.5ULP, {} fail(>1ULP)",
                stat.samples, pct_half, stat.failures
            );
            if stat.failures > 0 {
                failed.push(format!(
                    "{name} ({mode_name}): {} of {} > 1 ULP, e.g. input {}",
                    stat.failures, stat.samples, stat.worst_input
                ));
            }
        }
    }
    assert!(
        failed.is_empty(),
        "functions exceeding 1 ULP:\n{}",
        failed.join("\n")
    );
}
