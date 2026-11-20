//! A connection to a relay on a background thread, with a polling API the
//! game loop can call every frame without blocking.

use crate::proto::{ClientMsg, ServerMsg};
use std::net::SocketAddr;
use std::sync::mpsc;
use std::thread::JoinHandle;
use tokio::sync::mpsc as amp;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetEvent {
    Connected,
    Msg(ServerMsg),
    /// The connection is gone (or never came up); the reason for the player.
    Disconnected(String),
}

pub struct NetClient {
    tx: amp::UnboundedSender<ClientMsg>,
    rx: mpsc::Receiver<NetEvent>,
    thread: Option<JoinHandle<()>>,
}

impl NetClient {
    /// Starts connecting to `server`; progress arrives as [`NetEvent`]s.
    pub fn connect(server: SocketAddr) -> NetClient {
        let (tx, mut out_rx) = amp::unbounded_channel::<ClientMsg>();
        let (ev_tx, rx) = mpsc::channel::<NetEvent>();
        let thread = std::thread::Builder::new()
            .name("net".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ev_tx.send(NetEvent::Disconnected(e.to_string()));
                        return;
                    }
                };
                let result = rt.block_on(run(server, &mut out_rx, &ev_tx));
                let reason = match result {
                    Ok(()) => "connection closed".to_string(),
                    Err(e) => e,
                };
                let _ = ev_tx.send(NetEvent::Disconnected(reason));
            })
            .expect("spawn network thread");
        NetClient { tx, rx, thread: Some(thread) }
    }

    pub fn send(&self, m: ClientMsg) {
        let _ = self.tx.send(m);
    }

    /// Everything that arrived since the last call.
    pub fn poll(&self) -> Vec<NetEvent> {
        self.rx.try_iter().collect()
    }

    /// Waits up to `timeout` for the next event (tests, tools).
    pub fn recv_timeout(&self, timeout: std::time::Duration) -> Option<NetEvent> {
        self.rx.recv_timeout(timeout).ok()
    }
}

impl Drop for NetClient {
    fn drop(&mut self) {
        // Closing the sender ends the writer loop and with it the thread.
        let (dead, _) = amp::unbounded_channel();
        self.tx = dead;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

async fn run(
    server: SocketAddr,
    out_rx: &mut amp::UnboundedReceiver<ClientMsg>,
    ev_tx: &mpsc::Sender<NetEvent>,
) -> Result<(), String> {
    let bind: SocketAddr = if server.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" }.parse().expect("bind address");
    let mut endpoint = quinn::Endpoint::client(bind).map_err(|e| e.to_string())?;
    endpoint.set_default_client_config(crate::tls::client_config()?);
    let conn = endpoint
        .connect(server, "iron-dots")
        .map_err(|e| e.to_string())?
        .await
        .map_err(|e| format!("cannot reach {server}: {e}"))?;
    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| e.to_string())?;
    let _ = ev_tx.send(NetEvent::Connected);
    let reader = async {
        loop {
            let m: ServerMsg = crate::frame::read(&mut recv).await?;
            if ev_tx.send(NetEvent::Msg(m)).is_err() {
                return Ok::<(), String>(());
            }
        }
    };
    let writer = async {
        while let Some(m) = out_rx.recv().await {
            crate::frame::write(&mut send, &m).await?;
        }
        let _ = send.finish();
        Ok::<(), String>(())
    };
    let r = tokio::select! {
        r = reader => r,
        r = writer => r,
    };
    conn.close(0u32.into(), b"bye");
    endpoint.wait_idle().await;
    r
}
