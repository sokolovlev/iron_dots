//! Uniform spatial grid rebuilt every tick with a counting sort, O(n + cells).
//!
//! The cell size is a power of two in Q16.16 that is at least the contact
//! radius plus twice the largest per-tick move, so a 3×3 neighbourhood built
//! from positions at the start of the tick still contains every unit within
//! contact radius after the movement phase.

#[derive(Clone, Debug, Default)]
pub struct Grid {
    /// log2 of the cell size in raw Q16.16 units.
    shift: u32,
    cells_w: u32,
    cells_h: u32,
    /// `cell_start[c]..cell_start[c + 1]` is the range of `cell_units` for cell `c`.
    cell_start: Vec<u32>,
    /// Unit indices sorted by cell; within a cell by ascending index.
    cell_units: Vec<u32>,
    /// Cell of each unit.
    unit_cell: Vec<u32>,
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
        Grid {
            shift,
            cells_w,
            cells_h,
            cell_start: vec![0; (cells_w * cells_h) as usize + 1],
            cell_units: Vec::new(),
            unit_cell: Vec::new(),
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

    pub fn rebuild(&mut self, pos_x: &[i32], pos_y: &[i32]) {
        let n = pos_x.len();
        self.unit_cell.resize(n, 0);
        self.cell_units.resize(n, 0);
        self.cell_start.fill(0);
        for i in 0..n {
            let (cx, cy) = self.cell_xy(pos_x[i], pos_y[i]);
            let c = cy * self.cells_w + cx;
            self.unit_cell[i] = c;
            self.cell_start[c as usize + 1] += 1;
        }
        for c in 1..self.cell_start.len() {
            self.cell_start[c] += self.cell_start[c - 1];
        }
        // Scatter in ascending index order; `cell_start[c]` temporarily acts
        // as the write cursor and is restored afterwards by shifting.
        for i in 0..n {
            let c = self.unit_cell[i] as usize;
            let at = self.cell_start[c];
            self.cell_units[at as usize] = i as u32;
            self.cell_start[c] += 1;
        }
        for c in (1..self.cell_start.len()).rev() {
            self.cell_start[c] = self.cell_start[c - 1];
        }
        self.cell_start[0] = 0;
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
            let row = gy * self.cells_w;
            let a = self.cell_start[(row + x0) as usize] as usize;
            let b = self.cell_start[(row + x1) as usize + 1] as usize;
            for &j in &self.cell_units[a..b] {
                f(j as usize);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fx::ONE;

    #[test]
    fn finds_neighbours() {
        let mut g = Grid::new(ONE * 3 / 2, 64, 64);
        assert_eq!(g.cell_size(), 2 * ONE);
        let xs = [ONE, ONE * 2, ONE * 10, ONE * 63];
        let ys = [ONE, ONE, ONE * 10, ONE * 63];
        g.rebuild(&xs, &ys);
        let mut near = Vec::new();
        g.for_each_near(ONE, ONE, |j| near.push(j));
        near.sort();
        assert_eq!(near, vec![0, 1]);
        near.clear();
        g.for_each_near(ONE * 63, ONE * 63, |j| near.push(j));
        assert_eq!(near, vec![3]);
    }
}
