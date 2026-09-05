use bevy::pbr::{ExtendedMaterial, MaterialExtension};
use bevy::prelude::*;
use bevy::render::render_resource::AsBindGroup;
use bevy::shader::ShaderRef;

/// Terrain's actual material type: `StandardMaterial` (unchanged PBR
/// lighting, vertex-color biome tint from `terrain.rs`'s `build_chunk_mesh`)
/// extended with a fragment shader that adds cheap per-pixel procedural
/// detail on top — see `shaders/terrain_detail.wgsl`. Vertex colors alone
/// only vary once per mesh vertex (one every few meters), so up close every
/// biome reads as a flat, slightly-blurry color swatch; this gives each
/// "planet type" a textured look without ever loading a texture asset,
/// keeping every chunk's identical shape (`shared::terrain_gen` stays the
/// only source of truth for geometry/color) with a single shared material
/// instance for the whole terrain.
pub type TerrainMaterial = ExtendedMaterial<StandardMaterial, TerrainDetailExtension>;

const SHADER_ASSET_PATH: &str = "shaders/terrain_detail.wgsl";

/// Tunable from Rust without touching the shader itself. Four `f32`s so the
/// uniform buffer lands on a 16-byte boundary with no manual padding.
#[derive(Asset, AsBindGroup, Reflect, Debug, Clone)]
pub struct TerrainDetailExtension {
    /// Spatial frequency (1 / meters) of the broad patchy variation —
    /// "clumps" of slightly brighter/darker ground a few meters across.
    #[uniform(100)]
    pub macro_freq: f32,
    /// Spatial frequency of the fine grain layered on top, also reused as
    /// the height field the fake bump-normal is derived from.
    #[uniform(100)]
    pub micro_freq: f32,
    /// Max fractional albedo swing from the two noise layers combined (0.12
    /// = ground brightness wobbles by up to +/-12%). Kept small on purpose —
    /// this is meant to read as ground texture, not a psychedelic pattern.
    #[uniform(100)]
    pub color_detail_strength: f32,
    /// How strongly the fake heightfield bump perturbs the lighting normal.
    /// 0 = perfectly smooth (today's look); higher values make the same
    /// noise catch light as if the ground had real micro-relief.
    #[uniform(100)]
    pub normal_strength: f32,
}

impl Default for TerrainDetailExtension {
    fn default() -> Self {
        Self {
            // Mars regolith: bigger dust-drift/crater-field patches than
            // the old fine-grass grain (lower macro_freq), a tighter pebbly
            // grain up close (higher micro_freq), more visible dust/rock
            // tonal contrast (higher color_detail_strength), and a
            // stronger fake bump so the same noise reads as pitted,
            // cratered ground catching light rather than smooth turf.
            macro_freq: 0.035,
            micro_freq: 0.9,
            color_detail_strength: 0.22,
            normal_strength: 0.85,
        }
    }
}

impl MaterialExtension for TerrainDetailExtension {
    fn fragment_shader() -> ShaderRef {
        SHADER_ASSET_PATH.into()
    }
}

pub struct TerrainMaterialPlugin;

impl Plugin for TerrainMaterialPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(MaterialPlugin::<TerrainMaterial>::default());
    }
}
