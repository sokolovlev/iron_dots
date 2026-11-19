//! Territory (spec 2.2) and supply pockets (2.4).
//!
//! * Painting: every unit claims tiles within `territory.radius` that are not
//!   already its own or an ally's, unless an enemy unit is within the radius
//!   of that tile. City tiles always belong to the city's owner.
//! * Pockets: 4-connected components of each player's territory, recomputed
//!   every [`SUPPLY_INTERVAL`] ticks. Supply limits are counted per pocket.

use crate::fx::{ONE, dist2};
use crate::grid::Grid;
use crate::map::{CHUNK_SHIFT, Map, Terrain};
use crate::unit::UnitStore;

/// Pockets and supply are recomputed every this many ticks (spec 5.3).
pub const SUPPLY_INTERVAL: u32 = 15;
/// "No pocket": unit is not on or next to its own land.
pub const NO_POCKET: u32 = u32::MAX;
/// Each unit paints every this many ticks, staggered by its slot (spec 5.3:
/// spread work over ticks). A unit moves at most ~0.3 tiles in that time,
/// far less than the territory radius, so no gaps appear.
pub const PAINT_INTERVAL: u32 = 4;

/// Tiles unit `i` would claim, judged on the map as it is now: within
/// `radius`, not the unit's own or an ally's, not a city, and with no enemy
/// unit within `radius` of the tile. Appends `(x, y, owner)` to `out`.
/// `hot = false` means no enemy can be near (sleeping chunk); the enemy
/// search is skipped then.
#[allow(clippy::too_many_arguments)]
pub fn paint_candidates(
    map: &Map,
    units: &UnitStore,
    grid: &Grid,
    team: &[u8],
    radius: u32,
    i: usize,
    hot: bool,
    enemies: &mut Vec<(i32, i32)>,
    out: &mut Vec<(u16, u16, u8)>,
) {
    let r = radius as i32;
    let r_raw = r * ONE;
    let r2 = (r_raw as i64) * (r_raw as i64);
    let (w, h) = (map.width() as i32, map.height() as i32);
    let me = units.owner[i];
    let my_team = team[me as usize];
    let (ux, uy) = (units.pos_x[i], units.pos_y[i]);
    let (tx, ty) = (ux >> 16, uy >> 16);
    let claimable = |x: i32, y: i32| -> bool {
        if x < 0 || y < 0 || x >= w || y >= h {
            return false;
        }
        let o = map.owner(x as u32, y as u32);
        !(o != 0 && team[o as usize] == my_team) && map.terrain(x as u32, y as u32) != Terrain::City
    };
    // Quick pass: anything in reach that is not ours or an ally's?
    let mut needs = false;
    'scan: for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy <= r * r && claimable(tx + dx, ty + dy) {
                needs = true;
                break 'scan;
            }
        }
    }
    if !needs {
        return;
    }
    // Enemies that could be within the radius of any tile we touch.
    enemies.clear();
    if hot {
        grid.for_each_in_range(ux, uy, 2 * r_raw + ONE, |j| {
            if team[units.owner[j] as usize] != my_team {
                enemies.push((units.pos_x[j], units.pos_y[j]));
            }
        });
    }
    for dy in -r..=r {
        for dx in -r..=r {
            let (x, y) = (tx + dx, ty + dy);
            if dx * dx + dy * dy > r * r || !claimable(x, y) {
                continue;
            }
            let (cx, cy) = ((x << 16) + ONE / 2, (y << 16) + ONE / 2);
            // Any enemy within the radius of the tile centre. Scalar on
            // purpose: the early exit beats the SIMD kernel here (measured).
            if enemies.iter().any(|&(ex, ey)| dist2(ex, ey, cx, cy) <= r2) {
                continue;
            }
            out.push((x as u16, y as u16, me));
        }
    }
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
