//! Networking for Iron Dots (spec 6.2): deterministic lockstep over QUIC.
//!
//! Only commands travel over the network, never game state, so traffic
//! does not depend on the number of units. A relay server (`relay-server`
//! crate) is the clock: it collects the players' commands, assigns them to
//! a tick, and broadcasts one [`TickPacket`] per tick to everyone. Every
//! client simulates tick `t` only once it holds packet `t`, so all clients
//! apply the same commands at the same tick and stay bit-identical. Bots
//! run on every client and never touch the network.
//!
//! * [`proto`] — messages and lobby types (bitcode on the wire);
//! * [`frame`] — length-prefixed frames on a QUIC stream;
//! * [`lockstep`] — the client's tick scheduling with an adaptive jitter
//!   buffer (2–6 ticks, from measured packet timing);
//! * [`tls`] — QUIC/TLS configuration (self-signed server certificates);
//! * [`client`] — a connection on a background thread with a polling API
//!   for the game loop;
//! * [`session`] — a running match: world, lockstep, local bots, hashes;
//! * [`lan`] — announcing and finding LAN games;
//! * [`netem`] — a lossy, jittery UDP proxy for testing bad networks.

#![forbid(unsafe_code)]

pub mod client;
pub mod frame;
pub mod lan;
pub mod lockstep;
pub mod netem;
pub mod proto;
pub mod session;
pub mod tls;

pub use client::{NetClient, NetEvent};
pub use lockstep::Lockstep;
pub use proto::{ClientMsg, ServerMsg, TickPacket};
pub use session::Session;

/// Clients report their state hash every this many ticks.
pub const HASH_INTERVAL: u32 = 30;

/// Default UDP port of the relay server.
pub const DEFAULT_PORT: u16 = 27_960;
