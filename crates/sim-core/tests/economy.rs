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
    Command { tick, player, seq: 100, kind: CommandKind::Move { units: units.to_vec(), path: vec![to], queue: false } }
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
    w.claim(1, 10, 10, 5);
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
    w.claim(1, 10, 10, 5);
    run(&mut w, &[], 3 * 60 * TPS); // 1800 money
    let cmds: Vec<Command> = (0..8).map(|k| buy(w.tick(), 1, k, 0, UnitKind::Light)).collect();
    w.step(&cmds);
    // 5 places per city.
    assert_eq!(w.units().len(), 5, "money {} supply {:?} stats {:?}", w.player(1).money, w.city_supply(0), w.stats());
    assert_eq!(w.city_supply(0), Some((5, 5)));
    let heavy = [buy(w.tick(), 1, 99, 0, UnitKind::Heavy)];
    w.step(&heavy);
    assert_eq!(w.units().len(), 5);
}

#[test]
fn units_paint_territory_unless_enemies_are_near() {
    let mut w = world();
    let a = w.spawn(1, UnitKind::Light, pt(20, 20)).unwrap();
    w.step(&[]);
    assert_eq!(w.map().owner(20, 20), 1);
    assert_eq!(w.map().owner(23, 20), 1);
    assert_eq!(w.map().owner(24, 20), 0);
    // An enemy within the radius of a tile blocks repainting it.
    w.spawn(2, UnitKind::Light, pt(40, 20)).unwrap();
    w.step(&[]);
    assert_eq!(w.map().owner(40, 20), 2);
    let cmds = [mv(w.tick(), 1, &[a], pt(36, 20))];
    run(&mut w, &cmds, 10 * TPS);
    assert_eq!(w.map().owner(40, 20), 2, "tile next to the enemy flipped");
}

#[test]
fn encircled_units_starve() {
    let mut w = world();
    w.add_city(5, 5, 1, true).unwrap();
    w.claim(1, 5, 5, 4);
    // Group far away from any own city: its pocket has no supply.
    let lost = w.spawn(1, UnitKind::Light, pt(50, 50)).unwrap();
    let home = w.spawn(1, UnitKind::Light, pt(6, 6)).unwrap();
    run(&mut w, &[], 10 * TPS);
    let hp = |h| w.units().hp[w.units().index_of(h).unwrap()];
    assert_eq!(hp(home), 10_000);
    // 5 HP/s for ~10 s.
    assert!((4_500..=5_500).contains(&(10_000 - hp(lost) as i32)), "lost {}", 10_000 - hp(lost) as i32);
}

#[test]
fn overloaded_pocket_starves_proportionally() {
    let mut w = world();
    w.add_city(10, 10, 1, true).unwrap();
    w.claim(1, 10, 10, 8);
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
    w.claim(1, 10, 10, 8);
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
    let spec = ScenarioSpec::standard_match(256, 2, 3, &b).unwrap();
    let w = spec.build(&b).unwrap();
    assert_eq!(w.cities().iter().filter(|c| c.capital_of != 0).count(), 2);
    assert!(w.cities().len() >= 8, "only {} cities", w.cities().len());
    assert_eq!(w.units().len(), 6);
    assert_eq!(w.unit_counts()[1], 3);
}
