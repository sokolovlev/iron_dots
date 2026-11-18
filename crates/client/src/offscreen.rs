//! Renders a frame of a bot-vs-bot match to a PNG without a window. Used to
//! check the renderer in CI and on machines without a display.

use crate::game::{Game, MatchSettings};
use crate::gpu::Gpu;
use render::{Marker, Overlay, Renderer};

pub struct ShotOptions {
    pub settings: MatchSettings,
    pub ticks: u32,
    pub width: u32,
    pub height: u32,
    /// Pixels per tile; 0 = fit the whole map.
    pub zoom: f32,
    pub hud: bool,
}

pub fn screenshot(opts: &ShotOptions, path: &str) -> Result<String, String> {
    let mut game = Game::new(opts.settings.clone())?;
    game.add_bot_for_me(opts.settings.difficulty);
    for _ in 0..opts.ticks {
        game.step();
    }
    // Select our biggest group so selection rings and paths show up.
    let me = game.me;
    let u = game.world.units();
    game.selection = (0..u.len()).filter(|&i| u.owner[i] == me).take(12).map(|i| u.handle(i)).collect();
    let (w, h) = (opts.width, opts.height);
    let vp = [w as f32, h as f32];
    let map = game.world.map();
    if opts.zoom > 0.0 {
        // Centre on our units (or capital).
        let idx: Vec<usize> = (0..u.len()).filter(|&i| u.owner[i] == me).collect();
        if idx.is_empty() {
            if let Some(c) = game.capital_center() {
                game.camera.center = c;
            }
        } else {
            let n = idx.len() as f32;
            let s = idx.iter().map(|&i| game.unit_pos(i)).fold([0.0, 0.0], |a, p| [a[0] + p[0], a[1] + p[1]]);
            game.camera.center = [s[0] / n, s[1] / n];
        }
        game.camera.scale = opts.zoom;
    } else {
        game.camera.center = [map.width() as f32 / 2.0, map.height() as f32 / 2.0];
        game.camera.scale = (vp[0] / map.width() as f32).min(vp[1] / map.height() as f32);
    }

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let gpu = Gpu::new(instance, None)?;
    let info = gpu.adapter.get_info();
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let target = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("screenshot"),
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let mut r = Renderer::new(&gpu.device, &gpu.queue, format, game.world.map());
    let mut markers: Vec<Marker> = Vec::new();
    let mut overlay = Overlay::default();
    game.markers(&mut markers);
    game.overlay(&mut overlay, vp);
    r.set_markers(&gpu.device, &gpu.queue, &markers);
    r.set_overlay(&gpu.device, &gpu.queue, &overlay.vertices);
    r.set_camera(&gpu.queue, &game.camera, vp, game.time);

    // HUD through egui, exactly as in the window.
    let ctx = egui::Context::default();
    crate::ui::setup_style(&ctx);
    let mut egui_r = egui_wgpu::Renderer::new(&gpu.device, format, egui_wgpu::RendererOptions::default());
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(vp[0], vp[1]))),
        ..Default::default()
    };
    let mut esc = false;
    // Several passes with advancing time: egui measures text on the first
    // pass and fades windows in over a few frames.
    let mut deltas_all = Vec::new();
    let mut out = None;
    for pass in 0..8 {
        let mut input = raw.clone();
        input.time = Some(pass as f64 * 0.1);
        let mut o = ctx.run_ui(input, |ui| {
            let c = ui.ctx().clone();
            if opts.hud {
                let _ = crate::ui::hud(&c, &mut game, &mut esc);
            }
        });
        deltas_all.push(std::mem::take(&mut o.textures_delta));
        out = Some(o);
    }
    let out = out.expect("at least one pass");
    let paint = ctx.tessellate(out.shapes, 1.0);
    for delta in &deltas_all {
        for (id, deltas) in &delta.set {
            for d in deltas {
                egui_r.update_texture(&gpu.device, &gpu.queue, *id, d);
            }
        }
    }
    let screen = egui_wgpu::ScreenDescriptor { size_in_pixels: [w, h], pixels_per_point: 1.0 };

    let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("shot") });
    egui_r.update_buffers(&gpu.device, &gpu.queue, &mut enc, &paint, &screen);
    {
        let pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("shot"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.1, g: 0.11, b: 0.13, a: 1.0 }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        let mut pass = pass.forget_lifetime();
        r.draw(&mut pass);
        egui_r.render(&mut pass, &paint, &screen);
    }
    let bpr = (w * 4).div_ceil(256) * 256;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (bpr * h) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buf,
            layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(bpr), rows_per_image: Some(h) },
        },
        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );
    gpu.queue.submit([enc.finish()]);
    let slice = buf.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).map_err(|e| e.to_string())?;
    let data = slice.get_mapped_range().map_err(|e| format!("cannot read back the frame: {e:?}"))?;
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for row in 0..h {
        let start = (row * bpr) as usize;
        rgba.extend_from_slice(&data[start..start + (w * 4) as usize]);
    }
    drop(data);
    buf.unmap();

    let file = std::fs::File::create(path).map_err(|e| format!("{path}: {e}"))?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
    let mut writer = enc.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(&rgba).map_err(|e| e.to_string())?;
    Ok(format!("{} ({:?})", info.name, info.backend))
}
