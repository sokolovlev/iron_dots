//! Protocol messages. Both sides must use the same [`PROTOCOL_VERSION`]
//! and the same simulation version (`sim_core::sim_version`).

use serde::{Deserialize, Serialize};
use sim_core::command::{Command, CommandKind};
use sim_core::scenario::ScenarioSpec;

/// Bump on any change to the messages below (spec 6.5, protocol_version).
pub const PROTOCOL_VERSION: u32 = 5;

/// Commands of one tick, as decided by the relay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickPacket {
    pub tick: u32,
    pub commands: Vec<Command>,
}

/// What a lobby slot holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlotKind {
    Open,
    Closed,
    Human {
        client: u32,
        name: String,
        ready: bool,
    },
    /// A bot run deterministically on every client: 0 easy, 1 normal, 2 hard.
    Bot {
        difficulty: u8,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slot {
    pub kind: SlotKind,
    /// Team; slots with the same team are allies.
    pub team: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LobbySettings {
    /// Map size in tiles (multiples of 64).
    pub width: u32,
    pub height: u32,
    pub seed: u64,
    /// Number of slots (2..=8); one player per slot. With a map file,
    /// one slot per start position.
    pub slots: u8,
    /// A `.wmap` map (built-in or the host's own); `None` = generated from
    /// `width`, `height` and `seed`.
    pub map: Option<LobbyMap>,
    /// Limited war: units each team may create (`None` = war of attrition).
    pub unit_budget: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LobbyMap {
    pub name: String,
    /// The encoded map file, sent to everyone with the lobby.
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LobbyState {
    pub id: u32,
    pub name: String,
    /// Client id of the host (may change slots and start).
    pub host: u32,
    pub settings: LobbySettings,
    pub slots: Vec<Slot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LobbyInfo {
    pub id: u32,
    pub name: String,
    pub humans: u8,
    pub open: u8,
    pub width: u32,
    pub height: u32,
    /// Map name, if the lobby plays on a map file.
    pub map: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMsg {
    Hello {
        name: String,
        protocol: u32,
        sim_version: u64,
    },
    ListLobbies,
    CreateLobby {
        name: String,
        settings: LobbySettings,
    },
    JoinLobby {
        lobby: u32,
    },
    LeaveLobby,
    SetReady(bool),
    /// Host only: change what a slot holds (`Open`, `Closed` or `Bot`).
    SetSlot {
        slot: u8,
        kind: SlotKind,
    },
    /// Host only.
    SetTeam {
        slot: u8,
        team: u8,
    },
    /// Host only: start once every human is ready.
    Start,
    /// Commands for the relay to schedule; it adds tick and player.
    Commands {
        seq: u32,
        kinds: Vec<CommandKind>,
    },
    /// State hash after `tick` ticks (sent every 30 ticks).
    Hash {
        tick: u32,
        hash: u64,
    },
    /// The client has simulated up to (not including) `tick`.
    Progress {
        tick: u32,
    },
    /// Back into a running game after a disconnect.
    Rejoin {
        game: u32,
        token: u64,
    },
    Ping {
        nonce: u64,
    },
    Chat(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMsg {
    Welcome {
        client: u32,
    },
    Error(String),
    Lobbies(Vec<LobbyInfo>),
    Lobby(LobbyState),
    /// The match begins (or, after `Rejoin`, continues). Tick packets follow,
    /// starting at tick 0; on rejoin the past ones come at once.
    GameStart {
        game: u32,
        /// Secret for `Rejoin`.
        token: u64,
        /// Player id of this client (1..=8).
        you: u8,
        scenario: Box<ScenarioSpec>,
        /// `(player, difficulty)` of every bot.
        bots: Vec<(u8, u8)>,
        tick_rate: u32,
    },
    Tick(TickPacket),
    /// Hashes disagreed at `tick`: `(player, hash)` of every reporter.
    Desync {
        tick: u32,
        hashes: Vec<(u8, u64)>,
    },
    /// The relay paused: these players are too far behind.
    Stall {
        waiting_for: Vec<u8>,
    },
    Resume,
    PlayerLeft(u8),
    PlayerBack(u8),
    Pong {
        nonce: u64,
    },
    Chat {
        from: String,
        text: String,
    },
}

pub fn encode<T: Serialize>(m: &T) -> Vec<u8> {
    bitcode::serialize(m).expect("messages serialize")
}

pub fn decode<'a, T: Deserialize<'a>>(b: &'a [u8]) -> Result<T, String> {
    bitcode::deserialize(b).map_err(|e| format!("bad message: {e}"))
}
