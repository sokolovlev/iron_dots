//! egui screens: main menu, HUD, city panel, pause menu, end of match.

use crate::game::{Game, MatchSettings, SPEEDS};
use ai::Difficulty;
use egui::{Align2, Color32, RichText};
use sim_core::UnitKind;
use sim_core::scenario::MAP_PRESETS;

/// What the UI asks the app to do.
pub enum UiAction {
    None,
    StartMatch(MatchSettings),
    ToMenu,
    Quit,
}

#[derive(Default)]
pub struct Menu {
    pub settings: MatchSettings,
    pub error: Option<String>,
    pub online: crate::online::OnlineMenu,
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
    egui::CentralPanel::default().show(root, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.2);
            ui.label(RichText::new("IRON DOTS").size(48.0).strong());
            ui.label("minimalist real-time strategy");
            ui.add_space(24.0);
            egui::Grid::new("settings").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                ui.label("Map size");
                map_picker(ui, "map", &mut m.settings.map, MAP_PRESETS.len());
                ui.end_row();
                ui.label("Opponents");
                ui.add(egui::Slider::new(&mut m.settings.opponents, 1..=7));
                ui.end_row();
                ui.label("Difficulty");
                ui.horizontal(|ui| {
                    for (d, name) in
                        [(Difficulty::Easy, "Easy"), (Difficulty::Normal, "Normal"), (Difficulty::Hard, "Hard")]
                    {
                        ui.selectable_value(&mut m.settings.difficulty, d, name);
                    }
                });
                ui.end_row();
                ui.label("Map seed");
                ui.add(egui::DragValue::new(&mut m.settings.seed));
                ui.end_row();
                ui.label("Sandbox");
                ui.checkbox(&mut m.settings.sandbox, "huge armies, no upkeep or supply limits");
                ui.end_row();
                if m.settings.sandbox {
                    ui.label("Units per side");
                    ui.add(egui::Slider::new(&mut m.settings.units_per_player, 100..=12_500).logarithmic(true));
                    ui.end_row();
                    let total = m.settings.units_per_player as u64 * (m.settings.opponents as u64 + 1);
                    ui.label("");
                    ui.weak(format!("{total} units in total"));
                    ui.end_row();
                }
            });
            ui.add_space(16.0);
            if ui.button(RichText::new("  Play vs AI  ").size(20.0)).clicked() {
                action = UiAction::StartMatch(m.settings.clone());
            }
            ui.add_space(8.0);
            if ui.button(RichText::new("  Online  ").size(20.0)).clicked() {
                m.online.open = true;
            }
            ui.add_space(8.0);
            if ui.button("Quit").clicked() {
                action = UiAction::Quit;
            }
            if let Some(e) = &m.error {
                ui.colored_label(Color32::LIGHT_RED, e);
            }
            ui.add_space(24.0);
            ui.weak("Replays viewer, editor and tutorial arrive in later stages.");
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
                let leave = if g.online.is_some() { "Leave match" } else { "Surrender" };
                if ui.button(leave).clicked() {
                    action = UiAction::ToMenu;
                }
                if ui.button("Quit game").clicked() {
                    action = UiAction::Quit;
                }
                ui.separator();
                ui.weak("LMB select / drag box · RMB move / drag path · Shift queue");
                ui.weak("S stop · C clear queue · Q/W buy · Ctrl+1–9 groups · Home capital");
                ui.weak("Space pause · +/− speed · wheel zoom · MMB drag pan");
                ui.weak("F11 or Alt+Enter: full screen / window");
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
