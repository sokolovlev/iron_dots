//! Iron Dots client.
//!
//! ```text
//! iron-dots                         open the main menu
//! iron-dots --play [opts]           start a match vs AI right away
//! iron-dots --screenshot FILE [opts] [--ticks N] [--size WxH] [--zoom PX] [--no-hud]
//!                                   render a bot-vs-bot frame to PNG, no window
//! opts: --map N  --opponents N  --difficulty easy|normal|hard  --seed N
//!       --sandbox [--units N]   (spec 2.7: N units per side, no upkeep/supply)
//! ```

mod app;
mod game;
mod gpu;
mod offscreen;
mod online;
mod ui;

use game::MatchSettings;
use std::process::ExitCode;

fn parse() -> Result<(Option<String>, bool, offscreen::ShotOptions), String> {
    let mut shot = None;
    let mut play = false;
    let mut o = offscreen::ShotOptions {
        settings: MatchSettings::default(),
        ticks: 1800,
        width: 1280,
        height: 720,
        zoom: 0.0,
        hud: true,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().ok_or_else(|| format!("{a} needs a value"));
        let num = |s: String| s.parse::<u64>().map_err(|_| format!("not a number: {s}"));
        match a.as_str() {
            "--screenshot" => shot = Some(val()?),
            "--play" => play = true,
            "--map" => o.settings.map_size = num(val()?)? as u32,
            "--sandbox" => o.settings.sandbox = true,
            "--units" => o.settings.units_per_player = num(val()?)?.clamp(1, 12_500) as u32,
            "--opponents" => o.settings.opponents = num(val()?)?.clamp(1, 7) as u8,
            "--seed" => o.settings.seed = num(val()?)?,
            "--ticks" => o.ticks = num(val()?)? as u32,
            "--zoom" => o.zoom = val()?.parse().map_err(|_| "bad --zoom")?,
            "--no-hud" => o.hud = false,
            "--difficulty" => {
                o.settings.difficulty = match val()?.as_str() {
                    "easy" => ai::Difficulty::Easy,
                    "normal" => ai::Difficulty::Normal,
                    "hard" => ai::Difficulty::Hard,
                    d => return Err(format!("unknown difficulty {d}")),
                }
            }
            "--size" => {
                let v = val()?;
                let (w, h) = v.split_once('x').ok_or("--size is WxH")?;
                o.width = w.parse().map_err(|_| "bad width")?;
                o.height = h.parse().map_err(|_| "bad height")?;
            }
            "-h" | "--help" => return Err("see the top of crates/client/src/main.rs for options".into()),
            _ => return Err(format!("unknown option {a}")),
        }
    }
    Ok((shot, play, o))
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
    let (shot, play, opts) = match parse() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = match shot {
        Some(path) => offscreen::screenshot(&opts, &path).map(|adapter| println!("wrote {path} using {adapter}")),
        None => app::run(play.then(|| opts.settings.clone())),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
