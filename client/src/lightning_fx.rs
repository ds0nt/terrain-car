use bevy::math::DVec3;
use bevy::prelude::*;
use shared::protocol::LightningStrikeMsg;
use shared::terrain_gen::{height_at, TerrainNoise};

use crate::worldspace::WorldOrigin;

/// Purely cosmetic reaction to the server's `LightningStrikeMsg` — the
/// actual physics knockback already reached this client through ordinary
/// `CarSnapshot` replication (see server's `lightning.rs`), so all this
/// does is render the same boom every other client is seeing: a bright
/// expanding flash sphere plus a brief point light, both self-despawning.
pub struct LightningFxPlugin;

impl Plugin for LightningFxPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(spawn_lightning_fx)
            .add_systems(Update, animate_lightning_fx);
    }
}

const FLASH_LIFETIME_SECS: f32 = 0.35;
const FLASH_MAX_SCALE: f32 = 9.0;
const LIGHT_LIFETIME_SECS: f32 = 0.15;

#[derive(Component)]
struct LightningFlash {
    age: f32,
}

#[derive(Component)]
struct LightningFlashLight {
    age: f32,
}

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
            emissive: LinearRgba::rgb(8.0, 9.0, 14.0),
            alpha_mode: AlphaMode::Blend,
            unlit: true,
            ..default()
        })),
        Transform::from_xyz(local.x, ground_y + 0.5, local.z).with_scale(Vec3::splat(0.15)),
        LightningFlash { age: 0.0 },
    ));

    commands.spawn((
        PointLight {
            color: Color::srgb(0.8, 0.88, 1.0),
            intensity: 8_000_000.0,
            range: strike.radius * 4.0,
            shadow_maps_enabled: false,
            ..default()
        },
        Transform::from_xyz(local.x, ground_y + 5.0, local.z),
        LightningFlashLight { age: 0.0 },
    ));
}

fn animate_lightning_fx(
    time: Res<Time>,
    mut commands: Commands,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut flashes: Query<(Entity, &mut Transform, &MeshMaterial3d<StandardMaterial>, &mut LightningFlash)>,
    mut lights: Query<(Entity, &mut PointLight, &mut LightningFlashLight)>,
) {
    let dt = time.delta_secs();

    for (entity, mut transform, material, mut flash) in &mut flashes {
        flash.age += dt;
        let t = (flash.age / FLASH_LIFETIME_SECS).clamp(0.0, 1.0);
        // Expands fast, then eases off — a shockwave, not a linear balloon.
        let scale = 0.15 + FLASH_MAX_SCALE * t.sqrt();
        transform.scale = Vec3::splat(scale);
        if let Some(mut mat) = materials.get_mut(&material.0) {
            let fade = (1.0 - t).powf(2.0);
            mat.base_color.set_alpha(fade * 0.85);
            mat.emissive = LinearRgba::rgb(8.0, 9.0, 14.0) * fade;
        }
        if flash.age >= FLASH_LIFETIME_SECS {
            commands.entity(entity).despawn();
        }
    }

    for (entity, mut light, mut flash_light) in &mut lights {
        flash_light.age += dt;
        let t = (flash_light.age / LIGHT_LIFETIME_SECS).clamp(0.0, 1.0);
        light.intensity = 8_000_000.0 * (1.0 - t);
        if flash_light.age >= LIGHT_LIFETIME_SECS {
            commands.entity(entity).despawn();
        }
    }
}
