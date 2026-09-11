use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use bevy_rapier3d::prelude::{QueryFilter, ReadRapierContext};
use bevy_replicon::prelude::ClientTriggerExt;
pub use shared::tank_physics::TankChassis;
use shared::protocol::TankFlipUprightMsg;
use shared::protocol::{RecallTankMsg, TankInputMsg};
use shared::tank_physics::TankInput;
use uuid::Uuid;

use crate::camera::CarCamera;
use crate::pilot::ControlMode;
use crate::worldspace::WorldOrigin;

/// Tank driving/turret-aim input and camera — see `shared::tank_physics`'s
/// own module docs for why the hull reuses a car's exact wheel-physics
/// model, and `car.rs`'s top-level shape (this module mirrors it closely:
/// same server-authoritative, no-local-prediction relationship a tank's
/// `TankSnapshot` has, same reasoning). The one genuinely new piece here is
/// turret aim: a driver points the mouse anywhere on screen and the turret
/// aims there, independent of hull steering — see `read_tank_input`.
pub struct TankPlugin;

impl Plugin for TankPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<TankInput>()
            .init_resource::<DrivingTankId>()
            .add_systems(
                Update,
                (tag_local_tank, read_tank_input, flip_tank_upright, update_tank_camera).chain(),
            )
            .add_systems(FixedUpdate, send_tank_input);
    }
}

/// Marks the one replicated `TankChassis` entity that belongs to the local
/// player — same shape/reasoning as `aircraft::LocalPlane`.
#[derive(Component)]
pub struct LocalTank;

/// Which specific owned tank (`TankChassis::tank_id`) the player is
/// actually driving right now — `None` whenever `ControlMode` isn't `Tank`.
/// Same shape and reason as `car::DrivingCarId`: a multi-tank owner shares
/// one `owner_player_id`, so this is what actually picks the one you
/// walked up to and boarded.
#[derive(Resource, Default, Clone, Copy)]
pub struct DrivingTankId(pub Option<Uuid>);

/// Same retry shape `car::tag_local_car`/`aircraft::tag_local_plane` use —
/// see either's own docs on why a one-shot insert-time check can lose the
/// replication/login race.
fn tag_local_tank(
    mut commands: Commands,
    local_player_id: Res<crate::auth_ui::LocalPlayerId>,
    tanks: Query<(Entity, &TankChassis), Without<LocalTank>>,
) {
    let Some(player_id) = local_player_id.0 else {
        return;
    };
    for (entity, chassis) in &tanks {
        if chassis.owner_player_id == player_id {
            commands.entity(entity).insert(LocalTank);
        }
    }
}

/// A single Rapier raycast from the camera through wherever the cursor
/// currently is — same underlying idea `building_placement.rs`'s
/// `cursor_world_hit` uses for placement aiming, duplicated here in
/// miniature rather than making that module's private helper `pub(crate)`
/// for one small, self-contained use: this is a generic "where does the
/// mouse point in the world" utility, not vehicle-specific logic that
/// belongs in a shared vehicles module.
fn cursor_world_hit(
    mode: ControlMode,
    menu_open: bool,
    windows: &Query<&Window, With<PrimaryWindow>>,
    camera_q: &Query<(&Camera, &GlobalTransform), With<CarCamera>>,
    rapier_context: &ReadRapierContext,
    exclude: Entity,
) -> Option<Vec3> {
    let window = windows.iter().next()?;
    let cursor = crate::pilot::aim_position(mode, menu_open, window)?;
    let (camera, camera_transform) = camera_q.iter().next()?;
    let ray = camera.viewport_to_world(camera_transform, cursor).ok()?;
    let context = rapier_context.single().ok()?;
    let hit = context.cast_ray(ray.origin, *ray.direction, 2000.0, true, QueryFilter::new().exclude_rigid_body(exclude));
    let toi = match hit {
        Some((_, toi)) => toi,
        // No real geometry under the cursor (aiming at open sky) — fall
        // back to a horizontal plane at the tank's own height, same
        // "always defined, so aiming never just freezes" reasoning
        // `building_placement.rs`'s own `ray_ground_plane_hit` docs give.
        None => {
            let dir_y = ray.direction.y;
            if dir_y.abs() < 1e-4 {
                return None;
            }
            (0.0 - ray.origin.y) / dir_y
        }
    };
    if toi <= 0.0 {
        return None;
    }
    Some(ray.origin + *ray.direction * toi)
}

/// WASD drives the hull exactly like a car (`car::read_car_input`); the
/// turret aims independently, at whatever point on screen the free cursor
/// (`pilot::manage_cursor_confinement` frees it in `Tank` mode — see that
/// function's own docs) is currently over, converted to a world-space yaw
/// via `cursor_world_hit`. No steer-vs-aim conflict: driving faces the hull
/// wherever WASD points it, the turret keeps tracking the cursor
/// regardless.
#[allow(clippy::too_many_arguments)]
fn read_tank_input(
    mode: Res<ControlMode>,
    chat_open: Res<crate::chat::ChatOpen>,
    menu_open: Res<crate::pilot::MenuOpen>,
    keyboard: Res<ButtonInput<KeyCode>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    camera_q: Query<(&Camera, &GlobalTransform), With<CarCamera>>,
    rapier_context: ReadRapierContext,
    tank_q: Query<(Entity, &Transform), With<LocalTank>>,
    mut input: ResMut<TankInput>,
) {
    if *mode != ControlMode::Tank {
        return;
    }
    if chat_open.0 {
        *input = TankInput { throttle: 0.0, steer: 0.0, brake: true, turret_yaw: input.turret_yaw };
        return;
    }
    let mut throttle = 0.0;
    let mut steer = 0.0;
    if keyboard.pressed(KeyCode::KeyW) || keyboard.pressed(KeyCode::ArrowUp) {
        throttle += 1.0;
    }
    if keyboard.pressed(KeyCode::KeyS) || keyboard.pressed(KeyCode::ArrowDown) {
        throttle -= 1.0;
    }
    if keyboard.pressed(KeyCode::KeyA) || keyboard.pressed(KeyCode::ArrowLeft) {
        steer += 1.0;
    }
    if keyboard.pressed(KeyCode::KeyD) || keyboard.pressed(KeyCode::ArrowRight) {
        steer -= 1.0;
    }
    input.throttle = throttle;
    input.steer = steer;
    input.brake = keyboard.pressed(KeyCode::Space);

    let Ok((tank_entity, tank_tf)) = tank_q.single() else {
        return;
    };
    if let Some(hit) = cursor_world_hit(*mode, menu_open.0, &windows, &camera_q, &rapier_context, tank_entity) {
        let to_hit = hit - tank_tf.translation;
        // 0 faces local `+Z`, increasing toward `+X` — see
        // `shared::tank_physics::turret_muzzle_offset`'s own docs on this
        // convention (chosen fresh for the turret, not the hull's own
        // `-Z`-forward one, to match `bearing_to`/`GunFiredMsg` math with no
        // sign flip needed anywhere it's used).
        input.turret_yaw = to_hit.x.atan2(to_hit.z);
    }
}

fn send_tank_input(mode: Res<ControlMode>, input: Res<TankInput>, driving: Res<DrivingTankId>, mut commands: Commands) {
    if *mode != ControlMode::Tank {
        return;
    }
    let Some(tank_id) = driving.0 else { return };
    commands.client_trigger(TankInputMsg {
        tank_id,
        throttle: input.throttle,
        steer: input.steer,
        brake: input.brake,
        turret_yaw: input.turret_yaw,
    });
}

/// R: same "ask the server to right it exactly where it already is, no
/// local guess" shape `car::flip_car_upright` uses.
fn flip_tank_upright(
    keyboard: Res<ButtonInput<KeyCode>>,
    chat_open: Res<crate::chat::ChatOpen>,
    origin: Res<WorldOrigin>,
    tank_q: Query<(&Transform, &TankChassis), With<LocalTank>>,
    mut commands: Commands,
) {
    if chat_open.0 || !keyboard.just_pressed(KeyCode::KeyR) {
        return;
    }
    let Some((transform, chassis)) = tank_q.iter().next() else {
        return;
    };
    let true_pos = origin.to_true(transform.translation);
    commands.client_trigger(TankFlipUprightMsg { tank_id: chassis.tank_id, true_x: true_pos.x, true_z: true_pos.z });
}

/// H: recall — kept here rather than folded into `building_ui.rs`'s
/// `send_recall_input` only because that system doesn't otherwise depend on
/// this module; see that function's own docs for the car/plane recall
/// this mirrors. `pub(crate)` and called directly from there instead of
/// being its own separate `H`-gated system, so there's exactly one place
/// that owns "what does `H` do" rather than several independently checking
/// the same key.
pub(crate) fn send_tank_recall(commands: &mut Commands, tank_id: Uuid) {
    commands.client_trigger(RecallTankMsg { tank_id });
}

/// Chase-only camera while driving a tank — no cockpit mode for v1 (a
/// tank's driver view would mostly just be hull armor at close range, far
/// less useful a placeholder than a car's actual windshield-height seat);
/// `camera.rs`'s own `update_camera` already skips entirely outside
/// `Car`/`Passenger` (see its own match), so this never fights it for the
/// same `CarCamera` transform.
fn update_tank_camera(
    mode: Res<ControlMode>,
    driving: Res<DrivingTankId>,
    tank_q: Query<(&GlobalTransform, &TankChassis), With<LocalTank>>,
    mut camera_q: Query<&mut Transform, With<CarCamera>>,
) {
    if *mode != ControlMode::Tank {
        return;
    }
    let Some(driving_id) = driving.0 else {
        return;
    };
    let Some((tank_gt, _)) = tank_q.iter().find(|(_, chassis)| chassis.tank_id == driving_id) else {
        return;
    };
    let Ok(mut camera_tf) = camera_q.single_mut() else {
        return;
    };
    let transform = tank_gt.compute_transform();
    let look_target = transform.translation + Vec3::Y * 1.5;
    let desired = transform.translation - *transform.forward() * 11.0 + Vec3::Y * 5.0;
    camera_tf.translation = desired;
    camera_tf.rotation = Transform::from_translation(desired).looking_at(look_target, Vec3::Y).rotation;
}
