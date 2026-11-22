//! `iron-dots-headless` — the simulation without a window.
//!
//! ```text
//! iron-dots-headless record [opts] --out FILE   play scripted players, save a replay
//! iron-dots-headless verify FILE                re-simulate a replay, check every hash
//! iron-dots-headless hash   [opts]              print the final state hash
//! iron-dots-headless bench  [opts]              tick timings: p50 / p99 / max
//! iron-dots-headless match  [opts] [--out FILE] bots play a standard match to the end
//! iron-dots-headless scaling [--threads MAX]    thread-scaling table (Markdown) for the
//!                                               spec 3 targets, up to all hardware threads
//! iron-dots-headless aieval --difficulty A,B [--matches N]
//!                                               bot A vs bot B, sides swapped every
//!                                               match: win rates (spec 8.1, AI levels)
//! iron-dots-headless mapgen --map WxH --seed N --out FILE.wmap SYMMETRY LAYOUT [CITIES] [NAME]
//!                                               fair generated map: SYMMETRY mirror|rotate|quad,
//!                                               LAYOUT starts of one part as x,y in ‰ ("150,500;...")
//!       --trace (match): one line of cities, units, money, supply per minute
//!       --mapfile FILE (match, aieval): play on a .wmap map with its suggested teams
//! iron-dots-headless netsim [--matches N]       3v3 matches over QUIC through a proxy with
//!                                               10% loss and 100 ms jitter, one rejoin per
//!                                               match; fails on any desync (spec 6.2)
//!
//! opts: --map N or WxH (tiles, default 256)  --units N (per player, default 200)
//!       --players N (default 2)       --seed N (default 1)
//!       --ticks N (default 1800)      --balance FILE (default: built-in)
//!       --threads N (bench; default 1, results are identical for any N)
//!       --scenario battle|front (armies around the map / one long front line)
//!       --difficulty easy|normal|hard (match; default normal, or a list per
//!       player like easy,hard)
//!       --matches N --parallel N --tick-rate N --clean (netsim; defaults 20, 4,
//!       30, impaired network)
//! ```

use sim_core::replay::Replay;
use sim_core::scenario::{Controller, ScenarioSpec, ScriptedPlayer};
use sim_core::{Balance, Phase, World};
use std::process::ExitCode;
use std::time::{Duration, Instant};

struct Opts {
    map: u32,
    map_h: u32,
    units: u32,
    players: u8,
    seed: u64,
    ticks: u32,
    balance: Option<String>,
    out: Option<String>,
    difficulty: Vec<ai::Difficulty>,
    threads: usize,
    scenario: String,
    matches: u32,
    parallel: u32,
    tick_rate: u32,
    clean: bool,
    tune: String,
    mapfile: Option<String>,
    trace: bool,
    positional: Vec<String>,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        map: 256,
        map_h: 256,
        units: 200,
        players: 2,
        seed: 1,
        ticks: 1800,
        balance: None,
        out: None,
        difficulty: Vec::new(),
        threads: 1,
        scenario: "battle".into(),
        matches: 20,
        parallel: 4,
        tick_rate: 30,
        clean: false,
        tune: String::new(),
        mapfile: None,
        trace: false,
        positional: Vec::new(),
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| it.next().cloned().ok_or_else(|| format!("{name} needs a value"));
        let num = |s: String| s.parse::<u64>().map_err(|_| format!("not a number: {s}"));
        match a.as_str() {
            "--map" => {
                // N for a square map, or WxH.
                let v = val("--map")?;
                let (w, h) = v.split_once('x').unwrap_or((&v, &v));
                o.map = num(w.to_string())? as u32;
                o.map_h = num(h.to_string())? as u32;
            }
            "--units" => o.units = num(val("--units")?)? as u32,
            "--players" => o.players = num(val("--players")?)? as u8,
            "--seed" => o.seed = num(val("--seed")?)?,
            "--ticks" => o.ticks = num(val("--ticks")?)? as u32,
            "--balance" => o.balance = Some(val("--balance")?),
            "--out" => o.out = Some(val("--out")?),
            "--scenario" => o.scenario = val("--scenario")?,
            "--matches" => o.matches = num(val("--matches")?)? as u32,
            "--parallel" => o.parallel = num(val("--parallel")?)?.max(1) as u32,
            "--tick-rate" => o.tick_rate = num(val("--tick-rate")?)?.clamp(1, 240) as u32,
            "--clean" => o.clean = true,
            "--tune" => o.tune = val("--tune")?,
            "--mapfile" => o.mapfile = Some(val("--mapfile")?),
            "--trace" => o.trace = true,
            "--threads" => o.threads = num(val("--threads")?)?.max(1) as usize,
            "--difficulty" => {
                o.difficulty = val("--difficulty")?
                    .split(',')
                    .map(|d| match d {
                        "easy" => Ok(ai::Difficulty::Easy),
                        "normal" => Ok(ai::Difficulty::Normal),
                        "hard" => Ok(ai::Difficulty::Hard),
                        _ => Err(format!("unknown difficulty {d}")),
                    })
                    .collect::<Result<_, _>>()?
            }
            s if s.starts_with("--") => return Err(format!("unknown option {s}")),
            s => o.positional.push(s.to_string()),
        }
    }
    Ok(o)
}

fn balance(o: &Opts) -> Result<Balance, String> {
    match &o.balance {
        None => Ok(Balance::default_balance()),
        Some(path) => {
            let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
            Balance::from_ron(&text).map_err(|e| format!("{path}: {e}"))
        }
    }
}

fn scenario(o: &Opts) -> Result<ScenarioSpec, String> {
    match o.scenario.as_str() {
        "battle" => ScenarioSpec::battle(o.map, o.map_h, o.units, o.players, o.seed),
        "front" => ScenarioSpec::front(o.map, o.units, o.seed),
        s => Err(format!("unknown scenario {s} (battle or front)")),
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let Some(cmd) = args.first() else {
        return Err(usage());
    };
    let o = parse(&args[1..])?;
    let b = balance(&o)?;
    match cmd.as_str() {
        "record" => {
            let out = o.out.clone().ok_or("record needs --out FILE")?;
            let r = Replay::record_scripted(&scenario(&o)?, &b, o.ticks)?;
            let bytes = r.encode();
            std::fs::write(&out, &bytes).map_err(|e| format!("{out}: {e}"))?;
            println!(
                "wrote {out}: {} ticks, {} commands, {} bytes, final hash {:016x}",
                r.ticks,
                r.commands.len(),
                bytes.len(),
                r.final_hash().unwrap_or(0)
            );
        }
        "verify" => {
            let path = o.positional.first().ok_or("verify needs a FILE")?;
            let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
            let r = Replay::decode(&bytes)?;
            let t = Instant::now();
            let w = r.verify(&b).map_err(|e| e.to_string())?;
            println!(
                "ok: {} ticks, {} checkpoints, final hash {:016x}, {} units left, {:.2?}",
                r.ticks,
                r.checkpoints.len(),
                w.state_hash(),
                w.units().len(),
                t.elapsed()
            );
        }
        "hash" => {
            let r = Replay::record_scripted(&scenario(&o)?, &b, o.ticks)?;
            println!("{:016x}", r.final_hash().unwrap_or(0));
        }
        "bench" => bench(&o, &b)?,
        "scaling" => scaling(&o, &b)?,
        "match" => play_match(&o, &b)?,
        "netsim" => netsim(&o)?,
        "aieval" => aieval(&o, &b)?,
        "mapgen" => mapgen(&o)?,
        _ => return Err(usage()),
    }
    Ok(())
}

/// Result of running one scenario.
struct Measured {
    mean: Duration,
    p50: Duration,
    p99: Duration,
    max: Duration,
    phases: [Duration; Phase::ALL.len()],
    peak_combat: u32,
    units_left: usize,
    hash: u64,
}

/// Runs `spec` for `ticks` with scripted players on `threads` threads and
/// times every tick and phase (outside the simulation: it never reads a clock).
fn measure(spec: &ScenarioSpec, b: &Balance, threads: usize, ticks: u32) -> Result<Measured, String> {
    let mut world: World = spec.build(b)?;
    world.set_threads(threads)?;
    let mut bots: Vec<ScriptedPlayer> = (1..=world.player_count()).map(ScriptedPlayer::new).collect();
    let mut times: Vec<Duration> = Vec::with_capacity(ticks as usize);
    let mut peak_combat = 0;
    let mut phases = [Duration::ZERO; Phase::ALL.len()];
    let mut cmds = Vec::new();
    for _ in 0..ticks {
        cmds.clear();
        for bot in &mut bots {
            cmds.extend(bot.think(&world));
        }
        let t = Instant::now();
        let mut last = t;
        world.step_with(&cmds, &mut |p| {
            let now = Instant::now();
            phases[p as usize] += now - last;
            last = now;
        });
        times.push(t.elapsed());
        peak_combat = peak_combat.max(world.stats().units_in_combat);
    }
    times.sort();
    let pct = |p: usize| times[(times.len() - 1) * p / 100];
    let n = ticks.max(1);
    Ok(Measured {
        mean: times.iter().sum::<Duration>() / n,
        p50: pct(50),
        p99: pct(99),
        max: times.last().copied().unwrap_or_default(),
        phases: phases.map(|d| d / n),
        peak_combat,
        units_left: world.units().len(),
        hash: world.state_hash(),
    })
}

fn bench(o: &Opts, b: &Balance) -> Result<(), String> {
    let spec = scenario(o)?;
    let start_units = spec.spawns.len();
    let m = measure(&spec, b, o.threads, o.ticks)?;
    println!(
        "map {map}x{map}, {start_units} units ({} left), {} ticks, {} threads, simd {}, peak in combat {}",
        m.units_left,
        o.ticks,
        o.threads,
        simd_kernels::kernels().level().name(),
        m.peak_combat,
        map = o.map
    );
    println!("tick: mean {:.3?}  p50 {:.3?}  p99 {:.3?}  max {:.3?}", m.mean, m.p50, m.p99, m.max);
    let total: Duration = m.phases.iter().sum();
    let mut line = String::from("phases:");
    for p in Phase::ALL {
        let d = m.phases[p as usize];
        let pct = d.as_nanos() * 100 / total.as_nanos().max(1);
        line += &format!("  {} {:.2?} ({pct}%)", p.name(), d);
    }
    println!("{line}");
    println!("final hash {:016x}", m.hash);
    Ok(())
}

fn cpu_name() -> String {
    if let Ok(info) = std::fs::read_to_string("/proc/cpuinfo") {
        if let Some(l) = info.lines().find(|l| l.starts_with("model name")) {
            return l.split(':').nth(1).unwrap_or("").trim().to_string();
        }
    }
    std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_else(|_| "unknown CPU".into())
}

/// Thread-scaling table for the spec 3 targets: the three standard
/// scenarios on 1, 2, 4, … threads up to every hardware thread, best of
/// `repeats` runs each. Prints Markdown, ready to paste into an issue.
fn scaling(o: &Opts, b: &Balance) -> Result<(), String> {
    let hw = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let max_threads = if o.threads > 1 { o.threads } else { hw };
    let mut counts: Vec<usize> =
        std::iter::successors(Some(1usize), |&t| Some(t * 2)).take_while(|&t| t < max_threads).collect();
    counts.push(max_threads);
    let repeats = 3;
    let scenarios: [(&str, ScenarioSpec, u32); 3] = [
        ("10k units, 1024², marching", ScenarioSpec::battle(1024, 1024, 5000, 2, 1)?, 600),
        ("10k units, 1024², one front", ScenarioSpec::front(1024, 5000, 1)?, 300),
        ("100k units, 4096², 8 players", ScenarioSpec::battle(4096, 4096, 12_500, 8, 1)?, 150),
    ];
    println!("# Iron Dots scaling");
    println!();
    println!(
        "CPU: {} · {hw} hardware threads · SIMD level: {} · sim version {}",
        cpu_name(),
        simd_kernels::kernels().level().name(),
        sim_core::SIM_CODE_VERSION
    );
    println!();
    print!("| Scenario |");
    for t in &counts {
        print!(" {t} thr |");
    }
    println!(" Speed-up |");
    print!("|---|");
    for _ in &counts {
        print!("---|");
    }
    println!("---|");
    let mut hashes = Vec::new();
    for (name, spec, ticks) in &scenarios {
        let mut row = Vec::new();
        for &t in &counts {
            let mut best: Option<Measured> = None;
            for _ in 0..repeats {
                let m = measure(spec, b, t, *ticks)?;
                if best.as_ref().is_none_or(|bm| m.mean < bm.mean) {
                    best = Some(m);
                }
            }
            let m = best.expect("at least one run");
            hashes.push((name.to_string(), t, m.hash));
            row.push(m.mean);
            eprintln!("{name}: {t} threads → {:.3?}", m.mean);
        }
        print!("| {name} |");
        for d in &row {
            print!(" {:.2} ms |", d.as_secs_f64() * 1000.0);
        }
        let speedup = row[0].as_secs_f64() / row.last().copied().unwrap_or(row[0]).as_secs_f64();
        println!(" {speedup:.1}× |");
    }
    println!();
    // Every thread count must give the same final state.
    for (name, spec_hashes) in scenarios.iter().map(|(n, _, _)| (n, hashes.iter().filter(move |h| h.0 == **n))) {
        let mut hs: Vec<u64> = spec_hashes.map(|h| h.2).collect();
        hs.dedup();
        let verdict = if hs.len() == 1 { "identical on all thread counts" } else { "DIFFERS between thread counts" };
        println!("- {name}: final hash {:016x}, {verdict}", hs[0]);
    }
    println!();
    println!(
        "Spec 3 targets for 10 000 units on 1024²: ≤ 1 ms (6 cores / 12 threads, AVX2), ≤ 3 ms (4 cores, SSE4.2)."
    );
    Ok(())
}

/// The match scenario: a `.wmap` file with its suggested teams, or a
/// generated standard match.
fn match_spec(o: &Opts, b: &Balance, players: u8, seed: u64) -> Result<ScenarioSpec, String> {
    match &o.mapfile {
        Some(path) => {
            let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
            let f = sim_core::mapfile::MapFile::decode(&bytes)?;
            let slots: Vec<Option<u8>> = f.teams.iter().take(players as usize).map(|&t| Some(t)).collect();
            ScenarioSpec::from_map_file(&bytes, &slots, b)
        }
        None => ScenarioSpec::standard_match(o.map, o.map_h, players, seed, b),
    }
}

fn play_match(o: &Opts, b: &Balance) -> Result<(), String> {
    let spec = match_spec(o, b, o.players, o.seed)?;
    let mut bots: Vec<Box<dyn Controller>> = (1..=o.players)
        .map(|p| {
            let d = o.difficulty.get(p as usize - 1).or(o.difficulty.last()).copied().unwrap_or(ai::Difficulty::Normal);
            Box::new(ai::Bot::new(p, d)) as Box<dyn Controller>
        })
        .collect();
    let t = Instant::now();
    let mut winner = None;
    let trace = o.trace;
    let r = Replay::record(&spec, b, o.ticks, &mut bots, |w| {
        if trace && w.tick() % (60 * b.tick_rate) == 0 {
            let counts = w.unit_counts();
            let mut line = format!("{:>2} min:", w.tick() / b.tick_rate / 60);
            for p in 1..=w.player_count() {
                let cities = w.cities().iter().filter(|c| c.owner == p).count();
                let (used, limit) = w.supply_totals(p);
                let u = w.units();
                let mine = (0..u.len()).filter(|&i| u.owner[i] == p);
                let (mut idle, mut fight) = (0, 0);
                for i in mine {
                    idle += (u.order[i] == sim_core::unit::NO_ORDER) as u32;
                    fight += (u.flags[i] & sim_core::unit::flags::IN_COMBAT != 0) as u32;
                }
                line += &format!(
                    "  p{p} cities {cities:>2} units {:>3} (idle {idle:>3} fighting {fight:>3}) money {:>5} supply {used}/{limit}",
                    counts[p as usize],
                    w.player(p).money / 1000
                );
            }
            println!("{line}");
        }
        winner = w.winner();
        winner.is_some()
    })?;
    let secs = r.ticks / b.tick_rate;
    match winner {
        Some(team) => println!("team {team} won after {}:{:02} game time", secs / 60, secs % 60),
        None => println!("no winner after {}:{:02} game time", secs / 60, secs % 60),
    }
    println!(
        "{} commands, final hash {:016x}, simulated in {:.2?}",
        r.commands.len(),
        r.final_hash().unwrap_or(0),
        t.elapsed()
    );
    if let Some(out) = &o.out {
        std::fs::write(out, r.encode()).map_err(|e| format!("{out}: {e}"))?;
        println!("wrote {out}");
    }
    Ok(())
}

fn net_bot(player: u8, difficulty: u8) -> Box<dyn Controller + Send> {
    let d = match difficulty {
        0 => ai::Difficulty::Easy,
        1 => ai::Difficulty::Normal,
        _ => ai::Difficulty::Hard,
    };
    Box::new(ai::Bot::new(player, d))
}

/// One bot-vs-bot match on one thread: winning player (1 or 2) or `None`,
/// and the game time in seconds.
fn duel(spec: &ScenarioSpec, b: &Balance, ticks: u32, p: [ai::Profile; 2]) -> Result<(Option<u8>, u32), String> {
    let mut w = spec.build(b)?;
    w.set_threads(1)?;
    let mut bots = [
        ai::Bot::with_profile(1, ai::Difficulty::Normal, p[0]),
        ai::Bot::with_profile(2, ai::Difficulty::Normal, p[1]),
    ];
    let mut cmds = Vec::new();
    while w.tick() < ticks && w.winner().is_none() {
        cmds.clear();
        for bot in &mut bots {
            cmds.extend(bot.think(&w));
        }
        w.step(&cmds);
    }
    // Teams equal player ids in a standard match.
    Ok((w.winner(), w.tick() / b.tick_rate))
}

/// Bot A against bot B, `--matches` games on different maps with sides
/// swapped every game. Prints A's win rate; draws count as not won.
fn aieval(o: &Opts, b: &Balance) -> Result<(), String> {
    let [a, bd] = match o.difficulty[..] {
        [a, b] => [a, b],
        _ => return Err("aieval needs --difficulty A,B".into()),
    };
    let mut pa = a.profile();
    pa.tune(&o.tune)?;
    let pb = bd.profile();
    let next = std::sync::atomic::AtomicU32::new(0);
    let results = std::sync::Mutex::new(Vec::new());
    let t = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..o.parallel.max(1) {
            scope.spawn(|| {
                loop {
                    let k = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if k >= o.matches {
                        return;
                    }
                    // Each map is played twice, once from each side.
                    let seed = o.seed + (k / 2) as u64;
                    let swap = k % 2 == 1;
                    let r = match_spec(o, b, 2, seed)
                        .and_then(|spec| duel(&spec, b, o.ticks, if swap { [pb, pa] } else { [pa, pb] }));
                    let a_won = r.as_ref().ok().and_then(|(w, _)| *w).map(|w| (w == 1) != swap);
                    let secs = r.as_ref().map(|r| r.1).unwrap_or(0);
                    results.lock().expect("lock").push((k, a_won, secs));
                }
            });
        }
    });
    let mut r = results.into_inner().expect("lock");
    r.sort_by_key(|x| x.0);
    let wins = r.iter().filter(|x| x.1 == Some(true)).count();
    let losses = r.iter().filter(|x| x.1 == Some(false)).count();
    let draws = r.len() - wins - losses;
    let avg = r.iter().filter(|x| x.1.is_some()).map(|x| x.2).sum::<u32>() / (wins + losses).max(1) as u32;
    println!(
        "{a:?}{} vs {bd:?} on {}x{}: {wins} won, {losses} lost, {draws} undecided after {} min of {} ({}%), average decided match {}:{:02}, {:.0?}",
        if o.tune.is_empty() { String::new() } else { format!(" [{}]", o.tune) },
        o.map,
        o.map_h,
        o.ticks / b.tick_rate / 60,
        r.len(),
        wins * 100 / r.len().max(1),
        avg / 60,
        avg % 60,
        t.elapsed()
    );
    Ok(())
}

/// Writes a fair generated `.wmap` map (see `sim_core::mapfile::symmetric`).
fn mapgen(o: &Opts) -> Result<(), String> {
    use sim_core::mapfile::{Symmetry, symmetric};
    let out = o.out.clone().ok_or("mapgen needs --out FILE.wmap")?;
    let [sym, layout, rest @ ..] = &o.positional[..] else {
        return Err("mapgen needs SYMMETRY and LAYOUT".into());
    };
    let sym = match sym.as_str() {
        "mirror" => Symmetry::Mirror,
        "rotate" => Symmetry::Rotate,
        "quad" => Symmetry::Quad,
        s => return Err(format!("unknown symmetry {s}")),
    };
    let layout = layout
        .split(';')
        .map(|p| {
            let (x, y) = p.split_once(',').ok_or("layout points are x,y")?;
            Ok((x.trim().parse().map_err(|_| "bad x")?, y.trim().parse().map_err(|_| "bad y")?))
        })
        .collect::<Result<Vec<(u32, u32)>, &str>>()?;
    let cities = rest.first().map_or(Ok(5), |c| c.parse()).map_err(|_| "bad city count")?;
    let mut f = symmetric(o.map, o.map_h, o.seed, sym, &layout, cities)?;
    f.name = rest.get(1).cloned().unwrap_or_else(|| "Untitled".into());
    f.author = "Iron Dots".into();
    let bytes = f.encode();
    std::fs::write(&out, &bytes).map_err(|e| format!("{out}: {e}"))?;
    println!(
        "wrote {out}: {}x{}, {} starts (teams {:?}), {} neutral cities, {} bytes",
        f.width,
        f.height,
        f.starts.len(),
        f.teams,
        f.cities.len(),
        bytes.len()
    );
    Ok(())
}

/// Networked 3v3 matches (spec 8, stage 6 exit criterion).
fn netsim(o: &Opts) -> Result<(), String> {
    use relay_server::netsim::{self, Options};
    let network = if o.clean { "clean network" } else { "10% loss, 100 ms jitter" };
    println!(
        "{} matches 3v3, map {}×{}, {} ticks at {} ticks/s, {network}, one rejoin per match",
        o.matches, o.map, o.map_h, o.ticks, o.tick_rate
    );
    let next = std::sync::atomic::AtomicU32::new(0);
    let failures = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..o.parallel.min(o.matches) {
            scope.spawn(|| {
                loop {
                    let k = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if k >= o.matches {
                        return;
                    }
                    let seed = o.seed + k as u64;
                    let mut opts = Options::three_vs_three(seed, o.ticks);
                    opts.width = o.map;
                    opts.height = o.map_h;
                    opts.tick_rate = o.tick_rate;
                    if o.clean {
                        opts.network = None;
                    }
                    let line = match netsim::run(&opts, net_bot) {
                        Ok(r) => {
                            let ok = r.desync.is_none() && !r.relay_desync && r.ticks == o.ticks && r.rejoined;
                            if !ok {
                                failures.lock().expect("lock").push(k + 1);
                            }
                            format!(
                                "match {:>2} seed {seed}: {} ticks, {} hash checks, {}, rejoin {}, stalls {}, {:.0} s{}",
                                k + 1,
                                r.ticks,
                                r.checkpoints,
                                match (&r.desync, r.relay_desync) {
                                    (None, false) => "in sync".to_string(),
                                    (Some((t, _)), _) => format!("DESYNC at tick {t}"),
                                    (None, true) => "DESYNC reported by relay".to_string(),
                                },
                                if r.rejoined { "ok" } else { "MISSING" },
                                r.stalls,
                                r.seconds,
                                r.winner.map_or(String::new(), |w| format!(", team {w} won")),
                            )
                        }
                        Err(e) => {
                            failures.lock().expect("lock").push(k + 1);
                            format!("match {:>2} seed {seed}: FAILED: {e}", k + 1)
                        }
                    };
                    println!("{line}");
                }
            });
        }
    });
    let failures = failures.into_inner().expect("lock");
    if failures.is_empty() {
        println!("all {} matches in sync", o.matches);
        Ok(())
    } else {
        Err(format!("matches failed: {failures:?}"))
    }
}

fn usage() -> String {
    "usage: iron-dots-headless <record|verify|hash|bench|match|scaling|netsim|aieval|mapgen> [--map N] [--units N] [--players N] \
     [--seed N] [--ticks N] [--balance FILE] [--out FILE] [FILE]"
        .into()
}

/// With `--features tracy`, sends tick-phase spans to the Tracy profiler.
fn init_profiling() {
    #[cfg(feature = "tracy")]
    {
        use tracing_subscriber::layer::SubscriberExt;
        let subscriber = tracing_subscriber::registry().with(tracing_tracy::TracyLayer::default());
        if tracing::subscriber::set_global_default(subscriber).is_err() {
            eprintln!("profiling: a tracing subscriber is already installed");
        }
    }
}

fn main() -> ExitCode {
    init_profiling();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
