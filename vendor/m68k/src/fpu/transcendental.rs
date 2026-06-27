//! Extended-precision FPU transcendentals (and the FMOD/FREM remainders).
//!
//! Each function reduces its argument, evaluates a Taylor/atanh series in the
//! double-double layer (`dd.rs`) -- whose coefficients are exact rationals, so
//! no minimax tooling is needed -- and rounds the ~128-bit result to 64-bit
//! extended under the caller's FPCR mode, setting INEX/OPERR/DZ. This replaces
//! the former f64 bridge. Accuracy is faithful to ~64 bits (validated by
//! identities and exact anchors); it is not chip-bit-exact (the 6888x uses its
//! own CORDIC/polynomial microcode).
//!
//! Functions not yet converted to extended precision fall back to `bridge`
//! (f64) for now and are replaced in later milestones.

use super::dd::{self, Df};
use super::softfloat::{self, ExcFlags, RoundCtx, RoundMode};
use super::types::FloatX80;

// ============================== small helpers ============================

#[inline]
fn one_x80() -> FloatX80 {
    softfloat::from_u64(1, false)
}

#[inline]
fn fx(v: f64) -> FloatX80 {
    FloatX80::from_f64(v)
}

/// 2^k as an exact extended value (or +/-Inf / 0 on overflow/underflow).
#[inline]
fn two_pow(k: i32) -> FloatX80 {
    softfloat::scale(one_x80(), k, RoundCtx::NEAREST_EXT, &mut ExcFlags::default())
}

/// Result for a NaN operand: quiet it and signal SNAN/OPERR if it was signaling.
fn nan_result(x: FloatX80, f: &mut ExcFlags) -> FloatX80 {
    if x.is_signaling_nan() {
        f.raise(ExcFlags::SNAN);
        f.raise(ExcFlags::OPERR);
    }
    x.quiet()
}

#[inline]
fn noflags() -> ExcFlags {
    ExcFlags::default()
}

#[inline]
fn unbiased_exp(x: FloatX80) -> i32 {
    softfloat::to_i64(
        softfloat::getexp(x, &mut noflags()),
        RoundMode::Zero,
        i64::MIN,
        i64::MAX,
        &mut noflags(),
    ) as i32
}

// ============================== exp family ===============================

/// exp(r) for a small |r| <= ln2/2 via the Maclaurin series, in Df.
fn exp_reduced(r: Df) -> Df {
    let mut term = Df::from_i32(1);
    let mut sum = Df::from_i32(1);
    let mut k = 1i32;
    loop {
        term = dd::div(dd::mul(term, r), Df::from_i32(k));
        sum = dd::add(sum, term);
        if dd::term_negligible(term, sum) || k > 60 {
            break;
        }
        k += 1;
    }
    sum
}

/// e^p for an arbitrary Df exponent `p`: reduce p = k*ln2 + r (|r| <= ln2/2),
/// then exp(p) = 2^k * exp(r).
fn exp_dd(p: Df) -> Df {
    let k = (p.hi.to_f64() * std::f64::consts::LOG2_E).round();
    let ki = k as i32;
    let r = dd::sub(p, dd::mul_x80(dd::consts().ln2, fx(k)));
    dd::mul_x80(exp_reduced(r), two_pow(ki))
}

fn exp(x: FloatX80, ctx: RoundCtx, f: &mut ExcFlags) -> FloatX80 {
    if x.is_nan() {
        return nan_result(x, f);
    }
    if x.is_inf() {
        return if x.sign() { FloatX80::zero(false) } else { x };
    }
    if x.is_zero() {
        return one_x80();
    }
    exp_dd(Df::from_x80(x)).to_x80(ctx, f)
}

fn exp2(x: FloatX80, ctx: RoundCtx, f: &mut ExcFlags) -> FloatX80 {
    if x.is_nan() {
        return nan_result(x, f);
    }
    if x.is_inf() {
        return if x.sign() { FloatX80::zero(false) } else { x };
    }
    if x.is_zero() {
        return one_x80();
    }
    exp_dd(dd::mul_x80(dd::consts().ln2, x)).to_x80(ctx, f)
}

fn exp10(x: FloatX80, ctx: RoundCtx, f: &mut ExcFlags) -> FloatX80 {
    if x.is_nan() {
        return nan_result(x, f);
    }
    if x.is_inf() {
        return if x.sign() { FloatX80::zero(false) } else { x };
    }
    if x.is_zero() {
        return one_x80();
    }
    exp_dd(dd::mul_x80(dd::consts().ln10, x)).to_x80(ctx, f)
}

/// e^x - 1, accurate near 0 (direct series, no 1+ cancellation).
fn expm1(x: FloatX80, ctx: RoundCtx, f: &mut ExcFlags) -> FloatX80 {
    if x.is_nan() {
        return nan_result(x, f);
    }
    if x.is_inf() {
        return if x.sign() { softfloat::neg(one_x80()) } else { x };
    }
    if x.is_zero() {
        return x; // expm1(+-0) = +-0
    }
    let d = if x.to_f64().abs() <= 0.5 {
        // Series sum_{k>=1} x^k/k!.
        let xd = Df::from_x80(x);
        let mut term = xd;
        let mut sum = xd;
        let mut k = 2i32;
        loop {
            term = dd::div(dd::mul(term, xd), Df::from_i32(k));
            sum = dd::add(sum, term);
            if dd::term_negligible(term, sum) || k > 60 {
                break;
            }
            k += 1;
        }
        sum
    } else {
        dd::sub(exp_dd(Df::from_x80(x)), Df::from_i32(1))
    };
    d.to_x80(ctx, f)
}

// ============================== log family ===============================

/// Shared special-case prologue for ln/log2/log10. Returns the special result,
/// or None if `x` is finite and positive (compute the logarithm).
fn log_special(x: FloatX80, f: &mut ExcFlags) -> Option<FloatX80> {
    if x.is_nan() {
        Some(nan_result(x, f))
    } else if x.is_zero() {
        f.raise(ExcFlags::DZ);
        Some(FloatX80::infinity(true)) // log(0) = -inf
    } else if x.sign() {
        f.raise(ExcFlags::OPERR);
        Some(FloatX80::default_nan()) // log(negative)
    } else if x.is_inf() {
        Some(x) // log(+inf) = +inf
    } else {
        None
    }
}

/// ln(x) for finite x > 0, in Df: x = m*2^e with m in [sqrt(1/2), sqrt(2)),
/// ln(x) = e*ln2 + 2*atanh((m-1)/(m+1)).
fn ln_dd(x: FloatX80) -> Df {
    let mut e = unbiased_exp(x);
    let mut m = softfloat::getman(x, &mut noflags());
    if m.to_f64() > std::f64::consts::SQRT_2 {
        m = softfloat::scale(m, -1, RoundCtx::NEAREST_EXT, &mut noflags());
        e += 1;
    }
    let md = Df::from_x80(m);
    let s = dd::div(dd::sub(md, Df::from_i32(1)), dd::add(md, Df::from_i32(1)));
    let ln_f = dd::mul_x80(dd::atanh_small(s), fx(2.0));
    dd::add(dd::mul_x80(dd::consts().ln2, fx(e as f64)), ln_f)
}

fn ln(x: FloatX80, ctx: RoundCtx, f: &mut ExcFlags) -> FloatX80 {
    if let Some(r) = log_special(x, f) {
        return r;
    }
    ln_dd(x).to_x80(ctx, f)
}

fn log2(x: FloatX80, ctx: RoundCtx, f: &mut ExcFlags) -> FloatX80 {
    if let Some(r) = log_special(x, f) {
        return r;
    }
    dd::mul(ln_dd(x), dd::consts().log2e).to_x80(ctx, f)
}

fn log10(x: FloatX80, ctx: RoundCtx, f: &mut ExcFlags) -> FloatX80 {
    if let Some(r) = log_special(x, f) {
        return r;
    }
    dd::mul(ln_dd(x), dd::consts().log10e).to_x80(ctx, f)
}

/// ln(1+x), accurate near 0.
fn log1p(x: FloatX80, ctx: RoundCtx, f: &mut ExcFlags) -> FloatX80 {
    if x.is_nan() {
        return nan_result(x, f);
    }
    if x.is_zero() {
        return x; // log1p(+-0) = +-0
    }
    if x.is_inf() {
        return if x.sign() {
            f.raise(ExcFlags::OPERR);
            FloatX80::default_nan()
        } else {
            x
        };
    }
    let one = one_x80();
    // x <= -1: domain boundary.
    if x.sign() {
        let cmp = softfloat::compare(x, softfloat::neg(one), &mut noflags());
        use super::softfloat::FpCmp;
        if cmp == FpCmp::Equal {
            f.raise(ExcFlags::DZ);
            return FloatX80::infinity(true);
        }
        if cmp == FpCmp::Less {
            f.raise(ExcFlags::OPERR);
            return FloatX80::default_nan();
        }
    }
    let d = if x.to_f64().abs() <= 0.5 {
        // ln(1+x) = 2*atanh(x/(x+2)), no 1+x cancellation.
        let xd = Df::from_x80(x);
        let s = dd::div(xd, dd::add_x80(xd, fx(2.0)));
        dd::mul_x80(dd::atanh_small(s), fx(2.0))
    } else {
        ln_dd(softfloat::add(x, one, RoundCtx::NEAREST_EXT, &mut noflags()))
    };
    d.to_x80(ctx, f)
}

// ============================== still f64-bridged ========================

#[inline]
fn bridge(x: FloatX80, op: impl FnOnce(f64) -> f64) -> FloatX80 {
    FloatX80::from_f64(op(x.to_f64()))
}

// ============================== dispatch =================================

/// Evaluate a single-operand transcendental by opmode. Returns `None` if the
/// opmode is not a transcendental.
pub fn eval_unary(opmode: u16, src: FloatX80) -> Option<FloatX80> {
    let ctx = RoundCtx::NEAREST_EXT;
    let f = &mut noflags();
    let r = match opmode {
        // Extended-precision kernels.
        0x10 => exp(src, ctx, f),
        0x08 => expm1(src, ctx, f),
        0x11 => exp2(src, ctx, f),
        0x12 => exp10(src, ctx, f),
        0x14 => ln(src, ctx, f),
        0x06 => log1p(src, ctx, f),
        0x15 => log10(src, ctx, f),
        0x16 => log2(src, ctx, f),
        // Still f64-bridged (converted in later milestones).
        0x0E => bridge(src, f64::sin),
        0x1D => bridge(src, f64::cos),
        0x0F => bridge(src, f64::tan),
        0x0C => bridge(src, f64::asin),
        0x1C => bridge(src, f64::acos),
        0x0A => bridge(src, f64::atan),
        0x02 => bridge(src, f64::sinh),
        0x19 => bridge(src, f64::cosh),
        0x09 => bridge(src, f64::tanh),
        0x0D => bridge(src, f64::atanh),
        _ => return None,
    };
    Some(r)
}

/// FSINCOS: sine and cosine of the same operand.
pub fn sincos(src: FloatX80) -> (FloatX80, FloatX80) {
    let v = src.to_f64();
    (FloatX80::from_f64(v.sin()), FloatX80::from_f64(v.cos()))
}

/// FMOD: dst modulo src (truncated quotient).
pub fn fmod(dst: FloatX80, src: FloatX80) -> FloatX80 {
    FloatX80::from_f64(dst.to_f64() % src.to_f64())
}

/// FREM: IEEE remainder, r = x - y*round(x/y) (round-to-nearest quotient).
pub fn frem(dst: FloatX80, src: FloatX80) -> FloatX80 {
    let (x, y) = (dst.to_f64(), src.to_f64());
    if y == 0.0 {
        return FloatX80::default_nan();
    }
    let n = (x / y).round();
    FloatX80::from_f64(x - y * n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rn() -> RoundCtx {
        RoundCtx::NEAREST_EXT
    }
    fn ev(opmode: u16, x: f64) -> f64 {
        eval_unary(opmode, FloatX80::from_f64(x)).unwrap().to_f64()
    }
    fn close(a: f64, b: f64, rel: f64) -> bool {
        if b == 0.0 {
            a.abs() < rel
        } else {
            ((a - b) / b).abs() < rel
        }
    }

    #[test]
    fn exp_log_anchors() {
        // Exact / near-exact anchors.
        assert_eq!(ev(0x10, 0.0), 1.0); // exp(0)
        assert_eq!(ev(0x14, 1.0), 0.0); // ln(1)
        assert!(close(ev(0x11, 10.0), 1024.0, 1e-18)); // 2^10
        assert!(close(ev(0x12, 3.0), 1000.0, 1e-18)); // 10^3
        assert!(close(ev(0x16, 1024.0), 10.0, 1e-18)); // log2(1024)
        assert!(close(ev(0x15, 1000.0), 3.0, 1e-18)); // log10(1000)
        // exp(1) matches the e constant ROM pattern to within 1 ULP (faithful
        // rounding; last-bit-correct would need a TMD/Ziv test beyond 128 bits).
        let e = exp(fx(1.0), rn(), &mut noflags());
        let rom = softfloat::const_rom(0x0C);
        assert_eq!(e.sign_exp, rom.sign_exp);
        assert!((e.mantissa as i128 - rom.mantissa as i128).abs() <= 1, "exp(1) {e:?} vs {rom:?}");
    }

    #[test]
    fn exp_log_identities() {
        // exp(ln x) == x and ln(exp x) == x to ~2^-62.
        for &x in &[0.5_f64, 1.5, 2.0, 7.3, 100.0, 1e8] {
            let lx = ln(fx(x), rn(), &mut noflags());
            assert!(close(exp(lx, rn(), &mut noflags()).to_f64(), x, 1e-18), "exp(ln {x})");
        }
        for &x in &[-3.0_f64, -0.5, 0.0, 0.7, 5.0] {
            let ex = exp(fx(x), rn(), &mut noflags());
            assert!(close(ln(ex, rn(), &mut noflags()).to_f64(), x, 1e-15), "ln(exp {x})");
        }
        // 2^x == exp(x*ln2).
        for &x in &[-4.0_f64, 0.3, 1.0, 13.5] {
            assert!(close(ev(0x11, x), 2.0_f64.powf(x), 1e-15), "2^{x}");
        }
    }

    #[test]
    fn expm1_log1p_near_zero() {
        for &x in &[1e-6_f64, -1e-7, 0.01, -0.02, 1e-10] {
            assert!(close(ev(0x08, x), x.exp_m1(), 1e-14), "expm1 {x}");
            assert!(close(ev(0x06, x), x.ln_1p(), 1e-14), "log1p {x}");
        }
    }

    #[test]
    fn exp_log_specials() {
        assert!(exp(FloatX80::infinity(false), rn(), &mut noflags()).is_inf());
        assert!(exp(FloatX80::infinity(true), rn(), &mut noflags()).is_zero());
        let mut f = ExcFlags::default();
        assert!(ln(FloatX80::zero(false), rn(), &mut f).is_inf());
        assert!(f.has(ExcFlags::DZ));
        let mut f = ExcFlags::default();
        assert!(ln(fx(-1.0), rn(), &mut f).is_nan());
        assert!(f.has(ExcFlags::OPERR));
    }
}
