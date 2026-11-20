//! Terrain costs per unit type and straight-line checks. Routes themselves
//! come from flow fields (see `flow` and the `pathing` crate).

use crate::balance::Rules;
use crate::map::{Map, Terrain};
use crate::unit::UnitKind;

/// Extra cost factor for water on top of its slowness: it also hurts.
const WATER_PENALTY: u32 = 3;
const BLOCKED: u32 = u32::MAX;

/// Cost of stepping onto each terrain for a unit kind (1000 = plain).
pub fn terrain_costs(rules: &Rules, kind: UnitKind) -> [u32; Terrain::COUNT] {
    let mut c = [BLOCKED; Terrain::COUNT];
    for t in Terrain::ALL {
        let i = t as usize;
        if !rules.passable[i] {
            continue;
        }
        let speed = rules.terrain_speed[i][kind as usize].0.max(1) as u32;
        let mut cost = 1_000_000 / speed;
        if t == Terrain::Water {
            cost = cost.saturating_mul(WATER_PENALTY);
        }
        c[i] = cost.max(1);
    }
    c
}

#[inline]
fn cost_at(map: &Map, costs: &[u32; Terrain::COUNT], x: u32, y: u32) -> u32 {
    costs[map.terrain_raw(x, y) as usize]
}

/// Tiles on the segment between two tile centres (supercover-ish Bresenham).
fn line_tiles(a: (u32, u32), b: (u32, u32), mut f: impl FnMut(u32, u32) -> bool) -> bool {
    let (mut x, mut y) = (a.0 as i64, a.1 as i64);
    let (x1, y1) = (b.0 as i64, b.1 as i64);
    let dx = (x1 - x).abs();
    let dy = -(y1 - y).abs();
    let sx = if x < x1 { 1 } else { -1 };
    let sy = if y < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        if !f(x as u32, y as u32) {
            return false;
        }
        if x == x1 && y == y1 {
            return true;
        }
        let e2 = 2 * err;
        if e2 >= dy && e2 <= dx {
            // Diagonal step: also check both side tiles so we never cut a corner.
            if !f((x + sx) as u32, y as u32) || !f(x as u32, (y + sy) as u32) {
                return false;
            }
        }
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

/// True if walking straight from `a` to `b` never touches a tile costlier
/// than `max_cost` (so it is no worse than the route it replaces).
pub fn clear_line(map: &Map, costs: &[u32; Terrain::COUNT], a: (u32, u32), b: (u32, u32), max_cost: u32) -> bool {
    line_tiles(a, b, |x, y| cost_at(map, costs, x, y) <= max_cost)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::balance::Balance;

    #[test]
    fn clear_line_sees_walls_and_costly_terrain() {
        let mut m = Map::new(64, 64, Terrain::Plain).unwrap();
        m.set_terrain(30, 10, Terrain::Mountain);
        m.set_terrain(30, 20, Terrain::Forest);
        let rules = Rules::new(&Balance::default_balance());
        let light = terrain_costs(&rules, UnitKind::Light);
        let heavy = terrain_costs(&rules, UnitKind::Heavy);
        assert!(!clear_line(&m, &light, (20, 10), (40, 10), 1000));
        assert!(clear_line(&m, &light, (20, 11), (40, 11), 1000));
        // Forest is plain-cost for light units, twice as costly for heavy ones.
        assert!(clear_line(&m, &light, (20, 20), (40, 20), 1000));
        assert!(!clear_line(&m, &heavy, (20, 20), (40, 20), 1000));
    }
}
