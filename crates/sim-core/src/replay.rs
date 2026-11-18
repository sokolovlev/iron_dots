//! Replays: initial scenario + command stream + periodic state hashes.
//! One hour of play stays far below 1 MB because only commands are stored.

use crate::balance::Balance;
use crate::command::Command;
use crate::scenario::{ScenarioSpec, ScriptedPlayer};
use crate::world::World;
use serde::{Deserialize, Serialize};

pub const MAGIC: &[u8; 4] = b"IDRP";
pub const FORMAT_VERSION: u32 = 1;
/// A state hash is recorded every this many ticks (spec 4.3, phase 11).
pub const HASH_INTERVAL: u32 = 30;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Replay {
    pub sim_code_version: u32,
    pub balance_hash: u64,
    pub scenario: ScenarioSpec,
    /// Number of ticks the match lasted.
    pub ticks: u32,
    /// Commands sorted by tick.
    pub commands: Vec<Command>,
    /// `(tick, state hash after that many ticks)`.
    pub checkpoints: Vec<(u32, u64)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerifyError {
    WrongVersion { replay: u32, ours: u32 },
    WrongBalance { replay: u64, ours: u64 },
    Build(String),
    Desync { tick: u32, expected: u64, got: u64 },
}

impl core::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            VerifyError::WrongVersion { replay, ours } => {
                write!(f, "replay needs sim code version {replay}, this build has {ours}")
            }
            VerifyError::WrongBalance { replay, ours } => {
                write!(f, "replay balance hash {replay:016x} differs from ours {ours:016x}")
            }
            VerifyError::Build(e) => write!(f, "cannot build scenario: {e}"),
            VerifyError::Desync { tick, expected, got } => {
                write!(f, "desync at tick {tick}: expected {expected:016x}, got {got:016x}")
            }
        }
    }
}

impl Replay {
    /// Plays `scenario` for `ticks` with scripted players on every slot and
    /// records the commands and hashes.
    pub fn record_scripted(scenario: &ScenarioSpec, balance: &Balance, ticks: u32) -> Result<Replay, String> {
        let mut world = scenario.build(balance)?;
        let mut bots: Vec<ScriptedPlayer> = (1..=world.player_count()).map(ScriptedPlayer::new).collect();
        let mut commands = Vec::new();
        let mut checkpoints = Vec::new();
        for _ in 0..ticks {
            let start = commands.len();
            for b in &mut bots {
                commands.extend(b.think(&world));
            }
            world.step(&commands[start..]);
            if world.tick() % HASH_INTERVAL == 0 {
                checkpoints.push((world.tick(), world.state_hash()));
            }
        }
        if world.tick() % HASH_INTERVAL != 0 {
            checkpoints.push((world.tick(), world.state_hash()));
        }
        Ok(Replay {
            sim_code_version: crate::SIM_CODE_VERSION,
            balance_hash: balance.content_hash(),
            scenario: scenario.clone(),
            ticks,
            commands,
            checkpoints,
        })
    }

    /// Re-simulates the replay and checks every checkpoint. Returns the final
    /// world on success.
    pub fn verify(&self, balance: &Balance) -> Result<World, VerifyError> {
        if self.sim_code_version != crate::SIM_CODE_VERSION {
            return Err(VerifyError::WrongVersion { replay: self.sim_code_version, ours: crate::SIM_CODE_VERSION });
        }
        if self.balance_hash != balance.content_hash() {
            return Err(VerifyError::WrongBalance { replay: self.balance_hash, ours: balance.content_hash() });
        }
        let mut world = self.scenario.build(balance).map_err(VerifyError::Build)?;
        let mut cursor = 0;
        let mut checks = self.checkpoints.iter().peekable();
        for _ in 0..self.ticks {
            let start = cursor;
            while cursor < self.commands.len() && self.commands[cursor].tick == world.tick() {
                cursor += 1;
            }
            world.step(&self.commands[start..cursor]);
            while let Some(&&(t, expected)) = checks.peek() {
                if t > world.tick() {
                    break;
                }
                checks.next();
                if t == world.tick() {
                    let got = world.state_hash();
                    if got != expected {
                        return Err(VerifyError::Desync { tick: t, expected, got });
                    }
                }
            }
        }
        Ok(world)
    }

    pub fn final_hash(&self) -> Option<u64> {
        self.checkpoints.last().map(|&(_, h)| h)
    }

    pub fn encode(&self) -> Vec<u8> {
        let body = bitcode::serialize(self).expect("replay serializes");
        let mut out = Vec::with_capacity(8 + body.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Replay, String> {
        if bytes.len() < 8 || &bytes[..4] != MAGIC {
            return Err("not an Iron Dots replay".into());
        }
        let ver = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        if ver != FORMAT_VERSION {
            return Err(format!("replay format {ver} is not supported (expected {FORMAT_VERSION})"));
        }
        bitcode::deserialize(&bytes[8..]).map_err(|e| format!("corrupt replay: {e}"))
    }
}
