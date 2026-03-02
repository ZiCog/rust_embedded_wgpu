// Shared triangle shader with a time uniform
struct TimeUbo {
    time: f32,
};
@group(0) @binding(0)
var<uniform> U: TimeUbo;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec3<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> VsOut {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-0.5, -0.5),
        vec2<f32>( 0.5, -0.5),
        vec2<f32>( 0.0,  0.5)
    );

    var colors = array<vec3<f32>, 3>(
        vec3<f32>(1.0, 0.0, 0.0),
        vec3<f32>(0.0, 1.0, 0.0),
        vec3<f32>(0.0, 0.0, 1.0)
    );

    var out: VsOut;
    let p = positions[idx];

    // Optionally use time to animate color slightly; geometry stays static
    let t = U.time;
    let tint = 0.5 + 0.5 * sin(t);

    out.pos = vec4<f32>(p, 0.0, 1.0);
    out.color = colors[idx] * tint;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return vec4<f32>(in.color, 1.0);
}
