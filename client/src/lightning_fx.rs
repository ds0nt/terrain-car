use bevy::math::DVec3;
use bevy::prelude::*;
use shared::protocol::LightningStrikeMsg;
use shared::terrain_gen::{height_at, TerrainNoise};

use crate::fx::{FadeLight, FadeOut, GrowScale, Lifetime};
use crate::worldspace::WorldOrigin;

/// Purely cosmetic reaction to the server's `LightningStrikeMsg` — the
/// actual physics knockback already reached this client through ordinary
/// `CarSnapshot` replication (see server's `lightning.rs`), so all this
/// does is render the same boom every other client is seeing: a bright
/// expanding flash sphere plus a brief point light, both built from
/// `fx.rs`'s generic effect primitives rather than hand-rolled bookkeeping.
pub struct LightningFxPlugin;

impl Plugin for LightningFxPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(spawn_lightning_fx);
    }
}

const FLASH_LIFETIME_SECS: f32 = 0.35;
const FLASH_MAX_SCALE: f32 = 9.0;
const LIGHT_LIFETIME_SECS: f32 = 0.15;
const FLASH_EMISSIVE: LinearRgba = LinearRgba::rgb(8.0, 9.0, 14.0);
const LIGHT_INTENSITY: f32 = 8_000_000.0;

fn spawn_lightning_fx(
    strike: On<LightningStrikeMsg>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    noise: Res<TerrainNoise>,
    origin: Res<WorldOrigin>,
) {
    let strike_true = DVec3::new(strike.true_x, 0.0, strike.true_z);
    let local = (strike_true - origin.offset).as_vec3();
    let ground_y = height_at(&noise, strike.true_x, strike.true_z);

    commands.spawn((
        Mesh3d(meshes.add(Sphere::new(1.0))),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::srgba(0.8, 0.88, 1.0, 0.85),
            emissive: FLASH_EMISSIVE,
            alpha_mode: AlphaMode::Blend,
            unlit: true,
            ..default()
        })),
        Transform::from_xyz(local.x, ground_y + 0.5, local.z).with_scale(Vec3::splat(0.15)),
        Lifetime::new(FLASH_LIFETIME_SECS),
        FadeOut { base_alpha: 0.85, base_emissive: FLASH_EMISSIVE },
        GrowScale { start_scale: 0.15, end_scale: FLASH_MAX_SCALE },
    ));

    commands.spawn((
        PointLight {
            color: Color::srgb(0.8, 0.88, 1.0),
            intensity: LIGHT_INTENSITY,
            range: strike.radius * 4.0,
            shadow_maps_enabled: false,
            ..default()
        },
        Transform::from_xyz(local.x, ground_y + 5.0, local.z),
        Lifetime::new(LIGHT_LIFETIME_SECS),
        FadeLight { base_intensity: LIGHT_INTENSITY },
    ));
}
