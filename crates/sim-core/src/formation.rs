//! Formations (spec 2.9): slot offsets for a group that moves together.
//!
//! The group faces from its centre towards the target. Slots are laid out
//! in ranks across that direction, and units take them in an order that
//! keeps crossing paths rare: the units furthest forward fill the front
//! rank, each rank from left to right. Integer maths only.

use crate::command::Formation;
use crate::fx::{FxPoint, ONE, isqrt_u64};
use crate::unit::UnitStore;
use std::collections::BTreeMap;

/// Distance between neighbouring slots (1.25 tiles, raw Q16.16).
pub const SPACING: i64 = (ONE as i64) * 5 / 4;

/// `(columns, ranks)` for `n` units.
fn shape(f: Formation, n: i64) -> (i64, i64) {
    let root = isqrt_u64(n as u64) as i64;
    let cols = match f {
        Formation::Keep | Formation::Box => root.max(1),
        // Wide fronts: about four times as wide as deep.
        Formation::Line | Formation::Staggered | Formation::Flank => (2 * root).max(1),
        Formation::Column => (root / 3).max(1),
    };
    let cols = cols.min(n).max(1);
    (cols, (n + cols - 1) / cols)
}

/// Offset (right, forward) of slot `k` in raw units, formation centred.
fn slot(f: Formation, n: i64, k: i64) -> (i64, i64) {
    let (cols, ranks) = shape(f, n);
    let (c, r) = (k % cols, k / cols);
    // The last rank may be short: centre it too.
    let in_rank = if r == ranks - 1 { n - r * cols } else { cols };
    let mut right = (2 * c - (in_rank - 1)) * SPACING / 2;
    let forward = ((ranks - 1) * SPACING) / 2 - r * SPACING;
    match f {
        Formation::Staggered => {
            right *= 2;
            if r % 2 == 1 {
                right += SPACING;
            }
        }
        // A gap of a third of the width in the middle.
        Formation::Flank => {
            let gap = cols * SPACING / 3;
            right += if 2 * c < in_rank { -gap / 2 } else { gap / 2 };
        }
        _ => {}
    }
    (right, forward)
}

/// Offset of every unit in `idx` from the group's target point, as world
/// `(x, y)` raw Q16.16 vectors.
pub fn offsets(
    units: &UnitStore,
    idx: &[usize],
    centre: (i64, i64),
    target: FxPoint,
    f: Formation,
) -> BTreeMap<usize, (i64, i64)> {
    let n = idx.len() as i64;
    // Facing: unit vector (Q16.16) from the centre to the target; north if
    // the target is the centre itself.
    let (dx, dy) = (target.x.0 as i64 - centre.0, target.y.0 as i64 - centre.1);
    let len = isqrt_u64((dx * dx + dy * dy) as u64) as i64;
    let one = ONE as i64;
    let (fx, fy) = if len == 0 { (0, -one) } else { (dx * one / len, dy * one / len) };
    // Right of the facing (screen y grows downwards).
    let (rx, ry) = (-fy, fx);
    // Units furthest forward first; within a rank, left to right.
    let proj = |i: usize, (ax, ay): (i64, i64)| {
        ((units.pos_x[i] as i64 - centre.0) * ax + (units.pos_y[i] as i64 - centre.1) * ay) >> 16
    };
    let mut order: Vec<usize> = idx.to_vec();
    order.sort_by_key(|&i| (-proj(i, (fx, fy)), units.slot[i]));
    let (cols, _) = shape(f, n);
    let mut out = BTreeMap::new();
    for (r, rank) in order.chunks(cols as usize).enumerate() {
        let mut rank = rank.to_vec();
        rank.sort_by_key(|&i| (proj(i, (rx, ry)), units.slot[i]));
        for (c, &i) in rank.iter().enumerate() {
            let (sr, sf) = slot(f, n, r as i64 * cols + c as i64);
            out.insert(i, ((sr * rx + sf * fx) >> 16, (sr * ry + sf * fy) >> 16));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes() {
        assert_eq!(shape(Formation::Line, 100), (20, 5));
        assert_eq!(shape(Formation::Box, 100), (10, 10));
        assert_eq!(shape(Formation::Column, 100), (3, 34));
        assert_eq!(shape(Formation::Line, 2), (2, 1));
        // Every slot distinct, formation centred on the target.
        for f in Formation::ALL {
            for n in [2, 7, 30] {
                let s: Vec<(i64, i64)> = (0..n).map(|k| slot(f, n, k)).collect();
                for a in 0..s.len() {
                    for b in 0..a {
                        assert_ne!(s[a], s[b], "{f:?} n={n}");
                    }
                }
                let mean = s.iter().map(|p| p.0).sum::<i64>() / n;
                assert!(mean.abs() <= SPACING, "{f:?} n={n} off centre by {mean}");
            }
        }
    }
}
