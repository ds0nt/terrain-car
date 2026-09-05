// Subtle per-pixel procedural detail layered on top of terrain.rs's
// per-vertex biome color (see terrain_material.rs's module docs). Two cheap
// value-noise taps: one for a fine albedo grain, reused as an implicit
// heightfield to fake a bump-mapped normal — no texture samples, no loops,
// a handful of scalar hashes per fragment.

#import bevy_pbr::{
    pbr_fragment::pbr_input_from_standard_material,
    pbr_functions::{alpha_discard, apply_pbr_lighting, main_pass_post_lighting_processing},
    forward_io::{VertexOutput, FragmentOutput},
}

struct TerrainDetailExtension {
    macro_freq: f32,
    micro_freq: f32,
    color_detail_strength: f32,
    normal_strength: f32,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(100)
var<uniform> terrain_detail: TerrainDetailExtension;

fn hash21(p: vec2<f32>) -> f32 {
    var p3 = fract(vec3<f32>(p.x, p.y, p.x) * vec3<f32>(0.1031, 0.1030, 0.0973));
    p3 = p3 + dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

fn value_noise(p: vec2<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let a = hash21(i);
    let b = hash21(i + vec2<f32>(1.0, 0.0));
    let c = hash21(i + vec2<f32>(0.0, 1.0));
    let d = hash21(i + vec2<f32>(1.0, 1.0));
    let u = f * f * (3.0 - 2.0 * f);
    return mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
}

@fragment
fn fragment(in: VertexOutput, @builtin(front_facing) is_front: bool) -> FragmentOutput {
    var pbr_input = pbr_input_from_standard_material(in, is_front);

    let wp = in.world_position.xz;

    // Albedo: a broad "clumpy" layer plus fine grain, blended and remapped
    // to [-1, 1] so it can brighten or darken symmetrically around the
    // vertex-baked biome color rather than only ever washing it out.
    let macro_n = value_noise(wp * terrain_detail.macro_freq);
    let micro_n = value_noise(wp * terrain_detail.micro_freq);
    let detail = (macro_n * 0.6 + micro_n * 0.4) * 2.0 - 1.0;
    let albedo_factor = 1.0 + detail * terrain_detail.color_detail_strength;
    pbr_input.material.base_color = vec4<f32>(
        pbr_input.material.base_color.rgb * albedo_factor,
        pbr_input.material.base_color.a,
    );

    // Fake bump normal: treat `micro_n` as a heightfield sampled around
    // `wp` and turn its slope into a normal perturbation. `dHdx`/`dHdz` are
    // world-space (not screen-space) partial derivatives, so the result is
    // resolution-independent and free of screen-space-derivative aliasing.
    // Projecting the world X/Z axes onto the tangent plane of the *real*
    // (possibly sloped) normal before applying the offset keeps this
    // correct on mountainsides and canyon walls, not just flat ground.
    let eps = 0.35;
    let h_l = value_noise((wp - vec2<f32>(eps, 0.0)) * terrain_detail.micro_freq);
    let h_r = value_noise((wp + vec2<f32>(eps, 0.0)) * terrain_detail.micro_freq);
    let h_d = value_noise((wp - vec2<f32>(0.0, eps)) * terrain_detail.micro_freq);
    let h_u = value_noise((wp + vec2<f32>(0.0, eps)) * terrain_detail.micro_freq);
    let d_h_dx = (h_r - h_l) / (2.0 * eps);
    let d_h_dz = (h_u - h_d) / (2.0 * eps);

    let n = pbr_input.N;
    let tangent_x = normalize(vec3<f32>(1.0, 0.0, 0.0) - n * n.x);
    let tangent_z = normalize(vec3<f32>(0.0, 0.0, 1.0) - n * n.z);
    pbr_input.N = normalize(
        n - (tangent_x * d_h_dx + tangent_z * d_h_dz) * terrain_detail.normal_strength,
    );

    pbr_input.material.base_color = alpha_discard(pbr_input.material, pbr_input.material.base_color);

    var out: FragmentOutput;
    out.color = apply_pbr_lighting(pbr_input);
    out.color = main_pass_post_lighting_processing(pbr_input, out.color);
    return out;
}
