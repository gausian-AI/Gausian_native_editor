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
var p010_y_plane: texture_2d<f32>;
@group(0) @binding(1)
var p010_uv_plane: texture_2d<f32>;
@group(0) @binding(2)
var p010_sampler: sampler;

fn convert_component(raw_value: f32) -> f32 {
    // Raw normalized value -> 16-bit integer -> 10-bit sample
    let value16 = clamp(raw_value * 65535.0, 0.0, 65535.0);
    let value10 = floor(value16 / 64.0);
    return clamp(value10 / 1023.0, 0.0, 1.0);
}

@fragment
fn fs_p010(input: VertexOutput) -> @location(0) vec4<f32> {
    let y_sample = textureSampleLevel(p010_y_plane, p010_sampler, input.tex_coords, 0.0).r;
    let uv_sample = textureSampleLevel(p010_uv_plane, p010_sampler, input.tex_coords, 0.0).rg;

    let y10 = convert_component(y_sample);
    let u10 = convert_component(uv_sample.x);
    let v10 = convert_component(uv_sample.y);

    let y = clamp((y10 - (64.0 / 1023.0)) * (1023.0 / 876.0), 0.0, 1.0);
    let u = (u10 - (512.0 / 1023.0)) * (1023.0 / 896.0);
    let v = (v10 - (512.0 / 1023.0)) * (1023.0 / 896.0);

    let r = clamp(y + 1.5748 * v, 0.0, 1.0);
    let g = clamp(y - 0.1873 * u - 0.4681 * v, 0.0, 1.0);
    let b = clamp(y + 1.8556 * u, 0.0, 1.0);

    return vec4<f32>(r, g, b, 1.0);
}
