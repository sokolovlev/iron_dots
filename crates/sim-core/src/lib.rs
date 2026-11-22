//! # sim-core
//!
//! Deterministic simulation of Iron Dots: no IO, no floats, no hash-map
//! iteration, no wall clock. The same commands on the same initial state give
//! a bit-identical world on every OS, CPU and thread count (spec 3, 5.2).
//!
//! See `ARCHITECTURE.md` in the repository root for the determinism rules.

#![forbid(unsafe_code)]
#![deny(clippy::float_arithmetic)]

pub mod balance;
pub mod command;
pub mod flow;
pub mod formation;
pub mod fx;
pub mod grid;
pub mod map;
pub mod mapfile;
pub mod order;
pub mod pathfind;
pub mod replay;
pub mod rng;
pub mod scenario;
pub mod territory;
pub mod unit;
pub mod world;

pub use balance::{Balance, Rules};
pub use command::{Command, CommandKind};
pub use fx::{Fx, FxPoint, Milli};
pub use map::{Map, MapSource, Terrain};
pub use replay::Replay;
pub use unit::{UnitHandle, UnitKind};
pub use world::{City, Phase, PlayerSetup, PlayerState, World};

/// Version of the simulation code. Bump on any change that can alter the
/// outcome of a replay; CI fails if a golden replay hash changes without it.
pub const SIM_CODE_VERSION: u32 = 13;

/// Full simulation version: code version plus balance content (spec 6.5).
/// Only clients with equal `sim_version` can play together.
pub fn sim_version(balance: &Balance) -> u64 {
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    h.update(&SIM_CODE_VERSION.to_le_bytes());
    h.update(&balance.content_hash().to_le_bytes());
    h.digest()
}
