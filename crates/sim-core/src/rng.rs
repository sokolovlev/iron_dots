//! Counter-based randomness: `hash(seed, tick, unit, purpose)`.
//!
//! There is no shared RNG state, so the result is the same no matter which
//! thread computes it or in which order units are visited.

/// What a random number is used for. Each purpose gets an independent stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
pub enum Purpose {
    Damage = 1,
    Morale = 2,
    Environment = 3,
    Separation = 4,
    MapGen = 5,
    Scenario = 6,
}

#[inline]
pub const fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A 64-bit random value for `(seed, tick, id, purpose)`.
#[inline]
pub const fn rand(seed: u64, tick: u32, id: u32, purpose: Purpose) -> u64 {
    let a = splitmix64(seed ^ (purpose as u64).wrapping_mul(0xD6E8_FEB8_6659_FD93));
    let b = splitmix64(a ^ ((tick as u64) << 32 | id as u64));
    splitmix64(b)
}

/// Converts a Q16 fractional amount into an integer without bias:
/// `floor(q16 / 65536)` plus one with probability `frac / 65536`.
/// Expected value is exact, result is deterministic.
#[inline]
pub const fn stochastic_round_q16(q16: u64, r: u64) -> u64 {
    let whole = q16 >> 16;
    let frac = q16 & 0xFFFF;
    whole + ((r & 0xFFFF) < frac) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streams_differ() {
        assert_ne!(rand(1, 0, 0, Purpose::Damage), rand(1, 0, 0, Purpose::Morale));
        assert_ne!(rand(1, 0, 0, Purpose::Damage), rand(1, 1, 0, Purpose::Damage));
        assert_eq!(rand(7, 3, 9, Purpose::Damage), rand(7, 3, 9, Purpose::Damage));
    }

    #[test]
    fn stochastic_round_is_unbiased() {
        // 2.25 per call on average over many draws.
        let q = (2 << 16) + (1 << 14);
        let total: u64 = (0..100_000u32).map(|i| stochastic_round_q16(q, rand(42, i, 0, Purpose::Damage))).sum();
        assert!((224_000..226_000).contains(&total), "total={total}");
    }
}
