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
var nv12_y_plane: texture_2d<f32>;
@group(0) @binding(1)
var nv12_uv_plane: texture_2d<f32>;
@group(0) @binding(2)
var nv12_sampler: sampler;

@fragment
fn fs_nv12(input: VertexOutput) -> @location(0) vec4<f32> {
    let y_sample = textureSample(nv12_y_plane, nv12_sampler, input.tex_coords).r * 255.0;
    let uv_sample = textureSample(nv12_uv_plane, nv12_sampler, input.tex_coords).rg * 255.0;

    let y = clamp((y_sample - 16.0) * (1.0 / 219.0), 0.0, 1.0);
    let u = (uv_sample.x - 128.0) * (1.0 / 224.0);
    let v = (uv_sample.y - 128.0) * (1.0 / 224.0);

    let r = clamp(y + 1.5748 * v, 0.0, 1.0);
    let g = clamp(y - 0.1873 * u - 0.4681 * v, 0.0, 1.0);
    let b = clamp(y + 1.8556 * u, 0.0, 1.0);

    return vec4<f32>(r, g, b, 1.0);
}
