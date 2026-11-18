//! Bots play complete standard matches (stage 2 exit criterion: a match
//! runs from start to victory), and their replays re-simulate exactly.

use ai::{Bot, Difficulty};
use sim_core::Balance;
use sim_core::replay::Replay;
use sim_core::scenario::{Controller, ScenarioSpec};

fn play(map: u32, players: u8, seed: u64, d: &[Difficulty]) -> (Replay, Option<u8>) {
    let b = Balance::default_balance();
    let spec = ScenarioSpec::standard_match(map, players, seed, &b).unwrap();
    let mut bots: Vec<Box<dyn Controller>> =
        (1..=players).map(|p| Box::new(Bot::new(p, d[(p as usize - 1) % d.len()])) as Box<dyn Controller>).collect();
    let mut winner = None;
    // 25 minutes of game time at most.
    let r = Replay::record(&spec, &b, 25 * 60 * 30, &mut bots, |w| {
        winner = w.winner();
        winner.is_some()
    })
    .unwrap();
    (r, winner)
}

#[test]
fn duel_reaches_victory_and_replays_exactly() {
    let (r, winner) = play(256, 2, 1, &[Difficulty::Normal]);
    assert!(winner.is_some(), "no winner after {} ticks", r.ticks);
    let minutes = r.ticks / 30 / 60;
    assert!((2..=20).contains(&minutes), "match lasted {minutes} min");
    let w = r.verify(&Balance::default_balance()).unwrap();
    assert_eq!(w.winner(), winner);
    // Golden hash: bots + simulation must give the same match on every
    // platform (CI runs this on Linux, Windows, macOS and ARM64). Update it
    // together with SIM_CODE_VERSION when gameplay or bots change on purpose.
    assert_eq!(r.final_hash(), Some(0xf1c1_1489_705a_a0c4), "golden AI match changed");
}

#[test]
fn ffa_reaches_victory() {
    let (r, winner) = play(256, 4, 2, &[Difficulty::Easy, Difficulty::Normal, Difficulty::Hard]);
    assert!(winner.is_some(), "no winner after {} ticks", r.ticks);
}

#[test]
fn every_difficulty_plays() {
    for d in Difficulty::ALL {
        let (r, winner) = play(128, 2, 3, &[d]);
        assert!(winner.is_some(), "{d:?} vs {d:?}: no winner after {} ticks", r.ticks);
    }
}
