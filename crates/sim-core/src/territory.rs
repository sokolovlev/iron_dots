//! Territory (spec 2.2) and supply pockets (2.4).
//!
//! * Painting: every unit claims tiles within `territory.radius` that are not
//!   already its own or an ally's, unless an enemy unit is within the radius
//!   of that tile. City tiles always belong to the city's owner.
//! * Pockets: 4-connected components of each player's territory, recomputed
//!   every [`SUPPLY_INTERVAL`] ticks. Supply limits are counted per pocket.

use crate::fx::{ONE, isqrt_u64};
use crate::map::{CHUNK, CHUNK_SHIFT, Map, Terrain};
use crate::unit::UnitStore;

/// Pockets and supply are recomputed every this many ticks (spec 5.3).
pub const SUPPLY_INTERVAL: u32 = 15;
/// "No pocket": unit is not on or next to its own land.
pub const NO_POCKET: u32 = u32::MAX;
/// Units paint every this many ticks, staggered by chunk in a 2×2
/// checkerboard (spec 5.3: spread work over ticks). A unit moves at most
/// ~0.3 tiles in that time, far less than the territory radius, so no gaps
/// appear; each tick a quarter of the chunks paint, so the load stays even.
pub const PAINT_INTERVAL: u32 = 4;

/// Whether units in chunk `(cx, cy)` paint on `tick`.
#[inline]
pub fn chunk_paints(cx: u32, cy: u32, tick: u32) -> bool {
    ((cx & 1) | ((cy & 1) << 1)) == tick % PAINT_INTERVAL
}

/// Side of a presence plane: a chunk plus the radius on every side, so
/// every tile a unit of the chunk can claim is covered.
#[inline]
pub fn presence_side(radius: u32) -> usize {
    (CHUNK + 2 * radius) as usize
}

/// Tiles unit `i` would claim, judged on the map as it is now: within
/// `radius`, not the unit's own or an ally's, not a city, and with no enemy
/// unit within `radius` of the tile. Appends `(x, y, owner)` to `out`.
///
/// `presence`, given when the unit's chunk is hot, is that chunk's plane
/// from [`stamp_presence`]: the team bits of all units within `radius` of
/// each tile centre. `my_bit` is the unit's team bit. Without it (sleeping
/// chunk) no enemy can be near, so no check is needed.
#[allow(clippy::too_many_arguments)]
pub fn paint_candidates(
    map: &Map,
    units: &UnitStore,
    team: &[u8],
    radius: u32,
    i: usize,
    presence: Option<&[u8]>,
    my_bit: u8,
    out: &mut Vec<(u16, u16, u8)>,
) {
    let r = radius as i32;
    let (w, h) = (map.width() as i32, map.height() as i32);
    let me = units.owner[i];
    let my_team = team[me as usize];
    let (tx, ty) = (units.pos_x[i] >> 16, units.pos_y[i] >> 16);
    // Origin of the presence plane of this unit's chunk.
    let side = presence_side(radius) as i32;
    let (px0, py0) = ((tx & !(CHUNK as i32 - 1)) - r, (ty & !(CHUNK as i32 - 1)) - r);
    for dy in -r..=r {
        for dx in -r..=r {
            let (x, y) = (tx + dx, ty + dy);
            if dx * dx + dy * dy > r * r || x < 0 || y < 0 || x >= w || y >= h {
                continue;
            }
            let (xu, yu) = (x as u32, y as u32);
            let o = map.owner(xu, yu);
            if (o != 0 && team[o as usize] == my_team) || map.terrain(xu, yu) == Terrain::City {
                continue;
            }
            if let Some(p) = presence {
                if p[((y - py0) * side + (x - px0)) as usize] & !my_bit != 0 {
                    continue; // an enemy is within the radius of this tile
                }
            }
            out.push((x as u16, y as u16, me));
        }
    }
}

/// Fills `plane` (chunk `(cx, cy)` grown by `radius` tiles on every side,
/// [`presence_side`]² tiles, row-major) with the team bits of units within
/// `radius` of each tile centre (distance ≤ radius, the same test painting
/// uses). `units_near` yields `(x, y, team_bit)` of every unit that may be
/// that close (callers pass the grid neighbourhood).
pub fn stamp_presence(
    plane: &mut Vec<u8>,
    cx: u32,
    cy: u32,
    radius: u32,
    units_near: impl FnOnce(&mut dyn FnMut(i32, i32, u8)),
) {
    let side = presence_side(radius);
    plane.clear();
    plane.resize(side * side, 0);
    let r_raw = radius as i32 * ONE;
    let r2 = (r_raw as i64) * (r_raw as i64);
    let (ox, oy) = ((cx * CHUNK) as i32 - radius as i32, (cy * CHUNK) as i32 - radius as i32);
    let last = side as i32 - 1;
    units_near(&mut |x, y, bit| {
        let y0 = ((y - r_raw) >> 16).max(oy);
        let y1 = ((y + r_raw) >> 16).min(oy + last);
        for ty in y0..=y1 {
            // Tiles of this row whose centre is within the radius: |cx - x|
            // ≤ isqrt(r² - dy²) (exact for integers), i.e. one span.
            let dy = ((ty << 16) + ONE / 2 - y) as i64;
            let rem = r2 - dy * dy;
            if rem < 0 {
                continue;
            }
            let reach = isqrt_u64(rem as u64) as i64;
            // Centre (tx << 16) + ONE/2 must lie in [x - reach, x + reach].
            let lo = (x as i64 - reach - (ONE / 2) as i64 + (ONE - 1) as i64).div_euclid(ONE as i64) as i32;
            let hi = (x as i64 + reach - (ONE / 2) as i64).div_euclid(ONE as i64) as i32;
            let (x0, x1) = (lo.max(ox), hi.min(ox + last));
            let row = (ty - oy) as usize * side;
            for tx in x0..=x1 {
                plane[row + (tx - ox) as usize] |= bit;
            }
        }
    });
}

/// Applies candidate writes block by block, in order. A tile is skipped if
/// an earlier write this tick gave it to an ally of the writer, which is
/// exactly what a sequential pass over the units would do. Returns true if
/// any tile changed and marks changed chunks in `dirty`.
pub fn apply_paint(map: &mut Map, team: &[u8], blocks: &[Vec<(u16, u16, u8)>], dirty: &mut [bool]) -> bool {
    let chunks_w = map.chunks_w() as usize;
    let mut changed = false;
    for &(x, y, me) in blocks.iter().flatten() {
        let (x, y) = (x as u32, y as u32);
        let o = map.owner(x, y);
        if o != 0 && team[o as usize] == team[me as usize] {
            continue;
        }
        map.set_owner(x, y, me);
        dirty[(y as usize >> CHUNK_SHIFT) * chunks_w + (x as usize >> CHUNK_SHIFT)] = true;
        changed = true;
    }
    changed
}

/// Labels 4-connected components of equal non-zero owner. `labels` gets one
/// entry per tile (row-major `y * width + x`), [`NO_POCKET`] for unowned
/// tiles. Returns the owner of each label. Scan order is fixed, so labels
/// are deterministic.
pub fn label_pockets(map: &Map, labels: &mut Vec<u32>, stack: &mut Vec<(u32, u32)>) -> Vec<u8> {
    let (w, h) = (map.width(), map.height());
    labels.clear();
    labels.resize((w * h) as usize, NO_POCKET);
    let mut owners = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let o = map.owner(x, y);
            if o == 0 || labels[(y * w + x) as usize] != NO_POCKET {
                continue;
            }
            let label = owners.len() as u32;
            owners.push(o);
            stack.clear();
            stack.push((x, y));
            labels[(y * w + x) as usize] = label;
            while let Some((cx, cy)) = stack.pop() {
                let mut visit = |nx: u32, ny: u32| {
                    let k = (ny * w + nx) as usize;
                    if labels[k] == NO_POCKET && map.owner(nx, ny) == o {
                        labels[k] = label;
                        stack.push((nx, ny));
                    }
                };
                if cx > 0 {
                    visit(cx - 1, cy);
                }
                if cx + 1 < w {
                    visit(cx + 1, cy);
                }
                if cy > 0 {
                    visit(cx, cy - 1);
                }
                if cy + 1 < h {
                    visit(cx, cy + 1);
                }
            }
        }
    }
    owners
}

/// Pocket of a unit: the pocket under it if it stands on its own land,
/// otherwise the first own pocket within `radius` tiles (fixed scan order),
/// otherwise [`NO_POCKET`].
pub fn unit_pocket(map: &Map, labels: &[u32], owner: u8, x: i32, y: i32, radius: u32) -> u32 {
    let (w, h) = (map.width() as i32, map.height() as i32);
    let (tx, ty) = (x >> 16, y >> 16);
    let r = radius as i32;
    let mut best = (i32::MAX, NO_POCKET);
    for dy in -r..=r {
        for dx in -r..=r {
            let (nx, ny) = (tx + dx, ty + dy);
            if nx < 0 || ny < 0 || nx >= w || ny >= h {
                continue;
            }
            let d = dx * dx + dy * dy;
            if d < best.0 && map.owner(nx as u32, ny as u32) == owner {
                best = (d, labels[(ny * w + nx) as usize]);
            }
        }
    }
    best.1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fx::dist2;

    /// The span-based stamp marks exactly the tiles whose centre is within
    /// the radius (the test painting uses).
    #[test]
    fn stamp_matches_distance_test() {
        let mut s = 0x9e37_79b9u64;
        for _ in 0..2000 {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            let x = (s % (70 << 16)) as i32 - (3 << 16);
            let y = ((s >> 24) % (70 << 16)) as i32 - (3 << 16);
            for radius in [1u32, 3, 5] {
                let mut plane = Vec::new();
                stamp_presence(&mut plane, 0, 0, radius, |f| f(x, y, 1));
                let (r, side) = ((radius as i64) << 16, presence_side(radius) as i32);
                for ly in 0..side {
                    for lx in 0..side {
                        let (tx, ty) = (lx - radius as i32, ly - radius as i32);
                        let want = dist2(x, y, (tx << 16) + ONE / 2, (ty << 16) + ONE / 2) <= r * r;
                        assert_eq!(
                            plane[(ly * side + lx) as usize] == 1,
                            want,
                            "x={x} y={y} r={radius} tile {tx},{ty}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn labels_split_by_enemy_land() {
        let mut m = Map::new(64, 64, Terrain::Plain).unwrap();
        for y in 0..10 {
            for x in 0..30 {
                m.set_owner(x, y, if x == 15 { 2 } else { 1 });
            }
        }
        let (mut labels, mut stack) = (Vec::new(), Vec::new());
        let owners = label_pockets(&m, &mut labels, &mut stack);
        assert_eq!(owners, vec![1, 2, 1]);
        assert_ne!(labels[5], labels[20]);
        assert_eq!(unit_pocket(&m, &labels, 1, 40 << 16, 5 << 16, 3), NO_POCKET);
        assert_eq!(unit_pocket(&m, &labels, 1, 31 << 16, 5 << 16, 3), labels[29]);
    }
}
