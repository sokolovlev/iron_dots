//! Fixed-point arithmetic. Everything the simulation computes is an integer,
//! so results are bit-identical on every CPU, OS and SIMD level.
//!
//! * [`Fx`] — Q16.16 signed fixed point (1/65536 of a tile for positions).
//! * [`Milli`] — a ratio in thousandths, used for balance multipliers.

use core::ops::{Add, AddAssign, Neg, Sub, SubAssign};
use serde::{Deserialize, Serialize};

/// Number of fractional bits in [`Fx`].
pub const FRAC_BITS: u32 = 16;
/// `1.0` in raw [`Fx`] units.
pub const ONE: i32 = 1 << FRAC_BITS;

/// Q16.16 signed fixed-point number.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[repr(transparent)]
pub struct Fx(pub i32);

impl Fx {
    pub const ZERO: Fx = Fx(0);
    pub const ONE: Fx = Fx(ONE);
    pub const MAX: Fx = Fx(i32::MAX);

    #[inline]
    pub const fn from_int(v: i32) -> Fx {
        Fx(v << FRAC_BITS)
    }

    /// `num / den` rounded toward zero.
    #[inline]
    pub const fn from_ratio(num: i64, den: i64) -> Fx {
        Fx(((num << FRAC_BITS) / den) as i32)
    }

    /// Value of a [`Milli`] (thousandths) as fixed point.
    #[inline]
    pub const fn from_milli(m: Milli) -> Fx {
        Fx::from_ratio(m.0, 1000)
    }

    #[inline]
    pub const fn raw(self) -> i32 {
        self.0
    }

    /// Integer part (floor).
    #[inline]
    pub const fn floor_int(self) -> i32 {
        self.0 >> FRAC_BITS
    }

    #[inline]
    pub const fn mul(self, rhs: Fx) -> Fx {
        Fx(((self.0 as i64 * rhs.0 as i64) >> FRAC_BITS) as i32)
    }

    /// Multiply by a ratio in thousandths, rounding toward zero.
    #[inline]
    pub const fn mul_milli(self, m: Milli) -> Fx {
        Fx((self.0 as i64 * m.0 / 1000) as i32)
    }

    #[inline]
    pub const fn clamp(self, lo: Fx, hi: Fx) -> Fx {
        if self.0 < lo.0 {
            lo
        } else if self.0 > hi.0 {
            hi
        } else {
            self
        }
    }
}

impl Add for Fx {
    type Output = Fx;
    #[inline]
    fn add(self, rhs: Fx) -> Fx {
        Fx(self.0.wrapping_add(rhs.0))
    }
}
impl Sub for Fx {
    type Output = Fx;
    #[inline]
    fn sub(self, rhs: Fx) -> Fx {
        Fx(self.0.wrapping_sub(rhs.0))
    }
}
impl Neg for Fx {
    type Output = Fx;
    #[inline]
    fn neg(self) -> Fx {
        Fx(-self.0)
    }
}
impl AddAssign for Fx {
    #[inline]
    fn add_assign(&mut self, rhs: Fx) {
        *self = *self + rhs;
    }
}
impl SubAssign for Fx {
    #[inline]
    fn sub_assign(&mut self, rhs: Fx) {
        *self = *self - rhs;
    }
}

/// A 2D point in tiles, Q16.16 per axis.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FxPoint {
    pub x: Fx,
    pub y: Fx,
}

impl FxPoint {
    #[inline]
    pub const fn new(x: Fx, y: Fx) -> Self {
        Self { x, y }
    }

    /// Point at the centre of tile `(tx, ty)`.
    #[inline]
    pub const fn tile_center(tx: i32, ty: i32) -> Self {
        Self { x: Fx((tx << FRAC_BITS) + ONE / 2), y: Fx((ty << FRAC_BITS) + ONE / 2) }
    }
}

/// A decimal with three fractional digits: `Milli(1250)` is 1.25.
///
/// In RON files it is written as a string (`"1.25"`) or an integer (`2`), so
/// that loading balance never goes through floating point.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Milli(pub i64);

impl Milli {
    pub const ONE: Milli = Milli(1000);

    /// Parses `"-12.345"`-style decimals with at most three fractional digits.
    pub fn parse(s: &str) -> Result<Milli, String> {
        let t = s.trim();
        let (neg, t) = match t.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, t),
        };
        let (int_part, frac_part) = match t.split_once(['.', ',']) {
            Some((i, f)) => (i, f),
            None => (t, ""),
        };
        let bad = || format!("invalid decimal {s:?}: expected e.g. \"1.25\" (max 3 decimals)");
        if int_part.is_empty() && frac_part.is_empty() {
            return Err(bad());
        }
        if frac_part.len() > 3
            || !int_part.bytes().all(|b| b.is_ascii_digit())
            || !frac_part.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(bad());
        }
        let int: i64 = if int_part.is_empty() { 0 } else { int_part.parse().map_err(|_| bad())? };
        let mut frac: i64 = if frac_part.is_empty() { 0 } else { frac_part.parse().map_err(|_| bad())? };
        for _ in frac_part.len()..3 {
            frac *= 10;
        }
        let v = int.checked_mul(1000).and_then(|v| v.checked_add(frac)).ok_or_else(bad)?;
        Ok(Milli(if neg { -v } else { v }))
    }
}

impl core::fmt::Display for Milli {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let sign = if self.0 < 0 { "-" } else { "" };
        let a = self.0.unsigned_abs();
        let (i, fr) = (a / 1000, a % 1000);
        if fr == 0 {
            write!(f, "{sign}{i}")
        } else {
            let s = format!("{fr:03}");
            write!(f, "{sign}{i}.{}", s.trim_end_matches('0'))
        }
    }
}

impl Serialize for Milli {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Milli {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Milli, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = Milli;
            fn expecting(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str("an integer or a decimal string like \"1.25\"")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Milli, E> {
                Milli::parse(v).map_err(E::custom)
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Milli, E> {
                v.checked_mul(1000).map(Milli).ok_or_else(|| E::custom("decimal out of range"))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Milli, E> {
                i64::try_from(v)
                    .ok()
                    .and_then(|v| v.checked_mul(1000))
                    .map(Milli)
                    .ok_or_else(|| E::custom("decimal out of range"))
            }
        }
        d.deserialize_any(V)
    }
}

/// Integer square root of a `u64` (floor): exact and identical on every
/// CPU (see [`simd_kernels::isqrt`], which corrects a hardware estimate
/// with integer arithmetic; the simulation itself stays float-free).
#[inline]
pub fn isqrt_u64(n: u64) -> u64 {
    simd_kernels::isqrt(n)
}

/// Length of the vector `(dx, dy)` in [`Fx`] units (floor).
#[inline]
pub fn length(dx: i32, dy: i32) -> i32 {
    let d2 = dx as i64 * dx as i64 + dy as i64 * dy as i64;
    isqrt_u64(d2 as u64) as i32
}

/// Squared distance between two points, in raw Q32.32 units.
#[inline]
pub const fn dist2(ax: i32, ay: i32, bx: i32, by: i32) -> i64 {
    let dx = (bx as i64) - (ax as i64);
    let dy = (by as i64) - (ay as i64);
    dx * dx + dy * dy
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn isqrt_small() {
        for n in 0u64..10_000 {
            let r = isqrt_u64(n);
            assert!(r * r <= n && (r + 1) * (r + 1) > n, "n={n} r={r}");
        }
    }

    proptest! {
        #[test]
        fn isqrt_is_floor_sqrt(n in any::<u64>()) {
            let r = isqrt_u64(n) as u128;
            prop_assert!(r * r <= n as u128);
            prop_assert!((r + 1) * (r + 1) > n as u128);
        }
    }

    #[test]
    fn fx_basics() {
        assert_eq!(Fx::from_int(3).mul(Fx::from_ratio(1, 2)), Fx::from_ratio(3, 2));
        assert_eq!(Fx::from_int(2).mul_milli(Milli(1250)), Fx::from_ratio(5, 2));
        assert_eq!(length(3 * ONE, 4 * ONE), 5 * ONE);
        assert_eq!(Fx::from_ratio(3, 2).floor_int(), 1);
        assert_eq!(Fx::from_ratio(-1, 2).floor_int(), -1);
    }

    #[test]
    fn milli_parse_and_print() {
        for (s, v) in [("9.2", 9200), ("1", 1000), ("0.05", 50), ("-1.5", -1500), (".5", 500), ("1,25", 1250)] {
            assert_eq!(Milli::parse(s), Ok(Milli(v)), "{s}");
        }
        assert!(Milli::parse("1.2345").is_err());
        assert!(Milli::parse("abc").is_err());
        assert!(Milli::parse("").is_err());
        assert_eq!(Milli(9200).to_string(), "9.2");
        assert_eq!(Milli(-50).to_string(), "-0.05");
        assert_eq!(Milli(3000).to_string(), "3");
    }
}
