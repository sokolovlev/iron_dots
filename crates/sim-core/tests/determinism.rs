//! Determinism and golden replays (spec 7). The golden hashes below are run
//! by CI on Linux, Windows and macOS (x86-64 and ARM64): identical values on
//! all of them are the exit criterion of stage 1.
//!
//! If you intentionally change game logic or balance: bump
//! `sim_core::SIM_CODE_VERSION`, run `cargo test -p sim-core --test
//! determinism -- --nocapture` and paste the new hashes printed on failure.

use sim_core::Balance;
use sim_core::replay::{Replay, VerifyError};
use sim_core::scenario::ScenarioSpec;

struct Golden {
    name: &'static str,
    map: u32,
    units: u32,
    players: u8,
    seed: u64,
    ticks: u32,
    hash: u64,
}

const GOLDEN: &[Golden] = &[
    Golden { name: "duel-128", map: 128, units: 200, players: 2, seed: 1, ticks: 2400, hash: 0x4c1d_cab8_a799_9768 },
    Golden { name: "ffa4-128", map: 128, units: 100, players: 4, seed: 7, ticks: 2400, hash: 0x08d2_7a31_e0f3_0448 },
];

#[test]
fn golden_replays() {
    let b = Balance::default_balance();
    let mut failures = Vec::new();
    for g in GOLDEN {
        let spec = ScenarioSpec::battle(g.map, g.units, g.players, g.seed).unwrap();
        let r = Replay::record_scripted(&spec, &b, g.ticks).unwrap();
        let w = r.verify(&b).unwrap();
        let start = (g.units * g.players as u32) as usize;
        assert!(w.units().len() < start, "{}: no unit died, the scenario tests nothing", g.name);
        let got = r.final_hash().unwrap();
        println!("{}: {got:#018x}", g.name);
        if got != g.hash {
            failures.push(format!("{}: expected {:#018x}, got {got:#018x}", g.name, g.hash));
        }
    }
    assert!(failures.is_empty(), "golden hashes changed (bump SIM_CODE_VERSION if intended):\n{}", failures.join("\n"));
}

#[test]
fn same_input_same_hashes() {
    let b = Balance::default_balance();
    let spec = ScenarioSpec::battle(128, 80, 3, 99).unwrap();
    let a = Replay::record_scripted(&spec, &b, 600).unwrap();
    let c = Replay::record_scripted(&spec, &b, 600).unwrap();
    assert_eq!(a.checkpoints, c.checkpoints);
    assert_eq!(a.checkpoints.len(), 20);
}

#[test]
fn replay_round_trip_and_desync_detection() {
    let b = Balance::default_balance();
    let spec = ScenarioSpec::battle(128, 60, 2, 5).unwrap();
    let r = Replay::record_scripted(&spec, &b, 900).unwrap();
    let decoded = Replay::decode(&r.encode()).unwrap();
    assert_eq!(decoded, r);
    decoded.verify(&b).unwrap();

    // A dropped command must be caught at the next checkpoint.
    let mut tampered = r.clone();
    let k = tampered.commands.len() / 2;
    let tick = tampered.commands.remove(k).tick;
    match tampered.verify(&b) {
        Err(VerifyError::Desync { tick: t, .. }) => assert!(t > tick && t <= tick + 30 + 1),
        other => panic!("expected desync, got {:?}", other.map(|w| w.tick())),
    }

    // A different balance is refused up front.
    let other = Balance::from_ron(&sim_core::balance::DEFAULT_BALANCE_RON.replace("\"9.2\"", "\"9.3\"")).unwrap();
    assert!(matches!(r.verify(&other), Err(VerifyError::WrongBalance { .. })));
    assert!(Replay::decode(b"nope").is_err());
}

#[test]
fn sim_version_tracks_balance() {
    let a = Balance::default_balance();
    let b = Balance::from_ron(&sim_core::balance::DEFAULT_BALANCE_RON.replace("\"9.2\"", "\"9.3\"")).unwrap();
    assert_ne!(sim_core::sim_version(&a), sim_core::sim_version(&b));
}
