//! Reproducible match setups and a simple scripted player used by tests,
//! golden replays and benchmarks. The real AI lives in the `ai` crate.

use crate::balance::Balance;
use crate::command::{Command, CommandKind};
use crate::fx::{Fx, FxPoint, ONE};
use crate::map::{Map, MapSource, Terrain};
use crate::rng::{Purpose, rand};
use crate::unit::UnitKind;
use crate::world::{MAX_PLAYERS, PlayerSetup, World};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spawn {
    pub owner: u8,
    pub kind: UnitKind,
    pub at: FxPoint,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CitySpawn {
    pub x: u32,
    pub y: u32,
    /// 0 = neutral.
    pub owner: u8,
    pub capital: bool,
    /// Starting territory radius around the city, tiles (0 = just the city).
    pub claim: u32,
}

/// Everything needed to rebuild the initial world bit for bit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScenarioSpec {
    pub map: MapSource,
    pub seed: u64,
    pub players: Vec<PlayerSetup>,
    pub cities: Vec<CitySpawn>,
    pub spawns: Vec<Spawn>,
    /// Sandbox rules (spec 2.7): unbounded supply, no upkeep.
    pub sandbox: bool,
}

/// Directions of starting positions on a ring, in eighths of the half-size
/// (no trigonometry): W, E, N, S, NW, SE, NE, SW.
const DIRS: [(i32, i32); 8] = [(-6, 0), (6, 0), (0, -6), (0, 6), (-4, -4), (4, 4), (4, -4), (-4, 4)];

fn is_land(map: &Map, x: i32, y: i32) -> bool {
    x >= 0
        && y >= 0
        && (x as u32) < map.width()
        && (y as u32) < map.height()
        && !matches!(map.terrain(x as u32, y as u32), Terrain::Mountain | Terrain::Water)
}

/// Nearest land tile to `(x, y)` in growing square rings (fixed scan order).
fn nearest_land(map: &Map, x: i32, y: i32) -> Option<(i32, i32)> {
    let max = map.width().max(map.height()) as i32;
    for r in 0..max {
        for dy in -r..=r {
            for dx in -r..=r {
                if (dx.abs() == r || dy.abs() == r) && is_land(map, x + dx, y + dy) {
                    return Some((x + dx, y + dy));
                }
            }
        }
    }
    None
}

impl ScenarioSpec {
    pub fn build(&self, balance: &Balance) -> Result<World, String> {
        let map = self.map.build()?;
        let mut w = World::new(map, balance, self.seed, &self.players)?;
        w.set_sandbox(self.sandbox);
        for c in &self.cities {
            w.add_city(c.x, c.y, c.owner, c.capital)?;
            if c.owner != 0 && c.claim > 0 {
                w.claim(c.owner, c.x, c.y, c.claim);
            }
        }
        for s in &self.spawns {
            w.spawn(s.owner, s.kind, s.at)?;
        }
        Ok(w)
    }

    /// A normal match (spec 2.8 "match start"): every player gets a capital
    /// with some land and `start_light_units` light units; neutral cities are
    /// scattered over the map. Free-for-all teams unless `teams` is given.
    pub fn standard_match(map_size: u32, players: u8, seed: u64, balance: &Balance) -> Result<ScenarioSpec, String> {
        if !(2..=MAX_PLAYERS as u8).contains(&players) {
            return Err(format!("players must be in 2..={MAX_PLAYERS}"));
        }
        let source = MapSource::Generated { width: map_size, height: map_size, seed };
        let map = source.build()?;
        let half = map_size as i32 / 2;
        let mut cities: Vec<CitySpawn> = Vec::new();
        let mut spawns = Vec::new();
        for p in 0..players {
            let (dx, dy) = DIRS[p as usize];
            let (x, y) = nearest_land(&map, half + dx * half / 8, half + dy * half / 8).ok_or("map has no land")?;
            cities.push(CitySpawn { x: x as u32, y: y as u32, owner: p + 1, capital: true, claim: 6 });
            let start = balance.match_rules.start_light_units;
            for at in army_positions(&map, x + 2, y, start) {
                spawns.push(Spawn { owner: p + 1, kind: UnitKind::Light, at });
            }
        }
        // Neutral cities: rejection sampling with a minimum spacing.
        // Roughly 20 cities on a 256² map; never fewer than 3 per player.
        let spacing = (map_size as i64 / 8).clamp(16, 48);
        let target = ((map_size as i64 / spacing).pow(2) / 3).max(players as i64 * 3) as usize;
        let mut attempt = 0u32;
        while cities.len() < target + players as usize && attempt < 20_000 {
            let r = rand(seed, attempt, 0, Purpose::Scenario);
            attempt += 1;
            let margin = 4;
            let span = map_size as u64 - 2 * margin;
            let x = (margin + (r & 0xFFFF_FFFF) % span) as i32;
            let y = (margin + (r >> 32) % span) as i32;
            if !is_land(&map, x, y) {
                continue;
            }
            let far = cities.iter().all(|c| {
                let (ddx, ddy) = (c.x as i64 - x as i64, c.y as i64 - y as i64);
                ddx * ddx + ddy * ddy >= spacing * spacing
            });
            if far {
                cities.push(CitySpawn { x: x as u32, y: y as u32, owner: 0, capital: false, claim: 0 });
            }
        }
        Ok(ScenarioSpec {
            map: source,
            seed,
            players: (0..players).map(|p| PlayerSetup { team: p + 1 }).collect(),
            cities,
            spawns,
            sandbox: false,
        })
    }

    /// Sandbox battle for benchmarks: generated map with `players` armies
    /// (each `units_per_player` strong, every 4th unit heavy) around the
    /// centre, each with a capital. Free-for-all teams.
    pub fn battle(map_size: u32, units_per_player: u32, players: u8, seed: u64) -> Result<ScenarioSpec, String> {
        if !(1..=MAX_PLAYERS as u8).contains(&players) {
            return Err(format!("players must be in 1..={MAX_PLAYERS}"));
        }
        let source = MapSource::Generated { width: map_size, height: map_size, seed };
        let map = source.build()?;
        let half = map_size as i32 / 2;
        let mut spawns = Vec::new();
        let mut cities = Vec::new();
        for p in 0..players {
            let (dx, dy) = DIRS[p as usize];
            let (cx, cy) = nearest_land(&map, half + dx * half / 8, half + dy * half / 8).ok_or("map has no land")?;
            cities.push(CitySpawn { x: cx as u32, y: cy as u32, owner: p + 1, capital: true, claim: 4 });
            for (k, at) in army_positions(&map, cx, cy + 2, units_per_player).into_iter().enumerate() {
                let kind = if k % 4 == 3 { UnitKind::Heavy } else { UnitKind::Light };
                spawns.push(Spawn { owner: p + 1, kind, at });
            }
        }
        Ok(ScenarioSpec {
            map: source,
            seed,
            players: (0..players).map(|p| PlayerSetup { team: p + 1 }).collect(),
            cities,
            spawns,
            sandbox: true,
        })
    }
}

impl ScenarioSpec {
    /// "Everyone on one front" (spec 7): two armies in long facing lines
    /// across the middle of a plain map, in contact from the first tick.
    /// Worst case for combat; sandbox rules.
    pub fn front(map_size: u32, units_per_player: u32, seed: u64) -> Result<ScenarioSpec, String> {
        let map = Map::new(map_size, map_size, Terrain::Plain)?;
        let mid = map_size as i32 / 2;
        // Rows of units 0.75 tiles apart, as long as the map allows.
        let per_row = ((map_size as i32 - 16) * 4 / 3).max(1) as u32;
        let rows = units_per_player.div_ceil(per_row);
        let mut spawns = Vec::new();
        for p in 0..2u8 {
            for k in 0..units_per_player {
                let (row, col) = ((k / per_row) as i32, (k % per_row) as i32);
                let x = 8 * ONE + col * ONE * 3 / 4;
                // Player 1 above the line, player 2 below; first rows touch.
                let dy = (row * ONE * 3 / 4) + ONE / 2;
                let y = if p == 0 { mid * ONE - dy } else { mid * ONE + dy };
                let kind = if k % 4 == 3 { UnitKind::Heavy } else { UnitKind::Light };
                spawns.push(Spawn { owner: p + 1, kind, at: FxPoint::new(Fx(x), Fx(y)) });
            }
        }
        let _ = rows;
        let cities = vec![
            CitySpawn { x: map_size / 2, y: 4, owner: 1, capital: true, claim: 0 },
            CitySpawn { x: map_size / 2, y: map_size - 5, owner: 2, capital: true, claim: 0 },
        ];
        Ok(ScenarioSpec {
            map: MapSource::Tiles(map),
            seed,
            players: vec![PlayerSetup { team: 1 }, PlayerSetup { team: 2 }],
            cities,
            spawns,
            sandbox: true,
        })
    }
}

/// `count` positions on dry land around tile `(cx, cy)`, spaced 0.75 tile,
/// filled in growing square rings so armies are compact.
fn army_positions(map: &Map, cx: i32, cy: i32, count: u32) -> Vec<FxPoint> {
    let step = ONE * 3 / 4;
    let mut out = Vec::with_capacity(count as usize);
    let max_ring = (map.width().max(map.height()) as i32) * 2;
    let land = |x: i32, y: i32| {
        is_land(map, x >> 16, y >> 16) && map.terrain((x >> 16) as u32, (y >> 16) as u32) != Terrain::City
    };
    let (ox, oy) = (cx * ONE + ONE / 2, cy * ONE + ONE / 2);
    let mut ring = 0;
    while (out.len() as u32) < count && ring < max_ring {
        for gy in -ring..=ring {
            for gx in -ring..=ring {
                if gx.abs() != ring && gy.abs() != ring {
                    continue;
                }
                let (x, y) = (ox + gx * step, oy + gy * step);
                if (out.len() as u32) < count && land(x, y) {
                    out.push(FxPoint::new(Fx(x), Fx(y)));
                }
            }
        }
        ring += 1;
    }
    out
}

/// A deterministic scripted player: every `period` ticks it splits its
/// units into groups of `group` and sends each group at the enemy unit
/// nearest to the group's centre. Good enough to generate fights.
#[derive(Clone, Debug)]
pub struct ScriptedPlayer {
    pub player: u8,
    pub period: u32,
    pub group: usize,
    seq: u32,
}

impl ScriptedPlayer {
    pub fn new(player: u8) -> Self {
        Self { player, period: 90, group: 64, seq: 0 }
    }
}

/// Anything that issues commands from the world state: a scripted player,
/// the AI, a recorded stream.
pub trait Controller {
    /// Commands for the current tick of `world` (usually empty).
    fn think(&mut self, world: &World) -> Vec<Command>;
}

impl Controller for ScriptedPlayer {
    fn think(&mut self, world: &World) -> Vec<Command> {
        let tick = world.tick();
        // Players think on different ticks to spread the load.
        if (tick + self.player as u32 * 7) % self.period != 0 {
            return Vec::new();
        }
        let u = world.units();
        let me = self.player;
        let mine: Vec<usize> = (0..u.len()).filter(|&i| u.owner[i] == me).collect();
        let enemies: Vec<usize> = (0..u.len()).filter(|&i| u.owner[i] != me).collect();
        if mine.is_empty() || enemies.is_empty() {
            return Vec::new();
        }
        // Sample at most 256 enemies to keep this cheap.
        let stride = enemies.len().div_ceil(256);
        let mut out = Vec::new();
        for g in mine.chunks(self.group) {
            let n = g.len() as i64;
            let cx = (g.iter().map(|&i| u.pos_x[i] as i64).sum::<i64>() / n) as i32;
            let cy = (g.iter().map(|&i| u.pos_y[i] as i64).sum::<i64>() / n) as i32;
            let target = enemies
                .iter()
                .step_by(stride)
                .min_by_key(|&&j| (crate::fx::dist2(cx, cy, u.pos_x[j], u.pos_y[j]), u.slot[j]))
                .copied()
                .expect("enemies is not empty");
            let path = vec![FxPoint::new(Fx(u.pos_x[target]), Fx(u.pos_y[target]))];
            let units = g.iter().map(|&i| u.handle(i)).collect();
            out.push(Command {
                tick,
                player: me,
                seq: self.seq,
                kind: CommandKind::Move { units, path, queue: false },
            });
            self.seq += 1;
        }
        out
    }
}
