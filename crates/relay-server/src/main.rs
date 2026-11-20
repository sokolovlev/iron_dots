//! Dedicated relay server: `iron-dots-relay [--port N] [--tick-rate N]`.

fn main() {
    let mut port = net::DEFAULT_PORT;
    let mut config = relay_server::Config::default();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut value = || args.next().and_then(|v| v.parse::<u32>().ok());
        match a.as_str() {
            "--port" => port = value().and_then(|v| u16::try_from(v).ok()).unwrap_or_else(|| usage()),
            "--tick-rate" => config.tick_rate = value().filter(|v| (1..=240).contains(v)).unwrap_or_else(|| usage()),
            "--max-lobbies" => config.max_lobbies = value().unwrap_or_else(|| usage()) as usize,
            _ => usage(),
        }
    }
    let bind = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let endpoint = {
        let _guard = rt.enter();
        let cfg = net::tls::server_config().unwrap_or_else(|e| fail(&e));
        quinn::Endpoint::server(cfg, bind).unwrap_or_else(|e| fail(&format!("cannot listen on {bind}: {e}")))
    };
    println!("iron-dots relay on udp {bind}, {} ticks/s", config.tick_rate);
    rt.block_on(relay_server::serve(endpoint, config, async {
        let _ = tokio::signal::ctrl_c().await;
    }));
}

fn usage() -> ! {
    fail("usage: iron-dots-relay [--port N] [--tick-rate 1..240] [--max-lobbies N]")
}

fn fail(e: &str) -> ! {
    eprintln!("{e}");
    std::process::exit(2)
}
