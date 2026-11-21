//! Map editor (spec 6.4): terrain brushes, neutral cities and start
//! positions, symmetry for fair maps, undo/redo, generation by seed, PNG
//! import, save/open `.wmap`, and "play this map" straight from the editor.

use crate::maps::MapEntry;
use egui::{Align2, Color32, RichText};
use render::{Camera, Marker, Overlay};
use sim_core::map::{Map, Terrain};
use sim_core::mapfile::{MAX_STARTS, MapFile, Symmetry};
use sim_core::scenario::MAP_PRESETS;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

/// Terrains offered as brushes (cities are objects, not paint).
const BRUSHES: [(Terrain, &str); 9] = [
    (Terrain::Plain, "Plain"),
    (Terrain::Forest, "Forest"),
    (Terrain::Hills, "Hills"),
    (Terrain::Mountain, "Mountain"),
    (Terrain::Water, "Water"),
    (Terrain::Sand, "Sand"),
    (Terrain::Snow, "Snow"),
    (Terrain::Mud, "Mud"),
    (Terrain::Bridge, "Bridge"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Paint(Terrain),
    City,
    /// A start position for a player of this team.
    Start(u8),
    /// Removes cities and starts under the cursor.
    Erase,
}

/// Cities, starts and teams: small, so undo stores them whole.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
struct Objects {
    cities: Vec<(u32, u32)>,
    starts: Vec<(u32, u32)>,
    teams: Vec<u8>,
}

#[derive(Default)]
struct Edit {
    /// `(x, y, old, new)` terrain changes.
    tiles: Vec<(u32, u32, u8, u8)>,
    /// Objects before and after, if they changed.
    objects: Option<(Objects, Objects)>,
}

impl Edit {
    fn is_empty(&self) -> bool {
        self.tiles.is_empty() && self.objects.is_none()
    }
}

/// What the editor asks the app to do.
pub enum EditorAction {
    None,
    Exit,
    Play(Arc<MapEntry>),
}

pub struct Editor {
    pub map: Map,
    objects: Objects,
    pub name: String,
    pub author: String,
    pub description: String,
    pub camera: Camera,
    pub tool: Tool,
    pub brush: u32,
    pub symmetry: Option<Symmetry>,
    undo: Vec<Edit>,
    redo: Vec<Edit>,
    stroke: Option<Edit>,
    dirty: BTreeSet<(u32, u32)>,
    /// The renderer must be rebuilt (new map size).
    pub resized: bool,
    pub cursor: Option<[f32; 2]>,
    status: Option<(String, bool)>,
    // "New / generate" panel.
    new_size: [u32; 2],
    gen_seed: u64,
    import_path: String,
    /// Right edge of the side panel in physical pixels: presses left of
    /// it belong to the panel, never to the map.
    panel_right: f32,
    /// Changed since the last save.
    pub modified: bool,
    pub path: Option<PathBuf>,
}

impl Editor {
    pub fn new() -> Editor {
        let map = Map::new(320, 192, Terrain::Plain).expect("preset size");
        let mut e = Editor {
            camera: Camera { center: [160.0, 96.0], scale: 4.0 },
            map,
            objects: Objects::default(),
            name: "My map".into(),
            author: String::new(),
            description: String::new(),
            tool: Tool::Paint(Terrain::Forest),
            brush: 3,
            symmetry: Some(Symmetry::Mirror),
            undo: Vec::new(),
            redo: Vec::new(),
            stroke: None,
            dirty: BTreeSet::new(),
            resized: true,
            cursor: None,
            status: None,
            new_size: [320, 192],
            gen_seed: 1,
            import_path: String::new(),
            panel_right: 0.0,
            modified: false,
            path: None,
        };
        e.mark_all();
        e
    }

    fn mark_all(&mut self) {
        for cy in 0..self.map.chunks_h() {
            for cx in 0..self.map.chunks_w() {
                self.dirty.insert((cx, cy));
            }
        }
    }

    pub fn map_size(&self) -> [f32; 2] {
        [self.map.width() as f32, self.map.height() as f32]
    }

    /// Chunks whose terrain changed since the last call.
    pub fn take_dirty(&mut self) -> Vec<(u32, u32)> {
        std::mem::take(&mut self.dirty).into_iter().collect()
    }

    fn replace_map(&mut self, map: Map, objects: Objects) {
        self.camera = Camera { center: [map.width() as f32 / 2.0, map.height() as f32 / 2.0], scale: 1.0 };
        self.map = map;
        self.objects = objects;
        self.undo.clear();
        self.redo.clear();
        self.stroke = None;
        self.resized = true;
        self.modified = false;
        self.mark_all();
    }

    pub fn to_file(&self) -> MapFile {
        let mut f = MapFile::from_map(&self.map);
        f.name = self.name.trim().to_string();
        f.author = self.author.trim().to_string();
        f.description = self.description.clone();
        f.cities = self.objects.cities.clone();
        f.starts = self.objects.starts.clone();
        f.teams = self.objects.teams.clone();
        f
    }

    pub fn open(&mut self, entry: &MapEntry) -> Result<(), String> {
        let f = MapFile::decode(&entry.bytes)?;
        let map = f.to_map()?;
        self.name = f.name.clone();
        self.author = f.author.clone();
        self.description = f.description.clone();
        // Built-in maps open as a copy to be saved among your own maps.
        self.path = entry.path.clone();
        self.replace_map(map, Objects { cities: f.cities, starts: f.starts, teams: f.teams });
        self.status = Some((format!("Opened {}", entry.name), false));
        Ok(())
    }

    // ------------------------------------------------------------ editing

    fn images(&self, p: (u32, u32)) -> Vec<(u32, u32)> {
        match self.symmetry {
            Some(s) => s.images(self.map.width(), self.map.height(), p),
            None => vec![p],
        }
    }

    fn tile_at(&self, w: [f32; 2]) -> Option<(u32, u32)> {
        let (x, y) = (w[0].floor(), w[1].floor());
        (x >= 0.0 && y >= 0.0 && x < self.map.width() as f32 && y < self.map.height() as f32)
            .then_some((x as u32, y as u32))
    }

    fn set_tile(&mut self, x: u32, y: u32, t: Terrain) {
        let old = self.map.terrain_raw(x, y);
        if old == t as u8 {
            return;
        }
        self.map.set_terrain(x, y, t);
        self.dirty.insert((x / 64, y / 64));
        self.stroke.get_or_insert_with(Edit::default).tiles.push((x, y, old, t as u8));
    }

    fn paint(&mut self, center: (u32, u32), t: Terrain) {
        let r = self.brush as i64 - 1;
        for c in self.images(center) {
            for dy in -r..=r {
                for dx in -r..=r {
                    if dx * dx + dy * dy > r * r + r {
                        continue;
                    }
                    let (x, y) = (c.0 as i64 + dx, c.1 as i64 + dy);
                    if x >= 0 && y >= 0 && x < self.map.width() as i64 && y < self.map.height() as i64 {
                        self.set_tile(x as u32, y as u32, t);
                    }
                }
            }
        }
    }

    /// Applies an object change as its own undo step.
    fn change_objects(&mut self, f: impl FnOnce(&mut Objects)) {
        let before = self.objects.clone();
        f(&mut self.objects);
        if self.objects != before {
            let after = self.objects.clone();
            self.push_edit(Edit { tiles: Vec::new(), objects: Some((before, after)) });
        }
    }

    fn push_edit(&mut self, e: Edit) {
        if !e.is_empty() {
            self.undo.push(e);
            self.redo.clear();
            self.modified = true;
        }
    }

    fn near(a: (u32, u32), b: (u32, u32), d: u32) -> bool {
        a.0.abs_diff(b.0) <= d && a.1.abs_diff(b.1) <= d
    }

    fn place_city(&mut self, p: (u32, u32)) {
        let pts = self.images(p);
        self.change_objects(|o| {
            for q in pts {
                if !o.cities.iter().chain(&o.starts).any(|&c| Self::near(c, q, 2)) {
                    o.cities.push(q);
                }
            }
        });
    }

    fn place_start(&mut self, p: (u32, u32), team: u8) {
        let pts = self.images(p);
        let sym = self.symmetry;
        let mut full = false;
        self.change_objects(|o| {
            for (k, q) in pts.into_iter().enumerate() {
                if o.cities.iter().chain(&o.starts).any(|&c| Self::near(c, q, 2)) {
                    continue;
                }
                if o.starts.len() >= MAX_STARTS {
                    full = true;
                    break;
                }
                // Mirrored starts face each other: the image of a team-1
                // start is team 2 and vice versa; quarters count on.
                let t = match (sym, k) {
                    (_, 0) => team,
                    (Some(Symmetry::Quad), k) => ((team as usize - 1 + k) % 8 + 1) as u8,
                    _ if team % 2 == 1 => team + 1,
                    _ => team - 1,
                };
                o.starts.push(q);
                o.teams.push(t);
            }
        });
        if full {
            self.status = Some((format!("At most {MAX_STARTS} start positions"), true));
        }
    }

    fn erase(&mut self, p: (u32, u32)) {
        let pts = self.images(p);
        let d = (self.brush + 1).max(2);
        self.change_objects(|o| {
            o.cities.retain(|&c| !pts.iter().any(|&q| Self::near(c, q, d)));
            let keep: Vec<bool> = o.starts.iter().map(|&c| !pts.iter().any(|&q| Self::near(c, q, d))).collect();
            let mut it = keep.iter();
            o.starts.retain(|_| *it.next().expect("same length"));
            let mut it = keep.iter();
            o.teams.retain(|_| *it.next().expect("same length"));
        });
    }

    fn apply_tool(&mut self, w: [f32; 2], start: bool) {
        let Some(p) = self.tile_at(w) else { return };
        match self.tool {
            Tool::Paint(t) => self.paint(p, t),
            Tool::City if start => self.place_city(p),
            Tool::Start(team) if start => self.place_start(p, team),
            Tool::Erase => self.erase(p),
            _ => {}
        }
    }

    /// Left button pressed at screen point `px`.
    pub fn press(&mut self, px: [f32; 2], vp: [f32; 2]) {
        if px[0] <= self.panel_right {
            return;
        }
        self.stroke = Some(Edit::default());
        let w = self.camera.screen_to_world(px, vp);
        self.apply_tool(w, true);
    }

    /// Cursor moved; paints while the button is held.
    pub fn drag(&mut self, px: [f32; 2], vp: [f32; 2], held: bool) {
        let w = self.camera.screen_to_world(px, vp);
        self.cursor = Some(w);
        if held && self.stroke.is_some() {
            self.apply_tool(w, false);
        }
    }

    pub fn release(&mut self) {
        if let Some(s) = self.stroke.take() {
            self.push_edit(s);
        }
    }

    /// Right click: erase whatever object is there.
    pub fn right_click(&mut self, px: [f32; 2], vp: [f32; 2]) {
        if px[0] <= self.panel_right {
            return;
        }
        if let Some(p) = self.tile_at(self.camera.screen_to_world(px, vp)) {
            self.erase(p);
        }
    }

    fn apply_edit(&mut self, e: &Edit, forward: bool) {
        for &(x, y, old, new) in &e.tiles {
            let v = if forward { new } else { old };
            self.map.set_terrain(x, y, Terrain::from_u8(v).unwrap_or(Terrain::Plain));
            self.dirty.insert((x / 64, y / 64));
        }
        if let Some((before, after)) = &e.objects {
            self.objects = if forward { after.clone() } else { before.clone() };
        }
        self.modified = true;
    }

    pub fn undo(&mut self) {
        if let Some(e) = self.undo.pop() {
            self.apply_edit(&e, false);
            self.redo.push(e);
        }
    }

    pub fn redo(&mut self) {
        if let Some(e) = self.redo.pop() {
            self.apply_edit(&e, true);
            self.undo.push(e);
        }
    }

    // ------------------------------------------------------- whole maps

    fn generate(&mut self) {
        let [w, h] = self.new_size;
        let map = match self.symmetry {
            Some(s) => sim_core::mapfile::symmetric(w, h, self.gen_seed, s, &[], 0).map(|f| f.to_map()),
            None => Ok(Map::generate(w, h, self.gen_seed)),
        };
        match map.and_then(|m| m) {
            Ok(m) => {
                self.replace_map(m, Objects::default());
                self.status = Some(("Generated. Now place starts and cities.".into(), false));
            }
            Err(e) => self.status = Some((e, true)),
        }
    }

    /// Nearest-colour import; the image is scaled to the chosen size.
    fn import_png(&mut self) {
        match import_png(self.import_path.trim(), self.new_size) {
            Ok(m) => {
                self.replace_map(m, Objects::default());
                self.status = Some(("Imported".into(), false));
            }
            Err(e) => self.status = Some((e, true)),
        }
    }

    /// Imports a PNG dropped onto the window.
    pub fn drop_file(&mut self, path: &std::path::Path) -> Option<Arc<MapEntry>> {
        let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase());
        match ext.as_deref() {
            Some("png") => {
                self.import_path = path.display().to_string();
                self.import_png();
                None
            }
            Some("wmap") => match std::fs::read(path)
                .map_err(|e| e.to_string())
                .and_then(|b| MapEntry::from_bytes(b, Some(path.to_path_buf())))
            {
                Ok(e) => {
                    let e = Arc::new(e);
                    if let Err(err) = self.open(&e) {
                        self.status = Some((err, true));
                    }
                    Some(e)
                }
                Err(e) => {
                    self.status = Some((e, true));
                    None
                }
            },
            _ => None,
        }
    }

    fn save(&mut self) -> Result<PathBuf, String> {
        let f = self.to_file();
        f.validate()?;
        if f.name.is_empty() {
            return Err("give the map a name".into());
        }
        let dir = crate::maps::user_dir().ok_or("no folder for maps")?;
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let file_name: String = f
            .name
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c.to_ascii_lowercase() } else { '_' })
            .collect();
        let path = dir.join(format!("{file_name}.wmap"));
        std::fs::write(&path, f.encode()).map_err(|e| format!("{}: {e}", path.display()))?;
        self.path = Some(path.clone());
        self.modified = false;
        Ok(path)
    }

    // ------------------------------------------------------------ drawing

    pub fn markers(&self, out: &mut Vec<Marker>) {
        out.clear();
        let c = |p: (u32, u32)| [p.0 as f32 + 0.5, p.1 as f32 + 0.5];
        for &p in &self.objects.cities {
            out.push(Marker::city(c(p), 0, false, 0.0));
        }
        for (k, &p) in self.objects.starts.iter().enumerate() {
            out.push(Marker::city(c(p), k as u8 + 1, true, 0.0));
        }
    }

    pub fn overlay(&self, o: &mut Overlay) {
        o.clear();
        let (w, h) = (self.map.width() as f32, self.map.height() as f32);
        let px = 1.0 / self.camera.scale;
        let guide = [1.0, 1.0, 1.0, 0.25];
        match self.symmetry {
            Some(Symmetry::Mirror) => o.line([w / 2.0, 0.0], [w / 2.0, h], 2.0 * px, guide),
            Some(Symmetry::Rotate) => {
                o.line([w / 2.0, 0.0], [w / 2.0, h], 2.0 * px, guide);
                o.fill_rect([w / 2.0 - 3.0 * px, h / 2.0 - 3.0 * px], [w / 2.0 + 3.0 * px, h / 2.0 + 3.0 * px], guide);
            }
            Some(Symmetry::Quad) => {
                o.line([w / 2.0, 0.0], [w / 2.0, h], 2.0 * px, guide);
                o.line([0.0, h / 2.0], [w, h / 2.0], 2.0 * px, guide);
            }
            None => {}
        }
        if let Some(p) = self.cursor.and_then(|c| self.tile_at(c)) {
            let r = match self.tool {
                Tool::Paint(_) => self.brush as f32 - 0.5,
                Tool::Erase => self.brush as f32 + 1.5,
                _ => 1.5,
            };
            for q in self.images(p) {
                let c = [q.0 as f32 + 0.5, q.1 as f32 + 0.5];
                o.rect([c[0] - r, c[1] - r], [c[0] + r, c[1] + r], 1.5 * px, [1.0, 1.0, 1.0, 0.8]);
            }
        }
    }

    /// Slot numbers next to the start positions (screen-space text).
    fn start_labels(&self, ctx: &egui::Context, vp: [f32; 2], ppp: f32) {
        let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Background, egui::Id::unique("starts")));
        for (k, &(x, y)) in self.objects.starts.iter().enumerate() {
            let s = self.camera.world_to_screen([x as f32 + 0.5, y as f32 - 1.5], vp);
            let pos = egui::pos2(s[0] / ppp, s[1] / ppp);
            let team = self.objects.teams.get(k).copied().unwrap_or(0);
            let galley = painter.layout_no_wrap(
                format!("{} · team {team}", k + 1),
                egui::FontId::proportional(13.0),
                Color32::WHITE,
            );
            let rect = Align2::CENTER_BOTTOM.anchor_size(pos, galley.size());
            painter.rect_filled(rect.expand(3.0), 3.0, Color32::from_black_alpha(170));
            painter.galley(rect.min, galley, Color32::WHITE);
        }
    }

    // ----------------------------------------------------------------- UI

    pub fn ui(&mut self, root: &mut egui::Ui, vp: [f32; 2], maps: &[Arc<MapEntry>]) -> EditorAction {
        let mut action = EditorAction::None;
        let ctx = root.ctx().clone();
        self.start_labels(&ctx, vp, ctx.pixels_per_point());
        let panel = egui::Panel::left("editor").resizable(false).exact_size(270.0).show(root, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.label(RichText::new("MAP EDITOR").size(20.0).strong());
                ui.add_space(6.0);
                egui::Grid::new("meta").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
                    ui.label("Name");
                    ui.text_edit_singleline(&mut self.name);
                    ui.end_row();
                    ui.label("Author");
                    ui.text_edit_singleline(&mut self.author);
                    ui.end_row();
                });
                ui.weak(format!("{}×{} tiles", self.map.width(), self.map.height()));

                ui.separator();
                ui.label(RichText::new("Terrain").strong());
                ui.horizontal_wrapped(|ui| {
                    for (t, name) in BRUSHES {
                        let c = render::palette::TERRAIN[t as usize];
                        let fill = Color32::from_rgb((c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8);
                        let on = self.tool == Tool::Paint(t);
                        let b = egui::Button::new(RichText::new(name).color(Color32::BLACK))
                            .fill(fill)
                            .stroke(egui::Stroke::new(if on { 3.0 } else { 0.0 }, Color32::WHITE));
                        if ui.add(b).clicked() {
                            self.tool = Tool::Paint(t);
                        }
                    }
                });
                ui.add(egui::Slider::new(&mut self.brush, 1..=24).text("brush"));

                ui.separator();
                ui.label(RichText::new("Objects").strong());
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.tool, Tool::City, "City");
                    let team = if let Tool::Start(t) = self.tool { t } else { 1 };
                    ui.selectable_value(&mut self.tool, Tool::Start(team), "Start");
                    ui.selectable_value(&mut self.tool, Tool::Erase, "Erase");
                });
                if let Tool::Start(t) = &mut self.tool {
                    ui.add(egui::Slider::new(t, 1..=8).text("team of new start"));
                }
                ui.weak("Right click removes cities and starts.");
                ui.label(format!("{} starts, {} neutral cities", self.objects.starts.len(), self.objects.cities.len()));

                ui.separator();
                ui.label(RichText::new("Symmetry").strong());
                ui.horizontal_wrapped(|ui| {
                    ui.selectable_value(&mut self.symmetry, None, "None");
                    ui.selectable_value(&mut self.symmetry, Some(Symmetry::Mirror), "Mirror");
                    ui.selectable_value(&mut self.symmetry, Some(Symmetry::Rotate), "Rotate");
                    ui.selectable_value(&mut self.symmetry, Some(Symmetry::Quad), "Quad");
                });
                ui.horizontal(|ui| {
                    if ui.add_enabled(!self.undo.is_empty(), egui::Button::new("Undo")).clicked() {
                        self.undo();
                    }
                    if ui.add_enabled(!self.redo.is_empty(), egui::Button::new("Redo")).clicked() {
                        self.redo();
                    }
                    ui.weak("Ctrl+Z / Ctrl+Y");
                });

                ui.separator();
                ui.label(RichText::new("New map").strong());
                crate::ui::map_picker(ui, "esize", &mut self.new_size, MAP_PRESETS.len());
                ui.horizontal(|ui| {
                    if ui.button("Blank").clicked() {
                        let [w, h] = self.new_size;
                        if let Ok(m) = Map::new(w, h, Terrain::Plain) {
                            self.replace_map(m, Objects::default());
                        }
                    }
                    if ui.button("Generate").clicked() {
                        self.generate();
                    }
                    ui.add(egui::DragValue::new(&mut self.gen_seed).prefix("seed "));
                });
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.import_path).hint_text("image.png").desired_width(150.0),
                    );
                    if ui.button("Import PNG").clicked() {
                        self.import_png();
                    }
                });
                ui.weak("Or drop a .png or .wmap onto the window.");

                ui.separator();
                ui.label(RichText::new("Open").strong());
                egui::ComboBox::from_id_salt("eopen").selected_text("choose a map…").width(240.0).show_ui(ui, |ui| {
                    for m in maps {
                        if ui.selectable_label(false, m.label()).clicked() {
                            if let Err(e) = self.open(m) {
                                self.status = Some((e, true));
                            }
                        }
                    }
                });

                ui.separator();
                let problem = self.to_file().validate().err();
                match &problem {
                    Some(p) => ui.colored_label(Color32::LIGHT_YELLOW, format!("Not playable yet: {p}")),
                    None => ui.colored_label(Color32::LIGHT_GREEN, "Ready to play"),
                };
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("Save").size(16.0)).clicked() {
                        self.status = Some(match self.save() {
                            Ok(p) => (format!("Saved to {}", p.display()), false),
                            Err(e) => (e, true),
                        });
                    }
                    if ui.add_enabled(problem.is_none(), egui::Button::new(RichText::new("Play").size(16.0))).clicked()
                    {
                        match MapEntry::from_bytes(self.to_file().encode(), None) {
                            Ok(e) => action = EditorAction::Play(Arc::new(e)),
                            Err(e) => self.status = Some((e, true)),
                        }
                    }
                    if ui.button(RichText::new("Exit").size(16.0)).clicked() {
                        action = EditorAction::Exit;
                    }
                });
                if self.modified {
                    ui.weak("Unsaved changes");
                }
                if let Some((msg, err)) = &self.status {
                    ui.colored_label(if *err { Color32::LIGHT_RED } else { Color32::LIGHT_GREEN }, msg);
                }
            });
        });
        self.panel_right = panel.response.rect.right() * ctx.pixels_per_point();
        action
    }
}

impl Default for Editor {
    fn default() -> Self {
        Editor::new()
    }
}

/// PNG → terrain by nearest palette colour, scaled to `size` (nearest
/// neighbour). Cities are not imported (place them in the editor).
pub fn import_png(path: &str, size: [u32; 2]) -> Result<Map, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
    let mut dec = png::Decoder::new(std::io::BufReader::new(file));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().map_err(|e| format!("{path}: {e}"))?;
    let mut buf = vec![0; reader.output_buffer_size().ok_or("image too large")?];
    let info = reader.next_frame(&mut buf).map_err(|e| format!("{path}: {e}"))?;
    let channels = info.color_type.samples();
    if channels < 3 && info.color_type != png::ColorType::Grayscale && info.color_type != png::ColorType::GrayscaleAlpha
    {
        return Err("unsupported PNG colour type".into());
    }
    let (iw, ih) = (info.width, info.height);
    let [w, h] = size;
    let mut m = Map::new(w, h, Terrain::Plain)?;
    let palette: Vec<(Terrain, [f32; 3])> =
        BRUSHES.iter().map(|&(t, _)| (t, render::palette::TERRAIN[t as usize])).collect();
    for y in 0..h {
        for x in 0..w {
            let (sx, sy) = ((x as u64 * iw as u64 / w as u64) as u32, (y as u64 * ih as u64 / h as u64) as u32);
            let i = (sy as usize * iw as usize + sx as usize) * channels;
            let px = if channels >= 3 {
                [buf[i] as f32 / 255.0, buf[i + 1] as f32 / 255.0, buf[i + 2] as f32 / 255.0]
            } else {
                let g = buf[i] as f32 / 255.0;
                [g, g, g]
            };
            let best = palette
                .iter()
                .min_by(|a, b| dist(a.1, px).total_cmp(&dist(b.1, px)))
                .map(|p| p.0)
                .unwrap_or(Terrain::Plain);
            m.set_terrain(x, y, best);
        }
    }
    Ok(m)
}

fn dist(a: [f32; 3], b: [f32; 3]) -> f32 {
    (a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symmetric_painting_and_undo() {
        let mut e = Editor::new();
        e.symmetry = Some(Symmetry::Mirror);
        e.brush = 1;
        e.tool = Tool::Paint(Terrain::Water);
        let vp = [1280.0, 720.0];
        e.camera = Camera { center: [160.0, 96.0], scale: 4.0 };
        let px = e.camera.world_to_screen([10.5, 20.5], vp);
        e.press(px, vp);
        e.release();
        assert_eq!(e.map.terrain(10, 20), Terrain::Water);
        assert_eq!(e.map.terrain(309, 20), Terrain::Water, "mirrored");
        e.tool = Tool::Start(1);
        e.press(e.camera.world_to_screen([40.5, 96.5], vp), vp);
        e.release();
        assert_eq!(e.objects.starts, vec![(40, 96), (279, 96)]);
        assert_eq!(e.objects.teams, vec![1, 2]);
        e.undo();
        assert!(e.objects.starts.is_empty());
        e.undo();
        assert_eq!(e.map.terrain(10, 20), Terrain::Plain);
        e.redo();
        e.redo();
        assert_eq!(e.objects.starts.len(), 2);
        e.tool = Tool::City;
        e.press(e.camera.world_to_screen([160.5, 50.5], vp), vp);
        e.release();
        let f = e.to_file();
        f.validate().unwrap();
        let back = MapFile::decode(&f.encode()).unwrap();
        assert_eq!(back.starts, f.starts);
        e.right_click(e.camera.world_to_screen([279.5, 96.5], vp), vp);
        assert!(e.objects.starts.is_empty(), "erasing is symmetric too");
    }
}
