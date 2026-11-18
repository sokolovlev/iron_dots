//! Commands are the only way to change the world — for players, bots and
//! replays alike. A command is applied at the start of the tick it names.

use crate::fx::FxPoint;
use crate::unit::UnitHandle;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Command {
    /// Tick at which the command is applied.
    pub tick: u32,
    /// Issuing player (1..=8).
    pub player: u8,
    /// Per-player sequence number; commands of a tick are applied sorted by
    /// `(player, seq)`.
    pub seq: u32,
    pub kind: CommandKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandKind {
    /// Walk along `path` (one point = go to that point; a drawn line is
    /// simplified to ≤ 32 points by the client). The group keeps its
    /// formation: each unit follows the path shifted by its offset from the
    /// group centre. With `queue` the path is appended after current orders
    /// (Shift); queued paths keep the unit's existing offset.
    Move { units: Vec<UnitHandle>, path: Vec<FxPoint>, queue: bool },
    /// S: stop now and drop every order; the units hold position (defend).
    Stop { units: Vec<UnitHandle> },
    /// C: drop queued orders, keep walking the current one.
    ClearQueue { units: Vec<UnitHandle> },
    /// Q / W: buy a unit in one of your cities (index into `World::cities`).
    /// Needs the money and free supply in the city's pocket; the unit
    /// appears in the city at the end of the tick.
    Buy { city: u16, kind: crate::unit::UnitKind },
}

impl Command {
    pub fn sort_key(&self) -> (u32, u8, u32) {
        (self.tick, self.player, self.seq)
    }
}
