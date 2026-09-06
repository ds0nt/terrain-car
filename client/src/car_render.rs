use bevy::prelude::*;
use shared::car_physics::{wheel_mounts, CarChassis, Wheel};
use shared::protocol::{CarCosmetics, CarSnapshot, LocalCar};
use shared::worldspace::WorldOrigin;

use crate::owner_color::color_from_seed;

pub struct CarRenderPlugin;

impl Plugin for CarRenderPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(init_car_visuals)
            .add_systems(Update, (sync_remote_car_transforms, apply_car_cosmetics));
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

/// Tags the bow child mesh so `apply_car_cosmetics` can find and despawn
/// it again on the next change, without needing to remember the child's
/// `Entity` anywhere.
#[derive(Component)]
struct BowMarker;

/// Applies `CarCosmetics` to a car's existing mesh/material (spawned
/// generically by `init_car_visuals`, unaware of cosmetics at that
/// point) — a custom paint color overriding the automatic owner-hash one,
/// and a decorative bow child mesh. `Changed<CarCosmetics>` already fires
/// on the very first insert (Bevy's own change-detection rule), so this
/// one system handles both the initial application and every later
/// change a player makes, local or remote.
fn apply_car_cosmetics(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    changed: Query<
        (Entity, &CarChassis, &CarCosmetics, &MeshMaterial3d<StandardMaterial>, Option<&Children>),
        Changed<CarCosmetics>,
    >,
    bow_q: Query<(), With<BowMarker>>,
) {
    for (entity, chassis, cosmetics, material_handle, children) in &changed {
        if let Some(mut material) = materials.get_mut(&material_handle.0) {
            material.base_color = cosmetics
                .custom_color
                .map(|c| Color::srgb(c[0], c[1], c[2]))
                .unwrap_or_else(|| color_from_seed(chassis.color_seed));
        }

        let existing_bow =
            children.into_iter().flatten().find(|child| bow_q.contains(**child)).copied();
        match (cosmetics.has_bow, existing_bow) {
            (true, Some(_)) | (false, None) => {}
            (false, Some(bow)) => commands.entity(bow).despawn(),
            (true, None) => {
                let half_extents = chassis.half_extents;
                // Top-right corner of the chassis box, riding just above
                // the roof line. Built from squashed spheres (ribbon
                // loops), a small center knot, and two angled flattened
                // tails — reads as an actual tied bow rather than the
                // first version's two bare rings, still no external art
                // assets needed (matches every other greeble in
                // `init_car_visuals`).
                let bow_center =
                    Vec3::new(half_extents.x * 0.75, half_extents.y * 2.0 + 0.15, -half_extents.z * 0.3);
                let bow_material = materials.add(StandardMaterial {
                    base_color: Color::srgb(0.9, 0.15, 0.25),
                    perceptual_roughness: 0.35,
                    ..default()
                });

                // Each loop is a unit sphere squashed flat and wide, then
                // tilted so its flattened face reads as a puffed-out
                // ribbon loop rather than a ball.
                let loop_mesh = meshes.add(Sphere::new(0.16));
                let knot_mesh = meshes.add(Sphere::new(0.075));
                let tail_mesh = meshes.add(Cuboid::new(0.09, 0.045, 0.32));

                commands.entity(entity).with_children(|parent| {
                    // Left loop, splayed outward and tilted up.
                    parent.spawn((
                        Mesh3d(loop_mesh.clone()),
                        MeshMaterial3d(bow_material.clone()),
                        Transform::from_translation(bow_center + Vec3::new(-0.15, 0.03, 0.0))
                            .with_rotation(Quat::from_rotation_z(0.55) * Quat::from_rotation_x(0.3))
                            .with_scale(Vec3::new(1.3, 0.55, 0.8)),
                        BowMarker,
                    ));
                    // Right loop, mirrored.
                    parent.spawn((
                        Mesh3d(loop_mesh),
                        MeshMaterial3d(bow_material.clone()),
                        Transform::from_translation(bow_center + Vec3::new(0.15, 0.03, 0.0))
                            .with_rotation(Quat::from_rotation_z(-0.55) * Quat::from_rotation_x(0.3))
                            .with_scale(Vec3::new(1.3, 0.55, 0.8)),
                        BowMarker,
                    ));
                    // Center knot, sitting slightly forward/above so it
                    // overlaps both loops where they'd actually be tied.
                    parent.spawn((
                        Mesh3d(knot_mesh),
                        MeshMaterial3d(bow_material.clone()),
                        Transform::from_translation(bow_center + Vec3::new(0.0, 0.03, 0.03)),
                        BowMarker,
                    ));
                    // Two ribbon tails, splayed apart and angled down/back
                    // off the back of the knot.
                    parent.spawn((
                        Mesh3d(tail_mesh.clone()),
                        MeshMaterial3d(bow_material.clone()),
                        Transform::from_translation(bow_center + Vec3::new(-0.06, -0.08, 0.16))
                            .with_rotation(Quat::from_rotation_y(0.35) * Quat::from_rotation_x(-0.5)),
                        BowMarker,
                    ));
                    parent.spawn((
                        Mesh3d(tail_mesh),
                        MeshMaterial3d(bow_material),
                        Transform::from_translation(bow_center + Vec3::new(0.06, -0.08, 0.16))
                            .with_rotation(Quat::from_rotation_y(-0.35) * Quat::from_rotation_x(-0.5)),
                        BowMarker,
                    ));
                });
            }
        }
    }
}
