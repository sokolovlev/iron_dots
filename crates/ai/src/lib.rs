//! Bots for Iron Dots (spec 6.3).
//!
//! A bot only reads the world and returns ordinary [`Command`]s, exactly like
//! a player. It is deterministic (integers only, ordered containers), so in
//! a network game every client runs the same bots and stays in sync.
//!
//! Stage 2 bot, three layers in miniature:
//! * strategic — which cities to take (nearest neutral first, then enemy,
//!   capitals weighted), when to defend;
//! * operational — idle units are grouped by target city and sent together;
//!   stronger bots steer around well-defended cities instead of feeding them;
//! * tactical — units with broken morale fall back to the nearest own city.
//!
//! Difficulty changes how often the bot thinks and how carefully it attacks,
//! never what it is allowed to do.

#![forbid(unsafe_code)]

use sim_core::command::{Command, CommandKind};
use sim_core::fx::{FxPoint, ONE, dist2};
use sim_core::scenario::Controller;
use sim_core::unit::{NO_ORDER, flags};
use sim_core::{UnitHandle, UnitKind, World};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Difficulty {
    Easy,
    Normal,
    Hard,
}

impl Difficulty {
    pub const ALL: [Difficulty; 3] = [Difficulty::Easy, Difficulty::Normal, Difficulty::Hard];

    /// The parameters behind this level.
    pub fn profile(self) -> Profile {
        match self {
            Difficulty::Easy => Profile {
                period: 120,
                strategy_period: 120,
                defense_weight: 0,
                reserve: 400_000,
                tactical: false,
                margin: 0,
                rally: false,
                heavy_every: 0,
                hold: false,
                garrison: false,
                raid: false,
            },
            Difficulty::Normal => Profile {
                period: 30,
                strategy_period: 30,
                defense_weight: 50,
                reserve: 0,
                tactical: true,
                margin: 120,
                rally: false,
                heavy_every: 0,
                hold: false,
                garrison: true,
                raid: true,
            },
            Difficulty::Hard => Profile {
                period: 10,
                strategy_period: 30,
                defense_weight: 50,
                reserve: 0,
                tactical: true,
                margin: 250,
                rally: false,
                heavy_every: 0,
                hold: false,
                garrison: true,
                raid: true,
            },
        }
    }
}

/// What a difficulty level changes (spec 6.3: decision frequency and depth
/// of analysis, never cheating). Public so self-play can tune it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Profile {
    /// Ticks between tactical decisions (retreat, holding, defence).
    pub period: u32,
    /// Ticks between strategic ones (purchases, targets); a multiple of
    /// `period`. Sending units off every few ticks feeds them in one by one.
    pub strategy_period: u32,
    /// How much a defended city is avoided when choosing targets: percent
    /// extra distance per full-health light defender (0 = blind).
    pub defense_weight: i64,
    /// Money kept in the treasury (thousandths) before buying.
    pub reserve: i64,
    /// Pull back broken units and defend threatened cities.
    pub tactical: bool,
    /// Concentration of force: attack a city only with a group at least
    /// this many percent as strong as its defenders; 0 = every unit goes
    /// to its own nearest target (a trickle).
    pub margin: i64,
    /// Units not enough for any attack gather at the own city closest to
    /// the next target instead of waiting where they stand.
    pub rally: bool,
    /// Every n-th purchase is a heavy unit (0 = never).
    pub heavy_every: u32,
    /// Units that run into enemies while moving stop and fight where they
    /// stand: a unit without an order defends (less damage taken, slower
    /// morale loss, terrain bonus) and lets the enemy attack into it.
    pub hold: bool,
    /// Keep one unit in every own city: a city with a defender inside
    /// cannot be captured, and a unit in a city costs no upkeep.
    pub garrison: bool,
    /// Undefended enemy cities are worth a single raider (otherwise two).
    pub raid: bool,
}

impl Profile {
    /// Applies `key=value,...` overrides (self-play experiments).
    pub fn tune(&mut self, spec: &str) -> Result<(), String> {
        for kv in spec.split(',').filter(|s| !s.is_empty()) {
            let (k, v) = kv.split_once('=').ok_or_else(|| format!("{kv}: expected key=value"))?;
            let n: i64 = v.parse().map_err(|_| format!("{kv}: not a number"))?;
            match k {
                "period" => self.period = n.clamp(1, 600) as u32,
                "strategy_period" => self.strategy_period = n.clamp(1, 600) as u32,
                "defense_weight" => self.defense_weight = n,
                "reserve" => self.reserve = n,
                "tactical" => self.tactical = n != 0,
                "margin" => self.margin = n,
                "rally" => self.rally = n != 0,
                "heavy_every" => self.heavy_every = n.max(0) as u32,
                "hold" => self.hold = n != 0,
                "garrison" => self.garrison = n != 0,
                "raid" => self.raid = n != 0,
                _ => return Err(format!("unknown AI parameter {k}")),
            }
        }
        Ok(())
    }
}

/// Radius (tiles) around a city in which units count as its defenders.
const CITY_ZONE: i32 = 8;

pub struct Bot {
    player: u8,
    difficulty: Difficulty,
    p: Profile,
    seq: u32,
    /// Units already sent somewhere: handle → target city.
    assigned: BTreeMap<UnitHandle, u16>,
    /// The unit guarding each own city.
    garrison: BTreeMap<u16, UnitHandle>,
    bought: u32,
}

impl Bot {
    pub fn new(player: u8, difficulty: Difficulty) -> Bot {
        Bot::with_profile(player, difficulty, difficulty.profile())
    }

    pub fn with_profile(player: u8, difficulty: Difficulty, p: Profile) -> Bot {
        Bot { player, difficulty, p, seq: 0, assigned: BTreeMap::new(), garrison: BTreeMap::new(), bought: 0 }
    }

    pub fn difficulty(&self) -> Difficulty {
        self.difficulty
    }

    pub fn player(&self) -> u8 {
        self.player
    }

    fn cmd(&mut self, tick: u32, kind: CommandKind) -> Command {
        let c = Command { tick, player: self.player, seq: self.seq, kind };
        self.seq += 1;
        c
    }

    /// Combat strength of a unit: HP × attack-ish weight (heavy counts 1.5).
    fn strength(world: &World, i: usize) -> i64 {
        let u = world.units();
        let w = if u.kind[i] == UnitKind::Heavy as u8 { 3 } else { 2 };
        u.hp[i] as i64 * w
    }
}

/// Strength of one full-health light unit (see [`Bot::strength`]).
const LIGHT: i64 = 20_000;

impl Bot {
    /// Value-weighted distance from `(x, y)` to city `ci`: neutral cities
    /// count as closer, enemy capitals a bit closer than other enemy cities,
    /// defended ones further away (by difficulty).
    fn target_cost(&self, world: &World, ci: usize, x: i32, y: i32, enemy_near: &[i64]) -> i64 {
        let c = &world.cities()[ci];
        let d = dist2(x, y, c.center().x.0, c.center().y.0) >> 16;
        let mut weight = if c.owner == 0 {
            100
        } else if c.capital_of != 0 {
            170
        } else {
            250
        };
        weight += weight * self.p.defense_weight * enemy_near[ci] / (100 * LIGHT);
        d * weight
    }

    fn is_target(world: &World, ci: usize, my_team: u8) -> bool {
        let c = &world.cities()[ci];
        c.owner == 0 || world.team_of(c.owner) != my_team
    }

    /// Easy: each idle unit heads for its own best target.
    fn trickle(&self, world: &World, free: &[usize], enemy_near: &[i64], targets: &mut BTreeMap<u16, Vec<usize>>) {
        let u = world.units();
        let my_team = world.team_of(self.player);
        for &i in free {
            let (x, y) = (u.pos_x[i], u.pos_y[i]);
            let best = (0..world.cities().len())
                .filter(|&ci| Self::is_target(world, ci, my_team))
                .min_by_key(|&ci| (self.target_cost(world, ci, x, y, enemy_near), ci));
            if let Some(ci) = best {
                targets.entry(ci as u16).or_default().push(i);
            }
        }
    }

    /// Normal and hard: targets in order of cost; each gets the nearest
    /// free units until the group outweighs its defenders by `margin`
    /// percent (counting units already on their way). A target that cannot
    /// be matched is skipped, or, with rallying, becomes the gathering point.
    fn concentrate(
        &mut self,
        world: &World,
        free: &[usize],
        enemy_near: &[i64],
        margin: i64,
        targets: &mut BTreeMap<u16, Vec<usize>>,
        out: &mut Vec<Command>,
    ) {
        if free.is_empty() {
            return;
        }
        let u = world.units();
        let cities = world.cities();
        let me = self.player;
        let my_team = world.team_of(me);
        let mut en_route = vec![0i64; cities.len()];
        for (h, &c) in &self.assigned {
            if let Some(i) = u.index_of(*h) {
                en_route[c as usize] += Self::strength(world, i);
            }
        }
        // Order targets by their cost from the nearest free unit.
        let mut order: Vec<(i64, usize)> = (0..cities.len())
            .filter(|&ci| Self::is_target(world, ci, my_team))
            .map(|ci| {
                let cost = free
                    .iter()
                    .map(|&i| self.target_cost(world, ci, u.pos_x[i], u.pos_y[i], enemy_near))
                    .min()
                    .unwrap_or(i64::MAX);
                (cost, ci)
            })
            .collect();
        order.sort_unstable();
        let mut left: Vec<usize> = free.to_vec();
        let mut rally_at: Option<usize> = None;
        for &(_, ci) in &order {
            if left.is_empty() {
                break;
            }
            let undefended = enemy_near[ci] == 0;
            let base = if cities[ci].owner == 0 || (self.p.raid && undefended) { LIGHT } else { 2 * LIGHT };
            let need = (enemy_near[ci] * margin / 100).max(base) - en_route[ci];
            if need <= 0 {
                continue;
            }
            let p = cities[ci].center();
            left.sort_by_key(|&i| (dist2(u.pos_x[i], u.pos_y[i], p.x.0, p.y.0), i));
            let mut sum = 0;
            let mut n = 0;
            while n < left.len() && sum < need {
                sum += Self::strength(world, left[n]);
                n += 1;
            }
            if sum >= need {
                targets.entry(ci as u16).or_default().extend(left.drain(..n));
            } else if self.p.rally && rally_at.is_none() {
                rally_at = Some(ci);
            }
        }
        // Leftovers gather at our city closest to the target they could
        // not yet take, so the next wave starts together.
        let Some(goal) = rally_at else { return };
        let g = cities[goal].center();
        let Some(stage) = (0..cities.len())
            .filter(|&ci| cities[ci].owner == me)
            .min_by_key(|&ci| (dist2(cities[ci].center().x.0, cities[ci].center().y.0, g.x.0, g.y.0), ci))
        else {
            return;
        };
        let s = cities[stage].center();
        let zone2 = (CITY_ZONE as i64 * ONE as i64).pow(2);
        let far: Vec<UnitHandle> = left
            .iter()
            .filter(|&&i| dist2(u.pos_x[i], u.pos_y[i], s.x.0, s.y.0) > zone2)
            .map(|&i| u.handle(i))
            .collect();
        if !far.is_empty() {
            let tick = world.tick();
            out.push(self.cmd(
                tick,
                CommandKind::Move { units: far, path: vec![s], queue: false, formation: Default::default() },
            ));
        }
    }
}

impl Controller for Bot {
    fn think(&mut self, world: &World) -> Vec<Command> {
        let tick = world.tick();
        let period = self.p.period;
        if world.winner().is_some() || (tick + self.player as u32 * 3) % period != 0 {
            return Vec::new();
        }
        let me = self.player;
        let strategic = (tick + self.player as u32 * 3) % self.p.strategy_period.max(period) < period;
        if world.player(me).eliminated {
            return Vec::new();
        }
        let my_team = world.team_of(me);
        let u = world.units();
        let cities = world.cities();
        let mut out = Vec::new();

        // Forget dead units and units whose target became ours.
        // Also release units that stopped short of their target (broken
        // attack, stuck) so they can be used again.
        let zone2_release = (CITY_ZONE as i64 * ONE as i64).pow(2);
        self.assigned.retain(|h, c| {
            let Some(i) = u.index_of(*h) else { return false };
            let city = &cities[*c as usize];
            let still_enemy = city.owner == 0 || world.team_of(city.owner) != my_team;
            let p = city.center();
            let stalled = u.order[i] == NO_ORDER
                && u.flags[i] & flags::IN_COMBAT == 0
                && dist2(u.pos_x[i], u.pos_y[i], p.x.0, p.y.0) > zone2_release;
            still_enemy && !stalled
        });

        // ------------------------------------------------------- economy
        if strategic {
            // A limited war has no money, only a unit budget.
            let moneyless = world.moneyless();
            let mut budget = if moneyless { i64::MAX / 4 } else { world.player(me).money - self.p.reserve };
            let mut units_left = world.unit_budget(me).unwrap_or(u32::MAX);
            for (ci, c) in cities.iter().enumerate() {
                if c.owner != me {
                    continue;
                }
                let Some((used, limit)) = world.city_supply(ci) else { continue };
                let mut free = limit.saturating_sub(used);
                loop {
                    let every = self.p.heavy_every;
                    let want_heavy = every != 0 && self.bought % every == every - 1;
                    let kind = if want_heavy { UnitKind::Heavy } else { UnitKind::Light };
                    let cost = if moneyless { 0 } else { world.rules().cost[kind as usize] };
                    let need = world.rules().supply[kind as usize];
                    if budget < cost || free < need || units_left == 0 {
                        break;
                    }
                    units_left -= 1;
                    budget -= cost;
                    free -= need;
                    self.bought += 1;
                    out.push(self.cmd(tick, CommandKind::Buy { city: ci as u16, kind }));
                }
            }
        }

        // Strength of enemies and own units around each city.
        let zone2 = (CITY_ZONE as i64 * ONE as i64).pow(2);
        let mut enemy_near = vec![0i64; cities.len()];
        let mut own_near = vec![0i64; cities.len()];
        for i in 0..u.len() {
            let ally = world.team_of(u.owner[i]) == my_team;
            for (ci, c) in cities.iter().enumerate() {
                let p = c.center();
                if dist2(u.pos_x[i], u.pos_y[i], p.x.0, p.y.0) <= zone2 {
                    if ally {
                        own_near[ci] += Self::strength(world, i);
                    } else {
                        enemy_near[ci] += Self::strength(world, i);
                    }
                }
            }
        }

        let nearest_own_city = |x: i32, y: i32| -> Option<usize> {
            cities
                .iter()
                .enumerate()
                .filter(|(_, c)| c.owner == me)
                .min_by_key(|(ci, c)| (dist2(x, y, c.center().x.0, c.center().y.0), *ci))
                .map(|(ci, _)| ci)
        };

        // ------------------------------------------------------- tactics
        let mut retreating: Vec<UnitHandle> = Vec::new();
        if self.p.tactical {
            let mut by_city: BTreeMap<usize, Vec<UnitHandle>> = BTreeMap::new();
            for i in 0..u.len() {
                if u.owner[i] != me || u.flags[i] & flags::IN_COMBAT == 0 {
                    continue;
                }
                if (u.morale[i] as u32) * 4 < world.rules().max_morale as u32 {
                    if let Some(ci) = nearest_own_city(u.pos_x[i], u.pos_y[i]) {
                        by_city.entry(ci).or_default().push(u.handle(i));
                    }
                }
            }
            for (ci, units) in by_city {
                for h in &units {
                    self.assigned.remove(h);
                }
                retreating.extend(units.iter().copied());
                let to = cities[ci].center();
                out.push(self.cmd(
                    tick,
                    CommandKind::Move { units, path: vec![to], queue: false, formation: Default::default() },
                ));
            }
        }

        // Hold the line: moving units in contact stop and defend.
        if self.p.hold {
            let holding: Vec<UnitHandle> = (0..u.len())
                .filter(|&i| {
                    u.owner[i] == me
                        && u.flags[i] & flags::ATTACKING != 0
                        && !retreating.contains(&u.handle(i))
                        // At the target city keep pushing, or it is never taken.
                        && self.assigned.get(&u.handle(i)).is_none_or(|&c| {
                            let p = cities[c as usize].center();
                            dist2(u.pos_x[i], u.pos_y[i], p.x.0, p.y.0) > zone2_release
                        })
                })
                .map(|i| u.handle(i))
                .collect();
            if !holding.is_empty() {
                for h in &holding {
                    self.assigned.remove(h);
                }
                retreating.extend(holding.iter().copied());
                out.push(self.cmd(tick, CommandKind::Stop { units: holding }));
            }
        }

        // ------------------------------------------------------- strategy
        if !strategic {
            return out;
        }
        // Garrisons: forget dead ones and lost cities.
        self.garrison.retain(|&c, h| u.index_of(*h).is_some() && cities[c as usize].owner == me);
        let guards: Vec<UnitHandle> = self.garrison.values().copied().collect();
        // Idle units: no order, not retreating, not assigned, not guarding.
        let idle: Vec<usize> = (0..u.len())
            .filter(|&i| {
                u.owner[i] == me
                    && u.order[i] == NO_ORDER
                    && !self.assigned.contains_key(&u.handle(i))
                    && !retreating.contains(&u.handle(i))
                    // Holding units finish their fight first.
                    && !(self.p.hold && u.flags[i] & flags::IN_COMBAT != 0)
                    && !guards.contains(&u.handle(i))
            })
            .collect();
        // Unguarded own cities take the nearest idle unit.
        let mut idle = idle;
        if self.p.garrison {
            for (ci, c) in cities.iter().enumerate() {
                if c.owner != me || self.garrison.contains_key(&(ci as u16)) {
                    continue;
                }
                let p = c.center();
                let Some(k) =
                    (0..idle.len()).min_by_key(|&k| (dist2(u.pos_x[idle[k]], u.pos_y[idle[k]], p.x.0, p.y.0), k))
                else {
                    break;
                };
                let i = idle.swap_remove(k);
                let h = u.handle(i);
                self.garrison.insert(ci as u16, h);
                if dist2(u.pos_x[i], u.pos_y[i], p.x.0, p.y.0) > (ONE as i64) * (ONE as i64) {
                    out.push(self.cmd(
                        tick,
                        CommandKind::Move {
                            units: vec![h],
                            path: vec![p],
                            queue: false,
                            formation: Default::default(),
                        },
                    ));
                }
            }
        }
        if idle.is_empty() {
            return out;
        }

        // Defend first: threatened own cities pull idle units.
        let mut targets: BTreeMap<u16, Vec<usize>> = BTreeMap::new();
        let mut taken = vec![false; idle.len()];
        if self.p.tactical {
            for (ci, c) in cities.iter().enumerate() {
                if c.owner != me || enemy_near[ci] <= own_near[ci] {
                    continue;
                }
                let p = c.center();
                let mut order: Vec<usize> = (0..idle.len()).filter(|&k| !taken[k]).collect();
                order.sort_by_key(|&k| (dist2(u.pos_x[idle[k]], u.pos_y[idle[k]], p.x.0, p.y.0), k));
                let mut sent = 0i64;
                for k in order {
                    if sent > enemy_near[ci] - own_near[ci] {
                        break;
                    }
                    taken[k] = true;
                    sent += Self::strength(world, idle[k]);
                    targets.entry(ci as u16).or_default().push(idle[k]);
                }
            }
        }

        let free: Vec<usize> = (0..idle.len()).filter(|&k| !taken[k]).map(|k| idle[k]).collect();
        match self.p.margin {
            0 => self.trickle(world, &free, &enemy_near, &mut targets),
            margin => self.concentrate(world, &free, &enemy_near, margin, &mut targets, &mut out),
        }

        for (ci, group) in targets {
            let handles: Vec<UnitHandle> = group.iter().map(|&i| u.handle(i)).collect();
            for &h in &handles {
                self.assigned.insert(h, ci);
            }
            let to: FxPoint = cities[ci as usize].center();
            out.push(self.cmd(
                tick,
                CommandKind::Move { units: handles, path: vec![to], queue: false, formation: Default::default() },
            ));
        }
        out
    }
}
