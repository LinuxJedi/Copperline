//! Double-`FloatX80` ("double-double") arithmetic for the extended-precision
//! transcendental kernels.
//!
//! A [`Df`] carries a value as an unevaluated sum `hi + lo` of two
//! [`FloatX80`] with `|lo| <= 1/2 ulp(hi)`, giving ~128 significand bits. The
//! error-free transforms (`two_sum`, `two_prod`) are the classic Dekker/Knuth
//! algorithms; they are valid because the underlying [`softfloat`] add/sub/mul
//! are correctly-rounded round-to-nearest. Kernels evaluate series here and
//! round the final `hi + lo` to 64-bit extended via [`Df::to_x80`] (which is
//! just `softfloat::add`, i.e. the correctly-rounded sum under the caller's
//! FPCR mode, with INEX set appropriately).

use super::softfloat::{self, ExcFlags, RoundCtx};
use super::types::FloatX80;
use std::sync::OnceLock;

// Round-to-nearest scalar ops with flags discarded (the error-free transforms
// only rely on RN correctness, not on the exception flags).
#[inline]
fn fadd(a: FloatX80, b: FloatX80) -> FloatX80 {
    softfloat::add(a, b, RoundCtx::NEAREST_EXT, &mut ExcFlags::default())
}
#[inline]
fn fsub(a: FloatX80, b: FloatX80) -> FloatX80 {
    softfloat::sub(a, b, RoundCtx::NEAREST_EXT, &mut ExcFlags::default())
}
#[inline]
fn fmul(a: FloatX80, b: FloatX80) -> FloatX80 {
    softfloat::mul(a, b, RoundCtx::NEAREST_EXT, &mut ExcFlags::default())
}
#[inline]
fn fdiv(a: FloatX80, b: FloatX80) -> FloatX80 {
    softfloat::div(a, b, RoundCtx::NEAREST_EXT, &mut ExcFlags::default())
}

#[inline]
fn fx(v: f64) -> FloatX80 {
    FloatX80::from_f64(v)
}

// ============================== error-free transforms ====================

/// Exact sum: returns (s, e) with s = RN(a+b) and a+b = s+e exactly.
#[inline]
fn two_sum(a: FloatX80, b: FloatX80) -> (FloatX80, FloatX80) {
    let s = fadd(a, b);
    let bb = fsub(s, a);
    let err = fadd(fsub(a, fsub(s, bb)), fsub(b, bb));
    (s, err)
}

/// Exact sum for |a| >= |b|.
#[inline]
fn quick_two_sum(a: FloatX80, b: FloatX80) -> (FloatX80, FloatX80) {
    let s = fadd(a, b);
    let e = fsub(b, fsub(s, a));
    (s, e)
}

/// Veltkamp split of a 64-bit-significand value into two <=32-bit halves so
/// that products of the halves are exact. Factor is 2^32 + 1.
#[inline]
fn split(a: FloatX80) -> (FloatX80, FloatX80) {
    const SPLIT: f64 = 4294967297.0; // 2^32 + 1, exact in both f64 and x80
    let t = fmul(fx(SPLIT), a);
    let hi = fsub(t, fsub(t, a));
    let lo = fsub(a, hi);
    (hi, lo)
}

/// Exact product: returns (p, e) with p = RN(a*b) and a*b = p+e exactly.
#[inline]
fn two_prod(a: FloatX80, b: FloatX80) -> (FloatX80, FloatX80) {
    let p = fmul(a, b);
    let (ah, al) = split(a);
    let (bh, bl) = split(b);
    // e = ((ah*bh - p) + ah*bl + al*bh) + al*bl
    let e = fadd(
        fadd(fadd(fsub(fmul(ah, bh), p), fmul(ah, bl)), fmul(al, bh)),
        fmul(al, bl),
    );
    (p, e)
}

// ============================== Df type ==================================

#[derive(Clone, Copy)]
pub struct Df {
    pub hi: FloatX80,
    pub lo: FloatX80,
}

impl Df {
    #[inline]
    pub fn from_x80(a: FloatX80) -> Df {
        Df {
            hi: a,
            lo: FloatX80::zero(false),
        }
    }

    #[inline]
    pub fn from_i32(n: i32) -> Df {
        Df::from_x80(softfloat::from_u64(n.unsigned_abs() as u64, n < 0))
    }

    #[inline]
    pub fn neg(self) -> Df {
        Df {
            hi: softfloat::neg(self.hi),
            lo: softfloat::neg(self.lo),
        }
    }

    /// Round the value to 64-bit extended under `ctx`, setting INEX if inexact.
    #[inline]
    pub fn to_x80(self, ctx: RoundCtx, f: &mut ExcFlags) -> FloatX80 {
        softfloat::add(self.hi, self.lo, ctx, f)
    }
}

#[inline]
pub fn add(a: Df, b: Df) -> Df {
    let (s, e) = two_sum(a.hi, b.hi);
    let e = fadd(e, fadd(a.lo, b.lo));
    let (hi, lo) = quick_two_sum(s, e);
    Df { hi, lo }
}

#[inline]
pub fn sub(a: Df, b: Df) -> Df {
    add(a, b.neg())
}

#[inline]
pub fn add_x80(a: Df, b: FloatX80) -> Df {
    let (s, e) = two_sum(a.hi, b);
    let e = fadd(e, a.lo);
    let (hi, lo) = quick_two_sum(s, e);
    Df { hi, lo }
}

#[inline]
pub fn mul(a: Df, b: Df) -> Df {
    let (p, e) = two_prod(a.hi, b.hi);
    let e = fadd(e, fadd(fmul(a.hi, b.lo), fmul(a.lo, b.hi)));
    let (hi, lo) = quick_two_sum(p, e);
    Df { hi, lo }
}

#[inline]
pub fn mul_x80(a: Df, b: FloatX80) -> Df {
    let (p, e) = two_prod(a.hi, b);
    let e = fadd(e, fmul(a.lo, b));
    let (hi, lo) = quick_two_sum(p, e);
    Df { hi, lo }
}

#[inline]
pub fn sqr(a: Df) -> Df {
    mul(a, a)
}

pub fn div(a: Df, b: Df) -> Df {
    let q1 = fdiv(a.hi, b.hi);
    let r = sub(a, mul_x80(b, q1));
    let q2 = fdiv(r.hi, b.hi);
    let r = sub(r, mul_x80(b, q2));
    let q3 = fdiv(r.hi, b.hi);
    let (hi, lo) = quick_two_sum(q1, q2);
    add_x80(Df { hi, lo }, q3)
}

#[inline]
pub fn recip(b: Df) -> Df {
    div(Df::from_i32(1), b)
}

// ============================== series helpers ===========================

/// True when `pow` is negligible relative to `sum` (>= ~130 bits below).
fn negligible(pow: FloatX80, sum: FloatX80) -> bool {
    if pow.is_zero() {
        return true;
    }
    if sum.is_zero() {
        return false;
    }
    (sum.biased_exp() as i32) - (pow.biased_exp() as i32) > 130
}

/// Sum_{k>=0} s * x^(2k+1)/(2k+1), where s = (-1)^k if `alternating` else 1.
/// Used for atan (alternating) and atanh (not). `x` must be small (|x| < 0.5)
/// for fast convergence.
fn odd_series(x: Df, alternating: bool) -> Df {
    let x2 = sqr(x);
    let mut pow = x; // x^(2k+1)
    let mut sum = x;
    let mut k = 1usize;
    loop {
        pow = mul(pow, x2);
        let term = div(pow, Df::from_i32((2 * k + 1) as i32));
        let term = if alternating && (k & 1 == 1) {
            term.neg()
        } else {
            term
        };
        sum = add(sum, term);
        if negligible(pow.hi, sum.hi) || k > 200 {
            break;
        }
        k += 1;
    }
    sum
}

/// True when adding `term` to `sum` would not change `sum` to ~128 bits, i.e.
/// a series can stop. Also true once `term` underflows to zero.
pub fn term_negligible(term: Df, sum: Df) -> bool {
    negligible(term.hi, sum.hi)
}

pub fn atan_small(x: Df) -> Df {
    odd_series(x, true)
}

pub fn atanh_small(x: Df) -> Df {
    odd_series(x, false)
}

// ============================== constants ================================

#[derive(Clone, Copy)]
pub struct Consts {
    pub pi: Df,
    pub pi_2: Df,
    pub ln2: Df,
    pub ln10: Df,
    pub log2e: Df,
    pub log10e: Df,
}

static CONSTS: OnceLock<Consts> = OnceLock::new();

/// Cached Df constants, computed once via self-contained series.
pub fn consts() -> &'static Consts {
    CONSTS.get_or_init(|| {
        let inv = |n: i32| div(Df::from_i32(1), Df::from_i32(n));
        // pi = 16*atan(1/5) - 4*atan(1/239)  (Machin)
        let pi = sub(
            mul_x80(atan_small(inv(5)), fx(16.0)),
            mul_x80(atan_small(inv(239)), fx(4.0)),
        );
        // ln2 = 2*atanh(1/3); ln10 = 3*ln2 + 2*atanh(1/9)
        let ln2 = mul_x80(atanh_small(inv(3)), fx(2.0));
        let ln10 = add(mul_x80(ln2, fx(3.0)), mul_x80(atanh_small(inv(9)), fx(2.0)));
        Consts {
            pi,
            pi_2: mul_x80(pi, fx(0.5)),
            ln2,
            ln10,
            log2e: recip(ln2),
            log10e: recip(ln10),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rn() -> RoundCtx {
        RoundCtx::NEAREST_EXT
    }
    fn to_f64(d: Df) -> f64 {
        d.to_x80(rn(), &mut ExcFlags::default()).to_f64()
    }

    #[test]
    fn two_prod_is_exact() {
        // a*b reconstructed as p+e equals the f64 product for exact cases.
        let a = fx(1.5);
        let b = fx(3.25);
        let (p, e) = two_prod(a, b);
        assert_eq!(fadd(p, e).to_f64(), 1.5 * 3.25);
        assert!(e.is_zero()); // exact product fits, no error term
    }

    #[test]
    fn df_arithmetic_matches_f64() {
        let a = Df::from_i32(7);
        let b = Df::from_i32(3);
        // Exact results are exact.
        assert_eq!(to_f64(add(a, b)), 10.0);
        assert_eq!(to_f64(sub(a, b)), 4.0);
        assert_eq!(to_f64(mul(a, b)), 21.0);
        // Inexact: the Df result is correctly rounded to 64-bit extended;
        // narrowing to f64 can differ from f64's own 7/3 by one ULP
        // (double rounding), so allow a couple of f64 ULP.
        assert!((to_f64(div(a, b)) - 7.0 / 3.0).abs() < 1e-15);
    }

    #[test]
    fn df_keeps_more_than_64_bits() {
        // 1 + 2^-80: lost in a single FloatX80 (64-bit), retained in Df.
        let tiny = FloatX80 {
            sign_exp: (16383 - 80) as u16,
            mantissa: 0x8000_0000_0000_0000,
        };
        let d = add_x80(Df::from_i32(1), tiny);
        // hi rounds to exactly 1.0, but lo holds the tail.
        assert_eq!(d.hi.to_f64(), 1.0);
        assert!(!d.lo.is_zero());
    }

    #[test]
    fn constants_match_rom_hi_words() {
        let c = consts();
        // The hi word of each constant must equal the correctly-rounded 64-bit
        // ROM pattern in softfloat::const_rom.
        assert_eq!(c.pi.hi, softfloat::const_rom(0x00));
        assert_eq!(c.ln2.hi, softfloat::const_rom(0x30));
        assert_eq!(c.ln10.hi, softfloat::const_rom(0x31));
        assert_eq!(c.log2e.hi, softfloat::const_rom(0x0D));
        assert_eq!(c.log10e.hi, softfloat::const_rom(0x0E));
    }

    #[test]
    fn constants_value_sanity() {
        let c = consts();
        assert!((to_f64(c.pi) - std::f64::consts::PI).abs() < 1e-15);
        assert!((to_f64(c.ln2) - std::f64::consts::LN_2).abs() < 1e-15);
        assert!((to_f64(c.ln10) - std::f64::consts::LN_10).abs() < 1e-15);
    }
}
