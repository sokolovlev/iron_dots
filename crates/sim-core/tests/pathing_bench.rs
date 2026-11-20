//! Timing of the pathfinding graph on a 4096² generated map (ignored by
//! default; run with `cargo test --release -p sim-core --test pathing_bench -- --ignored --nocapture`).

#![allow(clippy::disallowed_types, clippy::disallowed_methods)] // timing a test, not the simulation

use sim_core::map::Map;
use sim_core::pathfind::terrain_costs;
use sim_core::{Balance, Rules, UnitKind};
use std::time::Instant;

#[test]
#[ignore]
fn graph_build_time() {
    for size in [1024, 4096] {
        let map = Map::generate(size, size, 1).unwrap();
        let rules = Rules::new(&Balance::default_balance());
        let costs = terrain_costs(&rules, UnitKind::Light);
        let t = pathing::Terrain { width: size, height: size, tiles: map.terrain_plane(), costs: &costs };
        let start = Instant::now();
        let g = pathing::Graph::build(&t);
        let built = start.elapsed();
        let goal = g.goal_of(size / 2, size / 2).or_else(|| g.goal_of(size / 2 + 3, size / 2)).expect("land");
        let start = Instant::now();
        let c = g.coarse(goal);
        let coarse = start.elapsed();
        let mut out = vec![0u8; pathing::CHUNK_TILES];
        let start = Instant::now();
        for k in 0..64u32 {
            g.chunk_field(&t, k % (size / 64), k / (size / 64), &c, None, &mut out);
        }
        let fields = start.elapsed() / 64;
        println!(
            "{size}²: {} portal nodes, build {built:.2?} ({} threads), coarse field {coarse:.2?}, chunk field {fields:.2?}",
            g.nodes().len(),
            rayon::current_num_threads()
        );
    }
}
