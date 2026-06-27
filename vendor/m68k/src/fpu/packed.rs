//! 68881/68882 packed-decimal real format (FMOVE formats 3 and 7).
//!
//! The 96-bit packed-decimal real is three big-endian longwords:
//!   L0: SM(31) SE(30) yy(29-28) e2(27-24) e1(23-20) e0(19-16) ... D0(3-0)
//!   L1: 8 BCD fraction digits (D1..D8), most significant in bits 31-28
//!   L2: 8 BCD fraction digits (D9..D16)
//! The value is  (-1)^SM * D0.D1D2...D16 * 10^((-1)^SE * e2e1e0).
//! An exponent field of $FFF encodes Inf (zero mantissa) or NaN.
//!
//! Decimal<->binary scaling goes through the floatx80 engine (powers of ten
//! built by exponentiation-by-squaring); the leading decimal exponent for the
//! binary->decimal direction is estimated with an f64 log10. This is the
//! 6888x "round to a decimal string" behavior, which is inherently inexact;
//! it is not bit-accurate to a real chip's internal guard-digit algorithm.

use super::softfloat::{self, ExcFlags, RoundCtx, RoundMode};
use super::types::FloatX80;

/// Largest decimal exponent the 3-digit field can hold.
const MAX_DECIMAL_EXP: i32 = 999;

fn pow10(n: u32, f: &mut ExcFlags) -> FloatX80 {
    // Exponentiation by squaring with base 10.
    let mut result = softfloat::from_u64(1, false);
    let mut base = softfloat::from_u64(10, false);
    let mut e = n;
    while e != 0 {
        if e & 1 != 0 {
            result = softfloat::mul(result, base, RoundCtx::NEAREST_EXT, f);
        }
        e >>= 1;
        if e != 0 {
            base = softfloat::mul(base, base, RoundCtx::NEAREST_EXT, f);
        }
    }
    result
}

/// Multiply `x` by 10^p (p may be negative).
fn mul_pow10(x: FloatX80, p: i32, f: &mut ExcFlags) -> FloatX80 {
    if p == 0 {
        return x;
    }
    let tp = pow10(p.unsigned_abs(), f);
    if p > 0 {
        softfloat::mul(x, tp, RoundCtx::NEAREST_EXT, f)
    } else {
        softfloat::div(x, tp, RoundCtx::NEAREST_EXT, f)
    }
}

/// Decode a packed-decimal real (12 bytes, big-endian) into an extended value.
pub fn from_packed(bytes: [u8; 12], f: &mut ExcFlags) -> FloatX80 {
    let w0 = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let w1 = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    let w2 = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);

    let sm = (w0 >> 31) & 1 != 0;
    let se = (w0 >> 30) & 1 != 0;
    let exp_field = (w0 >> 16) & 0xFFF;

    if exp_field == 0xFFF {
        // Inf (zero mantissa) or NaN.
        let mant_zero = (w0 & 0xF) == 0 && w1 == 0 && w2 == 0;
        return if mant_zero {
            FloatX80::infinity(sm)
        } else {
            FloatX80::default_nan()
        };
    }

    let e2 = (w0 >> 24) & 0xF;
    let e1 = (w0 >> 20) & 0xF;
    let e0 = (w0 >> 16) & 0xF;
    let exp = (e2 * 100 + e1 * 10 + e0) as i32;
    let exp = if se { -exp } else { exp };

    // Assemble the 17-digit integer D0 D1 ... D16 (fits in u64: < 10^17).
    let mut m: u64 = (w0 & 0xF) as u64;
    for shift in (0..32).step_by(4).rev() {
        m = m * 10 + ((w1 >> shift) & 0xF) as u64;
    }
    for shift in (0..32).step_by(4).rev() {
        m = m * 10 + ((w2 >> shift) & 0xF) as u64;
    }

    if m == 0 {
        return FloatX80::zero(sm);
    }
    // value = m * 10^(exp - 16) (m carries 16 fraction digits).
    let base = softfloat::from_u64(m, sm);
    mul_pow10(base, exp - 16, f)
}

fn pack_special(sign: bool, nan: bool) -> [u8; 12] {
    let mut out = [0u8; 12];
    let w0: u32 = ((sign as u32) << 31) | 0x0FFF_0000 | if nan { 0xF } else { 0 };
    out[0..4].copy_from_slice(&w0.to_be_bytes());
    if nan {
        out[4..8].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
    }
    out
}

/// 17 BCD digits (D0..D16) + sign + 3-digit exponent into the 12-byte format.
fn pack_value(sign_m: bool, exp: i32, digits: &[u8; 17]) -> [u8; 12] {
    let se = exp < 0;
    let ea = exp.unsigned_abs().min(MAX_DECIMAL_EXP as u32);
    let (e2, e1, e0) = (ea / 100 % 10, ea / 10 % 10, ea % 10);

    let mut w0: u32 = ((sign_m as u32) << 31) | ((se as u32) << 30);
    w0 |= e2 << 24 | e1 << 20 | e0 << 16;
    w0 |= digits[0] as u32 & 0xF;

    let mut w1: u32 = 0;
    for (i, d) in digits[1..9].iter().enumerate() {
        w1 |= (*d as u32 & 0xF) << (28 - i * 4);
    }
    let mut w2: u32 = 0;
    for (i, d) in digits[9..17].iter().enumerate() {
        w2 |= (*d as u32 & 0xF) << (28 - i * 4);
    }

    let mut out = [0u8; 12];
    out[0..4].copy_from_slice(&w0.to_be_bytes());
    out[4..8].copy_from_slice(&w1.to_be_bytes());
    out[8..12].copy_from_slice(&w2.to_be_bytes());
    out
}

/// Round the 17 digits to `sig` significant digits (1..=17), propagating any
/// carry into the exponent; raises INEX1 if nonzero digits are discarded.
fn round_to_sig(digits: &mut [u8; 17], exp: &mut i32, sig: usize, f: &mut ExcFlags) {
    let sig = sig.clamp(1, 17);
    if sig >= 17 {
        return;
    }
    if digits[sig..].iter().any(|&d| d != 0) {
        f.raise(ExcFlags::INEX1);
    }
    let round_up = digits[sig] >= 5;
    for d in digits[sig..].iter_mut() {
        *d = 0;
    }
    if round_up {
        let mut i = sig;
        loop {
            if i == 0 {
                // Carry past the leading digit: shift right, bump exponent.
                for j in (1..17).rev() {
                    digits[j] = digits[j - 1];
                }
                digits[0] = 1;
                *exp += 1;
                break;
            }
            i -= 1;
            if digits[i] == 9 {
                digits[i] = 0;
            } else {
                digits[i] += 1;
                break;
            }
        }
    }
}

/// Encode an extended value as a packed-decimal real. `k` is the FMOVE
/// k-factor: k > 0 selects k significant digits (1..17); k <= 0 selects -k
/// digits to the right of the decimal point.
pub fn to_packed(x: FloatX80, k: i8, f: &mut ExcFlags) -> [u8; 12] {
    let sign = x.sign();
    if x.is_nan() {
        return pack_special(sign, true);
    }
    if x.is_inf() {
        return pack_special(sign, false);
    }
    if x.is_zero() {
        return pack_value(sign, 0, &[0u8; 17]);
    }

    // Leading decimal exponent estimate, then scale to a 17-digit integer.
    let mag = softfloat::abs(x);
    let e10 = mag.to_f64().abs().log10().floor() as i32;
    let scaled = mul_pow10(mag, 16 - e10, f);
    let mut intval = softfloat::to_i64(scaled, RoundMode::Nearest, 0, i64::MAX, f) as u64;
    let mut exp = e10;

    // Correct the estimate so intval has exactly 17 digits [10^16, 10^17).
    if intval >= 100_000_000_000_000_000 {
        intval /= 10;
        exp += 1;
    } else if intval != 0 && intval < 10_000_000_000_000_000 {
        intval *= 10;
        exp -= 1;
    }

    let mut digits = [0u8; 17];
    let mut t = intval;
    for d in digits.iter_mut().rev() {
        *d = (t % 10) as u8;
        t /= 10;
    }

    // k-factor -> number of significant digits to keep.
    let sig = if k > 0 {
        k as usize
    } else {
        // -k digits after the decimal point; the value has (exp + 1) integer
        // digits (when exp >= 0), so keep that many plus the requested
        // fraction digits.
        (exp + 1 - k as i32).clamp(1, 17) as usize
    };
    round_to_sig(&mut digits, &mut exp, sig, f);

    if exp.unsigned_abs() as i32 > MAX_DECIMAL_EXP {
        f.raise(ExcFlags::OPERR); // exponent does not fit 3 BCD digits
    }
    pack_value(sign, exp, &digits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip_f64(v: f64) -> f64 {
        let mut f = ExcFlags::default();
        let x = FloatX80::from_f64(v);
        let packed = to_packed(x, 17, &mut f);
        from_packed(packed, &mut f).to_f64()
    }

    #[test]
    fn packed_roundtrip_exact_for_short_decimals() {
        // Few-digit values survive the 17-digit decimal round-trip exactly.
        for v in [0.0_f64, 1.0, -1.0, 2.5, -3.75, 100.0, 0.5, -9999.0, 1e10] {
            assert_eq!(roundtrip_f64(v), v, "round-trip {v}");
        }
    }

    #[test]
    fn packed_roundtrip_near_exact_for_long_decimals() {
        // The decimal<->binary scaling is correctly-rounded to ~17 significant
        // digits but not bit-exact, so allow a tiny relative error.
        for v in [
            123.456_f64,
            1e-10,
            3.141592653589793,
            6.022e23,
            -2.718281828,
        ] {
            let r = roundtrip_f64(v);
            let rel = ((r - v) / v).abs();
            assert!(rel < 1e-13, "round-trip {v} -> {r} (rel {rel:e})");
        }
    }

    #[test]
    fn from_packed_known_value() {
        // +1.0 x 10^0: D0=1, all fractions 0, exp 0.
        let mut f = ExcFlags::default();
        let mut bytes = [0u8; 12];
        bytes[3] = 0x01; // D0 = 1
        assert_eq!(from_packed(bytes, &mut f).to_f64(), 1.0);

        // +1.5 x 10^1 = 15: D0=1, D1=5, exp=1.
        let mut bytes = [0u8; 12];
        bytes[2] = 0x00; // e0 nibble already 0; set exp=1 below
        bytes[3] = 0x01; // D0 = 1
        bytes[1] = 0x01; // e0 = 1 (w0 bits 19-16)
        bytes[4] = 0x50; // D1 = 5 (w1 top nibble)
        assert_eq!(from_packed(bytes, &mut f).to_f64(), 15.0);
    }

    #[test]
    fn packed_infinity_and_nan() {
        let mut f = ExcFlags::default();
        let inf = to_packed(FloatX80::infinity(false), 17, &mut f);
        assert!(from_packed(inf, &mut f).is_inf());
        let nan = to_packed(FloatX80::default_nan(), 17, &mut f);
        assert!(from_packed(nan, &mut f).is_nan());
        // Negative infinity sign survives.
        let ninf = to_packed(FloatX80::infinity(true), 17, &mut f);
        let r = from_packed(ninf, &mut f);
        assert!(r.is_inf() && r.sign());
    }

    #[test]
    fn to_packed_significant_digits_kfactor() {
        // 123.456 with k=4 significant digits -> 123.5.
        let mut f = ExcFlags::default();
        let p = to_packed(FloatX80::from_f64(123.456), 4, &mut f);
        let back = from_packed(p, &mut f).to_f64();
        assert!((back - 123.5).abs() < 1e-9, "got {back}");
        assert!(f.has(ExcFlags::INEX1));
    }
}
