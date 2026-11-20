//! Hierarchical flow-field pathfinding (spec 5.4).
//!
//! The map is cut into 64×64 chunks. For each unit type (a table of tile
//! costs) a [`Graph`] holds:
//!
//! * **components**: 4-connected passable regions inside each chunk (with
//!   no corner cutting, 4-connectivity is exactly "reachable");
//! * **portals**: runs of passable tile pairs across each chunk border,
//!   split where a component changes and at most [`PORTAL_MAX`] long; each
//!   run gives two nodes (one per side) joined by an edge;
//! * **intra edges** between the portal nodes of one component, with the
//!   exact travel cost inside the chunk (a local Dijkstra per portal, in
//!   parallel per chunk, when the graph is built).
//!
//! A **coarse field** ([`Graph::coarse`]) is a reverse Dijkstra over that
//! graph from the portals of the target's component: the cost to the
//! target from every portal of the map and the next portal on the way (a
//! tree). It is shared by every unit going to the same chunk region.
//!
//! A **chunk field** ([`Graph::chunk_field`]) turns it into a direction per
//! tile for one chunk: a multi-source Dijkstra inside the chunk, seeded at
//! the exits whose next hop in the tree leaves the chunk. Because the tree
//! has no cycles, every border crossing strictly lowers the cost to go, so
//! units never bounce between two chunks. Units read one byte per tick.
//! Chunk fields are computed only where units are; the exact last part
//! (inside the target's chunk) is a chunk field seeded at the target tile.
//!
//! Everything is integer and deterministic (ties broken by index), so it
//! can run inside the lockstep simulation. This crate does not depend on
//! the simulation: it reads a chunk-major terrain plane and a cost table.

#![forbid(unsafe_code)]

use rayon::prelude::*;
use std::cmp::Reverse;
use std::collections::BinaryHeap;

pub const CHUNK_SHIFT: u32 = 6;
pub const CHUNK: u32 = 1 << CHUNK_SHIFT;
pub const CHUNK_TILES: usize = (CHUNK * CHUNK) as usize;
/// Cost of an impassable tile.
pub const BLOCKED: u32 = u32::MAX;
/// Longest portal run: wider openings are split so units spread over them.
pub const PORTAL_MAX: u32 = 32;
/// Component id of an impassable tile.
pub const NO_COMP: u8 = u8::MAX;
/// Chunk-field byte: no way to the target from here.
pub const NONE: u8 = u8::MAX;
/// Chunk-field byte: this is the target tile.
pub const ARRIVED: u8 = 8;
/// The eight directions; chunk-field bytes `0..8` index this table.
pub const DIRS: [(i32, i32); 8] = [(1, 0), (1, 1), (0, 1), (-1, 1), (-1, 0), (-1, -1), (0, -1), (1, -1)];
/// Diagonal steps cost √2 ≈ 1414/1000 of a straight one.
const DIAG: u64 = 1414;

/// A chunk-major terrain plane (as in the simulation's map) plus the cost
/// of entering each terrain type for one unit type.
#[derive(Clone, Copy)]
pub struct Terrain<'a> {
    pub width: u32,
    pub height: u32,
    /// Terrain id per tile, chunk-major: tile `(x, y)` is at
    /// `chunk * 4096 + (y % 64) * 64 + x % 64`.
    pub tiles: &'a [u8],
    /// Cost of entering a tile, by terrain id; [`BLOCKED`] if impassable.
    pub costs: &'a [u32],
}

impl Terrain<'_> {
    #[inline]
    pub fn index(&self, x: u32, y: u32) -> usize {
        let chunks_w = self.width >> CHUNK_SHIFT;
        let chunk = (y >> CHUNK_SHIFT) * chunks_w + (x >> CHUNK_SHIFT);
        ((chunk as usize) << (2 * CHUNK_SHIFT)) | (((y & (CHUNK - 1)) << CHUNK_SHIFT) | (x & (CHUNK - 1))) as usize
    }

    #[inline]
    pub fn cost(&self, x: u32, y: u32) -> u32 {
        self.costs.get(self.tiles[self.index(x, y)] as usize).copied().unwrap_or(BLOCKED)
    }
}

/// One side of a portal: a run of border tiles in `chunk`, each paired with
/// the tile across the border at `(x + ax, y + ay)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Node {
    pub chunk: u32,
    pub comp: u8,
    /// First tile of the run (global tile coordinates).
    pub x: u16,
    pub y: u16,
    /// Step along the run (0/1) and offset to the tile across (±1).
    pub step: (i8, i8),
    pub across: (i8, i8),
    pub len: u8,
}

impl Node {
    /// Middle tile of the run.
    pub fn mid(&self) -> (u32, u32) {
        let h = (self.len / 2) as i32;
        ((self.x as i32 + self.step.0 as i32 * h) as u32, (self.y as i32 + self.step.1 as i32 * h) as u32)
    }

    pub fn tiles(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        (0..self.len as i32)
            .map(|k| ((self.x as i32 + self.step.0 as i32 * k) as u32, (self.y as i32 + self.step.1 as i32 * k) as u32))
    }
}

/// Where a coarse field leads: a chunk region (component) to reach.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Goal {
    pub chunk: u32,
    pub comp: u8,
}

/// Portal graph of a map for one unit type. Build once per map (and again
/// if terrain changes).
pub struct Graph {
    width: u32,
    height: u32,
    chunks_w: u32,
    costs: Vec<u32>,
    /// Component per tile (chunk-major), [`NO_COMP`] if impassable.
    comp: Vec<u8>,
    nodes: Vec<Node>,
    /// Nodes of chunk `c` are `chunk_nodes[c]..chunk_nodes[c + 1]`.
    chunk_nodes: Vec<u32>,
    /// Adjacency (CSR): edges of node `n` are `adj[adj_start[n]..adj_start[n + 1]]`.
    adj_start: Vec<u32>,
    adj: Vec<(u32, u32)>,
}

/// Per-chunk build result before the global merge.
struct ChunkPart {
    /// Nodes of the four borders in order: left, right, up, down.
    nodes: [Vec<Node>; 4],
}

impl Graph {
    pub fn build(t: &Terrain<'_>) -> Graph {
        assert!(t.width % CHUNK == 0 && t.height % CHUNK == 0, "map size must be a multiple of 64");
        let (chunks_w, chunks_h) = (t.width / CHUNK, t.height / CHUNK);
        let chunks = (chunks_w * chunks_h) as usize;
        let mut comp = vec![NO_COMP; t.tiles.len()];
        // Components, chunk by chunk (in parallel).
        comp.par_chunks_mut(CHUNK_TILES)
            .enumerate()
            .for_each(|(c, plane)| label_chunk(t, c as u32 % chunks_w, c as u32 / chunks_w, plane));
        // Portals: every chunk finds the runs on its four borders; both sides
        // of a border split runs identically, so run k of c's right border
        // pairs with run k of the right neighbour's left border.
        let parts: Vec<ChunkPart> = (0..chunks as u32)
            .into_par_iter()
            .map(|c| {
                let (cx, cy) = (c % chunks_w, c / chunks_w);
                let mut nodes: [Vec<Node>; 4] = Default::default();
                let comp_of = |x: u32, y: u32| comp[t.index(x, y)];
                let (ox, oy) = (cx * CHUNK, cy * CHUNK);
                // (border index, exists, first tile, step along, across offset)
                let borders = [
                    (0, cx > 0, (ox, oy), (0i8, 1i8), (-1i8, 0i8)),
                    (1, cx + 1 < chunks_w, (ox + CHUNK - 1, oy), (0, 1), (1, 0)),
                    (2, cy > 0, (ox, oy), (1, 0), (0, -1)),
                    (3, cy + 1 < chunks_h, (ox, oy + CHUNK - 1), (1, 0), (0, 1)),
                ];
                for (b, exists, (sx, sy), step, across) in borders {
                    if !exists {
                        continue;
                    }
                    let mut run: Option<Node> = None;
                    for k in 0..CHUNK {
                        let x = (sx as i32 + step.0 as i32 * k as i32) as u32;
                        let y = (sy as i32 + step.1 as i32 * k as i32) as u32;
                        let (ax, ay) = ((x as i32 + across.0 as i32) as u32, (y as i32 + across.1 as i32) as u32);
                        let (ca, cb) = (comp_of(x, y), comp_of(ax, ay));
                        let open = ca != NO_COMP && cb != NO_COMP;
                        // Run continues if open, same components both sides, not too long.
                        let continues = run.as_ref().is_some_and(|r| {
                            open && r.comp == ca
                                && comp_of((r.x as i32 + across.0 as i32) as u32, (r.y as i32 + across.1 as i32) as u32)
                                    == cb
                                && (r.len as u32) < PORTAL_MAX
                        });
                        if continues {
                            if let Some(r) = run.as_mut() {
                                r.len += 1;
                            }
                            continue;
                        }
                        if let Some(r) = run.take() {
                            nodes[b].push(r);
                        }
                        if open {
                            run = Some(Node { chunk: c, comp: ca, x: x as u16, y: y as u16, step, across, len: 1 });
                        }
                    }
                    if let Some(r) = run.take() {
                        nodes[b].push(r);
                    }
                }
                ChunkPart { nodes }
            })
            .collect();
        // Merge: node ids in chunk order, borders in order.
        let mut nodes = Vec::new();
        let mut chunk_nodes = Vec::with_capacity(chunks + 1);
        let mut border_start = vec![[0u32; 4]; chunks];
        for (c, p) in parts.iter().enumerate() {
            chunk_nodes.push(nodes.len() as u32);
            for (start, list) in border_start[c].iter_mut().zip(&p.nodes) {
                *start = nodes.len() as u32;
                nodes.extend_from_slice(list);
            }
        }
        chunk_nodes.push(nodes.len() as u32);
        // Step costs bound the bucket queue.
        let passable = t.costs.iter().copied().filter(|&c| c != BLOCKED);
        let min_step = passable.clone().min().unwrap_or(1);
        let max_step = (passable.max().unwrap_or(1) as u64 * DIAG / 1000) as u32 + 1;
        // Intra edges: exact local costs between portal mids of one component
        // (one local Dijkstra per portal), in parallel per chunk.
        let intra: Vec<Vec<(u32, u32, u32)>> = (0..chunks)
            .into_par_iter()
            .map(|c| {
                let (s, e) = (chunk_nodes[c] as usize, chunk_nodes[c + 1] as usize);
                let (cx, cy) = (c as u32 % chunks_w, c as u32 / chunks_w);
                let mut out = Vec::new();
                if e - s < 2 {
                    return out;
                }
                let mut cost = vec![BLOCKED; PAD * PAD];
                padded_costs(t, &comp, cx, cy, &mut cost);
                let mut dist = vec![BLOCKED; PAD * PAD];
                let mut q = Buckets::new(min_step, max_step);
                for i in s..e - 1 {
                    let (mx, my) = nodes[i].mid();
                    local_costs(&cost, (mx & (CHUNK - 1), my & (CHUNK - 1)), &mut dist, &mut q);
                    for j in i + 1..e {
                        if nodes[j].comp != nodes[i].comp {
                            continue;
                        }
                        let (jx, jy) = nodes[j].mid();
                        let d = dist[((jy & (CHUNK - 1)) as usize + 1) * PAD + (jx & (CHUNK - 1)) as usize + 1];
                        if d != BLOCKED {
                            out.push((i as u32, j as u32, d));
                        }
                    }
                }
                out
            })
            .collect();
        // Edges.
        let mut edges: Vec<Vec<(u32, u32)>> = vec![Vec::new(); nodes.len()];
        for list in &intra {
            for &(i, j, d) in list {
                edges[i as usize].push((j, d));
                edges[j as usize].push((i, d));
            }
        }
        for c in 0..chunks {
            let (cx, cy) = (c as u32 % chunks_w, c as u32 / chunks_w);
            // Across borders (right and down, both directions).
            let right = (cx + 1 < chunks_w).then_some(c + 1);
            let down = (cy + 1 < chunks_h).then_some(c + chunks_w as usize);
            for (b, nb, n) in [(1usize, 0usize, right), (3, 2, down)] {
                let Some(n) = n else { continue };
                let mine = border_start[c][b]..parts[c].nodes[b].len() as u32 + border_start[c][b];
                let theirs = border_start[n][nb];
                debug_assert_eq!(parts[c].nodes[b].len(), parts[n].nodes[nb].len());
                for (k, a) in mine.enumerate() {
                    let bnode = theirs + k as u32;
                    let (na, nbn) = (nodes[a as usize], nodes[bnode as usize]);
                    let cost =
                        (t.cost(na.mid().0, na.mid().1) / 2).saturating_add(t.cost(nbn.mid().0, nbn.mid().1) / 2);
                    edges[a as usize].push((bnode, cost));
                    edges[bnode as usize].push((a, cost));
                }
            }
        }
        let mut adj_start = Vec::with_capacity(nodes.len() + 1);
        let mut adj = Vec::new();
        for e in &edges {
            adj_start.push(adj.len() as u32);
            adj.extend_from_slice(e);
        }
        adj_start.push(adj.len() as u32);
        Graph {
            width: t.width,
            height: t.height,
            chunks_w,
            costs: t.costs.to_vec(),
            comp,
            nodes,
            chunk_nodes,
            adj_start,
            adj,
        }
    }

    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    pub fn chunks_w(&self) -> u32 {
        self.chunks_w
    }

    #[inline]
    fn index(&self, x: u32, y: u32) -> usize {
        let chunk = (y >> CHUNK_SHIFT) * self.chunks_w + (x >> CHUNK_SHIFT);
        ((chunk as usize) << (2 * CHUNK_SHIFT)) | (((y & (CHUNK - 1)) << CHUNK_SHIFT) | (x & (CHUNK - 1))) as usize
    }

    /// The chunk region containing tile `(x, y)`, or `None` if impassable.
    pub fn goal_of(&self, x: u32, y: u32) -> Option<Goal> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let comp = self.comp[self.index(x, y)];
        (comp != NO_COMP).then_some(Goal { chunk: (y >> CHUNK_SHIFT) * self.chunks_w + (x >> CHUNK_SHIFT), comp })
    }

    /// Reverse Dijkstra over the portal graph: cost to reach `goal` from
    /// every node ([`BLOCKED`] if unreachable) and the next node on the way.
    /// Portals of the goal region itself are at 0 and have no next node.
    pub fn coarse(&self, goal: Goal) -> Coarse {
        let mut dist = vec![BLOCKED; self.nodes.len()];
        let mut next = vec![u32::MAX; self.nodes.len()];
        let mut heap = BinaryHeap::new();
        for n in self.chunk_nodes[goal.chunk as usize]..self.chunk_nodes[goal.chunk as usize + 1] {
            if self.nodes[n as usize].comp == goal.comp {
                dist[n as usize] = 0;
                heap.push(Reverse((0u32, n)));
            }
        }
        while let Some(Reverse((d, n))) = heap.pop() {
            if d > dist[n as usize] {
                continue;
            }
            for &(m, w) in &self.adj[self.adj_start[n as usize] as usize..self.adj_start[n as usize + 1] as usize] {
                let nd = d.saturating_add(w);
                if nd < dist[m as usize] {
                    dist[m as usize] = nd;
                    next[m as usize] = n;
                    heap.push(Reverse((nd, m)));
                }
            }
        }
        Coarse { dist, next }
    }

    /// Direction field of chunk `(cx, cy)` toward a goal. Seeds:
    /// * `target = Some((x, y))` (a tile inside this chunk): that tile, and
    ///   nothing else — the exact last stretch;
    /// * otherwise the border runs whose next hop in `coarse` is across the
    ///   border, at their coarse cost.
    ///
    /// `out[local]` gets a direction index into [`DIRS`] (which may point out
    /// of the chunk at border tiles), [`ARRIVED`] at the target, or [`NONE`].
    pub fn chunk_field(
        &self,
        t: &Terrain<'_>,
        cx: u32,
        cy: u32,
        coarse: &Coarse,
        target: Option<(u32, u32)>,
        out: &mut [u8],
    ) {
        let c = (cy * self.chunks_w + cx) as usize;
        let (ox, oy) = (cx * CHUNK, cy * CHUNK);
        // Costs in a padded array: no bounds checks in the inner loop.
        let mut cost = vec![BLOCKED; PAD * PAD];
        padded_costs(t, &self.comp, cx, cy, &mut cost);
        let pad = |lx: u32, ly: u32| (ly as usize + 1) * PAD + lx as usize + 1;
        let unpad = |p: usize| ((p / PAD - 1) << CHUNK_SHIFT as usize) | (p % PAD - 1);
        let mut dist = vec![BLOCKED; PAD * PAD];
        out[..CHUNK_TILES].fill(NONE);
        let mut heap = BinaryHeap::new();
        let mut seed = |lx: u32, ly: u32, d: u32, dir: u8, dist: &mut Vec<u32>, out: &mut [u8]| {
            let p = pad(lx, ly);
            if d < dist[p] {
                dist[p] = d;
                out[unpad(p)] = dir;
                heap.push(Reverse((d, p as u32)));
            }
        };
        match target {
            Some((tx, ty)) => {
                if tx >> CHUNK_SHIFT == cx && ty >> CHUNK_SHIFT == cy && cost[pad(tx - ox, ty - oy)] != BLOCKED {
                    seed(tx - ox, ty - oy, 0, ARRIVED, &mut dist, out);
                }
            }
            None => {
                for n in self.chunk_nodes[c]..self.chunk_nodes[c + 1] {
                    let node = self.nodes[n as usize];
                    // Only exits whose way to the goal leaves the chunk here.
                    let next = coarse.next[n as usize];
                    if next == u32::MAX || self.nodes[next as usize].chunk == node.chunk {
                        continue;
                    }
                    let d = coarse.dist[n as usize];
                    let dir = DIRS
                        .iter()
                        .position(|&v| v == (node.across.0 as i32, node.across.1 as i32))
                        .expect("axis direction") as u8;
                    for (x, y) in node.tiles() {
                        seed(x - ox, y - oy, d, dir, &mut dist, out);
                    }
                }
            }
        }
        // Reverse Dijkstra: moving from v into u costs cost(u) (× √2
        // diagonally); v = u - DIRS[k] gets direction k.
        const OFF: [isize; 8] = [
            1,
            PAD as isize + 1,
            PAD as isize,
            PAD as isize - 1,
            -1,
            -(PAD as isize) - 1,
            -(PAD as isize),
            -(PAD as isize) + 1,
        ];
        while let Some(Reverse((d, u))) = heap.pop() {
            let u = u as usize;
            if d > dist[u] {
                continue;
            }
            let cu = cost[u];
            for (k, &o) in OFF.iter().enumerate() {
                let v = (u as isize - o) as usize;
                if cost[v] == BLOCKED {
                    continue;
                }
                let step = if k % 2 == 1 {
                    // No corner cutting: the two tiles beside the diagonal.
                    let (dx, dy) = DIRS[k];
                    if cost[(v as isize + dx as isize) as usize] == BLOCKED
                        || cost[(v as isize + dy as isize * PAD as isize) as usize] == BLOCKED
                    {
                        continue;
                    }
                    (cu as u64 * DIAG / 1000) as u32
                } else {
                    cu
                };
                let nd = d.saturating_add(step).min(BLOCKED - 1);
                if nd < dist[v] {
                    dist[v] = nd;
                    out[unpad(v)] = k as u8;
                    heap.push(Reverse((nd, v as u32)));
                }
            }
        }
    }

    /// Cost of entering tile `(x, y)` with this graph's cost table.
    #[inline]
    pub fn tile_cost(&self, t: &Terrain<'_>, x: u32, y: u32) -> u32 {
        let i = self.index(x, y);
        if self.comp[i] == NO_COMP {
            return BLOCKED;
        }
        self.costs.get(t.tiles[i] as usize).copied().unwrap_or(BLOCKED)
    }

    /// The node on the other side of node `n`'s border run.
    #[cfg(test)]
    fn across_node(&self, n: u32) -> Option<u32> {
        self.adj[self.adj_start[n as usize] as usize..self.adj_start[n as usize + 1] as usize]
            .iter()
            .map(|&(m, _)| m)
            .find(|&m| self.nodes[m as usize].chunk != self.nodes[n as usize].chunk)
    }
}

/// Result of [`Graph::coarse`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Coarse {
    /// Cost to the goal from each node ([`BLOCKED`] if unreachable).
    pub dist: Vec<u32>,
    /// Next node on the way (`u32::MAX` at the goal or if unreachable).
    pub next: Vec<u32>,
}

/// Side of a padded per-chunk array: the chunk plus a ring of blocked tiles,
/// so neighbour lookups need no bounds checks.
const PAD: usize = CHUNK as usize + 2;

/// Costs of chunk `(cx, cy)` in a padded `PAD × PAD` array ([`BLOCKED`]
/// outside the chunk and on impassable tiles).
fn padded_costs(t: &Terrain<'_>, comp: &[u8], cx: u32, cy: u32, out: &mut [u32]) {
    out.fill(BLOCKED);
    let (ox, oy) = (cx * CHUNK, cy * CHUNK);
    let base = ((cy * (t.width >> CHUNK_SHIFT) + cx) as usize) << (2 * CHUNK_SHIFT);
    for ly in 0..CHUNK {
        for lx in 0..CHUNK {
            if comp[base + ((ly << CHUNK_SHIFT) | lx) as usize] != NO_COMP {
                out[(ly as usize + 1) * PAD + lx as usize + 1] = t.cost(ox + lx, oy + ly);
            }
        }
    }
}

/// Forward Dijkstra inside one chunk from local tile `from`, over the
/// padded costs `cost` (see [`padded_costs`]): travel cost to every tile
/// (cost of entering each tile, √2 diagonally, no corner cutting) in a
/// padded `dist`, [`BLOCKED`] if unreachable.
///
/// Uses a cyclic bucket queue with buckets as wide as the cheapest step:
/// no step is shorter, so every tile in the current bucket already has its
/// final distance — exactly Dijkstra's result, in linear time.
fn local_costs(cost: &[u32], from: (u32, u32), dist: &mut [u32], q: &mut Buckets) {
    // Neighbour offsets in the padded array, in DIRS order.
    const OFF: [isize; 8] = [
        1,
        PAD as isize + 1,
        PAD as isize,
        PAD as isize - 1,
        -1,
        -(PAD as isize) - 1,
        -(PAD as isize),
        -(PAD as isize) + 1,
    ];
    dist.fill(BLOCKED);
    let start = (from.1 as usize + 1) * PAD + from.0 as usize + 1;
    dist[start] = 0;
    q.reset();
    q.push(0, start as u32);
    while let Some((d, l)) = q.pop() {
        let l = l as usize;
        if d > dist[l] {
            continue;
        }
        for (k, &o) in OFF.iter().enumerate() {
            let n = (l as isize + o) as usize;
            let c = cost[n];
            if c == BLOCKED {
                continue;
            }
            let step = if k % 2 == 1 {
                // Diagonal: both side tiles must be open (no corner cutting).
                let (dx, dy) = DIRS[k];
                if cost[(l as isize + dx as isize) as usize] == BLOCKED
                    || cost[(l as isize + dy as isize * PAD as isize) as usize] == BLOCKED
                {
                    continue;
                }
                (c as u64 * DIAG / 1000) as u32
            } else {
                c
            };
            let nd = d.saturating_add(step).min(BLOCKED - 1);
            if nd < dist[n] {
                dist[n] = nd;
                q.push(nd, n as u32);
            }
        }
    }
}

/// Cyclic bucket queue for single-source Dijkstra with step costs in
/// `[width, max_step]`. Pops in non-decreasing bucket order; within a
/// bucket in insertion order (deterministic).
struct Buckets {
    width: u32,
    buckets: Vec<Vec<(u32, u32)>>,
    cur: usize,
    /// Absolute bucket index of `cur`.
    base: u64,
    len: usize,
}

impl Buckets {
    fn new(width: u32, max_step: u32) -> Buckets {
        let n = (max_step / width.max(1)) as usize + 2;
        Buckets { width: width.max(1), buckets: vec![Vec::new(); n], cur: 0, base: 0, len: 0 }
    }

    fn reset(&mut self) {
        for b in &mut self.buckets {
            b.clear();
        }
        self.cur = 0;
        self.base = 0;
        self.len = 0;
    }

    fn push(&mut self, d: u32, v: u32) {
        let abs = (d / self.width) as u64;
        debug_assert!(abs >= self.base && abs < self.base + self.buckets.len() as u64);
        let k = (self.cur + (abs - self.base) as usize) % self.buckets.len();
        self.buckets[k].push((d, v));
        self.len += 1;
    }

    fn pop(&mut self) -> Option<(u32, u32)> {
        if self.len == 0 {
            return None;
        }
        loop {
            if let Some(e) = self.buckets[self.cur].pop() {
                self.len -= 1;
                return Some(e);
            }
            self.cur = (self.cur + 1) % self.buckets.len();
            self.base += 1;
        }
    }
}

/// 4-connected components of the passable tiles of one chunk.
fn label_chunk(t: &Terrain<'_>, cx: u32, cy: u32, plane: &mut [u8]) {
    let (ox, oy) = (cx * CHUNK, cy * CHUNK);
    let cost = |lx: u32, ly: u32| t.cost(ox + lx, oy + ly);
    let mut count: u8 = 0;
    let mut stack = Vec::new();
    for start in 0..CHUNK_TILES as u32 {
        let (sx, sy) = (start & (CHUNK - 1), start >> CHUNK_SHIFT);
        if plane[start as usize] != NO_COMP || cost(sx, sy) == BLOCKED {
            continue;
        }
        // More than 254 regions in one chunk is not a real map; merge the rest.
        let id = count.min(NO_COMP - 1);
        if count < NO_COMP - 1 {
            count += 1;
        }
        plane[start as usize] = id;
        stack.push(start);
        while let Some(l) = stack.pop() {
            let (x, y) = (l & (CHUNK - 1), l >> CHUNK_SHIFT);
            for (dx, dy) in [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)] {
                let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                if nx < 0 || ny < 0 || nx >= CHUNK as i32 || ny >= CHUNK as i32 {
                    continue;
                }
                let n = ((ny as u32) << CHUNK_SHIFT) | nx as u32;
                if plane[n as usize] == NO_COMP && cost(nx as u32, ny as u32) != BLOCKED {
                    plane[n as usize] = id;
                    stack.push(n);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Terrain ids for the tests: 0 = plain (1000), 1 = forest (2000), 2 = wall.
    const COSTS: [u32; 3] = [1000, 2000, BLOCKED];

    /// A map from ASCII rows ('#' wall, 'f' forest, else plain), chunk-major.
    fn map(w: u32, h: u32, rows: &[&str]) -> Vec<u8> {
        let mut tiles = vec![0u8; (w * h) as usize];
        let t = Terrain { width: w, height: h, tiles: &[], costs: &COSTS };
        for (y, row) in rows.iter().enumerate() {
            for (x, ch) in row.chars().enumerate() {
                let v = match ch {
                    '#' => 2,
                    'f' => 1,
                    _ => 0,
                };
                tiles[t.index(x as u32, y as u32)] = v;
            }
        }
        tiles
    }

    /// Fields for every chunk a walk passes through, as a simulation would
    /// request them.
    struct Nav<'a> {
        g: &'a Graph,
        t: Terrain<'a>,
        coarse: Coarse,
        target: (u32, u32),
        goal: Goal,
    }

    impl Nav<'_> {
        fn dir(&self, x: u32, y: u32) -> u8 {
            let (cx, cy) = (x / CHUNK, y / CHUNK);
            let mut out = vec![0u8; CHUNK_TILES];
            let here = self.g.goal_of(x, y).expect("walking on passable tiles");
            let target = (here == self.goal).then_some(self.target);
            self.g.chunk_field(&self.t, cx, cy, &self.coarse, target, &mut out);
            out[(((y % CHUNK) << CHUNK_SHIFT) | (x % CHUNK)) as usize]
        }

        /// Follows the fields tile by tile; returns the number of steps.
        fn walk(&self, mut x: u32, mut y: u32, max: u32) -> Option<u32> {
            for steps in 0..max {
                match self.dir(x, y) {
                    ARRIVED => return Some(steps),
                    NONE => return None,
                    d => {
                        let (dx, dy) = DIRS[d as usize];
                        x = (x as i32 + dx) as u32;
                        y = (y as i32 + dy) as u32;
                        assert_ne!(self.t.cost(x, y), BLOCKED, "walked into a wall at {x},{y}");
                    }
                }
            }
            None
        }
    }

    fn nav<'a>(g: &'a Graph, t: Terrain<'a>, target: (u32, u32)) -> Nav<'a> {
        let goal = g.goal_of(target.0, target.1).expect("target passable");
        Nav { g, t, coarse: g.coarse(goal), target, goal }
    }

    #[test]
    fn open_field_goes_straight() {
        let tiles = map(256, 256, &[]);
        let t = Terrain { width: 256, height: 256, tiles: &tiles, costs: &COSTS };
        let g = Graph::build(&t);
        let n = nav(&g, t, (200, 150));
        let steps = n.walk(10, 20, 1000).expect("reaches the target");
        // Octile distance is 190 steps (130 diagonal + 60 straight). The
        // hierarchy (portal runs, node mid-points) costs a little extra.
        assert!((190..=218).contains(&steps), "{steps} steps (≤ 15% over the optimum)");
    }

    #[test]
    fn maze_of_walls_across_chunks() {
        // Long walls with gaps at alternating ends force a snake path
        // through several chunks.
        let mut rows: Vec<String> = vec![".".repeat(256); 256];
        for (k, wy) in [40usize, 90, 140, 190].iter().enumerate() {
            let mut r: Vec<char> = rows[*wy].chars().collect();
            for (x, c) in r.iter_mut().enumerate() {
                let gap = if k % 2 == 0 { x >= 240 } else { x < 16 };
                if !gap {
                    *c = '#';
                }
            }
            rows[*wy] = r.into_iter().collect();
        }
        let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
        let tiles = map(256, 256, &refs);
        let t = Terrain { width: 256, height: 256, tiles: &tiles, costs: &COSTS };
        let g = Graph::build(&t);
        let n = nav(&g, t, (128, 230));
        let steps = n.walk(128, 10, 5000).expect("finds the way through the gaps");
        assert!(steps > 4 * 200, "should snake around the walls, took {steps}");
    }

    #[test]
    fn unreachable_target() {
        let mut rows: Vec<String> = vec![".".repeat(128); 128];
        rows[64] = "#".repeat(128);
        let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
        let tiles = map(128, 128, &refs);
        let t = Terrain { width: 128, height: 128, tiles: &tiles, costs: &COSTS };
        let g = Graph::build(&t);
        let n = nav(&g, t, (10, 100));
        assert_eq!(n.walk(10, 10, 1000), None);
    }

    #[test]
    fn heavier_costs_steer_around_forest() {
        // A narrow, deep forest straight across the way: 60 steps through it
        // cost ~100 plain steps, going around ~71, so the field goes around.
        let mut rows: Vec<String> = vec![".".repeat(128); 128];
        for row in rows.iter_mut().take(80).skip(40) {
            let mut r: Vec<char> = row.chars().collect();
            for c in r.iter_mut().take(70).skip(50) {
                *c = 'f';
            }
            *row = r.into_iter().collect();
        }
        let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
        let tiles = map(128, 128, &refs);
        let t = Terrain { width: 128, height: 128, tiles: &tiles, costs: &COSTS };
        let g = Graph::build(&t);
        let n = nav(&g, t, (60, 95));
        let (mut x, mut y) = (60u32, 25u32);
        let mut forest = 0;
        for _ in 0..300 {
            match n.dir(x, y) {
                ARRIVED => break,
                NONE => panic!("lost"),
                d => {
                    let (dx, dy) = DIRS[d as usize];
                    x = (x as i32 + dx) as u32;
                    y = (y as i32 + dy) as u32;
                    forest += (t.cost(x, y) == 2000) as u32;
                }
            }
        }
        assert_eq!((x, y), (60, 95));
        assert!(forest < 10, "crossed {forest} forest tiles");
    }

    #[test]
    fn portals_pair_up_and_build_is_deterministic() {
        let rows: Vec<String> =
            (0..128).map(|y| (0..128).map(|x| if (x * 7 + y * 13) % 11 == 0 { '#' } else { '.' }).collect()).collect();
        let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
        let tiles = map(128, 128, &refs);
        let t = Terrain { width: 128, height: 128, tiles: &tiles, costs: &COSTS };
        let a = Graph::build(&t);
        let b = Graph::build(&t);
        assert_eq!(a.nodes(), b.nodes());
        for n in 0..a.nodes().len() as u32 {
            let m = a.across_node(n).expect("every portal side has a partner");
            assert_eq!(a.across_node(m), Some(n));
            let (p, q) = (a.nodes()[n as usize], a.nodes()[m as usize]);
            assert_eq!(p.len, q.len);
            assert_eq!((p.across.0 as i32, p.across.1 as i32), (-(q.across.0 as i32), -(q.across.1 as i32)));
        }
    }
}
