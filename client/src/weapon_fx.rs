use bevy::math::DVec3;
use bevy::prelude::*;
use bevy_hanabi::prelude::*;
use bevy_rapier3d::prelude::{Collider, RigidBody, Velocity};
use bevy_replicon::prelude::ClientTriggerExt;
use shared::combat::FIRE_COOLDOWN_SECS;
use shared::protocol::{FireGunMsg, GunFiredMsg};

use crate::fx::{FadeLight, FadeOut, GrowScale, Lifetime};
use crate::worldspace::WorldOrigin;

/// Local player's fire input plus every client's reaction to any car's shot
/// (`GunFiredMsg` is a broadcast — this fires identically whether the
/// local player, a remote player, or a miss into empty air caused it). All
/// the actual gun *logic* (rate limit, raycast, damage) is server-side
/// (`server/src/weapons.rs`); this module is purely "press F" and "render
/// the boom," built on `fx.rs`'s generic effect primitives.
pub struct WeaponFxPlugin;

impl Plugin for WeaponFxPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<LocalFireCooldown>()
            .add_systems(Startup, setup_smoke_effect)
            .add_systems(Update, send_fire_input)
            .add_observer(spawn_gun_fired_fx);
    }
}

/// The muzzle smoke puff's `EffectAsset`, built once at startup — every
/// shot spawns a fresh, independent one-shot `ParticleEffect` instance of
/// this same asset (see `spawn_gun_fired_fx`) rather than rebuilding the
/// effect graph per shot.
#[derive(Resource)]
struct SmokeEffect(Handle<EffectAsset>);

/// One-shot puff: a small burst of particles that drift up and slightly
/// forward, expanding and fading to nothing over under a second — smoke
/// off the muzzle, not a persistent cloud.
fn create_smoke_effect() -> EffectAsset {
    let writer = ExprWriter::new();

    let init_pos = SetPositionSphereModifier {
        center: writer.lit(Vec3::ZERO).expr(),
        radius: writer.lit(0.06).expr(),
        dimension: ShapeDimension::Volume,
    };

    let random_dir = (writer.rand(VectorType::VEC3F) * writer.lit(2.0) - writer.lit(1.0)).normalized();
    let vel = (random_dir * writer.lit(0.5) + writer.lit(Vec3::Y * 0.7)).expr();
    let init_vel = SetAttributeModifier::new(Attribute::VELOCITY, vel);

    let init_age = SetAttributeModifier::new(Attribute::AGE, writer.lit(0.0).expr());
    let init_lifetime =
        SetAttributeModifier::new(Attribute::LIFETIME, writer.lit(0.5).uniform(writer.lit(0.9)).expr());

    let update_drag = LinearDragModifier::new(writer.lit(1.2).expr());

    let mut color_gradient = bevy_hanabi::Gradient::new();
    color_gradient.add_key(0.0, Vec4::new(0.55, 0.55, 0.55, 0.5));
    color_gradient.add_key(1.0, Vec4::new(0.3, 0.3, 0.3, 0.0));

    let mut size_gradient = bevy_hanabi::Gradient::new();
    size_gradient.add_key(0.0, Vec3::splat(0.05));
    size_gradient.add_key(1.0, Vec3::splat(0.4));

    EffectAsset::new(32, SpawnerSettings::once(14.0.into()), writer.finish())
        .with_name("gun_smoke")
        .init(init_pos)
        .init(init_vel)
        .init(init_age)
        .init(init_lifetime)
        .update(update_drag)
        .render(ColorOverLifetimeModifier::new(color_gradient))
        .render(SizeOverLifetimeModifier { gradient: size_gradient, screen_space_size: false })
}

fn setup_smoke_effect(mut commands: Commands, mut effects: ResMut<Assets<EffectAsset>>) {
    commands.insert_resource(SmokeEffect(effects.add(create_smoke_effect())));
}

/// Long enough to cover the burst's own max particle lifetime (0.9s above)
/// with margin, so the emitter entity never disappears mid-fade.
const SMOKE_EMITTER_LIFETIME: f32 = 1.5;

/// Client-side mirror of the server's own cooldown gate — purely for
/// responsive UX (no flickering "did that even register" from spamming F
/// faster than the server will actually fire); the server enforces the
/// real limit independently and doesn't trust this at all.
#[derive(Resource, Default)]
struct LocalFireCooldown {
    remaining: f32,
}

fn send_fire_input(
    time: Res<Time>,
    keyboard: Res<ButtonInput<KeyCode>>,
    mut cooldown: ResMut<LocalFireCooldown>,
    mut commands: Commands,
) {
    cooldown.remaining = (cooldown.remaining - time.delta_secs()).max(0.0);
    if !keyboard.pressed(KeyCode::KeyF) || cooldown.remaining > 0.0 {
        return;
    }
    cooldown.remaining = FIRE_COOLDOWN_SECS;
    commands.client_trigger(FireGunMsg);
}

const MUZZLE_FLASH_LIFETIME: f32 = 0.06;
const MUZZLE_FLASH_SCALE: f32 = 0.4;
const MUZZLE_LIGHT_LIFETIME: f32 = 0.05;
const MUZZLE_LIGHT_INTENSITY: f32 = 2_000_000.0;
/// ~3 frames at 60fps — "a snap, not a laser beam" per the original ask.
const TRACER_LIFETIME: f32 = 0.05;
const IMPACT_FLASH_LIFETIME: f32 = 0.12;
const IMPACT_FLASH_SCALE: f32 = 1.2;
const SHELL_COUNT: usize = 2;
const SHELL_LIFETIME: f32 = 2.5;

const MUZZLE_EMISSIVE: LinearRgba = LinearRgba::rgb(12.0, 9.0, 3.0);
const TRACER_EMISSIVE: LinearRgba = LinearRgba::rgb(10.0, 8.0, 2.0);
const IMPACT_EMISSIVE: LinearRgba = LinearRgba::rgb(9.0, 6.0, 2.0);

/// Renders one resolved shot: muzzle flash + light, a brief tracer from
/// muzzle to end point, an impact flash at the end point, and a couple of
/// ejected shell casings. Every client runs this identically off the same
/// `GunFiredMsg`, so a miss (into empty terrain) and a hit (on another car)
/// look the same at the gun end regardless of which client is watching.
fn spawn_gun_fired_fx(
    fired: On<GunFiredMsg>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    origin: Res<WorldOrigin>,
    smoke: Res<SmokeEffect>,
) {
    let muzzle_true = DVec3::new(fired.muzzle_true_x, fired.muzzle_y as f64, fired.muzzle_true_z);
    let end_true = DVec3::new(fired.end_true_x, fired.end_y as f64, fired.end_true_z);
    let muzzle_local = (muzzle_true - origin.offset).as_vec3();
    let end_local = (end_true - origin.offset).as_vec3();

    commands.spawn((
        Transform::from_translation(muzzle_local),
        ParticleEffect::new(smoke.0.clone()),
        Lifetime::new(SMOKE_EMITTER_LIFETIME),
    ));

    commands.spawn((
        Mesh3d(meshes.add(Sphere::new(1.0))),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::srgba(1.0, 0.9, 0.6, 0.9),
            emissive: MUZZLE_EMISSIVE,
            alpha_mode: bevy::material::AlphaMode::Blend,
            unlit: true,
            ..default()
        })),
        Transform::from_translation(muzzle_local).with_scale(Vec3::splat(0.05)),
        Lifetime::new(MUZZLE_FLASH_LIFETIME),
        FadeOut { base_alpha: 0.9, base_emissive: MUZZLE_EMISSIVE },
        GrowScale { start_scale: 0.05, end_scale: MUZZLE_FLASH_SCALE },
    ));

    commands.spawn((
        PointLight {
            color: Color::srgb(1.0, 0.85, 0.5),
            intensity: MUZZLE_LIGHT_INTENSITY,
            range: 15.0,
            shadow_maps_enabled: false,
            ..default()
        },
        Transform::from_translation(muzzle_local),
        Lifetime::new(MUZZLE_LIGHT_LIFETIME),
        FadeLight { base_intensity: MUZZLE_LIGHT_INTENSITY },
    ));

    let delta = end_local - muzzle_local;
    let length = delta.length().max(0.01);
    let direction = delta / length;
    let mid = muzzle_local + delta * 0.5;
    // The cylinder mesh is Y-up by default; rotate it onto the shot
    // direction.
    let rotation = Quat::from_rotation_arc(Vec3::Y, direction);
    commands.spawn((
        Mesh3d(meshes.add(Cylinder::new(0.03, length))),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::srgba(1.0, 0.85, 0.3, 0.9),
            emissive: TRACER_EMISSIVE,
            alpha_mode: bevy::material::AlphaMode::Blend,
            unlit: true,
            ..default()
        })),
        Transform::from_translation(mid).with_rotation(rotation),
        Lifetime::new(TRACER_LIFETIME),
        FadeOut { base_alpha: 0.9, base_emissive: TRACER_EMISSIVE },
    ));

    commands.spawn((
        Mesh3d(meshes.add(Sphere::new(1.0))),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::srgba(1.0, 0.8, 0.5, 0.85),
            emissive: IMPACT_EMISSIVE,
            alpha_mode: bevy::material::AlphaMode::Blend,
            unlit: true,
            ..default()
        })),
        Transform::from_translation(end_local).with_scale(Vec3::splat(0.1)),
        Lifetime::new(IMPACT_FLASH_LIFETIME),
        FadeOut { base_alpha: 0.85, base_emissive: IMPACT_EMISSIVE },
        GrowScale { start_scale: 0.1, end_scale: IMPACT_FLASH_SCALE },
    ));

    let right = {
        let r = direction.cross(Vec3::Y);
        if r.length_squared() > 1e-6 { r.normalize() } else { Vec3::X }
    };
    for i in 0..SHELL_COUNT {
        let jitter = i as f32 * 0.6;
        let eject_dir =
            (right * 1.0 - direction * 0.3 + Vec3::Y * (0.4 + jitter * 0.1)).normalize();
        commands.spawn((
            Mesh3d(meshes.add(Cylinder::new(0.02, 0.08))),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color: Color::srgb(0.85, 0.7, 0.25),
                metallic: 0.9,
                perceptual_roughness: 0.4,
                ..default()
            })),
            Transform::from_translation(muzzle_local + right * 0.15),
            RigidBody::Dynamic,
            Collider::cylinder(0.04, 0.02),
            Velocity {
                linear: eject_dir * (2.5 + jitter),
                angular: Vec3::new(6.0, 3.0, 0.0),
            },
            Lifetime::new(SHELL_LIFETIME),
        ));
    }
}
