use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use bevy_rapier3d::prelude::*;
use bevy_replicon::prelude::ClientTriggerExt;
use shared::buildings::BuildingKind;
use shared::protocol::PlaceBuildingMsg;

use crate::building_render::building_mesh_and_transform;
use crate::camera::CarCamera;
use crate::car::LocalCar;
use crate::worldspace::WorldOrigin;

/// Mouse-raycast building placement: pick a kind (see `SelectBuildingKind`,
/// fired by the build menu's buttons — `building_ui.rs`), a translucent
/// ghost then follows wherever the cursor is actually pointing in the 3D
/// world (a Rapier raycast from the camera through the cursor, same "click
/// in the world" idea a raycast-based level editor would use, not the
/// old "always at my own car's position" placement). For a `Ramp`
/// specifically, the initial click sets the anchor and holding+dragging
/// the mouse before releasing sets which way it faces — every other kind
/// places immediately on click with a fixed default orientation. Escape or
/// right-click cancels at any point without spending anything (nothing is
/// deducted client-side anyway — the server is the sole authority on cost,
/// this is purely "where do you want it").
pub struct BuildingPlacementPlugin;

impl Plugin for BuildingPlacementPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PlacementState>()
            .add_message::<SelectBuildingKind>()
            .add_systems(Update, (begin_selection, drive_placement).chain());
    }
}

/// Fired by the build menu when a player clicks a building kind — the
/// menu itself never places anything directly anymore, it just starts
/// placement mode.
#[derive(Message, Clone, Copy)]
pub struct SelectBuildingKind(pub BuildingKind);

#[derive(Resource, Default)]
enum PlacementState {
    #[default]
    Idle,
    /// Ghost follows the raycast hit; a click either places immediately
    /// (non-Ramp) or moves to `Aiming` (Ramp).
    Selecting(BuildingKind),
    /// Ramp only: position is locked in at `anchor_local`/`anchor_true_*`
    /// from the moment the mouse went down; dragging further only changes
    /// `rotation_y`, released to confirm.
    Aiming { anchor_local: Vec3, anchor_true_x: f64, anchor_true_z: f64, rotation_y: f32 },
}

#[derive(Component)]
struct PlacementGhost;

fn begin_selection(
    mut commands: Commands,
    mut events: MessageReader<SelectBuildingKind>,
    mut state: ResMut<PlacementState>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    ghosts: Query<Entity, With<PlacementGhost>>,
) {
    let Some(SelectBuildingKind(kind)) = events.read().last().copied() else {
        return;
    };
    for entity in &ghosts {
        commands.entity(entity).despawn();
    }
    // Placeholder pose — `drive_placement` corrects it to the real cursor
    // raycast the very next frame; spawning it here (rather than waiting
    // for the first raycast hit) means there's never a one-frame gap with
    // no ghost at all.
    let (mesh, base_color, transform) =
        building_mesh_and_transform(kind, &mut meshes, 0.0, 0.0, 0.0, 0.0);
    commands.spawn((
        Mesh3d(mesh),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color,
            alpha_mode: AlphaMode::Blend,
            ..default()
        })),
        transform,
        PlacementGhost,
    ));
    *state = PlacementState::Selecting(kind);
}

/// A single Rapier raycast from the camera through the cursor — the same
/// underlying idea `weapons.rs`'s hitscan and `camera.rs`'s clip-avoidance
/// already use, just aimed by the mouse instead of the car's own forward
/// vector. Excludes the local car's own collider so pointing at yourself
/// doesn't just hit your own roof.
fn cursor_world_hit(
    windows: &Query<&Window, With<PrimaryWindow>>,
    camera_q: &Query<(&Camera, &GlobalTransform), With<CarCamera>>,
    rapier_context: &ReadRapierContext,
    exclude: Option<Entity>,
) -> Option<Vec3> {
    let window = windows.iter().next()?;
    let cursor = window.cursor_position()?;
    let (camera, camera_transform) = camera_q.iter().next()?;
    let ray = camera.viewport_to_world(camera_transform, cursor).ok()?;
    let context = rapier_context.single().ok()?;
    let mut filter = QueryFilter::default();
    if let Some(exclude) = exclude {
        filter = filter.exclude_rigid_body(exclude);
    }
    let (_, toi) = context.cast_ray(ray.origin, *ray.direction, 2000.0, true, filter)?;
    Some(ray.origin + *ray.direction * toi)
}

#[allow(clippy::too_many_arguments)]
fn drive_placement(
    mut commands: Commands,
    mut state: ResMut<PlacementState>,
    mouse: Res<ButtonInput<MouseButton>>,
    keyboard: Res<ButtonInput<KeyCode>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    camera_q: Query<(&Camera, &GlobalTransform), With<CarCamera>>,
    rapier_context: ReadRapierContext,
    local_car_q: Query<Entity, With<LocalCar>>,
    origin: Res<WorldOrigin>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut ghost_q: Query<(Entity, &mut Transform), With<PlacementGhost>>,
) {
    if matches!(*state, PlacementState::Idle) {
        return;
    }

    if keyboard.just_pressed(KeyCode::Escape) || mouse.just_pressed(MouseButton::Right) {
        for (entity, _) in &ghost_q {
            commands.entity(entity).despawn();
        }
        *state = PlacementState::Idle;
        return;
    }

    let exclude = local_car_q.iter().next();
    let Some(hit) = cursor_world_hit(&windows, &camera_q, &rapier_context, exclude) else {
        return;
    };
    let Ok((_, mut ghost_transform)) = ghost_q.single_mut() else {
        return;
    };

    match &mut *state {
        PlacementState::Idle => unreachable!("checked above"),
        PlacementState::Selecting(kind) => {
            let kind = *kind;
            let (_, _, transform) =
                building_mesh_and_transform(kind, &mut meshes, hit.x, hit.z, hit.y, 0.0);
            *ghost_transform = transform;

            if mouse.just_pressed(MouseButton::Left) {
                let hit_true = origin.to_true(hit);
                if kind == BuildingKind::Ramp {
                    *state = PlacementState::Aiming {
                        anchor_local: hit,
                        anchor_true_x: hit_true.x,
                        anchor_true_z: hit_true.z,
                        rotation_y: 0.0,
                    };
                } else {
                    commands.client_trigger(PlaceBuildingMsg {
                        kind,
                        true_x: hit_true.x,
                        true_z: hit_true.z,
                        rotation_y: 0.0,
                    });
                    commands.entity(ghost_q.single().unwrap().0).despawn();
                    *state = PlacementState::Idle;
                }
            }
        }
        PlacementState::Aiming { anchor_local, anchor_true_x, anchor_true_z, rotation_y } => {
            let dx = hit.x - anchor_local.x;
            let dz = hit.z - anchor_local.z;
            // Only update the facing once the drag has moved far enough to
            // mean something — right at the anchor point dx/dz are ~zero
            // and atan2 of that is meaningless noise.
            if dx * dx + dz * dz > 0.25 {
                // Ramp forward (local -Z rotated by rotation_y) is
                // (-sin(rotation_y), -cos(rotation_y)) in world (x, z);
                // solving for rotation_y so that forward points along
                // (dx, dz) gives atan2(-dx, -dz). If a placed ramp ends up
                // facing backward from the drag, flip both signs here.
                *rotation_y = (-dx).atan2(-dz);
            }
            let (_, _, transform) = building_mesh_and_transform(
                BuildingKind::Ramp,
                &mut meshes,
                anchor_local.x,
                anchor_local.z,
                anchor_local.y,
                *rotation_y,
            );
            *ghost_transform = transform;

            if mouse.just_released(MouseButton::Left) {
                commands.client_trigger(PlaceBuildingMsg {
                    kind: BuildingKind::Ramp,
                    true_x: *anchor_true_x,
                    true_z: *anchor_true_z,
                    rotation_y: *rotation_y,
                });
                commands.entity(ghost_q.single().unwrap().0).despawn();
                *state = PlacementState::Idle;
            }
        }
    }
}
