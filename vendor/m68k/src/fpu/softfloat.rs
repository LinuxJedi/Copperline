//! Pure-Rust 80-bit extended-precision (floatx80) software FPU engine.
//!
//! Operates purely on [`FloatX80`] values plus a [`RoundCtx`] (rounding mode +
//! precision) and an [`ExcFlags`] out-accumulator. No `CpuCore` dependency, so
//! it is unit-testable in isolation. The glue in `operations.rs` decodes the
//! instruction, calls these functions, and folds the returned flags into FPSR.
//!
//! Internal representation: a finite value is carried as a 128-bit significand
//! `m` with its leading integer bit normalized to bit 127, and an exponent
//! `e` such that the value magnitude is `m * 2^(e - 127)`. The low 64 bits of
//! `m` are guard/round/sticky room. [`round_pack`] is the single normalize +
//! round + pack path every arithmetic op funnels through.

use super::types::FloatX80;

/// Exponent bias of the 80-bit extended format.
const BIAS: i32 = 16383;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RoundMode {
    Nearest,
    Zero,
    NegInf,
    PosInf,
}

impl RoundMode {
    /// Decode FPCR bits 5:4.
    pub fn from_fpcr(fpcr: u32) -> Self {
        match (fpcr >> 4) & 3 {
            1 => RoundMode::Zero,
            2 => RoundMode::NegInf,
            3 => RoundMode::PosInf,
            _ => RoundMode::Nearest,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Precision {
    Extended,
    Single,
    Double,
}

impl Precision {
    /// Decode FPCR bits 7:6 (rounding precision).
    pub fn from_fpcr(fpcr: u32) -> Self {
        match (fpcr >> 6) & 3 {
            1 => Precision::Single,
            2 => Precision::Double,
            _ => Precision::Extended,
        }
    }

    /// Number of significand bits kept (including the integer bit).
    const fn bits(self) -> u32 {
        match self {
            Precision::Extended => 64,
            Precision::Double => 53,
            Precision::Single => 24,
        }
    }
}

#[derive(Clone, Copy)]
pub struct RoundCtx {
    pub mode: RoundMode,
    pub prec: Precision,
}

impl RoundCtx {
    /// Round-to-nearest, extended precision (the internal default).
    pub const NEAREST_EXT: RoundCtx = RoundCtx {
        mode: RoundMode::Nearest,
        prec: Precision::Extended,
    };
}

/// FPU exception accumulator. Bit layout matches the FPSR exception-status
/// byte shifted down to bits 7:0 (BSUN=7, SNAN=6, OPERR=5, OVFL=4, UNFL=3,
/// DZ=2, INEX2=1, INEX1=0).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct ExcFlags(pub u8);

impl ExcFlags {
    // BSUN and INEX1 are produced by the glue (FBcc / packed decimal), not by
    // the value engine itself.
    pub const BSUN: u8 = 0x80;
    pub const SNAN: u8 = 0x40;
    pub const OPERR: u8 = 0x20;
    pub const OVFL: u8 = 0x10;
    pub const UNFL: u8 = 0x08;
    pub const DZ: u8 = 0x04;
    pub const INEX2: u8 = 0x02;
    pub const INEX1: u8 = 0x01;

    #[inline]
    pub fn raise(&mut self, bits: u8) {
        self.0 |= bits;
    }

    #[inline]
    pub fn has(self, bits: u8) -> bool {
        self.0 & bits != 0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FpCmp {
    Less,
    Equal,
    Greater,
    Unordered,
}

// ============================== constructors ==============================

const fn one(sign: bool) -> FloatX80 {
    FloatX80 {
        sign_exp: ((sign as u16) << 15) | (BIAS as u16),
        mantissa: 0x8000_0000_0000_0000,
    }
}

// ============================== unpacking =================================

/// A finite, nonzero operand decomposed for arithmetic: sign, the exponent of
/// the integer bit (so magnitude ~ 2^exp), and a normalized 64-bit significand
/// with the integer bit at bit 63. Denormals are normalized (exp goes below
/// the format minimum, which round_pack re-denormalizes on output).
struct Unpacked {
    sign: bool,
    exp: i32,
    mant: u64,
}

fn unpack(x: FloatX80) -> Unpacked {
    let sign = x.sign();
    let biased = x.biased_exp();
    if biased == 0 {
        // Denormal: normalize so the integer bit reaches bit 63.
        let lz = x.mantissa.leading_zeros();
        Unpacked {
            sign,
            exp: (1 - BIAS) - lz as i32,
            mant: x.mantissa << lz,
        }
    } else {
        Unpacked {
            sign,
            exp: biased as i32 - BIAS,
            mant: x.mantissa,
        }
    }
}

// ============================== round + pack ==============================

/// Shift `m` right by `sh`, OR-ing any bits shifted out into bit 0 (sticky).
fn shift_right_sticky(m: u128, sh: u32) -> u128 {
    if sh == 0 {
        m
    } else if sh >= 128 {
        u128::from(m != 0)
    } else {
        let lost = m & ((1u128 << sh) - 1);
        (m >> sh) | u128::from(lost != 0)
    }
}

fn overflow_result(sign: bool, mode: RoundMode) -> FloatX80 {
    let to_inf = match mode {
        RoundMode::Nearest => true,
        RoundMode::Zero => false,
        RoundMode::PosInf => !sign,
        RoundMode::NegInf => sign,
    };
    if to_inf {
        FloatX80::infinity(sign)
    } else {
        // Largest finite magnitude.
        FloatX80 {
            sign_exp: ((sign as u16) << 15) | 0x7FFE,
            mantissa: 0xFFFF_FFFF_FFFF_FFFF,
        }
    }
}

/// Normalize, round to `ctx.prec`, and pack a finite result. `m` is a 128-bit
/// significand and `exp` is the exponent of bit 127 (value = m * 2^(exp-127)).
fn round_pack(sign: bool, mut exp: i32, mut m: u128, ctx: RoundCtx, f: &mut ExcFlags) -> FloatX80 {
    if m == 0 {
        return FloatX80::zero(sign);
    }
    // Normalize the integer bit up to bit 127.
    let lz = m.leading_zeros();
    m <<= lz;
    exp -= lz as i32;

    let mut biased = exp + BIAS;

    // Denormalize if the exponent is below the format minimum (biased < 1).
    if biased < 1 {
        let sh = (1 - biased) as u32;
        m = shift_right_sticky(m, sh);
        biased = 1;
    }

    let p = ctx.prec.bits();
    let shift = 128 - p; // discarded low bits
    let q = (m >> shift) as u64; // p significant bits, integer bit at bit p-1
    let rem = m & ((1u128 << shift) - 1);
    let halfway = 1u128 << (shift - 1);

    let round_up = match ctx.mode {
        RoundMode::Nearest => rem > halfway || (rem == halfway && (q & 1) == 1),
        RoundMode::Zero => false,
        RoundMode::PosInf => !sign && rem != 0,
        RoundMode::NegInf => sign && rem != 0,
    };
    if rem != 0 {
        f.raise(ExcFlags::INEX2);
    }

    let mut q = q as u128;
    if round_up {
        q += 1;
        if (q >> p) != 0 {
            // Carried out of the significand: renormalize.
            q >>= 1;
            biased += 1;
        }
    }

    let integer_bit_set = (q >> (p - 1)) & 1 != 0;
    if !integer_bit_set {
        // Result is subnormal (or zero).
        if q == 0 {
            return FloatX80::zero(sign);
        }
        if rem != 0 {
            f.raise(ExcFlags::UNFL);
        }
        let mantissa = (q as u64) << (64 - p);
        return FloatX80 {
            sign_exp: (sign as u16) << 15,
            mantissa,
        };
    }

    if biased >= 0x7FFF {
        f.raise(ExcFlags::OVFL);
        f.raise(ExcFlags::INEX2);
        return overflow_result(sign, ctx.mode);
    }

    let mantissa = (q as u64) << (64 - p);
    FloatX80 {
        sign_exp: ((sign as u16) << 15) | (biased as u16),
        mantissa,
    }
}

// ============================== NaN handling ==============================

fn propagate_nan(a: FloatX80, b: FloatX80, f: &mut ExcFlags) -> FloatX80 {
    if a.is_signaling_nan() || b.is_signaling_nan() {
        f.raise(ExcFlags::SNAN);
        f.raise(ExcFlags::OPERR);
    }
    if a.is_nan() { a.quiet() } else { b.quiet() }
}

// ============================== sign ops ==================================

pub fn neg(a: FloatX80) -> FloatX80 {
    FloatX80 {
        sign_exp: a.sign_exp ^ 0x8000,
        mantissa: a.mantissa,
    }
}

pub fn abs(a: FloatX80) -> FloatX80 {
    FloatX80 {
        sign_exp: a.sign_exp & 0x7FFF,
        mantissa: a.mantissa,
    }
}

// ============================== add / sub ================================

pub fn add(a: FloatX80, b: FloatX80, ctx: RoundCtx, f: &mut ExcFlags) -> FloatX80 {
    if a.is_nan() || b.is_nan() {
        return propagate_nan(a, b, f);
    }
    if a.is_inf() || b.is_inf() {
        if a.is_inf() && b.is_inf() && a.sign() != b.sign() {
            f.raise(ExcFlags::OPERR);
            return FloatX80::default_nan();
        }
        return if a.is_inf() { a } else { b };
    }
    if a.is_zero() && b.is_zero() {
        // -0 only when both are -0, or in round-to-minus-infinity.
        let sign = (a.sign() && b.sign()) || ctx.mode == RoundMode::NegInf;
        return FloatX80::zero(sign);
    }
    if a.is_zero() {
        return round_pack(b.sign(), b_exp_hi(b).0, b_exp_hi(b).1, ctx, f);
    }
    if b.is_zero() {
        return round_pack(a.sign(), b_exp_hi(a).0, b_exp_hi(a).1, ctx, f);
    }

    let ua = unpack(a);
    let ub = unpack(b);
    // Significands with integer bit at bit 127.
    let (hs, he, hm, ls, _le, lm) = if ua.exp >= ub.exp {
        (
            ua.sign,
            ua.exp,
            (ua.mant as u128) << 64,
            ub.sign,
            ub.exp,
            (ub.mant as u128) << 64,
        )
    } else {
        (
            ub.sign,
            ub.exp,
            (ub.mant as u128) << 64,
            ua.sign,
            ua.exp,
            (ua.mant as u128) << 64,
        )
    };
    let diff = (he - if ua.exp >= ub.exp { ub.exp } else { ua.exp }) as u32;
    let lm = shift_right_sticky(lm, diff);

    if hs == ls {
        let (sum, carry) = hm.overflowing_add(lm);
        if carry {
            let sticky = sum & 1;
            round_pack(hs, he + 1, (sum >> 1) | (1u128 << 127) | sticky, ctx, f)
        } else {
            round_pack(hs, he, sum, ctx, f)
        }
    } else if hm >= lm {
        let m = hm - lm;
        if m == 0 {
            return FloatX80::zero(ctx.mode == RoundMode::NegInf);
        }
        round_pack(hs, he, m, ctx, f)
    } else {
        round_pack(ls, he, lm - hm, ctx, f)
    }
}

/// Return (exp-of-bit-127, significand<<64) for a finite operand.
fn b_exp_hi(x: FloatX80) -> (i32, u128) {
    let u = unpack(x);
    (u.exp, (u.mant as u128) << 64)
}

pub fn sub(a: FloatX80, b: FloatX80, ctx: RoundCtx, f: &mut ExcFlags) -> FloatX80 {
    add(a, neg(b), ctx, f)
}

// ============================== mul / div ================================

pub fn mul(a: FloatX80, b: FloatX80, ctx: RoundCtx, f: &mut ExcFlags) -> FloatX80 {
    if a.is_nan() || b.is_nan() {
        return propagate_nan(a, b, f);
    }
    let sign = a.sign() ^ b.sign();
    if a.is_inf() || b.is_inf() {
        if a.is_zero() || b.is_zero() {
            f.raise(ExcFlags::OPERR);
            return FloatX80::default_nan();
        }
        return FloatX80::infinity(sign);
    }
    if a.is_zero() || b.is_zero() {
        return FloatX80::zero(sign);
    }
    let ua = unpack(a);
    let ub = unpack(b);
    let prod = (ua.mant as u128) * (ub.mant as u128);
    // value = prod * 2^((ua.exp-63)+(ub.exp-63)); round_pack wants exp of bit127.
    round_pack(sign, ua.exp + ub.exp + 1, prod, ctx, f)
}

pub fn div(a: FloatX80, b: FloatX80, ctx: RoundCtx, f: &mut ExcFlags) -> FloatX80 {
    if a.is_nan() || b.is_nan() {
        return propagate_nan(a, b, f);
    }
    let sign = a.sign() ^ b.sign();
    if a.is_inf() {
        if b.is_inf() {
            f.raise(ExcFlags::OPERR);
            return FloatX80::default_nan();
        }
        return FloatX80::infinity(sign);
    }
    if b.is_inf() {
        return FloatX80::zero(sign);
    }
    if b.is_zero() {
        if a.is_zero() {
            f.raise(ExcFlags::OPERR);
            return FloatX80::default_nan();
        }
        f.raise(ExcFlags::DZ);
        return FloatX80::infinity(sign);
    }
    if a.is_zero() {
        return FloatX80::zero(sign);
    }
    let ua = unpack(a);
    let ub = unpack(b);
    // q = floor((mant_a / mant_b) * 2^64): the ratio with 64 fraction bits.
    let num = (ua.mant as u128) << 64;
    let q = num / (ub.mant as u128);
    let rem = num % (ub.mant as u128);
    // Place the ratio's integer bit at bit 127 so the 64 fraction bits become
    // the guard/round window; carry a sticky from the division remainder.
    let mut m = q << 63;
    if rem != 0 {
        m |= 1;
    }
    round_pack(sign, ua.exp - ub.exp, m, ctx, f)
}

// ============================== sqrt =====================================

/// Floor integer square root of a u128, returning (root, remainder).
fn isqrt128(n: u128) -> (u128, u128) {
    if n == 0 {
        return (0, 0);
    }
    let mut x = n;
    let mut c: u128 = 0;
    // Highest power of four <= n.
    let mut d: u128 = 1u128 << (126 - (n.leading_zeros() & !1));
    while d > x {
        d >>= 2;
    }
    while d != 0 {
        if x >= c + d {
            x -= c + d;
            c = (c >> 1) + d;
        } else {
            c >>= 1;
        }
        d >>= 2;
    }
    (c, x)
}

pub fn sqrt(a: FloatX80, ctx: RoundCtx, f: &mut ExcFlags) -> FloatX80 {
    if a.is_nan() {
        return propagate_nan(a, a, f);
    }
    if a.is_zero() {
        return FloatX80::zero(a.sign()); // sqrt(-0) = -0
    }
    if a.sign() {
        f.raise(ExcFlags::OPERR); // sqrt of a negative
        return FloatX80::default_nan();
    }
    if a.is_inf() {
        return a; // +inf
    }
    let ua = unpack(a);
    let e = ua.exp - 63; // value = mant * 2^e, mant in [2^63, 2^64)
    // Choose a shift s in {63, 64} so that (e - s) is even and the radicand
    // mant<<s lands in [2^126, 2^128) -> root in [2^63, 2^64).
    let (s, big_e) = if (e & 1) == 0 {
        (64u32, e - 64)
    } else {
        (63u32, e - 63)
    };
    let radicand = (ua.mant as u128) << s;
    let (root, remb) = isqrt128(radicand);
    let mut m = root << 64; // integer bit at bit 127
    if remb != 0 {
        m |= 1; // sticky
    }
    // value = root * 2^(big_e/2); round_pack wants exp of bit127 = big_e/2 + 63.
    round_pack(false, big_e / 2 + 63, m, ctx, f)
}

// ============================== constant ROM =============================

/// 10^n in extended precision (exponentiation by squaring).
fn pow10_x80(n: u32) -> FloatX80 {
    let mut result = from_u64(1, false);
    let mut base = from_u64(10, false);
    let mut e = n;
    let mut f = ExcFlags::default();
    while e != 0 {
        if e & 1 != 0 {
            result = mul(result, base, RoundCtx::NEAREST_EXT, &mut f);
        }
        e >>= 1;
        if e != 0 {
            base = mul(base, base, RoundCtx::NEAREST_EXT, &mut f);
        }
    }
    result
}

/// FMOVECR on-chip constant ROM. The irrational constants use the exact
/// 80-bit MC68881 ROM bit patterns; the powers of ten are computed in
/// extended precision (so 10^512..10^4096 are finite, unlike an f64 ROM).
pub fn const_rom(offset: usize) -> FloatX80 {
    let bits = |se: u16, m: u64| FloatX80 {
        sign_exp: se,
        mantissa: m,
    };
    match offset {
        0x00 => bits(0x4000, 0xC90F_DAA2_2168_C235), // pi
        0x0B => bits(0x3FFD, 0x9A20_9A84_FBCF_F799), // log10(2)
        0x0C => bits(0x4000, 0xADF8_5458_A2BB_4A9A), // e
        0x0D => bits(0x3FFF, 0xB8AA_3B29_5C17_F0BC), // log2(e)
        0x0E => bits(0x3FFD, 0xDE5B_D8A9_3728_7195), // log10(e)
        0x0F => FloatX80::zero(false),               // 0.0
        0x30 => bits(0x3FFE, 0xB172_17F7_D1CF_79AC), // ln(2)
        0x31 => bits(0x4000, 0x935D_8DDD_AAA8_AC17), // ln(10)
        0x32 => bits(0x3FFF, 0x8000_0000_0000_0000), // 1.0 (10^0)
        // 10^1, 10^2, 10^4, 10^8, ... 10^4096.
        0x33..=0x3F => {
            const EXPS: [u32; 13] = [1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096];
            pow10_x80(EXPS[offset - 0x33])
        }
        _ => FloatX80::zero(false),
    }
}

// ============================== int <-> x80 ==============================

/// Build an exact extended value from a u64 magnitude and a sign.
pub fn from_u64(v: u64, sign: bool) -> FloatX80 {
    if v == 0 {
        return FloatX80::zero(sign);
    }
    round_pack(
        sign,
        63,
        (v as u128) << 64,
        RoundCtx::NEAREST_EXT,
        &mut ExcFlags::default(),
    )
}

/// Round `x` to an integer in `[min, max]`, saturating on overflow/NaN/Inf
/// (raising OPERR) and raising INEX on a fractional discard.
pub fn to_i64(x: FloatX80, mode: RoundMode, min: i64, max: i64, f: &mut ExcFlags) -> i64 {
    if x.is_nan() {
        f.raise(ExcFlags::OPERR);
        return min;
    }
    if x.is_inf() {
        f.raise(ExcFlags::OPERR);
        return if x.sign() { min } else { max };
    }
    let r = round_to_int(x, mode, f);
    if r.is_zero() {
        return 0;
    }
    let u = unpack(r);
    if u.exp > 63 {
        f.raise(ExcFlags::OPERR);
        return if u.sign { min } else { max };
    }
    let mag = (u.mant >> (63 - u.exp)) as u128;
    let val = if u.sign { -(mag as i128) } else { mag as i128 };
    if val < min as i128 {
        f.raise(ExcFlags::OPERR);
        min
    } else if val > max as i128 {
        f.raise(ExcFlags::OPERR);
        max
    } else {
        val as i64
    }
}

// ============================== compare ==================================

/// Compare magnitudes of two finite/inf operands as (biased exp, mantissa).
fn cmp_mag(a: FloatX80, b: FloatX80) -> core::cmp::Ordering {
    (a.biased_exp(), a.mantissa).cmp(&(b.biased_exp(), b.mantissa))
}

pub fn compare(a: FloatX80, b: FloatX80, f: &mut ExcFlags) -> FpCmp {
    if a.is_nan() || b.is_nan() {
        if a.is_signaling_nan() || b.is_signaling_nan() {
            f.raise(ExcFlags::SNAN);
            f.raise(ExcFlags::OPERR);
        }
        return FpCmp::Unordered;
    }
    if a.is_zero() && b.is_zero() {
        return FpCmp::Equal; // +0 == -0
    }
    let asign = a.sign();
    let bsign = b.sign();
    if asign != bsign {
        return if asign { FpCmp::Less } else { FpCmp::Greater };
    }
    // Same sign: order by magnitude, then flip for negatives.
    let ord = cmp_mag(a, b);
    use core::cmp::Ordering::*;
    match (ord, asign) {
        (Equal, _) => FpCmp::Equal,
        (Greater, false) | (Less, true) => FpCmp::Greater,
        _ => FpCmp::Less,
    }
}

// ============================== round to int =============================

pub fn round_to_int(a: FloatX80, mode: RoundMode, f: &mut ExcFlags) -> FloatX80 {
    if a.is_nan() {
        return propagate_nan(a, a, f);
    }
    if a.is_inf() || a.is_zero() {
        return a;
    }
    let ua = unpack(a);
    if ua.exp >= 63 {
        return a; // no fractional bits in the 64-bit significand
    }
    let neg = ua.sign;
    if ua.exp < 0 {
        // |value| < 1: result is 0 or +/-1.
        let inc = match mode {
            // exp == -1 => value in [0.5, 1); >0.5 rounds to 1, 0.5 ties to even (0).
            RoundMode::Nearest => ua.exp == -1 && ua.mant > (1u64 << 63),
            RoundMode::Zero => false,
            RoundMode::PosInf => !neg,
            RoundMode::NegInf => neg,
        };
        f.raise(ExcFlags::INEX2);
        return if inc { one(neg) } else { FloatX80::zero(neg) };
    }
    let fbits = (63 - ua.exp) as u32; // 1..=63 fractional bits
    let int_part = ua.mant >> fbits;
    let frac = ua.mant & ((1u64 << fbits) - 1);
    if frac != 0 {
        f.raise(ExcFlags::INEX2);
    }
    let half = 1u64 << (fbits - 1);
    let inc = match mode {
        RoundMode::Nearest => frac > half || (frac == half && (int_part & 1) == 1),
        RoundMode::Zero => false,
        RoundMode::PosInf => !neg && frac != 0,
        RoundMode::NegInf => neg && frac != 0,
    };
    let int_part = int_part + u64::from(inc);
    if int_part == 0 {
        return FloatX80::zero(neg);
    }
    // The integer value is exact: build it with round-to-nearest (no rounding
    // happens since it fits in <= 64 bits).
    round_pack(
        neg,
        63,
        (int_part as u128) << 64,
        RoundCtx::NEAREST_EXT,
        &mut ExcFlags::default(),
    )
}

// ============================== misc unary ===============================

/// FSCALE: a * 2^n (n already extracted as a truncated integer).
pub fn scale(a: FloatX80, n: i32, ctx: RoundCtx, f: &mut ExcFlags) -> FloatX80 {
    if a.is_nan() {
        return propagate_nan(a, a, f);
    }
    if a.is_inf() || a.is_zero() {
        return a;
    }
    let ua = unpack(a);
    round_pack(
        ua.sign,
        ua.exp.saturating_add(n),
        (ua.mant as u128) << 64,
        ctx,
        f,
    )
}

/// FGETEXP: the unbiased exponent as an integer-valued extended number.
pub fn getexp(a: FloatX80, f: &mut ExcFlags) -> FloatX80 {
    if a.is_nan() {
        return propagate_nan(a, a, f);
    }
    if a.is_inf() {
        f.raise(ExcFlags::OPERR);
        return FloatX80::default_nan();
    }
    if a.is_zero() {
        return a;
    }
    let ua = unpack(a);
    let e = ua.exp as i64;
    let sign = e < 0;
    let mag = e.unsigned_abs();
    if mag == 0 {
        return FloatX80::zero(false);
    }
    round_pack(
        sign,
        63,
        (mag as u128) << 64,
        RoundCtx::NEAREST_EXT,
        &mut ExcFlags::default(),
    )
}

/// FGETMAN: the mantissa as a value in [1, 2) with the sign of `a`.
pub fn getman(a: FloatX80, f: &mut ExcFlags) -> FloatX80 {
    if a.is_nan() {
        return propagate_nan(a, a, f);
    }
    if a.is_inf() {
        f.raise(ExcFlags::OPERR);
        return FloatX80::default_nan();
    }
    if a.is_zero() {
        return a;
    }
    let ua = unpack(a);
    FloatX80 {
        sign_exp: ((ua.sign as u16) << 15) | (BIAS as u16),
        mantissa: ua.mant,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn x(v: f64) -> FloatX80 {
        FloatX80::from_f64(v)
    }
    fn rn() -> RoundCtx {
        RoundCtx::NEAREST_EXT
    }

    /// For exactly-representable operands the extended result, narrowed to
    /// f64, must equal the f64 computation.
    #[test]
    fn arithmetic_matches_f64_for_exact_values() {
        let mut f = ExcFlags::default();
        let cases = [
            (3.0_f64, 4.0_f64),
            (1.5, 0.25),
            (-2.0, 8.0),
            (100.0, 0.0625),
            (-7.5, -0.5),
        ];
        for (a, b) in cases {
            assert_eq!(add(x(a), x(b), rn(), &mut f).to_f64(), a + b, "{a}+{b}");
            assert_eq!(sub(x(a), x(b), rn(), &mut f).to_f64(), a - b, "{a}-{b}");
            assert_eq!(mul(x(a), x(b), rn(), &mut f).to_f64(), a * b, "{a}*{b}");
            if b != 0.0 {
                assert_eq!(div(x(a), x(b), rn(), &mut f).to_f64(), a / b, "{a}/{b}");
            }
        }
    }

    #[test]
    fn sqrt_exact_squares() {
        let mut f = ExcFlags::default();
        for v in [1.0_f64, 4.0, 9.0, 16.0, 100.0, 0.25, 2.5 * 2.5] {
            assert_eq!(sqrt(x(v), rn(), &mut f).to_f64(), v.sqrt(), "sqrt {v}");
        }
    }

    #[test]
    fn sqrt_two_is_more_precise_than_f64() {
        // sqrt(2) to 64-bit extended: 1.6A09E667F3BCC908 (integer bit + frac).
        let mut f = ExcFlags::default();
        let r = sqrt(x(2.0), rn(), &mut f);
        assert_eq!(r.biased_exp(), BIAS as u16); // exponent 0 -> [1,2)
        assert_eq!(r.mantissa, 0xB504_F333_F9DE_6484);
        assert!(f.has(ExcFlags::INEX2));
    }

    #[test]
    fn add_extended_precision_beyond_f64() {
        // 1.0 + 2^-60: f64 has only 52 fraction bits so it collapses to 1.0,
        // but extended's 63 fraction bits retain it.
        let mut f = ExcFlags::default();
        let tiny = FloatX80 {
            sign_exp: (BIAS - 60) as u16,
            mantissa: 0x8000_0000_0000_0000,
        };
        let r = add(one(false), tiny, rn(), &mut f);
        assert_ne!(r.mantissa, 0x8000_0000_0000_0000); // not just 1.0
        assert_eq!(r.to_f64(), 1.0); // but rounds back to 1.0 in f64
    }

    #[test]
    fn compare_inf_and_nan() {
        let mut f = ExcFlags::default();
        let inf = FloatX80::infinity(false);
        assert_eq!(compare(inf, inf, &mut f), FpCmp::Equal); // fixes Inf==Inf
        assert_eq!(
            compare(FloatX80::default_nan(), x(1.0), &mut f),
            FpCmp::Unordered
        );
        assert_eq!(compare(x(-0.0), x(0.0), &mut f), FpCmp::Equal);
        assert_eq!(compare(x(-1.0), x(1.0), &mut f), FpCmp::Less);
        assert_eq!(compare(x(2.0), x(1.0), &mut f), FpCmp::Greater);
        assert_eq!(compare(x(-2.0), x(-1.0), &mut f), FpCmp::Less);
    }

    #[test]
    fn div_by_zero_and_operr() {
        let mut f = ExcFlags::default();
        let r = div(x(1.0), x(0.0), rn(), &mut f);
        assert!(r.is_inf() && !r.sign());
        assert!(f.has(ExcFlags::DZ));

        let mut f = ExcFlags::default();
        let r = div(x(0.0), x(0.0), rn(), &mut f);
        assert!(r.is_nan());
        assert!(f.has(ExcFlags::OPERR));

        let mut f = ExcFlags::default();
        let r = add(
            FloatX80::infinity(false),
            FloatX80::infinity(true),
            rn(),
            &mut f,
        );
        assert!(r.is_nan());
        assert!(f.has(ExcFlags::OPERR));
    }

    #[test]
    fn round_to_int_modes() {
        let mut f = ExcFlags::default();
        assert_eq!(round_to_int(x(1.5), RoundMode::Zero, &mut f).to_f64(), 1.0);
        assert_eq!(
            round_to_int(x(1.5), RoundMode::NegInf, &mut f).to_f64(),
            1.0
        );
        assert_eq!(
            round_to_int(x(1.5), RoundMode::PosInf, &mut f).to_f64(),
            2.0
        );
        assert_eq!(
            round_to_int(x(1.5), RoundMode::Nearest, &mut f).to_f64(),
            2.0
        );
        assert_eq!(
            round_to_int(x(2.5), RoundMode::Nearest, &mut f).to_f64(),
            2.0
        ); // ties to even
        assert_eq!(
            round_to_int(x(-1.5), RoundMode::Zero, &mut f).to_f64(),
            -1.0
        );
        assert_eq!(
            round_to_int(x(0.4), RoundMode::Nearest, &mut f).to_f64(),
            0.0
        );
        assert_eq!(
            round_to_int(x(0.5), RoundMode::Nearest, &mut f).to_f64(),
            0.0
        ); // ties to even
        assert_eq!(
            round_to_int(x(0.6), RoundMode::Nearest, &mut f).to_f64(),
            1.0
        );
    }

    #[test]
    fn scale_getexp_getman() {
        let mut f = ExcFlags::default();
        assert_eq!(scale(x(3.0), 4, rn(), &mut f).to_f64(), 48.0);
        assert_eq!(scale(x(3.0), -1, rn(), &mut f).to_f64(), 1.5);
        assert_eq!(getexp(x(8.0), &mut f).to_f64(), 3.0);
        assert_eq!(getexp(x(0.5), &mut f).to_f64(), -1.0);
        assert_eq!(getman(x(6.0), &mut f).to_f64(), 1.5);
        assert_eq!(getman(x(-6.0), &mut f).to_f64(), -1.5);
    }

    #[test]
    fn const_rom_values() {
        // pi is the exact 80-bit ROM pattern.
        let pi = const_rom(0x00);
        assert_eq!(pi.sign_exp, 0x4000);
        assert_eq!(pi.mantissa, 0xC90F_DAA2_2168_C235);
        // 0x0D is log2(e) (~1.4427), not ln(2) as the old f64 table had it.
        assert!((const_rom(0x0D).to_f64() - std::f64::consts::LOG2_E).abs() < 1e-15);
        // Powers of ten.
        assert_eq!(const_rom(0x32).to_f64(), 1.0);
        assert_eq!(const_rom(0x34).to_f64(), 100.0);
        // 10^512 is finite in extended (an f64 ROM returned infinity here).
        assert!(const_rom(0x3C).to_f64().is_infinite()); // 10^512 > f64 max, finite in x80
        assert!(!const_rom(0x3C).is_inf()); // ... but the extended value is finite
    }

    #[test]
    fn rounding_mode_directions() {
        // 1/3 is inexact; check the rounded extended significand differs by mode.
        let mut f = ExcFlags::default();
        let down = div(
            x(1.0),
            x(3.0),
            RoundCtx {
                mode: RoundMode::Zero,
                prec: Precision::Extended,
            },
            &mut f,
        );
        let up = div(
            x(1.0),
            x(3.0),
            RoundCtx {
                mode: RoundMode::PosInf,
                prec: Precision::Extended,
            },
            &mut f,
        );
        assert!(f.has(ExcFlags::INEX2));
        assert_eq!(up.mantissa, down.mantissa + 1);
    }
}
