//! Flow-field pathfinding inside the simulation (spec 5.4, tick phase 3).
//!
//! Built on the `pathing` crate. Per unit type there is one portal graph
//! of the map (built when first needed). Units far from their target, or
//! with no straight line to it, read a direction byte from a field:
//!
//! * a **coarse field** per target region (chunk + connected component),
//!   shared by every unit going there;
//! * **chunk fields** derived from it, only for chunks where units are;
//! * an **exact field** for the last stretch inside the target region.
//!
//! A unit that finds a field missing walks straight and files a request.
//! Requests are processed at the start of the next tick in a fixed order
//! with a fixed budget, so when a field becomes available never depends on
//! timing or thread count. Unused fields are dropped every few seconds.

use crate::balance::Rules;
use crate::map::{Map, Terrain};
use crate::pathfind::terrain_costs;
use crate::unit::{NO_ORDER, UnitHandle, UnitKind, UnitStore};
use pathing::{CHUNK_SHIFT, CHUNK_TILES, Coarse, Goal, Graph};
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Within this many tiles (Chebyshev) of its target a unit walks straight
/// if the line is clear (checked every [`LOS_INTERVAL`] ticks).
pub const NEAR: u32 = 24;
pub const LOS_INTERVAL: u32 = 8;
/// "No field" in [`UnitStore::path`].
pub const NO_PATH: u32 = u32::MAX;
/// New coarse fields per tick (each is a Dijkstra over the portal graph).
pub const COARSE_PER_TICK: usize = 4;
/// New chunk and exact fields per tick (each is a Dijkstra over 4096 tiles).
pub const FIELDS_PER_TICK: usize = 64;
/// Unused fields are dropped every this many ticks.
pub const GC_INTERVAL: u32 = 150;
/// Above this many chunk fields (4 KiB each), unused ones are dropped.
pub const MAX_FIELDS: usize = 8192;

/// What a unit asks for when its field is missing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Request {
    /// Build the portal graphs (first use).
    Graphs,
    /// A coarse field for `(goal, kind)`; `unit` gets it assigned.
    Entry { goal: Goal, kind: u8, unit: UnitHandle },
    /// The chunk field of `chunk` for coarse field `entry`.
    Chunk { entry: u32, chunk: u32 },
    /// The exact field toward `tile` (inside the goal's chunk).
    Exact { tile: (u32, u32), kind: u8 },
}

/// Answer of [`Flow::direction`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Steer {
    /// Direction index into [`pathing::DIRS`].
    Dir(u8),
    /// Walk straight (arrived, unreachable, or no field needed).
    Straight,
    /// Walk straight for now; this field is missing.
    Need(Request),
}

struct Entry {
    goal: Goal,
    kind: u8,
    coarse: Coarse,
    /// Chunk field slot per chunk, or [`NO_PATH`].
    chunks: Vec<u32>,
}

#[derive(Default)]
pub struct Flow {
    graphs: [Option<Arc<Graph>>; UnitKind::COUNT],
    entries: Vec<Option<Entry>>,
    index: BTreeMap<(Goal, u8), u32>,
    free_entries: Vec<u32>,
    fields: Vec<Box<[u8]>>,
    free_fields: Vec<u32>,
    exact: BTreeMap<((u32, u32), u8), u32>,
    /// Requests filed during the last tick, processed at the next.
    pub requests: Vec<Request>,
    /// Fields in use (for stats and tests).
    live_fields: usize,
}

fn local(x: u32, y: u32) -> usize {
    (((y & (pathing::CHUNK - 1)) << CHUNK_SHIFT) | (x & (pathing::CHUNK - 1))) as usize
}

impl Flow {
    /// Forgets everything (the terrain changed).
    pub fn reset(&mut self) {
        *self = Flow::default();
    }

    /// Builds the portal graphs now (match setup) instead of on the first
    /// move order. The graphs depend only on terrain, so results are the
    /// same either way; this only moves the cost out of the game.
    pub fn build_graphs(&mut self, map: &Map, rules: &Rules) {
        let costs: [[u32; Terrain::COUNT]; UnitKind::COUNT] =
            [terrain_costs(rules, UnitKind::Light), terrain_costs(rules, UnitKind::Heavy)];
        let terrain = |k: usize| pathing::Terrain {
            width: map.width(),
            height: map.height(),
            tiles: map.terrain_plane(),
            costs: &costs[k],
        };
        let (a, b) = rayon::join(|| Graph::build(&terrain(0)), || Graph::build(&terrain(1)));
        self.graphs = [Some(Arc::new(a)), Some(Arc::new(b))];
    }

    pub fn graphs_ready(&self) -> bool {
        self.graphs.iter().all(Option::is_some)
    }

    /// Number of live chunk/exact fields and coarse fields.
    pub fn stats(&self) -> (usize, usize) {
        (self.live_fields, self.index.len())
    }

    /// How unit `unit` (path cache `path`, of `kind`, standing at tile
    /// `(ux, uy)`) should move toward tile `(tx, ty)`.
    #[inline]
    pub fn direction(&self, unit: UnitHandle, kind: usize, path: u32, u: (u32, u32), t: (u32, u32)) -> Steer {
        let Some(g) = &self.graphs[kind] else { return Steer::Need(Request::Graphs) };
        let Some(goal) = g.goal_of(t.0, t.1) else { return Steer::Straight };
        let Some(here) = g.goal_of(u.0, u.1) else { return Steer::Straight };
        let k = kind as u8;
        if here == goal {
            return match self.exact.get(&(t, k)) {
                Some(&slot) => Self::dir_of(self.fields[slot as usize][local(u.0, u.1)]),
                None => Steer::Need(Request::Exact { tile: t, kind: k }),
            };
        }
        let entry = self.entries.get(path as usize).and_then(Option::as_ref);
        match entry {
            Some(e) if e.goal == goal && e.kind == k => match e.chunks[here.chunk as usize] {
                NO_PATH => Steer::Need(Request::Chunk { entry: path, chunk: here.chunk }),
                slot => Self::dir_of(self.fields[slot as usize][local(u.0, u.1)]),
            },
            _ => Steer::Need(Request::Entry { goal, kind: k, unit }),
        }
    }

    #[inline]
    fn dir_of(b: u8) -> Steer {
        if (b as usize) < pathing::DIRS.len() { Steer::Dir(b) } else { Steer::Straight }
    }

    fn alloc_field(&mut self) -> u32 {
        self.live_fields += 1;
        match self.free_fields.pop() {
            Some(s) => s,
            None => {
                self.fields.push(vec![pathing::NONE; CHUNK_TILES].into_boxed_slice());
                (self.fields.len() - 1) as u32
            }
        }
    }

    fn free_field(&mut self, slot: u32) {
        self.live_fields -= 1;
        self.free_fields.push(slot);
    }

    /// Phase 3: serves the requests filed last tick, within budget, in a
    /// fixed order. `units` gets coarse-field assignments.
    pub fn process(&mut self, map: &Map, rules: &Rules, units: &mut UnitStore, tick: u32) {
        let mut reqs = std::mem::take(&mut self.requests);
        reqs.sort_unstable();
        reqs.dedup();
        let costs: [[u32; Terrain::COUNT]; UnitKind::COUNT] =
            [terrain_costs(rules, UnitKind::Light), terrain_costs(rules, UnitKind::Heavy)];
        let terrain = |k: usize| pathing::Terrain {
            width: map.width(),
            height: map.height(),
            tiles: map.terrain_plane(),
            costs: &costs[k],
        };
        if !self.graphs_ready() && !reqs.is_empty() {
            self.build_graphs(map, rules);
        }
        if !self.graphs_ready() {
            return;
        }
        // Coarse fields: existing ones are assigned; up to COARSE_PER_TICK
        // new keys (in sorted order) are computed in parallel.
        let mut new_keys: Vec<(Goal, u8)> = Vec::new();
        for r in &reqs {
            if let Request::Entry { goal, kind, .. } = *r {
                let key = (goal, kind);
                if !self.index.contains_key(&key) && !new_keys.contains(&key) && new_keys.len() < COARSE_PER_TICK {
                    new_keys.push(key);
                }
            }
        }
        let graphs = self.graphs.clone();
        let computed: Vec<Coarse> = new_keys
            .par_iter()
            .map(|&(goal, k)| graphs[k as usize].as_ref().expect("graphs ready").coarse(goal))
            .collect();
        let chunks = (map.chunks_w() * map.chunks_h()) as usize;
        for ((goal, kind), coarse) in new_keys.into_iter().zip(computed) {
            let e = Entry { goal, kind, coarse, chunks: vec![NO_PATH; chunks] };
            let id = match self.free_entries.pop() {
                Some(id) => {
                    self.entries[id as usize] = Some(e);
                    id
                }
                None => {
                    self.entries.push(Some(e));
                    (self.entries.len() - 1) as u32
                }
            };
            self.index.insert((goal, kind), id);
        }
        for r in &reqs {
            if let Request::Entry { goal, kind, unit } = *r {
                if let (Some(&id), Some(i)) = (self.index.get(&(goal, kind)), units.index_of(unit)) {
                    units.path[i] = id;
                }
            }
        }
        // Chunk and exact fields, up to FIELDS_PER_TICK, in parallel.
        #[derive(Clone, Copy)]
        enum Job {
            Chunk { entry: u32, chunk: u32, slot: u32 },
            Exact { tile: (u32, u32), kind: u8, slot: u32 },
        }
        let mut jobs = Vec::new();
        for r in &reqs {
            if jobs.len() >= FIELDS_PER_TICK {
                break;
            }
            match *r {
                Request::Chunk { entry, chunk } => {
                    let Some(Some(e)) = self.entries.get(entry as usize) else { continue };
                    if e.chunks[chunk as usize] == NO_PATH {
                        let slot = self.alloc_field();
                        if let Some(Some(e)) = self.entries.get_mut(entry as usize) {
                            e.chunks[chunk as usize] = slot;
                        }
                        jobs.push(Job::Chunk { entry, chunk, slot });
                    }
                }
                Request::Exact { tile, kind } if !self.exact.contains_key(&(tile, kind)) => {
                    let slot = self.alloc_field();
                    self.exact.insert((tile, kind), slot);
                    jobs.push(Job::Exact { tile, kind, slot });
                }
                _ => {}
            }
        }
        // Compute into fresh buffers in parallel, then store.
        let entries = &self.entries;
        let results: Vec<(u32, Box<[u8]>)> = jobs
            .par_iter()
            .map(|job| {
                let mut out = vec![pathing::NONE; CHUNK_TILES].into_boxed_slice();
                match *job {
                    Job::Chunk { entry, chunk, slot } => {
                        let e = entries[entry as usize].as_ref().expect("entry exists");
                        let g = graphs[e.kind as usize].as_ref().expect("graphs ready");
                        let cw = g.chunks_w();
                        g.chunk_field(&terrain(e.kind as usize), chunk % cw, chunk / cw, &e.coarse, None, &mut out);
                        (slot, out)
                    }
                    Job::Exact { tile, kind, slot } => {
                        let g = graphs[kind as usize].as_ref().expect("graphs ready");
                        let empty = Coarse { dist: Vec::new(), next: Vec::new() };
                        let (cx, cy) = (tile.0 >> CHUNK_SHIFT, tile.1 >> CHUNK_SHIFT);
                        g.chunk_field(&terrain(kind as usize), cx, cy, &empty, Some(tile), &mut out);
                        (slot, out)
                    }
                }
            })
            .collect();
        for (slot, out) in results {
            self.fields[slot as usize] = out;
        }
        if tick % GC_INTERVAL == 0 {
            self.collect_garbage(units);
        }
    }

    /// Drops coarse fields no unit refers to, exact fields no unit needs,
    /// and (above [`MAX_FIELDS`]) chunk fields of chunks without users.
    fn collect_garbage(&mut self, units: &UnitStore) {
        let mut entry_used = vec![false; self.entries.len()];
        for i in 0..units.len() {
            if units.order[i] != NO_ORDER {
                if let Some(u) = entry_used.get_mut(units.path[i] as usize) {
                    *u = true;
                }
            }
        }
        for (id, used) in entry_used.iter().enumerate() {
            if *used {
                continue;
            }
            if let Some(e) = self.entries[id].take() {
                for &slot in &e.chunks {
                    if slot != NO_PATH {
                        self.free_field(slot);
                    }
                }
                self.index.remove(&(e.goal, e.kind));
                self.free_entries.push(id as u32);
            }
        }
        // Exact fields are cheap to recompute: keep none between collections.
        let exact = std::mem::take(&mut self.exact);
        for (_, slot) in exact {
            self.free_field(slot);
        }
        if self.live_fields > MAX_FIELDS {
            // Keep only chunk fields where a unit using that entry stands.
            let mut keep = vec![false; self.fields.len()];
            for i in 0..units.len() {
                let Some(Some(e)) = self.entries.get(units.path[i] as usize) else { continue };
                let c = ((units.pos_y[i] >> 22) as usize) * (self.entry_chunks_w(e)) + (units.pos_x[i] >> 22) as usize;
                if let Some(&slot) = e.chunks.get(c) {
                    if slot != NO_PATH {
                        keep[slot as usize] = true;
                    }
                }
            }
            for id in 0..self.entries.len() {
                let Some(e) = self.entries[id].as_mut() else { continue };
                let mut freed = Vec::new();
                for slot in e.chunks.iter_mut() {
                    if *slot != NO_PATH && !keep[*slot as usize] {
                        freed.push(*slot);
                        *slot = NO_PATH;
                    }
                }
                for s in freed {
                    self.free_field(s);
                }
            }
        }
    }

    fn entry_chunks_w(&self, _e: &Entry) -> usize {
        self.graphs[0].as_ref().map(|g| g.chunks_w() as usize).unwrap_or(1)
    }
}
