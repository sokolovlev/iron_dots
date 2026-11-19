//! SIMD kernels for the Iron Dots simulation (spec 5.5).
//!
//! Every kernel exists twice:
//!
//! * a **scalar reference** in [`reference`], written for clarity (wide
//!   integer types, no tricks), and
//! * a **fast version**: one generic body that LLVM auto-vectorises, compiled
//!   once per instruction set with `#[target_feature]` (SSE4.2, AVX2,
//!   AVX-512 on x86-64; NEON on AArch64).
//!
//! All kernels are integer-only, so every level returns bit-identical
//! results; property tests check each available level against the reference.
//! The level is chosen once ([`kernels`]), from CPUID or the `IRON_DOTS_SIMD`
//! environment variable, and calls go through a table of function pointers —
//! no per-call feature checks.

#![deny(unsafe_code)]

use std::sync::OnceLock;

/// Maximum elements per kernel call: hits come back as a 64-bit mask.
/// Callers process longer slices in chunks of this size.
pub const CHUNK: usize = 64;
/// Kernels work in groups of this many elements. Inputs whose length is a
/// multiple of it skip the padded tail path entirely.
pub const GROUP: usize = 16;
/// A coordinate that is never within any realistic radius of a map point
/// (|map coordinate| < 2^28, so dx² + dy² stays below 2^63): use it to pad
/// buffers to a multiple of [`GROUP`].
pub const FAR: i32 = i32::MAX;

/// Instruction-set level a kernel table is compiled for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    /// The scalar reference implementations.
    Scalar,
    /// x86-64-v2: SSE4.2 + POPCNT.
    Sse42,
    /// x86-64-v3: AVX2 + BMI1/2.
    Avx2,
    /// x86-64-v4: AVX-512 F/BW/DQ/VL.
    Avx512,
    /// AArch64 Advanced SIMD (always present on AArch64).
    Neon,
}

impl Level {
    pub const ALL: [Level; 5] = [Level::Scalar, Level::Sse42, Level::Avx2, Level::Avx512, Level::Neon];

    pub fn name(self) -> &'static str {
        match self {
            Level::Scalar => "scalar",
            Level::Sse42 => "sse4.2",
            Level::Avx2 => "avx2",
            Level::Avx512 => "avx512",
            Level::Neon => "neon",
        }
    }

    pub fn parse(s: &str) -> Option<Level> {
        Level::ALL.into_iter().find(|l| l.name().eq_ignore_ascii_case(s))
    }

    /// Whether this CPU can run the level.
    pub fn supported(self) -> bool {
        match self {
            Level::Scalar => true,
            #[cfg(target_arch = "x86_64")]
            Level::Sse42 => {
                std::arch::is_x86_feature_detected!("sse4.2") && std::arch::is_x86_feature_detected!("popcnt")
            }
            #[cfg(target_arch = "x86_64")]
            Level::Avx2 => {
                std::arch::is_x86_feature_detected!("avx2")
                    && std::arch::is_x86_feature_detected!("bmi1")
                    && std::arch::is_x86_feature_detected!("bmi2")
            }
            #[cfg(target_arch = "x86_64")]
            Level::Avx512 => {
                std::arch::is_x86_feature_detected!("avx512f")
                    && std::arch::is_x86_feature_detected!("avx512bw")
                    && std::arch::is_x86_feature_detected!("avx512dq")
                    && std::arch::is_x86_feature_detected!("avx512vl")
            }
            #[cfg(target_arch = "aarch64")]
            Level::Neon => true,
            #[allow(unreachable_patterns)]
            _ => false,
        }
    }

    /// Best level this CPU supports.
    pub fn best() -> Level {
        [Level::Avx512, Level::Avx2, Level::Neon, Level::Sse42]
            .into_iter()
            .find(|l| l.supported())
            .unwrap_or(Level::Scalar)
    }
}

/// `dist² < r2` filter over points `(xs[k], ys[k])` from `(qx, qy)`:
/// bit `k` of the result is set for hits. `r2` is in raw Q32.32 units
/// (coordinates are Q16.16, |coordinate| < 2^28). At most 64 points.
type WithinFn = unsafe fn(&[i32], &[i32], i32, i32, i64) -> u64;
/// Same, only for points whose `teams[k] != team`.
type EnemiesWithinFn = unsafe fn(&[i32], &[i32], &[u8], u8, i32, i32, i64) -> u64;
/// Number of bytes equal to `v`.
type CountEqFn = unsafe fn(&[u8], u8) -> usize;

/// A table of kernels for one [`Level`]. Obtain one with [`Kernels::get`]
/// or [`kernels`]; its methods are safe to call.
pub struct Kernels {
    level: Level,
    within: WithinFn,
    enemies_within: EnemiesWithinFn,
    count_eq: CountEqFn,
}

impl std::fmt::Debug for Kernels {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kernels").field("level", &self.level).finish()
    }
}

impl Kernels {
    pub fn level(&self) -> Level {
        self.level
    }

    /// Kernel table for `level`, or `None` if this CPU cannot run it.
    pub fn get(level: Level) -> Option<&'static Kernels> {
        if !level.supported() {
            return None;
        }
        match level {
            Level::Scalar => Some(&SCALAR),
            #[cfg(target_arch = "x86_64")]
            Level::Sse42 => Some(&x86::SSE42),
            #[cfg(target_arch = "x86_64")]
            Level::Avx2 => Some(&x86::AVX2),
            #[cfg(target_arch = "x86_64")]
            Level::Avx512 => Some(&x86::AVX512),
            #[cfg(target_arch = "aarch64")]
            Level::Neon => Some(&arm::NEON),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// Bit mask of points strictly closer than `sqrt(r2)` to `(qx, qy)`.
    /// Slices must have equal length of at most [`CHUNK`].
    // The only `unsafe` in the workspace: calling a feature-gated kernel.
    #[allow(unsafe_code)]
    #[inline]
    pub fn within(&self, xs: &[i32], ys: &[i32], qx: i32, qy: i32, r2: i64) -> u64 {
        let n = xs.len();
        assert!(n <= CHUNK && ys.len() == n, "kernel input must be ≤ CHUNK and of equal length");
        // SAFETY: `self.within` belongs to a table returned by `Kernels::get`,
        // which only hands out tables whose CPU features were detected.
        unsafe { (self.within)(xs, ys, qx, qy, r2) }
    }

    /// Like [`Kernels::within`], but only points with `teams[k] != team`.
    // The only `unsafe` in the workspace: calling a feature-gated kernel.
    #[allow(unsafe_code)]
    #[allow(clippy::too_many_arguments)]
    #[inline]
    pub fn enemies_within(&self, xs: &[i32], ys: &[i32], teams: &[u8], team: u8, qx: i32, qy: i32, r2: i64) -> u64 {
        let n = xs.len();
        assert!(n <= CHUNK && ys.len() == n && teams.len() == n, "kernel input must be ≤ CHUNK and of equal length");
        // SAFETY: see `within`; the table matches the detected CPU features.
        unsafe { (self.enemies_within)(xs, ys, teams, team, qx, qy, r2) }
    }

    /// Number of bytes in `bytes` equal to `v` (territory per player).
    // The only `unsafe` in the workspace: calling a feature-gated kernel.
    #[allow(unsafe_code)]
    #[inline]
    pub fn count_eq(&self, bytes: &[u8], v: u8) -> usize {
        // SAFETY: see `within`; the table matches the detected CPU features.
        unsafe { (self.count_eq)(bytes, v) }
    }
}

/// The kernel table chosen for this process: `IRON_DOTS_SIMD` if set to a
/// supported level (`scalar`, `sse4.2`, `avx2`, `avx512`, `neon`), otherwise
/// the best level of this CPU. Decided once.
pub fn kernels() -> &'static Kernels {
    static CHOSEN: OnceLock<&'static Kernels> = OnceLock::new();
    CHOSEN.get_or_init(|| {
        let wanted = std::env::var("IRON_DOTS_SIMD").ok().and_then(|s| Level::parse(&s));
        match wanted.and_then(Kernels::get) {
            Some(k) => k,
            None => Kernels::get(Level::best()).expect("the best level is supported"),
        }
    })
}

/// Scalar reference implementations: the definition of what every kernel
/// computes. Written for obviousness, not speed.
pub mod reference {
    pub fn within(xs: &[i32], ys: &[i32], qx: i32, qy: i32, r2: i64) -> u64 {
        let mut mask = 0;
        for k in 0..xs.len() {
            let dx = xs[k] as i128 - qx as i128;
            let dy = ys[k] as i128 - qy as i128;
            if dx * dx + dy * dy < r2 as i128 {
                mask |= 1 << k;
            }
        }
        mask
    }

    pub fn enemies_within(xs: &[i32], ys: &[i32], teams: &[u8], team: u8, qx: i32, qy: i32, r2: i64) -> u64 {
        let mut mask = 0;
        for k in 0..xs.len() {
            let dx = xs[k] as i128 - qx as i128;
            let dy = ys[k] as i128 - qy as i128;
            if teams[k] != team && dx * dx + dy * dy < r2 as i128 {
                mask |= 1 << k;
            }
        }
        mask
    }

    pub fn count_eq(bytes: &[u8], v: u8) -> usize {
        bytes.iter().filter(|&&b| b == v).count()
    }
}

static SCALAR: Kernels = Kernels {
    level: Level::Scalar,
    within: reference::within,
    enemies_within: reference::enemies_within,
    count_eq: reference::count_eq,
};

/// Generic bodies, written so LLVM vectorises them: fixed-width integer
/// lanes, no early exits. Differences fit in `i32` because coordinates are
/// below 2^28, so `dx * dx` is a 32×32→64-bit multiply (`pmuldq`).
#[cfg_attr(not(any(target_arch = "x86_64", target_arch = "aarch64")), allow(dead_code))]
mod generic {
    /// Lanes per inner step: a fixed trip count lets LLVM vectorise the
    /// step (compare, then pack the lane results into mask bits).
    pub const LANES: usize = 16;

    #[inline(always)]
    fn lanes(xs: &[i32; LANES], ys: &[i32; LANES], qx: i32, qy: i32, r2: i64, valid: &[bool; LANES]) -> u64 {
        let mut hit = [false; LANES];
        for l in 0..LANES {
            let dx = xs[l].wrapping_sub(qx) as i64;
            let dy = ys[l].wrapping_sub(qy) as i64;
            hit[l] = (dx * dx + dy * dy < r2) & valid[l];
        }
        let mut m = 0u64;
        for (l, &h) in hit.iter().enumerate() {
            m |= (h as u64) << l;
        }
        m
    }

    #[inline(always)]
    fn group<T: Copy>(v: &[T], base: usize) -> &[T; LANES] {
        v[base..base + LANES].try_into().expect("LANES")
    }

    /// The last partial group, padded (only when `n` is not a multiple of
    /// `LANES`; callers that pad their buffers never take this path).
    #[inline(always)]
    fn tail<T: Copy + Default>(v: &[T], base: usize) -> [T; LANES] {
        let mut a = [T::default(); LANES];
        a[..v.len() - base].copy_from_slice(&v[base..]);
        a
    }

    #[inline(always)]
    fn valid_lanes(count: usize) -> [bool; LANES] {
        let mut v = [false; LANES];
        for (l, x) in v.iter_mut().enumerate() {
            *x = l < count;
        }
        v
    }

    #[inline(always)]
    pub fn within(xs: &[i32], ys: &[i32], qx: i32, qy: i32, r2: i64) -> u64 {
        let n = xs.len();
        let ys = &ys[..n];
        let all = [true; LANES];
        let full = n / LANES * LANES;
        let mut mask = 0u64;
        let mut base = 0;
        while base < full {
            mask |= lanes(group(xs, base), group(ys, base), qx, qy, r2, &all) << base;
            base += LANES;
        }
        if base < n {
            let ok = valid_lanes(n - base);
            mask |= lanes(&tail(xs, base), &tail(ys, base), qx, qy, r2, &ok) << base;
        }
        mask
    }

    #[inline(always)]
    pub fn enemies_within(xs: &[i32], ys: &[i32], teams: &[u8], team: u8, qx: i32, qy: i32, r2: i64) -> u64 {
        let n = xs.len();
        let (ys, teams) = (&ys[..n], &teams[..n]);
        let full = n / LANES * LANES;
        let mut mask = 0u64;
        let mut base = 0;
        while base < n {
            let (gx, gy, gt, count);
            if base < full {
                (gx, gy, gt, count) = (*group(xs, base), *group(ys, base), *group(teams, base), LANES);
            } else {
                (gx, gy, gt, count) = (tail(xs, base), tail(ys, base), tail(teams, base), n - base);
            }
            let valid = valid_lanes(count);
            let mut ok = [false; LANES];
            for l in 0..LANES {
                ok[l] = valid[l] & (gt[l] != team);
            }
            mask |= lanes(&gx, &gy, qx, qy, r2, &ok) << base;
            base += LANES;
        }
        mask
    }

    #[inline(always)]
    pub fn count_eq(bytes: &[u8], v: u8) -> usize {
        // Count in u8 lanes over 255-byte runs, then widen: vectorises to
        // cmpeq + subtract + horizontal sums.
        let mut total = 0usize;
        for run in bytes.chunks(255) {
            let mut c = 0u8;
            for &b in run {
                c += (b == v) as u8;
            }
            total += c as usize;
        }
        total
    }
}

/// Instantiates the generic bodies with a set of target features and
/// builds the kernel table for them.
macro_rules! level {
    ($table:ident, $level:expr, $features:literal) => {
        #[target_feature(enable = $features)]
        fn within(xs: &[i32], ys: &[i32], qx: i32, qy: i32, r2: i64) -> u64 {
            crate::generic::within(xs, ys, qx, qy, r2)
        }
        #[target_feature(enable = $features)]
        fn enemies_within(xs: &[i32], ys: &[i32], teams: &[u8], team: u8, qx: i32, qy: i32, r2: i64) -> u64 {
            crate::generic::enemies_within(xs, ys, teams, team, qx, qy, r2)
        }
        #[target_feature(enable = $features)]
        fn count_eq(bytes: &[u8], v: u8) -> usize {
            crate::generic::count_eq(bytes, v)
        }
        pub static $table: crate::Kernels = crate::Kernels { level: $level, within, enemies_within, count_eq };
    };
}

#[cfg(target_arch = "x86_64")]
mod x86 {
    pub mod sse42 {
        level!(TABLE, crate::Level::Sse42, "sse4.2,popcnt");
    }
    pub mod avx2 {
        level!(TABLE, crate::Level::Avx2, "avx2,bmi1,bmi2,popcnt");
    }
    pub mod avx512 {
        level!(TABLE, crate::Level::Avx512, "avx512f,avx512bw,avx512dq,avx512vl,avx2,bmi1,bmi2,popcnt");
    }
    pub use avx2::TABLE as AVX2;
    pub use avx512::TABLE as AVX512;
    pub use sse42::TABLE as SSE42;
}

#[cfg(target_arch = "aarch64")]
mod arm {
    pub mod neon {
        level!(TABLE, crate::Level::Neon, "neon");
    }
    pub use neon::TABLE as NEON;
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const COORD: i32 = 1 << 28;

    fn levels() -> Vec<&'static Kernels> {
        Level::ALL.into_iter().filter_map(Kernels::get).collect()
    }

    #[test]
    fn best_level_is_supported_and_listed() {
        let best = Level::best();
        assert!(best.supported());
        assert!(levels().iter().any(|k| k.level() == best));
        println!("available: {:?}", levels().iter().map(|k| k.level().name()).collect::<Vec<_>>());
    }

    #[test]
    fn edge_cases() {
        for k in levels() {
            // Exactly on the radius is not "within" (strict).
            assert_eq!(k.within(&[3], &[4], 0, 0, 25), 0);
            assert_eq!(k.within(&[3], &[4], 0, 0, 26), 1);
            // Extreme coordinates must not overflow.
            let far = COORD - 1;
            assert_eq!(k.within(&[far, 0], &[far, 0], 0, 0, i64::MAX), 0b11);
            assert_eq!(k.within(&[], &[], 0, 0, 1), 0);
            // Padding with FAR never hits, even with a huge radius.
            assert_eq!(k.within(&[FAR, 0], &[FAR, 0], 0, 0, 1 << 40), 0b10);
            // A full chunk sets bit 63.
            assert_eq!(k.within(&[0; 64], &[0; 64], 0, 0, 1), u64::MAX);
            assert_eq!(k.enemies_within(&[0; 3], &[0; 3], &[1, 2, 1], 1, 0, 0, 1), 0b010);
            assert_eq!(k.count_eq(&[1, 2, 1, 1], 1), 3);
            assert_eq!(k.count_eq(&vec![7u8; 100_000], 7), 100_000);
        }
    }

    proptest! {
        #[test]
        fn within_matches_reference(
            pts in proptest::collection::vec((0..COORD, 0..COORD), 0..=CHUNK),
            qx in 0..COORD, qy in 0..COORD, r2 in 0i64..(1i64 << 58),
        ) {
            let xs: Vec<i32> = pts.iter().map(|p| p.0).collect();
            let ys: Vec<i32> = pts.iter().map(|p| p.1).collect();
            let want = reference::within(&xs, &ys, qx, qy, r2);
            for k in levels() {
                prop_assert_eq!(k.within(&xs, &ys, qx, qy, r2), want, "{}", k.level().name());
            }
        }

        #[test]
        fn enemies_within_matches_reference(
            pts in proptest::collection::vec((0..COORD, 0..COORD, 0u8..4), 0..=CHUNK),
            team in 0u8..4, qx in 0..COORD, qy in 0..COORD, r2 in 0i64..(1i64 << 58),
        ) {
            let xs: Vec<i32> = pts.iter().map(|p| p.0).collect();
            let ys: Vec<i32> = pts.iter().map(|p| p.1).collect();
            let ts: Vec<u8> = pts.iter().map(|p| p.2).collect();
            let want = reference::enemies_within(&xs, &ys, &ts, team, qx, qy, r2);
            for k in levels() {
                prop_assert_eq!(k.enemies_within(&xs, &ys, &ts, team, qx, qy, r2), want, "{}", k.level().name());
            }
        }

        #[test]
        fn count_eq_matches_reference(bytes in proptest::collection::vec(0u8..4, 0..2000), v in 0u8..4) {
            let want = reference::count_eq(&bytes, v);
            for k in levels() {
                prop_assert_eq!(k.count_eq(&bytes, v), want, "{}", k.level().name());
            }
        }
    }
}
