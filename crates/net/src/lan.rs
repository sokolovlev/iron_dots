//! LAN games without a dedicated server: the host runs the relay inside
//! the game and announces it with a UDP broadcast once a second; other
//! players on the network see it in their game list.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// UDP port the announcements go to.
pub const BEACON_PORT: u16 = 27_961;
const MAGIC: &[u8; 6] = b"IDLAN1";

/// Announces a relay on `port` under `name` until dropped.
pub struct Beacon {
    stop: Arc<AtomicBool>,
}

impl Beacon {
    pub fn start(name: &str, port: u16) -> Result<Beacon, String> {
        let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map_err(|e| e.to_string())?;
        sock.set_broadcast(true).map_err(|e| e.to_string())?;
        let mut msg = MAGIC.to_vec();
        msg.extend_from_slice(&port.to_le_bytes());
        msg.extend(name.bytes().take(64));
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        std::thread::Builder::new()
            .name("lan-beacon".into())
            .spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    let _ = sock.send_to(&msg, (Ipv4Addr::BROADCAST, BEACON_PORT));
                    let _ = sock.send_to(&msg, (Ipv4Addr::LOCALHOST, BEACON_PORT));
                    std::thread::sleep(Duration::from_secs(1));
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Beacon { stop })
    }
}

impl Drop for Beacon {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Listens for announcements; poll it from the menu.
pub struct Scanner {
    sock: UdpSocket,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LanGame {
    pub addr: SocketAddr,
    pub name: String,
}

impl Scanner {
    pub fn new() -> Result<Scanner, String> {
        let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, BEACON_PORT))
            .map_err(|e| format!("cannot listen for LAN games: {e}"))?;
        sock.set_nonblocking(true).map_err(|e| e.to_string())?;
        Ok(Scanner { sock })
    }

    /// Announcements received since the last call.
    pub fn poll(&self) -> Vec<LanGame> {
        let mut found = Vec::new();
        let mut buf = [0u8; 128];
        while let Ok((n, from)) = self.sock.recv_from(&mut buf) {
            let b = &buf[..n];
            if n < 8 || &b[..6] != MAGIC {
                continue;
            }
            let port = u16::from_le_bytes([b[6], b[7]]);
            let g = LanGame { addr: SocketAddr::new(from.ip(), port), name: String::from_utf8_lossy(&b[8..]).into() };
            if !found.contains(&g) {
                found.push(g);
            }
        }
        found
    }
}
