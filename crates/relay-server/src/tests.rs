use super::*;
use net::proto::ClientMsg as C;
use sim_core::command::CommandKind;

fn hello(r: &mut Relay, name: &str) -> u32 {
    let id = r.connect();
    let mut out = Vec::new();
    let sim_version = sim_core::sim_version(&Balance::default_balance());
    r.message(id, C::Hello { name: name.into(), protocol: PROTOCOL_VERSION, sim_version }, &mut out);
    assert_eq!(out, vec![Out::Send(id, ServerMsg::Welcome { client: id })]);
    id
}

fn to(out: &[Out], client: u32) -> Vec<ServerMsg> {
    out.iter()
        .filter_map(|o| match o {
            Out::Send(c, m) if *c == client => Some(m.clone()),
            _ => None,
        })
        .collect()
}

fn settings() -> LobbySettings {
    LobbySettings { width: 256, height: 128, seed: 3, slots: 4, map: None, unit_budget: None }
}

/// Two humans and a bot, started; returns (relay, host, guest, game, tokens).
fn started() -> (Relay, u32, u32, u32, [u64; 2]) {
    let mut r = Relay::new(Config::default(), 1);
    let a = hello(&mut r, "ann");
    let b = hello(&mut r, "bob");
    let mut out = Vec::new();
    r.message(a, C::CreateLobby { name: "test".into(), settings: settings() }, &mut out);
    let Some(ServerMsg::Lobby(l)) = to(&out, a).pop() else { panic!("{out:?}") };
    r.message(b, C::JoinLobby { lobby: l.id }, &mut out);
    r.message(a, C::SetSlot { slot: 2, kind: SlotKind::Bot { difficulty: 1 } }, &mut out);
    r.message(a, C::SetSlot { slot: 3, kind: SlotKind::Closed }, &mut out);
    r.message(a, C::SetTeam { slot: 1, team: 1 }, &mut out);
    r.message(a, C::SetTeam { slot: 2, team: 2 }, &mut out);
    out.clear();
    r.message(a, C::Start, &mut out);
    assert_eq!(to(&out, a), vec![ServerMsg::Error("not everyone is ready".into())]);
    r.message(b, C::SetReady(true), &mut out);
    out.clear();
    r.message(a, C::Start, &mut out);
    let starts: Vec<_> = [a, b]
        .iter()
        .map(|&c| match to(&out, c).pop() {
            Some(ServerMsg::GameStart { game, token, you, scenario, bots, .. }) => (game, token, you, scenario, bots),
            m => panic!("{m:?}"),
        })
        .collect();
    assert_eq!(starts[0].2, 1);
    assert_eq!(starts[1].2, 2);
    assert_eq!(starts[0].4, vec![(3, 1)]);
    let teams: Vec<u8> = starts[0].3.players.iter().map(|p| p.team).collect();
    assert_eq!(teams, vec![1, 1, 2], "ann and bob together against the bot");
    assert_eq!(starts[0].3, starts[1].3);
    (r, a, b, starts[0].0, [starts[0].1, starts[1].1])
}

#[test]
fn wrong_version_is_turned_away() {
    let mut r = Relay::new(Config::default(), 1);
    let id = r.connect();
    let mut out = Vec::new();
    r.message(id, C::Hello { name: "x".into(), protocol: PROTOCOL_VERSION, sim_version: 1 }, &mut out);
    assert_eq!(out.last(), Some(&Out::Close(id)));
}

#[test]
fn lobby_flow_starts_a_game() {
    let (r, ..) = started();
    assert_eq!(r.games(), 1);
}

#[test]
fn host_leaving_hands_over_the_lobby() {
    let mut r = Relay::new(Config::default(), 1);
    let a = hello(&mut r, "ann");
    let b = hello(&mut r, "bob");
    let mut out = Vec::new();
    r.message(a, C::CreateLobby { name: "l".into(), settings: settings() }, &mut out);
    r.message(b, C::JoinLobby { lobby: 3 }, &mut out);
    out.clear();
    r.disconnect(a, &mut out);
    let Some(ServerMsg::Lobby(l)) = to(&out, b).pop() else { panic!() };
    assert_eq!(l.host, b);
    out.clear();
    r.message(b, C::SetSlot { slot: 0, kind: SlotKind::Closed }, &mut out);
    assert!(matches!(to(&out, b).pop(), Some(ServerMsg::Lobby(_))));
}

#[test]
fn ticks_carry_stamped_commands() {
    let (mut r, a, b, ..) = started();
    let mut out = Vec::new();
    r.tick(&mut out);
    assert_eq!(to(&out, a), vec![ServerMsg::Tick(TickPacket { tick: 0, commands: vec![] })]);
    out.clear();
    r.message(b, C::Commands { seq: 1, kinds: vec![CommandKind::Stop { units: vec![] }] }, &mut out);
    r.tick(&mut out);
    let [ServerMsg::Tick(p)] = &to(&out, a)[..] else { panic!("{out:?}") };
    assert_eq!(p.tick, 1);
    assert_eq!(p.commands.len(), 1);
    assert_eq!((p.commands[0].tick, p.commands[0].player), (1, 2));
    assert_eq!(to(&out, a), to(&out, b));
}

/// Runs `ticks` clock ticks with `clients` keeping up.
fn run(r: &mut Relay, clients: &[u32], ticks: u32) {
    let mut out = Vec::new();
    for _ in 0..ticks {
        r.tick(&mut out);
        for &c in clients {
            r.message(c, C::Progress { tick: u32::MAX }, &mut out);
        }
    }
}

#[test]
fn desync_is_detected() {
    let (mut r, a, b, ..) = started();
    run(&mut r, &[a, b], 31);
    let mut out = Vec::new();
    r.message(a, C::Hash { tick: 30, hash: 5 }, &mut out);
    assert!(out.is_empty());
    r.message(b, C::Hash { tick: 30, hash: 5 }, &mut out);
    assert!(out.is_empty(), "agreeing hashes are fine");
    run(&mut r, &[a, b], 30);
    r.message(a, C::Hash { tick: 60, hash: 7 }, &mut out);
    r.message(b, C::Hash { tick: 60, hash: 8 }, &mut out);
    let d = ServerMsg::Desync { tick: 60, hashes: vec![(1, 7), (2, 8)] };
    assert_eq!(to(&out, a), vec![d.clone()]);
    assert_eq!(to(&out, b), vec![d]);
}

#[test]
fn lagging_client_stalls_the_game() {
    let (mut r, a, b, ..) = started();
    let mut out = Vec::new();
    r.message(a, C::Progress { tick: 1000 }, &mut out);
    for _ in 0..STALL_TICKS + 5 {
        r.tick(&mut out);
        r.message(a, C::Progress { tick: 1000 }, &mut out);
    }
    assert!(to(&out, a).contains(&ServerMsg::Stall { waiting_for: vec![2] }));
    let ticks = to(&out, a).iter().filter(|m| matches!(m, ServerMsg::Tick(_))).count() as u32;
    assert_eq!(ticks, STALL_TICKS + 1);
    out.clear();
    r.message(b, C::Progress { tick: STALL_TICKS }, &mut out);
    r.tick(&mut out);
    assert_eq!(to(&out, a)[0], ServerMsg::Resume);
}

#[test]
fn rejoin_replays_history_and_checks_hashes() {
    let (mut r, a, b, game, tokens) = started();
    run(&mut r, &[a, b], 40);
    let mut out = Vec::new();
    r.message(a, C::Hash { tick: 30, hash: 9 }, &mut out);
    r.message(b, C::Hash { tick: 30, hash: 9 }, &mut out);
    r.disconnect(b, &mut out);
    assert_eq!(to(&out, a), vec![ServerMsg::PlayerLeft(2)]);
    run(&mut r, &[a], 200);
    let b2 = hello(&mut r, "bob");
    out.clear();
    r.message(b2, C::Rejoin { game, token: tokens[0] }, &mut out);
    assert!(matches!(to(&out, b2)[..], [ServerMsg::Error(_)]), "ann's token is in use");
    out.clear();
    r.message(b2, C::Rejoin { game, token: tokens[1] }, &mut out);
    let msgs = to(&out, b2);
    assert!(matches!(msgs[0], ServerMsg::GameStart { you: 2, .. }));
    assert_eq!(msgs.len(), 1 + 240);
    assert_eq!(to(&out, a), vec![ServerMsg::PlayerBack(2)]);
    // Still catching up: the game goes on without waiting.
    out.clear();
    r.tick(&mut out);
    assert!(to(&out, a).iter().any(|m| matches!(m, ServerMsg::Tick(_))));
    // Its replayed hash is checked against the agreed one.
    r.message(b2, C::Hash { tick: 30, hash: 10 }, &mut out);
    assert!(to(&out, a).iter().any(|m| matches!(m, ServerMsg::Desync { tick: 30, .. })));
}

#[test]
fn map_lobby_uses_the_map_starts_and_teams() {
    let mut f = sim_core::mapfile::MapFile::blank(256, 128).unwrap();
    f.starts = vec![(30, 30), (225, 30), (30, 97), (225, 97)];
    f.teams = vec![1, 2, 1, 2];
    f.cities = vec![(128, 64)];
    let bytes = f.encode();
    let mut r = Relay::new(Config::default(), 1);
    let a = hello(&mut r, "ann");
    let b = hello(&mut r, "bob");
    let mut out = Vec::new();
    let settings = LobbySettings {
        width: 0,
        height: 0,
        seed: 0,
        slots: 2,
        map: Some(net::proto::LobbyMap { name: "Test".into(), bytes: bytes.clone() }),
        unit_budget: None,
    };
    r.message(a, C::CreateLobby { name: "m".into(), settings }, &mut out);
    let Some(ServerMsg::Lobby(l)) = to(&out, a).pop() else { panic!("{out:?}") };
    assert_eq!(l.slots.len(), 4, "one slot per start");
    assert_eq!(l.slots.iter().map(|s| s.team).collect::<Vec<_>>(), vec![1, 2, 1, 2]);
    // Bob takes slot 1 (team 2), a bot slot 2 (team 1); slot 3 stays empty.
    r.message(b, C::JoinLobby { lobby: l.id }, &mut out);
    r.message(b, C::SetReady(true), &mut out);
    r.message(a, C::SetSlot { slot: 2, kind: SlotKind::Bot { difficulty: 1 } }, &mut out);
    out.clear();
    r.message(a, C::Start, &mut out);
    let Some(ServerMsg::GameStart { scenario, bots, .. }) = to(&out, b).pop() else { panic!("{out:?}") };
    assert_eq!(scenario.players.iter().map(|p| p.team).collect::<Vec<_>>(), vec![1, 2, 1]);
    assert_eq!(bots, vec![(3, 1)]);
    // The empty start (225, 97) is a neutral city.
    assert!(scenario.cities.iter().any(|c| (c.x, c.y) == (225, 97) && c.owner == 0));
    scenario.build(&Balance::default_balance()).unwrap();
}
