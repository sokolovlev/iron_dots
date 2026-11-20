// Iron Dots shaders. Positions are in tiles; the camera maps tiles to pixels.

struct Camera {
    center: vec2<f32>,
    viewport: vec2<f32>,
    map_size: vec2<f32>,
    scale: f32,      // pixels per tile
    time: f32,       // seconds, for combat jitter
    colors: array<vec4<f32>, 9>, // player colours, index 0 = neutral
};

@group(0) @binding(0) var<uniform> cam: Camera;
@group(0) @binding(1) var terrain_tex: texture_2d<u32>;
@group(0) @binding(2) var owner_tex: texture_2d<u32>;

// Colours below are authored in sRGB; render targets are sRGB, which
// expects linear values, so convert on output.
fn lin(c: vec3<f32>) -> vec3<f32> {
    return pow(max(c, vec3<f32>(0.0)), vec3<f32>(2.2));
}

fn to_clip(p: vec2<f32>) -> vec4<f32> {
    let px = (p - cam.center) * cam.scale;
    let ndc = px / (cam.viewport * 0.5);
    return vec4<f32>(ndc.x, -ndc.y, 0.0, 1.0);
}

// ---------------------------------------------------------------- map

@vertex
fn vs_full(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(uv * 2.0 - 1.0, 0.0, 1.0);
}

fn terrain_color(t: u32) -> vec3<f32> {
    switch t {
        case 0u: { return vec3<f32>(0.80, 0.82, 0.68); }  // plain
        case 1u: { return vec3<f32>(0.47, 0.60, 0.42); }  // forest
        case 2u: { return vec3<f32>(0.71, 0.67, 0.52); }  // hills
        case 3u: { return vec3<f32>(0.47, 0.45, 0.44); }  // mountain
        case 4u: { return vec3<f32>(0.45, 0.63, 0.78); }  // water
        case 5u: { return vec3<f32>(0.89, 0.83, 0.63); }  // sand
        case 6u: { return vec3<f32>(0.94, 0.95, 0.97); }  // snow
        case 7u: { return vec3<f32>(0.60, 0.52, 0.42); }  // mud
        case 8u: { return vec3<f32>(0.80, 0.82, 0.68); }  // city (marker drawn on top)
        default: { return vec3<f32>(0.62, 0.52, 0.40); }  // bridge
    }
}

fn hash2(p: vec2<i32>) -> f32 {
    var h = u32(p.x) * 374761393u + u32(p.y) * 668265263u;
    h = (h ^ (h >> 13u)) * 1274126177u;
    return f32(h & 0xffffu) / 65535.0;
}

fn owner_at(t: vec2<i32>) -> u32 {
    let size = vec2<i32>(cam.map_size);
    if (t.x < 0 || t.y < 0 || t.x >= size.x || t.y >= size.y) {
        return 255u;
    }
    return textureLoad(owner_tex, t, 0).r;
}

@fragment
fn fs_map(@builtin(position) frag: vec4<f32>) -> @location(0) vec4<f32> {
    let world = cam.center + (frag.xy - cam.viewport * 0.5) / cam.scale;
    if (world.x < 0.0 || world.y < 0.0 || world.x >= cam.map_size.x || world.y >= cam.map_size.y) {
        return vec4<f32>(lin(vec3<f32>(0.13, 0.14, 0.16)), 1.0);
    }
    let tile = vec2<i32>(floor(world));
    let t = textureLoad(terrain_tex, tile, 0).r;
    let o = textureLoad(owner_tex, tile, 0).r;
    // Slight per-tile variation so large areas don't look flat.
    var col = terrain_color(t) * (0.96 + 0.06 * hash2(tile));
    if (t == 3u) {
        col = col * (0.85 + 0.3 * hash2(tile + vec2<i32>(7, 3)));
    }
    if (o != 0u) {
        col = mix(col, cam.colors[o].rgb, 0.30);
    }
    // Borders: a line on our side of every edge whose neighbour has another owner.
    let f = fract(world);
    let w = max(1.2 / cam.scale, 0.04);
    var border = false;
    if (o != 0u) {
        if (f.x < w && owner_at(tile + vec2<i32>(-1, 0)) != o) { border = true; }
        if (f.x > 1.0 - w && owner_at(tile + vec2<i32>(1, 0)) != o) { border = true; }
        if (f.y < w && owner_at(tile + vec2<i32>(0, -1)) != o) { border = true; }
        if (f.y > 1.0 - w && owner_at(tile + vec2<i32>(0, 1)) != o) { border = true; }
    }
    if (border) {
        col = cam.colors[o].rgb * 0.75;
    } else if (cam.scale > 24.0) {
        // Faint tile grid when zoomed in.
        let g = min(min(f.x, 1.0 - f.x), min(f.y, 1.0 - f.y));
        col = col * (0.97 + 0.03 * smoothstep(0.0, 1.0 / cam.scale, g));
    }
    return vec4<f32>(lin(col), 1.0);
}

// ---------------------------------------------------------------- markers

struct Marker {
    @location(0) pos: vec2<f32>,
    // bits 0-3 owner, 4 heavy, 5 ship, 6 selected, 7 in combat,
    // 8-9 shape (0 unit, 1 city, 2 capital)
    @location(1) info: u32,
    // bits 0-7 HP, 8-15 morale, 16-23 capture progress (all 0..255),
    // 24-31 fixed radius in screen pixels (density markers; 0 = normal)
    @location(2) stats: u32,
};

struct MarkerOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) @interpolate(flat) info: u32,
    @location(2) @interpolate(flat) stats: u32,
    @location(3) px_per_unit: f32,
};

const QUAD = array<vec2<f32>, 6>(
    vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
    vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, 1.0),
);

@vertex
fn vs_marker(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32, m: Marker) -> MarkerOut {
    let shape = (m.info >> 8u) & 3u;
    var radius = max(0.45, 4.0 / cam.scale);
    if (shape != 0u) {
        radius = max(0.75, 7.0 / cam.scale);
    }
    let fixed_px = f32(m.stats >> 24u);
    if (fixed_px > 0.0) {
        radius = fixed_px / cam.scale;
    }
    var pos = m.pos;
    if ((m.info & 0x80u) != 0u && shape == 0u) {
        // Units in combat tremble.
        let ph = cam.time * 37.0 + f32(ii) * 1.7;
        pos += vec2<f32>(sin(ph), cos(ph * 1.3)) * 0.05;
    }
    let extent = 1.8; // room for selection ring and bars
    let q = QUAD[vi] * extent;
    var out: MarkerOut;
    out.clip = to_clip(pos + q * radius);
    out.local = q;
    out.info = m.info;
    out.stats = m.stats;
    out.px_per_unit = radius * cam.scale;
    return out;
}

fn star(p: vec2<f32>) -> f32 {
    // Signed distance-ish to a five-pointed star of outer radius 1.
    let a = atan2(p.x, -p.y);
    let seg = 6.2831853 / 5.0;
    let k = abs(fract(a / seg + 0.5) - 0.5) * seg;
    let r = length(p);
    let edge = mix(0.48, 1.0, pow(1.0 - k / (seg * 0.5), 2.0));
    return r - edge;
}

@fragment
fn fs_marker(in: MarkerOut) -> @location(0) vec4<f32> {
    let p = in.local;
    let aa = 1.5 / max(in.px_per_unit, 1.0);
    let owner = in.info & 0xfu;
    let shape = (in.info >> 8u) & 3u;
    let hp = f32(in.stats & 0xffu) / 255.0;
    let morale = f32((in.stats >> 8u) & 0xffu) / 255.0;
    let team = cam.colors[owner].rgb;
    var col = vec4<f32>(0.0);

    if (shape == 0u) {
        let ship = (in.info & 0x20u) != 0u;
        var d = length(p) - 1.0;
        if (ship) {
            d = (abs(p.x) + abs(p.y)) * 0.8 - 1.0;
        }
        // Body: darker when wounded, dark outline.
        let fill = team * (0.55 + 0.45 * hp);
        let body = 1.0 - smoothstep(-aa, aa, d);
        let outline = smoothstep(-0.28 - aa, -0.28 + aa, d);
        col = vec4<f32>(mix(fill, team * 0.35, outline), body);
        if ((in.info & 0x10u) != 0u && !ship) {
            // Heavy: a ring inside the dot.
            let r = abs(length(p) - 0.48);
            let ring = 1.0 - smoothstep(0.12 - aa, 0.12 + aa, r);
            col = vec4<f32>(mix(col.rgb, team * 0.3, ring), col.a);
        }
        if ((in.info & 0x40u) != 0u) {
            let r = abs(length(p) - 1.3);
            let sel = 1.0 - smoothstep(0.1 - aa, 0.1 + aa, r);
            col = mix(col, vec4<f32>(1.0, 1.0, 1.0, 1.0), sel);
        }
        // HP and morale bars above the dot when zoomed in enough.
        if (in.px_per_unit > 4.0 && (in.stats >> 24u) == 0u && p.x > -1.0 && p.x < 1.0) {
            let x = (p.x + 1.0) * 0.5;
            if (p.y > -1.75 && p.y < -1.5) {
                let c = select(vec3<f32>(0.2, 0.2, 0.2), vec3<f32>(0.25, 0.8, 0.3), x < hp);
                col = vec4<f32>(c, 1.0);
            } else if (p.y >= -1.5 && p.y < -1.3) {
                let c = select(vec3<f32>(0.2, 0.2, 0.2), vec3<f32>(0.95, 0.85, 0.35), x < morale);
                col = vec4<f32>(c, 1.0);
            }
        }
    } else {
        // City: light badge with an owner-coloured ring; capital: star.
        var d = length(p) - 1.0;
        if (shape == 2u) {
            d = star(p);
        }
        let body = 1.0 - smoothstep(-aa, aa, d);
        let ring_col = select(vec3<f32>(0.45, 0.45, 0.45), team, owner != 0u);
        let ring = smoothstep(-0.3 - aa, -0.3 + aa, d);
        let inner = vec3<f32>(0.97, 0.90, 0.55);
        col = vec4<f32>(mix(inner, ring_col, ring), body);
        let progress = f32((in.stats >> 16u) & 0xffu) / 255.0;
        if (progress > 0.0 && p.y > 1.25 && p.y < 1.5 && p.x > -1.0 && p.x < 1.0) {
            let x = (p.x + 1.0) * 0.5;
            col = vec4<f32>(select(vec3<f32>(0.15, 0.15, 0.15), vec3<f32>(1.0, 1.0, 1.0), x < progress), 1.0);
        }
    }
    if (col.a <= 0.001) {
        discard;
    }
    return vec4<f32>(lin(col.rgb), col.a);
}

// ---------------------------------------------------------------- overlay

struct LineIn {
    @location(0) pos: vec2<f32>,
    @location(1) color: vec4<f32>,
};

struct LineOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_overlay(v: LineIn) -> LineOut {
    var out: LineOut;
    out.clip = to_clip(v.pos);
    out.color = v.color;
    return out;
}

@fragment
fn fs_overlay(v: LineOut) -> @location(0) vec4<f32> {
    return vec4<f32>(lin(v.color.rgb), v.color.a);
}
