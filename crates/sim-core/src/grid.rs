//! Spatial grid rebuilt every tick with a counting sort, O(n + occupied cells).
//!
//! The cell size is a power of two in Q16.16 that is at least the contact
//! radius plus twice the largest per-tick move, so a 3×3 neighbourhood built
//! from positions at the start of the tick still contains every unit within
//! contact radius after the movement phase.
//!
//! Cells are grouped in blocks of 32×32. Only blocks that contain units get
//! prefix sums, so a huge, mostly empty map (4096² tiles = 4 M cells) costs
//! as much as the area its armies cover.

const BLOCK_SHIFT: u32 = 5;
const BLOCK: u32 = 1 << BLOCK_SHIFT;
const BLOCK_CELLS: usize = (BLOCK * BLOCK) as usize;
/// Stride of one block's prefix-sum array (cells + 1 end marker).
const STRIDE: usize = BLOCK_CELLS + 1;
const EMPTY: u32 = u32::MAX;

#[derive(Clone, Debug, Default)]
pub struct Grid {
    /// log2 of the cell size in raw Q16.16 units.
    shift: u32,
    cells_w: u32,
    cells_h: u32,
    blocks_w: u32,
    /// Per block: index of its prefix-sum array in `starts`, or [`EMPTY`].
    block_slot: Vec<u32>,
    /// Occupied blocks in ascending block order (as filled this tick).
    occupied: Vec<u32>,
    /// `STRIDE` entries per occupied block: absolute start of each cell in
    /// `cell_units`, plus the end of the last cell.
    starts: Vec<u32>,
    /// Unit indices sorted by (block, cell); within a cell by ascending index.
    pub cell_units: Vec<u32>,
    /// Positions and teams in `cell_units` order: every cell row is one
    /// contiguous slice, which is what the SIMD kernels scan.
    pub sx: Vec<i32>,
    pub sy: Vec<i32>,
    pub steam: Vec<u8>,
    /// Inverse of `cell_units`: position of unit `i` in grid order.
    pub rank: Vec<u32>,
    /// Per unit: block and cell-within-block.
    unit_block: Vec<u32>,
    unit_cell: Vec<u16>,
}

impl Grid {
    /// `min_cell` is the minimum cell size (raw Q16.16), `map_w`/`map_h` in tiles.
    pub fn new(min_cell: i32, map_w: u32, map_h: u32) -> Grid {
        let shift = 32 - ((min_cell.max(1) as u32) - 1).leading_zeros();
        let span = |tiles: u32| -> u32 {
            let raw = (tiles as u64) << 16;
            raw.div_ceil(1u64 << shift) as u32
        };
        let (cells_w, cells_h) = (span(map_w), span(map_h));
        let blocks_w = cells_w.div_ceil(BLOCK);
        let blocks_h = cells_h.div_ceil(BLOCK);
        Grid {
            shift,
            cells_w,
            cells_h,
            blocks_w,
            block_slot: vec![EMPTY; (blocks_w * blocks_h) as usize],
            ..Default::default()
        }
    }

    /// Cell size in raw Q16.16.
    pub fn cell_size(&self) -> i32 {
        1 << self.shift
    }

    #[inline]
    fn cell_xy(&self, x: i32, y: i32) -> (u32, u32) {
        let cx = ((x.max(0) as u32) >> self.shift).min(self.cells_w - 1);
        let cy = ((y.max(0) as u32) >> self.shift).min(self.cells_h - 1);
        (cx, cy)
    }

    #[inline]
    fn block_and_local(&self, cx: u32, cy: u32) -> (u32, usize) {
        let b = (cy >> BLOCK_SHIFT) * self.blocks_w + (cx >> BLOCK_SHIFT);
        let l = ((cy & (BLOCK - 1)) << BLOCK_SHIFT) | (cx & (BLOCK - 1));
        (b, l as usize)
    }

    /// Rebuilds the grid. The team of unit `i` is `team_of[owner[i]]`.
    pub fn rebuild(&mut self, pos_x: &[i32], pos_y: &[i32], owner: &[u8], team_of: &[u8]) {
        let n = pos_x.len();
        for &b in &self.occupied {
            self.block_slot[b as usize] = EMPTY;
        }
        self.occupied.clear();
        self.unit_block.resize(n, 0);
        self.unit_cell.resize(n, 0);
        self.cell_units.resize(n, 0);
        // Pass 1: which blocks are occupied, and where each unit goes.
        for i in 0..n {
            let (cx, cy) = self.cell_xy(pos_x[i], pos_y[i]);
            let (b, l) = self.block_and_local(cx, cy);
            self.unit_block[i] = b;
            self.unit_cell[i] = l as u16;
            if self.block_slot[b as usize] == EMPTY {
                self.block_slot[b as usize] = 0; // mark; slot assigned below
                self.occupied.push(b);
            }
        }
        // Slots in ascending block order keep the layout independent of
        // which unit happened to touch a block first.
        self.occupied.sort_unstable();
        for (k, &b) in self.occupied.iter().enumerate() {
            self.block_slot[b as usize] = k as u32;
        }
        self.starts.clear();
        self.starts.resize(self.occupied.len() * STRIDE, 0);
        // Pass 2: counts.
        for i in 0..n {
            let slot = self.block_slot[self.unit_block[i] as usize] as usize;
            self.starts[slot * STRIDE + self.unit_cell[i] as usize + 1] += 1;
        }
        // Prefix sums across all occupied blocks: absolute positions.
        let mut running = 0u32;
        for k in 0..self.occupied.len() {
            let base = k * STRIDE;
            self.starts[base] = running;
            for c in 1..STRIDE {
                running += self.starts[base + c];
                self.starts[base + c] = running;
            }
        }
        // Pass 3: scatter in ascending index order, using a cursor per cell.
        // `starts[c]` is temporarily the cursor of cell c; shifted back after.
        for i in 0..n {
            let slot = self.block_slot[self.unit_block[i] as usize] as usize;
            let at = &mut self.starts[slot * STRIDE + self.unit_cell[i] as usize];
            self.cell_units[*at as usize] = i as u32;
            *at += 1;
        }
        for k in 0..self.occupied.len() {
            let base = k * STRIDE;
            let end = self.starts[base + STRIDE - 1];
            for c in (1..STRIDE - 1).rev() {
                self.starts[base + c] = self.starts[base + c - 1];
            }
            // Cell 0 started where the previous block ended.
            self.starts[base] = if k == 0 { 0 } else { self.starts[base - 1] };
            self.starts[base + STRIDE - 1] = end;
        }
        self.sx.resize(n, 0);
        self.sy.resize(n, 0);
        self.steam.resize(n, 0);
        self.rank.resize(n, 0);
        for (k, &i) in self.cell_units.iter().enumerate() {
            let i = i as usize;
            self.rank[i] = k as u32;
            (self.sx[k], self.sy[k], self.steam[k]) = (pos_x[i], pos_y[i], team_of[owner[i] as usize]);
        }
    }

    /// Re-copies positions into cell order after units moved (cells stay as
    /// built; the cell size has room for one tick of movement).
    pub fn refresh_positions(&mut self, pos_x: &[i32], pos_y: &[i32]) {
        use rayon::prelude::*;
        const B: usize = 4096;
        let units = &self.cell_units;
        (self.sx.par_chunks_mut(B), self.sy.par_chunks_mut(B)).into_par_iter().enumerate().for_each(|(k, (sx, sy))| {
            for (j, (x, y)) in sx.iter_mut().zip(sy.iter_mut()).enumerate() {
                let i = units[k * B + j] as usize;
                (*x, *y) = (pos_x[i], pos_y[i]);
            }
        });
    }

    /// Index ranges (into `cell_units`, `sx`, `sy`, `steam`) of cells
    /// `x0..=x1` in row `cy`: one range per block touched (usually one).
    #[inline]
    fn row_ranges(&self, cy: u32, x0: u32, x1: u32, f: &mut impl FnMut(std::ops::Range<usize>)) {
        let mut x = x0;
        while x <= x1 {
            let block_end = (x | (BLOCK - 1)).min(x1);
            let (b, l0) = self.block_and_local(x, cy);
            let slot = self.block_slot[b as usize];
            if slot != EMPTY {
                let base = slot as usize * STRIDE;
                let l1 = l0 + (block_end - x) as usize;
                let (a, z) = (self.starts[base + l0] as usize, self.starts[base + l1 + 1] as usize);
                if a < z {
                    f(a..z);
                }
            }
            x = block_end + 1;
        }
    }

    /// Cell coordinates of unit `i` (as of the last rebuild).
    #[inline]
    pub fn cell_of_unit(&self, i: usize) -> (u32, u32) {
        let (b, l) = (self.unit_block[i], self.unit_cell[i] as u32);
        let (bx, by) = (b % self.blocks_w, b / self.blocks_w);
        ((bx << BLOCK_SHIFT) | (l & (BLOCK - 1)), (by << BLOCK_SHIFT) | (l >> BLOCK_SHIFT))
    }

    /// Calls `f(range)` for the slices covering the 3×3 cells around cell `(cx, cy)`.
    #[inline]
    pub fn ranges_around_cell(&self, cx: u32, cy: u32, mut f: impl FnMut(std::ops::Range<usize>)) {
        let x0 = cx.saturating_sub(1);
        let x1 = (cx + 1).min(self.cells_w - 1);
        for gy in cy.saturating_sub(1)..=(cy + 1).min(self.cells_h - 1) {
            self.row_ranges(gy, x0, x1, &mut f);
        }
    }

    /// Calls `f(range)` for the slices covering the 3×3 cells around `(x, y)`.
    #[inline]
    pub fn ranges_near(&self, x: i32, y: i32, mut f: impl FnMut(std::ops::Range<usize>)) {
        let (cx, cy) = self.cell_xy(x, y);
        let x0 = cx.saturating_sub(1);
        let x1 = (cx + 1).min(self.cells_w - 1);
        for gy in cy.saturating_sub(1)..=(cy + 1).min(self.cells_h - 1) {
            self.row_ranges(gy, x0, x1, &mut f);
        }
    }

    /// Units of cells `x0..=x1` in row `cy`.
    #[inline]
    fn row(&self, cy: u32, x0: u32, x1: u32, f: &mut impl FnMut(usize)) {
        let mut x = x0;
        while x <= x1 {
            let block_end = (x | (BLOCK - 1)).min(x1);
            let (b, l0) = self.block_and_local(x, cy);
            let slot = self.block_slot[b as usize];
            if slot != EMPTY {
                let base = slot as usize * STRIDE;
                let l1 = l0 + (block_end - x) as usize;
                let (a, z) = (self.starts[base + l0] as usize, self.starts[base + l1 + 1] as usize);
                for &j in &self.cell_units[a..z] {
                    f(j as usize);
                }
            }
            x = block_end + 1;
        }
    }

    /// Calls `f(j)` for every unit in cells overlapping the square of
    /// half-size `range` (raw Q16.16) around `(x, y)`.
    #[inline]
    pub fn for_each_in_range(&self, x: i32, y: i32, range: i32, mut f: impl FnMut(usize)) {
        let (cx0, cy0) = self.cell_xy(x.saturating_sub(range), y.saturating_sub(range));
        let (cx1, cy1) = self.cell_xy(x.saturating_add(range), y.saturating_add(range));
        for gy in cy0..=cy1 {
            self.row(gy, cx0, cx1, &mut f);
        }
    }

    /// Calls `f(j)` for every unit in the 3×3 cells around the cell of `(x, y)`.
    #[inline]
    pub fn for_each_near(&self, x: i32, y: i32, mut f: impl FnMut(usize)) {
        let (cx, cy) = self.cell_xy(x, y);
        let x0 = cx.saturating_sub(1);
        let x1 = (cx + 1).min(self.cells_w - 1);
        let y0 = cy.saturating_sub(1);
        let y1 = (cy + 1).min(self.cells_h - 1);
        for gy in y0..=y1 {
            self.row(gy, x0, x1, &mut f);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fx::ONE;
    use proptest::prelude::*;

    #[test]
    fn finds_neighbours() {
        let mut g = Grid::new(ONE * 3 / 2, 64, 64);
        assert_eq!(g.cell_size(), 2 * ONE);
        let xs = [ONE, ONE * 2, ONE * 10, ONE * 63];
        let ys = [ONE, ONE, ONE * 10, ONE * 63];
        g.rebuild(&xs, &ys, &[0; 4], &[0]);
        let mut near = Vec::new();
        g.for_each_near(ONE, ONE, |j| near.push(j));
        near.sort();
        assert_eq!(near, vec![0, 1]);
        near.clear();
        g.for_each_near(ONE * 63, ONE * 63, |j| near.push(j));
        assert_eq!(near, vec![3]);
    }

    proptest! {
        /// The grid returns exactly the units a brute-force scan finds.
        #[test]
        fn matches_brute_force(
            pts in proptest::collection::vec((0i32..512 * ONE, 0i32..512 * ONE), 0..400),
            qx in 0i32..512 * ONE, qy in 0i32..512 * ONE,
        ) {
            let mut g = Grid::new(ONE * 3 / 2, 512, 512);
            let xs: Vec<i32> = pts.iter().map(|p| p.0).collect();
            let ys: Vec<i32> = pts.iter().map(|p| p.1).collect();
            // Rebuild twice to exercise clearing of the previous tick.
            let teams = vec![0u8; xs.len()];
            g.rebuild(&ys, &xs, &teams, &[0]);
            g.rebuild(&xs, &ys, &teams, &[0]);
            let cs = g.cell_size();
            let mut got = Vec::new();
            g.for_each_near(qx, qy, |j| got.push(j));
            got.sort();
            let cell = |v: i32| v / cs;
            let want: Vec<usize> = (0..xs.len())
                .filter(|&j| (cell(xs[j]) - cell(qx)).abs() <= 1 && (cell(ys[j]) - cell(qy)).abs() <= 1)
                .collect();
            prop_assert_eq!(&got, &want);
            // The range API covers the same units, and the sorted copies match.
            let mut by_range = Vec::new();
            g.ranges_near(qx, qy, |r| {
                for k in r {
                    by_range.push(g.cell_units[k] as usize);
                    assert_eq!((g.sx[k], g.sy[k]), (xs[g.cell_units[k] as usize], ys[g.cell_units[k] as usize]));
                }
            });
            by_range.sort();
            prop_assert_eq!(by_range, want);
            for i in 0..xs.len() {
                prop_assert_eq!(g.cell_units[g.rank[i] as usize] as usize, i);
                prop_assert_eq!(g.cell_of_unit(i), ((xs[i] / cs) as u32, (ys[i] / cs) as u32));
            }
        }
    }
}
