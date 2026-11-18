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
| 1 | Apply this tick's commands, sorted by `(player, seq)`; Move plans an A* route | 1–2 ✓ |
| 2 | Rebuild the spatial grid (counting sort) | 1 ✓ |
| 3 | Pathfinding: hierarchical flow fields | 5 |
| 4 | Intent: desired velocity from the current order | 1 ✓ |
| 5 | Separation + integration + terrain blocking; waypoints, water/ships | 1 ✓ |
| 6 | Combat: contacts → pulled damage (combat, water, starvation) → HP and morale | 1–2 ✓ |
| 7 | Territory painting | 2 ✓ |
| 8 | Supply: pockets every 15 ticks, starvation rates, healing | 2 ✓ |
| 9 | Cities: capture; once per second money and the victory check | 2 ✓ |
| 10 | Remove the dead (descending swap-remove), then spawn bought units | 1–2 ✓ |
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

## Gameplay rules: decisions the spec left open

Stage 2 had to pin down details that spec §2 describes only in words:

- **Territory:** a unit claims tiles within `territory.radius` unless an enemy
  is within that radius of the tile. Allied land and city tiles are never
  repainted; a city tile always belongs to the city's owner.
- **Pockets:** 4-connected components of one player's land. A unit belongs to
  the pocket under it, or the nearest own tile within the territory radius.
  Otherwise it has no supply and starves at the full rate. Units bought this
  tick already count against the pocket.
- **Upkeep and income** are settled once per second. A unit is "in a city"
  (no upkeep, double healing) inside the city's 3×3 tiles.
- **Healing** needs 3 s out of combat, no starvation, friendly land, and not
  being in water (otherwise healing cancelled the water damage).
- **Capture:** enemy units inside the 3×3 area with no defenders. Any
  defender resets the progress; attackers from two hostile teams freeze it.
- **Sandbox** (benchmarks, spec §2.7): no upkeep and no supply limits, so
  pocket labelling is skipped entirely.
- **Pathfinding:** A* only when the straight segment would cross mountains
  or terrain costlier than its endpoints. The search window is the bounding
  box of start and goal grown by the route length (max 512 tiles per side),
  with at most 20 000 nodes per search and 200 000 per tick. Commands beyond
  the budget walk straight. Waypoints inserted by A* are followed exactly;
  the player's own points keep the formation offset.

## Bots

`crates/ai` reads the world and returns ordinary commands, so the same bots
run on every client in a lockstep game. It follows the same determinism rules
(integers, ordered containers; a `clippy.toml` enforces it). Strategy is
simple: buy when money and supply allow, pick the nearest neutral or enemy
city (weighted by how well it is defended), defend threatened cities, and pull
back units with broken morale. Difficulty changes reaction time, defense
awareness, tactics and cash reserve.

Measured in self-play on 256² maps, 20 matches per pairing with sides swapped:
Normal beats Easy 13/20, while Hard beats Normal 9/20 and Easy 10/20. Making
each level reliably stronger (spec v1.0 target: ≥ 70%) is stage 7 work.

## Rendering

`crates/render` draws everything in three pipelines:

1. **Map:** a single fullscreen triangle. Terrain and owner planes are two
   `R8Uint` textures. The fragment shader colours tiles, tints territory and
   draws borders where a neighbour has another owner. Dirty 64×64 chunks are
   re-uploaded as one contiguous 4 KiB copy each (the map stores chunks
   row-major).
2. **Markers:** units and cities as instanced quads (16 bytes each). Shapes
   (dot, heavy ring, ship diamond, city, capital star), HP and morale bars,
   selection ring and combat tremble are drawn in the shader.
3. **Overlay:** coloured triangles in tile space for paths and the selection box.

The client interpolates unit positions between the last two ticks. Colours
are authored in sRGB and converted for the sRGB swapchain. Player colours
come from a colour-blind-safe palette (`render::palette`).

## Balance

`assets/balance.ron` (spec §2.8) is validated on load (range checks protect
against hostile balance mods). Its content hash is part of `sim_version`, so
peers with different balance cannot play together, and a replay refuses to
run against a different balance.

One deliberate interpretation of the spec: the per-terrain "atk" multiplier is
a **strength** multiplier applied to both attack and defense. With attack only,
a heavy unit (base defense 1.5) would still beat a light one in forest and
hills, which contradicts spec §2.3 ("slow and weak").

## Performance (stage 2, scalar, single thread)

Measured with `iron-dots-headless bench` (sandbox battle, scripted players)
on a 4-core cloud VM:

| Scenario | Mean tick | p99 |
|---|---|---|
| 10 000 units, 1024² map | 4.5 ms | 6.6 ms |
| 100 000 units, 4096² map, 8 players | 40 ms | 62 ms |
| 1 200 units, 256² map, 4 players | 0.7 ms | 1.4 ms |

In normal (non-sandbox) matches, pocket labelling floods the whole map every
15 ticks. That is fine up to 1024² but is replaced by union-find over the chunk
graph in stage 5.

The targets (≤ 1 ms and ≤ 8 ms on the reference PC) belong to stages 3–5:
rayon over fixed-size blocks, sleeping chunks, Morton re-sorting, SIMD kernels,
and a sparse grid for huge maps (the dense 4096² grid alone has 4 M cells).
