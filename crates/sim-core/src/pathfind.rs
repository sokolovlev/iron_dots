//! Simple pathfinding for stage 2: A* on the tile grid (8 directions, no
//! corner cutting) with per-unit-type terrain costs, then line-of-sight
//! smoothing so a long route becomes a handful of waypoints.
//!
//! Stage 5 replaces this with hierarchical flow fields (spec 5.4); the
//! interface (tile route in, waypoints out) stays the same.

use crate::balance::Rules;
use crate::map::{Map, Terrain};
use crate::unit::UnitKind;
use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// Upper bound on expanded nodes per search.
pub const MAX_EXPANDED: u32 = 20_000;
/// Expansions allowed per tick over all searches. Commands beyond the budget
/// walk straight (deterministic: it depends only on the command order).
pub const TICK_BUDGET: u32 = 200_000;
/// The search is limited to the bounding box of start and goal grown by at
/// least this many tiles...
pub const WINDOW_MARGIN: u32 = 64;
/// ...and never larger than this per side. Farther targets walk straight
/// until hierarchical pathfinding (stage 5).
pub const MAX_WINDOW: u32 = 512;
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

/// Reusable search buffers, so a search allocates nothing in steady state.
/// Entries are valid only when `stamp[i] == epoch`, so nothing is cleared.
#[derive(Default)]
pub struct Scratch {
    g: Vec<u32>,
    parent: Vec<u32>,
    stamp: Vec<u32>,
    epoch: u32,
    heap: BinaryHeap<Reverse<(u64, u32)>>,
    /// Expansions left in this tick's budget.
    pub budget: u32,
}

impl Scratch {
    fn begin(&mut self, n: usize) {
        if self.stamp.len() < n {
            self.g.resize(n, 0);
            self.parent.resize(n, 0);
            self.stamp.resize(n, 0);
        }
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            self.stamp.fill(0);
            self.epoch = 1;
        }
        self.heap.clear();
    }
}

/// A* from tile `from` to tile `to`. Returns the tile route including both
/// ends, or `None` if unreachable within the search window, out of
/// [`MAX_EXPANDED`] nodes, or out of the tick budget in `scratch`.
/// Ties are broken by tile index (deterministic).
pub fn find_route(
    map: &Map,
    costs: &[u32; Terrain::COUNT],
    from: (u32, u32),
    to: (u32, u32),
    scratch: &mut Scratch,
) -> Option<Vec<(u32, u32)>> {
    if cost_at(map, costs, to.0, to.1) == BLOCKED || scratch.budget == 0 {
        return None;
    }
    // Search window: bounding box grown by max(margin, span) on every side,
    // so detours up to the length of the route itself are found.
    let span = from.0.abs_diff(to.0).max(from.1.abs_diff(to.1));
    let m = WINDOW_MARGIN.max(span);
    let x0 = from.0.min(to.0).saturating_sub(m);
    let y0 = from.1.min(to.1).saturating_sub(m);
    let x1 = (from.0.max(to.0) + m).min(map.width() - 1);
    let y1 = (from.1.max(to.1) + m).min(map.height() - 1);
    let (w, h) = (x1 - x0 + 1, y1 - y0 + 1);
    if w > MAX_WINDOW || h > MAX_WINDOW {
        return None;
    }
    let local = |x: u32, y: u32| ((y - y0) * w + (x - x0)) as usize;
    let min_cost = costs.iter().copied().filter(|&c| c != BLOCKED).min().unwrap_or(1) as u64;
    let heur = |x: u32, y: u32| -> u64 {
        let dx = (x as i64 - to.0 as i64).unsigned_abs();
        let dy = (y as i64 - to.1 as i64).unsigned_abs();
        let (lo, hi) = (dx.min(dy), dx.max(dy));
        // Octile distance: diagonal steps cost ~1.414 (1414/1000).
        (hi - lo) * min_cost + lo * min_cost * 1414 / 1000
    };
    scratch.begin((w * h) as usize);
    let epoch = scratch.epoch;
    let Scratch { g, parent, stamp, heap, budget, .. } = scratch;
    let start = local(from.0, from.1);
    g[start] = 0;
    parent[start] = u32::MAX;
    stamp[start] = epoch;
    heap.push(Reverse((heur(from.0, from.1), start as u32)));
    let limit = MAX_EXPANDED.min(*budget);
    let mut expanded = 0u32;
    const DIRS: [(i32, i32); 8] = [(1, 0), (-1, 0), (0, 1), (0, -1), (1, 1), (1, -1), (-1, 1), (-1, -1)];
    let result = loop {
        let Some(Reverse((f, node))) = heap.pop() else { break None };
        let (x, y) = (x0 + node % w, y0 + node / w);
        let gn = g[node as usize] as u64;
        if f > gn + heur(x, y) {
            continue; // stale entry
        }
        if (x, y) == to {
            let mut route = vec![(x, y)];
            let mut cur = node;
            while parent[cur as usize] != u32::MAX {
                cur = parent[cur as usize];
                route.push((x0 + cur % w, y0 + cur / w));
            }
            route.reverse();
            break Some(route);
        }
        expanded += 1;
        if expanded > limit {
            break None;
        }
        for (dx, dy) in DIRS {
            let (nx, ny) = (x as i32 + dx, y as i32 + dy);
            if nx < x0 as i32 || ny < y0 as i32 || nx > x1 as i32 || ny > y1 as i32 {
                continue;
            }
            let (nx, ny) = (nx as u32, ny as u32);
            let c = cost_at(map, costs, nx, ny);
            if c == BLOCKED {
                continue;
            }
            let diagonal = dx != 0 && dy != 0;
            if diagonal && (cost_at(map, costs, nx, y) == BLOCKED || cost_at(map, costs, x, ny) == BLOCKED) {
                continue;
            }
            let step = if diagonal { c as u64 * 1414 / 1000 } else { c as u64 };
            let ng = gn + step;
            let ni = local(nx, ny);
            if stamp[ni] != epoch || ng < g[ni] as u64 {
                stamp[ni] = epoch;
                g[ni] = ng.min(u32::MAX as u64 - 1) as u32;
                parent[ni] = node;
                heap.push(Reverse((ng + heur(nx, ny), ni as u32)));
            }
        }
    };
    *budget = budget.saturating_sub(expanded);
    result
}

/// Turns a tile route into few waypoints: keep a point only when the
/// straight line from the previous kept point would be worse than the route.
pub fn smooth(map: &Map, costs: &[u32; Terrain::COUNT], route: &[(u32, u32)]) -> Vec<(u32, u32)> {
    if route.len() <= 2 {
        return route.to_vec();
    }
    let mut out = vec![route[0]];
    let mut anchor = 0;
    let mut i = 1;
    while i < route.len() {
        // Most expensive tile of the route between anchor and i.
        let max_cost = route[anchor..=i].iter().map(|&(x, y)| cost_at(map, costs, x, y)).max().unwrap_or(0);
        // Consecutive route tiles are always a valid step (A* checked it),
        // even when the line test is stricter (e.g. costlier side tiles).
        if i == anchor + 1 || clear_line(map, costs, route[anchor], route[i], max_cost) {
            i += 1;
            continue;
        }
        anchor = i - 1;
        out.push(route[anchor]);
    }
    out.push(*route.last().expect("route is not empty"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::balance::Balance;

    fn big_budget() -> Scratch {
        Scratch { budget: TICK_BUDGET, ..Default::default() }
    }

    #[test]
    fn budget_is_respected_and_scratch_reused() {
        let mut m = Map::new(256, 256, Terrain::Plain).unwrap();
        for y in 0..60 {
            m.set_terrain(128, y, Terrain::Mountain);
        }
        let rules = Rules::new(&Balance::default_balance());
        let costs = terrain_costs(&rules, UnitKind::Light);
        let mut s = big_budget();
        let a = find_route(&m, &costs, (100, 10), (150, 10), &mut s).unwrap();
        // Same search again on the reused buffers gives the same route.
        let b = find_route(&m, &costs, (100, 10), (150, 10), &mut s).unwrap();
        assert_eq!(a, b);
        assert!(s.budget < TICK_BUDGET);
        s.budget = 10;
        assert!(find_route(&m, &costs, (100, 10), (150, 10), &mut s).is_none());
        assert_eq!(s.budget, 0);
    }

    #[test]
    fn goes_around_a_wall() {
        let mut m = Map::new(64, 64, Terrain::Plain).unwrap();
        for y in 0..60 {
            m.set_terrain(30, y, Terrain::Mountain);
        }
        let rules = Rules::new(&Balance::default_balance());
        let costs = terrain_costs(&rules, UnitKind::Light);
        let route = find_route(&m, &costs, (20, 10), (40, 10), &mut big_budget()).unwrap();
        assert!(route.iter().all(|&(x, y)| m.terrain(x, y) != Terrain::Mountain));
        assert!(route.iter().any(|&(_, y)| y >= 60), "must pass below the wall");
        let wp = smooth(&m, &costs, &route);
        assert!(wp.len() <= 4, "too many waypoints: {wp:?}");
        assert_eq!(*wp.last().unwrap(), (40, 10));
    }

    #[test]
    fn heavy_avoids_forest_light_does_not() {
        let mut m = Map::new(64, 64, Terrain::Plain).unwrap();
        for y in 15..26 {
            for x in 25..35 {
                m.set_terrain(x, y, Terrain::Forest);
            }
        }
        let rules = Rules::new(&Balance::default_balance());
        let through = |kind| {
            let costs = terrain_costs(&rules, kind);
            let r = find_route(&m, &costs, (20, 20), (40, 20), &mut big_budget()).unwrap();
            r.iter().filter(|&&(x, y)| m.terrain(x, y) == Terrain::Forest).count()
        };
        assert!(through(UnitKind::Light) > 0);
        assert_eq!(through(UnitKind::Heavy), 0);
    }

    #[test]
    fn smoothing_terminates_next_to_costly_terrain() {
        // Diagonal steps between forest tiles used to stall the smoother.
        let mut m = Map::new(64, 64, Terrain::Plain).unwrap();
        for k in 0..30 {
            m.set_terrain(10 + k, 10 + k, Terrain::Forest);
            m.set_terrain(11 + k, 10 + k, Terrain::Water);
        }
        let rules = Rules::new(&Balance::default_balance());
        for kind in UnitKind::ALL {
            let costs = terrain_costs(&rules, kind);
            let route: Vec<(u32, u32)> = (0..30).map(|k| (10 + k, 10 + k)).collect();
            let wp = smooth(&m, &costs, &route);
            assert!(wp.len() <= route.len());
            assert_eq!(wp.last(), route.last());
        }
    }

    #[test]
    fn unreachable_is_none() {
        let mut m = Map::new(64, 64, Terrain::Plain).unwrap();
        for y in 0..64 {
            m.set_terrain(30, y, Terrain::Mountain);
        }
        let rules = Rules::new(&Balance::default_balance());
        let costs = terrain_costs(&rules, UnitKind::Light);
        assert!(find_route(&m, &costs, (20, 10), (40, 10), &mut big_budget()).is_none());
    }
}
