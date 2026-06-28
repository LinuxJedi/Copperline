//! Extended-precision FPU transcendentals (and the FMOD/FREM remainders).
//!
//! Each function reduces its argument, evaluates a Taylor/atanh series in the
//! double-double layer (`dd.rs`) -- whose coefficients are exact rationals, so
//! no minimax tooling is needed -- and rounds the ~128-bit result to 64-bit
//! extended under the caller's FPCR mode, setting INEX/OPERR/DZ. Accuracy is
//! faithful to ~64 bits (validated by identities and exact anchors); it is not
//! chip-bit-exact (the 6888x uses its own CORDIC/polynomial microcode, and a
//! bare 68040 traps these to a software FPSP). FMOD/FREM are exact.

use super::dd::{self, Df};
use super::softfloat::{self, ExcFlags, FpCmp, RoundCtx, RoundMode};
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
    softfloat::scale(
        one_x80(),
        k,
        RoundCtx::NEAREST_EXT,
        &mut ExcFlags::default(),
    )
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

/// e^x - 1 as Df, accurate near 0 (direct series, no 1+ cancellation).
fn expm1_df(x: FloatX80) -> Df {
    if x.to_f64().abs() <= 0.5 {
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
    }
}

fn expm1(x: FloatX80, ctx: RoundCtx, f: &mut ExcFlags) -> FloatX80 {
    if x.is_nan() {
        return nan_result(x, f);
    }
    if x.is_inf() {
        return if x.sign() {
            softfloat::neg(one_x80())
        } else {
            x
        };
    }
    if x.is_zero() {
        return x; // expm1(+-0) = +-0
    }
    expm1_df(x).to_x80(ctx, f)
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
        ln_dd(softfloat::add(
            x,
            one,
            RoundCtx::NEAREST_EXT,
            &mut noflags(),
        ))
    };
    d.to_x80(ctx, f)
}

// ============================== trig family ==============================

/// Reduce x = k*(pi/2) + r with |r| <= pi/4, returning (k, r). Accurate while
/// k*(pi/2) does not exceed the ~128-bit reduction precision (very large
/// arguments degrade, as they do on real hardware).
fn reduce_quadrant(x: FloatX80) -> (i64, Df) {
    let kf = (x.to_f64() * std::f64::consts::FRAC_2_PI).round();
    let k = kf as i64;
    let r = dd::sub(Df::from_x80(x), dd::mul_x80(dd::consts().pi_2, fx(kf)));
    (k, r)
}

/// sin(r) and cos(r) for |r| <= pi/4 via their Maclaurin series, in Df.
fn sin_cos_kernel(r: Df) -> (Df, Df) {
    let neg_r2 = dd::sqr(r).neg();
    // sin(r) = r - r^3/3! + ...
    let mut t = r;
    let mut s = r;
    let mut k = 1i32;
    loop {
        t = dd::div(dd::mul(t, neg_r2), Df::from_i32((2 * k) * (2 * k + 1)));
        s = dd::add(s, t);
        if dd::term_negligible(t, s) || k > 30 {
            break;
        }
        k += 1;
    }
    // cos(r) = 1 - r^2/2! + ...
    let mut tc = Df::from_i32(1);
    let mut c = Df::from_i32(1);
    let mut k = 1i32;
    loop {
        tc = dd::div(dd::mul(tc, neg_r2), Df::from_i32((2 * k - 1) * (2 * k)));
        c = dd::add(c, tc);
        if dd::term_negligible(tc, c) || k > 30 {
            break;
        }
        k += 1;
    }
    (s, c)
}

/// (sin x, cos x) as Df, plus quadrant handling; specials return Df NaN/etc.
fn sin_cos_df(x: FloatX80, f: &mut ExcFlags) -> Option<(Df, Df)> {
    if x.is_nan() {
        let n = Df::from_x80(nan_result(x, f));
        return Some((n, n));
    }
    if x.is_inf() {
        f.raise(ExcFlags::OPERR);
        let n = Df::from_x80(FloatX80::default_nan());
        return Some((n, n));
    }
    if x.is_zero() {
        return Some((Df::from_x80(x), Df::from_i32(1))); // sin(+-0)=+-0, cos=1
    }
    let (k, r) = reduce_quadrant(x);
    let (sr, cr) = sin_cos_kernel(r);
    let q = ((k % 4) + 4) % 4;
    Some(match q {
        0 => (sr, cr),
        1 => (cr, sr.neg()),
        2 => (sr.neg(), cr.neg()),
        _ => (cr.neg(), sr),
    })
}

fn sin_cos_x80(x: FloatX80, ctx: RoundCtx, f: &mut ExcFlags) -> (FloatX80, FloatX80) {
    let (s, c) = sin_cos_df(x, f).unwrap();
    (s.to_x80(ctx, f), c.to_x80(ctx, f))
}

fn sin(x: FloatX80, ctx: RoundCtx, f: &mut ExcFlags) -> FloatX80 {
    let (s, _) = sin_cos_df(x, f).unwrap();
    s.to_x80(ctx, f)
}

fn cos(x: FloatX80, ctx: RoundCtx, f: &mut ExcFlags) -> FloatX80 {
    let (_, c) = sin_cos_df(x, f).unwrap();
    c.to_x80(ctx, f)
}

fn tan(x: FloatX80, ctx: RoundCtx, f: &mut ExcFlags) -> FloatX80 {
    if x.is_nan() {
        return nan_result(x, f);
    }
    if x.is_inf() {
        f.raise(ExcFlags::OPERR);
        return FloatX80::default_nan();
    }
    if x.is_zero() {
        return x; // tan(+-0) = +-0
    }
    let (k, r) = reduce_quadrant(x);
    let (sr, cr) = sin_cos_kernel(r);
    // tan = sin/cos; with the quadrant this is sr/cr (even q) or -cr/sr (odd q).
    let t = if k & 1 == 0 {
        dd::div(sr, cr)
    } else {
        dd::div(cr, sr).neg()
    };
    t.to_x80(ctx, f)
}

// ============================== inverse trig =============================

/// sqrt of a Df via one Newton refinement of the 64-bit seed.
fn sqrt_df(v: Df) -> Df {
    if v.hi.is_zero() {
        return v;
    }
    let x0 = softfloat::sqrt(v.hi, RoundCtx::NEAREST_EXT, &mut noflags());
    let q = dd::div(v, Df::from_x80(x0));
    dd::mul_x80(dd::add_x80(q, x0), fx(0.5))
}

/// atan as a Df: reduce |a| to <= 0.3 via reciprocal (|a|>1) and the
/// half-angle identity atan(a) = 2*atan(a/(1+sqrt(1+a^2))), then series.
fn atan_df(x: Df) -> Df {
    let neg = x.hi.sign();
    let ax = if neg { x.neg() } else { x };
    let (mut a, comp) = if ax.hi.to_f64() > 1.0 {
        (dd::recip(ax), true)
    } else {
        (ax, false)
    };
    let mut halvings = 0u32;
    while a.hi.to_f64() > 0.3 {
        let s = sqrt_df(dd::add_x80(dd::sqr(a), fx(1.0)));
        a = dd::div(a, dd::add_x80(s, fx(1.0)));
        halvings += 1;
    }
    let mut r = dd::atan_small(a);
    for _ in 0..halvings {
        r = dd::mul_x80(r, fx(2.0));
    }
    if comp {
        r = dd::sub(dd::consts().pi_2, r);
    }
    if neg { r.neg() } else { r }
}

fn atan(x: FloatX80, ctx: RoundCtx, f: &mut ExcFlags) -> FloatX80 {
    if x.is_nan() {
        return nan_result(x, f);
    }
    if x.is_zero() {
        return x; // atan(+-0) = +-0
    }
    if x.is_inf() {
        let pi2 = dd::consts().pi_2;
        return if x.sign() { pi2.neg() } else { pi2 }.to_x80(ctx, f);
    }
    atan_df(Df::from_x80(x)).to_x80(ctx, f)
}

/// True if |x| > 1 (for domain checks).
fn abs_gt_one(x: FloatX80) -> bool {
    softfloat::compare(softfloat::abs(x), one_x80(), &mut noflags()) == FpCmp::Greater
}
fn abs_eq_one(x: FloatX80) -> bool {
    softfloat::compare(softfloat::abs(x), one_x80(), &mut noflags()) == FpCmp::Equal
}

fn asin(x: FloatX80, ctx: RoundCtx, f: &mut ExcFlags) -> FloatX80 {
    if x.is_nan() {
        return nan_result(x, f);
    }
    if x.is_zero() {
        return x; // asin(+-0) = +-0
    }
    if x.is_inf() || abs_gt_one(x) {
        f.raise(ExcFlags::OPERR);
        return FloatX80::default_nan();
    }
    if abs_eq_one(x) {
        let pi2 = dd::consts().pi_2;
        return if x.sign() { pi2.neg() } else { pi2 }.to_x80(ctx, f);
    }
    // asin(x) = atan(x / sqrt(1 - x^2)).
    let xd = Df::from_x80(x);
    let denom = sqrt_df(dd::sub(Df::from_i32(1), dd::sqr(xd)));
    atan_df(dd::div(xd, denom)).to_x80(ctx, f)
}

fn acos(x: FloatX80, ctx: RoundCtx, f: &mut ExcFlags) -> FloatX80 {
    if x.is_nan() {
        return nan_result(x, f);
    }
    if x.is_inf() || abs_gt_one(x) {
        f.raise(ExcFlags::OPERR);
        return FloatX80::default_nan();
    }
    if abs_eq_one(x) {
        // acos(1) = +0, acos(-1) = pi.
        return if x.sign() {
            dd::consts().pi.to_x80(ctx, f)
        } else {
            FloatX80::zero(false)
        };
    }
    // acos(x) = pi/2 - asin(x).
    let xd = Df::from_x80(x);
    let denom = sqrt_df(dd::sub(Df::from_i32(1), dd::sqr(xd)));
    let asin_df = atan_df(dd::div(xd, denom));
    dd::sub(dd::consts().pi_2, asin_df).to_x80(ctx, f)
}

// ============================== hyperbolic ===============================

fn sinh(x: FloatX80, ctx: RoundCtx, f: &mut ExcFlags) -> FloatX80 {
    if x.is_nan() {
        return nan_result(x, f);
    }
    if x.is_inf() || x.is_zero() {
        return x; // sinh(+-inf)=+-inf, sinh(+-0)=+-0
    }
    let d = if x.to_f64().abs() <= 0.5 {
        // Series sum_{k>=0} x^(2k+1)/(2k+1)!.
        let xd = Df::from_x80(x);
        let x2 = dd::sqr(xd);
        let mut term = xd;
        let mut sum = xd;
        let mut k = 1i32;
        loop {
            term = dd::div(dd::mul(term, x2), Df::from_i32((2 * k) * (2 * k + 1)));
            sum = dd::add(sum, term);
            if dd::term_negligible(term, sum) || k > 40 {
                break;
            }
            k += 1;
        }
        sum
    } else {
        let e = exp_dd(Df::from_x80(x));
        dd::mul_x80(dd::sub(e, dd::recip(e)), fx(0.5))
    };
    d.to_x80(ctx, f)
}

fn cosh(x: FloatX80, ctx: RoundCtx, f: &mut ExcFlags) -> FloatX80 {
    if x.is_nan() {
        return nan_result(x, f);
    }
    if x.is_inf() {
        return FloatX80::infinity(false); // cosh(+-inf) = +inf
    }
    if x.is_zero() {
        return one_x80();
    }
    let e = exp_dd(Df::from_x80(x));
    dd::mul_x80(dd::add(e, dd::recip(e)), fx(0.5)).to_x80(ctx, f)
}

fn tanh(x: FloatX80, ctx: RoundCtx, f: &mut ExcFlags) -> FloatX80 {
    if x.is_nan() {
        return nan_result(x, f);
    }
    if x.is_zero() {
        return x; // tanh(+-0) = +-0
    }
    if x.is_inf() || x.to_f64().abs() > 32.0 {
        // |x| large: tanh -> +-1 (inexact for finite x).
        let one = if x.sign() {
            softfloat::neg(one_x80())
        } else {
            one_x80()
        };
        if !x.is_inf() {
            f.raise(ExcFlags::INEX2);
        }
        return one;
    }
    // tanh(x) = (e^{2x}-1)/(e^{2x}+1) = u/(u+2), u = expm1(2x).
    let two_x = softfloat::scale(x, 1, RoundCtx::NEAREST_EXT, &mut noflags());
    let u = expm1_df(two_x);
    dd::div(u, dd::add_x80(u, fx(2.0))).to_x80(ctx, f)
}

fn atanh(x: FloatX80, ctx: RoundCtx, f: &mut ExcFlags) -> FloatX80 {
    if x.is_nan() {
        return nan_result(x, f);
    }
    if x.is_zero() {
        return x; // atanh(+-0) = +-0
    }
    if abs_eq_one(x) {
        f.raise(ExcFlags::DZ);
        return FloatX80::infinity(x.sign()); // atanh(+-1) = +-inf
    }
    if x.is_inf() || abs_gt_one(x) {
        f.raise(ExcFlags::OPERR);
        return FloatX80::default_nan();
    }
    let d = if x.to_f64().abs() <= 0.5 {
        dd::atanh_small(Df::from_x80(x))
    } else {
        // atanh(x) = 0.5 * ln((1+x)/(1-x)).
        let xd = Df::from_x80(x);
        let arg = dd::div(dd::add_x80(xd, fx(1.0)), dd::sub(Df::from_i32(1), xd));
        let l = ln_dd(arg.to_x80(RoundCtx::NEAREST_EXT, &mut noflags()));
        dd::mul_x80(l, fx(0.5))
    };
    d.to_x80(ctx, f)
}

// ============================== dispatch =================================

/// Evaluate a single-operand transcendental by opmode, rounding to `ctx` and
/// accumulating exceptions into `f`. Returns `None` if the opmode is not a
/// transcendental.
pub fn eval_unary(opmode: u16, src: FloatX80, ctx: RoundCtx, f: &mut ExcFlags) -> Option<FloatX80> {
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
        0x0E => sin(src, ctx, f),
        0x1D => cos(src, ctx, f),
        0x0F => tan(src, ctx, f),
        0x0C => asin(src, ctx, f),
        0x1C => acos(src, ctx, f),
        0x0A => atan(src, ctx, f),
        0x02 => sinh(src, ctx, f),
        0x19 => cosh(src, ctx, f),
        0x09 => tanh(src, ctx, f),
        0x0D => atanh(src, ctx, f),
        _ => return None,
    };
    Some(r)
}

/// FSINCOS: sine and cosine of the same operand.
pub fn sincos(src: FloatX80, ctx: RoundCtx, f: &mut ExcFlags) -> (FloatX80, FloatX80) {
    sin_cos_x80(src, ctx, f)
}

/// Result of FMOD/FREM: the exact remainder, the low 7 bits of |quotient|,
/// and the quotient's sign.
pub struct Remainder {
    pub value: FloatX80,
    pub quotient: u8,
    pub quotient_sign: bool,
}

/// Exact extended remainder of `dst` by `src`. `ieee` selects FREM (quotient
/// rounded to nearest) vs FMOD (truncated quotient). The remainder is computed
/// exactly by shift-subtract long division of the significands.
pub fn remainder(dst: FloatX80, src: FloatX80, ieee: bool, f: &mut ExcFlags) -> Remainder {
    let none = |v: FloatX80| Remainder {
        value: v,
        quotient: 0,
        quotient_sign: false,
    };
    if dst.is_nan() {
        return none(nan_result(dst, f));
    }
    if src.is_nan() {
        return none(nan_result(src, f));
    }
    let quotient_sign = dst.sign() ^ src.sign();
    if dst.is_inf() || src.is_zero() {
        f.raise(ExcFlags::OPERR);
        return none(FloatX80::default_nan());
    }
    if src.is_inf() || dst.is_zero() {
        // x mod inf = x; 0 mod y = 0.
        return Remainder {
            value: dst,
            quotient: 0,
            quotient_sign,
        };
    }

    let ax = softfloat::abs(dst);
    let ay = softfloat::abs(src);
    let ex = unbiased_exp(ax);
    let ey = unbiased_exp(ay);
    let mx = softfloat::getman(ax, &mut noflags()).mantissa;
    let my = softfloat::getman(ay, &mut noflags()).mantissa as u128;
    let ediff = ex - ey;

    // Floor remainder magnitude `rfloor` and the low bits of floor(|x|/|y|).
    let (mut rfloor, mut q, rem_int): (FloatX80, u64, u128) = if ediff < 0 {
        (ax, 0, u128::MAX) // |x| < |y|: remainder = |x| (rem_int unused)
    } else {
        // Long division of M = mx << ediff by my (bit by bit, MSB first).
        let mut rem: u128 = 0;
        let mut q: u64 = 0;
        let total = 64 + ediff;
        for j in (0..total).rev() {
            rem <<= 1;
            if j >= ediff {
                rem |= ((mx >> (j - ediff)) & 1) as u128;
            }
            q <<= 1;
            if rem >= my {
                rem -= my;
                q |= 1;
            }
        }
        let rv = if rem == 0 {
            FloatX80::zero(false)
        } else {
            softfloat::scale(
                softfloat::from_u64(rem as u64, false),
                ey - 63,
                RoundCtx::NEAREST_EXT,
                &mut noflags(),
            )
        };
        (rv, q, rem)
    };

    let mut sign = dst.sign();
    if ieee {
        // FREM: round the quotient to nearest. Compare 2*rfloor with |y|.
        let two_rfloor = softfloat::scale(rfloor, 1, RoundCtx::NEAREST_EXT, &mut noflags());
        let c = softfloat::compare(two_rfloor, ay, &mut noflags());
        let round_up = c == FpCmp::Greater || (c == FpCmp::Equal && (q & 1) == 1);
        if round_up {
            q += 1;
            // Remainder crosses zero: new magnitude = |y| - rfloor, sign flips.
            rfloor = if ediff >= 0 && rem_int != u128::MAX {
                let nr = my - rem_int;
                softfloat::scale(
                    softfloat::from_u64(nr as u64, false),
                    ey - 63,
                    RoundCtx::NEAREST_EXT,
                    &mut noflags(),
                )
            } else {
                softfloat::sub(ay, rfloor, RoundCtx::NEAREST_EXT, &mut noflags())
            };
            sign = !sign;
        }
    }

    let value = if rfloor.is_zero() {
        FloatX80::zero(sign)
    } else if sign {
        softfloat::neg(rfloor)
    } else {
        rfloor
    };
    Remainder {
        value,
        quotient: (q & 0x7F) as u8,
        quotient_sign,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rn() -> RoundCtx {
        RoundCtx::NEAREST_EXT
    }
    fn ev(opmode: u16, x: f64) -> f64 {
        eval_unary(opmode, FloatX80::from_f64(x), rn(), &mut noflags())
            .unwrap()
            .to_f64()
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
        assert!(
            (e.mantissa as i128 - rom.mantissa as i128).abs() <= 1,
            "exp(1) {e:?} vs {rom:?}"
        );
    }

    #[test]
    fn exp_log_identities() {
        // exp(ln x) == x and ln(exp x) == x to ~2^-62.
        for &x in &[0.5_f64, 1.5, 2.0, 7.3, 100.0, 1e8] {
            let lx = ln(fx(x), rn(), &mut noflags());
            assert!(
                close(exp(lx, rn(), &mut noflags()).to_f64(), x, 1e-18),
                "exp(ln {x})"
            );
        }
        for &x in &[-3.0_f64, -0.5, 0.0, 0.7, 5.0] {
            let ex = exp(fx(x), rn(), &mut noflags());
            assert!(
                close(ln(ex, rn(), &mut noflags()).to_f64(), x, 1e-15),
                "ln(exp {x})"
            );
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
    fn trig_anchors_and_identities() {
        assert_eq!(ev(0x0E, 0.0), 0.0); // sin(0)
        assert_eq!(ev(0x1D, 0.0), 1.0); // cos(0)
        assert_eq!(ev(0x0F, 0.0), 0.0); // tan(0)
        // sin^2 + cos^2 == 1 across quadrants and a large argument.
        for &x in &[0.3_f64, 1.2, 3.0, -2.5, 7.0, 100.0, 1000.0] {
            let s = ev(0x0E, x);
            let c = ev(0x1D, x);
            assert!((s * s + c * c - 1.0).abs() < 1e-15, "sin^2+cos^2 at {x}");
            // tan == sin/cos.
            let t = ev(0x0F, x);
            assert!(close(t, s / c, 1e-14), "tan at {x}");
            // agreement with libm to ~f64 precision.
            assert!((s - x.sin()).abs() < 1e-14, "sin {x}");
            assert!((c - x.cos()).abs() < 1e-14, "cos {x}");
        }
    }

    #[test]
    fn sincos_pair() {
        let (s, c) = sincos(fx(1.0), rn(), &mut noflags());
        assert!((s.to_f64() - 1.0_f64.sin()).abs() < 1e-15);
        assert!((c.to_f64() - 1.0_f64.cos()).abs() < 1e-15);
    }

    #[test]
    fn inverse_trig() {
        // atan(1)*4 == pi.
        assert!(close(ev(0x0A, 1.0) * 4.0, std::f64::consts::PI, 1e-15));
        assert_eq!(ev(0x0A, 0.0), 0.0);
        // asin(sin r) == r, acos(cos r) == r over the principal range.
        for &x in &[-0.9_f64, -0.4, 0.0, 0.25, 0.8, 0.999] {
            assert!(close(ev(0x0C, x.sin()), x, 1e-13), "asin(sin {x})");
        }
        for &x in &[0.1_f64, 0.7, 1.5, 3.0] {
            assert!(close(ev(0x1C, x.cos()), x, 1e-13), "acos(cos {x})");
        }
        // libm agreement.
        for &x in &[-5.0_f64, -0.3, 0.6, 42.0] {
            assert!((ev(0x0A, x) - x.atan()).abs() < 1e-14, "atan {x}");
        }
        // domain errors.
        let mut f = ExcFlags::default();
        assert!(asin(fx(2.0), rn(), &mut f).is_nan());
        assert!(f.has(ExcFlags::OPERR));
    }

    #[test]
    fn hyperbolic() {
        for &x in &[-3.0_f64, -0.3, 0.0, 0.2, 1.0, 5.0] {
            assert!(
                (ev(0x02, x) - x.sinh()).abs() < 1e-12 * (1.0 + x.sinh().abs()),
                "sinh {x}"
            );
            assert!(
                (ev(0x19, x) - x.cosh()).abs() < 1e-12 * (1.0 + x.cosh().abs()),
                "cosh {x}"
            );
            assert!((ev(0x09, x) - x.tanh()).abs() < 1e-14, "tanh {x}");
            // cosh^2 - sinh^2 == 1.
            let s = ev(0x02, x);
            let c = ev(0x19, x);
            assert!((c * c - s * s - 1.0).abs() < 1e-12, "cosh^2-sinh^2 at {x}");
        }
        // atanh near 0 and mid-range.
        for &x in &[-0.5_f64, -0.1, 0.0, 0.3, 0.9] {
            assert!((ev(0x0D, x) - x.atanh()).abs() < 1e-14, "atanh {x}");
        }
        let mut f = ExcFlags::default();
        assert!(atanh(fx(1.0), rn(), &mut f).is_inf());
        assert!(f.has(ExcFlags::DZ));
    }

    #[test]
    fn fmod_frem_exact() {
        let r = |a: f64, b: f64, ieee: bool| remainder(fx(a), fx(b), ieee, &mut noflags());
        // FMOD basics.
        let m = r(7.5, 2.0, false);
        assert_eq!(m.value.to_f64(), 1.5);
        assert_eq!(m.quotient, 3); // 7.5 = 3*2 + 1.5
        assert!(!m.quotient_sign);
        // Negative dividend: remainder takes the sign of x.
        let m = r(-7.5, 2.0, false);
        assert_eq!(m.value.to_f64(), -1.5);
        assert!(m.quotient_sign); // -/+ -> negative quotient
        // FREM rounds the quotient to nearest: 7.5/2 = 3.75 -> 4, rem = -0.5.
        let q = r(7.5, 2.0, true);
        assert_eq!(q.value.to_f64(), -0.5);
        assert_eq!(q.quotient, 4);
        // Exactness beyond f64: 1e300 mod 3 via the f64 path would be wrong;
        // here it is exact (result in [0,3)).
        let big = r(1e18, 3.0, false);
        assert!(big.value.to_f64() >= 0.0 && big.value.to_f64() < 3.0);
        // src = 0 -> OPERR + NaN.
        let mut f = ExcFlags::default();
        let z = remainder(fx(1.0), fx(0.0), false, &mut f);
        assert!(z.value.is_nan() && f.has(ExcFlags::OPERR));
    }

    #[test]
    fn fpcr_mode_and_inex_threaded() {
        use super::super::softfloat::{Precision, RoundMode};
        let rz = RoundCtx {
            mode: RoundMode::Zero,
            prec: Precision::Extended,
        };
        let rp = RoundCtx {
            mode: RoundMode::PosInf,
            prec: Precision::Extended,
        };
        let mut fz = ExcFlags::default();
        let mut fp = ExcFlags::default();
        let lo = eval_unary(0x14, fx(3.0), rz, &mut fz).unwrap(); // ln(3) toward zero
        let hi = eval_unary(0x14, fx(3.0), rp, &mut fp).unwrap(); // ln(3) toward +inf
        // ln(3) is irrational: rounding up gives a mantissa one ULP above
        // rounding toward zero, and both are inexact.
        assert_eq!(hi.mantissa, lo.mantissa + 1);
        assert!(fz.has(ExcFlags::INEX2) && fp.has(ExcFlags::INEX2));
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
