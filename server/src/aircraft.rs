use std::collections::HashSet;

use bevy::math::DVec3;
use bevy::prelude::*;
use bevy_rapier3d::prelude::*;
use bevy_replicon::prelude::*;
use uuid::Uuid;

use shared::buildings::BuildingKind;
use shared::protocol::{BuildingSnapshot, ExitPlaneMsg, PlaneInputMsg, PlaneSnapshot, RecallPlaneMsg};
use shared::terrain_gen::{height_at, TerrainNoise};
use shared::time::now_unix;
use shared::worldspace::WorldOrigin;

use crate::car_sim::PlayerIdentities;

/// Scout Plane spawning (`BuildingKind::AirFactory`) and flight —
/// see that kind's own docs for the v1 scope (one automatic plane per
/// completed factory, no manual queue) and `PlaneSnapshot`'s for why
/// planes are server-authoritative only, with no client-side prediction.
pub struct AircraftPlugin;

impl Plugin for AircraftPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SpawnedPlanesFor>()
            .add_observer(apply_plane_input)
            .add_observer(apply_exit_plane)
            .add_observer(apply_recall_plane)
            .add_systems(Update, spawn_planes_from_air_factories)
            .add_systems(FixedUpdate, fly_planes.before(PhysicsSet::SyncBackend));
    }
}

const PLANE_HALF_EXTENTS: Vec3 = Vec3::new(2.2, 0.6, 3.2);
const PLANE_MASS: f32 = 900.0;
const PLANE_LINEAR_DAMPING: f32 = 0.6;
const PLANE_ANGULAR_DAMPING: f32 = 4.0;
/// Forward thrust at full throttle — tuned so terminal speed (force /
/// (linear_damping * mass), same math `car_physics.rs`'s `engine_force`
/// doc already uses) comes out noticeably faster than a car.
const PLANE_THRUST_FORCE: f32 = 30_000.0;
const PLANE_YAW_RATE: f32 = 1.6;
/// Elevator (pitch) and aileron (roll) rates — same order of magnitude as
/// yaw, roll tuned a little snappier since banking reads as more
/// responsive in most arcade flight models without feeling twitchy.
const PLANE_PITCH_RATE: f32 = 1.2;
const PLANE_ROLL_RATE: f32 = 2.0;
/// How far above an Air Factory's own roof (`ground_y` plus its full
/// collider height — see `shared::buildings::collider_shape`) a spawned
/// plane's own center sits — matches `PLANE_HALF_EXTENTS.y + 0.5`, the
/// same clearance every other plane-placement path here already uses
/// (`apply_recall_plane`), just measured from the roof instead of raw
/// terrain.
const PLANE_ROOF_CLEARANCE: f32 = PLANE_HALF_EXTENTS.y + 0.5;

/// Server-only per-plane input, mirroring `CarInputState` — updated by
/// `apply_plane_input` whenever a `PlaneInputMsg` arrives, read every tick
/// by `fly_planes`. Starts (and stays, while unpiloted) at all-zero, so an
/// unclaimed or temporarily-abandoned plane just sits under ordinary
/// gravity/friction like a parked car, rather than needing separate "am I
/// currently being flown" bookkeeping. `pub(crate)` (fields included) so
/// `server::ai`'s patrol AI can drive a plane through the exact same input
/// surface a real player's `PlaneInputMsg` does, rather than needing its
/// own separate movement path.
#[derive(Component, Default)]
pub(crate) struct PlaneInputState {
    pub(crate) throttle: f32,
    pub(crate) yaw: f32,
    pub(crate) pitch: f32,
    pub(crate) roll: f32,
}

/// Which `BuildingSnapshot::id`s have already produced their one plane —
/// v1 scope note: in-memory only, not persisted. A server restart forgets
/// this, so every already-completed `AirFactory` spawns one more plane
/// the moment the server comes back up — the same "buildings persist,
/// bookkeeping around them doesn't" tradeoff `server::car_sim`'s
/// `SpawnAnchor` already accepts, for a much smaller blast radius (one
/// harmless extra plane per factory per restart, not a duplicate economy
/// building).
#[derive(Resource, Default)]
struct SpawnedPlanesFor(HashSet<Uuid>);

/// One plane per completed `AirFactory` — spawns directly on the
/// factory's own roof (its footprint center, at roof height) rather than
/// in a clearance-searched ring beside it at ground level, reported live
/// as wanting the plane to actually appear on top of the building it came
/// out of (full parity with the identical fix for a car and its Hangar —
/// see `car_sim::spawn_cars_from_hangars`). No clearance search against
/// other cars needed anymore either: `SpawnedPlanesFor` already guarantees
/// each factory produces its one plane exactly once, and a roof spawn is
/// naturally well clear of anything parked on the ground below it.
fn spawn_planes_from_air_factories(
    mut spawned: ResMut<SpawnedPlanesFor>,
    mut commands: Commands,
    origin: Res<WorldOrigin>,
    buildings: Query<&BuildingSnapshot>,
) {
    let now = now_unix();
    for building in &buildings {
        if building.kind != BuildingKind::AirFactory
            || building.build_complete_at > now
            || spawned.0.contains(&building.id)
        {
            continue;
        }
        spawned.0.insert(building.id);

        let altitude = building.ground_y
            + 2.0 * shared::buildings::collider_shape(BuildingKind::AirFactory).half_height()
            + PLANE_ROOF_CLEARANCE;
        spawn_plane_at(&mut commands, &origin, building.owner_player_id, building.true_x, building.true_z, altitude);

        info!("server: spawned a Scout Plane for `{}` from their Air Factory", building.owner_player_id);
    }
}

/// Spawns one plane entity at an already-resolved `(true_x, true_z,
/// altitude)` — factored out of `spawn_planes_from_air_factories` (its
/// only caller until `server::ai`'s patrol starter, which reuses this
/// exact bundle for an unowned/AI-controlled plane rather than
/// duplicating it) so both spawn identically real, physics-authoritative
/// planes.
pub(crate) fn spawn_plane_at(
    commands: &mut Commands,
    origin: &WorldOrigin,
    owner_player_id: Uuid,
    true_x: f64,
    true_z: f64,
    altitude: f32,
) -> Entity {
    let local = (DVec3::new(true_x, 0.0, true_z) - origin.offset).as_vec3();

    commands
        .spawn((
            Transform::from_xyz(local.x, altitude, local.z),
            RigidBody::Dynamic,
            Collider::cuboid(PLANE_HALF_EXTENTS.x, PLANE_HALF_EXTENTS.y, PLANE_HALF_EXTENTS.z),
            AdditionalMassProperties::Mass(PLANE_MASS),
            Velocity::zero(),
            ExternalForce::default(),
            Damping { linear_damping: PLANE_LINEAR_DAMPING, angular_damping: PLANE_ANGULAR_DAMPING },
            Friction::coefficient(0.8),
            // Hoverplane, not a real aircraft — no gravity to fight or
            // stall out of, so `fly_planes` doesn't need an artificial
            // lift term either (see that function's own docs on why the
            // old lift-from-airspeed force was removed alongside this).
            GravityScale(0.0),
            // Without this, a plane moving at its own terminal speed
            // (`PLANE_THRUST_FORCE / (PLANE_LINEAR_DAMPING * PLANE_MASS)`,
            // faster than a car's own top speed by design — see
            // `PLANE_THRUST_FORCE`'s docs) can tunnel straight through a
            // thin building collider (a roof, a wall) between one physics
            // step and the next instead of landing on it — reported live
            // as "should be able to land on objects, not just the
            // ground." `car_sim.rs`'s `spawn_car_for` already carries this
            // same `Ccd::enabled()` for the identical reason (a boosted
            // car vs. a thin `Ramp`/`Wall`); a plane needed it at least as
            // much and never had it.
            Ccd::enabled(),
            PlaneInputState::default(),
            PlaneSnapshot {
                owner_player_id,
                plane_id: Uuid::new_v4(),
                true_x,
                true_z,
                altitude,
                rotation: Quat::IDENTITY,
                linear_velocity: Vec3::ZERO,
                home_true_x: true_x,
                home_true_z: true_z,
            },
            Replicated,
        ))
        .id()
}

/// Applies the sender's latest stick position to *their own*
/// `plane_id`-named plane — same trust boundary every other client ->
/// server message here enforces (never trust the client on whose plane
/// this is for). Matched via `owner_player_id` *and* `plane_id`, not
/// `owner_player_id` alone (the original shape here): a player can own
/// several planes (full parity with several cars), and matching only the
/// owner applied every input to every owned plane at once, reported live
/// as flying one appearing to move all of them — see
/// `PlaneSnapshot::plane_id`'s docs and `car_sim::apply_car_input`'s
/// identical fix for cars, including why checking `plane_id` isn't a
/// security hole despite it being visible via replication (the
/// `owner_player_id` check still requires it to be your own).
fn apply_plane_input(
    input: On<FromClient<PlaneInputMsg>>,
    identities: Res<PlayerIdentities>,
    mut planes: Query<(&PlaneSnapshot, &mut PlaneInputState, &mut GravityScale)>,
) {
    let Some(client_entity) = input.client_id.entity() else {
        return;
    };
    let Some(player_id) = identities.get(client_entity) else {
        return;
    };
    for (snapshot, mut state, mut gravity) in &mut planes {
        if snapshot.owner_player_id == player_id && snapshot.plane_id == input.plane_id {
            state.throttle = input.throttle.clamp(-1.0, 1.0);
            state.yaw = input.yaw.clamp(-1.0, 1.0);
            state.pitch = input.pitch.clamp(-1.0, 1.0);
            state.roll = input.roll.clamp(-1.0, 1.0);
            // Someone's actively flying it again — back to the normal
            // no-gravity hoverplane model (see `apply_exit_plane`'s docs on
            // why this got turned on in the first place).
            gravity.0 = 0.0;
        }
    }
}

/// Hard-stops a plane the instant its pilot exits — see `ExitPlaneMsg`'s
/// own docs for why the zeroed `PlaneInputMsg` already sent on exit isn't
/// enough by itself (it only stops *future* thrust; existing momentum
/// would otherwise just coast down under damping). Zeroes `PlaneInputState`
/// too, so a stray in-flight `PlaneInputMsg` that lands after this can't
/// re-introduce thrust on an unpiloted plane.
fn apply_exit_plane(
    exit: On<FromClient<ExitPlaneMsg>>,
    identities: Res<PlayerIdentities>,
    mut planes: Query<(&PlaneSnapshot, &mut PlaneInputState, &mut Velocity, &mut ExternalForce, &mut GravityScale)>,
) {
    let Some(client_entity) = exit.client_id.entity() else {
        return;
    };
    let Some(player_id) = identities.get(client_entity) else {
        return;
    };
    for (snapshot, mut state, mut velocity, mut ext_force, mut gravity) in &mut planes {
        if snapshot.owner_player_id == player_id && snapshot.plane_id == exit.plane_id {
            *state = PlaneInputState::default();
            *velocity = Velocity::zero();
            *ext_force = ExternalForce::default();
            // An unpiloted plane isn't a hoverplane anymore — turn real
            // gravity back on (off at spawn, see that component's own
            // docs) so bailing out mid-air actually results in it falling/
            // crashing rather than just hanging motionless in the sky
            // where the engine cut left it. `apply_plane_input` turns this
            // back off the instant someone starts flying it again, so the
            // no-gravity thrust model still works normally for whoever
            // re-boards it later, wherever it ended up.
            gravity.0 = 1.0;
        }
    }
}

/// Teleports the sender's currently-flown plane (`recall.plane_id`, see
/// `PlaneSnapshot::plane_id`'s docs) back to wherever it spawned
/// (`PlaneSnapshot::home_true_x`/`home_true_z`) — `H`, same key
/// `car_sim::apply_recall_to_hangar` uses for a car. Zeroes velocity and
/// levels rotation, same "clean slate" reset shape that one uses too.
/// `break`s the instant it finds the matching plane since `plane_id` is
/// unique.
fn apply_recall_plane(
    recall: On<FromClient<RecallPlaneMsg>>,
    identities: Res<PlayerIdentities>,
    noise: Res<TerrainNoise>,
    origin: Res<WorldOrigin>,
    rapier_context: ReadRapierContext,
    mut planes: Query<(Entity, &mut Transform, &mut Velocity, &mut PlaneSnapshot)>,
) {
    let Some(client_entity) = recall.client_id.entity() else {
        return;
    };
    let Some(player_id) = identities.get(client_entity) else {
        return;
    };

    for (entity, mut transform, mut velocity, mut snapshot) in &mut planes {
        if snapshot.owner_player_id != player_id || snapshot.plane_id != recall.plane_id {
            continue;
        }
        // A real physics raycast (`economy::surface_height_at`), not a bare
        // `height_at` — `home_true_x`/`home_true_z` is the Air Factory's
        // own roof spawn point (see `spawn_planes_from_air_factories`), so
        // a bare terrain-noise lookup here would recall the plane back down
        // to raw *ground* level instead of the roof it actually came from,
        // the same class of bug `apply_flip_upright`'s own docs cover. Only
        // falls back to `height_at` if there's no physics context at all to
        // raycast against.
        // Excludes this same plane's own collider (see
        // `economy::surface_height_at`'s own docs on why: the ray is cast
        // straight down through the exact point this plane is currently
        // sitting on) — otherwise a plane recalled back onto the roof it
        // never left would just hit itself.
        let surface_y = match rapier_context.single() {
            Ok(context) => crate::economy::surface_height_at(
                &context,
                &noise,
                &origin,
                snapshot.home_true_x,
                snapshot.home_true_z,
                Some(entity),
            ),
            Err(_) => height_at(&noise, snapshot.home_true_x, snapshot.home_true_z),
        };
        let local =
            (DVec3::new(snapshot.home_true_x, 0.0, snapshot.home_true_z) - origin.offset).as_vec3();
        let altitude = surface_y + PLANE_HALF_EXTENTS.y + 0.5;

        transform.translation = Vec3::new(local.x, altitude, local.z);
        transform.rotation = Quat::IDENTITY;
        *velocity = Velocity::zero();
        snapshot.true_x = snapshot.home_true_x;
        snapshot.true_z = snapshot.home_true_z;
        snapshot.altitude = altitude;
        snapshot.rotation = Quat::IDENTITY;
        snapshot.linear_velocity = Vec3::ZERO;
        break;
    }
}

/// Applies each plane's current `PlaneInputState` as thrust/lift/attitude-
/// rate every physics tick, and mirrors the result back onto its own
/// `PlaneSnapshot` for replication — the same "read the just-integrated
/// Transform, write it onto the replicated snapshot" shape `car_sim.rs`'s
/// `step_cars` uses for `CarSnapshot`.
///
/// Pitch/yaw/roll aren't axis-locked — thrust follows wherever the nose is
/// currently pointed, so pitching the nose down and throttling forward
/// genuinely descends, same as pitching up climbs. Not a real aircraft
/// though — no gravity (`GravityScale(0.0)`, set at spawn) and no lift
/// force either: this is a hoverplane, so it simply holds still in the
/// air the moment thrust stops, rather than stalling and falling like a
/// real wing. An earlier pass modeled real lift-from-airspeed to fight
/// real gravity; both were removed together once "hoverplane" replaced
/// "real aircraft" as the actual design target.
fn fly_planes(
    origin: Res<WorldOrigin>,
    mut planes: Query<(&Transform, &PlaneInputState, &mut Velocity, &mut ExternalForce, &mut PlaneSnapshot)>,
) {
    for (transform, input, mut velocity, mut ext_force, mut snapshot) in &mut planes {
        let forward = *transform.forward();
        ext_force.force = forward * (input.throttle * PLANE_THRUST_FORCE);

        // Pitch/yaw/roll are rates around the plane's own *local* axes
        // (right/up/forward respectively — standard aviation convention),
        // so they have to be rotated into world space by the plane's
        // current orientation before being written to `Velocity::angular`
        // (which Rapier always reads in world space) — setting world-space
        // components directly, the way the old yaw-only version did, only
        // ever worked because a level, axis-locked plane's local axes and
        // world axes coincided. That stops being true the instant the
        // plane can actually bank or pitch.
        let local_angular_rate =
            Vec3::new(input.pitch * PLANE_PITCH_RATE, input.yaw * PLANE_YAW_RATE, input.roll * PLANE_ROLL_RATE);
        velocity.angular = transform.rotation * local_angular_rate;

        let true_pos = origin.to_true(transform.translation);
        snapshot.true_x = true_pos.x;
        snapshot.true_z = true_pos.z;
        snapshot.altitude = transform.translation.y;
        snapshot.rotation = transform.rotation;
        snapshot.linear_velocity = velocity.linear;
    }
}
