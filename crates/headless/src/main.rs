//! `iron-dots-headless` — the simulation without a window.
//!
//! ```text
//! iron-dots-headless record [opts] --out FILE   play scripted players, save a replay
//! iron-dots-headless verify FILE                re-simulate a replay, check every hash
//! iron-dots-headless hash   [opts]              print the final state hash
//! iron-dots-headless bench  [opts]              tick timings: p50 / p99 / max
//!
//! opts: --map N (tiles, default 256)  --units N (per player, default 200)
//!       --players N (default 2)       --seed N (default 1)
//!       --ticks N (default 1800)      --balance FILE (default: built-in)
//! ```

use sim_core::replay::Replay;
use sim_core::scenario::{ScenarioSpec, ScriptedPlayer};
use sim_core::{Balance, World};
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
    ScenarioSpec::battle(o.map, o.units, o.players, o.seed)
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
        _ => return Err(usage()),
    }
    Ok(())
}

fn bench(o: &Opts, b: &Balance) -> Result<(), String> {
    let spec = scenario(o)?;
    let mut world: World = spec.build(b)?;
    let mut bots: Vec<ScriptedPlayer> = (1..=world.player_count()).map(ScriptedPlayer::new).collect();
    let start_units = world.units().len();
    let mut times: Vec<Duration> = Vec::with_capacity(o.ticks as usize);
    let mut peak_combat = 0;
    let mut cmds = Vec::new();
    for _ in 0..o.ticks {
        cmds.clear();
        for bot in &mut bots {
            cmds.extend(bot.think(&world));
        }
        let t = Instant::now();
        world.step(&cmds);
        times.push(t.elapsed());
        peak_combat = peak_combat.max(world.stats().units_in_combat);
    }
    times.sort();
    let pct = |p: usize| times[(times.len() - 1) * p / 100];
    let total: Duration = times.iter().sum();
    println!(
        "map {m}x{m}, {start_units} units ({} left), {} ticks, peak in combat {peak_combat}",
        world.units().len(),
        o.ticks,
        m = o.map
    );
    println!(
        "tick: mean {:.3?}  p50 {:.3?}  p99 {:.3?}  max {:.3?}",
        total / o.ticks.max(1),
        pct(50),
        pct(99),
        times.last().copied().unwrap_or_default()
    );
    println!("final hash {:016x}", world.state_hash());
    Ok(())
}

fn usage() -> String {
    "usage: iron-dots-headless <record|verify|hash|bench> [--map N] [--units N] [--players N] \
     [--seed N] [--ticks N] [--balance FILE] [--out FILE] [FILE]"
        .into()
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
