//! Tile map: separate planes (terrain, owner) stored chunk by chunk.
//!
//! A chunk is 64×64 tiles, so one plane of one chunk is 4 KiB and fits in L1.
//! Tile `(x, y)` lives at `chunk_index * 4096 + local_y * 64 + local_x`.

use crate::rng::{Purpose, rand};
use serde::{Deserialize, Serialize};

pub const CHUNK_SHIFT: u32 = 6;
pub const CHUNK: u32 = 1 << CHUNK_SHIFT;
pub const CHUNK_TILES: usize = (CHUNK * CHUNK) as usize;
pub const MIN_SIZE: u32 = 64;
pub const MAX_SIZE: u32 = 4096;

/// Terrain type of a tile. The discriminant is the index into rule tables.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum Terrain {
    Plain = 0,
    Forest = 1,
    Hills = 2,
    Mountain = 3,
    Water = 4,
    Sand = 5,
    Snow = 6,
    Mud = 7,
    City = 8,
    Bridge = 9,
}

impl Terrain {
    pub const COUNT: usize = 10;
    pub const ALL: [Terrain; Terrain::COUNT] = [
        Terrain::Plain,
        Terrain::Forest,
        Terrain::Hills,
        Terrain::Mountain,
        Terrain::Water,
        Terrain::Sand,
        Terrain::Snow,
        Terrain::Mud,
        Terrain::City,
        Terrain::Bridge,
    ];

    #[inline]
    pub const fn from_u8(v: u8) -> Option<Terrain> {
        if (v as usize) < Terrain::COUNT { Some(Terrain::ALL[v as usize]) } else { None }
    }

    /// One-character code used by text maps in tests and scenarios.
    pub const fn to_char(self) -> char {
        match self {
            Terrain::Plain => '.',
            Terrain::Forest => 'f',
            Terrain::Hills => 'h',
            Terrain::Mountain => 'M',
            Terrain::Water => '~',
            Terrain::Sand => 's',
            Terrain::Snow => '*',
            Terrain::Mud => 'm',
            Terrain::City => 'C',
            Terrain::Bridge => '=',
        }
    }

    pub const fn from_char(c: char) -> Option<Terrain> {
        Some(match c {
            '.' => Terrain::Plain,
            'f' => Terrain::Forest,
            'h' => Terrain::Hills,
            'M' => Terrain::Mountain,
            '~' => Terrain::Water,
            's' => Terrain::Sand,
            '*' => Terrain::Snow,
            'm' => Terrain::Mud,
            'C' => Terrain::City,
            '=' => Terrain::Bridge,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Map {
    width: u32,
    height: u32,
    chunks_w: u32,
    chunks_h: u32,
    /// Terrain plane, chunk-major, values are [`Terrain`] discriminants.
    terrain: Vec<u8>,
    /// Owner plane (player id, 0 = nobody), chunk-major. Simulated from stage 2.
    owner: Vec<u8>,
}

/// Map description that can regenerate the exact same map (used in replays).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MapSource {
    /// Procedural map from a seed.
    Generated { width: u32, height: u32, seed: u64 },
    /// Explicit tiles (editor maps, tests).
    Tiles(Map),
    /// An encoded `.wmap` file (built-in or editor maps); compact enough
    /// to travel in lobby messages and replays.
    File(Vec<u8>),
}

impl MapSource {
    pub fn build(&self) -> Result<Map, String> {
        match self {
            MapSource::Generated { width, height, seed } => Map::generate(*width, *height, *seed),
            MapSource::Tiles(m) => Ok(m.clone()),
            MapSource::File(bytes) => crate::mapfile::MapFile::decode(bytes)?.to_map(),
        }
    }
}

impl Map {
    /// A map filled with one terrain. Width and height must be multiples of 64
    /// in `[64, 4096]`.
    pub fn new(width: u32, height: u32, fill: Terrain) -> Result<Map, String> {
        for (name, v) in [("width", width), ("height", height)] {
            if !(MIN_SIZE..=MAX_SIZE).contains(&v) || v % CHUNK != 0 {
                return Err(format!("map {name} {v} must be a multiple of {CHUNK} in [{MIN_SIZE}, {MAX_SIZE}]"));
            }
        }
        let n = (width * height) as usize;
        Ok(Map {
            width,
            height,
            chunks_w: width / CHUNK,
            chunks_h: height / CHUNK,
            terrain: vec![fill as u8; n],
            owner: vec![0; n],
        })
    }

    /// Builds a map from text rows; the text is tiled to fill the map and
    /// unknown characters are plain. Handy for tests.
    pub fn from_text(width: u32, height: u32, rows: &[&str]) -> Result<Map, String> {
        let mut m = Map::new(width, height, Terrain::Plain)?;
        for (y, row) in rows.iter().enumerate().take(height as usize) {
            for (x, c) in row.chars().enumerate().take(width as usize) {
                m.set_terrain(x as u32, y as u32, Terrain::from_char(c).unwrap_or(Terrain::Plain));
            }
        }
        Ok(m)
    }

    #[inline]
    pub const fn width(&self) -> u32 {
        self.width
    }
    #[inline]
    pub const fn height(&self) -> u32 {
        self.height
    }
    #[inline]
    pub const fn chunks_w(&self) -> u32 {
        self.chunks_w
    }
    #[inline]
    pub const fn chunks_h(&self) -> u32 {
        self.chunks_h
    }

    /// Plane index of tile `(x, y)`. Caller guarantees bounds.
    #[inline]
    pub const fn index(&self, x: u32, y: u32) -> usize {
        let chunk = (y >> CHUNK_SHIFT) * self.chunks_w + (x >> CHUNK_SHIFT);
        let local = ((y & (CHUNK - 1)) << CHUNK_SHIFT) | (x & (CHUNK - 1));
        ((chunk as usize) << (2 * CHUNK_SHIFT)) | local as usize
    }

    #[inline]
    pub fn terrain_raw(&self, x: u32, y: u32) -> u8 {
        self.terrain[self.index(x, y)]
    }

    #[inline]
    pub fn terrain(&self, x: u32, y: u32) -> Terrain {
        Terrain::ALL[self.terrain_raw(x, y) as usize]
    }

    pub fn set_terrain(&mut self, x: u32, y: u32, t: Terrain) {
        let i = self.index(x, y);
        self.terrain[i] = t as u8;
    }

    #[inline]
    pub fn owner(&self, x: u32, y: u32) -> u8 {
        self.owner[self.index(x, y)]
    }

    pub fn set_owner(&mut self, x: u32, y: u32, owner: u8) {
        let i = self.index(x, y);
        self.owner[i] = owner;
    }

    /// Raw terrain plane (chunk-major) — for upload to the GPU and hashing.
    pub fn terrain_plane(&self) -> &[u8] {
        &self.terrain
    }

    pub fn owner_plane(&self) -> &[u8] {
        &self.owner
    }

    /// Hash of the map content, used to check that all peers have the same map.
    pub fn content_hash(&self) -> u64 {
        let mut h = xxhash_rust::xxh3::Xxh3::new();
        h.update(&self.width.to_le_bytes());
        h.update(&self.height.to_le_bytes());
        h.update(&self.terrain);
        h.digest()
    }

    /// Text dump of a rectangle, one row per line.
    pub fn to_text(&self, x0: u32, y0: u32, w: u32, h: u32) -> String {
        let mut s = String::new();
        for y in y0..(y0 + h).min(self.height) {
            for x in x0..(x0 + w).min(self.width) {
                s.push(self.terrain(x, y).to_char());
            }
            s.push('\n');
        }
        s
    }

    /// Procedural map: two octaves of integer value noise for height and one
    /// for moisture. Deterministic for a given seed and size.
    pub fn generate(width: u32, height: u32, seed: u64) -> Result<Map, String> {
        let mut m = Map::new(width, height, Terrain::Plain)?;
        for y in 0..height {
            for x in 0..width {
                let h = (value_noise(seed, x, y, 64, 0) * 3 + value_noise(seed, x, y, 16, 1)) / 4;
                let wet = value_noise(seed, x, y, 48, 2);
                let t = match h {
                    0..=17_000 => Terrain::Water,
                    17_001..=19_500 => Terrain::Sand,
                    _ if h > 49_500 => Terrain::Mountain,
                    _ if h > 45_000 => {
                        if wet > 40_000 {
                            Terrain::Snow
                        } else {
                            Terrain::Hills
                        }
                    }
                    _ if wet > 46_000 => Terrain::Forest,
                    _ if wet < 14_000 && h < 26_000 => Terrain::Mud,
                    _ => Terrain::Plain,
                };
                m.set_terrain(x, y, t);
            }
        }
        Ok(m)
    }
}

/// Integer value noise in `[0, 65535]` with lattice step `step` (power of two).
fn value_noise(seed: u64, x: u32, y: u32, step: u32, octave: u32) -> u32 {
    let lattice =
        |gx: u32, gy: u32| -> u32 { (rand(seed ^ ((octave as u64) << 48), gx, gy, Purpose::MapGen) & 0xFFFF) as u32 };
    let (gx, gy) = (x / step, y / step);
    let (fx, fy) = (x % step, y % step);
    // Smoothstep weights in [0, 65536].
    let smooth = |f: u32| -> u64 {
        let t = (f as u64 * 65_536) / step as u64;
        (t * t * (3 * 65_536 - 2 * t)) >> 32
    };
    let (sx, sy) = (smooth(fx), smooth(fy));
    let a = lattice(gx, gy) as u64;
    let b = lattice(gx + 1, gy) as u64;
    let c = lattice(gx, gy + 1) as u64;
    let d = lattice(gx + 1, gy + 1) as u64;
    let top = (a * (65_536 - sx) + b * sx) >> 16;
    let bot = (c * (65_536 - sx) + d * sx) >> 16;
    ((top * (65_536 - sy) + bot * sy) >> 16) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_is_a_bijection() {
        let m = Map::new(128, 192, Terrain::Plain).unwrap();
        let mut seen = vec![false; 128 * 192];
        for y in 0..192 {
            for x in 0..128 {
                let i = m.index(x, y);
                assert!(!seen[i]);
                seen[i] = true;
            }
        }
        assert_eq!(m.index(65, 0), 4096 + 1);
    }

    #[test]
    fn rejects_bad_sizes() {
        assert!(Map::new(100, 128, Terrain::Plain).is_err());
        assert!(Map::new(8192, 128, Terrain::Plain).is_err());
    }

    #[test]
    fn generation_is_deterministic_and_varied() {
        let a = Map::generate(256, 256, 7).unwrap();
        let b = Map::generate(256, 256, 7).unwrap();
        let c = Map::generate(256, 256, 8).unwrap();
        assert_eq!(a.content_hash(), b.content_hash());
        assert_ne!(a.content_hash(), c.content_hash());
        let mut counts = [0usize; Terrain::COUNT];
        for &t in a.terrain_plane() {
            counts[t as usize] += 1;
        }
        let plain_share = counts[Terrain::Plain as usize] * 100 / (256 * 256);
        assert!(plain_share > 20, "too little plain: {counts:?}");
    }

    #[test]
    fn text_round_trip() {
        let m = Map::from_text(64, 64, &[".fhM~", "s*mC="]).unwrap();
        assert_eq!(m.to_text(0, 0, 5, 2), ".fhM~\ns*mC=\n");
    }
}
