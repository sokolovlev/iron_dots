//! egui screens: main menu, HUD, city panel, pause menu, end of match.

use crate::game::{Game, GameMode, MatchSettings, SPEEDS};
use ai::Difficulty;
use egui::{Align2, Color32, RichText};
use sim_core::UnitKind;
use sim_core::command::Formation;
use sim_core::scenario::MAP_PRESETS;

/// What the UI asks the app to do.
pub enum UiAction {
    None,
    StartMatch(MatchSettings),
    ToMenu,
    Quit,
    OpenSettings,
    CloseSettings,
    OpenEditor,
    CloseEditor,
}

pub struct Menu {
    pub settings: MatchSettings,
    pub error: Option<String>,
    pub online: crate::online::OnlineMenu,
    /// Built-in maps, then the player's own.
    pub maps: Vec<std::sync::Arc<crate::maps::MapEntry>>,
    /// The "Play vs AI" page is open.
    pub single_open: bool,
    /// Last unit budget chosen for a limited war.
    pub limited_units: u32,
}

impl Default for Menu {
    fn default() -> Self {
        let mut m = Menu {
            settings: MatchSettings::default(),
            error: None,
            online: Default::default(),
            maps: Vec::new(),
            single_open: false,
            limited_units: 150,
        };
        m.reload_maps();
        m
    }
}

impl Menu {
    pub fn reload_maps(&mut self) {
        self.maps = crate::maps::builtin();
        self.maps.extend(crate::maps::user_maps());
        self.online.maps = self.maps.clone();
    }
}

/// Map choice: a random generated map, or one from `maps`.
pub fn map_choice(
    ui: &mut egui::Ui,
    id: &str,
    maps: &[std::sync::Arc<crate::maps::MapEntry>],
    chosen: &mut Option<std::sync::Arc<crate::maps::MapEntry>>,
) {
    let text = chosen.as_ref().map_or("Random (generated)".to_string(), |m| m.label());
    egui::ComboBox::from_id_salt(id).width(300.0).selected_text(text).show_ui(ui, |ui| {
        if ui.selectable_label(chosen.is_none(), "Random (generated)").clicked() {
            *chosen = None;
        }
        let mut own = false;
        for m in maps {
            if m.path.is_some() && !own {
                own = true;
                ui.separator();
                ui.weak("Your maps");
            }
            let on = chosen.as_ref().is_some_and(|c| c.bytes == m.bytes);
            if ui.selectable_label(on, m.label()).clicked() {
                *chosen = Some(m.clone());
            }
        }
    });
}

/// Dark, high-contrast style used by every screen.
pub fn setup_style(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.global_style_mut(|s| {
        s.visuals.window_fill = Color32::from_rgba_unmultiplied(22, 24, 28, 235);
        s.visuals.panel_fill = Color32::from_rgb(26, 28, 32);
        s.visuals.override_text_color = Some(Color32::from_gray(230));
    });
}

fn hud_frame(ui: &egui::Ui) -> egui::Frame {
    egui::Frame::popup(ui.style()).fill(Color32::from_rgba_unmultiplied(22, 24, 28, 225))
}

pub fn main_menu(root: &mut egui::Ui, m: &mut Menu) -> UiAction {
    let mut action = UiAction::None;
    if m.online.open {
        egui::CentralPanel::default().show(root, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(ui.available_height() * 0.12);
                m.online.ui(ui);
            });
        });
        return action;
    }
    if m.single_open {
        return single_player(root, m);
    }
    egui::CentralPanel::default().show(root, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.25);
            ui.label(RichText::new("IRON DOTS").size(48.0).strong());
            ui.label("minimalist real-time strategy");
            ui.add_space(32.0);
            if ui.button(RichText::new("  Play vs AI  ").size(20.0)).clicked() {
                m.single_open = true;
            }
            ui.add_space(8.0);
            if ui.button(RichText::new("  Online  ").size(20.0)).clicked() {
                m.online.open = true;
            }
            ui.add_space(8.0);
            if ui.button("Map editor").clicked() {
                action = UiAction::OpenEditor;
            }
            if ui.button("Settings").clicked() {
                action = UiAction::OpenSettings;
            }
            if ui.button("Quit").clicked() {
                action = UiAction::Quit;
            }
            if let Some(e) = &m.error {
                ui.colored_label(Color32::LIGHT_RED, e);
            }
            ui.add_space(24.0);
            ui.weak("Replay viewer and tutorial arrive in later updates.");
        });
    });
    action
}

/// "Play vs AI": game mode, then the match settings.
fn single_player(root: &mut egui::Ui, m: &mut Menu) -> UiAction {
    let mut action = UiAction::None;
    egui::CentralPanel::default().show(root, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.1);
            ui.label(RichText::new("PLAY VS AI").size(32.0).strong());
            ui.add_space(16.0);
            ui.label(RichText::new("Game mode").strong());
            ui.add_space(6.0);
            let limited_units = match m.settings.mode {
                GameMode::Limited { units } => units,
                GameMode::Attrition => m.limited_units,
            };
            crate::ui::centered(ui, 640.0, |ui| {
                ui.horizontal(|ui| {
                    for (mode, title, text) in [
                        (
                            GameMode::Attrition,
                            "War of attrition",
                            "Start from your capital. Empty cities keep raising units for as long as\nyou can pay: the war goes on until one side breaks.",
                        ),
                        (
                            GameMode::Limited { units: limited_units },
                            "Limited war",
                            "The map is split in half from the start, with a front between.\nEvery city has a defender, and each side may create only so many units.",
                        ),
                    ] {
                        let on = std::mem::discriminant(&m.settings.mode) == std::mem::discriminant(&mode);
                        let fill = if on { Color32::from_rgb(30, 70, 110) } else { Color32::from_rgb(34, 37, 42) };
                        let r = egui::Frame::new()
                            .fill(fill)
                            .corner_radius(6.0)
                            .inner_margin(12.0)
                            .show(ui, |ui| {
                                ui.vertical(|ui| {
                                    ui.set_width(290.0);
                                    ui.label(RichText::new(title).size(18.0).strong());
                                    ui.add_space(4.0);
                                    ui.label(text.replace('\n', " "));
                                });
                            })
                            .response
                            .interact(egui::Sense::click());
                        if r.clicked() {
                            m.settings.mode = mode;
                        }
                    }
                });
            });
            ui.add_space(12.0);
            crate::ui::centered(ui, 420.0, |ui| {
                egui::Grid::new("settings").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                    if let GameMode::Limited { units } = &mut m.settings.mode {
                        ui.label("Units per side");
                        ui.add(egui::Slider::new(units, 10..=1000).logarithmic(true));
                        m.limited_units = *units;
                        ui.end_row();
                    }
                    ui.label("Map");
                    map_choice(ui, "mapfile", &m.maps, &mut m.settings.map_file);
                    ui.end_row();
                    if let Some(f) = &m.settings.map_file {
                        ui.label("Players");
                        ui.weak(format!("{}: you and {} bots, teams as on the map", f.mode(), f.players() - 1));
                        ui.end_row();
                    } else {
                        ui.label("Map size");
                        map_picker(ui, "map", &mut m.settings.map, MAP_PRESETS.len());
                        ui.end_row();
                        ui.label("Opponents");
                        ui.add(egui::Slider::new(&mut m.settings.opponents, 1..=7));
                        ui.end_row();
                    }
                    ui.label("Difficulty");
                    ui.horizontal(|ui| {
                        for (d, name) in
                            [(Difficulty::Easy, "Easy"), (Difficulty::Normal, "Normal"), (Difficulty::Hard, "Hard")]
                        {
                            ui.selectable_value(&mut m.settings.difficulty, d, name);
                        }
                    });
                    ui.end_row();
                    let attrition = m.settings.mode == GameMode::Attrition;
                    if m.settings.map_file.is_none() {
                        ui.label("Map seed");
                        ui.add(egui::DragValue::new(&mut m.settings.seed));
                        ui.end_row();
                        if attrition {
                            ui.label("Sandbox");
                            ui.checkbox(&mut m.settings.sandbox, "huge armies, no upkeep or supply limits");
                            ui.end_row();
                        }
                    }
                    if m.settings.sandbox && m.settings.map_file.is_none() && attrition {
                        ui.label("Units per side");
                        ui.add(egui::Slider::new(&mut m.settings.units_per_player, 100..=12_500).logarithmic(true));
                        ui.end_row();
                    }
                });
            });
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                ui.add_space((ui.available_width() - 220.0).max(0.0) / 2.0);
                if ui.button(RichText::new("  Start  ").size(20.0)).clicked() {
                    let mut s = m.settings.clone();
                    if s.mode != GameMode::Attrition {
                        s.sandbox = false;
                    }
                    action = UiAction::StartMatch(s);
                }
                if ui.button(RichText::new("  Back  ").size(20.0)).clicked() {
                    m.single_open = false;
                }
            });
            if let Some(e) = &m.error {
                ui.colored_label(Color32::LIGHT_RED, e);
            }
        });
    });
    action
}

/// Combo box over the 16:9-ish map presets (the first `count` of them).
pub fn map_picker(ui: &mut egui::Ui, id: &str, map: &mut [u32; 2], count: usize) {
    let label = |w: u32, h: u32| match MAP_PRESETS.iter().find(|p| p.1 == w && p.2 == h) {
        Some((name, ..)) => format!("{name}  {w}×{h}"),
        None => format!("{w}×{h}"),
    };
    egui::ComboBox::from_id_salt(id).selected_text(label(map[0], map[1])).show_ui(ui, |ui| {
        for &(_, w, h) in &MAP_PRESETS[..count] {
            ui.selectable_value(map, [w, h], label(w, h));
        }
    });
}

/// Lays out `f` in a column of `width` centred in the available space.
pub fn centered<R>(ui: &mut egui::Ui, width: f32, f: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.horizontal(|ui| {
        ui.add_space(((ui.available_width() - width) / 2.0).max(0.0));
        ui.vertical(|ui| {
            ui.set_max_width(width);
            f(ui)
        })
        .inner
    })
    .inner
}

fn money(v: i64) -> String {
    format!("{}", v / 1000)
}

/// In-game HUD and panels. `menu_open` is the Esc menu.
pub fn hud(ctx: &egui::Context, g: &mut Game, menu_open: &mut bool) -> UiAction {
    let mut action = UiAction::None;
    let w = &g.world;
    let me = g.me;
    let st = w.player(me);
    let (used, limit) = w.supply_totals(me);
    let total = w.cities().len().max(1);
    let mine = w.cities().iter().filter(|c| c.owner == me).count();
    let share = mine * 100 / total;
    let goal = w.rules().win_city_share.0 / 10;

    egui::Area::new(egui::Id::unique("hud")).anchor(Align2::LEFT_TOP, [8.0, 8.0]).show(ctx, |ui| {
        hud_frame(ui).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("Money {}", money(st.money))).strong());
                let inc = st.net_income / 1000;
                let col = if inc >= 0 { Color32::LIGHT_GREEN } else { Color32::LIGHT_RED };
                ui.colored_label(col, format!("{inc:+}/s"));
                ui.separator();
                let supply_col = if used > limit { Color32::LIGHT_RED } else { ui.visuals().text_color() };
                ui.colored_label(supply_col, format!("Supply {used}/{limit}"));
                ui.separator();
                ui.label(format!("Cities {mine}/{total} ({share}% of {goal}%)"));
                if let Some(b) = w.unit_budget(me) {
                    ui.separator();
                    let col = if b == 0 { Color32::LIGHT_RED } else { ui.visuals().text_color() };
                    ui.colored_label(col, format!("Units left to create {b}"));
                }
                if st.broke {
                    ui.separator();
                    ui.colored_label(Color32::LIGHT_RED, "Treasury empty: troops starve");
                }
            });
            let secs = w.tick() / w.rules().tick_rate;
            let speed = match &g.online {
                Some(o) => {
                    let (behind, buffer) = o.delay();
                    format!("online, delay {behind}/{buffer} ticks")
                }
                None if g.paused => "paused".to_string(),
                None => format!("×{}", SPEEDS[g.speed]),
            };
            ui.weak(format!("{}:{:02}  {speed}  ·  {} selected", secs / 60, secs % 60, g.selection.len()));
        });
    });

    // Positions: draw fronts that units hold and refill from the cities.
    egui::Area::new(egui::Id::unique("positions")).anchor(Align2::LEFT_BOTTOM, [8.0, -8.0]).show(ctx, |ui| {
        hud_frame(ui).show(ui, |ui| {
            ui.label(RichText::new("Positions").strong());
            let text = if g.front_tool { "Drawing: drag with the left button…" } else { "Draw a front  [F]" };
            if ui.selectable_label(g.front_tool, text).clicked() {
                g.front_tool = !g.front_tool;
                g.front_draft = None;
            }
            let mut remove = None;
            for (k, f) in g.fronts.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(format!("Front {}: {}/{} held", k + 1, f.filled(g.world.units()), f.places.len()));
                    if ui.small_button("remove").clicked() {
                        remove = Some(k);
                    }
                });
            }
            if let Some(k) = remove {
                g.fronts.remove(k);
            }
            if g.fronts.is_empty() && !g.front_tool {
                ui.weak("Selected units (or the nearest) take the places;");
                ui.weak("idle units in your cities fill the empty ones.");
            }
        });
    });

    if g.selection.len() >= 2 && g.selected_city.is_none() {
        egui::Area::new(egui::Id::unique("formation")).anchor(Align2::CENTER_BOTTOM, [0.0, -12.0]).show(ctx, |ui| {
            hud_frame(ui).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{} units", g.selection.len())).strong());
                    ui.separator();
                    for (f, name, tip) in [
                        (Formation::Keep, "Keep", "keep the current spacing"),
                        (Formation::Line, "Line", "wide front, a few ranks deep"),
                        (Formation::Staggered, "Staggered", "loose line with gaps"),
                        (Formation::Box, "Box", "square block"),
                        (Formation::Column, "Column", "narrow column for marching"),
                        (Formation::Flank, "Flank", "two wings with a gap in the centre"),
                    ] {
                        if ui.selectable_label(g.formation == f, name).on_hover_text(tip).clicked() {
                            g.set_formation(f);
                        }
                    }
                });
            });
        });
    }

    if let Some(c) = g.selected_city {
        egui::Area::new(egui::Id::unique("city")).anchor(Align2::CENTER_BOTTOM, [0.0, -12.0]).show(ctx, |ui| {
            hud_frame(ui).show(ui, |ui| {
                let city = &g.world.cities()[c];
                let title = if city.capital_of == me { "Capital" } else { "City" };
                ui.label(RichText::new(format!("{title} #{c}")).strong());
                if let Some((u, l)) = g.world.city_supply(c) {
                    ui.label(format!("Pocket supply {u}/{l}"));
                }
                let rules = g.world.rules();
                let (cl, ch) = (rules.cost[0] / 1000, rules.cost[1] / 1000);
                ui.horizontal(|ui| {
                    if ui.button(format!("Light ({cl})  [Q]")).clicked() {
                        g.buy(UnitKind::Light);
                    }
                    if ui.button(format!("Heavy ({ch})  [W]")).clicked() {
                        g.buy(UnitKind::Heavy);
                    }
                });
            });
        });
    }

    if let Some(o) = &g.online {
        let text = match (&o.desync, &o.status) {
            (Some(path), _) => Some((Color32::LIGHT_RED, format!("Out of sync with the other players. Dump: {path}"))),
            (None, Some(s)) => Some((Color32::LIGHT_YELLOW, s.clone())),
            _ => None,
        };
        if let Some((col, text)) = text {
            egui::Area::new(egui::Id::unique("net")).anchor(Align2::CENTER_TOP, [0.0, 48.0]).show(ctx, |ui| {
                hud_frame(ui).show(ui, |ui| {
                    ui.colored_label(col, text);
                });
            });
        }
    }

    if let Some((msg, _)) = &g.message {
        egui::Area::new(egui::Id::unique("msg")).anchor(Align2::CENTER_TOP, [0.0, 12.0]).show(ctx, |ui| {
            hud_frame(ui).show(ui, |ui| {
                ui.colored_label(Color32::LIGHT_YELLOW, msg);
            });
        });
    }

    if let Some(team) = g.world.winner() {
        let won = team == g.world.team_of(me);
        egui::Window::new(if won { "Victory" } else { "Defeat" })
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                let secs = g.world.tick() / g.world.rules().tick_rate;
                ui.label(format!("Match length {}:{:02}", secs / 60, secs % 60));
                end_charts(ui, g);
                ui.horizontal(|ui| {
                    if g.online.is_none() && ui.button("Rematch").clicked() {
                        action = UiAction::StartMatch(g.settings.clone());
                    }
                    if ui.button("Main menu").clicked() {
                        action = UiAction::ToMenu;
                    }
                });
            });
    } else if *menu_open {
        egui::Window::new("Menu").anchor(Align2::CENTER_CENTER, [0.0, 0.0]).collapsible(false).resizable(false).show(
            ctx,
            |ui| {
                if ui.button("Resume").clicked() {
                    *menu_open = false;
                }
                if ui.button("Settings").clicked() {
                    action = UiAction::OpenSettings;
                }
                let leave = if g.online.is_some() { "Leave match" } else { "Surrender" };
                if ui.button(leave).clicked() {
                    action = UiAction::ToMenu;
                }
                if ui.button("Quit game").clicked() {
                    action = UiAction::Quit;
                }
                ui.separator();
                ui.weak("LMB select / drag box · RMB move / drag path · Shift queue");
                ui.weak("Ctrl+1–9 store group · 1–9 select · wheel zoom · MMB drag pan");
                ui.weak("Other keys: see Settings, Keys");
            },
        );
    }
    action
}

/// Units / cities / money over time per player, as simple line charts.
fn end_charts(ui: &mut egui::Ui, g: &Game) {
    let h = &g.history;
    if h.len() < 2 {
        return;
    }
    let colors = render::palette::DEFAULT;
    for (title, pick) in [("Units", 0usize), ("Cities", 1), ("Money", 2)] {
        ui.label(title);
        let (rect, _) = ui.allocate_exact_size(egui::vec2(360.0, 60.0), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 2.0, Color32::from_black_alpha(60));
        let val = |row: &[(u32, u32, i64); 9], p: usize| -> f32 {
            match pick {
                0 => row[p].0 as f32,
                1 => row[p].1 as f32,
                _ => row[p].2 as f32,
            }
        };
        let max = h
            .iter()
            .flat_map(|row| (1..=g.world.player_count() as usize).map(move |p| val(row, p)))
            .fold(1.0f32, f32::max);
        for (p, c) in colors.iter().enumerate().take(g.world.player_count() as usize + 1).skip(1) {
            let col = Color32::from_rgb((c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8);
            let pts: Vec<egui::Pos2> = h
                .iter()
                .enumerate()
                .map(|(k, row)| {
                    let x = rect.left() + rect.width() * k as f32 / (h.len() - 1) as f32;
                    let y = rect.bottom() - rect.height() * val(row, p) / max;
                    egui::pos2(x, y)
                })
                .collect();
            painter.add(egui::Shape::line(pts, egui::Stroke::new(1.5, col)));
        }
    }
}

/// The settings window (spec 2.10). Returns true when it should close.
pub fn settings_window(
    ctx: &egui::Context,
    s: &mut crate::settings::Settings,
    rebinding: &mut Option<crate::settings::Action>,
) -> bool {
    use crate::settings::{Action, key_name};
    let mut close = false;
    egui::Window::new("Settings").anchor(Align2::CENTER_CENTER, [0.0, 0.0]).collapsible(false).resizable(false).show(
        ctx,
        |ui| {
            ui.label(RichText::new("Graphics").strong());
            egui::Grid::new("gfx").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
                ui.label("Full screen");
                ui.checkbox(&mut s.fullscreen, "borderless, F11 to switch");
                ui.end_row();
                ui.label("VSync");
                ui.checkbox(&mut s.vsync, "");
                ui.end_row();
                ui.label("FPS limit");
                ui.horizontal(|ui| {
                    let mut on = s.fps_limit > 0;
                    if ui.checkbox(&mut on, "").changed() {
                        s.fps_limit = if on { 144 } else { 0 };
                    }
                    if on {
                        ui.add(egui::Slider::new(&mut s.fps_limit, 30..=360));
                    }
                });
                ui.end_row();
                ui.label("Interface scale");
                ui.add(egui::Slider::new(&mut s.ui_scale, 0.75..=2.0).step_by(0.05));
                ui.end_row();
            });
            ui.separator();
            ui.label(RichText::new("Camera").strong());
            egui::Grid::new("cam").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
                ui.label("Scroll at screen edges");
                ui.checkbox(&mut s.edge_scroll, "");
                ui.end_row();
                ui.label("Scroll speed");
                ui.add(egui::Slider::new(&mut s.scroll_speed, 0.25..=4.0).logarithmic(true));
                ui.end_row();
                ui.label("Zoom sensitivity");
                ui.add(egui::Slider::new(&mut s.zoom_speed, 0.25..=4.0).logarithmic(true));
                ui.end_row();
            });
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(RichText::new("Keys").strong());
                if ui.small_button("Reset to defaults").clicked() {
                    s.reset_keys();
                    *rebinding = None;
                }
            });
            egui::Grid::new("keys").num_columns(4).spacing([16.0, 4.0]).show(ui, |ui| {
                for (k, a) in Action::ALL.into_iter().enumerate() {
                    ui.label(a.label());
                    let text = if *rebinding == Some(a) {
                        "press a key…".to_string()
                    } else {
                        key_name(s.key(a)).to_string()
                    };
                    if ui.add(egui::Button::new(text).min_size(egui::vec2(90.0, 0.0))).clicked() {
                        *rebinding = Some(a);
                    }
                    if k % 2 == 1 {
                        ui.end_row();
                    }
                }
            });
            ui.weak("Fixed: mouse, Ctrl+1–9 / 1–9 groups, Esc menu, Alt+Enter full screen, numpad +/− speed.");
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(RichText::new("Sound").strong());
                ui.weak("(sounds arrive in the next update)");
            });
            egui::Grid::new("snd").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
                for (name, v) in [
                    ("Master", &mut s.master_volume),
                    ("Effects", &mut s.effects_volume),
                    ("Music", &mut s.music_volume),
                    ("Alerts", &mut s.alert_volume),
                ] {
                    ui.label(name);
                    ui.add(egui::Slider::new(v, 0.0..=1.0).show_value(false));
                    ui.end_row();
                }
                ui.label("");
                ui.checkbox(&mut s.mute_unfocused, "Mute when the window is in the background");
                ui.end_row();
            });
            ui.add_space(8.0);
            if ui.button(RichText::new("  Done  ").size(16.0)).clicked() {
                close = true;
            }
        },
    );
    close
}
