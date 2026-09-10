use bevy::math::DVec3;
use bevy::prelude::*;
use bevy_replicon::prelude::*;
use shared::car_physics::{CarChassis, CarInput, CarInputState};
use shared::protocol::{PlaneSnapshot, SetCarPatrolMsg, SetPlanePatrolMsg};
use shared::terrain_gen::{find_flat_spawn, height_at, TerrainNoise};
use shared::worldspace::WorldOrigin;
use uuid::Uuid;

use crate::aircraft::{spawn_plane_at, PlaneInputState};
use crate::car_sim::spawn_car_for;

/// A from-scratch, deliberately dumb "brain" for an unowned, AI-controlled
/// vehicle — the user's own framing for this v1: hardcode something that
/// drives/flies toward one patrol point, using the *exact same*
/// `CarInput`/`PlaneInputState` surface a real player's own input already
/// drives (see `car_sim::apply_car_input`/`aircraft::apply_plane_input`).
/// `step_cars`/`fly_planes` need zero changes at all to also move an
/// AI-driven vehicle — from their point of view this is just another
/// source of input, exactly the same way the server console's own
/// commands are just another source of authority alongside a real client.
///
/// Deliberately narrow scope for this starter: one hardcoded patrol point,
/// one AI car (drives to it, brakes, stops), one AI plane (flies to it,
/// then orbits it indefinitely rather than trying to land/stop). No
/// obstacle avoidance, no multi-point routes, no ownership/UI to assign a
/// patrol point yet — a later, smarter brain (even eventually a learned
/// one) only ever needs to replace `drive_ai_cars`/`fly_ai_planes`;
/// everything downstream of `CarInput`/`PlaneInputState` stays identical
/// either way.
pub struct AiPlugin;

impl Plugin for AiPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_patrol_ai)
            .add_systems(FixedUpdate, (drive_ai_cars, fly_ai_planes))
            .add_observer(apply_set_car_patrol)
            .add_observer(apply_set_plane_patrol);
    }
}

/// No player owns an AI-controlled vehicle. Every ownership check in this
/// game (`apply_car_input`, `apply_recall_to_hangar`, the players list,
/// `player_markers.rs`'s nametags, ...) keys off `PlayerIdentities`/
/// `owner_player_id` matching a real logged-in player, which the nil UUID
/// can never do — so an AI vehicle is simply invisible to every one of
/// those paths (no floating nametag, can't be recalled by a player, etc.)
/// without needing its own special-cased exclusion anywhere.
const AI_OWNER: Uuid = Uuid::nil();

/// True-space patrol point every AI vehicle heads for — hardcoded for this
/// starter (see this module's own top-level docs). A real "assign a
/// patrol route" feature (per-unit, player- or building-owned, more than
/// one point) is a reasonable follow-up once this basic pipeline is
/// confirmed working.
const PATROL_TRUE_X: f64 = 60.0;
const PATROL_TRUE_Z: f64 = 60.0;

/// How close (true-space meters) counts as "arrived," for the car — it
/// brakes to a stop rather than trying to hit the point exactly.
const CAR_ARRIVE_RADIUS: f64 = 6.0;
const CAR_STEER_GAIN: f32 = 1.6;

/// The plane instead orbits once within this radius, rather than
/// stopping — a plane in this flight model can't usefully hover in place
/// (see `aircraft::fly_planes`'s own docs on why there's no separate lift
/// term), so circling is the closest sensible thing to "loiter here."
const PLANE_ORBIT_RADIUS: f64 = 40.0;
const PLANE_YAW_GAIN: f32 = 1.4;
/// Fixed turn rate while orbiting — a constant yaw input plus steady
/// forward thrust traces a circle around the target on its own, no actual
/// "am I still pointed at the center" steering needed once inside
/// `PLANE_ORBIT_RADIUS`.
const PLANE_ORBIT_YAW: f32 = 0.5;
const PLANE_CRUISE_THROTTLE: f32 = 0.7;
/// Simple proportional altitude hold — see `fly_ai_planes`'s own docs on
/// why this is a rate controller, not a true attitude hold, and why
/// that's good enough here.
const PLANE_PITCH_GAIN: f32 = 0.15;

#[derive(Component)]
struct AiCarPatrol {
    target_true_x: f64,
    target_true_z: f64,
}

#[derive(Component)]
struct AiPlanePatrol {
    target_true_x: f64,
    target_true_z: f64,
    /// Captured once at spawn (local-space Y, never affected by a
    /// `WorldOrigin` rebase — see `GunFiredMsg::y`'s own docs on why this
    /// field is safe to compare directly against `Transform::translation.y`
    /// on any later tick) — the altitude `fly_ai_planes` tries to hold
    /// while en route and while orbiting.
    cruise_altitude: f32,
}

fn spawn_patrol_ai(mut commands: Commands, noise: Res<TerrainNoise>, origin: Res<WorldOrigin>) {
    let car_spawn_true = find_flat_spawn(&noise, DVec3::new(0.0, 0.0, 0.0), 300.0);
    let car_ground_y = height_at(&noise, car_spawn_true.x, car_spawn_true.z);
    let car_entity =
        spawn_car_for(&mut commands, &origin, AI_OWNER, car_spawn_true.x, car_spawn_true.z, car_ground_y + 2.0);
    commands
        .entity(car_entity)
        .insert(AiCarPatrol { target_true_x: PATROL_TRUE_X, target_true_z: PATROL_TRUE_Z });

    // Spawned a little to the side of the car's own point so the two
    // don't start out overlapping.
    let plane_spawn_true = DVec3::new(car_spawn_true.x + 20.0, 0.0, car_spawn_true.z);
    let plane_ground_y = height_at(&noise, plane_spawn_true.x, plane_spawn_true.z);
    let plane_altitude = plane_ground_y + 25.0;
    let plane_entity = spawn_plane_at(
        &mut commands,
        &origin,
        AI_OWNER,
        plane_spawn_true.x,
        plane_spawn_true.z,
        plane_altitude,
    );
    commands.entity(plane_entity).insert(AiPlanePatrol {
        target_true_x: PATROL_TRUE_X,
        target_true_z: PATROL_TRUE_Z,
        cruise_altitude: plane_altitude,
    });

    info!("server: spawned patrol AI (car + plane) heading for true ({PATROL_TRUE_X}, {PATROL_TRUE_Z})");
}

/// Redirects an AI car's patrol point — right-click while it's selected
/// (see client's `selection.rs`). Requires `chassis.owner_player_id ==
/// AI_OWNER`: a real player's own `car_id` is visible to every client via
/// ordinary replication, so without this check anyone could name *any*
/// car here and silently take over steering it, the same "matched, but
/// also verified" trust boundary `apply_car_input`'s own docs already
/// spell out for the identical reason.
fn apply_set_car_patrol(set: On<FromClient<SetCarPatrolMsg>>, mut cars: Query<(&CarChassis, &mut AiCarPatrol)>) {
    for (chassis, mut patrol) in &mut cars {
        if chassis.owner_player_id == AI_OWNER && chassis.car_id == set.car_id {
            patrol.target_true_x = set.target_true_x;
            patrol.target_true_z = set.target_true_z;
            break;
        }
    }
}

/// Same as `apply_set_car_patrol`, for an AI plane.
fn apply_set_plane_patrol(
    set: On<FromClient<SetPlanePatrolMsg>>,
    mut planes: Query<(&PlaneSnapshot, &mut AiPlanePatrol)>,
) {
    for (snapshot, mut patrol) in &mut planes {
        if snapshot.owner_player_id == AI_OWNER && snapshot.plane_id == set.plane_id {
            patrol.target_true_x = set.target_true_x;
            patrol.target_true_z = set.target_true_z;
            break;
        }
    }
}

/// Bearing from one true-space point to another, in the same
/// "atan2(dx, dz)" convention every other facing-toward-a-point
/// calculation in this game already uses (see `building_placement.rs`'s
/// own docs on this exact convention — 0 faces `+Z`, increasing toward
/// `+X`).
fn bearing_to(from_x: f64, from_z: f64, to_x: f64, to_z: f64) -> f32 {
    ((to_x - from_x) as f32).atan2((to_z - from_z) as f32)
}

/// Wraps an angle (typically a `desired - current` bearing difference) to
/// `(-PI, PI]` — without this, a target almost directly behind (a
/// difference near `+-TAU`) would read as needing to turn nearly all the
/// way around the *long* way instead of the short way.
fn normalize_angle(angle: f32) -> f32 {
    (angle + std::f32::consts::PI).rem_euclid(std::f32::consts::TAU) - std::f32::consts::PI
}

/// Drives every `AiCarPatrol` car straight toward its target: turn to
/// face it, throttle scaled down the sharper the turn still needed
/// ("carefully," per the ask — easing off instead of flooring it while
/// pointed the wrong way), brake once within `CAR_ARRIVE_RADIUS`. Writes
/// the exact same `CarInputState` a real `CarInputMsg` does — `step_cars`
/// neither knows nor cares that nothing is actually driving this over the
/// network.
fn drive_ai_cars(origin: Res<WorldOrigin>, mut cars: Query<(&Transform, &AiCarPatrol, &mut CarInputState)>) {
    for (transform, patrol, mut state) in &mut cars {
        let true_pos = origin.to_true(transform.translation);
        let dx = patrol.target_true_x - true_pos.x;
        let dz = patrol.target_true_z - true_pos.z;
        if (dx * dx + dz * dz).sqrt() < CAR_ARRIVE_RADIUS {
            state.input = CarInput { throttle: 0.0, steer: 0.0, brake: true, boost: false };
            continue;
        }

        let desired_yaw = bearing_to(true_pos.x, true_pos.z, patrol.target_true_x, patrol.target_true_z);
        let forward = *transform.forward();
        let current_yaw = forward.x.atan2(forward.z);
        let yaw_error = normalize_angle(desired_yaw - current_yaw);

        // Positive `steer` turns the car toward local `-X` (see
        // `car_sim`'s per-wheel steering math), which *decreases* this
        // bearing convention's angle — so closing a positive error (need
        // to *increase* yaw) needs *negative* steer, hence the flip.
        let steer = (-yaw_error * CAR_STEER_GAIN).clamp(-1.0, 1.0);
        let throttle = (1.0 - yaw_error.abs() / std::f32::consts::PI).clamp(0.35, 1.0);

        state.input = CarInput { throttle, steer, brake: false, boost: false };
    }
}

/// Flies every `AiPlanePatrol` plane toward its target, then orbits it
/// indefinitely at `PLANE_ORBIT_RADIUS` instead of trying to stop (there's
/// no useful "hover" in this flight model). Altitude hold is a plain
/// proportional *rate* controller, not a real attitude hold (`pitch` is a
/// rotation rate, not an angle — see `aircraft::fly_planes`'s own docs) —
/// good enough to keep a "dumbass" AI roughly level without the real
/// attitude-tracking a smarter brain would eventually want. Roll is left
/// at zero: a nicer version would bank into turns, out of scope for this
/// starter.
fn fly_ai_planes(origin: Res<WorldOrigin>, mut planes: Query<(&Transform, &AiPlanePatrol, &mut PlaneInputState)>) {
    for (transform, patrol, mut state) in &mut planes {
        let true_pos = origin.to_true(transform.translation);
        let dx = patrol.target_true_x - true_pos.x;
        let dz = patrol.target_true_z - true_pos.z;
        let dist = (dx * dx + dz * dz).sqrt();

        let yaw = if dist < PLANE_ORBIT_RADIUS {
            PLANE_ORBIT_YAW
        } else {
            let desired_yaw = bearing_to(true_pos.x, true_pos.z, patrol.target_true_x, patrol.target_true_z);
            let forward = *transform.forward();
            let current_yaw = forward.x.atan2(forward.z);
            let yaw_error = normalize_angle(desired_yaw - current_yaw);
            // Same sign flip `drive_ai_cars` uses — positive yaw also
            // decreases this bearing convention's angle (see
            // `aircraft::fly_planes`'s own docs on the local-axis rotation
            // this feeds into).
            (-yaw_error * PLANE_YAW_GAIN).clamp(-1.0, 1.0)
        };

        let altitude_error = patrol.cruise_altitude - transform.translation.y;
        let pitch = (altitude_error * PLANE_PITCH_GAIN).clamp(-0.6, 0.6);

        state.throttle = PLANE_CRUISE_THROTTLE;
        state.yaw = yaw;
        state.pitch = pitch;
        state.roll = 0.0;
    }
}
