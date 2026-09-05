use std::collections::VecDeque;

use bevy::prelude::*;
use bevy_rapier3d::prelude::Velocity;
use bevy_replicon::prelude::ClientTriggerExt;
use shared::car_physics::CarInput;
use shared::protocol::{CarInputMsg, CarSnapshot, LocalCar};

/// How many past ticks' predicted state to keep — generous margin over any
/// realistic RTT (a few hundred ms at 64Hz is a few dozen ticks; 128 covers
/// ~2s, comfortably past what reconciliation ever needs to look back).
const HISTORY_CAP: usize = 128;
/// Position error below this blends in smoothly over several snapshots —
/// normal float drift / minor desync, never visible as a pop.
const SMALL_ERROR_METERS: f32 = 0.5;
const SOFT_CORRECTION_FACTOR: f32 = 0.2;
/// A real desync (e.g. a collision the client didn't predict) still isn't
/// teleported — corrected faster than the soft case, but as a glide over a
/// handful of snapshots rather than an instant snap. See prediction.rs
/// module docs for why this isn't full input-replay reconciliation.
const HARD_CORRECTION_FACTOR: f32 = 0.6;

/// Client-side prediction with server reconciliation for the local
/// player's own car (see the multiplayer plan's "Networking model"
/// section). The local car already runs full local physics unconditionally
/// (car.rs's `car_suspension_and_drive`, same system single-player used) —
/// that alone *is* the prediction, giving zero perceived input latency.
/// This module adds the other two pieces: telling the server what input
/// produced that prediction, and correcting drift against what the server
/// says actually happened.
///
/// Deliberately not full input-replay reconciliation (predict, then on
/// correction snap to the server's state and re-simulate every buffered
/// input since to catch back up): that requires an integrator that
/// reproduces Rapier's own rigid-body integration exactly (mass, inertia
/// tensor, substep behavior) outside of Rapier itself — getting that even
/// slightly wrong would make prediction *itself* an ongoing source of
/// drift, worse than not predicting at all. Instead this nudges the
/// already-Rapier-simulated `Transform`/`Velocity` toward the server's
/// snapshot directly; Rapier's next step continues cleanly from wherever
/// that lands, the same way it would after any other externally-applied
/// correction (an impulse, a teleport). A real desync is corrected over a
/// handful of snapshots rather than instantly, so it reads as a firm glide
/// rather than a rubber-band snap — not literally invisible the way a
/// correct replay would be, but never a jarring teleport either.
pub struct PredictionPlugin;

impl Plugin for PredictionPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<InputSequence>()
            .init_resource::<PredictionHistory>()
            .init_resource::<LocalResetGeneration>()
            .add_systems(FixedUpdate, send_input_and_record)
            .add_systems(Update, reconcile_with_server);
    }
}

#[derive(Resource, Default)]
struct InputSequence(u32);

/// The `CarSnapshot::reset_generation` the local car last reset to (see
/// car.rs's `reset_car`, which bumps this at the same moment it sends a
/// `CarResetMsg`). Snapshots older than this are recognized as stale
/// pre-reset data rather than a real desync — see `CarSnapshot`'s and this
/// module's own docs.
#[derive(Resource, Default)]
pub struct LocalResetGeneration(pub u32);

#[derive(Clone, Copy)]
struct PredictedState {
    translation: Vec3,
}

#[derive(Resource, Default)]
struct PredictionHistory(VecDeque<(u32, PredictedState)>);

/// Sends this tick's input to the server and records the local car's
/// resulting predicted position under the same sequence number, for
/// `reconcile_with_server` to later compare against the server's echo of
/// that same sequence. Runs in `FixedUpdate` alongside
/// `car_suspension_and_drive`, after it (see car.rs's system ordering) so
/// the recorded position reflects this tick's simulated result.
fn send_input_and_record(
    mut commands: Commands,
    input: Res<CarInput>,
    mut sequence: ResMut<InputSequence>,
    mut history: ResMut<PredictionHistory>,
    car_q: Query<&Transform, With<LocalCar>>,
) {
    let seq = sequence.0;
    sequence.0 = sequence.0.wrapping_add(1);

    commands.client_trigger(CarInputMsg {
        sequence: seq,
        throttle: input.throttle,
        steer: input.steer,
        brake: input.brake,
    });

    if let Ok(transform) = car_q.single() {
        history.0.push_back((
            seq,
            PredictedState {
                translation: transform.translation,
            },
        ));
        if history.0.len() > HISTORY_CAP {
            history.0.pop_front();
        }
    }
}

/// Reconciles the local car against the server's latest snapshot for it —
/// see this module's docs for the smooth-blend (not replay) approach.
fn reconcile_with_server(
    prediction_history: Res<PredictionHistory>,
    local_reset: Res<LocalResetGeneration>,
    mut car_q: Query<
        (&CarSnapshot, &mut Transform, &mut Velocity),
        (With<LocalCar>, Changed<CarSnapshot>),
    >,
) {
    let Ok((snapshot, mut transform, mut velocity)) = car_q.single_mut() else {
        return;
    };

    // Stale snapshot from before our last local reset (R key) — arrived
    // late purely due to network latency, not evidence of a desync. See
    // `CarSnapshot::reset_generation`'s docs.
    if snapshot.reset_generation < local_reset.0 {
        return;
    }

    let predicted = prediction_history
        .0
        .iter()
        .find(|(seq, _)| *seq == snapshot.last_input_sequence)
        .map(|(_, state)| *state);

    let Some(predicted) = predicted else {
        // No history for this sequence (just connected, or a big hitch) —
        // nothing to blend against, trust the server outright.
        transform.translation = snapshot.translation;
        transform.rotation = snapshot.rotation;
        velocity.linear = snapshot.linear_velocity;
        velocity.angular = snapshot.angular_velocity;
        return;
    };

    let error = snapshot.translation - predicted.translation;
    let correction = if error.length() < SMALL_ERROR_METERS {
        SOFT_CORRECTION_FACTOR
    } else {
        HARD_CORRECTION_FACTOR
    };

    transform.translation += error * correction;
    transform.rotation = transform.rotation.slerp(snapshot.rotation, correction);
    velocity.linear = velocity.linear.lerp(snapshot.linear_velocity, correction);
    velocity.angular = velocity.angular.lerp(snapshot.angular_velocity, correction);
}
