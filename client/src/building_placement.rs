use bevy::math::DVec3;
use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use bevy_rapier3d::prelude::*;
use bevy_replicon::prelude::ClientTriggerExt;
use shared::buildings::{slab_dims, slab_far_edge_true, BuildingKind};
use shared::protocol::{BuildingSnapshot, PlaceBuildingMsg};

use crate::building_render::building_mesh_and_transform;
use crate::camera::CarCamera;
use crate::car::LocalCar;
use crate::worldspace::WorldOrigin;

/// How close (true-space meters) a slab placement's start point has to
/// land to an existing slab's own near/far edge before it snaps onto that
/// exact point instead of using the raw cursor raycast — generous enough
/// to cover ordinary mouse imprecision, small enough that it never jumps to
/// some unrelated structure across the build site. See `nearest_slab_snap`.
const SLAB_SNAP_RADIUS: f64 = 3.5;

/// For a `uses_slab_geometry` kind, checks every existing slab building's
/// own near edge (`BuildingSnapshot::true_x`/`true_z`/`ground_y`) and far
/// edge (`slab_far_edge_true`) and returns the closest one within
/// `SLAB_SNAP_RADIUS`, if any. Placing a new Road/Platform/Wall/Ramp
/// starting exactly on one of these — rather than wherever the cursor's
/// own raycast happened to land, even a few centimeters off — is what
/// actually keeps two connected slabs flush: same point means same height
/// by construction, not "close enough that it usually lines up," which was
/// the actual cause of the reported stair-stepping at a seam between two
/// independently-eyeballed placements.
fn nearest_slab_snap(hit_true_x: f64, hit_true_z: f64, buildings: &Query<&BuildingSnapshot>) -> Option<(f64, f64, f32)> {
    let mut best: Option<(f64, (f64, f64, f32))> = None;
    for building in buildings {
        if !building.kind.uses_slab_geometry() {
            continue;
        }
        let dims = slab_dims(building.kind);
        let near = (building.true_x, building.true_z, building.ground_y);
        let far = slab_far_edge_true(dims, building.true_x, building.true_z, building.ground_y, building.rotation_y);
        for (cx, cz, cy) in [near, far] {
            let dx = cx - hit_true_x;
            let dz = cz - hit_true_z;
            let dist_sq = dx * dx + dz * dz;
            if dist_sq > SLAB_SNAP_RADIUS * SLAB_SNAP_RADIUS {
                continue;
            }
            if best.as_ref().is_none_or(|(best_dist_sq, _)| dist_sq < *best_dist_sq) {
                best = Some((dist_sq, (cx, cz, cy)));
            }
        }
    }
    best.map(|(_, point)| point)
}

/// Converts a snapped true-space point (see `nearest_slab_snap`) back to
/// local space for the ghost/anchor — `y` is carried through directly
/// (already the exact height that point's own building settled to), only
/// `x`/`z` go through the usual true-minus-origin conversion.
fn snap_point_to_local(true_x: f64, true_z: f64, y: f32, origin: &WorldOrigin) -> Vec3 {
    let local_xz = (DVec3::new(true_x, 0.0, true_z) - origin.offset).as_vec3();
    Vec3::new(local_xz.x, y, local_xz.z)
}

/// Mouse-raycast building placement: pick a kind (see `SelectBuildingKind`,
/// fired by the build menu's buttons — `building_ui.rs`), a translucent
/// ghost then follows wherever the cursor is actually pointing in the 3D
/// world (a Rapier raycast from the camera through the cursor, same "click
/// in the world" idea a raycast-based level editor would use, not the
/// old "always at my own car's position" placement). For a
/// `BuildingKind::uses_slab_geometry` kind (Ramp, Road, Platform, Wall)
/// specifically, the initial click sets the anchor and holding+dragging
/// the mouse before releasing sets which way it extends — every other kind
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
pub(crate) enum PlacementState {
    #[default]
    Idle,
    /// Ghost follows the raycast hit; a click either places immediately
    /// (a kind that isn't `uses_slab_geometry`) or moves to `Aiming` (one
    /// that is).
    Selecting(BuildingKind),
    /// `uses_slab_geometry` kinds only: position is locked in at
    /// `anchor_local`/`anchor_true_*` from the moment the mouse went down;
    /// dragging further only changes `rotation_y`, released to confirm.
    Aiming { kind: BuildingKind, anchor_local: Vec3, anchor_true_x: f64, anchor_true_z: f64, rotation_y: f32 },
}

impl PlacementState {
    /// `selection.rs`'s click-to-select must not steal a click that's
    /// actually part of placing a new building — this is the one flag
    /// that already fully tracks "am I busy placing something."
    pub(crate) fn is_idle(&self) -> bool {
        matches!(self, PlacementState::Idle)
    }
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

/// The camera ray through wherever the cursor is currently aiming — split
/// out from the actual raycast-against-geometry step (see
/// `cursor_world_hit`/`ray_ground_plane_hit` below) so `Aiming` state
/// (choosing a slab's direction) can fall back to intersecting a plane
/// instead of requiring the ray to hit real geometry at all.
fn cursor_ray(
    mode: crate::pilot::ControlMode,
    menu_open: bool,
    windows: &Query<&Window, With<PrimaryWindow>>,
    camera_q: &Query<(&Camera, &GlobalTransform), With<CarCamera>>,
) -> Option<Ray3d> {
    let window = windows.iter().next()?;
    let cursor = crate::pilot::aim_position(mode, menu_open, window)?;
    let (camera, camera_transform) = camera_q.iter().next()?;
    camera.viewport_to_world(camera_transform, cursor).ok()
}

/// A single Rapier raycast from the camera through the cursor — the same
/// underlying idea `weapons.rs`'s hitscan and `camera.rs`'s clip-avoidance
/// already use, just aimed by the mouse instead of the car's own forward
/// vector. Excludes the local car's own collider so pointing at yourself
/// doesn't just hit your own roof. Used only to pick the *initial* spot for
/// something (`PlacementState::Selecting`) — there's real 3D geometry to
/// aim at for that, unlike choosing a already-anchored slab's direction
/// (see `ray_ground_plane_hit`).
fn cursor_world_hit(ray: Ray3d, rapier_context: &ReadRapierContext, exclude: Option<Entity>) -> Option<Vec3> {
    let context = rapier_context.single().ok()?;
    let mut filter = QueryFilter::default();
    if let Some(exclude) = exclude {
        filter = filter.exclude_rigid_body(exclude);
    }
    let (_, toi) = context.cast_ray(ray.origin, *ray.direction, 2000.0, true, filter)?;
    Some(ray.origin + *ray.direction * toi)
}

/// Intersects the cursor ray with the horizontal plane at `plane_y`
/// (the slab's own anchor height) — used instead of a real-geometry
/// raycast while `Aiming`, so dragging to set a slab's direction keeps
/// responding even while literally pointing at open sky. A raycast-only
/// approach froze the ghost/rotation in place the instant the drag
/// overshot past the horizon (nothing there for it to hit), reported live
/// as needing to drag back onto solid ground/an object before the
/// direction would update again — a plane intersection is always defined
/// for any downward or upward-but-still-over-the-horizon look direction,
/// so the drag direction keeps tracking the cursor regardless of what (if
/// anything) is actually underneath it. `None` only in the degenerate case
/// of aiming almost exactly along the horizon, where the plane is never
/// crossed at all (or only infinitely far away).
fn ray_ground_plane_hit(ray: Ray3d, plane_y: f32) -> Option<Vec3> {
    let dir_y = ray.direction.y;
    if dir_y.abs() < 1e-4 {
        return None;
    }
    let t = (plane_y - ray.origin.y) / dir_y;
    if t <= 0.0 {
        return None;
    }
    Some(ray.origin + *ray.direction * t)
}

#[allow(clippy::too_many_arguments)]
fn drive_placement(
    mut commands: Commands,
    mut state: ResMut<PlacementState>,
    mouse: Res<ButtonInput<MouseButton>>,
    keyboard: Res<ButtonInput<KeyCode>>,
    mode: Res<crate::pilot::ControlMode>,
    menu_open: Res<crate::pilot::MenuOpen>,
    windows: Query<&Window, With<PrimaryWindow>>,
    camera_q: Query<(&Camera, &GlobalTransform), With<CarCamera>>,
    rapier_context: ReadRapierContext,
    local_car_q: Query<Entity, With<LocalCar>>,
    origin: Res<WorldOrigin>,
    buildings_q: Query<&BuildingSnapshot>,
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

    let Some(ray) = cursor_ray(*mode, menu_open.0, &windows, &camera_q) else {
        return;
    };
    let Ok((_, mut ghost_transform)) = ghost_q.single_mut() else {
        return;
    };

    match &mut *state {
        PlacementState::Idle => unreachable!("checked above"),
        PlacementState::Selecting(kind) => {
            let kind = *kind;
            // Still a real raycast here — there's genuine 3D geometry to
            // pick an *initial* spot from, unlike `Aiming` below (choosing
            // an already-anchored slab's direction), which no longer needs
            // one at all.
            let exclude = local_car_q.iter().next();
            let Some(hit) = cursor_world_hit(ray, &rapier_context, exclude) else {
                return;
            };
            let hit_true = origin.to_true(hit);
            // A slab kind's ghost previews the snapped point (if any)
            // rather than the raw raycast hit, so what you see hovering is
            // exactly what clicking will actually lock in — see
            // `nearest_slab_snap`'s own docs on why this is what actually
            // fixes connected slabs stair-stepping at a seam.
            let snapped = if kind.uses_slab_geometry() {
                nearest_slab_snap(hit_true.x, hit_true.z, &buildings_q)
            } else {
                None
            };
            let (preview_local, preview_true_x, preview_true_z) = match snapped {
                Some((sx, sz, sy)) => (snap_point_to_local(sx, sz, sy, &origin), sx, sz),
                None => (hit, hit_true.x, hit_true.z),
            };
            let (_, _, transform) = building_mesh_and_transform(
                kind,
                &mut meshes,
                preview_local.x,
                preview_local.z,
                preview_local.y,
                0.0,
            );
            *ghost_transform = transform;

            if mouse.just_pressed(MouseButton::Left) {
                if kind.uses_slab_geometry() {
                    *state = PlacementState::Aiming {
                        kind,
                        anchor_local: preview_local,
                        anchor_true_x: preview_true_x,
                        anchor_true_z: preview_true_z,
                        rotation_y: 0.0,
                    };
                } else {
                    commands.client_trigger(PlaceBuildingMsg {
                        kind,
                        true_x: preview_true_x,
                        true_z: preview_true_z,
                        rotation_y: 0.0,
                    });
                    commands.entity(ghost_q.single().unwrap().0).despawn();
                    *state = PlacementState::Idle;
                }
            }
        }
        PlacementState::Aiming { kind, anchor_local, anchor_true_x, anchor_true_z, rotation_y } => {
            let kind = *kind;
            // A ground-plane intersection, not a real-geometry raycast —
            // see `ray_ground_plane_hit`'s own docs on why: choosing a
            // direction only ever needs *a* point to measure an angle
            // against, and requiring the ray to hit real geometry froze
            // this the instant the drag overshot toward open sky.
            let Some(hit) = ray_ground_plane_hit(ray, anchor_local.y) else {
                return;
            };
            let dx = hit.x - anchor_local.x;
            let dz = hit.z - anchor_local.z;
            // Only update the facing once the drag has moved far enough to
            // mean something — right at the anchor point dx/dz are ~zero
            // and atan2 of that is meaningless noise.
            if dx * dx + dz * dz > 0.25 {
                // `anchor_local` is this slab's own near edge now (see
                // `shared::buildings::slab_transform`'s docs), and it
                // extends toward local +Z rotated by `rotation_y`, which is
                // (sin(rotation_y), cos(rotation_y)) in world (x, z);
                // solving for rotation_y so that direction points along
                // the drag (dx, dz) gives atan2(dx, dz) — dragging is
                // literally "point at where you want the far edge to be."
                // If a placed slab ends up extending backward from the
                // drag, flip both signs here.
                *rotation_y = dx.atan2(dz);
            }
            let (_, _, transform) = building_mesh_and_transform(
                kind,
                &mut meshes,
                anchor_local.x,
                anchor_local.z,
                anchor_local.y,
                *rotation_y,
            );
            *ghost_transform = transform;

            if mouse.just_released(MouseButton::Left) {
                commands.client_trigger(PlaceBuildingMsg {
                    kind,
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
