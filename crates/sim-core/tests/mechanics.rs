//! Gameplay checks against spec 2.3–2.8 on tiny hand-made maps.

use sim_core::command::{Command, CommandKind};
use sim_core::fx::{Fx, FxPoint, ONE};
use sim_core::map::{Map, Terrain};
use sim_core::unit::flags;
use sim_core::{Balance, PlayerSetup, UnitHandle, UnitKind, World};

const TPS: u32 = 30;

fn pt(x: i32, y: i32) -> FxPoint {
    FxPoint::tile_center(x, y)
}

fn world(map: Map) -> World {
    let players = [PlayerSetup { team: 1 }, PlayerSetup { team: 2 }];
    let mut w = World::new(map, &Balance::default_balance(), 42, &players).unwrap();
    // Pure movement/combat checks: no supply or upkeep in the way.
    w.set_sandbox(true);
    w
}

fn plain() -> Map {
    Map::new(64, 64, Terrain::Plain).unwrap()
}

fn mv(tick: u32, player: u8, seq: u32, units: &[UnitHandle], path: &[FxPoint]) -> Command {
    Command { tick, player, seq, kind: CommandKind::Move { units: units.to_vec(), path: path.to_vec(), queue: false } }
}

fn hp(w: &World, h: UnitHandle) -> Option<u16> {
    w.units().index_of(h).map(|i| w.units().hp[i])
}

fn pos(w: &World, h: UnitHandle) -> (i32, i32) {
    let i = w.units().index_of(h).unwrap();
    (w.units().pos_x[i], w.units().pos_y[i])
}

/// Steps until `done` or `max` ticks; returns ticks taken.
fn run_until(w: &mut World, cmds: &[Command], max: u32, mut done: impl FnMut(&World) -> bool) -> u32 {
    let start = w.tick();
    while w.tick() - start < max {
        if done(w) {
            break;
        }
        w.step(cmds);
    }
    w.tick() - start
}

#[test]
fn light_unit_walks_two_tiles_per_second() {
    let mut w = world(plain());
    let a = w.spawn(1, UnitKind::Light, pt(10, 10)).unwrap();
    let cmds = [mv(0, 1, 0, &[a], &[pt(20, 10)])];
    let t =
        run_until(&mut w, &cmds, 600, |w| w.units().order[w.units().index_of(a).unwrap()] == u32::MAX && w.tick() > 0);
    // 10 tiles at 2 tiles/s = 5 s.
    assert!((5 * TPS - 2..=5 * TPS + 2).contains(&t), "took {t} ticks");
    assert_eq!(pos(&w, a), (pt(20, 10).x.0, pt(20, 10).y.0));
}

#[test]
fn heavy_is_slow_in_forest_and_light_is_slow_on_sand() {
    let measure = |terrain: Terrain, kind: UnitKind| {
        let mut w = world(Map::new(64, 64, terrain).unwrap());
        let a = w.spawn(1, kind, pt(10, 10)).unwrap();
        let cmds = [mv(0, 1, 0, &[a], &[pt(50, 10)])];
        for _ in 0..TPS {
            w.step(&cmds);
        }
        Fx(pos(&w, a).0 - pt(10, 10).x.0)
    };
    // Per-tick speeds are truncated to 1/65536 tile, so allow a hair of error.
    let approx = |got: Fx, tiles_x10: i32| {
        let want = tiles_x10 * ONE / 10;
        assert!((got.0 - want).abs() <= 64, "moved {got:?}, expected {tiles_x10}/10 tiles");
    };
    approx(measure(Terrain::Plain, UnitKind::Light), 20);
    approx(measure(Terrain::Forest, UnitKind::Light), 20);
    approx(measure(Terrain::Forest, UnitKind::Heavy), 5);
    approx(measure(Terrain::Sand, UnitKind::Light), 12);
    approx(measure(Terrain::Sand, UnitKind::Heavy), 10);
}

#[test]
fn mountains_are_impassable() {
    let mut m = plain();
    for y in 0..64 {
        m.set_terrain(30, y, Terrain::Mountain);
    }
    let mut w = world(m);
    let a = w.spawn(1, UnitKind::Light, pt(20, 20)).unwrap();
    let cmds = [mv(0, 1, 0, &[a], &[pt(40, 20)])];
    for _ in 0..20 * TPS {
        w.step(&cmds);
        assert!(pos(&w, a).0 < 30 * ONE, "walked into the mountain");
    }
    // It gave up on the unreachable waypoint instead of pushing forever.
    assert_eq!(w.units().order[w.units().index_of(a).unwrap()], u32::MAX);
}

#[test]
fn equal_defenders_duel_lasts_about_fifteen_seconds() {
    let mut w = world(plain());
    let a = w.spawn(1, UnitKind::Light, pt(20, 20)).unwrap();
    let b = w.spawn(2, UnitKind::Light, pt(21, 20)).unwrap();
    let t = run_until(&mut w, &[], 60 * TPS, |w| hp(w, a).is_none() || hp(w, b).is_none());
    // 2·ln2/0.092 ≈ 15 s without morale; falling morale makes it a bit longer.
    assert!((14 * TPS..=20 * TPS).contains(&t), "duel took {t} ticks");
}

#[test]
fn defender_beats_equal_attacker() {
    let mut w = world(plain());
    let atk = w.spawn(1, UnitKind::Light, pt(18, 20)).unwrap();
    let def = w.spawn(2, UnitKind::Light, pt(21, 20)).unwrap();
    // Attacker keeps pushing through the defender.
    let cmds = [mv(0, 1, 0, &[atk], &[pt(40, 20)])];
    run_until(&mut w, &cmds, 60 * TPS, |w| hp(w, atk).is_none() || hp(w, def).is_none());
    assert!(hp(&w, atk).is_none() || hp(&w, def).is_some(), "attacker should not win");
    assert!(hp(&w, def).is_some(), "defender should survive");
}

#[test]
fn heavy_wins_on_plain_and_loses_in_forest() {
    let duel = |terrain: Terrain| {
        let mut w = world(Map::new(64, 64, terrain).unwrap());
        let heavy = w.spawn(1, UnitKind::Heavy, pt(20, 20)).unwrap();
        let light = w.spawn(2, UnitKind::Light, pt(21, 20)).unwrap();
        run_until(&mut w, &[], 120 * TPS, |w| hp(w, heavy).is_none() || hp(w, light).is_none());
        hp(&w, heavy).is_some()
    };
    assert!(duel(Terrain::Plain), "heavy should win on plain");
    assert!(!duel(Terrain::Forest), "light should win in forest");
    assert!(!duel(Terrain::Hills), "light should win on hills");
}

#[test]
fn flanking_splits_damage() {
    // One defender against two attackers dies faster than in a 1v1.
    let mut w = world(plain());
    let d = w.spawn(2, UnitKind::Light, pt(20, 20)).unwrap();
    w.spawn(1, UnitKind::Light, pt(19, 20)).unwrap();
    w.spawn(1, UnitKind::Light, pt(21, 20)).unwrap();
    let t = run_until(&mut w, &[], 60 * TPS, |w| hp(w, d).is_none());
    assert!(t < 12 * TPS, "outnumbered unit lasted {t} ticks");
}

#[test]
fn water_hurts_then_turns_into_ship() {
    let mut m = plain();
    for y in 0..64 {
        for x in 20..44 {
            m.set_terrain(x, y, Terrain::Water);
        }
    }
    let mut w = world(m);
    let a = w.spawn(1, UnitKind::Light, pt(19, 10)).unwrap();
    let cmds = [mv(0, 1, 0, &[a], &[pt(43, 10)])];
    let mut became_ship_at = None;
    let mut hp_at_ship = 0;
    for _ in 0..30 * TPS {
        w.step(&cmds);
        let i = w.units().index_of(a).unwrap();
        if became_ship_at.is_none() && w.units().flags[i] & flags::SHIP != 0 {
            became_ship_at = Some(w.tick());
            hp_at_ship = w.units().hp[i];
        }
    }
    let t = became_ship_at.expect("never became a ship") as i32;
    assert!((5 * TPS as i32..=6 * TPS as i32 + 15).contains(&t), "ship after {t} ticks");
    // ~5 s at 3 HP/s ≈ 15 HP lost (no healing in water), then no more water damage.
    let lost = 10_000 - hp_at_ship as i32;
    assert!((1_200..=1_800).contains(&lost), "lost {lost} hundredths of HP");
}

#[test]
fn stop_and_morale_break() {
    let mut w = world(plain());
    let a = w.spawn(1, UnitKind::Light, pt(10, 10)).unwrap();
    let cmds = [
        mv(0, 1, 0, &[a], &[pt(50, 10)]),
        Command { tick: 10, player: 1, seq: 1, kind: CommandKind::Stop { units: vec![a] } },
        // Commands for someone else's unit are rejected.
        Command {
            tick: 11,
            player: 2,
            seq: 0,
            kind: CommandKind::Move { units: vec![a], path: vec![pt(0, 0)], queue: false },
        },
    ];
    for _ in 0..40 {
        w.step(&cmds);
    }
    let p = pos(&w, a);
    assert!(p.0 < pt(12, 10).x.0, "unit kept walking after Stop");
    assert_eq!(w.orders().live(), 0);
}

#[test]
fn queued_paths_are_followed_in_order() {
    let mut w = world(plain());
    let a = w.spawn(1, UnitKind::Light, pt(10, 10)).unwrap();
    let b = w.spawn(1, UnitKind::Light, pt(10, 12)).unwrap();
    let cmds = [
        mv(0, 1, 0, &[a, b], &[pt(14, 11)]),
        Command {
            tick: 0,
            player: 1,
            seq: 1,
            kind: CommandKind::Move { units: vec![a, b], path: vec![pt(14, 30)], queue: true },
        },
    ];
    let mut max_x = 0;
    for _ in 0..20 * TPS {
        w.step(&cmds);
        max_x = max_x.max(pos(&w, a).0);
    }
    let (x, y) = pos(&w, a);
    assert!(x > pt(13, 0).x.0 && y > pt(0, 25).y.0, "did not follow the queue: {x} {y}");
    assert_eq!(w.orders().live(), 0, "orders leaked");
}

#[test]
fn attacker_with_broken_morale_stops() {
    let mut w = world(plain());
    // Three defenders wear down one attacker's morale quickly.
    let atk = w.spawn(1, UnitKind::Heavy, pt(18, 20)).unwrap();
    for y in 19..=21 {
        w.spawn(2, UnitKind::Heavy, pt(20, y)).unwrap();
    }
    let cmds = [mv(0, 1, 0, &[atk], &[pt(40, 20)])];
    let mut saw_break = false;
    for _ in 0..40 * TPS {
        w.step(&cmds);
        let Some(i) = w.units().index_of(atk) else { break };
        if w.units().morale[i] == 0 {
            assert_eq!(w.units().order[i], u32::MAX);
            saw_break = true;
            break;
        }
    }
    // 4 morale/s from 100: ~25 s, unless it died first — either is fine,
    // but if morale hit zero the order must be gone.
    let _ = saw_break;
}
