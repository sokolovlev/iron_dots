//! The relay server (spec 6.2): lobbies, the lockstep tick clock, desync
//! detection and rejoin.
//!
//! [`Relay`] is the whole server logic as a network-free state machine:
//! feed it connections, messages and clock ticks, and it returns messages
//! to send. That makes it fully testable and lets the same code run as a
//! dedicated server ([`serve`]) or inside a client for LAN games
//! ([`spawn_local`]).
//!
//! The relay never simulates. Per running game, on every clock tick it
//! takes the commands received since the previous tick, stamps them with
//! the tick number and the sender's player id, stores the packet in the
//! game history and broadcasts it. History makes rejoin simple: a returning
//! client gets the scenario and every packet so far and re-simulates.

#![forbid(unsafe_code)]

pub mod netsim;
mod server;

pub use server::{LocalServer, serve, spawn_local};

use net::proto::{
    ClientMsg, LobbyInfo, LobbySettings, LobbyState, PROTOCOL_VERSION, ServerMsg, Slot, SlotKind, TickPacket,
};
use sim_core::balance::Balance;
use sim_core::command::Command;
use sim_core::scenario::ScenarioSpec;
use std::collections::BTreeMap;

/// A client this far (in ticks) behind the relay pauses the game.
pub const STALL_TICKS: u32 = 150;
/// Hashes are compared every this many ticks.
pub const HASH_INTERVAL: u32 = net::HASH_INTERVAL;
/// A game with nobody connected is dropped after this many clock ticks.
pub const ABANDON_TICKS: u32 = 30 * 120;
/// Longest accepted name.
const MAX_NAME: usize = 32;
/// Most commands accepted per message, and per player per tick.
const MAX_COMMANDS: usize = 64;

#[derive(Clone, Debug)]
pub struct Config {
    /// Ticks per second of real time.
    pub tick_rate: u32,
    pub max_lobbies: usize,
    /// Map sizes a lobby may ask for.
    pub max_map: u32,
}

impl Default for Config {
    fn default() -> Config {
        Config { tick_rate: 30, max_lobbies: 256, max_map: 2048 }
    }
}

/// Something for the transport to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Out {
    Send(u32, ServerMsg),
    /// Close the connection (after sending what is queued).
    Close(u32),
}

#[derive(Default)]
enum Where {
    #[default]
    Idle,
    Lobby(u32),
    Game(u32, u8),
}

struct Client {
    name: Option<String>,
    at: Where,
}

struct Lobby {
    state: LobbyState,
}

struct Player {
    id: u8,
    name: String,
    token: u64,
    client: Option<u32>,
    /// Ticks the client reports as simulated.
    progress: u32,
    /// Rejoined and replaying history: not waited for until caught up.
    catching_up: bool,
    seq: u32,
    /// Commands received this tick.
    queued: usize,
}

struct Game {
    scenario: Box<ScenarioSpec>,
    bots: Vec<(u8, u8)>,
    players: Vec<Player>,
    history: Vec<TickPacket>,
    pending: Vec<Command>,
    /// Hashes reported but not yet compared.
    hashes: BTreeMap<u32, Vec<(u8, u64)>>,
    /// Hashes everyone agreed on (one per 30 ticks; checks rejoiners).
    agreed: BTreeMap<u32, u64>,
    desynced: bool,
    stalled: bool,
    idle: u32,
}

impl Game {
    fn tick(&self) -> u32 {
        self.history.len() as u32
    }

    fn connected(&self) -> impl Iterator<Item = &Player> {
        self.players.iter().filter(|p| p.client.is_some())
    }

    fn broadcast(&self, m: &ServerMsg, out: &mut Vec<Out>) {
        for p in self.connected() {
            out.push(Out::Send(p.client.expect("connected"), m.clone()));
        }
    }

    fn lagging(&self) -> Vec<u8> {
        let t = self.tick();
        self.connected().filter(|p| !p.catching_up && p.progress + STALL_TICKS < t).map(|p| p.id).collect()
    }

    fn report_desync(&mut self, tick: u32, hashes: Vec<(u8, u64)>, out: &mut Vec<Out>) {
        if !self.desynced {
            self.desynced = true;
            self.broadcast(&ServerMsg::Desync { tick, hashes }, out);
        }
    }

    fn add_hash(&mut self, player: u8, tick: u32, hash: u64, out: &mut Vec<Out>) {
        if let Some(&agreed) = self.agreed.get(&tick) {
            // A rejoined client replaying history: check it on its own.
            if hash != agreed {
                self.report_desync(tick, vec![(0, agreed), (player, hash)], out);
            }
            return;
        }
        let hs = self.hashes.entry(tick).or_default();
        if !hs.iter().any(|h| h.0 == player) {
            hs.push((player, hash));
        }
        self.check_hashes(tick, out);
    }

    /// Compares the hashes of `tick` once every connected player sent one.
    fn check_hashes(&mut self, tick: u32, out: &mut Vec<Out>) {
        let Some(hs) = self.hashes.get(&tick) else { return };
        if hs.iter().any(|h| h.1 != hs[0].1) {
            let hs = self.hashes.remove(&tick).expect("present");
            self.report_desync(tick, hs, out);
            return;
        }
        let all = self.connected().all(|p| p.catching_up || hs.iter().any(|h| h.0 == p.id));
        if all {
            let h = hs[0].1;
            self.hashes.remove(&tick);
            self.agreed.insert(tick, h);
        }
    }
}

pub struct Relay {
    config: Config,
    balance: Balance,
    sim_version: u64,
    clients: BTreeMap<u32, Client>,
    lobbies: BTreeMap<u32, Lobby>,
    games: BTreeMap<u32, Game>,
    next_id: u32,
    /// Source of rejoin tokens (splitmix64).
    token_state: u64,
}

impl Relay {
    /// `entropy` seeds the rejoin tokens (use the clock in a real server).
    pub fn new(config: Config, entropy: u64) -> Relay {
        let balance = Balance::default_balance();
        let sim_version = sim_core::sim_version(&balance);
        Relay {
            config,
            balance,
            sim_version,
            clients: BTreeMap::new(),
            lobbies: BTreeMap::new(),
            games: BTreeMap::new(),
            next_id: 1,
            token_state: entropy,
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    fn id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    fn token(&mut self) -> u64 {
        self.token_state = self.token_state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.token_state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A new connection; returns its client id.
    pub fn connect(&mut self) -> u32 {
        let id = self.id();
        self.clients.insert(id, Client { name: None, at: Where::Idle });
        id
    }

    pub fn disconnect(&mut self, client: u32, out: &mut Vec<Out>) {
        self.leave(client, out);
        self.clients.remove(&client);
    }

    /// Number of running games (for logs and tests).
    pub fn games(&self) -> usize {
        self.games.len()
    }

    pub fn message(&mut self, client: u32, msg: ClientMsg, out: &mut Vec<Out>) {
        let Some(c) = self.clients.get(&client) else { return };
        let err = |out: &mut Vec<Out>, e: &str| out.push(Out::Send(client, ServerMsg::Error(e.to_string())));
        if c.name.is_none() {
            match msg {
                ClientMsg::Hello { name, protocol, sim_version } => {
                    if protocol != PROTOCOL_VERSION || sim_version != self.sim_version {
                        err(out, "version mismatch: update the game");
                        out.push(Out::Close(client));
                        return;
                    }
                    let name: String = name.chars().filter(|c| !c.is_control()).take(MAX_NAME).collect();
                    let name = if name.trim().is_empty() { format!("Player {client}") } else { name };
                    self.clients.get_mut(&client).expect("checked").name = Some(name);
                    out.push(Out::Send(client, ServerMsg::Welcome { client }));
                }
                _ => {
                    err(out, "say hello first");
                    out.push(Out::Close(client));
                }
            }
            return;
        }
        match msg {
            ClientMsg::Hello { .. } => err(out, "already greeted"),
            ClientMsg::Ping { nonce } => out.push(Out::Send(client, ServerMsg::Pong { nonce })),
            ClientMsg::ListLobbies => {
                let list = self.lobbies.values().map(|l| info(&l.state)).collect();
                out.push(Out::Send(client, ServerMsg::Lobbies(list)));
            }
            ClientMsg::CreateLobby { name, settings } => self.create_lobby(client, name, settings, out),
            ClientMsg::JoinLobby { lobby } => self.join_lobby(client, lobby, out),
            ClientMsg::LeaveLobby => self.leave(client, out),
            ClientMsg::SetReady(r) => self.edit_lobby(client, false, out, |l, client| {
                for s in &mut l.slots {
                    if let SlotKind::Human { client: c, ready, .. } = &mut s.kind {
                        if *c == client {
                            *ready = r;
                        }
                    }
                }
                Ok(())
            }),
            ClientMsg::SetSlot { slot, kind } => self.edit_lobby(client, true, out, |l, _| {
                let s = l.slots.get_mut(slot as usize).ok_or("no such slot")?;
                if matches!(s.kind, SlotKind::Human { .. }) {
                    return Err("slot is taken by a player");
                }
                match kind {
                    SlotKind::Open | SlotKind::Closed => s.kind = kind,
                    SlotKind::Bot { difficulty } if difficulty <= 2 => s.kind = kind,
                    _ => return Err("bad slot kind"),
                }
                Ok(())
            }),
            ClientMsg::SetTeam { slot, team } => self.edit_lobby(client, true, out, |l, _| {
                if !(1..=8).contains(&team) {
                    return Err("teams are 1..=8");
                }
                l.slots.get_mut(slot as usize).ok_or("no such slot")?.team = team;
                Ok(())
            }),
            ClientMsg::Start => self.start(client, out),
            ClientMsg::Commands { seq: _, kinds } => {
                let Where::Game(g, player) = self.clients[&client].at else { return };
                let game = self.games.get_mut(&g).expect("client's game exists");
                let tick = game.tick();
                let p = game.players.iter_mut().find(|p| p.id == player).expect("player");
                for kind in kinds.into_iter().take(MAX_COMMANDS.saturating_sub(p.queued)) {
                    p.queued += 1;
                    p.seq += 1;
                    game.pending.push(Command { tick, player, seq: p.seq, kind });
                }
            }
            ClientMsg::Hash { tick, hash } => {
                let Where::Game(g, player) = self.clients[&client].at else { return };
                let game = self.games.get_mut(&g).expect("client's game exists");
                if tick % HASH_INTERVAL != 0 || tick > game.tick() {
                    return;
                }
                game.add_hash(player, tick, hash, out);
            }
            ClientMsg::Progress { tick } => {
                let Where::Game(g, player) = self.clients[&client].at else { return };
                let game = self.games.get_mut(&g).expect("client's game exists");
                let now = game.tick();
                let p = game.players.iter_mut().find(|p| p.id == player).expect("player");
                p.progress = tick.min(now);
                if p.catching_up && p.progress + STALL_TICKS / 2 >= now {
                    p.catching_up = false;
                }
            }
            ClientMsg::Rejoin { game: g, token } => self.rejoin(client, g, token, out),
            ClientMsg::Chat(text) => {
                let from = self.clients[&client].name.clone().unwrap_or_default();
                let text: String = text.chars().filter(|c| !c.is_control()).take(200).collect();
                let m = ServerMsg::Chat { from, text };
                match self.clients[&client].at {
                    Where::Idle => {}
                    Where::Lobby(l) => {
                        for c in humans(&self.lobbies[&l].state) {
                            out.push(Out::Send(c, m.clone()));
                        }
                    }
                    Where::Game(g, _) => self.games[&g].broadcast(&m, out),
                }
            }
        }
    }

    fn create_lobby(&mut self, client: u32, name: String, s: LobbySettings, out: &mut Vec<Out>) {
        if !matches!(self.clients[&client].at, Where::Idle) {
            self.leave(client, out);
        }
        if self.lobbies.len() >= self.config.max_lobbies {
            out.push(Out::Send(client, ServerMsg::Error("server is full".into())));
            return;
        }
        let mut s = s;
        let mut teams: Vec<u8> = (1..=s.slots).collect();
        if let Some(m) = &s.map {
            match sim_core::mapfile::MapFile::decode(&m.bytes).and_then(|f| f.validate().map(|_| f)) {
                Ok(f) => {
                    // One slot per start; teams as the map suggests.
                    s.slots = f.starts.len() as u8;
                    (s.width, s.height) = (f.width, f.height);
                    teams = f.teams;
                }
                Err(e) => {
                    out.push(Out::Send(client, ServerMsg::Error(format!("bad map: {e}"))));
                    return;
                }
            }
        }
        if !(2..=8).contains(&s.slots)
            || ![s.width, s.height].iter().all(|v| (64..=self.config.max_map).contains(v) && v % 64 == 0)
        {
            out.push(Out::Send(client, ServerMsg::Error("bad lobby settings".into())));
            return;
        }
        let id = self.id();
        let me = self.clients[&client].name.clone().unwrap_or_default();
        let name: String = name.chars().filter(|c| !c.is_control()).take(MAX_NAME).collect();
        let mut slots: Vec<Slot> = teams.iter().map(|&team| Slot { kind: SlotKind::Open, team }).collect();
        slots[0].kind = SlotKind::Human { client, name: me, ready: false };
        let state = LobbyState { id, name, host: client, settings: s, slots };
        self.lobbies.insert(id, Lobby { state });
        self.clients.get_mut(&client).expect("client").at = Where::Lobby(id);
        self.publish_lobby(id, out);
    }

    fn join_lobby(&mut self, client: u32, lobby: u32, out: &mut Vec<Out>) {
        if !matches!(self.clients[&client].at, Where::Idle) {
            self.leave(client, out);
        }
        let name = self.clients[&client].name.clone().unwrap_or_default();
        let Some(l) = self.lobbies.get_mut(&lobby) else {
            out.push(Out::Send(client, ServerMsg::Error("no such lobby".into())));
            return;
        };
        let Some(slot) = l.state.slots.iter_mut().find(|s| s.kind == SlotKind::Open) else {
            out.push(Out::Send(client, ServerMsg::Error("lobby is full".into())));
            return;
        };
        slot.kind = SlotKind::Human { client, name, ready: false };
        self.clients.get_mut(&client).expect("client").at = Where::Lobby(lobby);
        self.publish_lobby(lobby, out);
    }

    fn edit_lobby(
        &mut self,
        client: u32,
        host_only: bool,
        out: &mut Vec<Out>,
        f: impl FnOnce(&mut LobbyState, u32) -> Result<(), &'static str>,
    ) {
        let Where::Lobby(id) = self.clients[&client].at else { return };
        let l = &mut self.lobbies.get_mut(&id).expect("client's lobby exists").state;
        if host_only && l.host != client {
            out.push(Out::Send(client, ServerMsg::Error("only the host can do that".into())));
            return;
        }
        match f(l, client) {
            Ok(()) => self.publish_lobby(id, out),
            Err(e) => out.push(Out::Send(client, ServerMsg::Error(e.into()))),
        }
    }

    fn publish_lobby(&self, id: u32, out: &mut Vec<Out>) {
        let l = &self.lobbies[&id].state;
        for c in humans(l) {
            out.push(Out::Send(c, ServerMsg::Lobby(l.clone())));
        }
    }

    /// Takes the client out of its lobby or game.
    fn leave(&mut self, client: u32, out: &mut Vec<Out>) {
        let Some(c) = self.clients.get_mut(&client) else { return };
        match std::mem::take(&mut c.at) {
            Where::Idle => {}
            Where::Lobby(id) => {
                let l = &mut self.lobbies.get_mut(&id).expect("client's lobby exists").state;
                for s in &mut l.slots {
                    if matches!(s.kind, SlotKind::Human { client: c, .. } if c == client) {
                        s.kind = SlotKind::Open;
                    }
                }
                let rest: Vec<u32> = humans(l).collect();
                if rest.is_empty() {
                    self.lobbies.remove(&id);
                } else {
                    if l.host == client {
                        l.host = rest[0];
                    }
                    self.publish_lobby(id, out);
                }
            }
            Where::Game(g, player) => {
                let game = self.games.get_mut(&g).expect("client's game exists");
                if let Some(p) = game.players.iter_mut().find(|p| p.id == player) {
                    p.client = None;
                }
                game.broadcast(&ServerMsg::PlayerLeft(player), out);
                // Waiting on someone who left would stall forever.
                let tick = game.tick();
                let pending: Vec<u32> = game.hashes.keys().copied().filter(|&t| t <= tick).collect();
                for t in pending {
                    game.check_hashes(t, out);
                }
            }
        }
    }

    fn start(&mut self, client: u32, out: &mut Vec<Out>) {
        let Where::Lobby(id) = self.clients[&client].at else { return };
        let l = &self.lobbies[&id].state;
        let fail = |out: &mut Vec<Out>, e: &str| out.push(Out::Send(client, ServerMsg::Error(e.into())));
        if l.host != client {
            return fail(out, "only the host can start");
        }
        let not_ready =
            l.slots.iter().any(|s| matches!(s.kind, SlotKind::Human { client: c, ready: false, .. } if c != client));
        if not_ready {
            return fail(out, "not everyone is ready");
        }
        let taken: Vec<&Slot> =
            l.slots.iter().filter(|s| matches!(s.kind, SlotKind::Human { .. } | SlotKind::Bot { .. })).collect();
        let teams = {
            let mut t: Vec<u8> = taken.iter().map(|s| s.team).collect();
            t.sort_unstable();
            t.dedup();
            t.len()
        };
        if taken.len() < 2 || teams < 2 {
            return fail(out, "need at least two teams");
        }
        let s = &l.settings;
        let scenario = match &s.map {
            // Slot k plays at start k; empty starts become neutral cities.
            Some(m) => {
                let slots: Vec<Option<u8>> = l
                    .slots
                    .iter()
                    .map(|sl| matches!(sl.kind, SlotKind::Human { .. } | SlotKind::Bot { .. }).then_some(sl.team))
                    .collect();
                ScenarioSpec::from_map_file(&m.bytes, &slots, &self.balance)
            }
            None => ScenarioSpec::standard_match(s.width, s.height, taken.len() as u8, s.seed, &self.balance),
        };
        let mut scenario = match scenario {
            Ok(sc) => match s.unit_budget {
                Some(n) => sc.limited_war(n.clamp(1, 100_000)),
                None => sc,
            },
            Err(e) => return fail(out, &e),
        };
        let mut players = Vec::new();
        let mut bots = Vec::new();
        for (k, slot) in taken.iter().enumerate() {
            let id = k as u8 + 1;
            scenario.players[k].team = slot.team;
            match &slot.kind {
                SlotKind::Human { client, name, .. } => players.push((id, *client, name.clone())),
                SlotKind::Bot { difficulty } => bots.push((id, *difficulty)),
                _ => unreachable!("filtered"),
            }
        }
        let game_id = self.id();
        let mut game = Game {
            scenario: Box::new(scenario),
            bots,
            players: Vec::new(),
            history: Vec::new(),
            pending: Vec::new(),
            hashes: BTreeMap::new(),
            agreed: BTreeMap::new(),
            desynced: false,
            stalled: false,
            idle: 0,
        };
        for (id, client, name) in players {
            let token = self.token();
            game.players.push(Player {
                id,
                name,
                token,
                client: Some(client),
                progress: 0,
                catching_up: false,
                seq: 0,
                queued: 0,
            });
            self.clients.get_mut(&client).expect("lobby member").at = Where::Game(game_id, id);
            out.push(Out::Send(client, self.game_start(&game, game_id, id, token)));
        }
        self.lobbies.remove(&id);
        self.games.insert(game_id, game);
    }

    fn game_start(&self, game: &Game, id: u32, you: u8, token: u64) -> ServerMsg {
        ServerMsg::GameStart {
            game: id,
            token,
            you,
            scenario: game.scenario.clone(),
            bots: game.bots.clone(),
            tick_rate: self.config.tick_rate,
        }
    }

    fn rejoin(&mut self, client: u32, g: u32, token: u64, out: &mut Vec<Out>) {
        if !matches!(self.clients[&client].at, Where::Idle) {
            self.leave(client, out);
        }
        let Some(game) = self.games.get(&g) else {
            out.push(Out::Send(client, ServerMsg::Error("the game is over".into())));
            return;
        };
        let Some(p) = game.players.iter().find(|p| p.token == token && p.client.is_none()) else {
            out.push(Out::Send(client, ServerMsg::Error("cannot rejoin".into())));
            return;
        };
        let id = p.id;
        out.push(Out::Send(client, self.game_start(game, g, id, token)));
        for pkt in &game.history {
            out.push(Out::Send(client, ServerMsg::Tick(pkt.clone())));
        }
        let game = self.games.get_mut(&g).expect("checked");
        game.broadcast(&ServerMsg::PlayerBack(id), out);
        let p = game.players.iter_mut().find(|p| p.id == id).expect("checked");
        p.client = Some(client);
        p.progress = 0;
        p.catching_up = true;
        if let Some(c) = self.clients.get_mut(&client) {
            c.at = Where::Game(g, id);
            if c.name.is_none() {
                c.name = Some(p.name.clone());
            }
        }
    }

    /// One clock tick: every running game emits its next tick packet,
    /// unless it waits for lagging players.
    pub fn tick(&mut self, out: &mut Vec<Out>) {
        let mut dead = Vec::new();
        for (&id, game) in &mut self.games {
            if game.connected().next().is_none() {
                game.idle += 1;
                if game.idle > ABANDON_TICKS {
                    dead.push(id);
                }
                // Nobody to send to; keep the clock going for rejoiners.
            } else {
                game.idle = 0;
            }
            let lagging = game.lagging();
            if !lagging.is_empty() {
                if !game.stalled {
                    game.stalled = true;
                    game.broadcast(&ServerMsg::Stall { waiting_for: lagging }, out);
                }
                continue;
            }
            if game.stalled {
                game.stalled = false;
                game.broadcast(&ServerMsg::Resume, out);
            }
            let tick = game.tick();
            let mut commands = std::mem::take(&mut game.pending);
            for c in &mut commands {
                c.tick = tick;
            }
            for p in &mut game.players {
                p.queued = 0;
            }
            let pkt = TickPacket { tick, commands };
            game.broadcast(&ServerMsg::Tick(pkt.clone()), out);
            game.history.push(pkt);
        }
        for id in dead {
            self.games.remove(&id);
        }
    }
}

fn humans(l: &LobbyState) -> impl Iterator<Item = u32> + '_ {
    l.slots.iter().filter_map(|s| match s.kind {
        SlotKind::Human { client, .. } => Some(client),
        _ => None,
    })
}

fn info(l: &LobbyState) -> LobbyInfo {
    LobbyInfo {
        id: l.id,
        name: l.name.clone(),
        humans: humans(l).count() as u8,
        open: l.slots.iter().filter(|s| s.kind == SlotKind::Open).count() as u8,
        width: l.settings.width,
        height: l.settings.height,
        map: l.settings.map.as_ref().map(|m| m.name.clone()),
    }
}

#[cfg(test)]
mod tests;
