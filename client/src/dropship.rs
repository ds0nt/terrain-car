use bevy::input::mouse::MouseMotion;
use bevy_replicon::prelude::ClientTriggerExt;
use bevy::prelude::*;
use shared::protocol::{CargoVehicleId, DropVehicleMsg, DropshipInputMsg, DropshipSnapshot, PickupVehicleMsg, RecallDropshipMsg};
use shared::tank_physics::TankChassis;
use uuid::Uuid;

use crate::camera::CarCamera;
use crate::car::CarChassis;
use crate::pilot::ControlMode;
use crate::thrusters::sync_thruster_glow;

/// Dropship flight input and camera — a bigger, slower `ScoutPlane` with
/// four independent passenger seats instead of none (see
/// `server::dropship_sim`'s own module docs). Flight-stick input and
/// camera shape mirror `aircraft.rs` closely; the one addition is a shared
/// camera for whoever's merely riding along, not piloting (see
/// `update_dropship_camera`), the same "passenger gets the same view a
/// driver would" shape `camera.rs`'s own `update_camera` already gives a
/// car's passenger seat.
pub struct DropshipPlugin;

impl Plugin for DropshipPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DropshipInput>()
            .init_resource::<DrivingDropshipId>()
            .init_resource::<PassengerDropshipId>()
            .add_systems(
                Update,
                (
                    tag_local_dropship,
                    reset_dropship_stick_on_enter,
                    read_dropship_input,
                    handle_pickup_key,
                    update_dropship_camera,
                    sync_dropship_thrusters,
                )
                    .chain(),
            )
            .add_systems(FixedUpdate, send_dropship_input);
    }
}

/// Marks the one replicated `DropshipSnapshot` entity the local player
/// currently pilots — same shape/reasoning as `aircraft::LocalPlane`. Not
/// applied for a passenger seat: unlike piloting, riding along never needs
/// to distinguish "your own" dropship from anyone else's (see
/// `PassengerDropshipId`'s own docs).
#[derive(Component)]
pub struct LocalDropship;

/// Which owned dropship (`DropshipSnapshot::dropship_id`) the player is
/// piloting right now — `None` outside `ControlMode::DropshipPilot`. Same
/// shape/reason as `aircraft::DrivingPlaneId`.
#[derive(Resource, Default, Clone, Copy)]
pub struct DrivingDropshipId(pub Option<Uuid>);

/// Which dropship (by id, not necessarily owned) the player currently
/// occupies a passenger seat in — `None` outside
/// `ControlMode::DropshipPassenger`. Same shape/reason as
/// `pilot::PassengerCarId`: a ridden dropship is never tagged
/// `LocalDropship` (it may not be yours), so it can't be found by that
/// marker.
#[derive(Resource, Default, Clone, Copy)]
pub struct PassengerDropshipId(pub Option<Uuid>);

/// `pub` fields for the same reason `aircraft::PlaneInput`'s are — a future
/// HUD stick-position indicator could read these directly.
#[derive(Resource, Default)]
pub struct DropshipInput {
    pub throttle: f32,
    pub yaw: f32,
    pub pitch: f32,
    pub roll: f32,
}

const MOUSE_SENSITIVITY: f32 = 0.003;
const MOUSE_DEADZONE_PX: f32 = 0.8;

/// Same retry shape `aircraft::tag_local_plane` uses.
fn tag_local_dropship(
    mut commands: Commands,
    local_player_id: Res<crate::auth_ui::LocalPlayerId>,
    dropships: Query<(Entity, &DropshipSnapshot), Without<LocalDropship>>,
) {
    let Some(player_id) = local_player_id.0 else {
        return;
    };
    for (entity, snapshot) in &dropships {
        if snapshot.owner_player_id == player_id {
            commands.entity(entity).insert(LocalDropship);
        }
    }
}

/// Same "don't inherit a deflected stick from a previous flight" reasoning
/// `aircraft::reset_plane_stick_on_enter` gives.
fn reset_dropship_stick_on_enter(mode: Res<ControlMode>, mut input: ResMut<DropshipInput>) {
    if mode.is_changed() && *mode == ControlMode::DropshipPilot {
        input.pitch = 0.0;
        input.roll = 0.0;
    }
}

/// Same accumulating mouse-stick shape `aircraft::read_plane_input` uses —
/// see that function's own docs for why pitch/roll accumulate rather than
/// snap, and why a deadzone matters here more than for an ordinary look
/// camera.
fn read_dropship_input(
    mode: Res<ControlMode>,
    menu_open: Res<crate::pilot::MenuOpen>,
    chat_open: Res<crate::chat::ChatOpen>,
    keyboard: Res<ButtonInput<KeyCode>>,
    mut mouse_motion: MessageReader<MouseMotion>,
    mut input: ResMut<DropshipInput>,
) {
    if *mode != ControlMode::DropshipPilot || chat_open.0 {
        mouse_motion.clear();
        if chat_open.0 {
            *input = DropshipInput::default();
        }
        return;
    }
    let mut throttle = 0.0;
    let mut yaw = 0.0;
    if keyboard.pressed(KeyCode::KeyW) {
        throttle += 1.0;
    }
    if keyboard.pressed(KeyCode::KeyS) {
        throttle -= 1.0;
    }
    if keyboard.pressed(KeyCode::KeyA) || keyboard.pressed(KeyCode::ArrowLeft) {
        yaw += 1.0;
    }
    if keyboard.pressed(KeyCode::KeyD) || keyboard.pressed(KeyCode::ArrowRight) {
        yaw -= 1.0;
    }
    input.throttle = throttle;
    input.yaw = yaw;

    if menu_open.0 {
        mouse_motion.clear();
        return;
    }
    let mut mouse_delta = Vec2::ZERO;
    for event in mouse_motion.read() {
        mouse_delta += event.delta;
    }
    if mouse_delta.x.abs() < MOUSE_DEADZONE_PX {
        mouse_delta.x = 0.0;
    }
    if mouse_delta.y.abs() < MOUSE_DEADZONE_PX {
        mouse_delta.y = 0.0;
    }
    input.pitch = (input.pitch - mouse_delta.y * MOUSE_SENSITIVITY).clamp(-1.0, 1.0);
    input.roll = (input.roll - mouse_delta.x * MOUSE_SENSITIVITY).clamp(-1.0, 1.0);
}

fn send_dropship_input(
    mode: Res<ControlMode>,
    input: Res<DropshipInput>,
    driving: Res<DrivingDropshipId>,
    mut commands: Commands,
) {
    if *mode != ControlMode::DropshipPilot {
        return;
    }
    let Some(dropship_id) = driving.0 else { return };
    commands.client_trigger(DropshipInputMsg {
        dropship_id,
        throttle: input.throttle,
        yaw: input.yaw,
        pitch: input.pitch,
        roll: input.roll,
    });
}

/// H: recall — called directly from `building_ui.rs`'s `send_recall_input`,
/// same "one place owns what H does" shape `tank::send_tank_recall` uses.
pub(crate) fn send_dropship_recall(commands: &mut Commands, dropship_id: Uuid) {
    commands.client_trigger(RecallDropshipMsg { dropship_id });
}

/// Matches `server::dropship_sim::PICKUP_RADIUS` — this copy only ever
/// decides *which* message to send (pick up vs. drop, and which nearby
/// target), the server independently re-checks distance itself before
/// actually acting, the same "client names its guess, server is the sole
/// authority" trust boundary every other message here already uses.
const PICKUP_RADIUS: f32 = 12.0;

/// `G` while piloting a dropship — one key, context-dependent, same shape
/// `F` already uses for vehicle boarding (see `pilot::handle_vehicle_key`'s
/// own docs): picks up the nearest eligible car/tank in range if there's a
/// free cargo slot and one to grab, otherwise drops the most recently
/// picked-up cargo (if any) instead. Preferring pickup over drop when both
/// are momentarily possible (hovering near a second vehicle while already
/// carrying one) means `G` never surprises you by dropping cargo you were
/// actually trying to add to.
#[allow(clippy::too_many_arguments)]
fn handle_pickup_key(
    keyboard: Res<ButtonInput<KeyCode>>,
    chat_open: Res<crate::chat::ChatOpen>,
    mode: Res<ControlMode>,
    driving: Res<DrivingDropshipId>,
    mut commands: Commands,
    dropships: Query<(&Transform, &DropshipSnapshot)>,
    cars: Query<(&Transform, &CarChassis)>,
    tanks: Query<(&Transform, &TankChassis)>,
) {
    if chat_open.0 || !keyboard.just_pressed(KeyCode::KeyG) || *mode != ControlMode::DropshipPilot {
        return;
    }
    let Some(driving_id) = driving.0 else { return };
    let Some((dropship_tf, snapshot)) = dropships.iter().find(|(_, s)| s.dropship_id == driving_id) else {
        return;
    };

    if snapshot.cargo.iter().any(Option::is_none) {
        let car_candidates = cars.iter().map(|(tf, c)| (tf.translation, CargoVehicleId::Car(c.car_id)));
        let tank_candidates = tanks.iter().map(|(tf, c)| (tf.translation, CargoVehicleId::Tank(c.tank_id)));
        let nearest = car_candidates
            .chain(tank_candidates)
            .filter(|(_, id)| !snapshot.cargo.contains(&Some(*id)))
            .map(|(pos, id)| (pos.distance(dropship_tf.translation), id))
            .filter(|(dist, _)| *dist < PICKUP_RADIUS)
            .min_by(|(a, _), (b, _)| a.total_cmp(b));
        if let Some((_, target)) = nearest {
            commands.client_trigger(PickupVehicleMsg { dropship_id: driving_id, target });
            return;
        }
    }

    if let Some(target) = snapshot.cargo.iter().rev().flatten().next().copied() {
        commands.client_trigger(DropVehicleMsg { dropship_id: driving_id, target });
    }
}

/// Chase-only camera for whoever's aboard a dropship — pilot or passenger
/// alike, the same "riding along shouldn't mean staring at nothing" shape
/// `camera.rs`'s own `update_camera` gives a car's passenger seat. No
/// cockpit mode for v1: unlike a Scout Plane, this is a big transport, not
/// a fighter — a first-person seat view is a reasonable follow-up once
/// there's an actual named seat to place a camera at.
fn update_dropship_camera(
    mode: Res<ControlMode>,
    driving: Res<DrivingDropshipId>,
    passenger: Res<PassengerDropshipId>,
    dropship_q: Query<(&GlobalTransform, &DropshipSnapshot)>,
    mut camera_q: Query<&mut Transform, With<CarCamera>>,
) {
    let target_id = match *mode {
        ControlMode::DropshipPilot => driving.0,
        ControlMode::DropshipPassenger => passenger.0,
        _ => return,
    };
    let Some(target_id) = target_id else {
        return;
    };
    let Some((dropship_gt, _)) = dropship_q.iter().find(|(_, snapshot)| snapshot.dropship_id == target_id) else {
        return;
    };
    let Ok(mut camera_tf) = camera_q.single_mut() else {
        return;
    };

    let transform = dropship_gt.compute_transform();
    let dropship_up = *transform.up();
    let look_target = transform.translation + dropship_up * 2.0;
    let desired = transform.translation - *transform.forward() * 16.0 + dropship_up * 6.0;
    camera_tf.translation = desired;
    camera_tf.rotation = Transform::from_translation(desired).looking_at(look_target, dropship_up).rotation;
}

/// Lights the piloted dropship's own thruster nozzles from the live
/// `DropshipInput` stick — see `thrusters.rs`'s own module docs on why
/// only the ship you're actually flying ever lights up (a passenger has no
/// input of their own to reflect either, same reasoning).
fn sync_dropship_thrusters(
    mut materials: ResMut<Assets<StandardMaterial>>,
    input: Res<DropshipInput>,
    driving: Res<DrivingDropshipId>,
    dropships: Query<(&DropshipSnapshot, &Children), With<LocalDropship>>,
    mut nozzles: crate::thrusters::NozzleQuery,
) {
    let Some(driving_id) = driving.0 else {
        return;
    };
    let Some((_, children)) = dropships.iter().find(|(snapshot, _)| snapshot.dropship_id == driving_id) else {
        return;
    };
    sync_thruster_glow(&mut materials, children, &mut nozzles, input.throttle, input.yaw, input.pitch, input.roll);
}
