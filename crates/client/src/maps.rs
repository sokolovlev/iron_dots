//! Maps to play on: the built-in pool (compiled into the game) and the
//! player's own `.wmap` files from the editor.

use sim_core::mapfile::MapFile;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, PartialEq, Eq)]
pub struct MapEntry {
    pub name: String,
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Suggested team of each start.
    pub teams: Vec<u8>,
    /// File on disk for the player's own maps.
    pub path: Option<PathBuf>,
}

impl MapEntry {
    pub fn from_bytes(bytes: Vec<u8>, path: Option<PathBuf>) -> Result<MapEntry, String> {
        let f = MapFile::decode(&bytes)?;
        f.validate()?;
        let name = if f.name.trim().is_empty() {
            path.as_ref().and_then(|p| p.file_stem()).map_or("Untitled".into(), |s| s.to_string_lossy().into_owned())
        } else {
            f.name.clone()
        };
        Ok(MapEntry { name, bytes, width: f.width, height: f.height, teams: f.teams, path })
    }

    pub fn players(&self) -> usize {
        self.teams.len()
    }

    /// "1v1", "2v2", "3v3" or "4 FFA" from the suggested teams.
    pub fn mode(&self) -> String {
        let mut per_team = std::collections::BTreeMap::<u8, usize>::new();
        for &t in &self.teams {
            *per_team.entry(t).or_default() += 1;
        }
        let sizes: Vec<usize> = per_team.values().copied().collect();
        if sizes.iter().all(|&s| s == 1) && sizes.len() > 2 {
            format!("{} FFA", sizes.len())
        } else {
            sizes.iter().map(|s| s.to_string()).collect::<Vec<_>>().join("v")
        }
    }

    pub fn label(&self) -> String {
        format!("{}  ·  {}  ·  {}×{}", self.name, self.mode(), self.width, self.height)
    }
}

/// The built-in maps, in menu order.
pub fn builtin() -> Vec<Arc<MapEntry>> {
    const FILES: &[&[u8]] = &[
        include_bytes!("../../../assets/maps/crossing.wmap"),
        include_bytes!("../../../assets/maps/twin_rivers.wmap"),
        include_bytes!("../../../assets/maps/highlands.wmap"),
        include_bytes!("../../../assets/maps/lakeside.wmap"),
        include_bytes!("../../../assets/maps/two_fronts.wmap"),
        include_bytes!("../../../assets/maps/archipelago.wmap"),
        include_bytes!("../../../assets/maps/great_plain.wmap"),
        include_bytes!("../../../assets/maps/three_kingdoms.wmap"),
        include_bytes!("../../../assets/maps/long_war.wmap"),
        include_bytes!("../../../assets/maps/four_corners.wmap"),
    ];
    FILES.iter().filter_map(|b| MapEntry::from_bytes(b.to_vec(), None).ok()).map(Arc::new).collect()
}

/// Folder of the player's own maps (next to `settings.ron`).
pub fn user_dir() -> Option<PathBuf> {
    crate::settings::Settings::path().and_then(|p| p.parent().map(|d| d.join("maps")))
}

/// The player's own maps, sorted by name; broken files are skipped.
pub fn user_maps() -> Vec<Arc<MapEntry>> {
    let Some(dir) = user_dir() else { return Vec::new() };
    let Ok(rd) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut out: Vec<Arc<MapEntry>> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "wmap"))
        .filter_map(|p| {
            let bytes = std::fs::read(&p).ok()?;
            MapEntry::from_bytes(bytes, Some(p)).ok().map(Arc::new)
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_pool_is_complete_and_valid() {
        let maps = builtin();
        assert_eq!(maps.len(), 10, "a built-in map failed to load");
        let modes: Vec<String> = maps.iter().map(|m| m.mode()).collect();
        let count = |m: &str| modes.iter().filter(|x| *x == m).count();
        // Spec 8.1: 4 for 1v1, 3 for 2v2, 2 for 3v3, 1 FFA.
        assert_eq!((count("1v1"), count("2v2"), count("3v3"), count("4 FFA")), (4, 3, 2, 1), "{modes:?}");
    }
}
