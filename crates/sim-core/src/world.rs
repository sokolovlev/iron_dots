//! The world state and the fixed-order tick (spec 4.3).
//!
//!  1. apply commands (sorted by player, seq)
//!  2. rebuild the spatial grid
//!  3. pathing: flow fields requested last tick (`flow`, `pathing` crate)
//!  4. separation push of every unit (grid order, SIMD neighbour search)
//!  5. one pass per unit: intent, integration, terrain blocking, waypoints,
//!     water (pull: every unit writes only itself); then order transitions
//!  6. combat: contacts → damage (combat, water, starvation) → HP/morale update
//!  7. territory painting
//!  8. supply: pockets every 15 ticks, starvation rates, healing
//!  9. cities: capture; once per second money and victory check
//! 10. remove dead units, then spawn bought ones
//! 11. state hash (on demand; the runner records it every 30 ticks)

use crate::balance::{Balance, Rules, SHIP_ROW};
use crate::command::{Command, CommandKind, Formation};
use crate::flow::{self, Flow, Steer};
use crate::fx::{Fx, FxPoint, ONE, dist2, isqrt_u64, length};
use crate::grid::Grid;
use crate::map::{self, Map, Terrain};
use crate::order::{MAX_PATH_POINTS, OrderPool};
use crate::pathfind;
use crate::rng::{Purpose, rand, stochastic_round_q16};
use crate::territory::{self, NO_POCKET, SUPPLY_INTERVAL};
use crate::unit::{NO_ORDER, UnitHandle, UnitKind, UnitStore, flags};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use simd_kernels::{CHUNK, FAR, GROUP, Kernels, Level};
use std::collections::BTreeMap;
use std::mem::take;
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3;

/// Units per parallel work item. A fixed size (never derived from the
/// thread count) keeps every result independent of how many threads run.
pub const BLOCK: usize = 256;

/// Units are re-sorted into spatial (grid) order every this many ticks.
pub const SORT_INTERVAL: u32 = 8;

/// Max players per match (ids 1..=8; 0 is "nobody").
pub const MAX_PLAYERS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerSetup {
    /// Team id; players on the same team are allies.
    pub team: u8,
}

/// A city or capital (spec 2.4). Cities are points on the map; a city tile
/// always belongs to the city's owner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct City {
    pub x: u16,
    pub y: u16,
    /// Current owner (0 = neutral).
    pub owner: u8,
    /// Player whose capital this is (0 = ordinary city). Stays the same
    /// after capture, so the capital can be retaken.
    pub capital_of: u8,
    /// Player currently capturing it and for how many ticks.
    pub capture_by: u8,
    pub capture_ticks: u16,
    /// Ticks without any of the owner's units inside (city spawn).
    pub empty_ticks: u16,
}

impl City {
    pub fn center(&self) -> FxPoint {
        FxPoint::tile_center(self.x as i32, self.y as i32)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlayerState {
    /// Treasury in thousandths.
    pub money: i64,
    /// Income minus upkeep over the last second, thousandths.
    pub net_income: i64,
    /// The treasury ran dry at the last settlement: units outside cities starve.
    pub broke: bool,
    /// No cities and no units left.
    pub eliminated: bool,
}

/// Units of the 3×3 grid cells around one cell, copied into contiguous
/// arrays (positions as in the grid's copy).
#[derive(Default)]
struct Hood {
    cell: Option<(u32, u32)>,
    xs: Vec<i32>,
    ys: Vec<i32>,
    teams: Vec<u8>,
    ids: Vec<u32>,
}

impl Hood {
    fn load(&mut self, g: &Grid, cell: (u32, u32)) {
        if self.cell == Some(cell) {
            return;
        }
        self.cell = Some(cell);
        self.xs.clear();
        self.ys.clear();
        self.teams.clear();
        self.ids.clear();
        g.ranges_around_cell(cell.0, cell.1, |r| {
            self.xs.extend_from_slice(&g.sx[r.clone()]);
            self.ys.extend_from_slice(&g.sy[r.clone()]);
            self.teams.extend_from_slice(&g.steam[r.clone()]);
            self.ids.extend_from_slice(&g.cell_units[r]);
        });
        // Pad to whole kernel groups with points that never hit.
        while self.xs.len() % GROUP != 0 {
            self.xs.push(FAR);
            self.ys.push(FAR);
            self.teams.push(0);
            self.ids.push(u32::MAX);
        }
    }

    /// Calls `f(h)` for every hood entry `h` strictly within `sqrt(r2)` of
    /// `(x, y)` and, with `enemy_of = Some(team)`, of another team.
    #[inline]
    fn query(&self, k: &Kernels, x: i32, y: i32, r2: i64, enemy_of: Option<u8>, mut f: impl FnMut(usize)) {
        let mut start = 0;
        while start < self.xs.len() {
            let end = (start + CHUNK).min(self.xs.len());
            let (xs, ys) = (&self.xs[start..end], &self.ys[start..end]);
            let mut mask = match enemy_of {
                None => k.within(xs, ys, x, y, r2),
                Some(t) => k.enemies_within(xs, ys, &self.teams[start..end], t, x, y, r2),
            };
            while mask != 0 {
                f(start + mask.trailing_zeros() as usize);
                mask &= mask - 1;
            }
            start = end;
        }
    }
}

/// Corridor distance fields for every team (parallel).
fn this_corridors(w: &World, nteams: usize) -> Vec<territory::Corridors> {
    w.pool.install(|| {
        (0..nteams)
            .into_par_iter()
            .map(|t| {
                let bit = 1u8 << t;
                let cities: Vec<(u32, u32)> = w
                    .cities
                    .iter()
                    .filter(|c| c.owner != 0 && w.team_bit[c.owner as usize] == bit)
                    .map(|c| (c.x as u32, c.y as u32))
                    .collect();
                territory::Corridors::compute(
                    &w.map,
                    |o| o != 0 && w.team_bit[o as usize] != bit,
                    |x, y| !w.rules.passable[w.map.terrain_raw(x, y) as usize],
                    &cities,
                )
            })
            .collect()
    })
}

/// Result of the separation pass for one unit: its push away from
/// overlapping neighbours, and the directions away from the enemies it
/// touches (unit vectors, Q14), which limit how it may move in contact.
#[derive(Clone, Copy, Debug, Default)]
struct Sep {
    push: (i32, i32),
    away: [(i16, i16); MAX_CONTACT_NORMALS],
    n: u8,
}

/// Enemies in contact whose direction limits movement (the nearest ones in
/// grid order; four cover every practical case of being surrounded).
const MAX_CONTACT_NORMALS: usize = 4;

/// Tick phases as reported to the probe of [`World::step_with`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    Commands,
    Grid,
    Pathing,
    Separation,
    Move,
    Combat,
    Territory,
    Supply,
    Cities,
    Spawn,
}

impl Phase {
    pub const ALL: [Phase; 10] = [
        Phase::Commands,
        Phase::Grid,
        Phase::Pathing,
        Phase::Separation,
        Phase::Move,
        Phase::Combat,
        Phase::Territory,
        Phase::Supply,
        Phase::Cities,
        Phase::Spawn,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Phase::Commands => "commands",
            Phase::Grid => "grid",
            Phase::Pathing => "pathing",
            Phase::Separation => "separation",
            Phase::Move => "move",
            Phase::Combat => "combat",
            Phase::Territory => "territory",
            Phase::Supply => "supply",
            Phase::Cities => "cities",
            Phase::Spawn => "dead+spawn",
        }
    }
}

/// Counters of the last tick, for debug panels and tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TickStats {
    pub commands_applied: u32,
    pub commands_rejected: u32,
    pub units_in_combat: u32,
    /// Chunks with hostile units nearby (the rest sleep, spec 5.3).
    pub hot_chunks: u32,
    pub deaths: u32,
}

pub struct World {
    tick: u32,
    seed: u64,
    rules: Rules,
    map: Map,
    units: UnitStore,
    orders: OrderPool,
    grid: Grid,
    /// Team of each player id; index 0 unused.
    team: [u8; MAX_PLAYERS + 1],
    players: u8,
    stats: TickStats,
    cities: Vec<City>,
    /// Per tile (row-major): 1 + index of the city whose 3×3 area covers it, or 0.
    city_area: Vec<u16>,
    player: [PlayerState; MAX_PLAYERS + 1],
    /// Team that won, once decided.
    winner: Option<u8>,
    /// Connected land per team (index = bit of `team_bit`), see
    /// [`territory::TeamPockets`]; empty until first computed.
    pockets: Vec<territory::TeamPockets>,
    /// First pocket id of each player: pocket id = base + label in the
    /// player's team labelling. Owner/limit/used are per pocket id.
    pocket_base: [u32; MAX_PLAYERS + 1],
    pocket_owner: Vec<u8>,
    pocket_limit: Vec<u32>,
    pocket_used: Vec<u32>,
    territory_changed: bool,
    /// Chunks whose territory changed since the renderer last looked.
    dirty_chunks: Vec<bool>,
    pending_spawns: Vec<(u8, UnitKind, u16)>,
    /// Sandbox mode (spec 2.7): no upkeep, supply limits effectively unbounded.
    sandbox: bool,
    // Scratch buffers reused every tick (no allocations in steady state).
    arrive: Vec<bool>,
    contacts: Vec<u16>,
    dmg_in: Vec<u32>,
    cmd_order: Vec<u32>,
    /// Flow-field cache and pending field requests (phase 3).
    flow: Flow,
    /// Terrain costs per unit kind (straight-line checks).
    path_costs: [[u32; Terrain::COUNT]; UnitKind::COUNT],
    /// New owners of the chunks recomputed this tick: `(chunk, owners, any owned)`.
    zone_scratch: Vec<(usize, Vec<u8>, bool, Vec<u8>)>,
    /// Tiles a unit controls around its own (radius `territory.radius`).
    zone_disk: Vec<(i32, i32)>,
    /// Chunks that hold any owned tile (so they must be cleared when
    /// their units leave).
    chunk_owned: Vec<bool>,
    /// Cities whose zone or rear reaches each chunk.
    chunk_cities: Vec<Vec<u16>>,
    /// Player bits of city zones and rears per tile (chunk-major, like the
    /// owner plane); recomputed when a city changes hands.
    city_plane: Vec<u8>,
    /// Chunks with any city zone.
    chunk_city: Vec<bool>,
    city_plane_dirty: bool,
    /// Units each team may still create (index = bit of `team_bit`);
    /// `None` = no limit (war of attrition).
    team_budget: [Option<u32>; 8],
    /// Supply corridors per team (index = bit of `team_bit`).
    corridors: Vec<territory::Corridors>,
    /// Player bits of the corridor cells drawn as own land.
    corridor_marks: Vec<u8>,
    /// Chunks crossed by a corridor.
    chunk_corridor: Vec<bool>,
    /// Recompute every chunk's zones on the next territory phase.
    zones_stale: bool,
    /// Tile owner for every set of players present (bit p-1 = player p),
    /// see [`territory::resolve`].
    owner_of: [u8; 256],
    /// Bit of each player's team (teams numbered in ascending id order).
    team_bit: [u8; MAX_PLAYERS + 1],
    chunk_mask: Vec<u8>,
    hot: Vec<bool>,
    pool: Arc<rayon::ThreadPool>,
    threads: usize,
    /// SIMD kernel table (chosen once per process; tests may pick a level).
    kernels: &'static Kernels,
    // Per-unit results computed in grid order.
    push_sorted: Vec<Sep>,
    strike: Vec<u64>,
    contacts_sorted: Vec<u16>,
    dmg_sorted: Vec<u32>,
}

impl World {
    /// Creates an empty world. `players[k]` describes player id `k + 1`.
    pub fn new(map: Map, balance: &Balance, seed: u64, players: &[PlayerSetup]) -> Result<World, String> {
        balance.validate()?;
        if players.is_empty() || players.len() > MAX_PLAYERS {
            return Err(format!("need 1..={MAX_PLAYERS} players, got {}", players.len()));
        }
        let rules = Rules::new(balance);
        let zone_disk = territory::disk(rules.territory_radius);
        let path_costs =
            [pathfind::terrain_costs(&rules, UnitKind::Light), pathfind::terrain_costs(&rules, UnitKind::Heavy)];
        let mut team = [0u8; MAX_PLAYERS + 1];
        for (k, p) in players.iter().enumerate() {
            team[k + 1] = p.team;
        }
        let min_cell = rules.contact_radius.0 + 2 * max_move_per_tick(&rules).0;
        let grid = Grid::new(min_cell, map.width(), map.height());
        let mut player = [PlayerState::default(); MAX_PLAYERS + 1];
        for p in player.iter_mut().skip(1).take(players.len()) {
            p.money = rules.start_money;
        }
        let mut teams: Vec<u8> = players.iter().map(|p| p.team).collect();
        teams.sort_unstable();
        teams.dedup();
        let mut team_bit = [0u8; MAX_PLAYERS + 1];
        for (k, p) in players.iter().enumerate() {
            team_bit[k + 1] = 1 << teams.binary_search(&p.team).expect("team is listed");
        }
        let mut team_mask = [0u8; 256];
        for (b, m) in team_mask.iter_mut().enumerate() {
            for (p, &bit) in team_bit.iter().enumerate().skip(1).take(players.len()) {
                if b & (1 << (p - 1)) != 0 {
                    *m |= bit;
                }
            }
        }
        let tiles = (map.width() * map.height()) as usize;
        let chunks = (map.chunks_w() * map.chunks_h()) as usize;
        Ok(World {
            tick: 0,
            seed,
            rules,
            map,
            units: UnitStore::default(),
            orders: OrderPool::default(),
            grid,
            team,
            players: players.len() as u8,
            stats: TickStats::default(),
            cities: Vec::new(),
            city_area: vec![0; tiles],
            player,
            winner: None,
            pockets: Vec::new(),
            pocket_base: [0; MAX_PLAYERS + 1],
            pocket_owner: Vec::new(),
            pocket_limit: Vec::new(),
            pocket_used: Vec::new(),
            territory_changed: true,
            dirty_chunks: vec![true; chunks],
            pending_spawns: Vec::new(),
            sandbox: false,
            arrive: Vec::new(),
            contacts: Vec::new(),
            dmg_in: Vec::new(),
            cmd_order: Vec::new(),
            flow: Flow::default(),
            path_costs,
            zone_scratch: Vec::new(),
            zone_disk,
            chunk_owned: vec![false; chunks],
            chunk_cities: vec![Vec::new(); chunks],
            city_plane: vec![0; tiles],
            chunk_city: vec![false; chunks],
            city_plane_dirty: true,
            team_budget: [None; 8],
            corridors: Vec::new(),
            corridor_marks: Vec::new(),
            chunk_corridor: vec![false; chunks],
            zones_stale: true,
            owner_of: territory::owner_table(&team_mask),
            team_bit,
            chunk_mask: Vec::new(),
            hot: Vec::new(),
            pool: Arc::new(make_pool(1)?),
            threads: 1,
            kernels: simd_kernels::kernels(),
            push_sorted: Vec::new(),
            strike: Vec::new(),
            contacts_sorted: Vec::new(),
            dmg_sorted: Vec::new(),
        })
    }

    /// Runs the parallel phases on `n` worker threads (1 = single thread).
    /// The result of every tick is bit-identical for any `n`.
    pub fn set_threads(&mut self, n: usize) -> Result<(), String> {
        let n = n.max(1);
        if n != self.threads {
            self.pool = Arc::new(make_pool(n)?);
            self.threads = n;
        }
        Ok(())
    }

    pub fn threads(&self) -> usize {
        self.threads
    }

    /// Uses the SIMD kernels of `level` (results are identical at every
    /// level; this exists for tests and benchmarks).
    pub fn set_simd_level(&mut self, level: Level) -> Result<(), String> {
        self.kernels = Kernels::get(level).ok_or_else(|| format!("this CPU cannot run {}", level.name()))?;
        Ok(())
    }

    pub fn simd_level(&self) -> Level {
        self.kernels.level()
    }

    /// Computes `f(i, hood)` for every unit `i`, walking units in grid order
    /// in parallel blocks, and stores the results in grid order (`out[k]` is
    /// for unit `grid.cell_units[k]`). `hood` holds the units of the 3×3
    /// cells around `i`'s cell; it is gathered once per cell, so every unit
    /// of a cell is tested against one contiguous SIMD-friendly buffer.
    /// The hood is loaded only for units where `needs(i)` is true; `f`
    /// gets `None` otherwise (sleeping chunks, no contacts).
    fn par_by_grid<T: Send>(
        &self,
        out: &mut [T],
        needs: impl Fn(usize) -> bool + Sync,
        f: impl Fn(usize, Option<&Hood>) -> T + Sync,
    ) {
        let g = &self.grid;
        self.pool.install(|| {
            out.par_chunks_mut(BLOCK).enumerate().for_each(|(blk, out)| {
                let mut hood = Hood::default();
                for (j, o) in out.iter_mut().enumerate() {
                    let i = g.cell_units[blk * BLOCK + j] as usize;
                    *o = if needs(i) {
                        hood.load(g, g.cell_of_unit(i));
                        f(i, Some(&hood))
                    } else {
                        f(i, None)
                    };
                }
            });
        });
    }

    pub fn tick(&self) -> u32 {
        self.tick
    }
    pub fn seed(&self) -> u64 {
        self.seed
    }
    pub fn rules(&self) -> &Rules {
        &self.rules
    }
    pub fn map(&self) -> &Map {
        &self.map
    }
    pub fn units(&self) -> &UnitStore {
        &self.units
    }
    pub fn orders(&self) -> &OrderPool {
        &self.orders
    }
    pub fn stats(&self) -> TickStats {
        self.stats
    }
    pub fn player_count(&self) -> u8 {
        self.players
    }
    pub fn cities(&self) -> &[City] {
        &self.cities
    }
    /// State of player `id` (1..=8).
    pub fn player(&self, id: u8) -> PlayerState {
        self.player[id as usize]
    }
    pub fn team_of(&self, player: u8) -> u8 {
        self.team[player as usize]
    }
    /// Winning team, once the match is decided (spec 2.6).
    pub fn winner(&self) -> Option<u8> {
        self.winner
    }
    /// `(used, limit)` supply of the pocket that contains city `c`, if any.
    pub fn city_supply(&self, c: usize) -> Option<(u32, u32)> {
        let p = self.pocket_of_city(c)?;
        Some((self.pocket_used[p as usize], self.pocket_limit[p as usize]))
    }
    /// `(used, limit)` summed over all pockets of a player, for the HUD.
    pub fn supply_totals(&self, player: u8) -> (u32, u32) {
        let mut t = (0, 0);
        for (k, &o) in self.pocket_owner.iter().enumerate() {
            if o == player {
                t.0 += self.pocket_used[k];
                t.1 += self.pocket_limit[k];
            }
        }
        t
    }
    /// Chunks whose territory changed since the last call (for GPU upload).
    pub fn take_dirty_chunks(&mut self, mut f: impl FnMut(u32, u32)) {
        let cw = self.map.chunks_w() as usize;
        for (k, d) in self.dirty_chunks.iter_mut().enumerate() {
            if std::mem::take(d) {
                f((k % cw) as u32, (k / cw) as u32);
            }
        }
    }

    /// Sandbox mode (spec 2.7): supply limits scale up so huge armies don't
    /// starve, and there is no upkeep.
    pub fn set_sandbox(&mut self, on: bool) {
        self.sandbox = on;
    }

    /// Limited war: every team may create `n` more units in the whole
    /// match, bought or raised by empty cities. `None` is the usual war of
    /// attrition.
    pub fn set_unit_budget(&mut self, n: Option<u32>) {
        self.team_budget = [n; 8];
    }

    /// Limited war: no money (no income, upkeep or prices).
    #[inline]
    pub fn moneyless(&self) -> bool {
        self.team_budget.iter().any(|b| b.is_some())
    }

    /// Units `player`'s team may still create (`None` = unlimited).
    pub fn unit_budget(&self, player: u8) -> Option<u32> {
        self.team_budget[self.team_bit[player as usize].trailing_zeros() as usize]
    }

    /// Adds a city (match setup). Its tile becomes [`Terrain::City`] owned by
    /// `owner`; `capital` makes it the owner's capital.
    pub fn add_city(&mut self, x: u32, y: u32, owner: u8, capital: bool) -> Result<u16, String> {
        if x >= self.map.width() || y >= self.map.height() {
            return Err("city outside the map".into());
        }
        if owner > self.players || (capital && owner == 0) {
            return Err(format!("bad city owner {owner}"));
        }
        if self.cities.len() >= u16::MAX as usize - 1 {
            return Err("too many cities".into());
        }
        let idx = self.cities.len() as u16;
        self.map.set_terrain(x, y, Terrain::City);
        self.map.set_owner(x, y, owner);
        let w = self.map.width();
        for ny in y.saturating_sub(1)..=(y + 1).min(self.map.height() - 1) {
            for nx in x.saturating_sub(1)..=(x + 1).min(w - 1) {
                self.city_area[(ny * w + nx) as usize] = idx + 1;
            }
        }
        // Terrain changed: pathfinding graphs are rebuilt when next needed.
        self.flow.reset();
        self.cities.push(City {
            x: x as u16,
            y: y as u16,
            owner,
            capital_of: if capital { owner } else { 0 },
            capture_by: 0,
            capture_ticks: 0,
            empty_ticks: 0,
        });
        // Chunks this city's zone can reach.
        let r = self.rules.city_radius.max(self.rules.rear_radius) + 1;
        let cw = self.map.chunks_w();
        let (cx0, cx1) = (x.saturating_sub(r) / map::CHUNK, (x + r).min(w - 1) / map::CHUNK);
        let (cy0, cy1) = (y.saturating_sub(r) / map::CHUNK, (y + r).min(self.map.height() - 1) / map::CHUNK);
        for cy in cy0..=cy1 {
            for cx in cx0..=cx1 {
                self.chunk_cities[(cy * cw + cx) as usize].push(idx);
            }
        }
        self.territory_changed = true;
        self.zones_stale = true;
        self.city_plane_dirty = true;
        Ok(idx)
    }

    /// Recomputes every chunk's control zones now (match setup, so the
    /// first frame already shows them).
    pub fn refresh_zones(&mut self) {
        self.zones_stale = true;
        self.phase_territory();
        // Units are not in the spatial grid until the first tick builds it:
        // that tick recomputes everything again.
        self.zones_stale = true;
    }

    /// Index of the city whose 3×3 area contains the unit's tile, if owned by
    /// `owner` or an ally of `owner`.
    #[inline]
    fn friendly_city_at(&self, x: i32, y: i32, owner: u8) -> Option<usize> {
        let k = self.city_area[((y >> 16) as u32 * self.map.width() + (x >> 16) as u32) as usize];
        if k == 0 {
            return None;
        }
        let c = &self.cities[k as usize - 1];
        (c.owner != 0 && !self.enemies(c.owner, owner)).then_some(k as usize - 1)
    }

    fn pocket_of_city(&self, c: usize) -> Option<u32> {
        let city = &self.cities[c];
        if city.owner == 0 || self.pockets.is_empty() {
            return None;
        }
        self.pocket_at(city.owner, city.x as u32, city.y as u32)
    }

    /// Pocket id of `player` at tile `(x, y)`.
    #[inline]
    fn pocket_at(&self, player: u8, x: u32, y: u32) -> Option<u32> {
        let t = self.team_bit[player as usize].trailing_zeros() as usize;
        self.pockets.get(t)?.at(x, y).map(|l| self.pocket_base[player as usize] + l)
    }

    #[inline]
    fn enemies(&self, a: u8, b: u8) -> bool {
        self.team[a as usize] != self.team[b as usize]
    }

    /// Places a full-health unit. Used for match setup and (from stage 2) by
    /// city production at the end of a tick.
    /// Sets a unit's HP (tests and tools, never from commands).
    #[doc(hidden)]
    pub fn debug_set_hp(&mut self, h: UnitHandle, hp: u16) {
        if let Some(i) = self.units.index_of(h) {
            self.units.hp[i] = hp.min(self.rules.max_hp);
        }
    }

    pub fn spawn(&mut self, owner: u8, kind: UnitKind, at: FxPoint) -> Result<UnitHandle, String> {
        if owner == 0 || owner > self.players {
            return Err(format!("no player {owner}"));
        }
        let (w, h) = (self.map.width() as i32, self.map.height() as i32);
        if at.x.0 < 0 || at.y.0 < 0 || at.x.floor_int() >= w || at.y.floor_int() >= h {
            return Err("spawn point outside the map".into());
        }
        let t = self.map.terrain(at.x.floor_int() as u32, at.y.floor_int() as u32);
        if !self.rules.passable[t as usize] {
            return Err(format!("cannot spawn on {t:?}"));
        }
        let hdl = self.units.push(owner, kind, at.x.0, at.y.0, self.rules.max_hp, self.rules.max_morale);
        let i = self.units.len() - 1;
        self.units.row[i] = t as u8;
        Ok(hdl)
    }

    /// Advances the world by one tick. `commands` may contain commands for
    /// any tick; only those with `tick == self.tick()` are applied.
    pub fn step(&mut self, commands: &[Command]) {
        self.step_with(commands, &mut |_| {});
    }

    /// Like [`World::step`], calling `probe(phase)` right after each phase
    /// finishes. The simulation never reads a clock; profilers measure the
    /// time between probe calls instead (see `iron-dots-headless bench`).
    pub fn step_with(&mut self, commands: &[Command], probe: &mut dyn FnMut(Phase)) {
        macro_rules! phase {
            ($p:expr, $body:expr) => {{
                #[cfg(feature = "tracing")]
                let _span = tracing::info_span!("phase", name = $p.name()).entered();
                $body;
                probe($p);
            }};
        }
        self.stats = TickStats::default();
        self.prepare_scratch();
        phase!(Phase::Commands, self.apply_commands(commands));
        phase!(Phase::Grid, self.rebuild_grid());
        phase!(Phase::Pathing, self.phase_pathing());
        phase!(Phase::Separation, self.phase_separation());
        phase!(Phase::Move, self.phase_move());
        phase!(Phase::Combat, self.phase_combat());
        phase!(Phase::Territory, self.phase_territory());
        phase!(Phase::Supply, self.phase_supply());
        phase!(Phase::Cities, self.phase_cities());
        phase!(Phase::Spawn, {
            self.phase_remove_dead();
            self.phase_spawn()
        });
        self.tick += 1;
    }

    /// Rebuilds the spatial grid. Every [`SORT_INTERVAL`] ticks the units
    /// are also permuted into grid order (spec 5.1): neighbours on the map
    /// become neighbours in memory, so the grid-order passes read the unit
    /// arrays almost sequentially. Grid order is spatial (blocks of 32×32
    /// cells, rows inside a block) and ties keep index order: deterministic.
    fn rebuild_grid(&mut self) {
        let (grid, u, team, pool) = (&mut self.grid, &mut self.units, &self.team, &self.pool);
        pool.install(|| {
            grid.rebuild(&u.pos_x, &u.pos_y, &u.owner, team);
            if self.tick % SORT_INTERVAL == 0 && !u.is_empty() {
                let order = take(&mut grid.cell_units);
                u.permute(&order);
                grid.cell_units = order.clone();
                grid.units_permuted(&order);
            }
        });
    }

    /// Phase 3: flow fields requested last tick (see [`flow`]).
    fn phase_pathing(&mut self) {
        let (flow, map, rules, units, tick) = (&mut self.flow, &self.map, &self.rules, &mut self.units, self.tick);
        self.pool.install(|| flow.process(map, rules, units, tick));
    }

    /// Builds the pathfinding graphs now (end of match setup) so the first
    /// move orders do not pay for it during play. Results are unaffected.
    pub fn prepare_pathfinding(&mut self) {
        let (flow, map, rules, pool) = (&mut self.flow, &self.map, &self.rules, &self.pool);
        pool.install(|| flow.build_graphs(map, rules));
    }

    /// Live flow fields and coarse fields (debug panels, tests).
    pub fn flow_stats(&self) -> (usize, usize) {
        self.flow.stats()
    }

    fn prepare_scratch(&mut self) {
        let n = self.units.len();
        self.arrive.resize(n, false);
        self.contacts.resize(n, 0);
        self.dmg_in.resize(n, 0);
    }

    // ---------------------------------------------------------------- phase 1

    fn apply_commands(&mut self, commands: &[Command]) {
        let mut order = std::mem::take(&mut self.cmd_order);
        order.clear();
        order.extend(commands.iter().enumerate().filter(|(_, c)| c.tick == self.tick).map(|(k, _)| k as u32));
        // Stable sort: equal keys keep submission order.
        order.sort_by_key(|&k| {
            let c = &commands[k as usize];
            (c.player, c.seq)
        });
        for &k in &order {
            if self.apply_command(&commands[k as usize]) {
                self.stats.commands_applied += 1;
            } else {
                self.stats.commands_rejected += 1;
            }
        }
        self.cmd_order = order;
    }

    /// Valid, deduplicated indices of the player's own units, in handle order.
    fn own_units(&self, player: u8, handles: &[UnitHandle]) -> Vec<usize> {
        let mut hs: Vec<UnitHandle> = handles.to_vec();
        hs.sort();
        hs.dedup();
        hs.into_iter().filter_map(|h| self.units.index_of(h)).filter(|&i| self.units.owner[i] == player).collect()
    }

    fn apply_command(&mut self, c: &Command) -> bool {
        if c.player == 0 || c.player > self.players {
            return false;
        }
        match &c.kind {
            CommandKind::Move { units, path, queue, formation } => {
                let idx = self.own_units(c.player, units);
                if idx.is_empty() || path.is_empty() || path.len() > MAX_PATH_POINTS {
                    return false;
                }
                let max_x = Fx((self.map.width() as i32) * ONE - 1);
                let max_y = Fx((self.map.height() as i32) * ONE - 1);
                let path: Vec<FxPoint> =
                    path.iter().map(|p| FxPoint::new(p.x.clamp(Fx::ZERO, max_x), p.y.clamp(Fx::ZERO, max_y))).collect();
                // Routes come from flow fields as units walk (phase 3).
                let via = vec![false; path.len()];
                self.apply_move(&idx, path, via, *queue, *formation);
                true
            }
            CommandKind::Stop { units } => {
                let idx = self.own_units(c.player, units);
                for &i in &idx {
                    let o = std::mem::replace(&mut self.units.order[i], NO_ORDER);
                    self.orders.release(o);
                    self.units.waypoint[i] = 0;
                    self.units.stuck[i] = 0;
                }
                !idx.is_empty()
            }
            CommandKind::Buy { city, kind } => self.apply_buy(c.player, *city as usize, *kind),
            CommandKind::ClearQueue { units } => {
                let idx = self.own_units(c.player, units);
                // Units sharing (order, waypoint) share the trimmed copy.
                let mut copies: BTreeMap<(u32, u8), u32> = BTreeMap::new();
                for &i in &idx {
                    let (o, wp) = (self.units.order[i], self.units.waypoint[i]);
                    if o == NO_ORDER || self.orders.get(o).next == NO_ORDER {
                        continue;
                    }
                    let new = *copies
                        .entry((o, wp))
                        .or_insert_with(|| self.orders.copy_chain(o, wp as usize, false, NO_ORDER));
                    self.orders.retain(new);
                    self.orders.release(o);
                    self.units.order[i] = new;
                    self.units.waypoint[i] = 0;
                }
                !idx.is_empty()
            }
        }
    }

    fn apply_buy(&mut self, player: u8, city: usize, kind: UnitKind) -> bool {
        if city >= self.cities.len() || self.cities[city].owner != player {
            return false;
        }
        // No reinforcements into a city that is being taken.
        if self.cities[city].capture_by != 0 {
            return false;
        }
        if self.unit_budget(player) == Some(0) {
            return false;
        }
        let cost = if self.moneyless() { 0 } else { self.rules.cost[kind as usize] };
        if self.player[player as usize].money < cost {
            return false;
        }
        if self.sandbox {
            self.player[player as usize].money -= cost;
            self.pending_spawns.push((player, kind, city as u16));
            return true;
        }
        if self.pockets.is_empty() {
            self.recompute_supply();
        }
        let Some(p) = self.pocket_of_city(city) else { return false };
        let need = self.rules.supply[kind as usize];
        if self.pocket_used[p as usize] + need > self.pocket_limit[p as usize] {
            return false;
        }
        self.pocket_used[p as usize] += need;
        self.player[player as usize].money -= cost;
        self.pending_spawns.push((player, kind, city as u16));
        let t = self.team_bit[player as usize].trailing_zeros() as usize;
        if let Some(b) = &mut self.team_budget[t] {
            *b -= 1;
        }
        true
    }

    fn apply_move(&mut self, idx: &[usize], path: Vec<FxPoint>, via: Vec<bool>, queue: bool, formation: Formation) {
        let n = idx.len() as i64;
        let cx = idx.iter().map(|&i| self.units.pos_x[i] as i64).sum::<i64>() / n;
        let cy = idx.iter().map(|&i| self.units.pos_y[i] as i64).sum::<i64>() / n;
        let slots = (formation != Formation::Keep && idx.len() > 1)
            .then(|| crate::formation::offsets(&self.units, idx, (cx, cy), path[0], formation));
        // Keep the formation, but not wider than ~ (1 + sqrt(n)/2) tiles.
        let cap = ONE as i64 + (isqrt_u64(n as u64) as i64) * (ONE as i64);
        let new_order = self.orders.alloc_via(path, via, NO_ORDER);
        let mut queued_copies: BTreeMap<(u32, u8), u32> = BTreeMap::new();
        for &i in idx {
            let old = self.units.order[i];
            if queue && old != NO_ORDER {
                let wp = self.units.waypoint[i];
                // copy_chain retains `new_order` for the tail link.
                let head = *queued_copies
                    .entry((old, wp))
                    .or_insert_with(|| self.orders.copy_chain(old, wp as usize, true, new_order));
                self.orders.retain(head);
                self.orders.release(old);
                self.units.order[i] = head;
                self.units.waypoint[i] = 0;
            } else {
                let (ox, oy) = match &slots {
                    Some(s) => s[&i],
                    None => {
                        let mut ox = self.units.pos_x[i] as i64 - cx;
                        let mut oy = self.units.pos_y[i] as i64 - cy;
                        let len = isqrt_u64((ox * ox + oy * oy) as u64) as i64;
                        if len > cap {
                            ox = ox * cap / len;
                            oy = oy * cap / len;
                        }
                        (ox, oy)
                    }
                };
                self.units.off_x[i] = ox as i32;
                self.units.off_y[i] = oy as i32;
                self.orders.retain(new_order);
                self.orders.release(old);
                self.units.order[i] = new_order;
                self.units.waypoint[i] = 0;
            }
            self.units.stuck[i] = 0;
        }
        // If nobody references the new order directly or via a chain, free it.
        self.orders.retain(new_order);
        self.orders.release(new_order);
    }

    // ---------------------------------------------------------------- phase 4

    #[inline]
    fn waypoint_target(&self, i: usize) -> Option<(i32, i32)> {
        let o = self.units.order[i];
        if o == NO_ORDER {
            return None;
        }
        let order = self.orders.get(o);
        let wp = self.units.waypoint[i] as usize;
        let p = order.points[wp];
        if order.via[wp] {
            return Some((p.x.0, p.y.0));
        }
        let max_x = (self.map.width() as i32) * ONE - 1;
        let max_y = (self.map.height() as i32) * ONE - 1;
        let tx = (p.x.0 as i64 + self.units.off_x[i] as i64).clamp(0, max_x as i64) as i32;
        let ty = (p.y.0 as i64 + self.units.off_y[i] as i64).clamp(0, max_y as i64) as i32;
        Some((tx, ty))
    }

    // ---------------------------------------------------------------- phase 4-5

    /// Separation push for unit `i` (capped), pulled from neighbours'
    /// start-of-tick positions in `hood`, and the directions away from the
    /// enemies it touches (within the contact radius).
    #[inline]
    fn separation_of(&self, i: usize, hood: &Hood) -> Sep {
        let r = self.rules.separation_radius.0 as i64;
        let c = self.rules.contact_radius.0 as i64;
        let reach = r.max(c);
        let r2 = reach * reach;
        // Bodies are solid: a strong push, a quarter of the body diameter.
        let cap = (r / 4).max(1);
        let u = &self.units;
        let (x, y) = (u.pos_x[i], u.pos_y[i]);
        let my_team = self.team[u.owner[i] as usize];
        let (mut px, mut py) = (0i64, 0i64);
        let mut sep = Sep::default();
        hood.query(self.kernels, x, y, r2, None, |h| {
            let j = hood.ids[h] as usize;
            if j == i {
                return;
            }
            let (jx, jy) = (hood.xs[h], hood.ys[h]);
            let d2 = dist2(jx, jy, x, y);
            let (mut dx, mut dy) = ((x - jx) as i64, (y - jy) as i64);
            let dist = isqrt_u64(d2 as u64) as i64;
            if hood.teams[h] != my_team && dist > 0 && (sep.n as usize) < MAX_CONTACT_NORMALS && dist < c {
                sep.away[sep.n as usize] = ((dx * 16_384 / dist) as i16, (dy * 16_384 / dist) as i16);
                sep.n += 1;
            }
            if dist >= r {
                return;
            }
            let mut d = dist;
            if d == 0 {
                // Same spot: split along a direction derived from the
                // pair, opposite for the two units.
                let (a, b) = (u.slot[i].min(u.slot[j]), u.slot[i].max(u.slot[j]));
                let h = rand(self.seed, a, b, Purpose::Separation);
                dx = ((h & 0xFFFF) as i64) - 0x8000;
                dy = (((h >> 16) & 0xFFFF) as i64) - 0x8000;
                if dx == 0 && dy == 0 {
                    dx = 1;
                }
                if u.slot[i] > u.slot[j] {
                    dx = -dx;
                    dy = -dy;
                }
                d = isqrt_u64((dx * dx + dy * dy) as u64) as i64;
            }
            let overlap = r - dist;
            px += dx * overlap / (2 * d);
            py += dy * overlap / (2 * d);
        });
        let len = isqrt_u64((px * px + py * py) as u64) as i64;
        if len > cap {
            px = px * cap / len;
            py = py * cap / len;
        }
        sep.push = (px as i32, py as i32);
        sep
    }

    /// Separation (phase 5a): push of every unit, computed in grid order.
    fn phase_separation(&mut self) {
        let mut push = take(&mut self.push_sorted);
        push.resize(self.units.len(), Sep::default());
        self.par_by_grid(&mut push, |_| true, |i, hood| hood.map_or(Sep::default(), |h| self.separation_of(i, h)));
        self.push_sorted = push;
    }

    /// Phases 4 and 5 for every unit in one parallel pass: intent (desired
    /// velocity), walking plus the separation push, impassable tiles,
    /// waypoint progress and stuck detection, committing the position, water
    /// and ships. Then the order transitions, which touch the shared order
    /// pool, sequentially in unit order.
    ///
    /// Each unit reads only its own state (others' positions were read by
    /// the separation pass), so fusing these steps changes no result; it
    /// saves three fork/joins and three sweeps over memory per tick.
    fn phase_move(&mut self) {
        let n = self.units.len();
        let stuck_limit = (2 * self.rules.tick_rate).min(250) as u8;
        let max_x = (self.map.width() as i32) * ONE - 1;
        let max_y = (self.map.height() as i32) * ONE - 1;
        let u = &mut self.units;
        let (mut px, mut py, mut wt, mut fl, mut row, mut stuck) = (
            take(&mut u.pos_x),
            take(&mut u.pos_y),
            take(&mut u.water_ticks),
            take(&mut u.flags),
            take(&mut u.row),
            take(&mut u.stuck),
        );
        let mut adv = take(&mut self.arrive);
        adv.clear();
        adv.resize(n, false);
        let this = &*self;
        let rules = &this.rules;
        let requests = this.pool.install(|| {
            (
                px.par_chunks_mut(BLOCK),
                py.par_chunks_mut(BLOCK),
                wt.par_chunks_mut(BLOCK),
                fl.par_chunks_mut(BLOCK),
                row.par_chunks_mut(BLOCK),
                stuck.par_chunks_mut(BLOCK),
                adv.par_chunks_mut(BLOCK),
            )
                .into_par_iter()
                .enumerate()
                .map(|(k, (px, py, wt, fl, row, stuck, adv))| {
                    let mut reqs: Vec<flow::Request> = Vec::new();
                    for j in 0..px.len() {
                        let i = k * BLOCK + j;
                        let (x, y) = (px[j], py[j]);
                        let in_combat = fl[j] & flags::IN_COMBAT != 0;
                        let target = this.waypoint_target(i);
                        // Intent: speed from last tick's terrain and combat
                        // state; direction straight at the target, or from a
                        // flow field when far away or the line is blocked.
                        let (vx, vy, arrive, fielded) = match target {
                            None => (0, 0, false, false),
                            Some((tx, ty)) => {
                                let kind = this.units.kind[i] as usize;
                                let mut sp =
                                    rules.speed_per_tick[kind].mul_milli(rules.terrain_speed[row[j] as usize][kind]);
                                if in_combat {
                                    sp = sp.mul_milli(rules.engaged_speed);
                                }
                                let s = sp.0;
                                let (dx, dy) = (tx - x, ty - y);
                                let d = length(dx, dy);
                                if d <= s {
                                    (dx, dy, true, false)
                                } else {
                                    let straight = (
                                        (dx as i64 * s as i64 / d as i64) as i32,
                                        (dy as i64 * s as i64 / d as i64) as i32,
                                    );
                                    let (ut, gt) =
                                        (((x >> 16) as u32, (y >> 16) as u32), ((tx >> 16) as u32, (ty >> 16) as u32));
                                    let near = ut.0.abs_diff(gt.0).max(ut.1.abs_diff(gt.1)) <= flow::NEAR;
                                    if near && (this.tick + this.units.slot[i]) % flow::LOS_INTERVAL == 0 {
                                        let costs = &this.path_costs[kind];
                                        let at = |t: (u32, u32)| costs[this.map.terrain_raw(t.0, t.1) as usize];
                                        let limit = at(ut).max(at(gt)).max(1000);
                                        if pathfind::clear_line(&this.map, costs, ut, gt, limit) {
                                            fl[j] &= !flags::LOS_BLOCKED;
                                        } else {
                                            fl[j] |= flags::LOS_BLOCKED;
                                        }
                                    }
                                    if near && fl[j] & flags::LOS_BLOCKED == 0 {
                                        (straight.0, straight.1, false, false)
                                    } else {
                                        let h = this.units.handle(i);
                                        match this.flow.direction(h, kind, this.units.path[i], ut, gt) {
                                            Steer::Dir(k) => {
                                                let (ddx, ddy) = pathing::DIRS[k as usize];
                                                // Within 45° of the straight line the unit keeps
                                                // its own heading (formations stay intact);
                                                // otherwise it turns onto the field, around an
                                                // obstacle. cos 45°: dot² ≥ |a|²|b|² / 2.
                                                let dot = dx as i128 * ddx as i128 + dy as i128 * ddy as i128;
                                                let la = dx as i128 * dx as i128 + dy as i128 * dy as i128;
                                                let lb = (ddx * ddx + ddy * ddy) as i128;
                                                if dot > 0 && 2 * dot * dot >= la * lb {
                                                    (straight.0, straight.1, false, true)
                                                } else {
                                                    // Diagonals at 1/√2 (46341/65536) per axis.
                                                    let axis = if ddx != 0 && ddy != 0 {
                                                        ((s as i64 * 46_341) >> 16) as i32
                                                    } else {
                                                        s
                                                    };
                                                    (ddx * axis, ddy * axis, false, true)
                                                }
                                            }
                                            Steer::Straight => (straight.0, straight.1, false, false),
                                            Steer::Need(r) => {
                                                reqs.push(r);
                                                (straight.0, straight.1, false, false)
                                            }
                                        }
                                    }
                                }
                            }
                        };
                        // Walk, pushed by neighbours; impassable tiles: slide
                        // along an axis, otherwise stay.
                        let sep = &this.push_sorted[this.grid.rank[i] as usize];
                        let (ppx, ppy) = sep.push;
                        // In contact, a unit can only move away from the
                        // enemies it touches: speed × cos of the angle to
                        // the direction straight away from each (sin of the
                        // angle to the tangent); head-on or along the
                        // tangent it stays put. The tightest contact wins.
                        let (vx, vy) = if sep.n > 0 && (vx, vy) != (0, 0) {
                            let len = length(vx, vy).max(1) as i64;
                            let mut f = 16_384i64;
                            for &(ax, ay) in &sep.away[..sep.n as usize] {
                                let dot = (vx as i64 * ax as i64 + vy as i64 * ay as i64) / len;
                                f = f.min(dot.max(0));
                            }
                            (((vx as i64 * f) >> 14) as i32, ((vy as i64 * f) >> 14) as i32)
                        } else {
                            (vx, vy)
                        };
                        let nx = (x as i64 + vx as i64 + ppx as i64).clamp(0, max_x as i64) as i32;
                        let ny = (y as i64 + vy as i64 + ppy as i64).clamp(0, max_y as i64) as i32;
                        let pass = |x: i32, y: i32| {
                            rules.passable[this.map.terrain_raw((x >> 16) as u32, (y >> 16) as u32) as usize]
                        };
                        let (nx, ny) = if pass(nx, ny) {
                            (nx, ny)
                        } else if pass(nx, y) {
                            (nx, y)
                        } else if pass(x, ny) {
                            (x, ny)
                        } else {
                            (x, y)
                        };
                        // Waypoint progress and stuck detection.
                        if let Some((tx, ty)) = target {
                            let before = dist2(x, y, tx, ty);
                            let after = dist2(nx, ny, tx, ty);
                            let mut a = arrive;
                            if !a && !in_combat {
                                // Following a field may lead away from the target
                                // for a while: there, only standing still is stuck.
                                let progressing = if fielded { (nx, ny) != (x, y) } else { after < before };
                                if progressing {
                                    stuck[j] = 0;
                                } else {
                                    stuck[j] = stuck[j].saturating_add(1);
                                    a = stuck[j] >= stuck_limit;
                                }
                            }
                            if a {
                                stuck[j] = 0;
                            }
                            adv[j] = a;
                        }
                        // Commit; water and ships.
                        (px[j], py[j]) = (nx, ny);
                        let t = this.map.terrain_raw((nx >> 16) as u32, (ny >> 16) as u32);
                        if t == Terrain::Water as u8 {
                            wt[j] = wt[j].saturating_add(1);
                            if wt[j] >= rules.ship_after_ticks {
                                fl[j] |= flags::SHIP;
                            }
                        } else {
                            wt[j] = 0;
                            fl[j] &= !flags::SHIP;
                        }
                        row[j] = if fl[j] & flags::SHIP != 0 { SHIP_ROW as u8 } else { t };
                    }
                    reqs
                })
                .collect::<Vec<Vec<flow::Request>>>()
        });
        let u = &mut self.units;
        (u.pos_x, u.pos_y, u.water_ticks, u.flags, u.row, u.stuck) = (px, py, wt, fl, row, stuck);
        // Field requests, in block (= unit) order, for the next tick's phase 3.
        for r in requests.into_iter().flatten() {
            self.flow.requests.push(r);
        }
        // Order transitions touch the shared order pool: sequential, in
        // ascending unit order (same order of retains/releases as always).
        for (i, &a) in adv.iter().enumerate() {
            if !a {
                continue;
            }
            let o = self.units.order[i];
            let wp = self.units.waypoint[i] as usize + 1;
            if wp < self.orders.get(o).points.len() {
                self.units.waypoint[i] = wp as u8;
            } else {
                let next = self.orders.get(o).next;
                self.orders.retain(next);
                self.orders.release(o);
                self.units.order[i] = next;
                self.units.waypoint[i] = 0;
            }
        }
        self.arrive = adv;
    }

    // ---------------------------------------------------------------- sleep

    /// Marks "hot" chunks: a chunk is hot when units of two hostile teams are
    /// in it or its 8 neighbours. Elsewhere no unit can have an enemy within
    /// contact or territory radius (both far below a chunk), so combat and
    /// enemy searches are skipped there (spec 5.3, sleeping chunks). This
    /// only skips work that would find nothing: results are unchanged.
    fn update_hot_chunks(&mut self) {
        let (cw, ch) = (self.map.chunks_w() as usize, self.map.chunks_h() as usize);
        self.chunk_mask.clear();
        self.chunk_mask.resize(cw * ch, 0);
        let u = &self.units;
        for i in 0..u.len() {
            let c = ((u.pos_y[i] >> 22) as usize) * cw + (u.pos_x[i] >> 22) as usize;
            self.chunk_mask[c] |= self.team_bit[u.owner[i] as usize];
        }
        self.hot.clear();
        self.hot.resize(cw * ch, false);
        let mut hot = 0;
        for cy in 0..ch {
            for cx in 0..cw {
                let mut m = 0u8;
                for ny in cy.saturating_sub(1)..=(cy + 1).min(ch - 1) {
                    for nx in cx.saturating_sub(1)..=(cx + 1).min(cw - 1) {
                        m |= self.chunk_mask[ny * cw + nx];
                    }
                }
                let h = m.count_ones() >= 2;
                self.hot[cy * cw + cx] = h;
                hot += h as u32;
            }
        }
        self.stats.hot_chunks = hot;
    }

    #[inline]
    fn is_hot(&self, x: i32, y: i32) -> bool {
        self.hot[((y >> 22) as usize) * self.map.chunks_w() as usize + (x >> 22) as usize]
    }

    // ---------------------------------------------------------------- phase 6

    /// Attacker's part of the damage formula for unit `a`, Q16 HP hundredths
    /// per tick times attack in thousandths.
    #[inline]
    fn strike_of(&self, a: usize) -> u64 {
        let (u, rl) = (&self.units, &self.rules);
        let ka = u.kind[a] as usize;
        let f_hp = 32_768 + 32_768 * u.hp[a] as u64 / rl.max_hp as u64;
        let f_m = 32_768 + 32_768 * u.morale[a] as u64 / rl.max_morale as u64;
        let atk = (rl.atk[ka].0 * rl.terrain_atk[u.row[a] as usize][ka].0 / 1000) as u64;
        let d = (rl.base_dmg_q16 * f_hp) >> 16;
        ((d * f_m) >> 16) * atk
    }

    /// Damage unit `b` pulls from enemies in contact this tick, plus water,
    /// starvation and empty-treasury damage. `hood` is the neighbourhood of
    /// `b`, given when `b` has contacts.
    #[inline]
    fn damage_in(&self, b: usize, hood: Option<&Hood>) -> u32 {
        let r2 = self.rules.contact_r2;
        let rl = &self.rules;
        let u = &self.units;
        let mut q16: u64 = 0;
        if let Some(hood) = hood.filter(|_| self.contacts[b] > 0) {
            let (x, y, me) = (u.pos_x[b], u.pos_y[b], u.owner[b]);
            let kb = u.kind[b] as usize;
            let b_attacking = u.flags[b] & flags::ATTACKING != 0;
            let def_terrain = if b_attacking { 1000 } else { rl.terrain_def[u.row[b] as usize].0 };
            // The terrain strength multiplier ("atk" column) weakens a unit's
            // defense as well as its attack: a heavy in forest is "slow and
            // weak" (spec 2.3). With attack alone it would still beat a light.
            let strength = rl.terrain_atk[u.row[b] as usize][kb].0;
            let def = (rl.def[kb].0 * strength / 1000 * def_terrain / 1000).max(1) as u64;
            let taken = if b_attacking { rl.attacker_taken.0 as u64 } else { 1000 };
            // Spec 2.8 formula, folded to one division per pair: the
            // attacker's part (base × HP × morale × attack) is precomputed in
            // `strike`, the defender's (defense × terrain, attacker penalty)
            // and the split between the attacker's contacts are applied here.
            let denom_b = def * 1000;
            hood.query(self.kernels, x, y, r2 + 1, Some(self.team[me as usize]), |h| {
                let a = hood.ids[h] as usize;
                q16 += self.strike[a] * taken / (denom_b * self.contacts[a].max(1) as u64);
            });
        }
        if u.row[b] == Terrain::Water as u8 {
            q16 += rl.water_dmg_q16;
        }
        q16 += u.starve[b] as u64;
        if self.player[u.owner[b] as usize].broke && self.friendly_city_at(u.pos_x[b], u.pos_y[b], u.owner[b]).is_none()
        {
            q16 += rl.broke_starve_q16;
        }
        let dmg = stochastic_round_q16(q16, rand(self.seed, self.tick, u.slot[b], Purpose::Damage));
        dmg.min(u32::MAX as u64) as u32
    }

    fn phase_combat(&mut self) {
        self.update_hot_chunks();
        // With no hot chunk anywhere nobody can be in contact: the grid
        // passes below would find nothing, so they are skipped.
        let any_hot = self.stats.hot_chunks > 0;
        if any_hot {
            // The kernels scan the grid's position copies: bring them up to date.
            let (grid, units, pool) = (&mut self.grid, &self.units, &self.pool);
            pool.install(|| grid.refresh_positions(&units.pos_x, &units.pos_y));
        }
        let r2 = self.rules.contact_r2;
        // 6a. Contacts with enemies (positions after movement), counted in
        // grid order, then copied to units.
        let mut cs = take(&mut self.contacts_sorted);
        cs.clear();
        cs.resize(self.units.len(), 0);
        if any_hot {
            let hot = |i: usize| self.is_hot(self.units.pos_x[i], self.units.pos_y[i]);
            self.par_by_grid(&mut cs, hot, |i, hood| {
                let Some(hood) = hood else { return 0 };
                let (x, y, me) = (self.units.pos_x[i], self.units.pos_y[i], self.units.owner[i]);
                let mut c = 0u16;
                hood.query(self.kernels, x, y, r2 + 1, Some(self.team[me as usize]), |_| c = c.saturating_add(1));
                c
            });
        }
        self.contacts_sorted = cs;
        let (mut contacts, mut fl) = (take(&mut self.contacts), take(&mut self.units.flags));
        let this = &*self;
        let in_combat: u32 = this.pool.install(|| {
            (contacts.par_chunks_mut(BLOCK), fl.par_chunks_mut(BLOCK))
                .into_par_iter()
                .enumerate()
                .map(|(k, (contacts, fl))| {
                    let u = &this.units;
                    let mut count = 0u32;
                    for j in 0..contacts.len() {
                        let i = k * BLOCK + j;
                        let c = this.contacts_sorted[this.grid.rank[i] as usize];
                        contacts[j] = c;
                        let mut f = fl[j] & !(flags::IN_COMBAT | flags::ATTACKING);
                        if c > 0 {
                            count += 1;
                            f |= flags::IN_COMBAT;
                            if u.order[i] != NO_ORDER {
                                f |= flags::ATTACKING;
                            }
                        }
                        fl[j] = f;
                    }
                    count
                })
                .sum()
        });
        (self.contacts, self.units.flags) = (contacts, fl);
        self.stats.units_in_combat = in_combat;

        // 6b. Each unit pulls the damage dealt to it by enemies in contact
        // (computed in grid order, then copied to units). First every unit
        // in combat precomputes its strike.
        if !any_hot {
            // Only water, starvation and empty-treasury damage.
            let mut dmg = take(&mut self.dmg_in);
            let this = &*self;
            this.pool.install(|| {
                dmg.par_chunks_mut(BLOCK).enumerate().for_each(|(k, dmg)| {
                    for (j, d) in dmg.iter_mut().enumerate() {
                        *d = this.damage_in(k * BLOCK + j, None);
                    }
                });
            });
            self.dmg_in = dmg;
            self.apply_damage_and_morale();
            return;
        }
        let mut strike = take(&mut self.strike);
        strike.resize(self.units.len(), 0);
        let this = &*self;
        this.pool.install(|| {
            strike.par_chunks_mut(BLOCK).enumerate().for_each(|(k, st)| {
                for (j, s) in st.iter_mut().enumerate() {
                    let a = k * BLOCK + j;
                    *s = if this.contacts[a] > 0 { this.strike_of(a) } else { 0 };
                }
            });
        });
        self.strike = strike;
        let mut ds = take(&mut self.dmg_sorted);
        ds.resize(self.units.len(), 0);
        self.par_by_grid(&mut ds, |b| self.contacts[b] > 0, |b, hood| self.damage_in(b, hood));
        let mut dmg = take(&mut self.dmg_in);
        let this = &*self;
        this.pool.install(|| {
            dmg.par_chunks_mut(BLOCK).enumerate().for_each(|(k, dmg)| {
                for (j, d) in dmg.iter_mut().enumerate() {
                    *d = ds[this.grid.rank[k * BLOCK + j] as usize];
                }
            });
        });
        (self.dmg_in, self.dmg_sorted) = (dmg, ds);
        self.apply_damage_and_morale();
    }

    /// 6c. Apply damage and morale; broken attacks drop their orders. The
    /// releases touch the shared order pool, so they are collected per
    /// block and applied afterwards in ascending unit order.
    fn apply_damage_and_morale(&mut self) {
        let u = &mut self.units;
        let (mut hp, mut mo, mut ca, mut fl, mut ord, mut wp) = (
            take(&mut u.hp),
            take(&mut u.morale),
            take(&mut u.calm),
            take(&mut u.flags),
            take(&mut u.order),
            take(&mut u.waypoint),
        );
        let this = &*self;
        let released: Vec<Vec<u32>> = this.pool.install(|| {
            (
                hp.par_chunks_mut(BLOCK),
                mo.par_chunks_mut(BLOCK),
                ca.par_chunks_mut(BLOCK),
                fl.par_chunks_mut(BLOCK),
                ord.par_chunks_mut(BLOCK),
                wp.par_chunks_mut(BLOCK),
            )
                .into_par_iter()
                .enumerate()
                .map(|(k, (hp, mo, ca, fl, ord, wp))| {
                    let rules = &this.rules;
                    let mut released = Vec::new();
                    for j in 0..hp.len() {
                        let i = k * BLOCK + j;
                        hp[j] = hp[j].saturating_sub(this.dmg_in[i].min(u16::MAX as u32) as u16);
                        let f = fl[j];
                        let r = rand(this.seed, this.tick, this.units.slot[i], Purpose::Morale);
                        ca[j] = if f & flags::IN_COMBAT != 0 { 0 } else { ca[j].saturating_add(1) };
                        if f & flags::IN_COMBAT != 0 {
                            let loss_q16 = if f & flags::ATTACKING != 0 {
                                rules.morale_loss_attack_q16
                            } else {
                                rules.morale_loss_defend_q16
                            };
                            let loss = stochastic_round_q16(loss_q16, r).min(u16::MAX as u64) as u16;
                            mo[j] = mo[j].saturating_sub(loss);
                            if mo[j] == 0 && f & flags::ATTACKING != 0 {
                                // Broken attack: the order is cancelled, the unit holds.
                                released.push(std::mem::replace(&mut ord[j], NO_ORDER));
                                wp[j] = 0;
                                fl[j] &= !flags::ATTACKING;
                            }
                        } else {
                            let gain = stochastic_round_q16(rules.morale_regen_q16, r).min(u16::MAX as u64) as u16;
                            mo[j] = mo[j].saturating_add(gain).min(rules.max_morale);
                        }
                    }
                    released
                })
                .collect()
        });
        let u = &mut self.units;
        (u.hp, u.morale, u.calm, u.flags, u.order, u.waypoint) = (hp, mo, ca, fl, ord, wp);
        for o in released.into_iter().flatten() {
            self.orders.release(o);
        }
    }

    // ---------------------------------------------------------------- phase 7

    /// City zones and rears for every chunk (parallel); only when a city
    /// is added or changes hands.
    fn recompute_city_plane(&mut self) {
        let cw = self.map.chunks_w();
        let mut plane = take(&mut self.city_plane);
        let mut flags = take(&mut self.chunk_city);
        let this = &*self;
        this.pool.install(|| {
            plane.par_chunks_mut(map::CHUNK_TILES).zip(flags.par_iter_mut()).enumerate().for_each(
                |(c, (out, flag))| {
                    let cities: Vec<(u32, u32, u8, u8)> = this.chunk_cities[c]
                        .iter()
                        .map(|&k| &this.cities[k as usize])
                        .filter(|city| city.owner != 0)
                        .map(|city| (city.x as u32, city.y as u32, city.owner, this.team_bit[city.owner as usize]))
                        .collect();
                    *flag = !cities.is_empty()
                        && territory::city_zones(
                            out,
                            c as u32 % cw,
                            c as u32 / cw,
                            &cities,
                            this.rules.city_radius,
                            this.rules.rear_radius,
                        );
                    if cities.is_empty() {
                        out.fill(0);
                    }
                },
            );
        });
        self.city_plane = plane;
        self.chunk_city = flags;
    }

    /// Control zones (spec 2.2). Each chunk is recomputed every 4 ticks, a
    /// quarter of the chunks per tick ([`territory::chunk_paints`]), from
    /// the units and cities that reach it: in parallel per chunk into
    /// scratch planes, then copied into the map. A pure function of
    /// positions, so the thread count cannot change the result.
    fn phase_territory(&mut self) {
        let cw = self.map.chunks_w();
        let n = (cw * self.map.chunks_h()) as usize;
        let tick = self.tick;
        if std::mem::replace(&mut self.city_plane_dirty, false) {
            self.recompute_city_plane();
            self.zones_stale = true;
        }
        let full = std::mem::replace(&mut self.zones_stale, false);
        let mut scratch = take(&mut self.zone_scratch);
        let selected: Vec<usize> =
            (0..n).filter(|&c| full || territory::chunk_paints(c as u32 % cw, c as u32 / cw, tick)).collect();
        if scratch.len() < selected.len() {
            scratch.resize_with(selected.len(), || (0, vec![0; map::CHUNK_TILES], false, Vec::new()));
        }
        let this = &*self;
        this.pool.install(|| {
            scratch[..selected.len()].par_iter_mut().zip(selected.par_iter()).for_each(|(out, &c)| {
                let (cx, cy) = (c as u32 % cw, c as u32 / cw);
                let g = &this.grid;
                // Units whose zone can reach the chunk.
                let half = ((map::CHUNK / 2 + this.rules.territory_radius + 1) as i32) << 16;
                let (mx, my) = (
                    ((cx * map::CHUNK + map::CHUNK / 2) as i32) << 16,
                    ((cy * map::CHUNK + map::CHUNK / 2) as i32) << 16,
                );
                // Nothing reaches an empty chunk: leave it as it is.
                let mut any_unit = false;
                g.ranges_in_range(mx, my, half, |rg| any_unit |= !rg.is_empty());
                let mut city_tiles: Vec<(u32, u32, u8)> = Vec::new();
                for &k in &this.chunk_cities[c] {
                    let city = &this.cities[k as usize];
                    let (x, y) = (city.x as u32, city.y as u32);
                    if x / map::CHUNK == cx && y / map::CHUNK == cy {
                        city_tiles.push((x, y, city.owner));
                    }
                }
                if !any_unit && !this.chunk_city[c] && !this.chunk_owned[c] && !this.chunk_corridor[c] {
                    out.0 = usize::MAX;
                    return;
                }
                out.0 = c;
                out.2 = territory::chunk_owners(
                    &mut out.1,
                    &mut out.3,
                    cx,
                    cy,
                    this.rules.territory_radius,
                    &this.zone_disk,
                    &this.owner_of,
                    |seed| {
                        g.ranges_in_range(mx, my, half, |rg| {
                            for k in rg {
                                seed(g.sx[k], g.sy[k], this.units.owner[g.cell_units[k] as usize]);
                            }
                        });
                    },
                    &this.city_plane[c * map::CHUNK_TILES..(c + 1) * map::CHUNK_TILES],
                    this.corridors
                        .first()
                        .filter(|_| !this.corridor_marks.is_empty())
                        .map(|k| (&this.corridor_marks[..], k.w)),
                    &city_tiles,
                );
            });
        });
        let plane = self.map.owner_plane_mut();
        let mut changed = false;
        for (c, owners, any, _) in &scratch[..selected.len()] {
            if *c == usize::MAX {
                continue;
            }
            let dst = &mut plane[c * map::CHUNK_TILES..(c + 1) * map::CHUNK_TILES];
            if dst != &owners[..] {
                dst.copy_from_slice(owners);
                self.dirty_chunks[*c] = true;
                changed = true;
            }
            self.chunk_owned[*c] = *any;
        }
        self.zone_scratch = scratch;
        self.territory_changed |= changed;
    }

    // ---------------------------------------------------------------- phase 8

    /// Draws the supply corridors: from every unit, the shortest way to an
    /// own city over land no enemy holds (ways merge into a tree).
    fn trace_corridors(&mut self) {
        let Some(first) = self.corridors.first() else { return };
        let cells = (first.w * first.h) as usize;
        let mut marks = take(&mut self.corridor_marks);
        marks.clear();
        marks.resize(cells, 0);
        let u = &self.units;
        for i in 0..u.len() {
            let o = u.owner[i];
            let t = self.team_bit[o as usize].trailing_zeros() as usize;
            if let Some(c) = self.corridors.get(t) {
                c.trace(&mut marks, (u.pos_x[i] >> 16) as u32, (u.pos_y[i] >> 16) as u32, territory::player_bit(o));
            }
        }
        let (cw, w) = (self.map.chunks_w() as usize, first.w as usize);
        self.chunk_corridor.fill(false);
        let per = (map::CHUNK / territory::CELL) as usize;
        for (k, &m) in marks.iter().enumerate() {
            if m != 0 {
                self.chunk_corridor[(k / w / per) * cw + (k % w) / per] = true;
            }
        }
        self.corridor_marks = marks;
    }

    /// Pockets, supply limits and starvation rates (every 15 ticks).
    fn recompute_supply(&mut self) {
        if self.territory_changed || self.pockets.is_empty() {
            // One labelling per team: its land is everything not held by
            // another team (enemy zones are the only walls).
            let teams = (1..=self.players as usize).map(|p| self.team_bit[p]).fold(0u8, |a, b| a | b);
            let nteams = 8 - teams.leading_zeros() as usize;
            let this = &*self;
            let pockets: Vec<territory::TeamPockets> = this.pool.install(|| {
                (0..nteams)
                    .into_par_iter()
                    .map(|t| {
                        let bit = 1u8 << t;
                        territory::TeamPockets::label(&this.map, |o| o == 0 || this.team_bit[o as usize] == bit)
                    })
                    .collect()
            });
            self.pockets = pockets;
            self.territory_changed = false;
            // Supply corridors: distances to own cities per team…
            let corridors: Vec<territory::Corridors> = this_corridors(self, nteams);
            self.corridors = corridors;
        }
        // …and the ways from every unit to them, drawn as own land.
        self.trace_corridors();
        let mut total = 0u32;
        self.pocket_owner.clear();
        for p in 1..=self.players as usize {
            self.pocket_base[p] = total;
            let t = self.team_bit[p].trailing_zeros() as usize;
            let count = self.pockets.get(t).map_or(0, |tp| tp.count);
            total += count;
            self.pocket_owner.extend(std::iter::repeat_n(p as u8, count as usize));
        }
        let np = total as usize;
        self.pocket_limit.clear();
        self.pocket_limit.resize(np, 0);
        self.pocket_used.clear();
        self.pocket_used.resize(np, 0);
        for c in 0..self.cities.len() {
            if let Some(l) = self.pocket_of_city(c) {
                let per_city = if self.sandbox { 1 << 20 } else { self.rules.per_city };
                self.pocket_limit[l as usize] += per_city;
            }
        }
        // Pocket of every unit (parallel), then supply used per pocket
        // (integer sums: order does not matter).
        let mut pocket = take(&mut self.units.pocket);
        let this = &*self;
        this.pool.install(|| {
            pocket.par_chunks_mut(BLOCK).enumerate().for_each(|(k, pocket)| {
                let u = &this.units;
                for (j, p) in pocket.iter_mut().enumerate() {
                    let i = k * BLOCK + j;
                    let o = u.owner[i];
                    let t = this.team_bit[o as usize].trailing_zeros() as usize;
                    *p = this.pockets[t]
                        .near(&this.map, u.pos_x[i], u.pos_y[i], this.rules.territory_radius)
                        .map_or(NO_POCKET, |l| this.pocket_base[o as usize] + l);
                }
            });
        });
        self.units.pocket = pocket;
        let u = &mut self.units;
        for i in 0..u.len() {
            let p = u.pocket[i];
            if p != NO_POCKET {
                self.pocket_used[p as usize] += self.rules.supply[u.kind[i] as usize];
            }
        }
        // Units bought this tick are not spawned yet but already take supply.
        for k in 0..self.pending_spawns.len() {
            let (owner, kind, city) = self.pending_spawns[k];
            let c = &self.cities[city as usize];
            if let Some(l) = self.pocket_at(owner, c.x as u32, c.y as u32) {
                self.pocket_used[l as usize] += self.rules.supply[kind as usize];
            }
        }
        let u = &mut self.units;
        let max = self.rules.starve_q16;
        for i in 0..u.len() {
            let p = u.pocket[i];
            u.starve[i] = if self.sandbox {
                0
            } else if p == NO_POCKET || self.pocket_limit[p as usize] == 0 {
                max as u32
            } else {
                let (used, limit) = (self.pocket_used[p as usize] as u64, self.pocket_limit[p as usize] as u64);
                if used > limit { (max * (used - limit) / limit).min(max) as u32 } else { 0 }
            };
        }
    }

    fn phase_supply(&mut self) {
        // Sandbox: no supply limits or starvation, so no pockets to label
        // (labelling a 4096² map is the most expensive step at that size).
        if self.tick % SUPPLY_INTERVAL == 0 && !self.sandbox {
            self.recompute_supply();
        }
        // Healing: out of combat long enough, not starving, on friendly land.
        let mut hp = take(&mut self.units.hp);
        let this = &*self;
        this.pool.install(|| {
            hp.par_chunks_mut(BLOCK).enumerate().for_each(|(k, hp)| {
                for (j, h) in hp.iter_mut().enumerate() {
                    *h = this.healed_hp(k * BLOCK + j, *h);
                }
            });
        });
        self.units.hp = hp;
    }

    /// HP of unit `i` after this tick's healing (`hp` is its current HP).
    #[inline]
    fn healed_hp(&self, i: usize, hp: u16) -> u16 {
        let u = &self.units;
        if u.calm[i] < self.rules.heal_delay_ticks
            || u.starve[i] != 0
            || hp >= self.rules.max_hp
            || u.row[i] == Terrain::Water as u8
        {
            return hp;
        }
        let (x, y, me) = (u.pos_x[i], u.pos_y[i], u.owner[i]);
        let tile_owner = self.map.owner((x >> 16) as u32, (y >> 16) as u32);
        if tile_owner == 0 || self.enemies(tile_owner, me) {
            return hp;
        }
        let in_city = self.friendly_city_at(x, y, me).is_some();
        if self.player[me as usize].broke && !in_city {
            return hp;
        }
        // Supply quality: in a city, how many own cities it reaches (a
        // blockaded city heals less); outside, the corridor length.
        let share = if self.sandbox {
            1000
        } else if let Some(c) = self.friendly_city_at(x, y, me) {
            let n = self
                .pocket_of_city(c)
                .map_or(1, |p| (self.pocket_limit[p as usize] / self.rules.per_city.max(1)).max(1));
            (n as i64 * 1000 / self.rules.blockade_cities.max(1) as i64).min(1000)
        } else {
            let t = self.team_bit[me as usize].trailing_zeros() as usize;
            let d = self.corridors.get(t).map_or(territory::NO_WAY, |c| c.at((x >> 16) as u32, (y >> 16) as u32));
            territory::corridor_share(
                d,
                self.rules.corridor_full_cells,
                self.rules.corridor_max_cells,
                self.rules.corridor_min.0,
            )
        };
        // Limited war: the more supply places per unit in the pocket, the
        // faster wounds heal (×0.25 to ×2): many cities and few units heal
        // fast, one city and a crowd slowly.
        let share = if self.moneyless() && !self.sandbox {
            let p = u.pocket[i];
            let ratio = if p == NO_POCKET {
                250
            } else {
                let (used, limit) = (self.pocket_used[p as usize].max(1) as i64, self.pocket_limit[p as usize] as i64);
                (limit * 1000 / used).clamp(250, 2000)
            };
            share * ratio / 1000
        } else {
            share
        };
        let q16 = if in_city { self.rules.heal_city_q16 } else { self.rules.heal_q16 };
        let q16 = q16 * share as u64 / 1000;
        let heal = stochastic_round_q16(q16, rand(self.seed, self.tick, u.slot[i], Purpose::Environment));
        (hp as u64 + heal).min(self.rules.max_hp as u64) as u16
    }

    // ---------------------------------------------------------------- phase 9

    fn phase_cities(&mut self) {
        self.capture_cities();
        if self.tick % self.rules.tick_rate == self.rules.tick_rate - 1 {
            self.settle_money();
            self.check_victory();
        }
    }

    fn capture_cities(&mut self) {
        let mut raise: Vec<usize> = Vec::new();
        for c in 0..self.cities.len() {
            let city = &self.cities[c];
            let (cx, cy, owner) = (city.x as i32, city.y as i32, city.owner);
            let center = city.center();
            let u = &self.units;
            let mut defended = false;
            // Lowest attacking player id; attackers of two teams = contested.
            let mut attacker: Option<u8> = None;
            let mut contested = false;
            self.grid.for_each_in_range(center.x.0, center.y.0, 2 * ONE, |j| {
                let (tx, ty) = (u.pos_x[j] >> 16, u.pos_y[j] >> 16);
                if (tx - cx).abs() > 1 || (ty - cy).abs() > 1 {
                    return;
                }
                let o = u.owner[j];
                if owner != 0 && !self.enemies(o, owner) {
                    defended = true;
                    return;
                }
                attacker = match attacker {
                    None => Some(o),
                    Some(a) if !self.enemies(a, o) => Some(a.min(o)),
                    Some(a) => {
                        contested = true;
                        Some(a)
                    }
                };
            });
            let capture_ticks = self.rules.capture_ticks;
            let same_team = |a: u8, b: u8| a != 0 && b != 0 && self.team[a as usize] == self.team[b as usize];
            let city = &mut self.cities[c];
            match attacker {
                Some(a) if !defended && !contested => {
                    if !same_team(city.capture_by, a) {
                        city.capture_by = a;
                        city.capture_ticks = 0;
                    }
                    city.capture_ticks += 1;
                    if city.capture_ticks >= capture_ticks {
                        city.owner = city.capture_by;
                        city.capture_by = 0;
                        city.capture_ticks = 0;
                        let (x, y, o) = (city.x as u32, city.y as u32, city.owner);
                        self.map.set_owner(x, y, o);
                        let cw = self.map.chunks_w() as usize;
                        self.dirty_chunks[(y as usize >> 6) * cw + (x as usize >> 6)] = true;
                        self.territory_changed = true;
                        self.city_plane_dirty = true;
                    }
                }
                Some(_) if contested && !defended => {} // frozen while contested
                _ => {
                    city.capture_by = 0;
                    city.capture_ticks = 0;
                }
            }
            // An empty city (no owner's unit, nobody capturing or about to)
            // raises a unit.
            if city.owner != 0 && !defended && attacker.is_none() && city.capture_by == 0 {
                city.empty_ticks = city.empty_ticks.saturating_add(1);
                if city.empty_ticks >= self.rules.spawn_ticks {
                    raise.push(c);
                }
            } else {
                city.empty_ticks = 0;
            }
        }
        if self.sandbox {
            return;
        }
        for c in raise {
            // Paid like a purchase, and only with supply to spare in the
            // city's pocket; otherwise it tries again next tick. The unit
            // kind is drawn when the city becomes due, so waiting for money
            // does not reroll it.
            let Some(p) = self.pocket_of_city(c) else { continue };
            let due = self.tick - (self.cities[c].empty_ticks - self.rules.spawn_ticks) as u32;
            let r = rand(self.seed, due, c as u32, Purpose::CitySpawn);
            let heavy = (r % 1000) < self.rules.spawn_heavy.0 as u64;
            let kind = if heavy { UnitKind::Heavy } else { UnitKind::Light };
            let need = self.rules.supply[kind as usize];
            let owner = self.cities[c].owner;
            let cost = if self.moneyless() { 0 } else { self.rules.cost[kind as usize] };
            // In a limited war it also uses up the team's unit budget.
            let t = self.team_bit[owner as usize].trailing_zeros() as usize;
            if self.pocket_used[p as usize] + need > self.pocket_limit[p as usize]
                || self.player[owner as usize].money < cost
                || self.team_budget[t] == Some(0)
            {
                continue;
            }
            if let Some(b) = &mut self.team_budget[t] {
                *b -= 1;
            }
            self.pocket_used[p as usize] += need;
            self.player[owner as usize].money -= cost;
            self.pending_spawns.push((owner, kind, c as u16));
            self.cities[c].empty_ticks = 0;
        }
    }

    /// Once per second: income from cities, upkeep of units outside cities.
    fn settle_money(&mut self) {
        // A limited war has no money: units are limited by the budget.
        if self.moneyless() {
            return;
        }
        let mut delta = [0i64; MAX_PLAYERS + 1];
        for c in &self.cities {
            delta[c.owner as usize] += self.rules.city_income;
        }
        let u = &self.units;
        let w = self.map.width();
        for i in 0..u.len() {
            let o = u.owner[i];
            let k = self.city_area[((u.pos_y[i] >> 16) as u32 * w + (u.pos_x[i] >> 16) as u32) as usize];
            let in_own_city = k != 0 && self.cities[k as usize - 1].owner == o;
            if !in_own_city && !self.sandbox {
                delta[o as usize] -= self.rules.upkeep[u.kind[i] as usize];
            }
        }
        for (p, d) in delta.iter().enumerate().skip(1).take(self.players as usize) {
            let st = &mut self.player[p];
            st.net_income = *d;
            st.money += d;
            st.broke = st.money < 0;
            if st.money < 0 {
                st.money = 0;
            }
        }
    }

    /// Spec 2.6: a team wins holding every enemy capital and at least
    /// `win_city_share` of all cities, or by being the last team standing.
    fn check_victory(&mut self) {
        let counts = self.unit_counts();
        let mut cities_of = [0u32; MAX_PLAYERS + 1];
        for c in &self.cities {
            cities_of[c.owner as usize] += 1;
        }
        for p in 1..=self.players as usize {
            // Out of units with none left to create (limited war) is out too.
            let spent = self.team_budget[self.team_bit[p].trailing_zeros() as usize] == Some(0);
            if !self.player[p].eliminated && counts[p] == 0 && (cities_of[p] == 0 || spent) {
                self.player[p].eliminated = true;
            }
        }
        if self.winner.is_some() {
            return;
        }
        let mut alive_teams: Vec<u8> =
            (1..=self.players as usize).filter(|&p| !self.player[p].eliminated).map(|p| self.team[p]).collect();
        alive_teams.sort_unstable();
        alive_teams.dedup();
        let all_teams = {
            let mut t: Vec<u8> = (1..=self.players as usize).map(|p| self.team[p]).collect();
            t.sort_unstable();
            t.dedup();
            t.len()
        };
        if alive_teams.len() == 1 && all_teams > 1 {
            self.winner = Some(alive_teams[0]);
            return;
        }
        let total = self.cities.len() as i64;
        for &t in &alive_teams {
            let ours = |p: u8| p != 0 && self.team[p as usize] == t;
            let held = self.cities.iter().filter(|c| ours(c.owner)).count() as i64;
            let capitals = self.cities.iter().all(|c| c.capital_of == 0 || ours(c.capital_of) || ours(c.owner));
            if all_teams > 1 && capitals && total > 0 && held * 1000 >= self.rules.win_city_share.0 * total {
                self.winner = Some(t);
                return;
            }
        }
    }

    // --------------------------------------------------------------- phase 10

    fn phase_remove_dead(&mut self) {
        // Descending order: swap_remove only moves already-checked units.
        for i in (0..self.units.len()).rev() {
            if self.units.hp[i] == 0 {
                let o = std::mem::replace(&mut self.units.order[i], NO_ORDER);
                self.orders.release(o);
                self.units.swap_remove(i);
                self.stats.deaths += 1;
            }
        }
    }

    fn phase_spawn(&mut self) {
        let mut spawns = std::mem::take(&mut self.pending_spawns);
        for (k, &(owner, kind, city)) in spawns.iter().enumerate() {
            let c = self.cities[city as usize].center();
            // Small deterministic jitter so new units don't stack exactly.
            let r = rand(self.seed, self.tick, ((city as u32) << 8) | (k as u32 & 0xFF), Purpose::Scenario);
            let jx = ((r & 0xFFFF) as i32 - 0x8000) / 4;
            let jy = (((r >> 16) & 0xFFFF) as i32 - 0x8000) / 4;
            let at = FxPoint::new(c.x + Fx(jx), c.y + Fx(jy));
            if self.spawn(owner, kind, at).is_err() {
                // The city tile itself is always passable.
                let _ = self.spawn(owner, kind, c);
            }
        }
        spawns.clear();
        self.pending_spawns = spawns;
    }

    // --------------------------------------------------------------- phase 11

    /// xxh3 of the whole simulation state. Equal hashes on two machines mean
    /// (with overwhelming probability) bit-identical worlds.
    pub fn state_hash(&self) -> u64 {
        let mut h = Xxh3::new();
        h.update(&self.tick.to_le_bytes());
        h.update(&self.seed.to_le_bytes());
        h.update(&self.rules.balance_hash.to_le_bytes());
        for b in self.team_budget {
            h.update(&b.map_or(u32::MAX, |n| n).to_le_bytes());
        }
        h.update(&(self.units.len() as u64).to_le_bytes());
        let u = &self.units;
        hash_i32s(&mut h, &u.pos_x);
        hash_i32s(&mut h, &u.pos_y);
        hash_u16s(&mut h, &u.hp);
        hash_u16s(&mut h, &u.morale);
        h.update(&u.owner);
        h.update(&u.kind);
        h.update(&u.flags);
        h.update(&u.row);
        hash_u16s(&mut h, &u.water_ticks);
        hash_u32s(&mut h, &u.order);
        h.update(&u.waypoint);
        hash_i32s(&mut h, &u.off_x);
        hash_i32s(&mut h, &u.off_y);
        h.update(&u.stuck);
        hash_u32s(&mut h, &u.slot);
        hash_u16s(&mut h, &u.calm);
        hash_u32s(&mut h, &u.starve);
        hash_u32s(&mut h, &u.pocket);
        u.hash_slots(&mut h);
        for c in &self.cities {
            h.update(&[c.owner, c.capital_of, c.capture_by]);
            h.update(&c.capture_ticks.to_le_bytes());
            h.update(&c.empty_ticks.to_le_bytes());
            h.update(&c.x.to_le_bytes());
            h.update(&c.y.to_le_bytes());
        }
        for p in &self.player {
            h.update(&p.money.to_le_bytes());
            h.update(&p.net_income.to_le_bytes());
            h.update(&[p.broke as u8, p.eliminated as u8]);
        }
        h.update(&[self.winner.unwrap_or(0), self.sandbox as u8]);
        self.orders.hash_into(&mut h);
        h.update(self.map.owner_plane());
        h.digest()
    }

    /// Units per player (index = player id), for tests and the HUD.
    pub fn unit_counts(&self) -> [u32; MAX_PLAYERS + 1] {
        let mut c = [0u32; MAX_PLAYERS + 1];
        for &o in &self.units.owner {
            c[o as usize] += 1;
        }
        c
    }
}

fn make_pool(n: usize) -> Result<rayon::ThreadPool, String> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(n)
        .thread_name(|k| format!("sim-{k}"))
        .build()
        .map_err(|e| format!("cannot start simulation threads: {e}"))
}

/// Largest distance a unit can move in one tick (walking plus separation push).
pub fn max_move_per_tick(r: &Rules) -> Fx {
    let mut m = 0i32;
    for k in 0..UnitKind::COUNT {
        for row in 0..r.terrain_speed.len() {
            m = m.max(r.speed_per_tick[k].mul_milli(r.terrain_speed[row][k]).0);
        }
    }
    Fx(m + (r.separation_radius.0 / 8).max(1) + 1)
}

macro_rules! hash_slice {
    ($name:ident, $t:ty) => {
        /// Hashes a slice as little-endian bytes through a stack buffer, so
        /// the result is the same on every platform and nothing is allocated.
        pub(crate) fn $name(h: &mut Xxh3, v: &[$t]) {
            const W: usize = core::mem::size_of::<$t>();
            let mut buf = [0u8; 4096];
            for chunk in v.chunks(4096 / W) {
                for (k, x) in chunk.iter().enumerate() {
                    buf[k * W..(k + 1) * W].copy_from_slice(&x.to_le_bytes());
                }
                h.update(&buf[..chunk.len() * W]);
            }
        }
    };
}
hash_slice!(hash_i32s, i32);
hash_slice!(hash_u32s, u32);
hash_slice!(hash_u16s, u16);
