//! A 3v3 over real QUIC through a lossy, jittery proxy, with a rejoin:
//! all six clients must end with identical state hashes.

use relay_server::netsim::{self, Options};
use sim_core::scenario::Controller;

fn bot(player: u8, difficulty: u8) -> Box<dyn Controller + Send> {
    let d = match difficulty {
        0 => ai::Difficulty::Easy,
        1 => ai::Difficulty::Normal,
        _ => ai::Difficulty::Hard,
    };
    Box::new(ai::Bot::new(player, d))
}

#[test]
fn three_vs_three_on_a_bad_network() {
    let mut o = Options::three_vs_three(11, 900);
    o.tick_rate = 60;
    o.width = 256;
    o.height = 128;
    let r = netsim::run(&o, bot).expect("match runs");
    eprintln!("{r:?}");
    assert_eq!(r.ticks, 900);
    assert!(r.rejoined);
    assert_eq!(r.checkpoints, 30);
    assert_eq!(r.desync, None);
    assert!(!r.relay_desync);
}

#[test]
fn bots_in_slots_run_on_every_client() {
    let mut o = Options::three_vs_three(5, 600);
    o.teams = vec![1, 2];
    o.bots = vec![(1, 1), (2, 2)];
    o.tick_rate = 120;
    o.width = 256;
    o.height = 128;
    o.network = None;
    o.reconnect = false;
    let r = netsim::run(&o, bot).expect("match runs");
    assert_eq!(r.desync, None);
    assert_eq!(r.checkpoints, 20);
}
