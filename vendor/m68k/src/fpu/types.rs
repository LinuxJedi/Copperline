//! FPU types: the 68881/68882/68040 80-bit extended-precision value.

/// A 68k 80-bit extended-precision floating-point value, stored in the same
/// shape as the 96-bit memory format minus the padding: a 16-bit
/// sign+exponent word and a 64-bit mantissa whose bit 63 is the explicit
/// integer bit (this is NOT IEEE-implicit-bit form).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize,
)]
pub struct FloatX80 {
    pub mantissa: u64,
    pub sign_exp: u16,
}

impl FloatX80 {
    /// The 68040 default (non-signaling) NaN.
    pub const fn default_nan() -> Self {
        Self {
            sign_exp: 0x7FFF,
            mantissa: 0xFFFF_FFFF_FFFF_FFFF,
        }
    }

    /// Signed zero.
    pub const fn zero(sign: bool) -> Self {
        Self {
            sign_exp: (sign as u16) << 15,
            mantissa: 0,
        }
    }

    /// Signed infinity.
    pub const fn infinity(sign: bool) -> Self {
        Self {
            sign_exp: ((sign as u16) << 15) | 0x7FFF,
            mantissa: 0x8000_0000_0000_0000,
        }
    }

    /// Sign bit (true = negative).
    pub const fn sign(self) -> bool {
        (self.sign_exp >> 15) & 1 != 0
    }

    /// Biased 15-bit exponent field.
    pub const fn biased_exp(self) -> u16 {
        self.sign_exp & 0x7FFF
    }

    pub const fn is_nan(self) -> bool {
        self.biased_exp() == 0x7FFF && (self.mantissa << 1) != 0
    }

    /// Signaling NaN: exponent all ones, fraction nonzero, fraction MSB clear.
    pub const fn is_signaling_nan(self) -> bool {
        self.is_nan() && (self.mantissa & 0x4000_0000_0000_0000) == 0
    }

    pub const fn is_inf(self) -> bool {
        self.biased_exp() == 0x7FFF && (self.mantissa << 1) == 0
    }

    pub const fn is_zero(self) -> bool {
        self.biased_exp() == 0 && self.mantissa == 0
    }

    pub const fn is_denormal(self) -> bool {
        self.biased_exp() == 0 && self.mantissa != 0
    }

    /// Force a NaN to its quiet form (set the fraction MSB).
    pub const fn quiet(self) -> Self {
        Self {
            sign_exp: self.sign_exp,
            mantissa: self.mantissa | 0x4000_0000_0000_0000,
        }
    }

    /// Build from the raw 96-bit memory representation (sign+exponent word
    /// and 64-bit mantissa). This is lossless -- the in-register form already
    /// IS the extended format.
    pub const fn from_extended(exp_word: u16, mantissa: u64) -> Self {
        Self {
            sign_exp: exp_word,
            mantissa,
        }
    }

    /// Decompose into the raw 96-bit memory representation. Lossless.
    pub const fn to_extended(self) -> (u16, u64) {
        (self.sign_exp, self.mantissa)
    }

    /// Convert to the nearest f64. Lossy for values outside f64's range or
    /// precision; used by the temporary arithmetic bridge and (permanently)
    /// by the transcendental shim. Values that originated from an f64
    /// round-trip back exactly.
    pub fn to_f64(self) -> f64 {
        let sign = (self.sign_exp >> 15) & 1;
        let exp = (self.sign_exp & 0x7FFF) as i32;

        if exp == 0 && self.mantissa == 0 {
            return if sign != 0 { -0.0 } else { 0.0 };
        }
        if exp == 0x7FFF {
            return if self.mantissa << 1 == 0 {
                if sign != 0 {
                    f64::NEG_INFINITY
                } else {
                    f64::INFINITY
                }
            } else {
                f64::NAN
            };
        }

        // Bias for 80-bit extended: 16383; for f64: 1023.
        let biased_exp = exp - 16383 + 1023;
        if biased_exp <= 0 || biased_exp >= 2047 {
            // Out of f64 range: saturate.
            return if biased_exp >= 2047 {
                if sign != 0 {
                    f64::NEG_INFINITY
                } else {
                    f64::INFINITY
                }
            } else if sign != 0 {
                -0.0
            } else {
                0.0
            };
        }

        // Extended has an explicit integer bit; f64 does not.
        let frac = (self.mantissa << 1) >> 12;
        let bits = ((sign as u64) << 63) | ((biased_exp as u64) << 52) | frac;
        f64::from_bits(bits)
    }

    /// Build from an f64. Lossless (extended has more range and precision
    /// than f64).
    pub fn from_f64(value: f64) -> Self {
        let bits = value.to_bits();
        let sign = ((bits >> 63) as u16) << 15;
        let exp = ((bits >> 52) & 0x7FF) as i32;
        let frac = bits & 0x000F_FFFF_FFFF_FFFF;

        if exp == 0x7FF {
            // Infinity / NaN.
            let mantissa = if frac == 0 {
                0x8000_0000_0000_0000
            } else {
                0xC000_0000_0000_0000 | (frac << 11)
            };
            return Self {
                sign_exp: sign | 0x7FFF,
                mantissa,
            };
        }
        if exp == 0 {
            if frac == 0 {
                return Self {
                    sign_exp: sign,
                    mantissa: 0,
                };
            }
            // Subnormal f64: value = frac * 2^-1074. Normalize into the
            // extended format's much larger exponent range.
            let lz = frac.leading_zeros();
            let mantissa = frac << lz;
            let true_exp = -1074 + (63 - lz as i32);
            return Self {
                sign_exp: sign | ((true_exp + 16383) as u16),
                mantissa,
            };
        }
        Self {
            sign_exp: sign | ((exp - 1023 + 16383) as u16),
            mantissa: 0x8000_0000_0000_0000 | (frac << 11),
        }
    }
}
