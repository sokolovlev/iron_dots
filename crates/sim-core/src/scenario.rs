//! Reproducible match setups and a simple scripted player used by tests,
//! golden replays and benchmarks. The real AI lives in the `ai` crate (stage 2).

use crate::balance::Balance;
use crate::command::{Command, CommandKind};
use crate::fx::{Fx, FxPoint, ONE};
use crate::map::{Map, MapSource, Terrain};
use crate::unit::UnitKind;
use crate::world::{MAX_PLAYERS, PlayerSetup, World};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spawn {
    pub owner: u8,
    pub kind: UnitKind,
    pub at: FxPoint,
}

/// Everything needed to rebuild the initial world bit for bit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScenarioSpec {
    pub map: MapSource,
    pub seed: u64,
    pub players: Vec<PlayerSetup>,
    pub spawns: Vec<Spawn>,
}

impl ScenarioSpec {
    pub fn build(&self, balance: &Balance) -> Result<World, String> {
        let map = self.map.build()?;
        let mut w = World::new(map, balance, self.seed, &self.players)?;
        for s in &self.spawns {
            w.spawn(s.owner, s.kind, s.at)?;
        }
        Ok(w)
    }

    /// Generated map with `players` armies (each `units_per_player` strong,
    /// every 4th unit heavy) placed around the centre, free-for-all teams.
    pub fn battle(map_size: u32, units_per_player: u32, players: u8, seed: u64) -> Result<ScenarioSpec, String> {
        if !(1..=MAX_PLAYERS as u8).contains(&players) {
            return Err(format!("players must be in 1..={MAX_PLAYERS}"));
        }
        let source = MapSource::Generated { width: map_size, height: map_size, seed };
        let map = source.build()?;
        // Directions on a ring, in eighths of the half-size (no trigonometry).
        const DIRS: [(i32, i32); 8] = [(-6, 0), (6, 0), (0, -6), (0, 6), (-4, -4), (4, 4), (4, -4), (-4, 4)];
        let half = map_size as i32 / 2;
        let mut spawns = Vec::new();
        for p in 0..players {
            let (dx, dy) = DIRS[p as usize];
            let cx = half + dx * half / 8;
            let cy = half + dy * half / 8;
            for (k, at) in army_positions(&map, cx, cy, units_per_player).into_iter().enumerate() {
                let kind = if k % 4 == 3 { UnitKind::Heavy } else { UnitKind::Light };
                spawns.push(Spawn { owner: p + 1, kind, at });
            }
        }
        Ok(ScenarioSpec {
            map: source,
            seed,
            players: (0..players).map(|p| PlayerSetup { team: p + 1 }).collect(),
            spawns,
        })
    }
}

/// `count` positions on dry land around tile `(cx, cy)`, spaced 0.75 tile,
/// filled in growing square rings so armies are compact.
fn army_positions(map: &Map, cx: i32, cy: i32, count: u32) -> Vec<FxPoint> {
    let step = ONE * 3 / 4;
    let mut out = Vec::with_capacity(count as usize);
    let max_ring = (map.width().max(map.height()) as i32) * 2;
    let land = |x: i32, y: i32| -> bool {
        let (tx, ty) = (x >> 16, y >> 16);
        tx >= 0
            && ty >= 0
            && (tx as u32) < map.width()
            && (ty as u32) < map.height()
            && !matches!(map.terrain(tx as u32, ty as u32), Terrain::Mountain | Terrain::Water)
    };
    let (ox, oy) = (cx * ONE + ONE / 2, cy * ONE + ONE / 2);
    let mut ring = 0;
    while (out.len() as u32) < count && ring < max_ring {
        // Walk the square ring of radius `ring` in a fixed order.
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

    /// Commands for the current tick of `world` (empty most ticks).
    pub fn think(&mut self, world: &World) -> Vec<Command> {
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
