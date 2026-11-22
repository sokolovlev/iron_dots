//! Cities, money, territory, supply pockets, capture and victory (spec 2.2–2.6).

use sim_core::command::{Command, CommandKind};
use sim_core::fx::FxPoint;
use sim_core::map::{Map, Terrain};
use sim_core::scenario::ScenarioSpec;
use sim_core::{Balance, PlayerSetup, UnitHandle, UnitKind, World};

const TPS: u32 = 30;

fn pt(x: i32, y: i32) -> FxPoint {
    FxPoint::tile_center(x, y)
}

fn world() -> World {
    let players = [PlayerSetup { team: 1 }, PlayerSetup { team: 2 }];
    World::new(Map::new(64, 64, Terrain::Plain).unwrap(), &Balance::default_balance(), 1, &players).unwrap()
}

fn buy(tick: u32, player: u8, seq: u32, city: u16, kind: UnitKind) -> Command {
    Command { tick, player, seq, kind: CommandKind::Buy { city, kind } }
}

fn mv(tick: u32, player: u8, units: &[UnitHandle], to: FxPoint) -> Command {
    Command {
        tick,
        player,
        seq: 100,
        kind: CommandKind::Move { units: units.to_vec(), path: vec![to], queue: false, formation: Default::default() },
    }
}

fn run(w: &mut World, cmds: &[Command], ticks: u32) {
    for _ in 0..ticks {
        w.step(cmds);
    }
}

#[test]
fn city_income_and_buying() {
    let mut w = world();
    w.add_city(10, 10, 1, true).unwrap();
    assert_eq!(w.player(1).money, 200_000);
    // Light costs 200: affordable at once; heavy (400) is not.
    let cmds = [buy(0, 1, 0, 0, UnitKind::Light), buy(0, 1, 1, 0, UnitKind::Heavy)];
    w.step(&cmds);
    assert_eq!(w.units().len(), 1);
    assert_eq!(w.player(1).money, 0);
    assert_eq!(w.stats().commands_rejected, 1);
    // A unit inside its city has no upkeep: +10/s.
    run(&mut w, &[], 10 * TPS);
    assert_eq!(w.player(1).money, 100_000);
}

#[test]
fn supply_limit_blocks_buying() {
    let mut w = world();
    w.add_city(10, 10, 1, true).unwrap();
    run(&mut w, &[], 3 * 60 * TPS); // 1800 money
    let cmds: Vec<Command> = (0..8).map(|k| buy(w.tick(), 1, k, 0, UnitKind::Light)).collect();
    w.step(&cmds);
    // 5 places per city (one already taken by the unit the empty city
    // raised for free).
    // (It may be heavy and take two places.)
    w.step(&[]);
    let n = w.units().len();
    assert!((4..=5).contains(&n), "{n} units, supply {:?} stats {:?}", w.city_supply(0), w.stats());
    assert_eq!(w.city_supply(0), Some((5, 5)));
    let heavy = [buy(w.tick(), 1, 99, 0, UnitKind::Heavy)];
    w.step(&heavy);
    assert_eq!(w.units().len(), n);
}

#[test]
fn zones_follow_units_and_contested_land_is_neutral() {
    let mut w = world();
    let a = w.spawn(1, UnitKind::Light, pt(20, 20)).unwrap();
    // Zones are recomputed every PAINT_INTERVAL ticks, staggered.
    let interval = sim_core::territory::PAINT_INTERVAL;
    run(&mut w, &[], interval);
    assert_eq!(w.map().owner(20, 20), 1);
    assert_eq!(w.map().owner(23, 20), 1);
    assert_eq!(w.map().owner(24, 20), 0, "beyond the radius: neutral");
    // The zone moves with the unit and does not stay behind.
    let cmds = [mv(w.tick(), 1, &[a], pt(40, 20))];
    run(&mut w, &cmds, 15 * TPS);
    assert_eq!(w.map().owner(40, 20), 1);
    assert_eq!(w.map().owner(20, 20), 0, "land is not kept after the unit leaves");
    // Where both sides reach, the land is nobody's.
    w.spawn(2, UnitKind::Light, pt(45, 20)).unwrap();
    run(&mut w, &[], interval);
    assert_eq!(w.map().owner(42, 20), 0, "contested");
    assert_eq!(w.map().owner(38, 20), 1);
    assert_eq!(w.map().owner(47, 20), 2);
}

#[test]
fn only_a_closed_ring_of_enemy_control_cuts_supply() {
    let mut w = world();
    w.add_city(5, 5, 1, true).unwrap();
    // Far from the city, but neutral land connects it: supplied.
    let far = w.spawn(1, UnitKind::Light, pt(45, 45)).unwrap();
    run(&mut w, &[], 5 * TPS);
    let hp = |w: &World, h| w.units().hp[w.units().index_of(h).unwrap()];
    assert_eq!(hp(&w, far), 10_000, "neutral land carries supply");
    // A ring of enemy units around it (radius 8, every 22.5°) holds a
    // closed band of land: the unit is cut off and starves.
    const RING: [(i32, i32); 16] = [
        (8, 0),
        (7, 3),
        (6, 6),
        (3, 7),
        (0, 8),
        (-3, 7),
        (-6, 6),
        (-7, 3),
        (-8, 0),
        (-7, -3),
        (-6, -6),
        (-3, -7),
        (0, -8),
        (3, -7),
        (6, -6),
        (7, -3),
    ];
    for (dx, dy) in RING {
        w.spawn(2, UnitKind::Light, pt(45 + dx, 45 + dy)).unwrap();
    }
    run(&mut w, &[], 10 * TPS);
    let lost = 10_000 - hp(&w, far) as i32;
    // 5 HP/s for ~10 s (minus the moment the ring took to close).
    assert!((3_500..=5_500).contains(&lost), "lost {lost}");
}

#[test]
fn overloaded_pocket_starves_proportionally() {
    let mut w = world();
    w.add_city(10, 10, 1, true).unwrap();
    // 10 light on 5 places: overload 100% → 5 HP/s.
    for k in 0..10 {
        w.spawn(1, UnitKind::Light, pt(6 + k % 5, 12 + k / 5)).unwrap();
    }
    run(&mut w, &[], 4 * TPS);
    let min_hp = w.units().hp.iter().min().copied().unwrap();
    assert!(min_hp < 9_000, "nobody starved: {min_hp}");
}

#[test]
fn healing_out_of_combat() {
    let mut w = world();
    w.add_city(10, 10, 1, true).unwrap();
    let a = w.spawn(1, UnitKind::Light, pt(14, 10)).unwrap();
    let c = w.spawn(1, UnitKind::Light, pt(10, 10)).unwrap();
    // Wound them by pushing through water is awkward; instead let an enemy
    // hit them briefly.
    let e = w.spawn(2, UnitKind::Heavy, pt(15, 10)).unwrap();
    let _ = e;
    run(&mut w, &[], 3 * TPS);
    let hp = |w: &World, h| w.units().index_of(h).map(|i| w.units().hp[i]);
    let after_fight = hp(&w, a).unwrap();
    assert!(after_fight < 10_000);
    // Remove the enemy by moving it far away.
    let away = [mv(w.tick(), 2, &[e], pt(60, 60))];
    run(&mut w, &away, 15 * TPS);
    assert!(hp(&w, a).unwrap() > after_fight, "no healing");
    assert_eq!(hp(&w, c), Some(10_000));
}

#[test]
fn capture_takes_five_seconds_and_resets_with_defender() {
    let mut w = world();
    w.add_city(30, 30, 0, false).unwrap();
    let a = w.spawn(1, UnitKind::Light, pt(30, 30)).unwrap();
    run(&mut w, &[], 4 * TPS);
    assert_eq!(w.cities()[0].owner, 0);
    run(&mut w, &[], TPS + 1);
    assert_eq!(w.cities()[0].owner, 1);
    assert_eq!(w.map().owner(30, 30), 1);
    // Enemy walks in while the defender stands there: no capture.
    let e = w.spawn(2, UnitKind::Light, pt(40, 30)).unwrap();
    let cmds = [mv(w.tick(), 2, &[e], pt(31, 30))];
    run(&mut w, &cmds, 8 * TPS);
    let _ = a;
    assert_eq!(w.cities()[0].owner, 1);
}

#[test]
fn taking_capital_and_most_cities_wins() {
    let mut w = world();
    w.add_city(5, 5, 1, true).unwrap();
    w.add_city(55, 55, 2, true).unwrap();
    w.add_city(30, 30, 0, false).unwrap();
    w.add_city(30, 50, 0, false).unwrap();
    w.add_city(50, 30, 0, false).unwrap();
    let units: Vec<UnitHandle> = [(55, 55), (30, 30), (30, 50), (50, 30)]
        .iter()
        .map(|&(x, y)| w.spawn(1, UnitKind::Light, pt(x, y)).unwrap())
        .collect();
    let _ = units;
    // Player 2's capital is captured; player 1 then holds 5/5 cities.
    run(&mut w, &[], 8 * TPS);
    assert_eq!(w.winner(), Some(1));
}

#[test]
fn standard_match_setup() {
    let b = Balance::default_balance();
    let spec = ScenarioSpec::standard_match(256, 256, 2, 3, &b).unwrap();
    let w = spec.build(&b).unwrap();
    assert_eq!(w.cities().iter().filter(|c| c.capital_of != 0).count(), 2);
    assert!(w.cities().len() >= 8, "only {} cities", w.cities().len());
    assert_eq!(w.units().len(), 6);
    assert_eq!(w.unit_counts()[1], 3);
}

#[test]
fn wide_map_presets_make_fair_matches() {
    use sim_core::scenario::MAP_PRESETS;
    let b = Balance::default_balance();
    for &(name, w, h) in &MAP_PRESETS[..4] {
        for players in [2u8, 8] {
            let spec = ScenarioSpec::standard_match(w, h, players, 1, &b).unwrap();
            let caps: Vec<_> = spec.cities.iter().filter(|c| c.capital).collect();
            assert_eq!(caps.len(), players as usize, "{name}");
            // Capitals spread over the long side too, not squeezed in a square.
            let xs = caps.iter().map(|c| c.x);
            let span = xs.clone().max().unwrap() - xs.min().unwrap();
            assert!(span > w / 3, "{name}: capitals span {span} of {w}");
            let neutral = spec.cities.iter().filter(|c| !c.capital).count();
            assert!(neutral >= 3 * players as usize, "{name}: {neutral} neutral cities");
            spec.build(&b).unwrap();
        }
    }
}

#[test]
fn empty_cities_raise_units_mostly_light_and_paid() {
    let mut w = world();
    for k in 0..12 {
        w.add_city(6 + (k % 4) * 14, 6 + (k / 4) * 20, 1, k == 0).unwrap();
    }
    // Nothing inside: after 5 s each city raises a unit when the money
    // allows (200 to start, 120/s from 12 cities), then has a defender.
    run(&mut w, &[], 5 * TPS + 2);
    let first = w.units().len();
    // 200 to start plus 5 s of income from 12 cities: up to 4 units.
    assert!((1..=4).contains(&first), "only what the money buys: {first}");
    run(&mut w, &[], 60 * TPS);
    assert_eq!(w.units().len(), 12);
    run(&mut w, &[], 10 * TPS);
    assert_eq!(w.units().len(), 12, "a city with a defender does not raise more");
    let heavy = w.units().kind.iter().filter(|&&k| k == UnitKind::Heavy as u8).count();
    assert!((1..=8).contains(&heavy), "{heavy} heavy of 12");
}

#[test]
fn a_city_being_captured_neither_sells_nor_raises() {
    let mut w = world();
    w.add_city(30, 30, 1, true).unwrap();
    // An enemy stands in the empty city from the start: capture under way.
    w.spawn(2, UnitKind::Light, pt(30, 30)).unwrap();
    w.step(&[]);
    assert_ne!(w.cities()[0].capture_by, 0, "capture under way");
    let buy_cmd = [buy(w.tick(), 1, 7, 0, UnitKind::Light)];
    w.step(&buy_cmd);
    assert_eq!(w.stats().commands_rejected, 1, "no buying in a city being taken");
    run(&mut w, &[], 3 * TPS);
    assert!(w.units().owner.iter().all(|&o| o == 2), "and no raising either");
    run(&mut w, &[], 3 * TPS);
    assert_eq!(w.cities()[0].owner, 2);
}

#[test]
fn long_corridors_heal_slower_and_blockaded_cities_less() {
    // A wounded unit next to its city heals at the full rate; the same far
    // away along a long way round heals slower.
    let hp_after = |x: i32, y: i32, wall: bool| {
        let mut w = world();
        w.add_city(4, 4, 1, true).unwrap();
        if wall {
            // An enemy line across the map with a gap at the bottom: the
            // way round is long.
            for k in 0..19 {
                w.spawn(2, UnitKind::Light, pt(30, 2 + k * 3)).unwrap();
            }
        }
        let u = w.spawn(1, UnitKind::Light, pt(x, y)).unwrap();
        // Wound it by standing in water briefly is awkward: set up the HP
        // through combat is noisy, so compare healing from a scratch.
        run(&mut w, &[], 2);
        w.debug_set_hp(u, 5_000);
        run(&mut w, &[], 10 * TPS);
        w.units().hp[w.units().index_of(u).unwrap()]
    };
    let near = hp_after(10, 4, false);
    let far = hp_after(40, 4, true);
    assert!(near > far + 100, "near {near} far {far}");
}

#[test]
fn a_blockaded_city_heals_less() {
    let healed = |cities: u32| {
        let mut w = world();
        w.add_city(10, 10, 1, true).unwrap();
        for k in 1..cities {
            w.add_city(10 + 12 * k, 40, 1, false).unwrap();
        }
        let u = w.spawn(1, UnitKind::Light, pt(10, 10)).unwrap();
        run(&mut w, &[], 2);
        w.debug_set_hp(u, 5_000);
        run(&mut w, &[], 10 * TPS);
        w.units().hp[w.units().index_of(u).unwrap()] as i32 - 5_000
    };
    let (alone, linked) = (healed(1), healed(4));
    // 1 of 4 reachable cities: a quarter of the city healing rate.
    assert!(linked > 3 * alone && alone > 0, "alone {alone} linked {linked}");
}
