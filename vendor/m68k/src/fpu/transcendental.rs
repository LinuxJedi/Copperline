//! f64-bridged FPU operations: the transcendentals plus the remainder family.
//!
//! These are the only operations still evaluated in f64 (53-bit) and widened
//! back to 80-bit extended. The real 6888x computes them with internal
//! polynomial/CORDIC tables that are not bit-accurate to libm regardless, so
//! this module is the single, isolated boundary to replace later with an
//! extended-precision or chip-accurate implementation. The rest of the FPU
//! (arithmetic, compare, conversions) is exact extended precision in
//! `softfloat.rs`.

use super::types::FloatX80;

#[inline]
fn bridge(x: FloatX80, op: impl FnOnce(f64) -> f64) -> FloatX80 {
    FloatX80::from_f64(op(x.to_f64()))
}

/// Evaluate a single-operand transcendental by opmode. Returns `None` if the
/// opmode is not one of the f64-bridged transcendentals.
pub fn eval_unary(opmode: u16, src: FloatX80) -> Option<FloatX80> {
    let r = match opmode {
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
        0x10 => bridge(src, f64::exp),
        0x08 => bridge(src, f64::exp_m1),
        0x11 => bridge(src, |v| 2.0_f64.powf(v)),
        0x12 => bridge(src, |v| 10.0_f64.powf(v)),
        0x14 => bridge(src, f64::ln),
        0x06 => bridge(src, f64::ln_1p),
        0x15 => bridge(src, f64::log10),
        0x16 => bridge(src, f64::log2),
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
