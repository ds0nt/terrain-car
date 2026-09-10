use bevy::prelude::*;
use bevy_replicon::prelude::ClientTriggerExt;
pub use shared::car_physics::{CarChassis, CarInput};
pub use shared::protocol::LocalCar;
use shared::protocol::CarInputMsg;
use uuid::Uuid;

use crate::terrain::RegenerateWorldEvent;
use crate::worldspace::WorldOrigin;

pub struct CarPlugin;

impl Plugin for CarPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CarInput>()
            .init_resource::<DrivingCarId>()
            .add_message::<CarResetEvent>()
            .add_systems(Update, (tag_local_car, read_car_input, flip_car_upright, reset_car_after_regen).chain())
            // Sends whatever `read_car_input` gathered this tick — `FixedUpdate`
            // so it goes out at a steady network rate regardless of render
            // framerate, the same reasoning `aircraft.rs`'s `send_plane_input`
            // already uses. There's no local physics step to order this
            // against anymore (see this module's top-level docs on why): a
            // car is server-authoritative only now, exactly like a plane.
            .add_systems(FixedUpdate, send_car_input);
    }
}

/// Which specific owned car (`CarChassis::car_id`) the player is actually
/// sitting in right now — `None` whenever `ControlMode` isn't `Car`. Set
/// by `pilot::handle_vehicle_key` on boarding (the *nearest* car within
/// `ENTER_RADIUS`, not just any owned one) and cleared on exit.
///
/// Exists because `owner_player_id` alone can't answer "which one" for a
/// multi-car owner — `camera.rs`'s chase cam, `hud.rs`'s health readout,
/// and this module's own `send_car_input` all used to just grab whichever
/// owned car happened to be first in iteration order, which silently
/// diverged from whichever car you'd actually walked up to and boarded
/// the moment you owned more than one — reported live as "when I join a
/// car it should be the correct car."
#[derive(Resource, Default, Clone, Copy)]
pub struct DrivingCarId(pub Option<Uuid>);

/// Fired the frame a reset message is sent, so the camera can snap to
/// wherever the car ends up instead of smoothly chasing what — until the
/// server's reply actually arrives and replicates back — still looks like
/// the car sitting exactly where it was.
#[derive(Message)]
pub struct CarResetEvent;

/// Skips entirely unless actively driving (`pilot::ControlMode::Car`) —
/// `pilot.rs`'s `handle_vehicle_key` already sent a final zeroed/braked
/// `CarInputMsg` the moment you stepped out (`F`), and leaving this system
/// running would immediately overwrite the *local* `CarInput` resource with
/// whatever WASD state exists (now meant for the plane or your own on-foot
/// walk instead) — harmless on its own since nothing reads `CarInput`
/// locally to move anything anymore, but `send_car_input` would then start
/// re-sending that stale WASD state to the server every tick, right back
/// into the exact bug `handle_vehicle_key`'s final message was sent to
/// prevent.
fn read_car_input(
    mode: Res<crate::pilot::ControlMode>,
    chat_open: Res<crate::chat::ChatOpen>,
    keyboard: Res<ButtonInput<KeyCode>>,
    mut input: ResMut<CarInput>,
) {
    if *mode != crate::pilot::ControlMode::Car {
        return;
    }
    // Brake to a stop instead of just no longer reading WASD — leaving
    // whatever throttle/steer was last held would keep being re-sent every
    // tick (see `send_car_input`'s own docs on why nothing else ever times
    // that out), reading as the car speeding off on its own the moment you
    // start typing a chat message mid-drive.
    if chat_open.0 {
        *input = CarInput { throttle: 0.0, steer: 0.0, brake: true, boost: false };
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
    input.boost = keyboard.pressed(KeyCode::ShiftLeft) || keyboard.pressed(KeyCode::ShiftRight);
}

/// Sends the current `CarInput` to the server every `FixedUpdate` tick
/// while actively driving — the only place car input actually goes
/// anywhere now that there's no local physics/prediction to also feed (see
/// this module's top-level docs). Mirrors `aircraft.rs`'s
/// `send_plane_input` exactly, including gating on mode: while flying or on
/// foot there's nothing meaningful to send, and continuing to send the
/// last-set brake state every tick would just be wasted traffic (the
/// server already keeps re-applying whatever it last received, forever,
/// same as it does for a plane).
fn send_car_input(
    mode: Res<crate::pilot::ControlMode>,
    input: Res<CarInput>,
    driving: Res<DrivingCarId>,
    mut commands: Commands,
) {
    if *mode != crate::pilot::ControlMode::Car {
        return;
    }
    let Some(car_id) = driving.0 else { return };
    commands.client_trigger(CarInputMsg {
        car_id,
        throttle: input.throttle,
        steer: input.steer,
        brake: input.brake,
        boost: input.boost,
    });
}

/// Tags a newly-replicated car as `LocalCar` once its ownership can be
/// checked — every car the player owns, not just one (full parity with
/// `aircraft.rs`'s `tag_local_plane`, which already does the same for
/// planes: a player can own several, exactly like several planes, now that
/// a car carries no local-prediction singleton assumption to break). A
/// retried `Update` system, not a one-shot `On<Insert, CarChassis>`
/// observer: replication and the `AuthResultMsg` this client's own
/// `LocalPlayerId` comes from travel over separate renet channels with no
/// ordering guarantee, so an insert-time-only check can silently lose that
/// race and never tag the car at all (see `player_account.rs`'s
/// `tag_local_player_account`, which hit exactly this live).
/// `Without<LocalCar>` keeps the retry cheap (a tagged car, or someone
/// else's, is skipped every frame).
fn tag_local_car(
    mut commands: Commands,
    local_player_id: Res<crate::auth_ui::LocalPlayerId>,
    cars: Query<(Entity, &CarChassis), Without<LocalCar>>,
) {
    let Some(player_id) = local_player_id.0 else {
        return;
    };
    for (entity, chassis) in &cars {
        if chassis.owner_player_id == player_id {
            commands.entity(entity).insert(LocalCar);
        }
    }
}

/// R: ask the server to right *a* car exactly where it already is — no
/// local guess, no local Transform/Velocity write at all now (see this
/// module's top-level docs on why a car is server-authoritative only).
/// Picks whichever owned car happens to be first in iteration order when
/// you own more than one — the same "arbitrary but consistent, not an
/// error" tolerance `aircraft.rs`'s multi-plane systems already accept,
/// rather than a `.single()` that would silently do nothing at all for
/// anyone who's ever built a second Hangar.
fn flip_car_upright(
    keyboard: Res<ButtonInput<KeyCode>>,
    chat_open: Res<crate::chat::ChatOpen>,
    origin: Res<WorldOrigin>,
    chassis_q: Query<&Transform, With<LocalCar>>,
    mut reset_events: MessageWriter<CarResetEvent>,
    mut commands: Commands,
) {
    if chat_open.0 || !keyboard.just_pressed(KeyCode::KeyR) {
        return;
    }
    let Some(transform) = chassis_q.iter().next() else {
        return;
    };

    let true_pos = origin.to_true(transform.translation);
    reset_events.write(CarResetEvent);
    commands.client_trigger(shared::protocol::FlipUprightMsg {
        true_x: true_pos.x,
        true_z: true_pos.z,
    });
}

/// Handles a `RegenerateWorldEvent` (N, see terrain.rs): asks the server to
/// find fresh flat ground and reposition *a* car there — same "server does
/// the actual work, client just asks and waits for the replicated result"
/// shape `flip_car_upright` uses, and the same "first owned car, not an
/// error if there's more than one" tolerance.
fn reset_car_after_regen(
    origin: Res<WorldOrigin>,
    mut regenerated: MessageReader<RegenerateWorldEvent>,
    chassis_q: Query<(), With<LocalCar>>,
    mut reset_events: MessageWriter<CarResetEvent>,
    mut commands: Commands,
) {
    if regenerated.read().next().is_none() {
        return;
    }
    if chassis_q.iter().next().is_none() {
        return;
    }
    reset_events.write(CarResetEvent);
    commands.client_trigger(shared::protocol::CarResetMsg {
        near_true_x: origin.offset.x,
        near_true_z: origin.offset.z,
    });
}
