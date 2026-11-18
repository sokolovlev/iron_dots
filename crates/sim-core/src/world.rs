//! The world state and the fixed-order tick (spec 4.3).
//!
//! Phases implemented in stage 1:
//!  1. apply commands (sorted by player, seq)
//!  2. rebuild the spatial grid
//!  4. intent: desired velocity from the current order
//!  5. separation + integration + terrain blocking (pull: every unit writes only itself)
//!  6. combat: contacts → damage pulled from enemies → HP/morale update
//! 10. remove dead units
//! 11. state hash (on demand; the runner records it every 30 ticks)
//!
//! Stage 2 adds pathing (3), territory (7), supply (8) and cities (9).

use crate::balance::{Balance, Rules, SHIP_ROW};
use crate::command::{Command, CommandKind};
use crate::fx::{Fx, FxPoint, ONE, dist2, isqrt_u64, length};
use crate::grid::Grid;
use crate::map::{Map, Terrain};
use crate::order::{MAX_PATH_POINTS, OrderPool};
use crate::rng::{Purpose, rand, stochastic_round_q16};
use crate::unit::{NO_ORDER, UnitHandle, UnitKind, UnitStore, flags};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use xxhash_rust::xxh3::Xxh3;

/// Max players per match (ids 1..=8; 0 is "nobody").
pub const MAX_PLAYERS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerSetup {
    /// Team id; players on the same team are allies.
    pub team: u8,
}

/// Counters of the last tick, for debug panels and tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TickStats {
    pub commands_applied: u32,
    pub commands_rejected: u32,
    pub units_in_combat: u32,
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
    // Scratch buffers reused every tick (no allocations in steady state).
    vel_x: Vec<i32>,
    vel_y: Vec<i32>,
    next_x: Vec<i32>,
    next_y: Vec<i32>,
    arrive: Vec<bool>,
    contacts: Vec<u16>,
    dmg_in: Vec<u32>,
    cmd_order: Vec<u32>,
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
            vel_x: Vec::new(),
            vel_y: Vec::new(),
            next_x: Vec::new(),
            next_y: Vec::new(),
            arrive: Vec::new(),
            contacts: Vec::new(),
            dmg_in: Vec::new(),
            cmd_order: Vec::new(),
        })
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
        self.stats = TickStats::default();
        self.prepare_scratch();
        self.apply_commands(commands);
        self.grid.rebuild(&self.units.pos_x, &self.units.pos_y);
        self.phase_intent();
        self.phase_move();
        self.phase_orders_and_terrain();
        self.phase_combat();
        self.phase_remove_dead();
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
                self.apply_move(&idx, path, *queue);
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

    fn apply_move(&mut self, idx: &[usize], path: Vec<FxPoint>, queue: bool) {
        let n = idx.len() as i64;
        let cx = idx.iter().map(|&i| self.units.pos_x[i] as i64).sum::<i64>() / n;
        let cy = idx.iter().map(|&i| self.units.pos_y[i] as i64).sum::<i64>() / n;
        // Keep the formation, but not wider than ~ (1 + sqrt(n)/2) tiles.
        let cap = ONE as i64 + (isqrt_u64(n as u64) as i64) * (ONE as i64) / 2;
        let new_order = self.orders.alloc(path, NO_ORDER);
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
        let p = self.orders.get(o).points[self.units.waypoint[i] as usize];
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

    fn phase_intent(&mut self) {
        for i in 0..self.units.len() {
            self.vel_x[i] = 0;
            self.vel_y[i] = 0;
            self.arrive[i] = false;
            let Some((tx, ty)) = self.waypoint_target(i) else { continue };
            let dx = tx - self.units.pos_x[i];
            let dy = ty - self.units.pos_y[i];
            let d = length(dx, dy);
            let s = self.speed_of(i);
            if d <= s {
                self.vel_x[i] = dx;
                self.vel_y[i] = dy;
                self.arrive[i] = true;
            } else {
                self.vel_x[i] = (dx as i64 * s as i64 / d as i64) as i32;
                self.vel_y[i] = (dy as i64 * s as i64 / d as i64) as i32;
            }
        }
    }

    // ---------------------------------------------------------------- phase 5

    fn phase_move(&mut self) {
        let r = self.rules.separation_radius.0 as i64;
        let r2 = r * r;
        let cap = (r / 8).max(1);
        let max_x = (self.map.width() as i32) * ONE - 1;
        let max_y = (self.map.height() as i32) * ONE - 1;
        let u = &self.units;
        for i in 0..u.len() {
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
                    let mut d = isqrt_u64(d2 as u64) as i64;
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
                    let overlap = r - isqrt_u64(d2 as u64) as i64;
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
            let (fx, fy) = if pass(nx, ny) {
                (nx, ny)
            } else if pass(nx, y) {
                (nx, y)
            } else if pass(x, ny) {
                (x, ny)
            } else {
                (x, y)
            };
            self.next_x[i] = fx;
            self.next_y[i] = fy;
        }
    }

    fn phase_orders_and_terrain(&mut self) {
        let stuck_limit = (2 * self.rules.tick_rate).min(250) as u8;
        for i in 0..self.units.len() {
            // Waypoint progress and stuck detection.
            if let Some((tx, ty)) = self.waypoint_target(i) {
                let before = dist2(self.units.pos_x[i], self.units.pos_y[i], tx, ty);
                let after = dist2(self.next_x[i], self.next_y[i], tx, ty);
                let mut advance = self.arrive[i];
                if !advance && self.units.flags[i] & flags::IN_COMBAT == 0 {
                    if after >= before {
                        self.units.stuck[i] = self.units.stuck[i].saturating_add(1);
                        advance = self.units.stuck[i] >= stuck_limit;
                    } else {
                        self.units.stuck[i] = 0;
                    }
                }
                if advance {
                    self.units.stuck[i] = 0;
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
            }
            self.units.pos_x[i] = self.next_x[i];
            self.units.pos_y[i] = self.next_y[i];
            // Water and ships.
            let t = self.map.terrain_raw((self.next_x[i] >> 16) as u32, (self.next_y[i] >> 16) as u32);
            if t == Terrain::Water as u8 {
                let w = self.units.water_ticks[i].saturating_add(1);
                self.units.water_ticks[i] = w;
                if w >= self.rules.ship_after_ticks {
                    self.units.flags[i] |= flags::SHIP;
                }
            } else {
                self.units.water_ticks[i] = 0;
                self.units.flags[i] &= !flags::SHIP;
            }
            self.units.row[i] = if self.units.flags[i] & flags::SHIP != 0 { SHIP_ROW as u8 } else { t };
        }
    }

    // ---------------------------------------------------------------- phase 6

    fn phase_combat(&mut self) {
        let r2 = self.rules.contact_r2;
        let n = self.units.len();
        // 6a. Contacts with enemies (positions after movement).
        let mut in_combat = 0;
        for i in 0..n {
            let u = &self.units;
            let (x, y, me) = (u.pos_x[i], u.pos_y[i], u.owner[i]);
            let mut c = 0u16;
            self.grid.for_each_near(x, y, |j| {
                if self.enemies(me, u.owner[j]) && dist2(u.pos_x[j], u.pos_y[j], x, y) <= r2 {
                    c = c.saturating_add(1);
                }
            });
            self.contacts[i] = c;
            let mut f = self.units.flags[i] & !(flags::IN_COMBAT | flags::ATTACKING);
            if c > 0 {
                in_combat += 1;
                f |= flags::IN_COMBAT;
                if self.units.order[i] != NO_ORDER {
                    f |= flags::ATTACKING;
                }
            }
            self.units.flags[i] = f;
        }
        self.stats.units_in_combat = in_combat;

        // 6b. Each unit pulls the damage dealt to it by enemies in contact.
        let rl = &self.rules;
        for b in 0..n {
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
            let dmg = stochastic_round_q16(q16, rand(self.seed, self.tick, u.slot[b], Purpose::Damage));
            self.dmg_in[b] = dmg.min(u32::MAX as u64) as u32;
        }

        // 6c. Apply damage and morale.
        for i in 0..n {
            let u = &mut self.units;
            u.hp[i] = u.hp[i].saturating_sub(self.dmg_in[i].min(u16::MAX as u32) as u16);
            let f = u.flags[i];
            let r = rand(self.seed, self.tick, u.slot[i], Purpose::Morale);
            if f & flags::IN_COMBAT != 0 {
                let loss_q16 = if f & flags::ATTACKING != 0 {
                    self.rules.morale_loss_attack_q16
                } else {
                    self.rules.morale_loss_defend_q16
                };
                let loss = stochastic_round_q16(loss_q16, r).min(u16::MAX as u64) as u16;
                u.morale[i] = u.morale[i].saturating_sub(loss);
                if u.morale[i] == 0 && f & flags::ATTACKING != 0 {
                    // Broken attack: the order is cancelled, the unit holds.
                    let o = std::mem::replace(&mut u.order[i], NO_ORDER);
                    u.waypoint[i] = 0;
                    u.flags[i] &= !flags::ATTACKING;
                    self.orders.release(o);
                }
            } else {
                let gain = stochastic_round_q16(self.rules.morale_regen_q16, r).min(u16::MAX as u64) as u16;
                u.morale[i] = u.morale[i].saturating_add(gain).min(self.rules.max_morale);
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
        u.hash_slots(&mut h);
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
