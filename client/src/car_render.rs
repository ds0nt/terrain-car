use bevy::prelude::*;
use shared::car_physics::{wheel_mounts, CarChassis, Wheel};
use shared::protocol::{CarSnapshot, LocalCar};
use shared::worldspace::WorldOrigin;

use crate::owner_color::color_from_seed;

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
        //
        // Cabin: an open-front "greenhouse" shell (roof + rear window + two
        // side pillars) rather than one solid box enclosing the whole
        // cockpit volume. The old single tinted cuboid fully surrounded the
        // cockpit camera's position (see camera.rs's `local_seat`) — every
        // forward-looking frame in Cockpit mode looked straight through at
        // least one, often two, faces of that same 75%-alpha box from the
        // inside, reading as a permanently dim/foggy windshield. Leaving
        // the entire front arc with no geometry at all fixes that
        // directly: the roof/pillars are opaque structural frame (never in
        // the forward view, which points between them, not through them),
        // and only the rear window stays actual tinted glass — safely
        // behind the driver's seat, not in front of it.
        let cabin_half = Vec3::new(half_extents.x * 0.7, half_extents.y * 0.5, half_extents.z * 0.35);
        let cabin_center = Vec3::new(0.0, half_extents.y + cabin_half.y, -half_extents.z * 0.25);
        let panel_half_thickness = 0.02;
        let frame_material = materials.add(StandardMaterial {
            base_color: Color::srgb(0.1, 0.1, 0.11),
            perceptual_roughness: 0.5,
            ..default()
        });

        // Roof.
        parent.spawn((
            Mesh3d(meshes.add(Cuboid::from_size(Vec3::new(
                cabin_half.x * 2.0,
                panel_half_thickness * 2.0,
                cabin_half.z * 2.0,
            )))),
            MeshMaterial3d(frame_material.clone()),
            Transform::from_translation(cabin_center + Vec3::Y * cabin_half.y),
        ));

        // Rear window — real tinted glass, but entirely behind the
        // cockpit camera's seat position, never in its forward view.
        parent.spawn((
            Mesh3d(meshes.add(Cuboid::from_size(Vec3::new(
                cabin_half.x * 2.0,
                cabin_half.y * 2.0,
                panel_half_thickness * 2.0,
            )))),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color: Color::srgba(0.08, 0.1, 0.12, 0.75),
                perceptual_roughness: 0.1,
                alpha_mode: AlphaMode::Blend,
                ..default()
            })),
            Transform::from_translation(cabin_center + Vec3::Z * cabin_half.z),
        ));

        // Side pillars, connecting roof to body — opaque frame, not glass.
        for side in [-1.0, 1.0] {
            parent.spawn((
                Mesh3d(meshes.add(Cuboid::from_size(Vec3::new(
                    panel_half_thickness * 2.0,
                    cabin_half.y * 2.0,
                    cabin_half.z * 2.0,
                )))),
                MeshMaterial3d(frame_material.clone()),
                Transform::from_translation(cabin_center + Vec3::X * (side * cabin_half.x)),
            ));
        }

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
