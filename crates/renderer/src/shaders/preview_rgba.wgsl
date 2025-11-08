struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) tex_coords: vec2<f32>,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) tex_coords: vec2<f32>,
}

@vertex
fn vs_fullscreen(input: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip_position = vec4<f32>(input.position, 0.0, 1.0);
    out.tex_coords = input.tex_coords;
    return out;
}

@group(0) @binding(0)
var rgba_texture: texture_2d<f32>;
@group(0) @binding(1)
var rgba_sampler: sampler;

@fragment
fn fs_rgba(input: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(rgba_texture, rgba_sampler, input.tex_coords);
}
