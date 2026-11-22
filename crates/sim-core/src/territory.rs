//! Territory (spec 2.2) and supply pockets (2.4).
//!
//! * Control zones: a tile belongs to a side while that side has a unit
//!   within `territory.radius` of it (or one of its cities within
//!   `territory.city_radius`, or it lies in that side's rear: within
//!   `territory.rear_radius` of two or more of its cities and of no city of
//!   another side) and no other side does. Tiles nobody reaches,
//!   or that two sides reach, are neutral. Zones follow the units: land is
//!   not kept after the army leaves. City tiles always show the city owner.
//! * Pockets: per team, 4-connected components of the land that is not
//!   held by another team (own, allied and neutral land), recomputed every
//!   [`SUPPLY_INTERVAL`] ticks. So only a closed ring of enemy control cuts
//!   a pocket off; neutral land carries supply.

use crate::fx::{ONE, isqrt_u64};
use crate::map::{CHUNK, CHUNK_TILES, Map};

/// Pockets and supply are recomputed every this many ticks (spec 5.3).
pub const SUPPLY_INTERVAL: u32 = 15;
/// "No pocket": the unit stands on land held by another team.
pub const NO_POCKET: u32 = u32::MAX;
/// Each chunk recomputes its zones every this many ticks, staggered in a
/// 2×2 checkerboard (spec 5.3: spread work over ticks). A unit moves at
/// most ~0.3 tiles in that time.
pub const PAINT_INTERVAL: u32 = 4;

/// Whether chunk `(cx, cy)` recomputes its zones on `tick`.
#[inline]
pub fn chunk_paints(cx: u32, cy: u32, tick: u32) -> bool {
    ((cx & 1) | ((cy & 1) << 1)) == tick % PAINT_INTERVAL
}

/// Bit of player `p` (1..=8) in a presence byte.
#[inline]
pub const fn player_bit(p: u8) -> u8 {
    1 << (p - 1)
}

/// Owner of a tile from the players present: the lowest player id if all
/// of them are on one team (`team_mask[bits]` has a single bit), else 0.
#[inline]
pub fn resolve(bits: u8, team_mask: &[u8; 256]) -> u8 {
    if bits != 0 && team_mask[bits as usize].is_power_of_two() { bits.trailing_zeros() as u8 + 1 } else { 0 }
}

/// [`resolve`] for every presence byte, so the per-tile work is one lookup.
pub fn owner_table(team_mask: &[u8; 256]) -> [u8; 256] {
    let mut t = [0u8; 256];
    for (b, o) in t.iter_mut().enumerate() {
        *o = resolve(b as u8, team_mask);
    }
    t
}

/// Stamps `bit` on the tiles of a `side`×`side` plane with origin tile
/// `(ox, oy)` whose centre is within `r_raw` (fixed point) of `(x, y)`.
/// The plane is row-major, like a chunk of the map.
#[allow(clippy::too_many_arguments)]
pub fn stamp(plane: &mut [u8], ox: i32, oy: i32, side: i32, r_raw: i32, x: i32, y: i32, bit: u8) {
    let r2 = (r_raw as i64) * (r_raw as i64);
    let last = side - 1;
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
        let lo = (x as i64 - reach - (ONE / 2) as i64 + (ONE - 1) as i64).div_euclid(ONE as i64) as i32;
        let hi = (x as i64 + reach - (ONE / 2) as i64).div_euclid(ONE as i64) as i32;
        let (x0, x1) = (lo.max(ox), hi.min(ox + last));
        let row = (ty - oy) as usize * side as usize;
        for tx in x0..=x1 {
            plane[row + (tx - ox) as usize] |= bit;
        }
    }
}

/// Player bits of the city zones over chunk `(cx, cy)` (`out` is its
/// slice of the city plane). `cities` are `(x, y, owner, team bit)` of
/// owned cities that may reach the chunk. A tile gets:
/// * the owner of every city within `city_r` tiles;
/// * the rear: if at least two cities lie within `rear_r` tiles and all of
///   them are on one team, the owners of those cities.
pub fn city_zones(out: &mut [u8], cx: u32, cy: u32, cities: &[(u32, u32, u8, u8)], city_r: u32, rear_r: u32) -> bool {
    let out = &mut out[..CHUNK_TILES];
    out.fill(0);
    let (cr2, rr2) = ((city_r * city_r) as i64, (rear_r * rear_r) as i64);
    let mut any = 0u8;
    for ly in 0..CHUNK {
        for lx in 0..CHUNK {
            let (x, y) = ((cx * CHUNK + lx) as i64, (cy * CHUNK + ly) as i64);
            let (mut bits, mut rear_bits, mut teams, mut near) = (0u8, 0u8, 0u8, 0u32);
            for &(ccx, ccy, owner, team) in cities {
                let (dx, dy) = (ccx as i64 - x, ccy as i64 - y);
                let d2 = dx * dx + dy * dy;
                if d2 <= cr2 {
                    bits |= player_bit(owner);
                }
                if d2 <= rr2 {
                    rear_bits |= player_bit(owner);
                    teams |= team;
                    near += 1;
                }
            }
            if near >= 2 && teams.is_power_of_two() {
                bits |= rear_bits;
            }
            out[(ly * CHUNK + lx) as usize] = bits;
            any |= bits;
        }
    }
    any != 0
}

/// Tile offsets within `r` tiles of a tile (centre to centre), the disk a
/// unit standing on that tile controls.
pub fn disk(r: u32) -> Vec<(i32, i32)> {
    let r = r as i32;
    let mut v = Vec::new();
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy <= r * r {
                v.push((dx, dy));
            }
        }
    }
    v
}

/// New owners of the tiles of chunk `(cx, cy)` (its slice of the owner
/// plane, `CHUNK_TILES` long).
///
/// * `units` yields `(x, y, player)` of every unit that may reach the
///   chunk. Units mark their tile first (many units share a tile in a dense
///   army), then each marked tile spreads over `disk` (radius `r` tiles).
/// * `city_bits` is the chunk's slice of the city plane ([`city_zones`]);
///   `city_tiles` the `(x, y, owner)` city tiles inside the chunk.
///
/// `seeds` is scratch. Returns whether anything is owned.
#[allow(clippy::too_many_arguments)]
pub fn chunk_owners(
    out: &mut [u8],
    seeds: &mut Vec<u8>,
    cx: u32,
    cy: u32,
    r: u32,
    disk: &[(i32, i32)],
    owner_of: &[u8; 256],
    units: impl FnOnce(&mut dyn FnMut(i32, i32, u8)),
    city_bits: &[u8],
    city_tiles: &[(u32, u32, u8)],
) -> bool {
    let out = &mut out[..CHUNK_TILES];
    out.fill(0);
    let c = CHUNK as i32;
    let (ox, oy) = (cx as i32 * c, cy as i32 * c);
    // Seed plane: the chunk grown by r on every side.
    let ri = r as i32;
    let side = c + 2 * ri;
    seeds.clear();
    seeds.resize((side * side) as usize, 0);
    units(&mut |x, y, p| {
        let (sx, sy) = ((x >> 16) - ox + ri, (y >> 16) - oy + ri);
        if sx >= 0 && sy >= 0 && sx < side && sy < side {
            seeds[(sy * side + sx) as usize] |= player_bit(p);
        }
    });
    for sy in 0..side {
        let row = &seeds[(sy * side) as usize..((sy + 1) * side) as usize];
        for (sx, &bits) in row.iter().enumerate() {
            if bits == 0 {
                continue;
            }
            let (tx, ty) = (sx as i32 - ri, sy - ri);
            let inner = tx >= ri && ty >= ri && tx < c - ri && ty < c - ri;
            for &(dx, dy) in disk {
                let (x, y) = (tx + dx, ty + dy);
                if inner || (x >= 0 && y >= 0 && x < c && y < c) {
                    out[(y * c + x) as usize] |= bits;
                }
            }
        }
    }
    for (o, &b) in out.iter_mut().zip(city_bits) {
        *o |= b;
    }
    let mut any = 0u8;
    for v in out.iter_mut() {
        *v = owner_of[*v as usize];
        any |= *v;
    }
    let mut any = any != 0;
    for &(x, y, o) in city_tiles {
        out[((y - cy * CHUNK) * CHUNK + (x - cx * CHUNK)) as usize] = o;
        any |= o != 0;
    }
    any
}

/// Connected land of one team: rows of runs of passable tiles, each run
/// with its component label (0.. in scan order).
#[derive(Clone, Debug, Default)]
pub struct TeamPockets {
    /// `rows[y]..rows[y + 1]` index the runs of row `y`.
    rows: Vec<u32>,
    /// `(x0, x1 inclusive, label)`.
    runs: Vec<(u16, u16, u32)>,
    pub count: u32,
}

impl TeamPockets {
    /// Labels the land where `passable(owner)` holds.
    pub fn label(map: &Map, passable: impl Fn(u8) -> bool) -> TeamPockets {
        let (w, h) = (map.width(), map.height());
        let mut rows = Vec::with_capacity(h as usize + 1);
        let mut runs: Vec<(u16, u16, u32)> = Vec::new();
        let mut parent: Vec<u32> = Vec::new();
        fn find(parent: &mut [u32], mut a: u32) -> u32 {
            while parent[a as usize] != a {
                parent[a as usize] = parent[parent[a as usize] as usize];
                a = parent[a as usize];
            }
            a
        }
        let mut prev = 0..0usize;
        for y in 0..h {
            rows.push(runs.len() as u32);
            let start = runs.len();
            let mut x = 0;
            while x < w {
                if !passable(map.owner(x, y)) {
                    x += 1;
                    continue;
                }
                let x0 = x;
                while x < w && passable(map.owner(x, y)) {
                    x += 1;
                }
                let id = runs.len() as u32;
                runs.push((x0 as u16, (x - 1) as u16, id));
                parent.push(id);
                // Join the overlapping runs of the row above (4-connected).
                for k in prev.clone() {
                    let (a0, a1, _) = runs[k];
                    if (a1 as u32) < x0 || a0 as u32 > x - 1 {
                        continue;
                    }
                    let (ra, rb) = (find(&mut parent, k as u32), find(&mut parent, id));
                    if ra != rb {
                        // The older run is the root: keeps labels in scan order.
                        let (lo, hi) = (ra.min(rb), ra.max(rb));
                        parent[hi as usize] = lo;
                    }
                }
            }
            prev = start..runs.len();
        }
        rows.push(runs.len() as u32);
        // Compact roots to labels in order of first appearance.
        let mut label = vec![u32::MAX; runs.len()];
        let mut count = 0;
        for (k, run) in runs.iter_mut().enumerate() {
            let r = find(&mut parent, k as u32) as usize;
            if label[r] == u32::MAX {
                label[r] = count;
                count += 1;
            }
            run.2 = label[r];
        }
        TeamPockets { rows, runs, count }
    }

    /// Label of tile `(x, y)`, or `None` if it is held by another team.
    pub fn at(&self, x: u32, y: u32) -> Option<u32> {
        let (a, b) = (self.rows[y as usize] as usize, self.rows[y as usize + 1] as usize);
        let row = &self.runs[a..b];
        let k = row.partition_point(|r| (r.1 as u32) < x);
        row.get(k).filter(|r| r.0 as u32 <= x).map(|r| r.2)
    }

    /// Label for a unit at `(x, y)` (fixed point): the tile under it, or
    /// the nearest labelled tile within `radius` (fixed scan order) if its
    /// zone has not caught up with it yet.
    pub fn near(&self, map: &Map, x: i32, y: i32, radius: u32) -> Option<u32> {
        let (tx, ty) = (x >> 16, y >> 16);
        let (w, h) = (map.width() as i32, map.height() as i32);
        let r = radius as i32;
        let mut best: Option<(i32, u32)> = None;
        for dy in -r..=r {
            for dx in -r..=r {
                let (nx, ny) = (tx + dx, ty + dy);
                if nx < 0 || ny < 0 || nx >= w || ny >= h {
                    continue;
                }
                let d = dx * dx + dy * dy;
                if best.is_some_and(|b| b.0 <= d) {
                    continue;
                }
                if let Some(l) = self.at(nx as u32, ny as u32) {
                    best = Some((d, l));
                }
            }
        }
        best.map(|b| b.1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fx::dist2;
    use crate::map::Terrain;

    /// The span-based stamp marks exactly the tiles whose centre is within
    /// the radius.
    #[test]
    fn stamp_matches_distance_test() {
        let mut s = 0x9e37_79b9u64;
        for _ in 0..2000 {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            let x = (s % (70 << 16)) as i32 - (3 << 16);
            let y = ((s >> 24) % (70 << 16)) as i32 - (3 << 16);
            for radius in [1i32, 3, 5] {
                let side = 64;
                let mut plane = vec![0u8; 64 * 64];
                stamp(&mut plane, 0, 0, side, radius << 16, x, y, 1);
                for ty in 0..side {
                    for tx in 0..side {
                        let r = (radius as i64) << 16;
                        let want = dist2(x, y, (tx << 16) + ONE / 2, (ty << 16) + ONE / 2) <= r * r;
                        assert_eq!(plane[(ty * side + tx) as usize] == 1, want, "x={x} y={y} r={radius} {tx},{ty}");
                    }
                }
            }
        }
    }

    #[test]
    fn contested_and_empty_land_is_neutral() {
        // Players 1 and 2 on different teams, 3 allied with 1.
        let mut mask = [0u8; 256];
        for (b, m) in mask.iter_mut().enumerate() {
            for (p, bit) in [1u8, 2, 1, 4, 8, 16, 32, 64].into_iter().enumerate() {
                if b & (1 << p) != 0 {
                    *m |= bit;
                }
            }
        }
        assert_eq!(resolve(0, &mask), 0);
        assert_eq!(resolve(0b001, &mask), 1);
        assert_eq!(resolve(0b101, &mask), 1, "allies: the lower id shows");
        assert_eq!(resolve(0b011, &mask), 0, "contested");
        let mut out = vec![0u8; CHUNK_TILES];
        let mut seeds = Vec::new();
        let any = chunk_owners(
            &mut out,
            &mut seeds,
            0,
            0,
            3,
            &disk(3),
            &owner_table(&mask),
            |f| f(10 << 16, 10 << 16, 2),
            &[0; CHUNK_TILES],
            &[(40, 40, 1)],
        );
        assert!(any);
        assert_eq!(out[10 * 64 + 10], 2);
        assert_eq!(out[10 * 64 + 20], 0, "nobody there");
        assert_eq!(out[40 * 64 + 40], 1, "city tile");
    }

    #[test]
    fn pockets_need_a_closed_ring() {
        let mut m = Map::new(64, 64, Terrain::Plain).unwrap();
        // An enemy (2) wall across the map with a gap: one pocket for team 1.
        for y in 0..64 {
            if y != 30 {
                m.set_owner(20, y, 2);
            }
        }
        let p = TeamPockets::label(&m, |o| o != 2);
        assert_eq!(p.count, 1);
        assert_eq!(p.at(5, 5), p.at(50, 50));
        assert_eq!(p.at(20, 5), None);
        // Closing the gap splits the land; neutral land on both sides.
        m.set_owner(20, 30, 2);
        let p = TeamPockets::label(&m, |o| o != 2);
        assert_eq!(p.count, 2);
        assert_ne!(p.at(5, 5), p.at(50, 50));
        assert_eq!(p.near(&m, 20 << 16, 5 << 16, 1), p.at(19, 5));
    }
}

#[cfg(test)]
mod city_tests {
    use super::*;

    #[test]
    fn rear_fills_between_own_cities_only() {
        let mut out = vec![0u8; CHUNK_TILES];
        // Two cities of player 1 (team bit 1), 40 tiles apart: the middle
        // (20 from each) is beyond city_r 15 but in the rear.
        let mine = [(10, 32, 1, 1), (50, 32, 1, 1)];
        assert!(city_zones(&mut out, 0, 0, &mine, 15, 30));
        assert_eq!(out[32 * 64 + 30], 1, "rear between own cities");
        assert_eq!(out[32 * 64 + 12], 1, "near a city");
        // An enemy city nearby: no rear, only the city circles.
        let mixed = [(10, 32, 1, 1), (50, 32, 1, 1), (30, 60, 2, 2)];
        city_zones(&mut out, 0, 0, &mixed, 15, 30);
        assert_eq!(out[32 * 64 + 30], 0, "contested rear stays grey");
        assert_eq!(out[32 * 64 + 12], 1);
        // A single city: no rear beyond its circle.
        city_zones(&mut out, 0, 0, &[(10, 32, 1, 1)], 15, 30);
        assert_eq!(out[32 * 64 + 35], 0);
    }
}
