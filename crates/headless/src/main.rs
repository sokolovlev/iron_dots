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
//!
//! opts: --map N (tiles, default 256)  --units N (per player, default 200)
//!       --players N (default 2)       --seed N (default 1)
//!       --ticks N (default 1800)      --balance FILE (default: built-in)
//!       --threads N (bench; default 1, results are identical for any N)
//!       --scenario battle|front (armies around the map / one long front line)
//!       --difficulty easy|normal|hard (match; default normal, or a list per
//!       player like easy,hard)
//! ```

use sim_core::replay::Replay;
use sim_core::scenario::{Controller, ScenarioSpec, ScriptedPlayer};
use sim_core::{Balance, Phase, World};
use std::process::ExitCode;
use std::time::{Duration, Instant};

struct Opts {
    map: u32,
    units: u32,
    players: u8,
    seed: u64,
    ticks: u32,
    balance: Option<String>,
    out: Option<String>,
    difficulty: Vec<ai::Difficulty>,
    threads: usize,
    scenario: String,
    positional: Vec<String>,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        map: 256,
        units: 200,
        players: 2,
        seed: 1,
        ticks: 1800,
        balance: None,
        out: None,
        difficulty: Vec::new(),
        threads: 1,
        scenario: "battle".into(),
        positional: Vec::new(),
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| it.next().cloned().ok_or_else(|| format!("{name} needs a value"));
        let num = |s: String| s.parse::<u64>().map_err(|_| format!("not a number: {s}"));
        match a.as_str() {
            "--map" => o.map = num(val("--map")?)? as u32,
            "--units" => o.units = num(val("--units")?)? as u32,
            "--players" => o.players = num(val("--players")?)? as u8,
            "--seed" => o.seed = num(val("--seed")?)?,
            "--ticks" => o.ticks = num(val("--ticks")?)? as u32,
            "--balance" => o.balance = Some(val("--balance")?),
            "--out" => o.out = Some(val("--out")?),
            "--scenario" => o.scenario = val("--scenario")?,
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
        "battle" => ScenarioSpec::battle(o.map, o.units, o.players, o.seed),
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
        ("10k units, 1024², marching", ScenarioSpec::battle(1024, 5000, 2, 1)?, 600),
        ("10k units, 1024², one front", ScenarioSpec::front(1024, 5000, 1)?, 300),
        ("100k units, 4096², 8 players", ScenarioSpec::battle(4096, 12_500, 8, 1)?, 150),
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

fn play_match(o: &Opts, b: &Balance) -> Result<(), String> {
    let spec = ScenarioSpec::standard_match(o.map, o.players, o.seed, b)?;
    let mut bots: Vec<Box<dyn Controller>> = (1..=o.players)
        .map(|p| {
            let d = o.difficulty.get(p as usize - 1).or(o.difficulty.last()).copied().unwrap_or(ai::Difficulty::Normal);
            Box::new(ai::Bot::new(p, d)) as Box<dyn Controller>
        })
        .collect();
    let t = Instant::now();
    let mut winner = None;
    let r = Replay::record(&spec, b, o.ticks, &mut bots, |w| {
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

fn usage() -> String {
    "usage: iron-dots-headless <record|verify|hash|bench|match|scaling> [--map N] [--units N] [--players N] \
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
