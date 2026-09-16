@group(0) @binding(0) var previous_frame: texture_2d<f32>;
@group(0) @binding(1) var previous_sampler: sampler;
@group(1) @binding(0) var mask: texture_2d<f32>;
@group(1) @binding(1) var mask_sampler: sampler;
struct Parameters {
    mode: u32,
    phase: u32,
}
var<push_constant> parameters: Parameters;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex fn vs_main(@builtin(vertex_index) index: u32) -> VertexOutput {
    let uv = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    return VertexOutput(vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0), uv);
}

@fragment fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let old = textureSample(previous_frame, previous_sampler, input.uv);
    let pixel = vec2<i32>(input.uv * vec2<f32>(textureDimensions(mask)));
    let value = round(textureLoad(mask, pixel, 0).a * 255.0);
    let phase = f32(parameters.phase);
    var alpha: f32;
    if parameters.mode == 4u {
        if value < phase { discard; }
        alpha = 1.0;
    } else if parameters.mode == 5u {
        alpha = clamp((value + 255.0 - phase) / 255.0, 0.0, 1.0);
    } else {
        alpha = 1.0 - phase / 255.0;
    }
    return vec4<f32>(old.rgb, alpha);
}
