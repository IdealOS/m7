// Instanced rounded rectangles with optional border, anti-aliased with a signed
// distance field. One instance per rect; four vertices per instance as a strip.

struct Globals {
    viewport: vec2<f32>, // physical pixels
    _pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;

struct Instance {
    @location(0) rect: vec4<f32>,         // x, y, w, h (physical px)
    @location(1) color: vec4<f32>,        // linear rgba
    @location(2) border_color: vec4<f32>, // linear rgba
    @location(3) params: vec4<f32>,       // radius, border width, unused, unused
    @location(4) clip: vec4<f32>,         // x, y, w, h (physical px)
};

struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) world: vec2<f32>,
    @location(2) size: vec2<f32>,
    @location(3) color: vec4<f32>,
    @location(4) border_color: vec4<f32>,
    @location(5) params: vec4<f32>,
    @location(6) clip: vec4<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32, inst: Instance) -> VOut {
    // Expand by one pixel so the anti-aliased edge is not cut off.
    let pad = 1.0;
    let uv = vec2<f32>(f32(vi & 1u), f32(vi >> 1u));
    let origin = inst.rect.xy - vec2<f32>(pad);
    let extent = inst.rect.zw + vec2<f32>(2.0 * pad);
    let world = origin + uv * extent;
    let ndc = world / globals.viewport * 2.0 - 1.0;

    var out: VOut;
    out.pos = vec4<f32>(ndc.x, -ndc.y, 0.0, 1.0);
    out.local = world - inst.rect.xy;
    out.world = world;
    out.size = inst.rect.zw;
    out.color = inst.color;
    out.border_color = inst.border_color;
    out.params = inst.params;
    out.clip = inst.clip;
    return out;
}

fn sd_round_box(p: vec2<f32>, half: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - half + vec2<f32>(r);
    return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
}

@fragment
fn fs_main(in: VOut) -> @location(0) vec4<f32> {
    let c = in.clip;
    if (in.world.x < c.x || in.world.y < c.y || in.world.x > c.x + c.z || in.world.y > c.y + c.w) {
        discard;
    }
    let half = in.size * 0.5;
    let radius = min(in.params.x, min(half.x, half.y));
    let border = in.params.y;
    let d = sd_round_box(in.local - half, half, radius);

    let coverage = clamp(0.5 - d, 0.0, 1.0);
    var color = in.color;
    if (border > 0.0) {
        // Inside the border band the fill fades to the border color.
        let inner = clamp(0.5 - (d + border), 0.0, 1.0);
        color = mix(in.border_color, in.color, inner);
    }
    return vec4<f32>(color.rgb, color.a * coverage);
}
