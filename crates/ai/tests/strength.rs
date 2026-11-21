//! Each difficulty must beat the one below it (spec 8.1: ≥ 70% over 100
//! matches, measured with `headless aieval`). This is a small, fixed sample
//! of that as a regression guard: deterministic, so not flaky.

use ai::{Bot, Difficulty};
use sim_core::Balance;
use sim_core::scenario::{Controller, ScenarioSpec};

/// Plays `a` vs `b` on `maps` maps from both sides; returns `a`'s wins.
fn wins(a: Difficulty, b: Difficulty, maps: u64) -> u32 {
    let bal = Balance::default_balance();
    let mut won = 0;
    for seed in 0..maps {
        for swap in [false, true] {
            let spec = ScenarioSpec::standard_match(320, 192, 2, 500 + seed, &bal).unwrap();
            let mut w = spec.build(&bal).unwrap();
            let (pa, pb) = if swap { (2, 1) } else { (1, 2) };
            let mut bots = [Bot::new(pa, a), Bot::new(pb, b)];
            while w.tick() < 25 * 60 * 30 && w.winner().is_none() {
                let cmds: Vec<_> = bots.iter_mut().flat_map(|bot| bot.think(&w)).collect();
                w.step(&cmds);
            }
            won += (w.winner() == Some(pa)) as u32;
        }
    }
    won
}

#[test]
fn normal_beats_easy() {
    let w = wins(Difficulty::Normal, Difficulty::Easy, 5);
    assert!(w >= 7, "normal won only {w} of 10 against easy");
}

#[test]
fn hard_beats_normal() {
    let w = wins(Difficulty::Hard, Difficulty::Normal, 5);
    assert!(w >= 6, "hard won only {w} of 10 against normal");
}
