//! Client-side lockstep scheduling.
//!
//! The relay sends one packet per tick. A client may simulate tick `t` only
//! once it holds packet `t`. To ride out network jitter it stays a few
//! ticks behind the newest packet (the jitter buffer), sized from measured
//! packet timing: 2–6 ticks (66–200 ms at 30 tick/s), as in spec 6.2.
//! When it falls far behind (rejoin, a hiccup) it catches up at full speed.

use crate::proto::TickPacket;
use sim_core::command::Command;
use std::collections::BTreeMap;

pub const MIN_BUFFER: u32 = 2;
pub const MAX_BUFFER: u32 = 6;
/// More than this many ticks behind: catch up as fast as possible.
pub const CATCH_UP: u32 = 30;

pub struct Lockstep {
    tick_rate: f64,
    packets: BTreeMap<u32, Vec<Command>>,
    /// Next tick to simulate.
    next: u32,
    /// Ticks received contiguously from `next` on.
    newest: Option<u32>,
    /// Smoothed jitter of packet arrivals, in seconds.
    jitter: f64,
    last_arrival: Option<(f64, u32)>,
    acc: f64,
}

impl Lockstep {
    pub fn new(tick_rate: u32) -> Lockstep {
        Lockstep {
            tick_rate: tick_rate.max(1) as f64,
            packets: BTreeMap::new(),
            next: 0,
            newest: None,
            jitter: 0.0,
            last_arrival: None,
            acc: 0.0,
        }
    }

    /// Stores a packet that arrived at time `now` (seconds, any origin).
    pub fn receive(&mut self, p: TickPacket, now: f64) {
        if p.tick < self.next {
            return; // duplicate
        }
        // Jitter: how far arrivals stray from the expected spacing.
        if let Some((t0, k0)) = self.last_arrival {
            if p.tick > k0 {
                let expected = (p.tick - k0) as f64 / self.tick_rate;
                let dev = ((now - t0) - expected).abs();
                self.jitter += (dev - self.jitter) * 0.05;
            }
        }
        self.last_arrival = Some((now, p.tick));
        self.packets.insert(p.tick, p.commands);
        let mut n = self.newest.map_or(self.next, |t| t + 1);
        while self.packets.contains_key(&n) {
            n += 1;
        }
        if n > self.next {
            self.newest = Some(n - 1);
        }
    }

    /// Ticks held contiguously and not yet simulated.
    pub fn available(&self) -> u32 {
        self.newest.map_or(0, |t| (t + 1).saturating_sub(self.next))
    }

    /// Current jitter buffer in ticks.
    pub fn buffer(&self) -> u32 {
        let ticks = (2.0 * self.jitter * self.tick_rate).ceil() as u32 + MIN_BUFFER;
        ticks.clamp(MIN_BUFFER, MAX_BUFFER)
    }

    /// Fraction of the next tick already elapsed (0..1), for interpolation.
    pub fn phase(&self) -> f64 {
        self.acc.clamp(0.0, 1.0)
    }

    pub fn next_tick(&self) -> u32 {
        self.next
    }

    /// How many ticks to simulate now, `dt` seconds after the last call.
    /// Normally one per 1/tick_rate s, a little faster or slower to keep
    /// `buffer()` ticks in hand, all at once when far behind.
    pub fn due(&mut self, dt: f64, max: u32) -> u32 {
        let avail = self.available();
        let buffer = self.buffer();
        if avail > buffer + CATCH_UP {
            self.acc = 0.0;
            return (avail - buffer).min(max);
        }
        let speed = if avail > buffer + 1 {
            1.1
        } else if avail < buffer {
            0.9
        } else {
            1.0
        };
        self.acc += dt * self.tick_rate * speed;
        let mut n = self.acc.floor() as u32;
        n = n.min(avail).min(max);
        self.acc -= n as f64;
        if avail == 0 {
            self.acc = self.acc.min(1.0);
        }
        n
    }

    /// The next tick and its commands, if its packet is here.
    pub fn take(&mut self) -> Option<(u32, Vec<Command>)> {
        let cmds = self.packets.remove(&self.next)?;
        let t = self.next;
        self.next += 1;
        if self.newest.is_some_and(|n| n < self.next) {
            self.newest = None;
        }
        Some((t, cmds))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(t: u32) -> TickPacket {
        TickPacket { tick: t, commands: Vec::new() }
    }

    #[test]
    fn simulates_only_received_ticks_in_order() {
        let mut l = Lockstep::new(30);
        l.receive(pkt(1), 0.0);
        assert_eq!(l.available(), 0, "tick 0 missing");
        l.receive(pkt(0), 0.01);
        assert_eq!(l.available(), 2);
        assert_eq!(l.take().map(|p| p.0), Some(0));
        assert_eq!(l.take().map(|p| p.0), Some(1));
        assert_eq!(l.take(), None);
    }

    #[test]
    fn keeps_a_buffer_and_catches_up() {
        let mut l = Lockstep::new(30);
        for t in 0..100 {
            l.receive(pkt(t), t as f64 / 30.0);
        }
        // Far behind: everything but the buffer at once.
        let n = l.due(0.0, 1000);
        assert_eq!(n, 100 - l.buffer());
        for _ in 0..n {
            l.take();
        }
        // Then about one tick per 1/30 s.
        let mut total = 0;
        for k in 0..30 {
            l.receive(pkt(100 + k), (100 + k) as f64 / 30.0);
            let n = l.due(1.0 / 30.0, 1000);
            for _ in 0..n {
                l.take();
            }
            total += n;
        }
        assert!((27..=33).contains(&total), "{total}");
    }

    #[test]
    fn jitter_grows_the_buffer() {
        let mut l = Lockstep::new(30);
        assert_eq!(l.buffer(), MIN_BUFFER);
        for t in 0..200u32 {
            let wobble = if t % 2 == 0 { 0.1 } else { 0.0 };
            l.receive(pkt(t), t as f64 / 30.0 + wobble);
        }
        assert!(l.buffer() > MIN_BUFFER && l.buffer() <= MAX_BUFFER, "{}", l.buffer());
    }
}
