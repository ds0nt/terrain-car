use bevy::prelude::*;
use shared::car_physics::{wheel_mounts, CarChassis, Wheel};
use shared::protocol::{CarSnapshot, LocalCar};
use shared::worldspace::WorldOrigin;

pub struct CarRenderPlugin;

impl Plugin for CarRenderPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(init_car_visuals)
            .add_systems(Update, sync_remote_car_transforms);
    }
}

/// Fires for every car — the local player's own (`CarChassis` inserted the
/// same frame as its physics bundle, see car.rs's spawn_car) and every
/// remote player's (inserted the moment replication first receives their
/// `CarChassis`). Spawns the chassis mesh and wheel/cabin cosmetics
/// generically off just the replicated tuning data, so a remote car needs
/// zero extra network traffic to look right — this is purely local
/// rendering setup.
fn init_car_visuals(
    insert: On<Insert, CarChassis>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    chassis_q: Query<&CarChassis>,
) {
    let Ok(chassis) = chassis_q.get(insert.entity) else {
        return;
    };
    let half_extents = chassis.half_extents;
    let wheel_radius = chassis.wheel_radius;

    // The local car already has its own spawn-computed Transform (same
    // bundle as CarChassis) — a remote car has none yet (Transform isn't
    // itself replicated; see protocol.rs's CarSnapshot docs for why), so
    // `insert_if_new` only actually does anything for remote cars.
    commands
        .entity(insert.entity)
        .insert_if_new(Transform::IDENTITY);

    commands.entity(insert.entity).insert((
        Mesh3d(meshes.add(Cuboid::from_size(half_extents * 2.0))),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: color_from_seed(chassis.color_seed),
            ..default()
        })),
    ));

    let wheel_mesh = meshes.add(Cylinder::new(wheel_radius, 0.3));
    let wheel_material = materials.add(StandardMaterial {
        base_color: Color::srgb(0.05, 0.05, 0.05),
        ..default()
    });

    commands.entity(insert.entity).with_children(|parent| {
        for (offset, is_front) in wheel_mounts(half_extents) {
            parent.spawn((
                Mesh3d(wheel_mesh.clone()),
                MeshMaterial3d(wheel_material.clone()),
                Transform::from_translation(offset)
                    .with_rotation(Quat::from_rotation_z(std::f32::consts::FRAC_PI_2)),
                Wheel {
                    local_offset: offset,
                    is_front,
                    spin: 0.0,
                },
            ));
        }

        // Cosmetic-only greebles: no colliders, ride on the chassis's own
        // collider (local car) or aren't collided with at all (remote
        // cars have no collider client-side to begin with).
        let cabin_half = Vec3::new(half_extents.x * 0.7, half_extents.y * 0.5, half_extents.z * 0.35);
        parent.spawn((
            Mesh3d(meshes.add(Cuboid::from_size(cabin_half * 2.0))),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color: Color::srgba(0.08, 0.1, 0.12, 0.75),
                perceptual_roughness: 0.1,
                alpha_mode: AlphaMode::Blend,
                ..default()
            })),
            Transform::from_xyz(0.0, half_extents.y + cabin_half.y, -half_extents.z * 0.25),
        ));

        parent.spawn((
            Mesh3d(meshes.add(Cylinder::new(0.06, 1.2))),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color: Color::srgb(0.15, 0.15, 0.17),
                metallic: 0.8,
                perceptual_roughness: 0.3,
                ..default()
            })),
            Transform::from_xyz(0.0, half_extents.y * 0.4, -(half_extents.z + 0.5))
                .with_rotation(Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2)),
        ));
    });
}

/// Deterministic seed -> vibrant paint color. Every car with the same
/// `color_seed` (i.e. the same connection id — see `CarChassis::color_seed`'s
/// docs) gets the same color on every viewer, since `CarChassis` itself is
/// replicated. Fixed saturation/lightness, varying only hue, so every color
/// reads clearly as "a car" against the terrain regardless of which hue it
/// lands on.
fn color_from_seed(seed: u32) -> Color {
    let hue = (seed.wrapping_mul(2_654_435_761) % 360) as f32;
    Color::hsl(hue, 0.75, 0.5)
}

/// Remote (non-local) cars have no Rapier body client-side at all — just
/// mirror the server's replicated snapshot onto Transform every frame. No
/// smoothing/interpolation in this first pass (a reasonable fast-follow if
/// remote cars look jittery at low replication rates).
///
/// `CarSnapshot.translation` is server-space, which is always true-space
/// (the server never rebases its own `WorldOrigin` — a known gap, fine
/// while everyone stays within a few km of spawn, see server's
/// terrain_phys.rs). Converting through the client's own `WorldOrigin`
/// keeps remote cars positioned correctly relative to local terrain even
/// after *this* client has rebased, without needing the server to.
fn sync_remote_car_transforms(
    origin: Res<WorldOrigin>,
    mut cars_q: Query<(&CarSnapshot, &mut Transform), Without<LocalCar>>,
) {
    for (snapshot, mut transform) in &mut cars_q {
        let local = (snapshot.translation.as_dvec3() - origin.offset).as_vec3();
        transform.translation = local;
        transform.rotation = snapshot.rotation;
    }
}
