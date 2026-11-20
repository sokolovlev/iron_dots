//! Network matches with headless clients, for tests and `headless netsim`.
//!
//! A relay runs in-process, optionally behind a [`netem`](net::netem) proxy
//! that loses and delays packets. Every client is a real QUIC client on its
//! own thread with its own world; a bot plays each client's player and its
//! commands travel through the relay like a human's. At the end every
//! client's state hashes are compared.

use crate::Config;
use net::client::{NetClient, NetEvent};
use net::netem::{self, Conditions};
use net::proto::{ClientMsg, LobbySettings, PROTOCOL_VERSION, ServerMsg};
use net::session::Session;
use sim_core::balance::Balance;
use sim_core::scenario::{Controller, ScenarioSpec};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

/// Makes a bot for `(player, difficulty 0..=2)`.
pub type BotFactory = fn(u8, u8) -> Box<dyn Controller + Send>;

#[derive(Clone, Debug)]
pub struct Options {
    pub map_size: u32,
    pub seed: u64,
    /// Team of each human client (6 entries for 3v3).
    pub teams: Vec<u8>,
    /// Extra bot slots: `(team, difficulty)`, run on every client.
    pub bots: Vec<(u8, u8)>,
    pub ticks: u32,
    pub tick_rate: u32,
    pub network: Option<Conditions>,
    /// One client drops out and rejoins during the match.
    pub reconnect: bool,
}

impl Options {
    pub fn three_vs_three(seed: u64, ticks: u32) -> Options {
        Options {
            map_size: 256,
            seed,
            teams: vec![1, 1, 1, 2, 2, 2],
            bots: Vec::new(),
            ticks,
            tick_rate: 30,
            network: Some(Conditions::bad(seed)),
            reconnect: true,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Outcome {
    /// Ticks every client reached.
    pub ticks: u32,
    /// Compared checkpoints (tick, hash), common to all clients.
    pub checkpoints: usize,
    /// First mismatch, if any: (tick, hashes of the clients).
    pub desync: Option<(u32, Vec<u64>)>,
    /// The relay reported a desync.
    pub relay_desync: bool,
    pub rejoined: bool,
    pub stalls: u32,
    pub winner: Option<u8>,
    pub seconds: f64,
}

fn wait(c: &NetClient, timeout: Duration, mut f: impl FnMut(&ServerMsg) -> bool) -> Result<ServerMsg, String> {
    let end = Instant::now() + timeout;
    loop {
        let left = end.saturating_duration_since(Instant::now());
        match c.recv_timeout(left) {
            Some(NetEvent::Msg(m)) if f(&m) => return Ok(m),
            Some(NetEvent::Disconnected(e)) => return Err(e),
            Some(_) => {}
            None => return Err("timed out".into()),
        }
    }
}

fn hello(addr: SocketAddr, name: &str) -> Result<NetClient, String> {
    let c = NetClient::connect(addr);
    let sim_version = sim_core::sim_version(&Balance::default_balance());
    c.send(ClientMsg::Hello { name: name.into(), protocol: PROTOCOL_VERSION, sim_version });
    wait(&c, Duration::from_secs(20), |m| matches!(m, ServerMsg::Welcome { .. }))?;
    Ok(c)
}

struct Start {
    game: u32,
    token: u64,
    you: u8,
    scenario: Box<ScenarioSpec>,
    bots: Vec<(u8, u8)>,
    tick_rate: u32,
}

fn as_start(m: ServerMsg) -> Start {
    match m {
        ServerMsg::GameStart { game, token, you, scenario, bots, tick_rate } => {
            Start { game, token, you, scenario, bots, tick_rate }
        }
        _ => unreachable!("waited for GameStart"),
    }
}

fn session(s: &Start, make_bot: BotFactory) -> Result<Session, String> {
    let bots = s.bots.iter().map(|&(p, d)| make_bot(p, d)).collect();
    let mut session =
        Session::new(s.game, s.token, s.you, (*s.scenario).clone(), s.tick_rate, bots, Balance::default_balance())?;
    session.world.set_threads(1)?;
    session.set_pilot(make_bot(s.you, 2));
    Ok(session)
}

struct ClientRun {
    checkpoints: Vec<(u32, u64)>,
    relay_desync: bool,
    rejoined: bool,
    stalls: u32,
    ticks: u32,
    winner: Option<u8>,
}

/// Plays one client to `ticks`. With `drop_at`, disconnects at that tick
/// and rejoins with a fresh world.
fn play(
    mut net: NetClient,
    start: Start,
    addr: SocketAddr,
    ticks: u32,
    drop_at: Option<u32>,
    make_bot: BotFactory,
) -> Result<ClientRun, String> {
    let t0 = Instant::now();
    let mut s = session(&start, make_bot)?;
    let mut r =
        ClientRun { checkpoints: Vec::new(), relay_desync: false, rejoined: false, stalls: 0, ticks: 0, winner: None };
    let mut drop_at = drop_at;
    let mut last = Instant::now();
    let mut out = Vec::new();
    let deadline = Duration::from_secs(60) + Duration::from_secs_f64(ticks as f64 / start.tick_rate as f64 * 4.0);
    while s.world.tick() < ticks {
        if t0.elapsed() > deadline {
            return Err(format!("player {} stuck at tick {}", s.you, s.world.tick()));
        }
        if drop_at.is_some_and(|t| s.world.tick() >= t) {
            drop_at = None;
            drop(net);
            std::thread::sleep(Duration::from_millis(1500));
            net = hello(addr, "rejoiner")?;
            net.send(ClientMsg::Rejoin { game: start.game, token: start.token });
            let st = as_start(wait(&net, Duration::from_secs(20), |m| matches!(m, ServerMsg::GameStart { .. }))?);
            s = session(&st, make_bot)?;
            r.rejoined = true;
        }
        let now = t0.elapsed().as_secs_f64();
        for ev in net.poll() {
            match ev {
                NetEvent::Msg(ServerMsg::Tick(p)) => s.receive(p, now),
                NetEvent::Msg(ServerMsg::Desync { .. }) => r.relay_desync = true,
                NetEvent::Msg(ServerMsg::Stall { .. }) => r.stalls += 1,
                NetEvent::Disconnected(e) => return Err(format!("player {} lost the relay: {e}", s.you)),
                _ => {}
            }
        }
        let dt = last.elapsed().as_secs_f64();
        last = Instant::now();
        let left = ticks - s.world.tick();
        if s.advance(dt, left.min(500), &mut out) == 0 {
            std::thread::sleep(Duration::from_millis(2));
        }
        for m in out.drain(..) {
            net.send(m);
        }
    }
    // Let the last hashes reach the relay before leaving.
    std::thread::sleep(Duration::from_millis(500));
    for ev in net.poll() {
        if let NetEvent::Msg(ServerMsg::Desync { .. }) = ev {
            r.relay_desync = true;
        }
    }
    r.checkpoints = s.checkpoints().to_vec();
    r.ticks = s.world.tick();
    r.winner = s.world.winner();
    Ok(r)
}

/// Runs one networked match and compares every client's hashes.
pub fn run(opts: &Options, make_bot: BotFactory) -> Result<Outcome, String> {
    let t0 = Instant::now();
    let config = Config { tick_rate: opts.tick_rate, ..Config::default() };
    let server = crate::spawn_local("127.0.0.1:0".parse().expect("address"), config)?;
    let proxy = match opts.network {
        Some(c) => Some(netem::spawn(server.addr, c)?),
        None => None,
    };
    let addr = proxy.as_ref().map_or(server.addr, |p| p.addr);
    let n = opts.teams.len();
    let mut clients = Vec::new();
    for k in 0..n {
        clients.push(hello(addr, &format!("bot{k}"))?);
    }
    let slots = (n + opts.bots.len()) as u8;
    let host = &clients[0];
    host.send(ClientMsg::CreateLobby {
        name: "netsim".into(),
        settings: LobbySettings { map_size: opts.map_size, seed: opts.seed, slots },
    });
    let ServerMsg::Lobby(lobby) = wait(host, Duration::from_secs(20), |m| matches!(m, ServerMsg::Lobby(_)))? else {
        unreachable!()
    };
    for c in &clients[1..] {
        c.send(ClientMsg::JoinLobby { lobby: lobby.id });
        c.send(ClientMsg::SetReady(true));
    }
    // Wait until every human is in (joins race over separate connections).
    wait(host, Duration::from_secs(20), |m| {
        matches!(m, ServerMsg::Lobby(l)
            if l.slots.iter().filter(|s| matches!(s.kind, net::proto::SlotKind::Human { .. })).count() == n)
    })?;
    // Humans landed in slots in arrival order; give teams by slot.
    for (k, &team) in opts.teams.iter().enumerate() {
        host.send(ClientMsg::SetTeam { slot: k as u8, team });
    }
    for (k, &(team, difficulty)) in opts.bots.iter().enumerate() {
        let slot = (n + k) as u8;
        host.send(ClientMsg::SetSlot { slot, kind: net::proto::SlotKind::Bot { difficulty } });
        host.send(ClientMsg::SetTeam { slot, team });
    }
    let mut start0 = None;
    for _ in 0..50 {
        host.send(ClientMsg::Start);
        if let Ok(m) = wait(host, Duration::from_millis(400), |m| matches!(m, ServerMsg::GameStart { .. })) {
            start0 = Some(as_start(m));
            break;
        }
    }
    let start0 = start0.ok_or("the lobby never started")?;
    let mut starts = vec![Some(start0)];
    for c in &clients[1..] {
        starts.push(Some(as_start(wait(c, Duration::from_secs(20), |m| matches!(m, ServerMsg::GameStart { .. }))?)));
    }
    let handles: Vec<_> = clients
        .into_iter()
        .zip(starts)
        .enumerate()
        .map(|(k, (c, s))| {
            let s = s.expect("start");
            let drop_at = (opts.reconnect && k == n - 1).then_some(opts.ticks / 3);
            let ticks = opts.ticks;
            std::thread::spawn(move || play(c, s, addr, ticks, drop_at, make_bot))
        })
        .collect();
    let runs: Vec<ClientRun> =
        handles.into_iter().map(|h| h.join().map_err(|_| "client panicked".to_string())?).collect::<Result<_, _>>()?;
    drop(proxy);
    server.stop();

    let mut o = Outcome {
        ticks: runs.iter().map(|r| r.ticks).min().unwrap_or(0),
        relay_desync: runs.iter().any(|r| r.relay_desync),
        rejoined: runs.iter().any(|r| r.rejoined),
        stalls: runs.iter().map(|r| r.stalls).max().unwrap_or(0),
        winner: runs[0].winner,
        seconds: t0.elapsed().as_secs_f64(),
        ..Outcome::default()
    };
    let base = &runs[0].checkpoints;
    for (i, &(tick, h)) in base.iter().enumerate() {
        let hs: Vec<u64> = runs.iter().filter_map(|r| r.checkpoints.get(i).map(|c| c.1)).collect();
        if hs.len() != runs.len() || runs.iter().any(|r| r.checkpoints[i].0 != tick) {
            return Err(format!("checkpoint lists differ at #{i}"));
        }
        o.checkpoints += 1;
        if o.desync.is_none() && hs.iter().any(|&x| x != h) {
            o.desync = Some((tick, hs));
        }
    }
    Ok(o)
}
