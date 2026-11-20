//! A running network match on one client: the world, the lockstep
//! schedule, local bots, hash reports and a desync dump.

use crate::lockstep::Lockstep;
use crate::proto::{ClientMsg, TickPacket};
use sim_core::balance::Balance;
use sim_core::command::Command;
use sim_core::replay::Replay;
use sim_core::scenario::{Controller, ScenarioSpec};
use sim_core::world::World;

/// Ticks between progress reports to the relay.
const PROGRESS_EVERY: u32 = 10;

pub struct Session {
    pub game: u32,
    pub token: u64,
    /// This client's player id.
    pub you: u8,
    pub world: World,
    pub scenario: ScenarioSpec,
    balance: Balance,
    lockstep: Lockstep,
    /// Bots run here as on every other client, outside the network.
    bots: Vec<Box<dyn Controller + Send>>,
    /// Every applied command (network and bots), for the desync dump.
    log: Vec<Command>,
    /// Plays this client's own player (headless tests); its commands are
    /// sent to the relay like a human's.
    pilot: Option<Box<dyn Controller + Send>>,
    seq: u32,
    checkpoints: Vec<(u32, u64)>,
    last_progress: u32,
}

impl Session {
    /// Builds the starting world. `bots` must be the same controllers, in
    /// the same order, on every client.
    pub fn new(
        game: u32,
        token: u64,
        you: u8,
        scenario: ScenarioSpec,
        tick_rate: u32,
        bots: Vec<Box<dyn Controller + Send>>,
        balance: Balance,
    ) -> Result<Session, String> {
        let world = scenario.build(&balance)?;
        Ok(Session {
            game,
            token,
            you,
            world,
            scenario,
            balance,
            lockstep: Lockstep::new(tick_rate),
            bots,
            log: Vec::new(),
            pilot: None,
            seq: 0,
            checkpoints: Vec::new(),
            last_progress: 0,
        })
    }

    /// Lets a controller (a bot) play this client's player over the network.
    pub fn set_pilot(&mut self, pilot: Box<dyn Controller + Send>) {
        self.pilot = Some(pilot);
    }

    /// Next sequence number for a `Commands` message.
    pub fn next_seq(&mut self) -> u32 {
        self.seq += 1;
        self.seq
    }

    pub fn receive(&mut self, p: TickPacket, now: f64) {
        self.lockstep.receive(p, now);
    }

    /// Ticks received but not simulated yet.
    pub fn behind(&self) -> u32 {
        self.lockstep.available()
    }

    pub fn buffer(&self) -> u32 {
        self.lockstep.buffer()
    }

    /// Simulates what is due after `dt` seconds (at most `max` ticks);
    /// messages for the relay go to `out`. Returns the ticks simulated.
    pub fn advance(&mut self, dt: f64, max: u32, out: &mut Vec<ClientMsg>) -> u32 {
        let n = self.lockstep.due(dt, max);
        for _ in 0..n {
            let Some((t, mut cmds)) = self.lockstep.take() else { break };
            debug_assert_eq!(t, self.world.tick());
            for b in &mut self.bots {
                cmds.extend(b.think(&self.world));
            }
            self.world.step(&cmds);
            self.log.extend(cmds);
            let now = self.world.tick();
            if now % crate::HASH_INTERVAL == 0 {
                let hash = self.world.state_hash();
                self.checkpoints.push((now, hash));
                out.push(ClientMsg::Hash { tick: now, hash });
            }
            if let Some(p) = &mut self.pilot {
                let kinds: Vec<_> = p.think(&self.world).into_iter().map(|c| c.kind).collect();
                if !kinds.is_empty() {
                    self.seq += 1;
                    out.push(ClientMsg::Commands { seq: self.seq, kinds });
                }
            }
        }
        let now = self.world.tick();
        if n > 0 && now >= self.last_progress + PROGRESS_EVERY {
            self.last_progress = now;
            out.push(ClientMsg::Progress { tick: now });
        }
        n
    }

    /// The match so far as a replay, with every hash this client computed:
    /// replaying it elsewhere shows where clients parted ways.
    pub fn replay(&self) -> Replay {
        Replay {
            sim_code_version: sim_core::SIM_CODE_VERSION,
            balance_hash: self.balance.content_hash(),
            scenario: self.scenario.clone(),
            ticks: self.world.tick(),
            commands: self.log.clone(),
            checkpoints: self.checkpoints.clone(),
        }
    }

    pub fn checkpoints(&self) -> &[(u32, u64)] {
        &self.checkpoints
    }
}
