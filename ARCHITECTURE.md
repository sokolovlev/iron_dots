# Architecture

## Simulation is a pure function

`sim-core` is a library with no IO, no clock and no floating point. Given the
same initial `ScenarioSpec`, balance and command stream, it produces a
bit-identical world on every OS, CPU, thread count and (later) SIMD level.
Lockstep networking, replays, server-side match verification and desync
detection all rely on this.

```
input ─► Command { tick, player, seq, kind } ─► World::step ─► state (SoA)
                                                     │
                                       state_hash() every 30 ticks
```

Commands are the **only** way to change the world, for players, bots and
replays alike.

## Tick phases (spec §4.3)

The order is fixed. Each phase only reads what earlier phases wrote, and every
unit writes only its own slot (the "pull" model), so phases can later run in
parallel without changing results.

| # | Phase | Stage |
|---|---|---|
| 1 | Apply this tick's commands, sorted by `(player, seq)` | 1 ✓ |
| 2 | Rebuild the spatial grid (counting sort) | 1 ✓ |
| 3 | Pathfinding: flow fields | 2 / 5 |
| 4 | Intent: desired velocity from the current order | 1 ✓ |
| 5 | Separation + integration + terrain blocking; waypoints, water/ships | 1 ✓ |
| 6 | Combat: contacts → pulled damage → HP and morale | 1 ✓ |
| 7 | Territory painting | 2 |
| 8 | Supply (pockets), starvation, healing | 2 |
| 9 | Cities: production and capture | 2 |
| 10 | Remove the dead (descending swap-remove), spawn | 1 ✓ |
| 11 | State hash (xxh3) | 1 ✓ |

The grid cell is a power of two no smaller than the contact radius plus twice
the maximum per-tick move. A grid built from start-of-tick positions is
therefore still valid for the combat phase after movement, so the grid is
rebuilt only once per tick.

## Data layout

- `UnitStore`: one `Vec` per field (positions `i32` Q16.16, HP and morale
  `u16` in hundredths, owner, kind, flags, order...). Units are reached from
  outside through generational `UnitHandle`s, because array indices change on
  swap-remove.
- `Map`: separate byte planes (terrain, owner), stored chunk-major in 64×64
  chunks (4 KiB per plane per chunk).
- Orders: a ref-counted pool of immutable paths (≤ 32 points). All units given
  one command share it, and each unit adds its own formation offset.
  Shift-queue builds chains through `next` (copy-on-write).
- `Rules`: balance converted to integer per-tick values and small lookup tables
  `[terrain][kind]`.

## Determinism rules

Enforced by `crates/sim-core/clippy.toml`, `#![deny(clippy::float_arithmetic)]`,
`#![forbid(unsafe_code)]`, and CI comparing hashes across platforms.

1. **No `f32`/`f64`** in `sim-core`. Use `fx::Fx` (Q16.16) or integers in fixed
   units. Balance decimals are written as strings (`"9.2"`) and parsed exactly
   into `Milli`.
2. **No `HashMap`/`HashSet` iteration.** Use `Vec` or `BTreeMap`.
3. **No wall clock, no thread identity, no OS randomness.** Randomness comes
   from `rng::rand(seed, tick, unit_slot, purpose)`, a counter-based hash
   with no shared state.
4. **Order-independent accumulation.** A unit sums contributions from
   neighbours with integer addition, which is associative, so visiting order
   never matters. Fractional per-tick amounts use deterministic stochastic
   rounding (`rng::stochastic_round_q16`).
5. **Deterministic structural changes.** Commands are sorted, deaths are
   removed in descending index order, and slots and order ids are reused LIFO.
6. **Anything that changes outcomes bumps `SIM_CODE_VERSION`.** The golden
   replays in `crates/sim-core/tests/determinism.rs` fail otherwise.

Rendering and UI may use floats freely, since they never feed back into the
simulation.

## Balance

`assets/balance.ron` (spec §2.8) is validated on load (range checks protect
against hostile balance mods). Its content hash is part of `sim_version`, so
peers with different balance cannot play together, and a replay refuses to
run against a different balance.

One deliberate interpretation of the spec: the per-terrain "atk" multiplier is
a **strength** multiplier applied to both attack and defense. With attack only,
a heavy unit (base defense 1.5) would still beat a light one in forest and
hills, which contradicts spec §2.3 ("slow and weak").

## Performance (stage 1, scalar, single thread)

Measured with `iron-dots-headless bench` on a 4-core cloud VM:

| Scenario | Mean tick |
|---|---|
| 10 000 units, 1024² map | ~4.2 ms |
| 100 000 units, 4096² map, 8 players | ~40 ms |

The targets (≤ 1 ms and ≤ 8 ms on the reference PC) belong to stages 3–5:
rayon over fixed-size blocks, sleeping chunks, Morton re-sorting, SIMD kernels,
and a sparse grid for huge maps (the dense 4096² grid alone has 4 M cells).
