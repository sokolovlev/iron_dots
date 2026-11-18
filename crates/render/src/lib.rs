//! Renderer for Iron Dots (spec 6.1), on wgpu.
//!
//! * Map: one fullscreen pass. Terrain and owner planes live in two `R8Uint`
//!   textures; the fragment shader colours tiles, tints territory and draws
//!   borders. Only dirty 64×64 chunks are re-uploaded.
//! * Markers: units and cities as instanced quads, one draw call. An
//!   instance is 16 bytes (position, packed flags, packed bars).
//! * Overlay: coloured triangles in tile space for paths and selection.
//!
//! The renderer never touches simulation state directly: the client builds
//! [`Marker`]s from a snapshot, so floats here never feed back into the sim.

use bytemuck::{Pod, Zeroable};
use sim_core::map::{CHUNK, CHUNK_TILES, Map};

pub mod palette;

/// Camera: what tile is at the centre of the screen and how big a tile is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    pub center: [f32; 2],
    /// Pixels per tile.
    pub scale: f32,
}

impl Camera {
    pub const MIN_SCALE: f32 = 0.5;
    pub const MAX_SCALE: f32 = 96.0;

    pub fn screen_to_world(&self, px: [f32; 2], viewport: [f32; 2]) -> [f32; 2] {
        [
            self.center[0] + (px[0] - viewport[0] * 0.5) / self.scale,
            self.center[1] + (px[1] - viewport[1] * 0.5) / self.scale,
        ]
    }

    pub fn world_to_screen(&self, w: [f32; 2], viewport: [f32; 2]) -> [f32; 2] {
        [
            (w[0] - self.center[0]) * self.scale + viewport[0] * 0.5,
            (w[1] - self.center[1]) * self.scale + viewport[1] * 0.5,
        ]
    }

    /// Zooms by `factor` keeping the world point under `px` fixed.
    pub fn zoom_at(&mut self, factor: f32, px: [f32; 2], viewport: [f32; 2]) {
        let before = self.screen_to_world(px, viewport);
        self.scale = (self.scale * factor).clamp(Self::MIN_SCALE, Self::MAX_SCALE);
        let after = self.screen_to_world(px, viewport);
        self.center[0] += before[0] - after[0];
        self.center[1] += before[1] - after[1];
    }
}

/// Marker shapes.
pub mod shape {
    pub const UNIT: u32 = 0;
    pub const CITY: u32 = 1;
    pub const CAPITAL: u32 = 2;
}

/// Marker flags (low byte of [`Marker::info`] holds owner and flags).
pub mod mflags {
    pub const HEAVY: u32 = 1 << 4;
    pub const SHIP: u32 = 1 << 5;
    pub const SELECTED: u32 = 1 << 6;
    pub const IN_COMBAT: u32 = 1 << 7;
}

/// One unit or city on screen. 16 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct Marker {
    pub pos: [f32; 2],
    /// Bits 0-3 owner, 4-7 [`mflags`], 8-9 [`shape`].
    pub info: u32,
    /// Bits 0-7 HP, 8-15 morale, 16-23 capture progress (0..=255 each).
    pub stats: u32,
}

impl Marker {
    pub fn unit(pos: [f32; 2], owner: u8, flags: u32, hp: f32, morale: f32) -> Marker {
        Marker {
            pos,
            info: (owner as u32 & 0xF) | (flags & 0xF0) | (shape::UNIT << 8),
            stats: unit_byte(hp) | (unit_byte(morale) << 8),
        }
    }

    pub fn city(pos: [f32; 2], owner: u8, capital: bool, progress: f32) -> Marker {
        let s = if capital { shape::CAPITAL } else { shape::CITY };
        Marker { pos, info: (owner as u32 & 0xF) | (s << 8), stats: unit_byte(progress) << 16 }
    }
}

fn unit_byte(v: f32) -> u32 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u32
}

/// Vertex of the overlay (tile space).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct OverlayVertex {
    pub pos: [f32; 2],
    pub color: [f32; 4],
}

/// Helper to build overlay geometry.
#[derive(Default)]
pub struct Overlay {
    pub vertices: Vec<OverlayVertex>,
}

impl Overlay {
    pub fn clear(&mut self) {
        self.vertices.clear();
    }

    /// A line segment `width` tiles wide.
    pub fn line(&mut self, a: [f32; 2], b: [f32; 2], width: f32, color: [f32; 4]) {
        let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
        let len = (dx * dx + dy * dy).sqrt();
        if len < 1e-6 {
            return;
        }
        let (nx, ny) = (-dy / len * width * 0.5, dx / len * width * 0.5);
        let p = [[a[0] + nx, a[1] + ny], [b[0] + nx, b[1] + ny], [b[0] - nx, b[1] - ny], [a[0] - nx, a[1] - ny]];
        for k in [0, 1, 2, 0, 2, 3] {
            self.vertices.push(OverlayVertex { pos: p[k], color });
        }
    }

    pub fn polyline(&mut self, pts: &[[f32; 2]], width: f32, color: [f32; 4]) {
        for w in pts.windows(2) {
            self.line(w[0], w[1], width, color);
        }
    }

    /// Outline of an axis-aligned rectangle.
    pub fn rect(&mut self, a: [f32; 2], b: [f32; 2], width: f32, color: [f32; 4]) {
        let (c, d) = ([b[0], a[1]], [a[0], b[1]]);
        self.polyline(&[a, c, b, d, a], width, color);
    }

    pub fn fill_rect(&mut self, a: [f32; 2], b: [f32; 2], color: [f32; 4]) {
        let p = [a, [b[0], a[1]], b, [a[0], b[1]]];
        for k in [0, 1, 2, 0, 2, 3] {
            self.vertices.push(OverlayVertex { pos: p[k], color });
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct CameraUniform {
    center: [f32; 2],
    viewport: [f32; 2],
    map_size: [f32; 2],
    scale: f32,
    time: f32,
    colors: [[f32; 4]; 9],
}

pub struct Renderer {
    map_w: u32,
    map_h: u32,
    terrain_tex: wgpu::Texture,
    owner_tex: wgpu::Texture,
    camera_buf: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    map_pipeline: wgpu::RenderPipeline,
    marker_pipeline: wgpu::RenderPipeline,
    overlay_pipeline: wgpu::RenderPipeline,
    marker_buf: wgpu::Buffer,
    marker_cap: usize,
    marker_count: u32,
    overlay_buf: wgpu::Buffer,
    overlay_cap: usize,
    overlay_count: u32,
    /// Player colours (index 0 = neutral). Change with [`Renderer::set_colors`].
    colors: [[f32; 4]; 9],
}

fn r8_texture(device: &wgpu::Device, w: u32, h: u32, label: &str) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R8Uint,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn alpha_blend_target(format: wgpu::TextureFormat) -> [Option<wgpu::ColorTargetState>; 1] {
    [Some(wgpu::ColorTargetState {
        format,
        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
        write_mask: wgpu::ColorWrites::ALL,
    })]
}

impl Renderer {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, format: wgpu::TextureFormat, map: &Map) -> Renderer {
        let (map_w, map_h) = (map.width(), map.height());
        let terrain_tex = r8_texture(device, map_w, map_h, "terrain");
        let owner_tex = r8_texture(device, map_w, map_h, "owner");
        let camera_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("camera"),
            size: std::mem::size_of::<CameraUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Uint,
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("scene"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                tex_entry(1),
                tex_entry(2),
            ],
        });
        let tv = |t: &wgpu::Texture| t.create_view(&wgpu::TextureViewDescriptor::default());
        let (terrain_view, owner_view) = (tv(&terrain_tex), tv(&owner_tex));
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("scene"),
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: camera_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&terrain_view) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&owner_view) },
            ],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("iron-dots"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders.wgsl").into()),
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("scene"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = |label: &str, vs: &str, fs: &str, buffers: &[Option<wgpu::VertexBufferLayout>], blend: bool| {
            let targets = if blend {
                alpha_blend_target(format)
            } else {
                [Some(wgpu::ColorTargetState { format, blend: None, write_mask: wgpu::ColorWrites::ALL })]
            };
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&pl),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some(vs),
                    compilation_options: Default::default(),
                    buffers,
                },
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    targets: &targets,
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let map_pipeline = pipeline("map", "vs_full", "fs_map", &[], false);
        let marker_attrs = wgpu::vertex_attr_array![0 => Float32x2, 1 => Uint32, 2 => Uint32];
        let marker_pipeline = pipeline(
            "markers",
            "vs_marker",
            "fs_marker",
            &[Some(wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<Marker>() as u64,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &marker_attrs,
            })],
            true,
        );
        let overlay_attrs = wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4];
        let overlay_pipeline = pipeline(
            "overlay",
            "vs_overlay",
            "fs_overlay",
            &[Some(wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<OverlayVertex>() as u64,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &overlay_attrs,
            })],
            true,
        );
        let marker_buf = Self::vertex_buffer(device, "markers", 1024 * 16);
        let overlay_buf = Self::vertex_buffer(device, "overlay", 4096 * 24);
        let r = Renderer {
            map_w,
            map_h,
            terrain_tex,
            owner_tex,
            camera_buf,
            bind_group,
            map_pipeline,
            marker_pipeline,
            overlay_pipeline,
            marker_buf,
            marker_cap: 1024,
            marker_count: 0,
            overlay_buf,
            overlay_cap: 4096,
            overlay_count: 0,
            colors: palette::DEFAULT,
        };
        for cy in 0..map.chunks_h() {
            for cx in 0..map.chunks_w() {
                r.upload_chunk(queue, map, cx, cy);
            }
        }
        r
    }

    fn vertex_buffer(device: &wgpu::Device, label: &str, bytes: usize) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: bytes as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    pub fn set_colors(&mut self, colors: [[f32; 4]; 9]) {
        self.colors = colors;
    }

    /// Uploads terrain and owner of one chunk. A chunk plane is stored
    /// row-major inside the map, so it is one contiguous 4 KiB copy.
    pub fn upload_chunk(&self, queue: &wgpu::Queue, map: &Map, cx: u32, cy: u32) {
        let k = (cy * map.chunks_w() + cx) as usize * CHUNK_TILES;
        for (tex, plane) in [(&self.terrain_tex, map.terrain_plane()), (&self.owner_tex, map.owner_plane())] {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x: cx * CHUNK, y: cy * CHUNK, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                &plane[k..k + CHUNK_TILES],
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(CHUNK), rows_per_image: Some(CHUNK) },
                wgpu::Extent3d { width: CHUNK, height: CHUNK, depth_or_array_layers: 1 },
            );
        }
    }

    pub fn set_markers(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, markers: &[Marker]) {
        if markers.len() > self.marker_cap {
            self.marker_cap = markers.len().next_power_of_two();
            self.marker_buf = Self::vertex_buffer(device, "markers", self.marker_cap * std::mem::size_of::<Marker>());
        }
        queue.write_buffer(&self.marker_buf, 0, bytemuck::cast_slice(markers));
        self.marker_count = markers.len() as u32;
    }

    pub fn set_overlay(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, verts: &[OverlayVertex]) {
        if verts.len() > self.overlay_cap {
            self.overlay_cap = verts.len().next_power_of_two();
            self.overlay_buf =
                Self::vertex_buffer(device, "overlay", self.overlay_cap * std::mem::size_of::<OverlayVertex>());
        }
        queue.write_buffer(&self.overlay_buf, 0, bytemuck::cast_slice(verts));
        self.overlay_count = verts.len() as u32;
    }

    /// Writes the camera uniform; call before [`Renderer::draw`].
    pub fn set_camera(&self, queue: &wgpu::Queue, cam: &Camera, viewport: [f32; 2], time: f32) {
        let u = CameraUniform {
            center: cam.center,
            viewport,
            map_size: [self.map_w as f32, self.map_h as f32],
            scale: cam.scale,
            time,
            colors: self.colors,
        };
        queue.write_buffer(&self.camera_buf, 0, bytemuck::bytes_of(&u));
    }

    /// Draws map, markers and overlay into an open render pass.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_pipeline(&self.map_pipeline);
        pass.draw(0..3, 0..1);
        if self.marker_count > 0 {
            pass.set_pipeline(&self.marker_pipeline);
            pass.set_vertex_buffer(0, self.marker_buf.slice(..));
            pass.draw(0..6, 0..self.marker_count);
        }
        if self.overlay_count > 0 {
            pass.set_pipeline(&self.overlay_pipeline);
            pass.set_vertex_buffer(0, self.overlay_buf.slice(..));
            pass.draw(0..self.overlay_count, 0..1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_is_16_bytes() {
        assert_eq!(std::mem::size_of::<Marker>(), 16);
    }

    #[test]
    fn camera_round_trip_and_zoom_keeps_cursor() {
        let mut c = Camera { center: [100.0, 50.0], scale: 8.0 };
        let vp = [800.0, 600.0];
        let w = c.screen_to_world([123.0, 456.0], vp);
        let s = c.world_to_screen(w, vp);
        assert!((s[0] - 123.0).abs() < 1e-3 && (s[1] - 456.0).abs() < 1e-3);
        c.zoom_at(2.0, [123.0, 456.0], vp);
        let w2 = c.screen_to_world([123.0, 456.0], vp);
        assert!((w[0] - w2[0]).abs() < 1e-3 && (w[1] - w2[1]).abs() < 1e-3);
    }
}
