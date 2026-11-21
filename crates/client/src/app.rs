//! Window, input and frame loop (winit + wgpu + egui).

use crate::game::{Game, MatchSettings, SPEEDS};
use crate::gpu::Gpu;
use crate::settings::{Action, Settings};
use crate::ui::{self, Menu, UiAction};
use render::{Marker, Overlay, Renderer};
use sim_core::UnitKind;
use std::sync::Arc;
use std::time::Instant;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::{Fullscreen, Window, WindowId};

/// Mouse movement (pixels) below which a press counts as a click.
const CLICK_SLOP: f32 = 6.0;
const DOUBLE_CLICK: f32 = 0.35;
const EDGE_SCROLL_PX: f32 = 6.0;

struct Gfx {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    gpu: Gpu,
    config: wgpu::SurfaceConfiguration,
    egui_ctx: egui::Context,
    egui_state: egui_winit::State,
    egui_renderer: egui_wgpu::Renderer,
}

#[derive(Default)]
struct Mouse {
    pos: [f32; 2],
    left_down: Option<[f32; 2]>,
    right_down: Option<[f32; 2]>,
    middle_down: Option<[f32; 2]>,
    last_click: Option<(Instant, [f32; 2])>,
}

pub struct App {
    gfx: Option<Gfx>,
    renderer: Option<Renderer>,
    game: Option<Game>,
    menu: Menu,
    esc_menu: bool,
    mouse: Mouse,
    mods: ModifiersState,
    last_frame: Instant,
    last_group: Option<(usize, Instant)>,
    markers: Vec<Marker>,
    overlay: Overlay,
    focused: bool,
    settings: Settings,
    /// The settings window is open; `Some(action)` while waiting for a key.
    settings_open: bool,
    rebinding: Option<Action>,
    /// Camera keys held: left, right, up, down.
    camera_keys: [bool; 4],
    /// Start of the current frame, for the FPS cap.
    frame_start: Instant,
    /// `None` without an audio device, or before the window exists.
    audio: Option<crate::sound::Audio>,
    audio_ready: bool,
    /// The map editor is open (it stays open while a test match runs).
    editor: Option<crate::editor::Editor>,
    pending_start: Option<MatchSettings>,
    /// Borderless full screen (default) or a normal window; F11 / Alt+Enter.
    fullscreen: bool,
}

pub fn run(start: Option<MatchSettings>, windowed: bool) -> Result<(), String> {
    let settings = Settings::load();
    let fullscreen = settings.fullscreen && !windowed;
    let event_loop = EventLoop::new().map_err(|e| e.to_string())?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = App {
        gfx: None,
        renderer: None,
        game: None,
        menu: Menu::default(),
        esc_menu: false,
        mouse: Mouse::default(),
        mods: ModifiersState::empty(),
        last_frame: Instant::now(),
        last_group: None,
        markers: Vec::new(),
        overlay: Overlay::default(),
        focused: true,
        settings_open: false,
        rebinding: None,
        camera_keys: [false; 4],
        frame_start: Instant::now(),
        audio: None,
        audio_ready: false,
        editor: None,
        pending_start: None,
        fullscreen,
        settings,
    };
    app.menu.online.name = app.settings.name.clone();
    app.menu.online.address = app.settings.server.clone();
    if let Some(s) = start {
        app.menu.settings = s.clone();
        app.pending_start = Some(s);
    }
    event_loop.run_app(&mut app).map_err(|e| e.to_string())
}

impl App {
    fn viewport(&self) -> [f32; 2] {
        self.gfx.as_ref().map(|g| [g.config.width as f32, g.config.height as f32]).unwrap_or([1.0, 1.0])
    }

    fn start_match(&mut self, s: MatchSettings) {
        match Game::new(s) {
            Ok(game) => {
                if let Some(gfx) = &self.gfx {
                    self.renderer =
                        Some(Renderer::new(&gfx.gpu.device, &gfx.gpu.queue, gfx.config.format, game.world.map()));
                }
                self.game = Some(game);
                self.esc_menu = false;
                self.menu.error = None;
            }
            Err(e) => self.menu.error = Some(e),
        }
    }

    fn start_online(&mut self, s: crate::online::Started) {
        if self.settings.name != self.menu.online.name || self.settings.server != self.menu.online.address {
            self.settings.name = self.menu.online.name.clone();
            self.settings.server = self.menu.online.address.clone();
            self.settings.save();
        }
        match Game::online(s) {
            Ok(game) => {
                if let Some(gfx) = &self.gfx {
                    self.renderer =
                        Some(Renderer::new(&gfx.gpu.device, &gfx.gpu.queue, gfx.config.format, game.world.map()));
                }
                self.game = Some(game);
                self.esc_menu = false;
                self.menu.error = None;
            }
            Err(e) => self.menu.error = Some(e),
        }
    }

    fn close_settings(&mut self) {
        self.settings_open = false;
        self.rebinding = None;
        self.settings.sanitize();
        self.settings.save();
        self.apply_settings();
    }

    /// Pushes settings that live outside `Settings` into the window and GPU.
    fn apply_settings(&mut self) {
        if self.fullscreen != self.settings.fullscreen {
            self.fullscreen = self.settings.fullscreen;
            if let Some(gfx) = &self.gfx {
                gfx.window.set_fullscreen(self.fullscreen.then_some(Fullscreen::Borderless(None)));
            }
        }
        if let Some(gfx) = &mut self.gfx {
            let mode = if self.settings.vsync { wgpu::PresentMode::AutoVsync } else { wgpu::PresentMode::AutoNoVsync };
            if gfx.config.present_mode != mode {
                gfx.config.present_mode = mode;
                gfx.surface.configure(&gfx.gpu.device, &gfx.config);
            }
            gfx.egui_ctx.set_zoom_factor(self.settings.ui_scale);
        }
    }

    /// Saves the current match to `replays/<unix time>.idr` (spec 2.10).
    fn save_replay(&self) {
        let Some(g) = &self.game else { return };
        if g.world.tick() == 0 {
            return;
        }
        let stamp =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default();
        let dir = std::path::Path::new("replays");
        let path = dir.join(format!("{stamp}.idr"));
        if let Err(e) = std::fs::create_dir_all(dir).and_then(|_| std::fs::write(&path, g.replay().encode())) {
            eprintln!("cannot save replay {}: {e}", path.display());
        }
    }

    fn handle_key(&mut self, code: KeyCode, _event_loop: &ActiveEventLoop) {
        let vp = self.viewport();
        let ctrl = self.mods.control_key() || self.mods.super_key();
        let shift = self.mods.shift_key();
        let action = self.settings.action(code);
        if action == Some(Action::Fullscreen) || (code == KeyCode::Enter && self.mods.alt_key()) {
            self.fullscreen = !self.fullscreen;
            if let Some(gfx) = &self.gfx {
                gfx.window.set_fullscreen(self.fullscreen.then_some(Fullscreen::Borderless(None)));
            }
            self.settings.fullscreen = self.fullscreen;
            self.settings.save();
            return;
        }
        if code == KeyCode::Escape {
            if self.settings_open {
                self.close_settings();
            } else if self.game.is_some() {
                self.esc_menu = !self.esc_menu;
            } else if self.editor.is_none() && self.menu.online.open {
                // Esc backs out of the online screen; the main menu itself
                // never quits on Esc (that is what the Quit button is for).
                self.menu.online.open = false;
            }
            return;
        }
        if self.game.is_none() {
            if let Some(e) = &mut self.editor {
                match code {
                    KeyCode::KeyZ if ctrl && shift => e.redo(),
                    KeyCode::KeyZ if ctrl => e.undo(),
                    KeyCode::KeyY if ctrl => e.redo(),
                    _ => {}
                }
                return;
            }
        }
        let Some(g) = &mut self.game else { return };
        let digit = match code {
            KeyCode::Digit1 => Some(1),
            KeyCode::Digit2 => Some(2),
            KeyCode::Digit3 => Some(3),
            KeyCode::Digit4 => Some(4),
            KeyCode::Digit5 => Some(5),
            KeyCode::Digit6 => Some(6),
            KeyCode::Digit7 => Some(7),
            KeyCode::Digit8 => Some(8),
            KeyCode::Digit9 => Some(9),
            _ => None,
        };
        if let Some(k) = digit {
            if ctrl {
                g.store_group(k);
            } else {
                let center = g.recall_group(k);
                let twice = self.last_group.is_some_and(|(lk, t)| lk == k && t.elapsed().as_secs_f32() < 0.4);
                if twice {
                    if let Some(c) = center {
                        g.camera.center = c;
                    }
                }
                self.last_group = Some((k, Instant::now()));
            }
            return;
        }
        // The numpad keeps +/− for speed whatever the bindings.
        let action = match code {
            KeyCode::NumpadAdd if action.is_none() => Some(Action::SpeedUp),
            KeyCode::NumpadSubtract if action.is_none() => Some(Action::SpeedDown),
            _ => action,
        };
        let offline = g.online.is_none();
        match action {
            Some(Action::Stop) => g.stop(),
            Some(Action::ClearQueue) => g.clear_queue(),
            Some(Action::BuyLight) => g.buy(UnitKind::Light),
            Some(Action::BuyHeavy) => g.buy(UnitKind::Heavy),
            Some(Action::Capital) => {
                if let Some(c) = g.capital_center() {
                    g.camera.center = c;
                }
            }
            // Network matches run at the relay's pace.
            Some(Action::Pause) if offline => g.paused = !g.paused,
            Some(Action::SpeedUp) if offline => g.speed = (g.speed + 1).min(SPEEDS.len() - 1),
            Some(Action::SpeedDown) if offline => g.speed = g.speed.saturating_sub(1),
            _ => {}
        }
        let _ = (vp, shift);
    }

    fn mouse_button(&mut self, button: MouseButton, state: ElementState) {
        let vp = self.viewport();
        let pos = self.mouse.pos;
        let shift = self.mods.shift_key();
        if self.game.is_none() {
            if let Some(e) = &mut self.editor {
                match (button, state) {
                    (MouseButton::Left, ElementState::Pressed) => {
                        self.mouse.left_down = Some(pos);
                        e.press(pos, vp);
                    }
                    (MouseButton::Left, ElementState::Released) => {
                        self.mouse.left_down = None;
                        e.release();
                    }
                    (MouseButton::Right, ElementState::Pressed) => e.right_click(pos, vp),
                    (MouseButton::Middle, ElementState::Pressed) => self.mouse.middle_down = Some(pos),
                    (MouseButton::Middle, ElementState::Released) => self.mouse.middle_down = None,
                    _ => {}
                }
                return;
            }
        }
        let Some(g) = &mut self.game else { return };
        let dist = |a: [f32; 2]| ((a[0] - pos[0]).powi(2) + (a[1] - pos[1]).powi(2)).sqrt();
        match (button, state) {
            (MouseButton::Left, ElementState::Pressed) => self.mouse.left_down = Some(pos),
            (MouseButton::Left, ElementState::Released) => {
                let Some(start) = self.mouse.left_down.take() else { return };
                if g.box_select.is_some() && dist(start) > CLICK_SLOP {
                    g.finish_box(vp, shift);
                } else {
                    g.box_select = None;
                    let double = self
                        .mouse
                        .last_click
                        .is_some_and(|(t, p)| t.elapsed().as_secs_f32() < DOUBLE_CLICK && dist(p) < CLICK_SLOP);
                    if double {
                        g.select_same_type(pos, vp);
                        self.mouse.last_click = None;
                    } else {
                        g.click_select(pos, vp, shift);
                        self.mouse.last_click = Some((Instant::now(), pos));
                    }
                }
            }
            (MouseButton::Right, ElementState::Pressed) => {
                self.mouse.right_down = Some(pos);
                g.drawing = Some(vec![g.camera.screen_to_world(pos, vp)]);
            }
            (MouseButton::Right, ElementState::Released) => {
                let Some(start) = self.mouse.right_down.take() else { return };
                let path = g.drawing.take().unwrap_or_default();
                if dist(start) <= CLICK_SLOP {
                    let w = g.camera.screen_to_world(pos, vp);
                    g.order_move(&[w], shift);
                } else {
                    g.order_move(&path, shift);
                }
            }
            (MouseButton::Middle, ElementState::Pressed) => self.mouse.middle_down = Some(pos),
            (MouseButton::Middle, ElementState::Released) => self.mouse.middle_down = None,
            _ => {}
        }
    }

    fn mouse_moved(&mut self, pos: [f32; 2]) {
        let vp = self.viewport();
        let prev = self.mouse.pos;
        self.mouse.pos = pos;
        if self.game.is_none() {
            if let Some(e) = &mut self.editor {
                if self.mouse.middle_down.is_some() {
                    e.camera.center[0] -= (pos[0] - prev[0]) / e.camera.scale;
                    e.camera.center[1] -= (pos[1] - prev[1]) / e.camera.scale;
                }
                e.drag(pos, vp, self.mouse.left_down.is_some());
                return;
            }
        }
        let Some(g) = &mut self.game else { return };
        if self.mouse.middle_down.is_some() {
            g.camera.center[0] -= (pos[0] - prev[0]) / g.camera.scale;
            g.camera.center[1] -= (pos[1] - prev[1]) / g.camera.scale;
        }
        if let Some(start) = self.mouse.left_down {
            let d = ((start[0] - pos[0]).powi(2) + (start[1] - pos[1]).powi(2)).sqrt();
            if d > CLICK_SLOP {
                g.box_select = Some((start, pos));
            }
        }
        if let Some(path) = &mut g.drawing {
            let w = g.camera.screen_to_world(pos, vp);
            let last = *path.last().expect("path starts with a point");
            if (w[0] - last[0]).powi(2) + (w[1] - last[1]).powi(2) > 0.09 {
                path.push(w);
            }
        }
    }

    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        self.frame_start = now;
        let dt = (now - self.last_frame).as_secs_f32();
        self.last_frame = now;
        let vp = self.viewport();
        if self.game.is_none() {
            if let Some(started) = self.menu.online.update(dt) {
                self.start_online(started);
            }
        }
        let Some(gfx) = &mut self.gfx else { return };

        // Edge scrolling and camera keys.
        let p = self.mouse.pos;
        // Only while the cursor is inside the window (once it leaves, its
        // position is a far-away placeholder, not an edge) and not over UI.
        let inside = (0.0..vp[0]).contains(&p[0]) && (0.0..vp[1]).contains(&p[1]);
        let edge = self.settings.edge_scroll
            && self.focused
            && !self.esc_menu
            && !self.settings_open
            && inside
            && !gfx.egui_ctx.is_pointer_over_egui();
        let scroll = |cam: &mut render::Camera, size: [f32; 2], keys: [bool; 4], speed: f32| {
            let [left, right, up, down] = keys;
            let speed = 900.0 * speed / cam.scale * dt;
            if left || (edge && p[0] <= EDGE_SCROLL_PX) {
                cam.center[0] -= speed;
            }
            if right || (edge && p[0] >= vp[0] - EDGE_SCROLL_PX) {
                cam.center[0] += speed;
            }
            if up || (edge && p[1] <= EDGE_SCROLL_PX) {
                cam.center[1] -= speed;
            }
            if down || (edge && p[1] >= vp[1] - EDGE_SCROLL_PX) {
                cam.center[1] += speed;
            }
            cam.clamp_to(size, vp);
        };
        if let Some(g) = &mut self.game {
            let size = g.map_size();
            scroll(&mut g.camera, size, self.camera_keys, self.settings.scroll_speed);
            g.update(dt);
        } else if let Some(e) = &mut self.editor {
            let size = e.map_size();
            scroll(&mut e.camera, size, self.camera_keys, self.settings.scroll_speed);
        }
        if let Some(a) = &mut self.audio {
            a.update(dt);
            if self.settings_open {
                // Let the player hear slider changes right away.
                a.apply(&self.settings, self.focused);
            }
            match &mut self.game {
                Some(g) => {
                    a.set_in_match(true);
                    for s in g.sounds.drain(..) {
                        a.play(s);
                    }
                    a.set_combat(g.visible_combat.get(), g.camera.scale);
                }
                None => a.set_in_match(false),
            }
        }

        // UI.
        let raw = gfx.egui_state.take_egui_input(&gfx.window);
        let mut action = UiAction::None;
        let ctx = gfx.egui_ctx.clone();
        let full = ctx.run_ui(raw, |ui| {
            let ctx = ui.ctx().clone();
            action = match &mut self.game {
                // The menu would show through the settings window.
                None if self.settings_open => UiAction::None,
                None if self.editor.is_some() => {
                    let e = self.editor.as_mut().expect("checked");
                    match e.ui(ui, vp, &self.menu.maps) {
                        crate::editor::EditorAction::None => UiAction::None,
                        crate::editor::EditorAction::Exit => UiAction::CloseEditor,
                        crate::editor::EditorAction::Play(m) => UiAction::StartMatch(MatchSettings {
                            map_file: Some(m),
                            difficulty: self.menu.settings.difficulty,
                            ..MatchSettings::default()
                        }),
                    }
                }
                None => ui::main_menu(ui, &mut self.menu),
                Some(g) => {
                    let mut esc = self.esc_menu && !self.settings_open;
                    let a = ui::hud(&ctx, g, &mut esc);
                    if !self.settings_open {
                        self.esc_menu = esc;
                    }
                    a
                }
            };
            if self.settings_open && ui::settings_window(&ctx, &mut self.settings, &mut self.rebinding) {
                action = UiAction::CloseSettings;
            }
        });
        gfx.egui_state.handle_platform_output(&gfx.window, full.platform_output);
        let paint = gfx.egui_ctx.tessellate(full.shapes, full.pixels_per_point);

        let frame = match gfx.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                gfx.surface.configure(&gfx.gpu.device, &gfx.config);
                return;
            }
            _ => return,
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let device = &gfx.gpu.device;
        let queue = &gfx.gpu.queue;
        for (id, deltas) in &full.textures_delta.set {
            for delta in deltas {
                gfx.egui_renderer.update_texture(device, queue, *id, delta);
            }
        }
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [gfx.config.width, gfx.config.height],
            pixels_per_point: full.pixels_per_point,
        };
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        gfx.egui_renderer.update_buffers(device, queue, &mut encoder, &paint, &screen);

        if let (Some(g), Some(r)) = (&mut self.game, &mut self.renderer) {
            let world = &mut g.world;
            let map_changed = {
                let mut chunks = Vec::new();
                world.take_dirty_chunks(|cx, cy| chunks.push((cx, cy)));
                chunks
            };
            for (cx, cy) in map_changed {
                r.upload_chunk(queue, g.world.map(), cx, cy);
            }
            g.markers(&mut self.markers, vp);
            g.overlay(&mut self.overlay, vp);
            r.set_markers(device, queue, &self.markers);
            r.set_overlay(device, queue, &self.overlay.vertices);
            r.set_camera(queue, &g.camera, vp, g.time);
        } else if let Some(e) = &mut self.editor {
            if e.resized || self.renderer.is_none() {
                e.resized = false;
                e.take_dirty();
                self.renderer = Some(Renderer::new(device, queue, gfx.config.format, &e.map));
            }
            let r = self.renderer.as_mut().expect("just made");
            for (cx, cy) in e.take_dirty() {
                r.upload_chunk(queue, &e.map, cx, cy);
            }
            e.markers(&mut self.markers);
            e.overlay(&mut self.overlay);
            r.set_markers(device, queue, &self.markers);
            r.set_overlay(device, queue, &self.overlay.vertices);
            r.set_camera(queue, &e.camera, vp, 0.0);
        }
        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("main"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.10, g: 0.11, b: 0.13, a: 1.0 }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            let mut pass = pass.forget_lifetime();
            if let (true, Some(r)) = (self.game.is_some() || self.editor.is_some(), &self.renderer) {
                r.draw(&mut pass);
            }
            gfx.egui_renderer.render(&mut pass, &paint, &screen);
        }
        queue.submit([encoder.finish()]);
        gfx.window.pre_present_notify();
        queue.present(frame);
        for id in &full.textures_delta.free {
            gfx.egui_renderer.free_texture(id);
        }

        match action {
            UiAction::None => {}
            UiAction::StartMatch(s) => {
                self.save_replay();
                if let Some(e) = &mut self.editor {
                    // Back in the editor after the match: redraw its map.
                    e.resized = true;
                }
                self.start_match(s)
            }
            UiAction::ToMenu => {
                self.save_replay();
                self.game = None;
                self.renderer = None;
            }
            UiAction::OpenEditor => self.editor = Some(crate::editor::Editor::new()),
            UiAction::CloseEditor => {
                self.editor = None;
                self.renderer = None;
                self.menu.reload_maps();
            }
            UiAction::Quit => event_loop.exit(),
            UiAction::OpenSettings => self.settings_open = true,
            UiAction::CloseSettings => self.close_settings(),
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gfx.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("Iron Dots")
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 800.0))
            // Borderless on the current monitor: no title bar, no taskbar.
            .with_fullscreen(self.fullscreen.then_some(Fullscreen::Borderless(None)));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("cannot create window: {e}");
                event_loop.exit();
                return;
            }
        };
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle_from_env(Box::new(
            event_loop.owned_display_handle(),
        )));
        let surface = match instance.create_surface(window.clone()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("cannot create surface: {e}");
                event_loop.exit();
                return;
            }
        };
        let gpu = match Gpu::new(instance, Some(&surface)) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("{e}");
                event_loop.exit();
                return;
            }
        };
        let size = window.inner_size();
        let mut config = surface
            .get_default_config(&gpu.adapter, size.width.max(1), size.height.max(1))
            .expect("surface is supported by the adapter");
        // Prefer an sRGB format so colours match the shader's intent.
        let caps = surface.get_capabilities(&gpu.adapter);
        if let Some(f) = caps.formats.iter().find(|f| f.is_srgb()) {
            config.format = *f;
        }
        config.present_mode =
            if self.settings.vsync { wgpu::PresentMode::AutoVsync } else { wgpu::PresentMode::AutoNoVsync };
        surface.configure(&gpu.device, &config);
        if !self.audio_ready {
            self.audio_ready = true;
            self.audio = crate::sound::Audio::new();
            if let Some(a) = &mut self.audio {
                a.apply(&self.settings, true);
            }
        }
        let egui_ctx = egui::Context::default();
        ui::setup_style(&egui_ctx);
        egui_ctx.set_zoom_factor(self.settings.ui_scale);
        let egui_state = egui_winit::State::new(
            egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            None,
            Some(gpu.device.limits().max_texture_dimension_2d as usize),
        );
        let egui_renderer = egui_wgpu::Renderer::new(&gpu.device, config.format, egui_wgpu::RendererOptions::default());
        self.gfx = Some(Gfx { window, surface, gpu, config, egui_ctx, egui_state, egui_renderer });
        if let Some(s) = self.pending_start.take() {
            self.start_match(s);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(gfx) = &mut self.gfx else { return };
        let response = gfx.egui_state.on_window_event(&gfx.window, &event);
        let ui_wants_pointer = gfx.egui_ctx.egui_wants_pointer_input() || gfx.egui_ctx.is_pointer_over_egui();
        let ui_wants_keys = gfx.egui_ctx.egui_wants_keyboard_input();
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                gfx.config.width = size.width.max(1);
                gfx.config.height = size.height.max(1);
                gfx.surface.configure(&gfx.gpu.device, &gfx.config);
            }
            WindowEvent::Focused(f) => {
                self.focused = f;
                if let Some(a) = &mut self.audio {
                    a.apply(&self.settings, f);
                }
                // Releases are not delivered to an unfocused window.
                self.camera_keys = [false; 4];
            }
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::DroppedFile(path) => {
                if let (None, Some(e)) = (&self.game, &mut self.editor) {
                    e.drop_file(&path);
                }
            }
            WindowEvent::RedrawRequested => self.redraw(event_loop),
            WindowEvent::CursorMoved { position, .. } => self.mouse_moved([position.x as f32, position.y as f32]),
            WindowEvent::CursorLeft { .. } => self.mouse.pos = [-1000.0, -1000.0],
            WindowEvent::MouseInput { state, button, .. } => {
                // Releases always go to the game so drags finish cleanly.
                if !(response.consumed || ui_wants_pointer) || state == ElementState::Released {
                    self.mouse_button(button, state);
                }
            }
            WindowEvent::MouseWheel { delta, .. } if !ui_wants_pointer => {
                let vp = self.viewport();
                let steps = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32 / 60.0,
                };
                let cam = match (&mut self.game, &mut self.editor) {
                    (Some(g), _) => Some((g.map_size(), &mut g.camera)),
                    (None, Some(e)) => Some((e.map_size(), &mut e.camera)),
                    _ => None,
                };
                if let Some((size, cam)) = cam {
                    // Stop zooming out where the map still fills the screen,
                    // so the point under the cursor stays put at the limit.
                    let min = render::Camera::min_scale_for(size, vp);
                    let target = (cam.scale * 1.15f32.powf(steps * self.settings.zoom_speed)).max(min);
                    cam.zoom_at(target / cam.scale, self.mouse.pos, vp);
                    cam.clamp_to(size, vp);
                }
            }
            WindowEvent::KeyboardInput { event, .. } if self.rebinding.is_some() => {
                if let (PhysicalKey::Code(code), ElementState::Pressed) = (event.physical_key, event.state) {
                    let a = self.rebinding.take().expect("checked");
                    if code != KeyCode::Escape && crate::settings::bindable(code) {
                        self.settings.bind(a, code);
                    }
                }
            }
            WindowEvent::KeyboardInput { event, .. }
                if matches!(event.physical_key, PhysicalKey::Code(c) if self.settings.action(c).is_some_and(|a| a >= Action::CameraLeft && a <= Action::CameraDown))
                    && (!ui_wants_keys || event.state == ElementState::Released) =>
            {
                if let PhysicalKey::Code(code) = event.physical_key {
                    let k = self.settings.action(code).expect("checked") as usize - Action::CameraLeft as usize;
                    self.camera_keys[k] = event.state == ElementState::Pressed;
                }
            }
            WindowEvent::KeyboardInput { event, .. }
                if (!ui_wants_keys || event.physical_key == PhysicalKey::Code(KeyCode::Escape))
                    && event.state == ElementState::Pressed
                    && !event.repeat =>
            {
                if let PhysicalKey::Code(code) = event.physical_key {
                    self.handle_key(code, event_loop);
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.settings.fps_limit > 0 {
            let next = self.frame_start + std::time::Duration::from_secs_f64(1.0 / self.settings.fps_limit as f64);
            if Instant::now() < next {
                event_loop.set_control_flow(ControlFlow::WaitUntil(next));
                return;
            }
        }
        event_loop.set_control_flow(ControlFlow::Poll);
        if let Some(gfx) = &self.gfx {
            gfx.window.request_redraw();
        }
    }
}
