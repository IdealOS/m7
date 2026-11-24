// Textured quads for images, with rounded corners and the same clipping as quads.
// One instance per image; four vertices per instance as a strip.

struct Globals {
    viewport: vec2<f32>, // physical pixels
    _pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;

struct Instance {
    @location(0) rect: vec4<f32>,   // x, y, w, h (physical px)
    @location(1) params: vec4<f32>, // radius, unused...
    @location(2) clip: vec4<f32>,   // x, y, w, h (physical px)
};

struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) world: vec2<f32>,
    @location(2) size: vec2<f32>,
    @location(3) params: vec4<f32>,
    @location(4) clip: vec4<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32, inst: Instance) -> VOut {
    let uv = vec2<f32>(f32(vi & 1u), f32(vi >> 1u));
    let world = inst.rect.xy + uv * inst.rect.zw;
    let ndc = world / globals.viewport * 2.0 - 1.0;
    var out: VOut;
    out.pos = vec4<f32>(ndc.x, -ndc.y, 0.0, 1.0);
    out.local = world - inst.rect.xy;
    out.world = world;
    out.size = inst.rect.zw;
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
    let d = sd_round_box(in.local - half, half, radius);
    let coverage = clamp(0.5 - d, 0.0, 1.0);
    let color = textureSample(tex, samp, in.local / in.size);
    return vec4<f32>(color.rgb, color.a * coverage);
}
