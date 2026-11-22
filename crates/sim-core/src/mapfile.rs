//! `.wmap` map files (spec 6.4): hand-made or generated maps with terrain,
//! neutral cities and start positions.
//!
//! Layout: `"WMAP"`, a u32 LE format version, then a zstd frame holding the
//! bitcode-encoded [`MapFile`] (terrain as the map's chunk-major plane).
//! The hash of the encoded bytes identifies the map in lobbies and replays.

use crate::map::{Map, Terrain};
use serde::{Deserialize, Serialize};
use std::io::Read;

const MAGIC: &[u8; 4] = b"WMAP";
pub const FORMAT_VERSION: u32 = 1;
/// Largest accepted decompressed body (a 4096² map is 16 MiB of terrain).
const MAX_BODY: u64 = 20 << 20;
pub const MAX_STARTS: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MapFile {
    pub name: String,
    pub author: String,
    pub description: String,
    pub width: u32,
    pub height: u32,
    /// Terrain plane in [`Map`]'s chunk-major order. City tiles are not
    /// stored here: cities are listed below and placed when a match starts.
    pub terrain: Vec<u8>,
    /// Neutral cities.
    pub cities: Vec<(u32, u32)>,
    /// Capital of the player in each slot; its length is the player count
    /// the map is made for.
    pub starts: Vec<(u32, u32)>,
    /// Suggested team per start (same length as `starts`), e.g. `[1, 2]`
    /// for a duel or `[1, 1, 2, 2]` for 2v2.
    pub teams: Vec<u8>,
}

impl MapFile {
    /// An empty map of plain terrain.
    pub fn blank(width: u32, height: u32) -> Result<MapFile, String> {
        let m = Map::new(width, height, Terrain::Plain)?;
        Ok(MapFile::from_map(&m))
    }

    /// Terrain of `m` with no cities or starts yet.
    pub fn from_map(m: &Map) -> MapFile {
        MapFile {
            name: String::new(),
            author: String::new(),
            description: String::new(),
            width: m.width(),
            height: m.height(),
            terrain: m.terrain_plane().to_vec(),
            cities: Vec::new(),
            starts: Vec::new(),
            teams: Vec::new(),
        }
    }

    pub fn players(&self) -> usize {
        self.starts.len()
    }

    /// The terrain as a [`Map`] (no cities placed).
    pub fn to_map(&self) -> Result<Map, String> {
        let mut m = Map::new(self.width, self.height, Terrain::Plain)?;
        if self.terrain.len() != m.terrain_plane().len() {
            return Err("terrain size does not match the map size".into());
        }
        for y in 0..self.height {
            for x in 0..self.width {
                let t = self.terrain[m.index(x, y)];
                let t = Terrain::from_u8(t).filter(|&t| t != Terrain::City).unwrap_or(Terrain::Plain);
                m.set_terrain(x, y, t);
            }
        }
        Ok(m)
    }

    /// Checks everything a match needs; returns the first problem.
    pub fn validate(&self) -> Result<(), String> {
        let m = self.to_map()?;
        if !(2..=MAX_STARTS).contains(&self.starts.len()) {
            return Err(format!("a map needs 2 to {MAX_STARTS} start positions"));
        }
        if self.teams.len() != self.starts.len() || self.teams.iter().any(|&t| !(1..=8).contains(&t)) {
            return Err("every start needs a team 1..=8".into());
        }
        let all: Vec<(u32, u32)> = self.starts.iter().chain(&self.cities).copied().collect();
        for (k, &(x, y)) in all.iter().enumerate() {
            if x >= self.width || y >= self.height {
                return Err(format!("city at ({x}, {y}) is outside the map"));
            }
            if matches!(m.terrain(x, y), Terrain::Mountain | Terrain::Water) {
                return Err(format!("city at ({x}, {y}) stands on {:?}", m.terrain(x, y)));
            }
            if all[..k].iter().any(|&(ox, oy)| ox.abs_diff(x) < 3 && oy.abs_diff(y) < 3) {
                return Err(format!("cities at ({x}, {y}) are too close together"));
            }
        }
        Ok(())
    }

    pub fn encode(&self) -> Vec<u8> {
        let body = bitcode::serialize(self).expect("map files serialize");
        let packed = ruzstd::encoding::compress_to_vec(&body[..], ruzstd::encoding::CompressionLevel::Fastest);
        let mut out = Vec::with_capacity(8 + packed.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&packed);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<MapFile, String> {
        if bytes.len() < 8 || &bytes[..4] != MAGIC {
            return Err("not an Iron Dots map".into());
        }
        let version = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        if version != FORMAT_VERSION {
            return Err(format!("map format {version} is not supported (this game reads {FORMAT_VERSION})"));
        }
        let mut src = &bytes[8..];
        let dec = ruzstd::decoding::StreamingDecoder::new(&mut src).map_err(|e| format!("broken map: {e}"))?;
        let mut body = Vec::new();
        dec.take(MAX_BODY).read_to_end(&mut body).map_err(|e| format!("broken map: {e}"))?;
        let f: MapFile = bitcode::deserialize(&body).map_err(|e| format!("broken map: {e}"))?;
        f.to_map()?;
        Ok(f)
    }

    /// Hash of the encoded file, to check every peer has the same map.
    pub fn hash(bytes: &[u8]) -> u64 {
        xxhash_rust::xxh3::xxh3_64(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_validation() {
        let mut f = MapFile::from_map(&Map::generate(256, 128, 3).unwrap());
        f.name = "Test".into();
        f.starts = vec![(40, 64), (215, 64)];
        f.teams = vec![1, 2];
        f.cities = vec![(128, 30), (128, 98)];
        let bytes = f.encode();
        assert!(bytes.len() < 256 * 128 / 2, "compressed to {} bytes", bytes.len());
        let g = MapFile::decode(&bytes).unwrap();
        assert_eq!(f, g);
        assert_eq!(MapFile::hash(&bytes), MapFile::hash(&g.encode()));
        let mut ok = MapFile::blank(256, 128).unwrap();
        ok.starts = g.starts.clone();
        ok.teams = g.teams.clone();
        ok.cities = g.cities.clone();
        ok.validate().unwrap();
        let mut bad = ok.clone();
        bad.cities.push((41, 65));
        assert!(bad.validate().unwrap_err().contains("too close"));
        assert!(MapFile::decode(b"WMAP\x09\0\0\0").unwrap_err().contains("not supported"));
        assert!(MapFile::decode(b"hello").is_err());
    }
}

/// How a generated map is made fair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Symmetry {
    /// Left half mirrored onto the right: two sides facing each other.
    Mirror,
    /// Rotated by 180°: each start sees the same map from its corner.
    Rotate,
    /// Mirrored both ways: four equal quarters (free-for-all).
    Quad,
}

impl Symmetry {
    /// `p` and its images in the other parts of a `w`×`h` map (`p` first).
    pub fn images(self, w: u32, h: u32, (x, y): (u32, u32)) -> Vec<(u32, u32)> {
        let mut v = match self {
            Symmetry::Mirror => vec![(x, y), (w - 1 - x, y)],
            Symmetry::Rotate => vec![(x, y), (w - 1 - x, h - 1 - y)],
            Symmetry::Quad => vec![(x, y), (w - 1 - x, y), (x, h - 1 - y), (w - 1 - x, h - 1 - y)],
        };
        // A point on an axis is its own image.
        v.dedup();
        v.sort_unstable();
        v.dedup();
        if let Some(k) = v.iter().position(|&q| q == (x, y)) {
            v.swap(0, k);
        }
        v
    }
}

/// A fair generated map: noise terrain made symmetric, starts and neutral
/// cities placed in one part and copied to the others. `layout` gives the
/// starts of the left (or top-left) part as fractions of the map in
/// thousandths; `teams` is the suggested team of each resulting start.
pub fn symmetric(
    width: u32,
    height: u32,
    seed: u64,
    sym: Symmetry,
    layout: &[(u32, u32)],
    cities_per_part: usize,
) -> Result<MapFile, String> {
    let base = Map::generate(width, height, seed)?;
    let (w, h) = (width, height);
    let mirror = |x: u32, y: u32| -> (u32, u32) {
        match sym {
            Symmetry::Mirror => (x.min(w - 1 - x), y),
            Symmetry::Rotate if x >= w / 2 => (w - 1 - x, h - 1 - y),
            Symmetry::Rotate => (x, y),
            Symmetry::Quad => (x.min(w - 1 - x), y.min(h - 1 - y)),
        }
    };
    let mut m = Map::new(w, h, Terrain::Plain)?;
    for y in 0..h {
        for x in 0..w {
            let (sx, sy) = mirror(x, y);
            m.set_terrain(x, y, base.terrain(sx, sy));
        }
    }
    // Images of a point in the other parts.
    let copies = |p: (u32, u32)| -> Vec<(u32, u32)> {
        match sym {
            Symmetry::Mirror => vec![p, (w - 1 - p.0, p.1)],
            Symmetry::Rotate => vec![p, (w - 1 - p.0, h - 1 - p.1)],
            Symmetry::Quad => vec![p, (w - 1 - p.0, p.1), (p.0, h - 1 - p.1), (w - 1 - p.0, h - 1 - p.1)],
        }
    };
    let part_w = w / 2 - 4;
    let part_h = if sym == Symmetry::Quad { h / 2 - 4 } else { h - 4 };
    let land = |x: u32, y: u32| !matches!(m.terrain(x, y), Terrain::Mountain | Terrain::Water);
    // Nearest land tile inside the part, scanning rings.
    let settle = |x: u32, y: u32| -> Option<(u32, u32)> {
        for r in 0..w.max(h) as i64 {
            for dy in -r..=r {
                for dx in -r..=r {
                    if dx.abs() != r && dy.abs() != r {
                        continue;
                    }
                    let (nx, ny) = (x as i64 + dx, y as i64 + dy);
                    if nx >= 4 && ny >= 4 && nx < part_w as i64 && ny < part_h as i64 && land(nx as u32, ny as u32) {
                        return Some((nx as u32, ny as u32));
                    }
                }
            }
        }
        None
    };
    let mut part_starts = Vec::new();
    for &(fx, fy) in layout {
        let p = settle(w * fx / 1000, h * fy / 1000).ok_or("no land for a start")?;
        part_starts.push(p);
    }
    // Neutral cities in the part by rejection sampling, spaced apart and
    // away from the starts.
    let spacing = (w.min(h) / 12).clamp(10, 32) as i64;
    let mut part_cities: Vec<(u32, u32)> = Vec::new();
    let mut attempt = 0u32;
    while part_cities.len() < cities_per_part && attempt < 50_000 {
        let r = crate::rng::rand(seed, attempt, 1, crate::rng::Purpose::Scenario);
        attempt += 1;
        let x = 6 + (r & 0xFFFF) as u32 % (part_w - 6);
        let y = 6 + ((r >> 16) & 0xFFFF) as u32 % (part_h - 6);
        if !land(x, y) {
            continue;
        }
        let far = |&(ox, oy): &(u32, u32), min: i64| {
            let (dx, dy) = (ox as i64 - x as i64, oy as i64 - y as i64);
            dx * dx + dy * dy >= min * min
        };
        if part_cities.iter().all(|c| far(c, spacing)) && part_starts.iter().all(|c| far(c, spacing)) {
            part_cities.push((x, y));
        }
    }
    let mut f = MapFile::from_map(&m);
    // Starts: all images of the first part start, then of the second, ...
    // so a mirror layout gives slots L1, R1, L2, R2 with teams 1, 2, 1, 2.
    for &s in &part_starts {
        for (k, c) in copies(s).into_iter().enumerate() {
            f.starts.push(c);
            f.teams.push(if sym == Symmetry::Quad { f.starts.len() as u8 } else { k as u8 + 1 });
        }
    }
    for &c in &part_cities {
        f.cities.extend(copies(c));
    }
    // Terrain only (the editor) is not a playable map yet.
    if !layout.is_empty() {
        f.validate()?;
    }
    Ok(f)
}
