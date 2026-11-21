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
    /// `formation` arranges the group around the path (Keep = as they stand).
    Move { units: Vec<UnitHandle>, path: Vec<FxPoint>, queue: bool, formation: Formation },
    /// S: stop now and drop every order; the units hold position (defend).
    Stop { units: Vec<UnitHandle> },
    /// C: drop queued orders, keep walking the current one.
    ClearQueue { units: Vec<UnitHandle> },
    /// Q / W: buy a unit in one of your cities (index into `World::cities`).
    /// Needs the money and free supply in the city's pocket; the unit
    /// appears in the city at the end of the tick.
    Buy { city: u16, kind: crate::unit::UnitKind },
}

/// How a group lines up for a move (spec 2.9; like the formation buttons
/// of classic RTS games). Facing is from the group's centre to the first
/// path point.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Formation {
    /// Keep the current spacing (compressed if it is very spread out).
    #[default]
    Keep,
    /// A wide front a few ranks deep (about 4:1).
    Line,
    /// A line with gaps, odd ranks shifted half a step: loose and wide.
    Staggered,
    /// A square block.
    Box,
    /// A narrow column for marching through gaps.
    Column,
    /// Two blocks on the wings with a gap in the centre.
    Flank,
}

impl Formation {
    pub const ALL: [Formation; 6] =
        [Formation::Keep, Formation::Line, Formation::Staggered, Formation::Box, Formation::Column, Formation::Flank];
}

impl Command {
    pub fn sort_key(&self) -> (u32, u8, u32) {
        (self.tick, self.player, self.seq)
    }
}
