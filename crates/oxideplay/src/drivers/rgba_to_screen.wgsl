// GPU-resident RGBA video texture -> output surface, preserving aspect ratio.

struct Uniforms {
    content_scale: vec4<f32>,
}

@group(0) @binding(0) var rgba_tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
@group(0) @binding(2) var<uniform> uni: Uniforms;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) viewport: vec2<f32>,
}

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VsOut {
    let x = f32((i << 1u) & 2u);
    let y = f32(i & 2u);
    var o: VsOut;
    o.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    o.viewport = vec2<f32>(x, y);
    return o;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    let uv = (in.viewport - uni.content_scale.zw) * uni.content_scale.xy;
    if (uv.x < 0.0 || uv.x > 1.0 || uv.y < 0.0 || uv.y > 1.0) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }
    return textureSample(rgba_tex, samp, uv);
}
