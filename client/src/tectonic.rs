use bevy::prelude::*;
use bevy_rapier3d::prelude::*;
use rand::Rng;

use crate::car::LocalCar;

/// Experimental alternate terrain-generation approach, separate from the
/// heightmap-noise system in terrain.rs: instead of computing a height
/// function, drop a pile of randomly-shaped rigid blocks and let the
/// *physics simulation already running* settle them under gravity —
/// plates crashing together and mashing, the way real orogeny piles up
/// rock, rather than a smooth mathematical surface. Once a pile stops
/// moving it freezes into static terrain.
///
/// The key difference from terrain.rs's heightfield: this is not a pure
/// function of (x, z). A block resting diagonally across two others is a
/// genuine overhang — there are two different heights at the same (x, z),
/// and a cave underneath. A heightfield collider fundamentally cannot
/// represent that; this uses real cuboid colliders instead, so it can.
///
/// Bounded and on-demand rather than infinite/streamed like the main
/// terrain — each press drops one pile near the car. Making this kind of
/// generation stream indefinitely like terrain.rs does is a fundamentally
/// harder problem (chunk-independent generation doesn't make sense when
/// neighboring piles physically depend on each other as they settle), so
/// this stays an experiment you trigger, not a replacement for the main
/// world.
pub struct TectonicPlugin;

impl Plugin for TectonicPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, (spawn_tectonic_pile, settle_tectonic_blocks));
    }
}

#[derive(Component)]
struct TectonicBlock;

#[derive(Component)]
struct Settled;

#[derive(Component, Default)]
struct SettleTimer(f32);

const BLOCK_COUNT: usize = 70;
const DROP_RADIUS: f32 = 22.0;
/// A block frozen once it's been slower than this for SETTLE_TIME seconds.
const SETTLE_SPEED: f32 = 0.2;
const SETTLE_TIME: f32 = 1.2;

/// T: drop a fresh pile of blocks a short distance ahead of the car.
fn spawn_tectonic_pile(
    keyboard: Res<ButtonInput<KeyCode>>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    chassis_q: Query<&GlobalTransform, With<LocalCar>>,
) {
    if !keyboard.just_pressed(KeyCode::KeyT) {
        return;
    }
    let Ok(chassis_gt) = chassis_q.single() else {
        return;
    };
    let chassis_transform = chassis_gt.compute_transform();
    let center =
        chassis_transform.translation + *chassis_transform.forward() * 45.0 + Vec3::Y * 5.0;

    let mut rng = rand::thread_rng();
    for i in 0..BLOCK_COUNT {
        let size = Vec3::new(
            rng.gen_range(2.5..8.0),
            rng.gen_range(2.5..8.0),
            rng.gen_range(2.5..8.0),
        );
        let pos = center
            + Vec3::new(
                rng.gen_range(-DROP_RADIUS..DROP_RADIUS),
                // Staggered drop heights so blocks don't all spawn
                // overlapping each other at once (Rapier would violently
                // eject overlapping colliders on the first step).
                12.0 + i as f32 * 2.2,
                rng.gen_range(-DROP_RADIUS..DROP_RADIUS),
            );
        let rotation = Quat::from_euler(
            EulerRot::XYZ,
            rng.gen_range(0.0..std::f32::consts::TAU),
            rng.gen_range(0.0..std::f32::consts::TAU),
            rng.gen_range(0.0..std::f32::consts::TAU),
        );

        // Muted rock-toned gray with a little per-block variation, so a
        // settled pile reads as "rock formation" rather than "red boxes."
        let tone = rng.gen_range(0.32..0.48);
        let material = materials.add(StandardMaterial {
            base_color: Color::srgb(tone, tone * 0.96, tone * 0.92),
            perceptual_roughness: 0.95,
            ..default()
        });

        commands.spawn((
            Mesh3d(meshes.add(Cuboid::from_size(size))),
            MeshMaterial3d(material),
            Transform::from_translation(pos).with_rotation(rotation),
            RigidBody::Dynamic,
            Collider::cuboid(size.x * 0.5, size.y * 0.5, size.z * 0.5),
            Friction::coefficient(1.3),
            Restitution::coefficient(0.02),
            Damping {
                linear_damping: 0.4,
                angular_damping: 0.6,
            },
            Velocity::zero(),
            TectonicBlock,
            SettleTimer::default(),
        ));
    }
}

/// Freezes each block into static (`RigidBody::Fixed`) terrain once it's
/// been moving slower than SETTLE_SPEED for SETTLE_TIME seconds straight —
/// i.e. once the pile has actually finished crashing together, not just
/// momentarily paused mid-tumble.
fn settle_tectonic_blocks(
    time: Res<Time>,
    mut commands: Commands,
    mut blocks: Query<
        (Entity, &Velocity, &mut SettleTimer),
        (With<TectonicBlock>, Without<Settled>),
    >,
) {
    let dt = time.delta_secs();
    for (entity, velocity, mut timer) in &mut blocks {
        let speed = velocity.linear.length() + velocity.angular.length();
        if speed < SETTLE_SPEED {
            timer.0 += dt;
        } else {
            timer.0 = 0.0;
        }
        if timer.0 > SETTLE_TIME {
            commands.entity(entity).insert((RigidBody::Fixed, Settled));
        }
    }
}
