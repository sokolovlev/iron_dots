//! QUIC transport for [`Relay`]: one task owns the relay state, connection
//! tasks feed it messages, and a clock drives the ticks.

use crate::{Config, Out, Relay};
use net::proto::{ClientMsg, ServerMsg};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

enum Event {
    Connect(mpsc::UnboundedSender<Option<ServerMsg>>, oneshot::Sender<u32>),
    Msg(u32, ClientMsg),
    Gone(u32),
}

/// Serves on `endpoint` until `stop` resolves (or forever).
pub async fn serve(endpoint: quinn::Endpoint, config: Config, stop: impl Future<Output = ()>) {
    let entropy = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
    let rate = config.tick_rate.max(1);
    let mut relay = Relay::new(config, entropy);
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<Event>();
    let accept = {
        let endpoint = endpoint.clone();
        let ev_tx = ev_tx.clone();
        tokio::spawn(async move {
            while let Some(incoming) = endpoint.accept().await {
                tokio::spawn(connection(incoming, ev_tx.clone()));
            }
        })
    };
    let mut clock = tokio::time::interval(Duration::from_secs(1) / rate);
    clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
    let mut conns: BTreeMap<u32, mpsc::UnboundedSender<Option<ServerMsg>>> = BTreeMap::new();
    let mut out = Vec::new();
    tokio::pin!(stop);
    loop {
        tokio::select! {
            _ = &mut stop => break,
            _ = clock.tick() => relay.tick(&mut out),
            ev = ev_rx.recv() => match ev {
                Some(Event::Connect(tx, reply)) => {
                    let id = relay.connect();
                    conns.insert(id, tx);
                    let _ = reply.send(id);
                }
                Some(Event::Msg(id, m)) => relay.message(id, m, &mut out),
                Some(Event::Gone(id)) => {
                    relay.disconnect(id, &mut out);
                    conns.remove(&id);
                }
                None => break,
            },
        }
        for o in out.drain(..) {
            match o {
                Out::Send(id, m) => {
                    if let Some(tx) = conns.get(&id) {
                        let _ = tx.send(Some(m));
                    }
                }
                Out::Close(id) => {
                    if let Some(tx) = conns.get(&id) {
                        let _ = tx.send(None);
                    }
                }
            }
        }
    }
    accept.abort();
    endpoint.close(0u32.into(), b"server stopped");
}

async fn connection(incoming: quinn::Incoming, ev_tx: mpsc::UnboundedSender<Event>) {
    let Ok(conn) = incoming.await else { return };
    let Ok((mut send, mut recv)) = conn.accept_bi().await else { return };
    let (tx, mut rx) = mpsc::unbounded_channel::<Option<ServerMsg>>();
    let (id_tx, id_rx) = oneshot::channel();
    if ev_tx.send(Event::Connect(tx, id_tx)).is_err() {
        return;
    }
    let Ok(id) = id_rx.await else { return };
    let reader = async {
        while let Ok(m) = net::frame::read::<ClientMsg>(&mut recv).await {
            if ev_tx.send(Event::Msg(id, m)).is_err() {
                break;
            }
        }
    };
    let writer = async {
        while let Some(Some(m)) = rx.recv().await {
            if net::frame::write(&mut send, &m).await.is_err() {
                return;
            }
        }
        // Asked to close: let the last messages reach the peer first.
        let _ = send.finish();
        let _ = tokio::time::timeout(Duration::from_secs(2), send.stopped()).await;
    };
    tokio::select! {
        _ = reader => {}
        _ = writer => {}
    }
    let _ = ev_tx.send(Event::Gone(id));
    conn.close(0u32.into(), b"bye");
}

/// A relay running on its own thread (LAN hosting, tests, tools).
pub struct LocalServer {
    pub addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LocalServer {
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for LocalServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Starts a relay on `bind` (port 0 picks a free one) on a new thread.
pub fn spawn_local(bind: SocketAddr, config: Config) -> Result<LocalServer, String> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    let endpoint = {
        let _guard = rt.enter();
        quinn::Endpoint::server(net::tls::server_config()?, bind)
            .map_err(|e| format!("cannot listen on {bind}: {e}"))?
    };
    let addr = endpoint.local_addr().map_err(|e| e.to_string())?;
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let thread = std::thread::Builder::new()
        .name("relay".into())
        .spawn(move || {
            rt.block_on(serve(endpoint, config, async {
                let _ = stop_rx.await;
            }));
        })
        .map_err(|e| e.to_string())?;
    Ok(LocalServer { addr, stop: Some(stop_tx), thread: Some(thread) })
}
