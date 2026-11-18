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

    /// Ticks between decisions.
    fn period(self) -> u32 {
        match self {
            Difficulty::Easy => 90,
            Difficulty::Normal => 30,
            Difficulty::Hard => 10,
        }
    }

    /// How much a defended city is avoided when choosing targets, percent
    /// extra distance per full-health light defender (0 = blind).
    fn defense_weight(self) -> i64 {
        match self {
            Difficulty::Easy => 0,
            Difficulty::Normal => 50,
            Difficulty::Hard => 100,
        }
    }

    /// Every n-th purchase is a heavy unit (0 = never). The stage 2 bot
    /// does not yet steer heavies onto open ground, and in self-play they
    /// made it weaker, so it buys light units only for now.
    fn heavy_every(self) -> u32 {
        0
    }

    /// Money kept in the treasury (thousandths) before buying: the easy bot
    /// sits on cash and grows slower.
    fn reserve(self) -> i64 {
        match self {
            Difficulty::Easy => 300_000,
            _ => 0,
        }
    }

    /// Whether the bot pulls back broken units and defends threatened cities.
    fn tactical(self) -> bool {
        self != Difficulty::Easy
    }
}

/// Radius (tiles) around a city in which units count as its defenders.
const CITY_ZONE: i32 = 8;

pub struct Bot {
    player: u8,
    difficulty: Difficulty,
    seq: u32,
    /// Units already sent somewhere: handle → target city.
    assigned: BTreeMap<UnitHandle, u16>,
    bought: u32,
}

impl Bot {
    pub fn new(player: u8, difficulty: Difficulty) -> Bot {
        Bot { player, difficulty, seq: 0, assigned: BTreeMap::new(), bought: 0 }
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

impl Controller for Bot {
    fn think(&mut self, world: &World) -> Vec<Command> {
        let tick = world.tick();
        let period = self.difficulty.period();
        if world.winner().is_some() || (tick + self.player as u32 * 3) % period != 0 {
            return Vec::new();
        }
        let me = self.player;
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
        let money = world.player(me).money;
        let mut budget = money - self.difficulty.reserve();
        for (ci, c) in cities.iter().enumerate() {
            if c.owner != me {
                continue;
            }
            let Some((used, limit)) = world.city_supply(ci) else { continue };
            let mut free = limit.saturating_sub(used);
            loop {
                let every = self.difficulty.heavy_every();
                let want_heavy = every != 0 && self.bought % every == every - 1;
                let kind = if want_heavy { UnitKind::Heavy } else { UnitKind::Light };
                let cost = world.rules().cost[kind as usize];
                let need = world.rules().supply[kind as usize];
                if budget < cost || free < need {
                    break;
                }
                budget -= cost;
                free -= need;
                self.bought += 1;
                out.push(self.cmd(tick, CommandKind::Buy { city: ci as u16, kind }));
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
        if self.difficulty.tactical() {
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
                out.push(self.cmd(tick, CommandKind::Move { units, path: vec![to], queue: false }));
            }
        }

        // ------------------------------------------------------- strategy
        // Idle units: no order, not retreating, not assigned.
        let idle: Vec<usize> = (0..u.len())
            .filter(|&i| {
                u.owner[i] == me
                    && u.order[i] == NO_ORDER
                    && !self.assigned.contains_key(&u.handle(i))
                    && !retreating.contains(&u.handle(i))
            })
            .collect();
        if idle.is_empty() {
            return out;
        }

        // Defend first: threatened own cities pull idle units.
        let mut targets: BTreeMap<u16, Vec<usize>> = BTreeMap::new();
        let mut taken = vec![false; idle.len()];
        if self.difficulty.tactical() {
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

        // Then expand/attack: each idle unit picks a city to take. Neutral
        // cities count as closer, capitals of enemies as a bit closer too.
        for (k, &i) in idle.iter().enumerate() {
            if taken[k] {
                continue;
            }
            let (x, y) = (u.pos_x[i], u.pos_y[i]);
            let best = cities
                .iter()
                .enumerate()
                .filter(|(_, c)| c.owner == 0 || world.team_of(c.owner) != my_team)
                .min_by_key(|(ci, c)| {
                    let d = dist2(x, y, c.center().x.0, c.center().y.0) >> 16;
                    let mut weight = if c.owner == 0 {
                        100
                    } else if c.capital_of != 0 {
                        170
                    } else {
                        250
                    };
                    // Strength 20 000 = one full-health light unit.
                    weight += weight * self.difficulty.defense_weight() * enemy_near[*ci] / (100 * 20_000);
                    (d * weight, *ci)
                });
            if let Some((ci, _)) = best {
                targets.entry(ci as u16).or_default().push(i);
            }
        }

        for (ci, group) in targets {
            let handles: Vec<UnitHandle> = group.iter().map(|&i| u.handle(i)).collect();
            for &h in &handles {
                self.assigned.insert(h, ci);
            }
            let to: FxPoint = cities[ci as usize].center();
            out.push(self.cmd(tick, CommandKind::Move { units: handles, path: vec![to], queue: false }));
        }
        out
    }
}
