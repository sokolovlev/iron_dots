//! The world state and the fixed-order tick (spec 4.3).
//!
//!  1. apply commands (sorted by player, seq); Move runs the pathfinder
//!  2. rebuild the spatial grid
//!  3. (flow fields — stage 5; stage 2 plans routes when a command is applied)
//!  4. intent: desired velocity from the current order
//!  5. separation + integration + terrain blocking (pull: every unit writes only itself)
//!  6. combat: contacts → damage (combat, water, starvation) → HP/morale update
//!  7. territory painting
//!  8. supply: pockets every 15 ticks, starvation rates, healing
//!  9. cities: capture; once per second money and victory check
//! 10. remove dead units, then spawn bought ones
//! 11. state hash (on demand; the runner records it every 30 ticks)

use crate::balance::{Balance, Rules, SHIP_ROW};
use crate::command::{Command, CommandKind};
use crate::fx::{Fx, FxPoint, ONE, dist2, isqrt_u64, length};
use crate::grid::Grid;
use crate::map::{Map, Terrain};
use crate::order::{MAX_PATH_POINTS, OrderPool};
use crate::pathfind;
use crate::rng::{Purpose, rand, stochastic_round_q16};
use crate::territory::{self, NO_POCKET, SUPPLY_INTERVAL};
use crate::unit::{NO_ORDER, UnitHandle, UnitKind, UnitStore, flags};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::mem::take;
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3;

/// Units per parallel work item. A fixed size (never derived from the
/// thread count) keeps every result independent of how many threads run.
pub const BLOCK: usize = 1024;

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

/// Tick phases as reported to the probe of [`World::step_with`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    Commands,
    Grid,
    Intent,
    Move,
    Combat,
    Territory,
    Supply,
    Cities,
    Spawn,
}

impl Phase {
    pub const ALL: [Phase; 9] = [
        Phase::Commands,
        Phase::Grid,
        Phase::Intent,
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
            Phase::Intent => "intent",
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
    /// Pocket label per tile (row-major), owner/limit/used per label.
    labels: Vec<u32>,
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
    vel_x: Vec<i32>,
    vel_y: Vec<i32>,
    next_x: Vec<i32>,
    next_y: Vec<i32>,
    arrive: Vec<bool>,
    contacts: Vec<u16>,
    dmg_in: Vec<u32>,
    cmd_order: Vec<u32>,
    stack_scratch: Vec<(u32, u32)>,
    path_scratch: pathfind::Scratch,
    paint_scratch: Vec<Vec<(u16, u16, u8)>>,
    /// Bit of each player's team (teams numbered in ascending id order).
    team_bit: [u8; MAX_PLAYERS + 1],
    chunk_mask: Vec<u8>,
    hot: Vec<bool>,
    pool: Arc<rayon::ThreadPool>,
    threads: usize,
}

impl World {
    /// Creates an empty world. `players[k]` describes player id `k + 1`.
    pub fn new(map: Map, balance: &Balance, seed: u64, players: &[PlayerSetup]) -> Result<World, String> {
        balance.validate()?;
        if players.is_empty() || players.len() > MAX_PLAYERS {
            return Err(format!("need 1..={MAX_PLAYERS} players, got {}", players.len()));
        }
        let rules = Rules::new(balance);
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
            labels: Vec::new(),
            pocket_owner: Vec::new(),
            pocket_limit: Vec::new(),
            pocket_used: Vec::new(),
            territory_changed: true,
            dirty_chunks: vec![true; chunks],
            pending_spawns: Vec::new(),
            sandbox: false,
            vel_x: Vec::new(),
            vel_y: Vec::new(),
            next_x: Vec::new(),
            next_y: Vec::new(),
            arrive: Vec::new(),
            contacts: Vec::new(),
            dmg_in: Vec::new(),
            cmd_order: Vec::new(),
            stack_scratch: Vec::new(),
            path_scratch: pathfind::Scratch::default(),
            paint_scratch: Vec::new(),
            team_bit,
            chunk_mask: Vec::new(),
            hot: Vec::new(),
            pool: Arc::new(make_pool(1)?),
            threads: 1,
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
        self.cities.push(City {
            x: x as u16,
            y: y as u16,
            owner,
            capital_of: if capital { owner } else { 0 },
            capture_by: 0,
            capture_ticks: 0,
        });
        self.territory_changed = true;
        Ok(idx)
    }

    /// Gives `player` all non-city tiles within `radius` of tile `(x, y)`
    /// (match setup: land around starting capitals).
    pub fn claim(&mut self, player: u8, x: u32, y: u32, radius: u32) {
        let r = radius as i64;
        for ny in y.saturating_sub(radius)..=(y + radius).min(self.map.height() - 1) {
            for nx in x.saturating_sub(radius)..=(x + radius).min(self.map.width() - 1) {
                let (dx, dy) = (nx as i64 - x as i64, ny as i64 - y as i64);
                if dx * dx + dy * dy <= r * r && self.map.terrain(nx, ny) != Terrain::City {
                    self.map.set_owner(nx, ny, player);
                }
            }
        }
        self.territory_changed = true;
        self.dirty_chunks.fill(true);
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
        if city.owner == 0 || self.labels.is_empty() {
            return None;
        }
        let l = self.labels[(city.y as u32 * self.map.width() + city.x as u32) as usize];
        (l != NO_POCKET).then_some(l)
    }

    #[inline]
    fn enemies(&self, a: u8, b: u8) -> bool {
        self.team[a as usize] != self.team[b as usize]
    }

    /// Places a full-health unit. Used for match setup and (from stage 2) by
    /// city production at the end of a tick.
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
        phase!(Phase::Grid, self.grid.rebuild(&self.units.pos_x, &self.units.pos_y));
        phase!(Phase::Intent, self.phase_intent());
        phase!(Phase::Move, {
            self.phase_move();
            self.phase_orders_and_terrain()
        });
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

    fn prepare_scratch(&mut self) {
        let n = self.units.len();
        self.vel_x.resize(n, 0);
        self.vel_y.resize(n, 0);
        self.next_x.resize(n, 0);
        self.next_y.resize(n, 0);
        self.arrive.resize(n, false);
        self.contacts.resize(n, 0);
        self.dmg_in.resize(n, 0);
    }

    // ---------------------------------------------------------------- phase 1

    fn apply_commands(&mut self, commands: &[Command]) {
        self.path_scratch.budget = pathfind::TICK_BUDGET;
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
            CommandKind::Move { units, path, queue } => {
                let idx = self.own_units(c.player, units);
                if idx.is_empty() || path.is_empty() || path.len() > MAX_PATH_POINTS {
                    return false;
                }
                let max_x = Fx((self.map.width() as i32) * ONE - 1);
                let max_y = Fx((self.map.height() as i32) * ONE - 1);
                let path: Vec<FxPoint> =
                    path.iter().map(|p| FxPoint::new(p.x.clamp(Fx::ZERO, max_x), p.y.clamp(Fx::ZERO, max_y))).collect();
                let (points, via) = self.plan_route(&idx, &path);
                self.apply_move(&idx, points, via, *queue);
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

    /// Inserts pathfinder waypoints where a straight segment of the requested
    /// path would cross mountains or costlier terrain than its endpoints
    /// (spec 2.9: "segments through mountains are routed around").
    fn plan_route(&mut self, idx: &[usize], path: &[FxPoint]) -> (Vec<FxPoint>, Vec<bool>) {
        const MAX_POINTS: usize = u8::MAX as usize;
        let heavy = idx.iter().any(|&i| self.units.kind[i] == UnitKind::Heavy as u8);
        let kind = if heavy { UnitKind::Heavy } else { UnitKind::Light };
        let costs = pathfind::terrain_costs(&self.rules, kind);
        let n = idx.len() as i64;
        let cx = idx.iter().map(|&i| self.units.pos_x[i] as i64).sum::<i64>() / n;
        let cy = idx.iter().map(|&i| self.units.pos_y[i] as i64).sum::<i64>() / n;
        let tile = |x: i64, y: i64| ((x >> 16) as u32, (y >> 16) as u32);
        let cost = |t: (u32, u32)| costs[self.map.terrain_raw(t.0, t.1) as usize];
        let mut prev = tile(cx, cy);
        let (mut points, mut via) = (Vec::new(), Vec::new());
        for p in path {
            let target = tile(p.x.0 as i64, p.y.0 as i64);
            let limit = cost(prev).max(cost(target)).max(1000);
            if !pathfind::clear_line(&self.map, &costs, prev, target, limit) {
                if let Some(route) = pathfind::find_route(&self.map, &costs, prev, target, &mut self.path_scratch) {
                    let wps = pathfind::smooth(&self.map, &costs, &route);
                    for &(x, y) in &wps[1..wps.len() - 1] {
                        points.push(FxPoint::tile_center(x as i32, y as i32));
                        via.push(true);
                    }
                }
            }
            points.push(*p);
            via.push(false);
            prev = target;
        }
        // Order waypoints are indexed by u8: keep the last MAX_POINTS.
        if points.len() > MAX_POINTS {
            let cut = points.len() - MAX_POINTS;
            points.drain(..cut);
            via.drain(..cut);
        }
        (points, via)
    }

    fn apply_buy(&mut self, player: u8, city: usize, kind: UnitKind) -> bool {
        if city >= self.cities.len() || self.cities[city].owner != player {
            return false;
        }
        let cost = self.rules.cost[kind as usize];
        if self.player[player as usize].money < cost {
            return false;
        }
        if self.sandbox {
            self.player[player as usize].money -= cost;
            self.pending_spawns.push((player, kind, city as u16));
            return true;
        }
        if self.labels.is_empty() {
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
        true
    }

    fn apply_move(&mut self, idx: &[usize], path: Vec<FxPoint>, via: Vec<bool>, queue: bool) {
        let n = idx.len() as i64;
        let cx = idx.iter().map(|&i| self.units.pos_x[i] as i64).sum::<i64>() / n;
        let cy = idx.iter().map(|&i| self.units.pos_y[i] as i64).sum::<i64>() / n;
        // Keep the formation, but not wider than ~ (1 + sqrt(n)/2) tiles.
        let cap = ONE as i64 + (isqrt_u64(n as u64) as i64) * (ONE as i64) / 2;
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
                let mut ox = self.units.pos_x[i] as i64 - cx;
                let mut oy = self.units.pos_y[i] as i64 - cy;
                let len = isqrt_u64((ox * ox + oy * oy) as u64) as i64;
                if len > cap {
                    ox = ox * cap / len;
                    oy = oy * cap / len;
                }
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

    #[inline]
    fn speed_of(&self, i: usize) -> i32 {
        let kind = self.units.kind[i] as usize;
        let row = self.units.row[i] as usize;
        let mut s = self.rules.speed_per_tick[kind].mul_milli(self.rules.terrain_speed[row][kind]);
        if self.units.flags[i] & flags::IN_COMBAT != 0 {
            s = s.mul_milli(self.rules.engaged_speed);
        }
        s.0
    }

    /// Desired velocity of unit `i` and whether it reaches its waypoint now.
    #[inline]
    fn intent_of(&self, i: usize) -> (i32, i32, bool) {
        let Some((tx, ty)) = self.waypoint_target(i) else { return (0, 0, false) };
        let dx = tx - self.units.pos_x[i];
        let dy = ty - self.units.pos_y[i];
        let d = length(dx, dy);
        let s = self.speed_of(i);
        if d <= s {
            (dx, dy, true)
        } else {
            ((dx as i64 * s as i64 / d as i64) as i32, (dy as i64 * s as i64 / d as i64) as i32, false)
        }
    }

    fn phase_intent(&mut self) {
        let (mut vx, mut vy, mut ar) = (take(&mut self.vel_x), take(&mut self.vel_y), take(&mut self.arrive));
        let this = &*self;
        this.pool.install(|| {
            (vx.par_chunks_mut(BLOCK), vy.par_chunks_mut(BLOCK), ar.par_chunks_mut(BLOCK))
                .into_par_iter()
                .enumerate()
                .for_each(|(k, (vx, vy, ar))| {
                    for j in 0..vx.len() {
                        (vx[j], vy[j], ar[j]) = this.intent_of(k * BLOCK + j);
                    }
                });
        });
        (self.vel_x, self.vel_y, self.arrive) = (vx, vy, ar);
    }

    // ---------------------------------------------------------------- phase 5

    /// Position of unit `i` after walking and soft separation, before
    /// terrain blocking is resolved. Reads start-of-tick positions only.
    #[inline]
    fn move_of(&self, i: usize) -> (i32, i32) {
        let r = self.rules.separation_radius.0 as i64;
        let r2 = r * r;
        let cap = (r / 8).max(1);
        let max_x = (self.map.width() as i32) * ONE - 1;
        let max_y = (self.map.height() as i32) * ONE - 1;
        let u = &self.units;
        let (x, y) = (u.pos_x[i], u.pos_y[i]);
        // Soft separation, pulled from neighbours' start-of-tick positions.
        let (mut px, mut py) = (0i64, 0i64);
        if r > 0 {
            self.grid.for_each_near(x, y, |j| {
                if j == i {
                    return;
                }
                let d2 = dist2(u.pos_x[j], u.pos_y[j], x, y);
                if d2 >= r2 {
                    return;
                }
                let (mut dx, mut dy) = ((x - u.pos_x[j]) as i64, (y - u.pos_y[j]) as i64);
                let dist = isqrt_u64(d2 as u64) as i64;
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
        }
        let nx = (x as i64 + self.vel_x[i] as i64 + px).clamp(0, max_x as i64) as i32;
        let ny = (y as i64 + self.vel_y[i] as i64 + py).clamp(0, max_y as i64) as i32;
        // Impassable tiles: slide along an axis, otherwise stay.
        let pass =
            |x: i32, y: i32| self.rules.passable[self.map.terrain_raw((x >> 16) as u32, (y >> 16) as u32) as usize];
        if pass(nx, ny) {
            (nx, ny)
        } else if pass(nx, y) {
            (nx, y)
        } else if pass(x, ny) {
            (x, ny)
        } else {
            (x, y)
        }
    }

    fn phase_move(&mut self) {
        let (mut nx, mut ny) = (take(&mut self.next_x), take(&mut self.next_y));
        let this = &*self;
        this.pool.install(|| {
            (nx.par_chunks_mut(BLOCK), ny.par_chunks_mut(BLOCK)).into_par_iter().enumerate().for_each(
                |(k, (nx, ny))| {
                    for j in 0..nx.len() {
                        (nx[j], ny[j]) = this.move_of(k * BLOCK + j);
                    }
                },
            );
        });
        (self.next_x, self.next_y) = (nx, ny);
    }

    fn phase_orders_and_terrain(&mut self) {
        let stuck_limit = (2 * self.rules.tick_rate).min(250) as u8;
        let n = self.units.len();
        // 5b. Waypoint progress and stuck detection (parallel, per unit).
        // `arrive` becomes "advance to the next waypoint".
        let (mut adv, mut stuck) = (take(&mut self.arrive), take(&mut self.units.stuck));
        let this = &*self;
        this.pool.install(|| {
            (adv.par_chunks_mut(BLOCK), stuck.par_chunks_mut(BLOCK)).into_par_iter().enumerate().for_each(
                |(k, (adv, stuck))| {
                    for j in 0..adv.len() {
                        let i = k * BLOCK + j;
                        let Some((tx, ty)) = this.waypoint_target(i) else {
                            adv[j] = false;
                            continue;
                        };
                        let before = dist2(this.units.pos_x[i], this.units.pos_y[i], tx, ty);
                        let after = dist2(this.next_x[i], this.next_y[i], tx, ty);
                        let mut a = adv[j];
                        if !a && this.units.flags[i] & flags::IN_COMBAT == 0 {
                            if after >= before {
                                stuck[j] = stuck[j].saturating_add(1);
                                a = stuck[j] >= stuck_limit;
                            } else {
                                stuck[j] = 0;
                            }
                        }
                        if a {
                            stuck[j] = 0;
                        }
                        adv[j] = a;
                    }
                },
            );
        });
        self.units.stuck = stuck;
        // 5c. Order transitions touch the shared order pool: sequential, in
        // ascending unit order (same order of retains/releases as always).
        for (i, &a) in adv.iter().enumerate().take(n) {
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
        // 5d. Commit positions; water and ships (parallel).
        let u = &mut self.units;
        let (mut px, mut py, mut wt, mut fl, mut row) =
            (take(&mut u.pos_x), take(&mut u.pos_y), take(&mut u.water_ticks), take(&mut u.flags), take(&mut u.row));
        let this = &*self;
        this.pool.install(|| {
            (
                px.par_chunks_mut(BLOCK),
                py.par_chunks_mut(BLOCK),
                wt.par_chunks_mut(BLOCK),
                fl.par_chunks_mut(BLOCK),
                row.par_chunks_mut(BLOCK),
            )
                .into_par_iter()
                .enumerate()
                .for_each(|(k, (px, py, wt, fl, row))| {
                    for j in 0..px.len() {
                        let i = k * BLOCK + j;
                        let (x, y) = (this.next_x[i], this.next_y[i]);
                        (px[j], py[j]) = (x, y);
                        let t = this.map.terrain_raw((x >> 16) as u32, (y >> 16) as u32);
                        if t == Terrain::Water as u8 {
                            wt[j] = wt[j].saturating_add(1);
                            if wt[j] >= this.rules.ship_after_ticks {
                                fl[j] |= flags::SHIP;
                            }
                        } else {
                            wt[j] = 0;
                            fl[j] &= !flags::SHIP;
                        }
                        row[j] = if fl[j] & flags::SHIP != 0 { SHIP_ROW as u8 } else { t };
                    }
                });
        });
        let u = &mut self.units;
        (u.pos_x, u.pos_y, u.water_ticks, u.flags, u.row) = (px, py, wt, fl, row);
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

    /// Damage unit `b` pulls from enemies in contact this tick, plus water,
    /// starvation and empty-treasury damage.
    #[inline]
    fn damage_in(&self, b: usize) -> u32 {
        let r2 = self.rules.contact_r2;
        let rl = &self.rules;
        let u = &self.units;
        let mut q16: u64 = 0;
        if self.contacts[b] > 0 {
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
            self.grid.for_each_near(x, y, |a| {
                if !self.enemies(me, u.owner[a]) || dist2(u.pos_x[a], u.pos_y[a], x, y) > r2 {
                    return;
                }
                let ka = u.kind[a] as usize;
                let f_hp = 32_768 + 32_768 * u.hp[a] as u64 / rl.max_hp as u64;
                let f_m = 32_768 + 32_768 * u.morale[a] as u64 / rl.max_morale as u64;
                let atk = (rl.atk[ka].0 * rl.terrain_atk[u.row[a] as usize][ka].0 / 1000) as u64;
                let mut d = (rl.base_dmg_q16 * f_hp) >> 16;
                d = (d * f_m) >> 16;
                d = d * atk / def;
                d = d * taken / 1000;
                q16 += d / self.contacts[a].max(1) as u64;
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
        let r2 = self.rules.contact_r2;
        // 6a. Contacts with enemies (positions after movement).
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
                        let (x, y, me) = (u.pos_x[i], u.pos_y[i], u.owner[i]);
                        let mut c = 0u16;
                        if this.is_hot(x, y) {
                            this.grid.for_each_near(x, y, |j| {
                                if this.enemies(me, u.owner[j]) && dist2(u.pos_x[j], u.pos_y[j], x, y) <= r2 {
                                    c = c.saturating_add(1);
                                }
                            });
                        }
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

        // 6b. Each unit pulls the damage dealt to it by enemies in contact.
        let mut dmg = take(&mut self.dmg_in);
        let this = &*self;
        this.pool.install(|| {
            dmg.par_chunks_mut(BLOCK).enumerate().for_each(|(k, dmg)| {
                for (j, d) in dmg.iter_mut().enumerate() {
                    *d = this.damage_in(k * BLOCK + j);
                }
            });
        });
        self.dmg_in = dmg;

        // 6c. Apply damage and morale; broken attacks drop their orders. The
        // releases touch the shared order pool, so they are collected per
        // block and applied afterwards in ascending unit order.
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

    /// Territory: candidate tiles are found in parallel from the
    /// start-of-phase map; writes are applied in ascending unit order with
    /// the ally check repeated, which gives exactly the sequential result.
    fn phase_territory(&mut self) {
        let mut out = take(&mut self.paint_scratch);
        let this = &*self;
        let n = this.units.len();
        let blocks = n.div_ceil(BLOCK);
        out.resize_with(blocks.max(out.len()), Vec::new);
        this.pool.install(|| {
            out[..blocks].par_iter_mut().enumerate().for_each(|(k, out)| {
                out.clear();
                let mut enemies = Vec::new();
                for i in k * BLOCK..((k + 1) * BLOCK).min(n) {
                    if (this.tick + this.units.slot[i]) % territory::PAINT_INTERVAL != 0 {
                        continue;
                    }
                    let hot = this.is_hot(this.units.pos_x[i], this.units.pos_y[i]);
                    territory::paint_candidates(
                        &this.map,
                        &this.units,
                        &this.grid,
                        &this.team,
                        this.rules.territory_radius,
                        i,
                        hot,
                        &mut enemies,
                        out,
                    );
                }
            });
        });
        let changed = territory::apply_paint(&mut self.map, &self.team, &out[..blocks], &mut self.dirty_chunks);
        self.paint_scratch = out;
        self.territory_changed |= changed;
    }

    // ---------------------------------------------------------------- phase 8

    /// Pockets, supply limits and starvation rates (every 15 ticks).
    fn recompute_supply(&mut self) {
        if self.territory_changed || self.labels.is_empty() {
            self.pocket_owner = territory::label_pockets(&self.map, &mut self.labels, &mut self.stack_scratch);
            self.territory_changed = false;
        }
        let np = self.pocket_owner.len();
        self.pocket_limit.clear();
        self.pocket_limit.resize(np, 0);
        self.pocket_used.clear();
        self.pocket_used.resize(np, 0);
        let w = self.map.width();
        for c in &self.cities {
            if c.owner == 0 {
                continue;
            }
            let l = self.labels[(c.y as u32 * w + c.x as u32) as usize];
            if l != NO_POCKET && self.pocket_owner[l as usize] == c.owner {
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
                    *p = territory::unit_pocket(
                        &this.map,
                        &this.labels,
                        u.owner[i],
                        u.pos_x[i],
                        u.pos_y[i],
                        this.rules.territory_radius,
                    );
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
        for &(owner, kind, city) in &self.pending_spawns {
            let c = &self.cities[city as usize];
            let l = self.labels[(c.y as u32 * w + c.x as u32) as usize];
            if l != NO_POCKET && self.pocket_owner[l as usize] == owner {
                self.pocket_used[l as usize] += self.rules.supply[kind as usize];
            }
        }
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
        let q16 = if in_city { self.rules.heal_city_q16 } else { self.rules.heal_q16 };
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
                    }
                }
                Some(_) if contested && !defended => {} // frozen while contested
                _ => {
                    city.capture_by = 0;
                    city.capture_ticks = 0;
                }
            }
        }
    }

    /// Once per second: income from cities, upkeep of units outside cities.
    fn settle_money(&mut self) {
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
            if !self.player[p].eliminated && cities_of[p] == 0 && counts[p] == 0 {
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
