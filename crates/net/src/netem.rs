//! A UDP proxy that makes the network worse on purpose: it drops packets
//! and delays each one by a random amount (which also reorders them).
//! Used by tests and `headless netsim` to check lockstep under the spec's
//! bad-network conditions (10% loss, 100 ms jitter).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::oneshot;

#[derive(Clone, Copy, Debug)]
pub struct Conditions {
    /// Probability of dropping a packet, per mille (100 = 10%).
    pub loss_permille: u32,
    /// Fixed one-way delay, ms.
    pub delay_ms: u32,
    /// Extra uniformly random one-way delay, 0..jitter_ms.
    pub jitter_ms: u32,
    pub seed: u64,
}

impl Conditions {
    /// The spec's exit criterion: 10% loss, 100 ms jitter.
    pub fn bad(seed: u64) -> Conditions {
        Conditions { loss_permille: 100, delay_ms: 20, jitter_ms: 100, seed }
    }
}

struct Rng(Mutex<u64>);

impl Rng {
    fn next(&self) -> u64 {
        let mut s = self.0.lock().expect("rng lock");
        *s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *s;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// A running proxy: clients connect to `addr` instead of the server.
pub struct Netem {
    pub addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Netem {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Spawns a proxy in front of `upstream` on a local port.
pub fn spawn(upstream: SocketAddr, cond: Conditions) -> Result<Netem, String> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    let front = {
        let _g = rt.enter();
        let s = std::net::UdpSocket::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
        s.set_nonblocking(true).map_err(|e| e.to_string())?;
        UdpSocket::from_std(s).map_err(|e| e.to_string())?
    };
    let addr = front.local_addr().map_err(|e| e.to_string())?;
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let thread = std::thread::Builder::new()
        .name("netem".into())
        .spawn(move || {
            rt.block_on(async move {
                tokio::select! {
                    _ = run(Arc::new(front), upstream, cond) => {}
                    _ = stop_rx => {}
                }
            });
        })
        .map_err(|e| e.to_string())?;
    Ok(Netem { addr, stop: Some(stop_tx), thread: Some(thread) })
}

/// Sends `data` through `sock` to `to` after the simulated delay, unless lost.
fn impair(rng: &Arc<Rng>, cond: Conditions, sock: Arc<UdpSocket>, to: Option<SocketAddr>, data: Vec<u8>) {
    let r = rng.next();
    if (r % 1000) < cond.loss_permille as u64 {
        return;
    }
    let jitter = if cond.jitter_ms > 0 { (r >> 16) % cond.jitter_ms as u64 } else { 0 };
    let delay = Duration::from_millis(cond.delay_ms as u64 + jitter);
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let _ = match to {
            Some(a) => sock.send_to(&data, a).await,
            None => sock.send(&data).await,
        };
    });
}

async fn run(front: Arc<UdpSocket>, upstream: SocketAddr, cond: Conditions) {
    let rng = Arc::new(Rng(Mutex::new(cond.seed)));
    let mut backs: HashMap<SocketAddr, Arc<UdpSocket>> = HashMap::new();
    let mut buf = vec![0u8; 65_536];
    loop {
        let Ok((n, from)) = front.recv_from(&mut buf).await else { return };
        let back = match backs.get(&from) {
            Some(b) => b.clone(),
            None => {
                let Ok(b) = UdpSocket::bind("127.0.0.1:0").await else { continue };
                if b.connect(upstream).await.is_err() {
                    continue;
                }
                let b = Arc::new(b);
                backs.insert(from, b.clone());
                // Server -> client direction.
                let (front, b2, rng) = (front.clone(), b.clone(), rng.clone());
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65_536];
                    while let Ok(n) = b2.recv(&mut buf).await {
                        impair(&rng, cond, front.clone(), Some(from), buf[..n].to_vec());
                    }
                });
                b
            }
        };
        impair(&rng, cond, back, None, buf[..n].to_vec());
    }
}
