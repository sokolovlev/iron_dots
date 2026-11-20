//! A single-player match: the world, the bots, the local player's input
//! turned into commands, and what the renderer should show. No windowing
//! here, so the same code drives the window and offscreen screenshots.

use ai::{Bot, Difficulty};
use render::{Camera, Marker, Overlay, mflags};
use sim_core::command::{Command, CommandKind};
use sim_core::fx::{Fx, FxPoint};
use sim_core::replay::Replay;
use sim_core::scenario::{Controller, ScenarioSpec};
use sim_core::unit::{NO_ORDER, flags};
use sim_core::{Balance, UnitHandle, UnitKind, World};

/// Settings chosen in the menu.
#[derive(Clone, Debug, PartialEq)]
pub struct MatchSettings {
    pub map_size: u32,
    pub opponents: u8,
    pub difficulty: Difficulty,
    pub seed: u64,
    /// Sandbox (spec 2.7): big armies from the start, no upkeep or supply
    /// limits; `units_per_player` per faction.
    pub sandbox: bool,
    pub units_per_player: u32,
}

impl Default for MatchSettings {
    fn default() -> Self {
        MatchSettings {
            map_size: 256,
            opponents: 1,
            difficulty: Difficulty::Normal,
            seed: 1,
            sandbox: false,
            units_per_player: 2_000,
        }
    }
}

/// Below this zoom (pixels per tile) units are drawn as density markers.
pub const LOD_SCALE: f32 = 2.0;

/// Game speeds (spec 2.9): ×0.5, ×1, ×2, ×4.
pub const SPEEDS: [f32; 4] = [0.5, 1.0, 2.0, 4.0];

pub struct Game {
    pub world: World,
    pub me: u8,
    pub settings: MatchSettings,
    spec: ScenarioSpec,
    balance: Balance,
    bots: Vec<Bot>,
    /// Local commands waiting for the next tick (tick is stamped then).
    pending: Vec<CommandKind>,
    seq: u32,
    /// Every command applied so far, for the replay.
    log: Vec<Command>,
    pub paused: bool,
    pub speed: usize,
    acc: f32,
    /// Position of every slot at the previous tick, for interpolation.
    prev_pos: Vec<(i32, i32, u32)>,
    pub camera: Camera,
    pub selection: Vec<UnitHandle>,
    pub selected_city: Option<usize>,
    groups: [Vec<UnitHandle>; 10],
    /// Path being drawn with the right button (world coordinates).
    pub drawing: Option<Vec<[f32; 2]>>,
    /// Selection rectangle in screen pixels: (start, current).
    pub box_select: Option<([f32; 2], [f32; 2])>,
    /// Time in seconds, for animation.
    pub time: f32,
    /// Last tick's command feedback, e.g. "not enough money".
    pub message: Option<(String, f32)>,
    /// Per-second history for the end-of-match charts: (units, cities, money) per player.
    pub history: Vec<[(u32, u32, i64); 9]>,
}

impl Game {
    pub fn new(settings: MatchSettings) -> Result<Game, String> {
        let balance = Balance::default_balance();
        let players = settings.opponents + 1;
        let spec = if settings.sandbox {
            ScenarioSpec::battle(settings.map_size, settings.units_per_player, players, settings.seed)?
        } else {
            ScenarioSpec::standard_match(settings.map_size, players, settings.seed, &balance)?
        };
        let mut world = spec.build(&balance)?;
        // Leave one core for rendering and the UI; results do not depend on
        // the thread count.
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        world.set_threads(cores.saturating_sub(1).max(1))?;
        let me = 1;
        let bots = (2..=players).map(|p| Bot::new(p, settings.difficulty)).collect();
        let cap = world.cities().iter().find(|c| c.capital_of == me).map(|c| c.center()).unwrap_or_default();
        let scale = if settings.sandbox { 3.0 } else { 10.0 };
        let camera = Camera { center: [fx(cap.x), fx(cap.y)], scale };
        Ok(Game {
            world,
            me,
            settings,
            spec,
            balance,
            bots,
            pending: Vec::new(),
            seq: 0,
            log: Vec::new(),
            paused: false,
            speed: 1,
            acc: 0.0,
            prev_pos: Vec::new(),
            camera,
            selection: Vec::new(),
            selected_city: None,
            groups: Default::default(),
            drawing: None,
            box_select: None,
            time: 0.0,
            message: None,
            history: Vec::new(),
        })
    }

    /// Replaces the local player with a bot (for screenshots and demos).
    pub fn add_bot_for_me(&mut self, d: Difficulty) {
        self.bots.push(Bot::new(self.me, d));
    }

    pub fn tick_seconds(&self) -> f32 {
        1.0 / self.world.rules().tick_rate as f32
    }

    /// Advances real time by `dt` seconds; steps the simulation as needed.
    pub fn update(&mut self, dt: f32) {
        self.time += dt;
        if let Some((_, t)) = &mut self.message {
            *t -= dt;
            if *t <= 0.0 {
                self.message = None;
            }
        }
        if self.paused {
            return;
        }
        let tick = self.tick_seconds();
        self.acc += dt.min(0.25) * SPEEDS[self.speed];
        let mut steps = 0;
        while self.acc >= tick && steps < 16 {
            self.acc -= tick;
            self.step();
            steps += 1;
        }
        if steps == 16 {
            self.acc = 0.0;
        }
    }

    /// One simulation tick with local and bot commands.
    pub fn step(&mut self) {
        let tick = self.world.tick();
        // Remember positions for interpolation.
        let u = self.world.units();
        for i in 0..u.len() {
            let s = u.slot[i] as usize;
            if self.prev_pos.len() <= s {
                self.prev_pos.resize(s + 1, (0, 0, u32::MAX));
            }
            self.prev_pos[s] = (u.pos_x[i], u.pos_y[i], u.handle(i).generation);
        }
        let mut cmds: Vec<Command> = Vec::new();
        for kind in self.pending.drain(..) {
            cmds.push(Command { tick, player: self.me, seq: self.seq, kind });
            self.seq += 1;
        }
        for b in &mut self.bots {
            cmds.extend(b.think(&self.world));
        }
        let before = self.world.stats();
        self.world.step(&cmds);
        let after = self.world.stats();
        if after.commands_rejected > 0 && cmds.iter().any(|c| c.player == self.me) && before != after {
            // Only report rejections of our own commands.
            let mine_rejected = cmds.iter().filter(|c| c.player == self.me).count() as u32 > 0;
            if mine_rejected {
                self.message = Some(("Not possible: money, supply or ownership".into(), 2.0));
            }
        }
        self.log.extend(cmds);
        if self.world.tick() % self.world.rules().tick_rate == 0 {
            let mut row = [(0u32, 0u32, 0i64); 9];
            let counts = self.world.unit_counts();
            for p in 1..=self.world.player_count() {
                let cities = self.world.cities().iter().filter(|c| c.owner == p).count() as u32;
                row[p as usize] = (counts[p as usize], cities, self.world.player(p).money / 1000);
            }
            self.history.push(row);
        }
        // Drop dead units from the selection.
        let w = &self.world;
        self.selection.retain(|h| w.units().index_of(*h).is_some());
        if let Some(c) = self.selected_city {
            if self.world.cities()[c].owner != self.me {
                self.selected_city = None;
            }
        }
    }

    /// Replay of the match so far.
    pub fn replay(&self) -> Replay {
        Replay {
            sim_code_version: sim_core::SIM_CODE_VERSION,
            balance_hash: self.balance.content_hash(),
            scenario: self.spec.clone(),
            ticks: self.world.tick(),
            commands: self.log.clone(),
            checkpoints: vec![(self.world.tick(), self.world.state_hash())],
        }
    }

    fn alpha(&self) -> f32 {
        (self.acc / self.tick_seconds()).clamp(0.0, 1.0)
    }

    /// Interpolated position of unit `i` in tiles.
    pub fn unit_pos(&self, i: usize) -> [f32; 2] {
        let u = self.world.units();
        let (x, y) = (u.pos_x[i], u.pos_y[i]);
        let s = u.slot[i] as usize;
        let a = self.alpha();
        let (px, py) = match self.prev_pos.get(s) {
            Some(&(px, py, g)) if g == u.handle(i).generation => (px, py),
            _ => (x, y),
        };
        let lerp = |p: i32, c: i32| (p as f32 + (c - p) as f32 * a) / 65536.0;
        [lerp(px, x), lerp(py, y)]
    }

    // ------------------------------------------------------------- input

    fn issue(&mut self, kind: CommandKind) {
        self.pending.push(kind);
    }

    /// Own unit nearest to `w` within `radius` tiles.
    fn pick_unit(&self, w: [f32; 2], radius: f32) -> Option<usize> {
        let u = self.world.units();
        let mut best: Option<(f32, usize)> = None;
        for i in 0..u.len() {
            if u.owner[i] != self.me {
                continue;
            }
            let p = self.unit_pos(i);
            let d = (p[0] - w[0]).powi(2) + (p[1] - w[1]).powi(2);
            if d <= radius * radius && best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, i));
            }
        }
        best.map(|(_, i)| i)
    }

    fn pick_city(&self, w: [f32; 2]) -> Option<usize> {
        self.world.cities().iter().position(|c| {
            let p = c.center();
            (fx(p.x) - w[0]).abs() < 1.2 && (fx(p.y) - w[1]).abs() < 1.2
        })
    }

    /// Left click at screen `px`.
    pub fn click_select(&mut self, px: [f32; 2], viewport: [f32; 2], add: bool) {
        let w = self.camera.screen_to_world(px, viewport);
        let radius = (10.0 / self.camera.scale).max(0.7);
        if let Some(i) = self.pick_unit(w, radius) {
            let h = self.world.units().handle(i);
            if !add {
                self.selection.clear();
            }
            if let Some(k) = self.selection.iter().position(|&x| x == h) {
                if add {
                    self.selection.remove(k);
                }
            } else {
                self.selection.push(h);
            }
            self.selected_city = None;
            return;
        }
        if !add {
            self.selection.clear();
        }
        self.selected_city = self.pick_city(w).filter(|&c| self.world.cities()[c].owner == self.me);
    }

    /// Double click: every own unit of the same type on screen.
    pub fn select_same_type(&mut self, px: [f32; 2], viewport: [f32; 2]) {
        let w = self.camera.screen_to_world(px, viewport);
        let radius = (10.0 / self.camera.scale).max(0.7);
        let Some(i) = self.pick_unit(w, radius) else { return };
        let kind = self.world.units().kind[i];
        let a = self.camera.screen_to_world([0.0, 0.0], viewport);
        let b = self.camera.screen_to_world(viewport, viewport);
        self.select_in_rect(a, b, Some(kind), false);
    }

    fn select_in_rect(&mut self, a: [f32; 2], b: [f32; 2], kind: Option<u8>, add: bool) {
        let (x0, x1) = (a[0].min(b[0]), a[0].max(b[0]));
        let (y0, y1) = (a[1].min(b[1]), a[1].max(b[1]));
        if !add {
            self.selection.clear();
        }
        let u = self.world.units();
        let mut picked = Vec::new();
        for i in 0..u.len() {
            if u.owner[i] != self.me || kind.is_some_and(|k| u.kind[i] != k) {
                continue;
            }
            let p = self.unit_pos(i);
            if p[0] >= x0 && p[0] <= x1 && p[1] >= y0 && p[1] <= y1 {
                picked.push(u.handle(i));
            }
        }
        for h in picked {
            if !self.selection.contains(&h) {
                self.selection.push(h);
            }
        }
        self.selected_city = None;
    }

    pub fn finish_box(&mut self, viewport: [f32; 2], add: bool) {
        if let Some((a, b)) = self.box_select.take() {
            let wa = self.camera.screen_to_world(a, viewport);
            let wb = self.camera.screen_to_world(b, viewport);
            self.select_in_rect(wa, wb, None, add);
        }
    }

    /// Right click (or a finished drawn path): move the selection.
    pub fn order_move(&mut self, path_world: &[[f32; 2]], queue: bool) {
        if self.selection.is_empty() || path_world.is_empty() {
            return;
        }
        let path = simplify(path_world, 32)
            .into_iter()
            .map(|p| FxPoint::new(Fx((p[0] * 65536.0) as i32), Fx((p[1] * 65536.0) as i32)))
            .collect();
        self.issue(CommandKind::Move { units: self.selection.clone(), path, queue });
    }

    pub fn stop(&mut self) {
        if !self.selection.is_empty() {
            self.issue(CommandKind::Stop { units: self.selection.clone() });
        }
    }

    pub fn clear_queue(&mut self) {
        if !self.selection.is_empty() {
            self.issue(CommandKind::ClearQueue { units: self.selection.clone() });
        }
    }

    pub fn buy(&mut self, kind: UnitKind) {
        match self.selected_city {
            Some(c) => self.issue(CommandKind::Buy { city: c as u16, kind }),
            None => self.message = Some(("Select one of your cities first".into(), 2.0)),
        }
    }

    pub fn store_group(&mut self, k: usize) {
        self.groups[k] = self.selection.clone();
    }

    /// Selects group `k`; returns its centre for "press twice to jump".
    pub fn recall_group(&mut self, k: usize) -> Option<[f32; 2]> {
        let w = &self.world;
        self.groups[k].retain(|h| w.units().index_of(*h).is_some());
        self.selection = self.groups[k].clone();
        self.selected_city = None;
        let idx: Vec<usize> = self.selection.iter().filter_map(|h| self.world.units().index_of(*h)).collect();
        if idx.is_empty() {
            return None;
        }
        let n = idx.len() as f32;
        let (sx, sy) = idx.iter().map(|&i| self.unit_pos(i)).fold((0.0, 0.0), |a, p| (a.0 + p[0], a.1 + p[1]));
        Some([sx / n, sy / n])
    }

    pub fn capital_center(&self) -> Option<[f32; 2]> {
        let me = self.me;
        let c = self
            .world
            .cities()
            .iter()
            .find(|c| c.capital_of == me && c.owner == me)
            .or_else(|| self.world.cities().iter().find(|c| c.owner == me))?;
        let p = c.center();
        Some([fx(p.x), fx(p.y)])
    }

    // ------------------------------------------------------------- view

    /// Builds markers for what is on screen: cities first, units on top.
    /// Zoomed far out (below [`LOD_SCALE`]) units are aggregated into
    /// density markers, one per few screen pixels, coloured by the side
    /// with most units there.
    pub fn markers(&self, out: &mut Vec<Marker>, viewport: [f32; 2]) {
        out.clear();
        let rules = self.world.rules();
        let a = self.camera.screen_to_world([0.0, 0.0], viewport);
        let b = self.camera.screen_to_world(viewport, viewport);
        let margin = 2.0;
        let visible = |p: [f32; 2]| {
            p[0] >= a[0] - margin && p[0] <= b[0] + margin && p[1] >= a[1] - margin && p[1] <= b[1] + margin
        };
        for c in self.world.cities() {
            let p = c.center();
            let pos = [fx(p.x), fx(p.y)];
            if visible(pos) {
                let progress = c.capture_ticks as f32 / rules.capture_ticks as f32;
                out.push(Marker::city(pos, c.owner, c.capital_of != 0, progress));
            }
        }
        let u = self.world.units();
        if self.camera.scale < LOD_SCALE {
            // Bins of about 6 screen pixels.
            let bin = (6.0 / self.camera.scale).max(1.0);
            let (x0, y0) = (a[0] - margin, a[1] - margin);
            let cols = (((b[0] - a[0]) + 2.0 * margin) / bin).ceil().max(1.0) as usize;
            let rows = (((b[1] - a[1]) + 2.0 * margin) / bin).ceil().max(1.0) as usize;
            let mut counts = vec![[0u32; 10]; cols * rows]; // 9 owners + "in combat"
            for i in 0..u.len() {
                let p = [u.pos_x[i] as f32 / 65536.0, u.pos_y[i] as f32 / 65536.0];
                let (cx, cy) = (((p[0] - x0) / bin).floor(), ((p[1] - y0) / bin).floor());
                if cx < 0.0 || cy < 0.0 || cx >= cols as f32 || cy >= rows as f32 {
                    continue;
                }
                let cell = &mut counts[cy as usize * cols + cx as usize];
                cell[u.owner[i] as usize] += 1;
                cell[9] |= (u.flags[i] & flags::IN_COMBAT != 0) as u32;
            }
            for (k, cell) in counts.iter().enumerate() {
                let total: u32 = cell[..9].iter().sum();
                if total == 0 {
                    continue;
                }
                let owner = (1..9).max_by_key(|&o| (cell[o], std::cmp::Reverse(o))).unwrap_or(1) as u8;
                let pos = [x0 + (k % cols) as f32 * bin + bin / 2.0, y0 + (k / cols) as f32 * bin + bin / 2.0];
                let radius = (1.5 + (total as f32).sqrt() * 0.8).min(4.0) as u8;
                out.push(Marker::density(pos, owner, radius, cell[9] != 0));
            }
            return;
        }
        let mut selected = vec![false; u.len()];
        for h in &self.selection {
            if let Some(i) = u.index_of(*h) {
                selected[i] = true;
            }
        }
        for (i, &sel) in selected.iter().enumerate() {
            let pos = self.unit_pos(i);
            if !visible(pos) {
                continue;
            }
            let mut f = 0;
            if u.kind[i] == UnitKind::Heavy as u8 {
                f |= mflags::HEAVY;
            }
            if u.flags[i] & flags::SHIP != 0 {
                f |= mflags::SHIP;
            }
            if u.flags[i] & flags::IN_COMBAT != 0 {
                f |= mflags::IN_COMBAT;
            }
            if sel {
                f |= mflags::SELECTED;
            }
            out.push(Marker::unit(
                pos,
                u.owner[i],
                f,
                u.hp[i] as f32 / rules.max_hp as f32,
                u.morale[i] as f32 / rules.max_morale as f32,
            ));
        }
    }

    /// Paths of selected units, the path being drawn, the selection box.
    pub fn overlay(&self, o: &mut Overlay, viewport: [f32; 2]) {
        o.clear();
        let px = 1.0 / self.camera.scale;
        let u = self.world.units();
        // Orders of selected units (one line per distinct order).
        let mut shown: Vec<u32> = Vec::new();
        for h in &self.selection {
            let Some(i) = u.index_of(*h) else { continue };
            let mut ord = u.order[i];
            if ord == NO_ORDER || shown.contains(&ord) {
                continue;
            }
            shown.push(ord);
            let mut pts = vec![self.unit_pos(i)];
            let mut wp = u.waypoint[i] as usize;
            while ord != NO_ORDER {
                let o2 = self.world.orders().get(ord);
                for p in &o2.points[wp.min(o2.points.len())..] {
                    pts.push([fx(p.x), fx(p.y)]);
                }
                ord = o2.next;
                wp = 0;
            }
            o.polyline(&pts, 2.0 * px, [1.0, 1.0, 1.0, 0.55]);
        }
        if let Some(path) = &self.drawing {
            o.polyline(path, 3.0 * px, [1.0, 1.0, 1.0, 0.9]);
        }
        if let Some((a, b)) = self.box_select {
            let wa = self.camera.screen_to_world(a, viewport);
            let wb = self.camera.screen_to_world(b, viewport);
            o.fill_rect(wa, wb, [1.0, 1.0, 1.0, 0.08]);
            o.rect(wa, wb, 1.5 * px, [1.0, 1.0, 1.0, 0.8]);
        }
        if let Some(c) = self.selected_city {
            let p = self.world.cities()[c].center();
            let (x, y) = (fx(p.x), fx(p.y));
            o.rect([x - 1.6, y - 1.6], [x + 1.6, y + 1.6], 2.0 * px, [1.0, 1.0, 1.0, 0.9]);
        }
    }
}

#[inline]
pub fn fx(v: Fx) -> f32 {
    v.0 as f32 / 65536.0
}

/// Ramer–Douglas–Peucker simplification to at most `max` points (spec 2.9).
pub fn simplify(pts: &[[f32; 2]], max: usize) -> Vec<[f32; 2]> {
    if pts.len() <= 2 {
        return pts.to_vec();
    }
    let mut eps = 0.05;
    loop {
        let out = rdp(pts, eps);
        if out.len() <= max {
            return out;
        }
        eps *= 1.5;
    }
}

fn rdp(pts: &[[f32; 2]], eps: f32) -> Vec<[f32; 2]> {
    let (a, b) = (pts[0], pts[pts.len() - 1]);
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len = (dx * dx + dy * dy).sqrt().max(1e-6);
    let mut best = (0.0f32, 0usize);
    for (k, p) in pts.iter().enumerate().take(pts.len() - 1).skip(1) {
        let d = ((p[0] - a[0]) * dy - (p[1] - a[1]) * dx).abs() / len;
        if d > best.0 {
            best = (d, k);
        }
    }
    if best.0 > eps {
        let mut left = rdp(&pts[..=best.1], eps);
        let right = rdp(&pts[best.1..], eps);
        left.pop();
        left.extend(right);
        left
    } else {
        vec![a, b]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simplify_caps_points() {
        let pts: Vec<[f32; 2]> = (0..500).map(|k| [k as f32 * 0.1, (k as f32 * 0.2).sin() * 5.0]).collect();
        let s = simplify(&pts, 32);
        assert!(s.len() <= 32 && s.len() > 4);
        assert_eq!(s[0], pts[0]);
        assert_eq!(*s.last().unwrap(), *pts.last().unwrap());
    }

    #[test]
    fn local_commands_reach_the_world() {
        let mut g = Game::new(MatchSettings { map_size: 128, ..Default::default() }).unwrap();
        // Select our three starting units by box and send them somewhere.
        let vp = [1280.0, 720.0];
        g.box_select = Some(([0.0, 0.0], vp));
        g.camera.scale = 2.0;
        g.finish_box(vp, false);
        assert_eq!(g.selection.len(), 3);
        let cap = g.capital_center().unwrap();
        g.order_move(&[[cap[0] + 10.0, cap[1]]], false);
        g.step();
        let u = g.world.units();
        let i = u.index_of(g.selection[0]).unwrap();
        assert_ne!(u.order[i], NO_ORDER);
        // The replay of what happened re-simulates to the same state.
        for _ in 0..60 {
            g.step();
        }
        let r = g.replay();
        let w = r.verify(&Balance::default_balance()).unwrap();
        assert_eq!(w.state_hash(), g.world.state_hash());
    }
}
