# Iron Dots

A minimalist real-time strategy game in Rust. Armies are dots on an
operational map, territory shifts as they move, cities pay for new troops, and
encircled armies starve. The goal of the project is a simulation that handles
**100 000 units on a 4096² map** at a fixed 30 ticks per second. It is fully
deterministic, which is what makes lockstep multiplayer and tiny replays
possible.

Free and open source (MIT OR Apache-2.0). The full spec (in Russian) is in
[`docs/spec-ru.md`](docs/spec-ru.md).

![A match in progress](docs/screenshot.png)

## Status

**Stage 1: simulation prototype** ✓
fixed-point SoA core, spatial grid, movement, combat with morale, replays, golden hashes on 4 platforms.

**Stage 2: playable version** ✓ (spec §8)

- [x] Cities and capitals, money (income, upkeep, purchases), territory painting and borders
- [x] Supply per pocket (connected territory), starvation, healing, city capture, victory (capital + 80% of cities)
- [x] A* pathfinding around mountains and costly terrain (per unit type), with a per-tick budget
- [x] Bots (`ai` crate) with three difficulty levels; bot-vs-bot matches play to victory in CI
- [x] wgpu renderer: tile map, territory tint, borders, instanced units and cities, paths, selection
- [x] Client (winit + egui): main menu, HUD, city panel, controls from spec §2.9, pause/speed, end-of-match charts, replays saved to `replays/`

**Stage 3: parallelism** ✓

- [x] rayon over fixed 1024-unit blocks in every per-unit phase; the hash is identical on 1–16 threads (tested)
- [x] Sleeping chunks: no combat or enemy searches where no hostile units are near
- [x] Sparse spatial grid (only occupied 32×32-cell blocks), territory painting spread over 4 ticks
- [x] Per-phase profiling in `bench`, optional Tracy spans (`--features tracy`)
- 100 000 units on 4096²: 24 ms → 8 ms per tick with 4 threads (see [ARCHITECTURE.md](ARCHITECTURE.md#performance-stage-3-parallel-still-scalar))

**Stage 4: SIMD** ✓

- [x] `simd-kernels` crate: auto-vectorised integer kernels compiled for SSE4.2 / AVX2 / AVX-512 / NEON, picked once at start-up
- [x] Scalar reference kernels; property tests prove bit-identical results at every level; Miri in CI
- [x] Neighbour searches run on contiguous grid-ordered slices; units are re-sorted spatially every 8 ticks
- [x] The hash is identical at every SIMD level (tested) and on 1–16 threads
- Reference PC (6 cores / 12 threads, AVX2): 10 000 units 0.56 ms marching, 0.98 ms with all of them fighting on one front, 100 000 units 3.12 ms. All spec 3 tick targets met; hashes identical on Windows and Linux ([details](ARCHITECTURE.md#performance-stage-4-parallel--simd))
- Measure any machine with `cargo run --release -p headless -- scaling` (Markdown table, 1…N threads)

**Stage 5: pathfinding and scale** ✓ — 100 000 units: 6.8 ms per tick even on a 4-core PC (target ≤ 8 ms on the 12-thread reference)

- [x] `pathing` crate: chunk portal graph per unit type, shared coarse fields per target, direction fields per chunk where units are
- [x] Units follow fields around mountains, lakes and costly terrain; formations kept on open ground; A* removed from commands
- [x] Fields computed in tick phase 3 with a fixed budget (deterministic), unused ones collected
- [x] Sandbox in the menu: maps up to 4096², up to 8 sides × 12 500 units, no upkeep or supply limits
- [x] Rendering LOD: density markers when zoomed far out, on-screen culling

**Stage 6: multiplayer** ✓ — 20 matches 3v3 over a network with 10% packet loss and 100 ms jitter, one player dropping out and rejoining in each, ended in sync

- [x] Deterministic lockstep: only commands travel, the relay server stamps them with a tick, every client simulates the same ticks
- [x] `relay-server` (QUIC via quinn, bitcode messages): lobbies with slots, teams and bots; dedicated binary `iron-dots-relay`
- [x] Adaptive input delay of 2–6 ticks from measured jitter; catch-up after hiccups; the game waits for a player who falls 5 s behind
- [x] Reconnect: the client redials and re-simulates the match from the command history
- [x] Desync detector: state hashes every 30 ticks compared by the relay; on mismatch each client saves a dump (replay with all its hashes) to `desyncs/`
- [x] LAN without a server: "Host LAN game" runs the relay inside the game and announces it to the local network
- [x] `headless netsim`: the stage exit test (QUIC through a lossy UDP proxy, 6 bot-driven clients)

Next: stage 7, content and polish.

## Play

```bash
cargo run --release -p client
```

Or jump straight into a match:

```bash
cargo run --release -p client -- --play --map 448x256 --opponents 1 --difficulty normal
```

Or the sandbox (spec 2.7), 8 sides × 12 500 units on the largest map:

```bash
cargo run --release -p client -- --play --sandbox --map 4096x2304 --opponents 7 --units 12500
```

| Action | Input |
|---|---|
| Select / add to selection | LMB / Shift + LMB, drag for a box |
| All units of that type on screen | double LMB |
| Move (shortest path) / along a drawn path | RMB click / RMB drag; Shift queues |
| Stop / clear queued orders | S / C |
| Buy light / heavy unit in the selected city | Q / W (click your city first) |
| Groups | Ctrl + 1–9 store, 1–9 select, twice to jump |
| Camera | wheel zoom to cursor, MMB drag, screen edges, Home = capital |
| Pause / speed | Space / + and − (×0.5 … ×4) |
| Menu | Esc |
| Full screen / window | F11 or Alt+Enter (starts full screen; `--windowed` to start in a window) |

**Online.** Main menu → Online. Either type a server address and Connect, or
press *Host LAN game*: your game becomes the server and appears under "Games
on your network" for everyone on the same LAN (UDP ports 27960 and 27961).
The host fills slots with players or bots, sets teams and starts once all
players are ready. In a network match there is no pause or speed control,
and the HUD shows the current input delay. If the host of a LAN game quits,
the match ends for everyone; a dedicated server avoids that:

```bash
cargo run --release -p relay-server -- --port 27960
```

**Maps.** Ten built-in maps (4 duels, 3 for 2v2, 2 for 3v3, one 4-player
free-for-all), all symmetric so no start is better than another, or a random
generated map of any size. Pick them in the main menu or when creating an
online lobby; your own maps work online too (the map travels with the lobby).

**Map editor** (main menu → Map editor): terrain brushes, neutral cities and
start positions with teams, mirror / rotate / four-way symmetry, undo and redo
(Ctrl+Z / Ctrl+Y), generate from a seed, import a PNG (colours matched to
terrain; or drop a `.png` or `.wmap` onto the window), save, and Play to try
the map against bots at once. Maps are saved as `.wmap` next to
`settings.ron`, in `maps/`.

**Territory and supply.** Land belongs to whoever has units next to it right
now, or cities within 15 tiles; land between two or more of your cities with
no enemy city near is your rear and is yours too; land nobody holds, or both sides contest, is neutral.
Supply flows over your own and neutral land, so to cut an enemy off you
must surround it with a closed ring of your own control and keep standing
there. The shortest supply way from each army to a city shows as a strip of
your colour; the longer it is, the slower your units heal (full up to 40
tiles, a quarter at 200). A city cut off from your other cities heals at a
quarter rate. A city left without your units for 5 s raises one at the usual price
(two out of three light), if you have the money and the supply. A city that
is being captured cannot raise or buy units. Keep a unit in each city: a city with a defender inside cannot be
captured, and units in cities cost no upkeep.

**Formations.** With two or more units selected, a bar at the bottom offers
Keep, Line, Staggered, Box, Column and Flank. Picking one lines the group up
facing the nearest enemy and is used for the following orders.

Generated maps are wide like the screen (close to 16:9): from Small 320×192 to
Colossal 4096×2304. Sides are multiples of the 64-tile chunk, so exact 16:9
starts at 1024×576. `--map` takes `WxH` or a single number for a square map.

All keys can be rebound in Settings (main menu or Esc), along with full
screen, VSync, FPS limit, interface scale, edge scrolling and camera/zoom
speed. Settings live in `%APPDATA%\IronDots\settings.ron` on Windows and
`~/.config/iron-dots/settings.ron` elsewhere. Arrow keys move the camera.

Keys are matched by physical position, so they work on any keyboard layout.
Linux needs a Vulkan or GL driver and `libxkbcommon-x11-0` (or Wayland) at runtime,
and `libasound2-dev` to build (sound). Without an audio device the game runs silent.

Sound is synthesised by the game itself (no audio files): order, selection,
purchase, losses in view, city captured or lost, capital under attack,
victory and defeat; a battle bed that grows with the fighting on screen; and
an ambient pad. Volumes are in Settings.

## Headless tools

```bash
cargo run --release -p headless -- match --map 256 --players 2 --difficulty hard,easy --out match.idr
```

```bash
cargo run --release -p headless -- verify match.idr
```

```bash
cargo run --release -p headless -- bench --map 4096 --units 12500 --players 8 --ticks 150 --threads 4
```

```bash
cargo run --release -p client -- --screenshot frame.png --map 256 --ticks 4500
```

```bash
cargo run --release -p headless -- netsim --matches 20
```

```bash
cargo run --release -p headless -- aieval --difficulty hard,normal --matches 100 --map 320x192 --ticks 45000
```

The screenshot command renders a bot-vs-bot frame without a window (CI uses
Mesa's software Vulkan driver). `netsim` plays networked 3v3 matches through
a proxy that drops 10% of packets and adds up to 100 ms of jitter, and fails
on any desync. Balance lives in [`assets/balance.ron`](assets/balance.ron);
pass `--balance FILE` to the headless runner to try another one.

## Workspace

| Crate | What it is |
|---|---|
| `crates/simd-kernels` | SIMD kernels with scalar references and runtime CPU dispatch (the only `unsafe`) |
| `crates/pathing` | Hierarchical flow fields: chunk portal graph, coarse and chunk fields |
| `crates/sim-core` | Deterministic simulation: world, tick phases, commands, flow-field steering, replays |
| `crates/ai` | Bots: read the world, issue ordinary commands; deterministic |
| `crates/render` | wgpu renderer: map, territory, borders, instanced markers, overlay |
| `crates/net` | Protocol, framing, QUIC client, lockstep scheduling, LAN discovery, network emulator |
| `crates/relay-server` | Lobbies, tick clock, desync detection, rejoin; `iron-dots-relay` binary; `netsim` harness |
| `crates/client` | `iron-dots`: window, input, egui UI, online menu, offscreen screenshots |
| `crates/headless` | `iron-dots-headless`: matches, replays, determinism checks, benchmarks |

A later stage adds `editor`. See [ARCHITECTURE.md](ARCHITECTURE.md).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). The one rule that matters most is that
`sim-core` (and `ai`) must stay deterministic.

## License

Code: [MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE), at your option.
Assets and maps: CC BY 4.0.
