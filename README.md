# Iron Dots

A minimalist real-time strategy game in Rust: armies are dots on an operational
map, territory shifts as they move, and cities pay for new troops. The goal of
the project is a simulation that handles **100 000 units on a 4096² map** at a
fixed 30 ticks per second. It is fully deterministic, which is what makes
lockstep multiplayer and tiny replays possible.

Free and open source (MIT OR Apache-2.0). The full spec (in Russian) is in
[`docs/spec-ru.md`](docs/spec-ru.md).

## Status

**Stage 1 — simulation prototype** (spec §8):

- [x] Fixed-point (Q16.16) SoA simulation core, no floats, no `unsafe`
- [x] Spatial grid (counting sort), soft separation, movement along drawn paths with formation offsets and Shift-queue
- [x] Terrain multipliers, impassable mountains, water damage, units turning into ships
- [x] Combat by the formula in §2.8: attacker / defender, morale, damage split between enemies in contact, broken attacks
- [x] Commands as the only input; replays (scenario + commands + hash every 30 ticks)
- [x] Headless runner: record / verify / hash / bench
- [x] Golden replays and a CI matrix comparing hashes on Linux, Windows, macOS, ARM64

Next: stage 2 (rendering, cities, territory, supply, pathfinding, simple AI).

## Quick start

```bash
cargo test --workspace
```

```bash
cargo run --release -p headless -- bench --map 1024 --units 5000 --players 2 --ticks 600
```

```bash
cargo run --release -p headless -- record --map 256 --units 300 --players 4 --ticks 3000 --out match.idr
```

```bash
cargo run --release -p headless -- verify match.idr
```

Balance lives in [`assets/balance.ron`](assets/balance.ron); pass
`--balance FILE` to the runner to try a different one.

## Workspace

| Crate | What it is |
|---|---|
| `crates/sim-core` | Deterministic simulation: world, tick, commands, replays |
| `crates/headless` | `iron-dots-headless` CLI: replays, determinism checks, benchmarks |

More crates (`render`, `client`, `net`, `ai`, `pathing`, `simd-kernels`,
`relay-server`, `editor`) arrive in later stages. See [ARCHITECTURE.md](ARCHITECTURE.md).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). The one rule that matters most is that
`sim-core` must stay deterministic.

## License

Code: [MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE), at your option.
Assets and maps: CC BY 4.0.
