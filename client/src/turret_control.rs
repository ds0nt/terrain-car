use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use bevy_rapier3d::prelude::{QueryFilter, ReadRapierContext};
use bevy_replicon::prelude::ClientTriggerExt;
use shared::protocol::TurretAimMsg;
use shared::tank_physics::turret_aim_direction;
use uuid::Uuid;

use crate::camera::CarCamera;
use crate::pilot::ControlMode;

/// Manual turret aim/camera — see `shared::protocol::TurretSnapshot::
/// occupant_player_id`'s own docs for the server-side occupancy model this
/// reacts to, and `pilot::handle_turret_key` for the actual enter/exit
/// flow. Aiming reuses the exact same "raycast the free cursor against the
/// world, fall back to a ground plane" technique `tank::read_tank_input`
/// already uses for a tank's turret — this is genuinely the same "point a
/// turret head at the mouse" mechanism, just for a stationary mount instead
/// of one riding on a moving hull.
///
/// Aim is computed once per *render* frame (`update_local_turret_aim`, in
/// `Update`) into `LocalTurretAim`, not directly inside the network-tick-
/// rate `send_turret_aim` (`FixedUpdate`) — see that resource's own docs on
/// why: this is what lets the camera and the operator's own turret-head
/// mesh (`building_render::sync_turret_heads`) both read a value that's
/// always fresh as of *this* frame, instead of only updating at the
/// server's own tick rate. Reading the *replicated* `TurretSnapshot`
/// for the operator's own view (the original shape here) was the actual
/// cause of a real reported bug: it made both the camera and the head
/// visibly lag/stutter a network round-trip behind the operator's own
/// already-instant mouse movement — jitter, not really "replicon fighting
/// for control" in the sense of two writers disagreeing, but the same
/// underlying class of issue (rendering a remote-authoritative value for
/// something that has a perfectly good local source of truth).
pub struct TurretControlPlugin;

impl Plugin for TurretControlPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<OccupiedTurretId>()
            .init_resource::<OccupiedTurretLocal>()
            .init_resource::<LocalTurretAim>()
            .add_systems(Update, (update_local_turret_aim, update_turret_camera).chain())
            .add_systems(FixedUpdate, send_turret_aim);
    }
}

/// Which owned turret (`BuildingSnapshot::id`) the player is currently
/// manually operating — `None` whenever `ControlMode` isn't
/// `TurretOperator`. Set by `pilot::handle_turret_key` on entry, cleared
/// on exit — same shape/reason as every other `DrivingXId` in this game.
#[derive(Resource, Default, Clone, Copy)]
pub struct OccupiedTurretId(pub Option<Uuid>);

/// The occupied turret's own local-space position, cached once at entry —
/// see `pilot::sync_player_focus`'s own docs on why this is cached rather
/// than re-derived from a `BuildingSnapshot` query every frame (a turret
/// never moves, and that function is already at Bevy's `SystemParam`
/// parameter limit).
#[derive(Resource, Default, Clone, Copy)]
pub struct OccupiedTurretLocal(pub Vec3);

/// The local operator's own live aim, recomputed every render frame — see
/// this module's own top-level docs on why both the camera
/// (`update_turret_camera`) and the operator's own turret-head mesh
/// (`building_render::sync_turret_heads`, which reads this via
/// `OccupiedTurretId` rather than the replicated snapshot for whichever
/// turret matches it) use this instead of waiting for the server's own
/// echo back.
#[derive(Resource, Default, Clone, Copy)]
pub struct LocalTurretAim {
    pub yaw: f32,
    pub pitch: f32,
}

/// Same miniature "raycast the cursor, fall back to a ground plane at the
/// shooter's own height" helper `tank::cursor_world_hit` duplicates from
/// `building_placement.rs`'s private equivalents — see that function's own
/// docs on why duplicating this small, generic utility beats exporting it.
fn cursor_world_hit(
    mode: ControlMode,
    menu_open: bool,
    windows: &Query<&Window, With<PrimaryWindow>>,
    camera_q: &Query<(&Camera, &GlobalTransform), With<CarCamera>>,
    rapier_context: &ReadRapierContext,
    ground_y: f32,
) -> Option<Vec3> {
    let window = windows.iter().next()?;
    let cursor = crate::pilot::aim_position(mode, menu_open, window)?;
    let (camera, camera_transform) = camera_q.iter().next()?;
    let ray = camera.viewport_to_world(camera_transform, cursor).ok()?;
    if let Ok(context) = rapier_context.single()
        && let Some((_, toi)) = context.cast_ray(ray.origin, *ray.direction, 2000.0, true, QueryFilter::default())
    {
        return Some(ray.origin + *ray.direction * toi);
    }
    let dir_y = ray.direction.y;
    if dir_y.abs() < 1e-4 {
        return None;
    }
    let t = (ground_y - ray.origin.y) / dir_y;
    if t <= 0.0 {
        return None;
    }
    Some(ray.origin + *ray.direction * t)
}

/// Recomputes `LocalTurretAim` from the cursor every render frame — see
/// this module's own top-level docs on why this is `Update`, not
/// `FixedUpdate`.
#[allow(clippy::too_many_arguments)]
fn update_local_turret_aim(
    mode: Res<ControlMode>,
    menu_open: Res<crate::pilot::MenuOpen>,
    occupied: Res<OccupiedTurretId>,
    occupied_local: Res<OccupiedTurretLocal>,
    windows: Query<&Window, With<PrimaryWindow>>,
    camera_q: Query<(&Camera, &GlobalTransform), With<CarCamera>>,
    rapier_context: ReadRapierContext,
    mut local_aim: ResMut<LocalTurretAim>,
) {
    if *mode != ControlMode::TurretOperator || occupied.0.is_none() {
        return;
    }
    let Some(hit) = cursor_world_hit(*mode, menu_open.0, &windows, &camera_q, &rapier_context, occupied_local.0.y)
    else {
        return;
    };
    let to_hit = hit - occupied_local.0;
    // Same "0 faces local +Z" convention every turret-yaw value in this
    // game already uses — see `shared::tank_physics::turret_muzzle_offset`'s
    // own docs.
    let horizontal = (to_hit.x * to_hit.x + to_hit.z * to_hit.z).sqrt();
    local_aim.yaw = to_hit.x.atan2(to_hit.z);
    local_aim.pitch = to_hit.y.atan2(horizontal.max(0.01));
}

/// Sends the occupant's live mouse-aim (`LocalTurretAim`, already computed
/// this frame — see this module's own docs) every `FixedUpdate` tick — the
/// server applies it directly with no turn-rate limit, only a hardware
/// elevation clamp (see `TurretAimMsg`'s own docs).
fn send_turret_aim(mode: Res<ControlMode>, occupied: Res<OccupiedTurretId>, local_aim: Res<LocalTurretAim>, mut commands: Commands) {
    if *mode != ControlMode::TurretOperator {
        return;
    }
    let Some(building_id) = occupied.0 else {
        return;
    };
    commands.client_trigger(TurretAimMsg { building_id, aim_yaw: local_aim.yaw, aim_pitch: local_aim.pitch });
}

/// Close, slightly elevated chase view looking along the turret's current
/// aim — a manually-aimed defense emplacement reads better from just
/// behind/above its own head than from the tank-style distance a moving
/// vehicle's chase cam uses, since there's no hull movement to need room
/// for. Uses `LocalTurretAim` directly, not the replicated snapshot — see
/// this module's own top-level docs on why.
fn update_turret_camera(
    mode: Res<ControlMode>,
    occupied: Res<OccupiedTurretId>,
    occupied_local: Res<OccupiedTurretLocal>,
    local_aim: Res<LocalTurretAim>,
    mut camera_q: Query<&mut Transform, With<CarCamera>>,
) {
    if *mode != ControlMode::TurretOperator || occupied.0.is_none() {
        return;
    }
    let Ok(mut camera_tf) = camera_q.single_mut() else {
        return;
    };

    let head_pos = occupied_local.0 + Vec3::Y * 2.0;
    let aim_dir = turret_aim_direction(local_aim.yaw, local_aim.pitch);
    let desired = head_pos - aim_dir * 8.0 + Vec3::Y * 3.0;
    camera_tf.translation = desired;
    camera_tf.rotation = Transform::from_translation(desired).looking_at(head_pos + aim_dir * 5.0, Vec3::Y).rotation;
}
