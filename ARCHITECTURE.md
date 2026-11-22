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
| 3 | Pathfinding: flow fields requested last tick, fixed budget | 5 ✓ |
| 4 | Intent: desired velocity from the current order | 1 ✓ |
| 5 | Separation + integration + terrain blocking; waypoints, water/ships | 1 ✓ |
| 6 | Combat: contacts → pulled damage (combat, water, starvation) → HP and morale | 1–2 ✓ |
| 7 | Control zones (per chunk, every 4 ticks) | 2 ✓ |
| 8 | Supply: pockets every 15 ticks, starvation rates, healing | 2 ✓ |
| 9 | Cities: capture; once per second money and the victory check | 2 ✓ |
| 10 | Remove the dead (descending swap-remove), then spawn bought units | 1–2 ✓ |
| 11 | State hash (xxh3) | 1 ✓ |

The grid cell is a power of two no smaller than the contact radius plus twice
the maximum per-tick move. Cells are grouped in 32×32 blocks and only occupied
blocks get prefix sums, so a mostly empty 4096² map costs no more than the
area its armies cover. A grid built from start-of-tick positions is
therefore still valid for the combat phase after movement, so the grid is
rebuilt only once per tick.

## Parallelism (stage 3)

Every phase that touches all units runs on a rayon pool owned by the `World`
(`World::set_threads`). The rules that keep it deterministic:

- **Fixed blocks.** Units are processed in blocks of `BLOCK = 1024` indices.
  The block size never depends on the thread count, and per-block results
  (counters, released orders, territory writes) are merged in block order.
- **Pull model.** A unit reads neighbours' start-of-phase state and writes
  only its own slots. Arrays being written are moved out of the world for
  the phase (`std::mem::take`), so the borrow checker proves there is no
  shared mutable state. There are no atomics and no locks.
- **Shared structures are touched sequentially, in unit order.** These are
  order-pool refcounts (waypoint transitions, broken attacks) and territory
  writes. Territory candidates are found in parallel from the start-of-phase
  map, then applied in unit order with the ally check repeated. This gives
  exactly the sequential result.

The parallel code reproduced the previous single-threaded hashes bit for bit.
`hash_is_independent_of_thread_count` compares every 30-tick checkpoint on 1,
2, 3, 4, 8 and 16 threads (battle with deaths), and a full match with
buying and supply on 1, 4 and 7 threads.

**Sleeping chunks (spec 5.3).** After movement, each 64×64 chunk records which
teams have units in it. A chunk is *hot* if two hostile teams are in it or its
8 neighbours. Elsewhere no unit can have an enemy within contact (1.5 tiles)
or territory (7 tiles) range, so the combat contact search and the territory
enemy search are skipped. This skips only work that would find nothing, so
results are unchanged.

**Spread over ticks.** Each unit paints territory every 4th tick, staggered by
slot (`territory::PAINT_INTERVAL`). This is the one stage 3 change to game
results (`SIM_CODE_VERSION` 3).

**Dirty flags.** Territory changes mark chunks dirty. The renderer re-uploads
only those chunks, and pockets are re-labelled only when territory changed.

**Profiling.** `World::step_with` calls a probe after every phase;
`iron-dots-headless bench` prints the per-phase split from it. Building
`headless` or `client` with `--features tracy` adds a `tracing` span per
phase for the Tracy profiler.

## SIMD (stage 4)

`crates/simd-kernels` holds the vector code and is the only crate allowed
`unsafe` (three calls through function pointers, each with a SAFETY comment;
Miri runs its tests in CI).

- **Kernels:** `within` (points strictly within a radius → 64-bit hit mask),
  `enemies_within` (same, other team only), and `count_eq` (bytes equal to a
  value, for territory statistics).
- **One body, many targets.** Each kernel is plain integer Rust, written so
  LLVM auto-vectorises it (fixed 16-lane groups, no early exits), and is
  compiled per level with `#[target_feature]`: SSE4.2, AVX2, AVX-512 and NEON.
  Differences are taken in `i32` (coordinates < 2²⁸), so squares use the
  32×32→64-bit vector multiply. No intrinsics were needed (spec 5.5 rule 3).
  The assembly was checked for `ymm`/`zmm` registers and `vpmuldq`.
- **Scalar references** in `simd_kernels::reference` define the results
  (written with `i128`). Property tests compare every available level with
  them bit for bit.
- **Dispatch once.** `simd_kernels::kernels()` picks the level at start-up from
  CPUID (or `IRON_DOTS_SIMD=scalar|sse4.2|avx2|avx512|neon`). Calls go
  through a table of function pointers, with no per-call checks.
  `World::set_simd_level` overrides it in tests.
- **Data the kernels can scan.** The grid keeps x, y and team copies in grid
  order, so a row of cells is a contiguous slice. Per-unit queries run in
  grid order: units of one cell share one neighbourhood buffer (≈ 60 units,
  padded to whole 16-lane groups with far-away points), so each unit is
  one long kernel call. Results are written in grid order and read back
  through `grid.rank`.
- **Spatial sorting (spec 5.1).** Every 8 ticks the unit arrays are permuted
  into grid order. Neighbours on the map become neighbours in memory; this
  costs O(n) because the grid order is already spatial.
- **Fewer divisions.** Exact integer square roots use `u64::isqrt` (about 2×
  faster than the old Newton loop; same results). The damage formula is
  folded to one division per contact pair, with the attacker's part
  precomputed once per tick. This and the sorting changed results
  (`SIM_CODE_VERSION` 4).
- **Where SIMD lost, it is not used.** The territory enemy check stays scalar:
  its early exit beat the kernel (2.3 vs 2.8 ms in the front-line bench).

`hash_is_independent_of_simd_level` runs a battle and a front line at every
level the CPU supports and requires identical checkpoints.

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

- **Control zones** (changed after playtesting): a tile belongs to a side
  while that side has a unit within `territory.radius` (3) of it, one of its
  cities within `territory.city_radius` (15), or lies in its rear, and no
  other side does. The rear is land within `territory.rear_radius` (30) of
  two or more cities of one team and of no city of another team, so the
  land behind the front is solid colour without grey gaps. City zones and
  rears form a separate plane, recomputed (in parallel per chunk) only when
  a city is founded or changes hands.
  Land nobody reaches, or that two hostile sides reach, is neutral. Zones
  follow the units: land is not kept after the army leaves, so an
  encirclement lasts only while the encircling units stay. City tiles always
  show the city owner. Each chunk is recomputed every 4 ticks (a 2×2
  checkerboard): units mark their tiles, each marked tile spreads over the
  radius disk; owner from a 256-entry table of the players present. A pure
  function of positions, so thread count cannot change it.
- **Pockets:** per team, 4-connected components of everything not held by
  another team (own, allied and neutral land), found with row runs and
  union-find. So supply is cut only by a closed ring of enemy control; an
  army far from its cities is supplied as long as neutral land connects it.
  Supply limits are counted per player per pocket. Units bought this tick
  already count against the pocket.
- **Formations** (`Move { formation }`): Keep (current spacing), Line
  (about 4:1, a few ranks), Staggered (line with gaps), Box, Column, Flank
  (two wings). Facing is from the group's centre to the first path point;
  the units furthest forward fill the front rank, left to right, so paths
  rarely cross (`sim_core::formation`).
- **Game modes:** *war of attrition* (the default: start from the
  capitals, empty cities raise units without end) and *limited war*
  (`ScenarioSpec::limited_war(n)`): every neutral city goes to the player
  whose capital is nearest, so each side starts with its half of the map and
  a front between; every city gets a light defender; each team may create
  `n` units in the whole match (`World::unit_budget`), counting purchases
  and units raised by empty cities alike. A player with no units and nothing left to
  create is out. Lobbies carry the mode (`LobbySettings::unit_budget`).
- **Positions** (client helper, `client::fronts`): the player draws a curve;
  places every 1.6 tiles along it (none on impassable tiles) are taken by
  the selected units, or the nearest own units not in combat. Once a second
  empty places (unit dead, ordered elsewhere by the player, or left idle far
  from it) are given to idle units standing in own cities. It only issues
  ordinary Move commands, so it is not part of the simulation and works
  online unchanged.
- **Supply corridors** (playtesting): every 15 ticks each team gets a
  distance field (4×4-tile cells, BFS) from its cities over cells with no
  tile held by another team. From every unit the shortest way back to a
  city is traced (ways merge into a tree) and painted as the team's land,
  4 tiles wide, on neutral tiles only: a corridor never contests land units
  hold, or it could reopen the ring that cut it. Healing outside cities is
  multiplied by the corridor share: 100% up to `supply.corridor_full` (40
  tiles), falling linearly to `supply.corridor_min` (25%) at
  `supply.corridor_max` (200 tiles) and beyond. Units cut off completely
  (no city in their pocket) starve as before.
- **Blockaded cities:** healing in a city is multiplied by
  min(1, n / `supply.blockade_cities`) where n is the number of the owner's
  cities in that city's pocket (itself included): a city cut off from all
  others heals at a quarter rate, but still supplies its pocket.
- **City spawn:** an owned city with none of its owner's units inside for
  `city.spawn_time` (5 s), and nobody capturing it, raises a unit at the
  usual price if the owner can pay and its pocket has supply to spare:
  heavy with probability `city.spawn_heavy` (0.34), otherwise light
  (deterministic RNG, drawn when the city becomes due). A city being
  captured neither raises units nor accepts purchases.
- **Solid bodies** (changed after playtesting): a unit is a disc 1.5 tiles
  across (`unit.separation_radius` is the centre distance of touching
  units); overlapping units are pushed apart by up to a quarter diameter per
  tick, and the renderer draws exactly that disc, so dots never overlap on
  screen. Units fight when their centres are within `combat.contact_radius`
  (1.75): touching. Starting armies and formation slots are spaced to fit.
- **Movement in contact:** a unit touching enemies can only move away from
  them. For each enemy it touches, its speed is multiplied by
  max(0, cos θ), θ being the angle between its heading and the direction
  straight away from that enemy (equivalently sin of the angle to the
  tangent); the smallest factor wins. Head-on or along the tangent it stays
  put, straight back it walks at the engaged speed. So an attacker cannot
  slide past a defender it has touched.
- **Upkeep and income** are settled once per second. A unit is "in a city"
  (no upkeep, double healing) inside the city's 3×3 tiles.
- **Healing** needs 3 s out of combat, no starvation, friendly land, and not
  being in water (otherwise healing cancelled the water damage).
- **Capture:** enemy units inside the 3×3 area with no defenders. Any
  defender resets the progress; attackers from two hostile teams freeze it.
- **Sandbox** (benchmarks, spec §2.7): no upkeep and no supply limits, so
  pocket labelling is skipped entirely.
- **Pathfinding** (see the next section): within 24 tiles of its target a
  unit walks straight if the line is clear (re-checked every 8 ticks);
  otherwise it follows a flow field, but keeps its own heading while the
  field direction is within 45° of it, so formations survive open ground.
  A unit following a field counts as stuck only if it does not move at all.

## Pathfinding (stage 5)

Hierarchical flow fields (spec 5.4), in the `pathing` crate (which knows
nothing about the simulation: it reads a chunk-major terrain plane and a cost
table) and the `sim_core::flow` module.

- **Portal graph**, one per unit type, built at match setup (parallel per
  chunk). Each 64×64 chunk is split into 4-connected passable components.
  With no corner cutting, 4-connectivity is exactly reachability. Runs of
  passable tile pairs across a chunk border (≤ 32 tiles, split where a
  component changes) become portals, one node per side. Nodes of one
  component are joined with their **exact** travel cost, from a local
  Dijkstra per portal. It uses a cyclic bucket queue as wide as the cheapest
  step (exact, linear time) on a padded 66×66 cost array.
- **Coarse field** per target region (chunk + component) and unit type: a
  reverse Dijkstra over the graph giving every portal its cost to the
  target and the next portal (a tree). It is shared by every unit going
  there.
- **Chunk field**: the direction byte per tile for one chunk, computed only
  where units stand. It is a multi-source Dijkstra seeded at the exits whose
  next hop in the tree leaves the chunk. Because the tree has no cycles,
  every border crossing lowers the cost to go and units never bounce
  between chunks. (The first version seeded every border tile with a
  lower-bound estimate; units oscillated at borders, which is why intra
  edges are exact now.)
- **Exact field** for the last stretch: inside the target's region, a chunk
  field seeded at the target tile.
- **Requests and budget.** A unit whose field is missing walks straight and
  files a request. Phase 3 of the next tick serves the requests sorted, at
  most 4 new coarse fields and 64 chunk/exact fields per tick, in parallel.
  When a field appears never depends on timing or threads. Every 150 ticks,
  coarse fields no unit uses are dropped. Above 8192 chunk fields (32 MiB),
  those in chunks without users are dropped too.

Measured by the project owner with sim version 6 (flow fields) on a
4-core / 4-thread Intel desktop with AVX2, a machine weaker than the spec 3
reference PC:

| Scenario | 1 thr | 2 thr | 4 thr | Speed-up |
|---|---|---|---|---|
| 10k units, 1024², marching | 2.96 ms | 1.55 ms | **0.97 ms** | 3.1× |
| 10k units, 1024², one front | 5.26 ms | 2.88 ms | **1.64 ms** | 3.2× |
| 100k units, 4096², 8 players | 21.63 ms | 11.98 ms | **6.82 ms** | 3.2× |

The stage 5 exit criterion (100 000 units, tick ≤ 8 ms on the reference
PC) is met even on this 4-core machine, and so is the weak-PC target (≤ 3 ms
for 10 000 units). Final hashes (`a0c767a5…`, `5aa56467…`, `ab879546…`)
were identical on Windows and on the Linux VM.

Costs on a 4096² generated map (4 threads, per unit type): graph 32 412
portals, built in ~1.1 s at setup; coarse field ~5 ms; chunk field ~0.4 ms.
On 1024²: 70 ms, 0.3 ms, 0.4 ms. Routes are within ~15% of the true
shortest path (hierarchy cost; tested on an open map). In the benchmarks
flow fields add about 8–9% to the tick, almost all of it the per-unit
lookups in the movement pass. Phase 3 itself is ~3%.

## Maps

`.wmap` (`sim_core::mapfile`): `"WMAP"`, a format version, then a zstd frame
(pure-Rust `ruzstd`) with the bitcode-encoded `MapFile`: name, author,
terrain in the map's chunk-major order, neutral cities, start positions and
the suggested team of each start. A 448×256 map is about 16 KB. A match on a
map carries the encoded file in its scenario (`MapSource::File`), so replays
and network games need nothing else; the scenario seed is the file's hash.
Starts without a player become neutral cities.

The built-in pool (`assets/maps`, compiled into the client) is made by
`headless mapgen`: noise terrain made symmetric (mirror for two sides facing
each other, 180° rotation, or four-way for free-for-all), starts and neutral
cities placed in one part and copied to the others. Starting armies are
placed on the side of the capital facing the map centre, so mirrored starts
get mirrored armies.

## Multiplayer (stage 6)

Deterministic lockstep (spec 6.2). Only player commands cross the network;
traffic does not depend on the number of units.

**Relay.** `relay-server` is the clock. Clients send `Commands { kinds }`;
the relay stamps them with the next tick and the sender's player id (a
client cannot act as someone else), and broadcasts one `TickPacket` per
tick to everyone in the game, an empty one if nobody did anything. A client
simulates tick `t` only once it holds packet `t`, so every client applies
the same commands at the same tick. Bots are not players on the network:
the `GameStart` message lists them and every client runs them locally, which
is deterministic. The server logic (`Relay`) is a pure state machine:
connections, messages and clock ticks in, messages out. Tests drive it
directly; the tokio/quinn layer around it is thin. The relay never simulates,
so one small server holds many games.

**Transport.** QUIC (quinn) with one reliable bidirectional stream per
client, u32-length-prefixed bitcode frames. QUIC retransmits lost packets,
so loss shows up as delay, which the jitter buffer absorbs. The server makes
a self-signed certificate at start-up; clients accept any certificate (the
link is encrypted but the server is not authenticated, fine for typed-in and
LAN addresses; official servers will use Steam tickets, spec 6.6). `Hello`
carries the protocol version and `sim_version` (code version + balance
hash); mismatches are refused.

**Input delay.** Commands take effect at the tick the relay is at when they
arrive, i.e. after one network trip plus the client's buffer. The client
keeps 2–6 ticks of packets in hand, sized from measured arrival jitter
(`net::lockstep`), and runs slightly faster or slower to stay at that
buffer, or at full speed after a hiccup or a rejoin. A player who falls more
than 150 ticks (5 s) behind pauses the game for everyone (`Stall`) until
they catch up; a player who disconnects does not.

**Desync detection.** Every 30 ticks each client sends its state hash. When
all connected players have reported a tick the relay compares them; agreed
hashes are kept (one u64 per second of play) so a rejoining client is
checked as it replays. On a mismatch the relay broadcasts `Desync`, and the
client writes `desyncs/game<id>-p<player>-t<tick>.idr`: a replay of the
match with every hash that client computed, so dumps from two machines show
exactly where they diverged.

**Rejoin.** The relay keeps the whole command history of each game (a few
bytes per tick). A client that lost its connection redials automatically,
sends `Rejoin { game, token }` and gets the scenario plus every packet so
far, then re-simulates at full speed (bounded by how fast the machine
simulates). While it catches up it is not waited for.

**LAN.** "Host LAN game" runs the same relay on a thread inside the client
and announces it by UDP broadcast on port 27961 once a second; other clients
list what they hear.

**Bad network test.** `relay_server::netsim` runs a relay, puts
`net::netem` in front of it (a UDP proxy that drops 10% of datagrams in both
directions and delays each by 20–120 ms, which also reorders them), and
connects six real QUIC clients on their own threads, each with its own world
and a hard bot playing its player through the network. One client drops out
at a third of the match and rejoins with a fresh world. At the end every
client's hash list must be identical. `headless netsim --matches 20` (1800
ticks each, 3v3 on 256²) passed 20 of 20 with no stall; a shorter run is in
CI, and a 900-tick match runs in `cargo test`.

## Bots

`crates/ai` reads the world and returns ordinary commands, so the same bots
run on every client in a lockstep game. It follows the same determinism rules
(integers, ordered containers; a `clippy.toml` enforces it).

Each think: buy as money and supply allow; pull back units with broken
morale; keep a garrison in every city; send idle units to own cities that are
outnumbered; then pick targets. Levels differ only in decision frequency and
depth (spec 6.3), set by `ai::Profile`:

| | Easy | Normal | Hard |
|---|---|---|---|
| Tactical decisions every | 4 s | 1 s | ⅓ s |
| Purchases and targets every | 4 s | 1 s | 1 s |
| Treasury kept | 400 | 0 | 0 |
| Targets | each unit its own nearest city | groups 1.2× the defenders | groups 2.5× the defenders |
| Garrisons, defence, retreat | no | yes | yes |
| Single raiders to undefended enemy cities | no | yes | yes |

With control zones tied to units (see above), a lone raider deep in enemy
land stays supplied, so undefended cities fall to single units: garrisons
(one unit per city, free of upkeep) are the answer, and attacking a
garrisoned city pays only with a large margin, because the defender has the
city bonus, takes less damage and loses morale four times slower. Hard's
edge is mostly that margin. Sending units off every third of a second made
bots weaker (they reached the target one at a time), hence the separate
strategy period.

Measured with `headless aieval` (100 matches per pairing, both sides of
each map, unseen seeds), with control zones but before solid bodies and
contact movement:

| Pairing | Small 320×192 | Medium 448×256 |
|---|---|---|
| Normal vs Easy | 86% | 89% |
| Hard vs Normal | 71% | 79% |

With solid bodies and contact movement (sim version 8) the bots are not
retuned yet: Normal vs Easy 49% / 57%, Hard vs Normal 64% / 63%. Matches
also last longer. With city rears and twice as many cities (version 9) the
ordering is off too, so the strength tests are `#[ignore]`d until bots are
retuned, which waits until the rules settle after playtesting. City spawns cost money since version 11, and
bot matches finish again.

A fixed 10-match sample runs in `cargo test` as a regression guard. Heavy
units still make bots weaker, so bots buy light units only.

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

The client interpolates unit positions between the last two ticks and
only builds markers for what is on screen. Zoomed out below 2 pixels per
tile (stage 5 LOD), units are aggregated into density markers about 6
pixels apart: one dot per cell in the colour of the side with most units
there, sized by the count, trembling if any of them fights. A 4096² map
with 100 000 units is then a few thousand instances instead of 100 000. Colours
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

## Performance (stage 4: parallel + SIMD)

Run `iron-dots-headless scaling` to get this table for any machine: the three
standard scenarios on 1, 2, 4, … up to every hardware thread, best of 3 runs,
plus a check that every thread count ends in the same state. The
**Scaling benchmark** workflow (Actions → Run workflow) runs it on any runner
label and puts the table in the job summary.

**Reference PC** (spec 3 "average PC": Intel 12th gen, 6 cores / 12 threads,
AVX2, Windows), sim version 4, measured by the project owner:

| Scenario | 1 thr | 2 thr | 4 thr | 8 thr | 12 thr | Speed-up |
|---|---|---|---|---|---|---|
| 10k units, 1024², marching | 1.71 ms | 1.05 ms | 0.74 ms | 0.64 ms | **0.57 ms** | 3.0× |
| 10k units, 1024², one front | 3.73 ms | 2.33 ms | 1.59 ms | 1.26 ms | **1.08 ms** | 3.5× |
| 100k units, 4096², 8 players | 13.11 ms | 8.13 ms | 4.99 ms | 3.74 ms | **3.55 ms** | 3.7× |

The final hashes on Windows were identical to those on the Linux VM, which
confirms cross-platform determinism on real hardware. On 12 threads the
front-line tick split as follows: combat 456 µs, territory 261 µs,
separation 208 µs, grid 121 µs, move 83 µs.

Changes after that measurement, aimed at the front line (sim version 5):

- **Painting staggered by chunk, not by unit.** Each tick a quarter of the
  chunks (a 2×2 checkerboard) paint. Presence planes (chunk plus a margin of
  the radius) are stamped only for hot painting chunks, roughly a quarter of
  the previous stamping. Territory: 1.8 → 1.1 ms single-threaded on the VM.
- **Row spans for stamping.** One exact square root per row gives the span of
  tiles inside the circle (tested against the per-tile distance check).
- **Fast exact square root.** `simd_kernels::isqrt` takes the hardware `f64`
  estimate and corrects it with integer arithmetic. It is bit-identical to
  `u64::isqrt` (property-tested) and was 10% of the front-line profile before.
- **Smaller parallel blocks** (1024 → 256 units, painting 256). With 10 000
  units, 1024-unit blocks gave only 10 work items: on 12 threads two threads
  always idled and the rest were unevenly loaded. Results do not depend on
  the block size (merges follow unit order).

Tried and dropped: folding the damage formula to one division per defender
(no measurable gain; combat cost is the neighbour search, not divisions).

The same reference PC with sim version 5:

| Scenario | 1 thr | 2 thr | 4 thr | 8 thr | 12 thr | Speed-up |
|---|---|---|---|---|---|---|
| 10k units, 1024², marching | 1.50 ms | 0.99 ms | 0.70 ms | 0.57 ms | **0.56 ms** | 2.7× |
| 10k units, 1024², one front | 3.27 ms | 2.09 ms | 1.30 ms | 1.05 ms | **0.98 ms** | 3.3× |
| 100k units, 4096², 8 players | 12.50 ms | 7.27 ms | 4.62 ms | 3.47 ms | **3.12 ms** | 4.0× |

Final hashes (`4d514b28…`, `4139d8f7…`, `6e6d0aea…`) were again identical on
Windows and on the Linux VM.

Measured here on a 4-vCPU cloud VM (Xeon 2.1 GHz, AVX-512 level):

| Scenario | 1 thr | 2 thr | 4 thr | Speed-up |
|---|---|---|---|---|
| 10k units, 1024², marching | 2.39 ms | 1.55 ms | 1.26 ms | 1.9× |
| 10k units, 1024², one front | 5.07 ms | 3.42 ms | 2.45 ms | 2.1× |
| 100k units, 4096², 8 players | 16.50 ms | 10.55 ms | 6.47 ms | 2.6× |

For comparison, stage 3 (scalar, no sorting) measured 2.90 / 7.25 / 23.9 ms on
1 thread and 1.27 / 2.77 / 7.99 ms on 4 threads. The VM is noisy: repeated runs
vary by about ±10%.

What brought the 4-thread numbers down after the first SIMD version:

- **One pass for movement.** Intent, walking, terrain blocking, waypoint
  progress, committing positions and water handling run as a single
  per-unit parallel pass after the separation pass, instead of four. This
  means fewer fork/joins and fewer sweeps over memory. Results are
  bit-identical.
- **Cheaper re-sorting.** Units are permuted in parallel (large worlds), and
  the grid is updated in place afterwards instead of being rebuilt. Cell
  computation and the grid-ordered copies run in parallel for large worlds.
  Below 32k units these stay sequential, because forking cost more than it
  saved.
- **Territory at the front.** Near hostile units (hot chunks and their
  neighbours), every tile first gets the team bits of all units within the
  territory radius. This is stamped in parallel per 64×64 chunk with the
  same distance test as before. A painting unit then checks a tile with one
  lookup instead of scanning every nearby enemy. Same results; territory
  dropped from 2.2 to 1.7 ms single-threaded in the front-line case.

Tried and dropped because the benchmark showed no gain: 16×16-cell grid
blocks (and they change the unit order, i.e. results), and collecting
enemies for territory with the SIMD kernel.

Against the spec 3 targets for 10 000 units on 1024²:

- **Weak PC (4 cores, SSE4.2): ≤ 3 ms.** Met on this VM: 1.3 ms marching,
  2.5 ms on a full front.
- **Average PC (6 cores / 12 threads, AVX2): ≤ 1 ms.** Met on the reference
  PC: 0.56 ms marching, 0.98 ms with all 10 000 units fighting on one front
  (the worst case, just under the limit). 100 000 units: 3.12 ms.

In normal (non-sandbox) matches, pocket labelling floods the whole map every
15 ticks. That is fine up to 1024² but is replaced by union-find over the chunk
graph in stage 5.
