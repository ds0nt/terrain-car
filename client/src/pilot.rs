use bevy::input::mouse::MouseMotion;
use bevy::math::DVec3;
use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};
use bevy_rapier3d::prelude::{
    CharacterAutostep, CharacterLength, Collider, KinematicCharacterController,
    KinematicCharacterControllerOutput, PhysicsSet, QueryFilter, ReadRapierContext, RigidBody,
};
use bevy_replicon::prelude::ClientTriggerExt;
use shared::car_physics::{CarChassis, CarInput};
use shared::protocol::{
    AuthResultMsg, BoardDropshipMsg, BoardPassengerMsg, BuildingSnapshot, CarInputMsg, CarSnapshot, DropshipInputMsg,
    ExitDropshipMsg, ExitDropshipPassengerMsg, ExitPassengerMsg, ExitPlaneMsg, PlaneInputMsg, PlaneSnapshot,
    PlayerPositionMsg, TankInputMsg, TeleportMsg, TurretSnapshot,
};
use shared::terrain_gen::{height_at, TerrainNoise};
use uuid::Uuid;

use shared::protocol::{DropshipSnapshot, TankSnapshot};
use shared::tank_physics::TankChassis;

use crate::aircraft::{DrivingPlaneId, LocalPlane};
use crate::auth_ui::AuthState;
use crate::camera::CarCamera;
use crate::car::{DrivingCarId, LocalCar};
use crate::dropship::{DrivingDropshipId, LocalDropship, PassengerDropshipId};
use crate::tank::{DrivingTankId, LocalTank};
use crate::worldspace::WorldOrigin;

/// `F`: step out of whichever vehicle you're in and walk around, or (once
/// on foot, standing near one) step back into it — see `handle_vehicle_key`
/// for the actual exit/enter logic and `ControlMode`'s docs for how every
/// other input-reading system in this game (car, plane) reacts to it.
///
/// The on-foot avatar (`Pilot`) is client-local only for this first pass —
/// it never replicates, so other players don't currently see you walking
/// around, only your vehicle sitting parked. That's a real, known v1
/// limitation, not an oversight — giving this its own replicated snapshot
/// is a reasonable follow-up. Movement itself is a real Rapier
/// `KinematicCharacterController` (see `spawn_pilot`/`move_pilot`), so
/// unlike the very first pass it does collide with terrain, buildings, and
/// parked vehicles, and can jump.
pub struct PilotPlugin;

impl Plugin for PilotPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ControlMode>()
            .init_resource::<PlayerFocus>()
            .init_resource::<MenuOpen>()
            .init_resource::<PilotPitch>()
            .init_resource::<PassengerCarId>()
            .add_observer(spawn_pilot_after_login)
            .add_observer(apply_teleport)
            .add_systems(
                Update,
                (
                    // Gated on `chat::chat_closed` via `run_if` rather than
                    // an added `Res<ChatOpen>` parameter on each — this
                    // chain's own systems already sit right at Bevy's
                    // per-system parameter-tuple limit (`handle_vehicle_key`
                    // especially), so one more param here would stop the
                    // whole chain from compiling at all rather than just
                    // this one system.
                    toggle_menu.run_if(crate::chat::chat_closed),
                    manage_cursor_confinement,
                    handle_vehicle_key.run_if(crate::chat::chat_closed),
                    // Runs right after `handle_vehicle_key` in the same
                    // chain, not merged into it (Bevy's `SystemParam` tuple
                    // impl tops out at 16 elements — `handle_vehicle_key`
                    // already sits at that limit) — see this function's own
                    // docs for the one narrow edge case that ordering
                    // choice accepts.
                    handle_tank_dropship_key.run_if(crate::chat::chat_closed),
                    handle_turret_key.run_if(crate::chat::chat_closed),
                    look_pilot,
                    update_pilot_camera,
                    sync_player_focus,
                )
                    .chain(),
            )
            // Alongside Rapier itself (see main.rs's `in_fixed_schedule()`)
            // and `before(PhysicsSet::SyncBackend)` — same reasoning
            // `car.rs`'s `car_suspension_and_drive` docs already give: this
            // only *sets* `KinematicCharacterController::translation`, it
            // doesn't move anything itself, so it has to land before Rapier
            // reads that input for the step. `look_pilot` stays in `Update`
            // (it only reads a continuous mouse delta and writes rotation
            // directly, no physics involved).
            .add_systems(FixedUpdate, move_pilot.before(PhysicsSet::SyncBackend))
            .add_systems(FixedUpdate, send_player_position);
    }
}

/// The single source of truth for "where is the player and what are they
/// doing right now" — one `Vec3` position/facing/velocity, re-derived
/// every frame from whichever entity `ControlMode` currently points at
/// (car, plane, or on-foot walk). Every other system that needs "the
/// player's position" (`hud.rs`, `minimap.rs`, and any future one) reads
/// this single resource instead of separately hardcoding its own
/// `Query<_, With<LocalCar>>` and its own opinion of which entity that
/// means.
///
/// This exists because that duplication was exactly the bug: `hud.rs` and
/// `minimap.rs` each independently assumed "the player" meant the car
/// entity specifically, so stepping out of it (flying, walking) silently
/// left both still reporting the parked car's stale position/speed
/// instead of whatever the player actually occupied — the query itself
/// never had a reason to know a plane or an on-foot walk even existed. A
/// second hardcoded query would only reintroduce the same class of bug
/// the next time a new vehicle kind shows up; this makes "read the
/// player's current position" a single call site instead of an
/// assumption every consumer has to independently get right.
#[derive(Resource, Default, Clone, Copy)]
pub struct PlayerFocus {
    pub translation: Vec3,
    pub forward: Vec3,
    pub linear_velocity: Vec3,
}

/// Tracks the on-foot avatar's current velocity, for `sync_player_focus`
/// (HUD speed/G-force, same as a car's or plane's `Velocity`/
/// `linear_velocity`) and as `move_pilot`'s own persistent vertical-speed
/// accumulator across ticks (gravity, jump). A `KinematicCharacterController`
/// doesn't expose a `Velocity` component the way a dynamic body does — it
/// only reports the *effective* (post-collision) displacement for the tick
/// that just ran, via `KinematicCharacterControllerOutput` — so `move_pilot`
/// derives this from that instead of a naive "desired input" velocity: it
/// correctly reads back to zero when walking straight into a wall, or drops
/// to zero the instant a jump bonks a ceiling, rather than reporting
/// whatever WASD asked for regardless of what actually happened.
#[derive(Component, Default)]
struct PilotMotion(Vec3);

/// Which vehicle (if any) the local player's input/camera currently
/// follows. Car input/sending (`car.rs`'s `read_car_input`,
/// `prediction.rs`'s `send_input`) and plane input/sending
/// (`aircraft.rs`'s `read_plane_input`/`send_plane_input`) both check this
/// before doing anything, so only ever one of {car, plane, on-foot walk}
/// actually receives your input at a time — everything else just sits
/// under ordinary physics, parked.
#[derive(Resource, Default, PartialEq, Eq, Clone, Copy)]
pub enum ControlMode {
    Car,
    Plane,
    /// Riding along in someone else's car — see `PassengerCarId` for which
    /// one. No input of any kind gets sent anywhere while this is active
    /// (`car.rs`'s `read_car_input`/`send_car_input` and `aircraft.rs`'s
    /// equivalents all gate on `ControlMode::Car`/`Plane` specifically),
    /// so a passenger genuinely cannot drive — only watch.
    Passenger,
    /// Driving a `Tank` — see `crate::tank::DrivingTankId` for which one.
    /// Same mutual-exclusion role `Car` plays for `car.rs`.
    Tank,
    /// Piloting a `Dropship` — see `crate::dropship::DrivingDropshipId`.
    /// Same role `Plane` plays for `aircraft.rs`.
    DropshipPilot,
    /// Riding along in someone else's (or your own, non-piloting) dropship
    /// seat — see `crate::dropship::PassengerDropshipId`. Same role
    /// `Passenger` plays for a car, just for one of a dropship's four
    /// independent seats.
    DropshipPassenger,
    /// Manually aiming/firing an owned, unoccupied `Turret` building — see
    /// `crate::turret_control::OccupiedTurretId` for which one. Only the
    /// turret's own owner can ever enter it (see `shared::protocol::
    /// TurretSnapshot::occupant_player_id`'s own docs on why this isn't a
    /// shared passenger-style seat).
    TurretOperator,
    #[default]
    OnFoot,
}

/// Which car (by `CarChassis::car_id`) the local player is currently
/// riding as a passenger — `None` whenever `ControlMode` isn't
/// `Passenger`. Same shape as `car::DrivingCarId`/`aircraft::DrivingPlaneId`
/// and the same reason: a car's own identity, not just "the nearest one,"
/// has to be remembered between boarding and the later exit/camera-follow
/// systems that need to find that exact entity again — and unlike a
/// driven car, a ridden one is never tagged `LocalCar` (it isn't yours),
/// so it can't be found by that marker either.
#[derive(Resource, Default, Clone, Copy)]
pub struct PassengerCarId(pub Option<Uuid>);

/// The on-foot avatar entity — see this module's own top-level docs on
/// why it's client-local only for now. `pub(crate)` so `worldspace.rs`'s
/// `rebase_world` can include it in the floating-origin shift — unlike a
/// car or plane, nothing re-derives its position from a replicated true-
/// space source every frame, so without this it would drift out of sync
/// with everything else the instant a rebase happened while on foot.
#[derive(Component)]
pub(crate) struct Pilot;

// Bumped from `6.0` — reported live as too slow crossing terrain this
// large compared to a car's/plane's own top speed.
const WALK_SPEED: f32 = 10.0;
/// Radians per pixel of raw mouse motion — same order of magnitude as
/// `camera.rs`'s `ORBIT_SENSITIVITY` (0.006), tuned separately since this
/// drives the avatar's actual facing every frame rather than a
/// button-gated orbit look.
const MOUSE_SENSITIVITY: f32 = 0.0045;
/// Capsule collider/mesh dimensions — `radius` matches both the visible
/// mesh (`Capsule3d::new(PILOT_RADIUS, PILOT_CYLINDER_LENGTH)`) and the
/// physics collider (`Collider::capsule_y`, which wants a *half*-length),
/// so the two can never quietly drift apart.
pub(crate) const PILOT_RADIUS: f32 = 0.4;
pub(crate) const PILOT_CYLINDER_LENGTH: f32 = 1.1;
/// How far above the character controller's origin (feet level) the
/// visible mesh sits — the capsule's own half-height (radius + half the
/// cylindrical length).
const PILOT_HALF_HEIGHT: f32 = PILOT_RADIUS + PILOT_CYLINDER_LENGTH * 0.5;
/// Downward acceleration while airborne — noticeably snappier than real
/// Earth gravity (9.81), matching this project's existing "arcade, not
/// sim" physics feel (see `car_physics.rs`'s own tuning comments).
const GRAVITY: f32 = 24.0;
/// Initial upward speed on jump — `v^2 / (2 * GRAVITY)` gives roughly a
/// 1.3m hop, enough to clear a curb or a low ledge without floating.
const JUMP_SPEED: f32 = 8.0;
/// Small constant downward speed fed to the controller while grounded and
/// not jumping (instead of exactly zero) — a `KinematicCharacterController`
/// only snaps back onto a surface it's already moving toward, so a bare
/// zero here would read as "hovering" the instant the ground dips by even
/// a few centimeters (every terrain heightfield triangle edge, in
/// practice).
const GROUND_STICK_SPEED: f32 = 1.0;
/// How large a gap `snap_to_ground` (see `spawn_pilot`) will pull the
/// character down across — same value used there; named here too so
/// `move_pilot` can restore the exact same setting after temporarily
/// disabling it (see that function's own docs on why).
const SNAP_TO_GROUND_DISTANCE: f32 = 0.4;
/// How close (local-space, full 3D distance — close enough given both
/// sides are already near the same ground height) you need to be standing
/// to a parked vehicle to board it with `F`.
const ENTER_RADIUS: f32 = 5.0;

/// Places the on-foot avatar at login, at the same spot the server picked
/// for the starter villager (`AuthResultMsg::spawn_true_x`/`spawn_true_z`)
/// — the player now starts on foot with no car to predict a position from
/// (see `LocalCar`'s docs), so unlike the old car spawn this can't guess
/// locally and has to wait for the server to actually say where "here" is.
/// A one-shot latch (`spawned`) the same shape the old `spawn_car_after_login`
/// used, for the same reason: a retry after a failed attempt reuses the
/// same already-spawned Pilot rather than spawning a second one.
#[allow(clippy::too_many_arguments)]
fn spawn_pilot_after_login(
    result: On<AuthResultMsg>,
    mut commands: Commands,
    noise: Res<TerrainNoise>,
    mut origin: ResMut<WorldOrigin>,
    mut focus: ResMut<PlayerFocus>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut terrain_materials: ResMut<Assets<crate::terrain_material::TerrainMaterial>>,
    mut loaded_chunks: ResMut<crate::terrain::LoadedChunks>,
    view_distance: Res<crate::terrain::ViewDistance>,
    mut spawned: Local<bool>,
) {
    if *spawned || !result.ok {
        return;
    }
    *spawned = true;

    // The `Startup` terrain batch was spawned around true chunk (0,0) —
    // only correct if the server also happens to spawn the player near
    // true (0,0). Re-centering `WorldOrigin` on the real spawn point
    // *without* also wiping/re-seeding those chunks would leave them
    // rendered at their old (now wrong) local positions — exactly the
    // "terrain shows up as if I'm still at true (0,0)" bug this fixes.
    // Both steps happen in this one system (not two separately-ordered
    // observers on the same trigger) so there's no ordering hazard between
    // "origin changed" and "chunks re-seeded to match."
    let spawn_true = bevy::math::DVec3::new(result.spawn_true_x, 0.0, result.spawn_true_z);
    origin.offset = spawn_true;
    crate::terrain::respawn_chunks_immediate(
        &mut commands,
        &mut meshes,
        &mut materials,
        &mut terrain_materials,
        &noise,
        &origin,
        &mut loaded_chunks,
        &view_distance,
    );

    let local_spawn = Vec3::ZERO;
    // No vehicle to raycast from at login — plain terrain height is the
    // only option here (see `spawn_pilot`'s own docs).
    let ground_y = height_at(&noise, spawn_true.x, spawn_true.z);
    spawn_pilot(&mut commands, &mut meshes, &mut materials, ground_y, local_spawn);
    focus.translation = local_spawn;
}

/// Server -> this client only, sent by `/tp`/`/respawn` (see
/// `server::chat`) — repositions the on-foot avatar if one currently
/// exists. A `Pilot` entity only exists while `ControlMode::OnFoot` (see
/// `handle_vehicle_key`, which despawns it the instant you board any
/// vehicle) — while driving/flying/riding this is simply a no-op, because
/// the vehicle itself was already moved directly server-side (see
/// `TeleportMsg`'s own docs on why cars/planes don't need this at all).
#[allow(clippy::too_many_arguments)]
fn apply_teleport(
    teleport: On<TeleportMsg>,
    mut commands: Commands,
    rapier_context: ReadRapierContext,
    noise: Res<TerrainNoise>,
    mut origin: ResMut<WorldOrigin>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut terrain_materials: ResMut<Assets<crate::terrain_material::TerrainMaterial>>,
    mut loaded_chunks: ResMut<crate::terrain::LoadedChunks>,
    view_distance: Res<crate::terrain::ViewDistance>,
    mut pilot_q: Query<&mut Transform, With<Pilot>>,
) {
    let Ok(mut transform) = pilot_q.single_mut() else {
        return;
    };

    // Re-centers the floating origin on the teleport target and
    // immediately (not gradually, via the ordinary per-frame
    // `stream_chunks` budget) rebuilds terrain around it — the exact same
    // "big one-time origin reset" pattern `spawn_pilot_after_login`
    // already uses, for the identical reason (see `respawn_chunks_immediate`'s
    // own docs): leaving this to normal streaming meant no terrain
    // collider existed under the avatar's feet for the many frames it
    // takes to catch up after a large jump, and gravity doesn't wait —
    // reported live as falling through the ground/starting underground
    // after `/tp`, `/respawn`, or `H`'s on-foot recall. A raycast alone
    // (this function's own previous fix) only ever addressed *which*
    // surface to land on, not *whether it's actually loaded yet* — the
    // real bug the user correctly pointed at ("the wrong terrain
    // position... because it loads in pieces").
    origin.offset = DVec3::new(teleport.true_x, 0.0, teleport.true_z);
    crate::terrain::respawn_chunks_immediate(
        &mut commands,
        &mut meshes,
        &mut materials,
        &mut terrain_materials,
        &noise,
        &origin,
        &mut loaded_chunks,
        &view_distance,
    );

    // Local (0, _, 0) *is* the teleport target now, by construction above
    // — same reasoning `spawn_pilot_after_login` uses for its own
    // `Vec3::ZERO`. `surface_y_below` still does the real work of finding
    // the right *height* (terrain, or a building's roof — see
    // `RecallPlayerMsg`'s docs on why `H`'s on-foot recall specifically
    // needs that), now against terrain that's actually guaranteed to
    // exist by the time this raycasts it.
    let ground_y = surface_y_below(&rapier_context, &noise, Vec3::new(0.0, 500.0, 0.0), origin.offset);
    transform.translation = Vec3::new(0.0, ground_y + PILOT_HALF_HEIGHT, 0.0);
}

/// `F`, handled as a single system with one match on the current mode —
/// deliberately *not* two separate "exit" and "enter" systems both gated
/// on `just_pressed(KeyE)`: with two, exiting the car (which spawns you
/// standing right next to it, well within `ENTER_RADIUS`) would let the
/// "enter" system see that same still-true `just_pressed` this same frame
/// and immediately re-board it, making `F` appear to do nothing at all.
/// One system with one match arm executing per press rules that out
/// structurally.
/// `car_q`/`plane_q` use `.iter()`, not `.single()`, throughout this
/// function — a player can own several cars or planes now (full parity,
/// see `car.rs`'s top-level docs), so a `.single()` here would silently do
/// nothing at all (pressing `F` appears completely dead) the instant
/// anyone owns more than one of either.
///
/// Boarding a car or plane specifically picks the *nearest* one within
/// `ENTER_RADIUS`, not just any owned one, and records it in
/// `DrivingCarId`/`DrivingPlaneId` — the earlier "whichever happens to be
/// first in iteration order" tolerance was exactly the bug reported live
/// as "when I join a car it should be the correct car" (and the same for
/// planes — boarding one appeared to move all of them): walking up to and
/// boarding car/plane B could silently start driving car/plane A's
/// camera/input instead the moment you owned more than one. Exiting reads
/// the same resource back to find the *specific* vehicle you're actually
/// in (not just "first" again, for the identical reason), so `exit_local`
/// is computed next to the right one too.
#[allow(clippy::too_many_arguments)]
fn handle_vehicle_key(
    keyboard: Res<ButtonInput<KeyCode>>,
    mut mode: ResMut<ControlMode>,
    mut car_input: ResMut<CarInput>,
    mut driving_car: ResMut<DrivingCarId>,
    mut driving_plane: ResMut<DrivingPlaneId>,
    mut passenger_car: ResMut<PassengerCarId>,
    mut commands: Commands,
    car_q: Query<(&Transform, &CarChassis), With<LocalCar>>,
    plane_q: Query<(&Transform, &PlaneSnapshot), With<LocalPlane>>,
    // *Not* `With<LocalCar>` — a passenger seat only ever makes sense in a
    // car you *don't* already own the driver's seat of; your own car(s)
    // are handled by `car_q`/`nearest_car` above instead.
    foreign_car_q: Query<(&Transform, &CarChassis, &CarSnapshot), Without<LocalCar>>,
    noise: Res<TerrainNoise>,
    origin: Res<WorldOrigin>,
    rapier_context: ReadRapierContext,
    pilot_q: Query<(Entity, &Transform), With<Pilot>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    if !keyboard.just_pressed(KeyCode::KeyF) {
        return;
    }

    match *mode {
        ControlMode::Car => {
            let Some((car_tf, _)) =
                car_q.iter().find(|(_, chassis)| Some(chassis.car_id) == driving_car.0)
            else {
                return;
            };
            let car_id = driving_car.0;
            // Actively brake to a firm stop rather than just cutting
            // throttle — `read_car_input` won't touch `CarInput` again
            // until you re-board (see its own docs), so whatever this is
            // set to now is what "parked" means for as long as you're
            // out. Coasting on momentum or a slope is exactly what read
            // as "the car doesn't park."
            *car_input = CarInput { throttle: 0.0, steer: 0.0, brake: true, boost: false };
            // The *server's* car needs this too, not just this resource:
            // `car.rs`'s `send_car_input` stops sending entirely once mode
            // leaves `Car` (see its own docs), so without one final
            // message here the server keeps re-applying whatever
            // throttle/steer it last received, forever — there's no
            // server-side timeout. Mirrors the `Plane` arm below, which
            // already sends a final zeroed `PlaneInputMsg` for the same
            // reason.
            if let Some(car_id) = car_id {
                commands.client_trigger(CarInputMsg {
                    car_id,
                    throttle: 0.0,
                    steer: 0.0,
                    brake: true,
                    boost: false,
                });
            }
            let exit_local = car_tf.translation + *car_tf.right() * 2.5;
            let ground_y = surface_y_below(&rapier_context, &noise, exit_local, origin.to_true(exit_local));
            spawn_pilot(&mut commands, &mut meshes, &mut materials, ground_y, exit_local);
            driving_car.0 = None;
            *mode = ControlMode::OnFoot;
        }
        ControlMode::Plane => {
            let Some((plane_tf, snapshot)) =
                plane_q.iter().find(|(_, snapshot)| Some(snapshot.plane_id) == driving_plane.0)
            else {
                return;
            };
            // Whatever the plane is actually resting on — terrain or a
            // building roof — not a raw terrain lookup (see
            // `surface_y_below`'s own docs).
            let surface_y =
                surface_y_below(&rapier_context, &noise, plane_tf.translation, origin.to_true(plane_tf.translation));
            commands.client_trigger(PlaneInputMsg {
                plane_id: snapshot.plane_id,
                throttle: 0.0,
                yaw: 0.0,
                pitch: 0.0,
                roll: 0.0,
            });
            // A zeroed `PlaneInputMsg` alone only stops *future* thrust —
            // this is the actual hard stop, see `ExitPlaneMsg`'s own docs.
            commands.client_trigger(ExitPlaneMsg { plane_id: snapshot.plane_id });
            let exit_local = plane_tf.translation + *plane_tf.right() * 3.0;
            // Always allowed, at any altitude — there used to be a gate
            // here refusing to exit mid-flight above a few meters, which
            // read as "the door doesn't open," reported live as wanting to
            // *always* be able to bail out instead, parachute-less: engine
            // cut (already true, above) and just fall. `.max(surface_y)`
            // is what actually produces that — landed (`plane_tf`'s own Y
            // is already ~`surface_y`) exits right onto the ground exactly
            // as before, but bailing out from height spawns the on-foot
            // avatar at the plane's own current altitude instead of
            // teleporting it down to the ground, so `move_pilot`'s own
            // gravity (it's airborne — no `KinematicCharacterControllerOutput`
            // yet the instant it spawns) takes over and it actually falls.
            let spawn_y = plane_tf.translation.y.max(surface_y);
            spawn_pilot(&mut commands, &mut meshes, &mut materials, spawn_y, exit_local);
            driving_plane.0 = None;
            *mode = ControlMode::OnFoot;
        }
        ControlMode::Passenger => {
            let Some(car_id) = passenger_car.0 else { return };
            let Some((car_tf, ..)) = foreign_car_q.iter().find(|(_, chassis, _)| chassis.car_id == car_id)
            else {
                return;
            };
            commands.client_trigger(ExitPassengerMsg { car_id });
            let exit_local = car_tf.translation + *car_tf.right() * 2.5;
            let ground_y = surface_y_below(&rapier_context, &noise, exit_local, origin.to_true(exit_local));
            spawn_pilot(&mut commands, &mut meshes, &mut materials, ground_y, exit_local);
            passenger_car.0 = None;
            *mode = ControlMode::OnFoot;
        }
        ControlMode::OnFoot => {
            let Ok((pilot_entity, pilot_tf)) = pilot_q.single() else { return };
            let nearest_car = car_q
                .iter()
                .filter(|(car_tf, _)| car_tf.translation.distance(pilot_tf.translation) < ENTER_RADIUS)
                .min_by(|(a_tf, _), (b_tf, _)| {
                    a_tf.translation
                        .distance(pilot_tf.translation)
                        .total_cmp(&b_tf.translation.distance(pilot_tf.translation))
                });
            let nearest_plane = plane_q
                .iter()
                .filter(|(plane_tf, _)| plane_tf.translation.distance(pilot_tf.translation) < ENTER_RADIUS)
                .min_by(|(a_tf, _), (b_tf, _)| {
                    a_tf.translation
                        .distance(pilot_tf.translation)
                        .total_cmp(&b_tf.translation.distance(pilot_tf.translation))
                });
            // Only offered once neither of your own vehicles is in reach —
            // walking up to your *own* parked car always boards you as the
            // driver, never as a passenger of it.
            let nearest_passenger_seat = foreign_car_q
                .iter()
                .filter(|(car_tf, _, snapshot)| {
                    snapshot.passenger_player_id.is_none()
                        && car_tf.translation.distance(pilot_tf.translation) < ENTER_RADIUS
                })
                .min_by(|(a_tf, _, _), (b_tf, _, _)| {
                    a_tf.translation
                        .distance(pilot_tf.translation)
                        .total_cmp(&b_tf.translation.distance(pilot_tf.translation))
                });
            if let Some((_, chassis)) = nearest_car {
                driving_car.0 = Some(chassis.car_id);
                commands.entity(pilot_entity).despawn();
                *mode = ControlMode::Car;
            } else if let Some((_, snapshot)) = nearest_plane {
                driving_plane.0 = Some(snapshot.plane_id);
                commands.entity(pilot_entity).despawn();
                *mode = ControlMode::Plane;
            } else if let Some((_, chassis, _)) = nearest_passenger_seat {
                commands.client_trigger(BoardPassengerMsg { car_id: chassis.car_id });
                passenger_car.0 = Some(chassis.car_id);
                commands.entity(pilot_entity).despawn();
                *mode = ControlMode::Passenger;
            }
        }
        // Tank/dropship/turret boarding and exit are each their own
        // separate system's concern — see `handle_tank_dropship_key`'s own
        // docs for why.
        ControlMode::Tank | ControlMode::DropshipPilot | ControlMode::DropshipPassenger | ControlMode::TurretOperator => {}
    }
}

/// Tank/dropship counterpart to `handle_vehicle_key` — kept as a genuinely
/// separate system rather than folded into that one match (which already
/// sits at Bevy's 16-parameter `SystemParam` tuple limit) instead of, say,
/// grouping several of its existing parameters into a custom
/// `#[derive(SystemParam)]` struct to make room. That refactor was the more
/// "textbook" fix, but `handle_vehicle_key` is explicitly documented as
/// fragile — one deliberately single, carefully-ordered match existing
/// specifically to prevent exit-then-immediately-re-enter in one frame —
/// and reworking its parameter shape risked introducing a subtle regression
/// into that already-tuned, working car/plane path for a purely additive
/// feature. The tradeoff this separate system accepts instead: since it
/// runs *after* `handle_vehicle_key` in the same chain, exiting a car/plane/
/// passenger seat that happens to leave you within `ENTER_RADIUS` of an
/// owned tank or a boardable dropship could auto-board that in the very
/// same `F` press — narrow (it needs a tank/dropship parked within a few
/// meters of wherever you got out), and never a car/plane/passenger
/// regression, since `handle_vehicle_key` itself is untouched.
#[allow(clippy::too_many_arguments)]
fn handle_tank_dropship_key(
    keyboard: Res<ButtonInput<KeyCode>>,
    mut mode: ResMut<ControlMode>,
    mut driving_tank: ResMut<DrivingTankId>,
    mut driving_dropship: ResMut<DrivingDropshipId>,
    mut passenger_dropship: ResMut<PassengerDropshipId>,
    mut commands: Commands,
    tank_q: Query<(&Transform, &TankChassis), With<LocalTank>>,
    dropship_q: Query<(&Transform, &DropshipSnapshot), With<LocalDropship>>,
    foreign_dropship_q: Query<(&Transform, &DropshipSnapshot), Without<LocalDropship>>,
    noise: Res<TerrainNoise>,
    origin: Res<WorldOrigin>,
    rapier_context: ReadRapierContext,
    pilot_q: Query<(Entity, &Transform), With<Pilot>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    if !keyboard.just_pressed(KeyCode::KeyF) {
        return;
    }

    match *mode {
        ControlMode::Tank => {
            let Some((tank_tf, chassis)) = tank_q.iter().find(|(_, chassis)| Some(chassis.tank_id) == driving_tank.0)
            else {
                return;
            };
            // Same "server keeps re-applying the last input forever, so
            // send one final hard-braked message" reasoning
            // `handle_vehicle_key`'s `Car` arm gives — a tank has no
            // separate `ExitTankMsg` (it's grounded, not a hoverplane; see
            // `shared::tank_physics`'s own docs), so this final
            // `TankInputMsg` is the whole story, same as a car.
            commands.client_trigger(TankInputMsg {
                tank_id: chassis.tank_id,
                throttle: 0.0,
                steer: 0.0,
                brake: true,
                turret_yaw: 0.0,
            });
            let exit_local = tank_tf.translation + *tank_tf.right() * 3.5;
            let ground_y = surface_y_below(&rapier_context, &noise, exit_local, origin.to_true(exit_local));
            spawn_pilot(&mut commands, &mut meshes, &mut materials, ground_y, exit_local);
            driving_tank.0 = None;
            *mode = ControlMode::OnFoot;
        }
        ControlMode::DropshipPilot => {
            let Some((dropship_tf, snapshot)) =
                dropship_q.iter().find(|(_, snapshot)| Some(snapshot.dropship_id) == driving_dropship.0)
            else {
                return;
            };
            let surface_y = surface_y_below(
                &rapier_context,
                &noise,
                dropship_tf.translation,
                origin.to_true(dropship_tf.translation),
            );
            commands.client_trigger(DropshipInputMsg {
                dropship_id: snapshot.dropship_id,
                throttle: 0.0,
                yaw: 0.0,
                pitch: 0.0,
                roll: 0.0,
            });
            // Same "cut engines, hard stop" reasoning `ExitPlaneMsg` needs
            // beyond a zeroed input message alone — see that message's own
            // docs.
            commands.client_trigger(ExitDropshipMsg { dropship_id: snapshot.dropship_id });
            let exit_local = dropship_tf.translation + *dropship_tf.right() * 4.0;
            // Same "always allowed, at any altitude — fall rather than
            // teleport to ground" shape `handle_vehicle_key`'s `Plane` arm
            // uses.
            let spawn_y = dropship_tf.translation.y.max(surface_y);
            spawn_pilot(&mut commands, &mut meshes, &mut materials, spawn_y, exit_local);
            driving_dropship.0 = None;
            *mode = ControlMode::OnFoot;
        }
        ControlMode::DropshipPassenger => {
            let Some(dropship_id) = passenger_dropship.0 else { return };
            let Some((dropship_tf, _)) =
                foreign_dropship_q.iter().find(|(_, snapshot)| snapshot.dropship_id == dropship_id)
            else {
                return;
            };
            commands.client_trigger(ExitDropshipPassengerMsg { dropship_id });
            let exit_local = dropship_tf.translation + *dropship_tf.right() * 4.0;
            let ground_y = surface_y_below(&rapier_context, &noise, exit_local, origin.to_true(exit_local));
            spawn_pilot(&mut commands, &mut meshes, &mut materials, ground_y, exit_local);
            passenger_dropship.0 = None;
            *mode = ControlMode::OnFoot;
        }
        ControlMode::OnFoot => {
            let Ok((pilot_entity, pilot_tf)) = pilot_q.single() else { return };
            let nearest_tank = tank_q
                .iter()
                .filter(|(tank_tf, _)| tank_tf.translation.distance(pilot_tf.translation) < ENTER_RADIUS)
                .min_by(|(a_tf, _), (b_tf, _)| {
                    a_tf.translation
                        .distance(pilot_tf.translation)
                        .total_cmp(&b_tf.translation.distance(pilot_tf.translation))
                });
            let nearest_own_dropship = dropship_q
                .iter()
                .filter(|(dropship_tf, _)| dropship_tf.translation.distance(pilot_tf.translation) < ENTER_RADIUS)
                .min_by(|(a_tf, _), (b_tf, _)| {
                    a_tf.translation
                        .distance(pilot_tf.translation)
                        .total_cmp(&b_tf.translation.distance(pilot_tf.translation))
                });
            // Only offered once neither of your own tank/dropship is in
            // reach — same "walking up to your own always boards you as
            // the operator" priority `handle_vehicle_key`'s passenger-seat
            // check gives a car.
            let nearest_passenger_seat = foreign_dropship_q
                .iter()
                .filter(|(dropship_tf, snapshot)| {
                    snapshot.passenger_player_ids.contains(&None)
                        && dropship_tf.translation.distance(pilot_tf.translation) < ENTER_RADIUS
                })
                .min_by(|(a_tf, _), (b_tf, _)| {
                    a_tf.translation
                        .distance(pilot_tf.translation)
                        .total_cmp(&b_tf.translation.distance(pilot_tf.translation))
                });
            if let Some((_, chassis)) = nearest_tank {
                driving_tank.0 = Some(chassis.tank_id);
                commands.entity(pilot_entity).despawn();
                *mode = ControlMode::Tank;
            } else if let Some((_, snapshot)) = nearest_own_dropship {
                driving_dropship.0 = Some(snapshot.dropship_id);
                commands.entity(pilot_entity).despawn();
                *mode = ControlMode::DropshipPilot;
            } else if let Some((_, snapshot)) = nearest_passenger_seat {
                commands.client_trigger(BoardDropshipMsg { dropship_id: snapshot.dropship_id });
                passenger_dropship.0 = Some(snapshot.dropship_id);
                commands.entity(pilot_entity).despawn();
                *mode = ControlMode::DropshipPassenger;
            }
        }
        // Car/Plane/Passenger are entirely `handle_vehicle_key`'s concern;
        // turret boarding is `handle_turret_key`'s.
        ControlMode::Car | ControlMode::Plane | ControlMode::Passenger | ControlMode::TurretOperator => {}
    }
}

/// Turret counterpart to `handle_vehicle_key`/`handle_tank_dropship_key` —
/// same reasoning for being its own system (`handle_vehicle_key`'s own
/// docs on the 16-parameter `SystemParam` limit), chained immediately
/// after `handle_tank_dropship_key` so the same "only acts if a prior
/// system in this chain didn't already consume this `F` press" ordering
/// holds — see that function's own docs on the one narrow edge case this
/// ordering accepts.
#[allow(clippy::too_many_arguments)]
fn handle_turret_key(
    keyboard: Res<ButtonInput<KeyCode>>,
    mut mode: ResMut<ControlMode>,
    mut occupied_turret: ResMut<crate::turret_control::OccupiedTurretId>,
    mut occupied_turret_local: ResMut<crate::turret_control::OccupiedTurretLocal>,
    local_player_id: Res<crate::auth_ui::LocalPlayerId>,
    mut commands: Commands,
    turrets_q: Query<(&BuildingSnapshot, &TurretSnapshot)>,
    pilot_q: Query<(Entity, &Transform), With<Pilot>>,
    origin: Res<WorldOrigin>,
    noise: Res<TerrainNoise>,
    rapier_context: ReadRapierContext,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    if !keyboard.just_pressed(KeyCode::KeyF) {
        return;
    }

    match *mode {
        ControlMode::TurretOperator => {
            let Some(building_id) = occupied_turret.0 else { return };
            let Some((building, _)) = turrets_q.iter().find(|(b, _)| b.id == building_id) else {
                return;
            };
            commands.client_trigger(shared::protocol::ExitTurretMsg { building_id });
            // `building.ground_y`, not a placeholder `0.0` — the turret's
            // own settled surface height, same field its visual mesh uses
            // (`building_render.rs`'s `building_transform`). A bare `0.0`
            // here only happened to work near true (0,0,0); anywhere the
            // terrain sits well above or below that, `exit_local`'s
            // raycast origin below would start from entirely the wrong
            // altitude.
            let local =
                (DVec3::new(building.true_x, building.ground_y as f64, building.true_z) - origin.offset).as_vec3();
            let exit_local = local + Vec3::new(2.5, 0.0, 0.0);
            let ground_y = surface_y_below(&rapier_context, &noise, exit_local, origin.to_true(exit_local));
            spawn_pilot(&mut commands, &mut meshes, &mut materials, ground_y, exit_local);
            occupied_turret.0 = None;
            *mode = ControlMode::OnFoot;
        }
        ControlMode::OnFoot => {
            let Ok((pilot_entity, pilot_tf)) = pilot_q.single() else { return };
            let Some(player_id) = local_player_id.0 else { return };
            let nearest_turret = turrets_q
                .iter()
                .filter(|(b, snapshot)| {
                    b.kind == shared::buildings::BuildingKind::Turret
                        && b.owner_player_id == player_id
                        && snapshot.occupant_player_id.is_none()
                })
                .map(|(b, _)| {
                    // `b.ground_y`, not a placeholder `0.0` — the turret's
                    // own settled surface height, same field its visual
                    // mesh uses (`building_render.rs`'s
                    // `building_transform`). A flat `0.0` here was the
                    // actual root cause of a real reported bug ("turrets
                    // aren't enterable"): on any terrain that isn't near
                    // true (0,0,0)'s own height, this placed the detection
                    // point far above or below the turret's real,
                    // visually-correct position — the distance check below
                    // then failed even while standing right next to it,
                    // since it was really measuring distance to a point
                    // floating underground or in midair.
                    let local = (DVec3::new(b.true_x, b.ground_y as f64, b.true_z) - origin.offset).as_vec3();
                    (b.id, local)
                })
                .filter(|(_, local)| local.distance(pilot_tf.translation) < ENTER_RADIUS)
                .min_by(|(_, a), (_, b)| {
                    a.distance(pilot_tf.translation).total_cmp(&b.distance(pilot_tf.translation))
                });
            if let Some((building_id, local)) = nearest_turret {
                commands.client_trigger(shared::protocol::EnterTurretMsg { building_id });
                occupied_turret.0 = Some(building_id);
                occupied_turret_local.0 = local;
                commands.entity(pilot_entity).despawn();
                *mode = ControlMode::TurretOperator;
            }
        }
        _ => {}
    }
}

/// Raycasts straight down from a little above `from_local` to find
/// whatever surface is actually there, falling back to raw terrain height
/// (`height_at`) only if the raycast finds nothing at all. Unlike a bare
/// `height_at` lookup — which only ever answers "what would empty terrain
/// be here," blind to anything built on top of it — this finds a
/// building's roof, a parked car, or any other collider a vehicle might
/// actually be resting on. Used wherever a vehicle exit needs to place the
/// on-foot avatar on whatever real surface is under the vehicle, not
/// wherever bare ground happens to be several meters below it.
fn surface_y_below(rapier_context: &ReadRapierContext, noise: &TerrainNoise, from_local: Vec3, true_xz: DVec3) -> f32 {
    if let Ok(context) = rapier_context.single() {
        let ray_origin = Vec3::new(from_local.x, from_local.y + 5.0, from_local.z);
        if let Some((_, toi)) = context.cast_ray(ray_origin, Vec3::NEG_Y, 2000.0, true, QueryFilter::default()) {
            return ray_origin.y - toi;
        }
    }
    height_at(noise, true_xz.x, true_xz.z)
}

/// Simple procedural placeholder mesh (a capsule — no external texture/
/// model assets, matching this project's existing cosmetic style),
/// ground-snapped at `exit_local`'s (x, z) using the given `ground_y` — a
/// one-time placement before any physics runs, same as any other kinematic
/// body's initial spawn transform. From then on `move_pilot` owns its
/// position via the character controller.
///
/// Takes `ground_y` directly rather than computing it internally (it used
/// to, via a bare `height_at` call) — callers with a vehicle to exit
/// already know a good `surface_y_below` raycast origin (the vehicle's own
/// current position, resting on whatever it's resting on), and a bare
/// terrain lookup here would place the avatar at raw ground level even
/// when exiting on top of a building, reported live as (among other
/// things) not being able to exit a plane landed on one at all — the
/// height-above-ground gate in `handle_vehicle_key`'s `Plane` arm used the
/// same wrong terrain-only height. `spawn_pilot_after_login` is the one
/// caller with no vehicle to raycast from, so it still falls back to
/// `height_at` itself before calling this.
fn spawn_pilot(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    ground_y: f32,
    exit_local: Vec3,
) {
    commands
        .spawn((
            Transform::from_xyz(exit_local.x, ground_y + PILOT_HALF_HEIGHT, exit_local.z),
            Visibility::default(),
            Pilot,
            PilotMotion::default(),
            RigidBody::KinematicPositionBased,
            Collider::capsule_y(PILOT_CYLINDER_LENGTH * 0.5, PILOT_RADIUS),
            KinematicCharacterController {
                offset: CharacterLength::Absolute(0.05),
                snap_to_ground: Some(CharacterLength::Absolute(SNAP_TO_GROUND_DISTANCE)),
                // Lets the character step up onto a low curb/root instead
                // of just stopping dead against it — small enough to never
                // let it "climb" a real wall or a parked car's bumper.
                autostep: Some(CharacterAutostep {
                    max_height: CharacterLength::Absolute(0.35),
                    min_width: CharacterLength::Absolute(0.2),
                    include_dynamic_bodies: true,
                }),
                ..default()
            },
        ))
        .with_children(|parent| {
            parent.spawn((
                Mesh3d(meshes.add(Capsule3d::new(PILOT_RADIUS, PILOT_CYLINDER_LENGTH))),
                MeshMaterial3d(materials.add(StandardMaterial {
                    base_color: Color::srgb(0.85, 0.75, 0.55),
                    perceptual_roughness: 0.8,
                    ..default()
                })),
                Transform::from_xyz(0.0, 0.0, 0.0),
            ));
        });
}

/// Whether the build-bar/UI-focused cursor is currently forced free —
/// toggled by `Alt` (see `toggle_menu`). Exists because `OnFoot`/`Plane`
/// locking the cursor for mouse-look (see `manage_cursor_confinement`)
/// otherwise leaves no way to click the always-visible build bar
/// (`building_ui.rs`) or select a building at all while on foot, now the
/// player's default starting mode — there's no separate "menu screen" this
/// opens, it just hands the cursor back for the UI/placement clicking that
/// was already there.
#[derive(Resource, Default)]
pub struct MenuOpen(pub bool);

/// Accumulated up/down look angle for the on-foot chase camera (see
/// `update_pilot_camera`) — mouse Y drives this the same way mouse X
/// drives the avatar's own yaw (`look_pilot`), but pitch stays purely a
/// camera concern rather than tilting the avatar's whole body, the usual
/// third-person split (the character only ever turns to face left/right,
/// the camera independently looks up/down over/around them).
#[derive(Resource, Default)]
struct PilotPitch(f32);

/// Clamped a little short of straight up/down (~75°) — past that a
/// `looking_at` target this close to directly overhead/underfoot starts
/// producing degenerate/flickery camera rotations (the classic gimbal-ish
/// issue any `looking_at`-based camera hits near the poles).
const PILOT_PITCH_LIMIT: f32 = 1.3;

// `E`, not `Tab` — `Tab` used to double as this game's own keybinding for
// this on top of being `egui`'s default keyboard-focus-navigation key
// (Tab-cycling build-bar buttons, then a later unrelated Enter/Space
// "clicking" whichever one Tab left focused), which is also why every
// clickable widget in this game's UI is now built with a non-focusable
// `Sense::CLICK` instead of the focusable default (see `building_ui.rs`'s
// `building_button` docs) — Tab genuinely does nothing anywhere now,
// game-bound or not. `E` also now hides/shows the build UI itself
// (`building_ui.rs`'s own `MenuOpen` check), not just the cursor —
// vehicle enter/exit moved to `F` to make room for this.
fn toggle_menu(keyboard: Res<ButtonInput<KeyCode>>, mut menu_open: ResMut<MenuOpen>) {
    if keyboard.just_pressed(KeyCode::KeyE) {
        menu_open.0 = !menu_open.0;
    } else if keyboard.just_pressed(KeyCode::Escape) && menu_open.0 {
        // Escape always closes it rather than toggling — pressing it while
        // already closed has other meanings (`selection.rs` clearing the
        // current selection, `building_placement.rs` canceling an in-
        // progress placement), so this only ever acts while there's
        // actually a menu open to close.
        menu_open.0 = false;
    }
}

/// Locks and hides the cursor while mouse-look is active (`Plane`,
/// `OnFoot`), releasing it back to fully free and visible in `Car`, while
/// `MenuOpen` (`Alt`) is on, or before login even completes (the login
/// form itself needs a real, clickable cursor, and `ControlMode` defaults
/// to `OnFoot` from the moment the app starts, well before there's any
/// account to check `AuthState` against). A genuinely hidden crosshair-
/// style cursor (not just confined-but-visible, an earlier pass at this)
/// is what was actually asked for; the tradeoff is that `Locked` freezes
/// the cursor's own reported position (the OS pins it at the window
/// center and only reports relative motion from then on), which breaks
/// any cursor-*position*-based raycast — see `aim_position`, which is
/// what lets `building_placement.rs`/`selection.rs` keep working under
/// this by aiming from the screen center instead whenever the cursor's
/// real position isn't meaningful. `aircraft.rs`'s mouse-driven flight
/// stick doesn't care either way: it only ever reads `MouseMotion` deltas,
/// which are reported identically under both grab modes.
fn manage_cursor_confinement(
    mode: Res<ControlMode>,
    menu_open: Res<MenuOpen>,
    chat_open: Res<crate::chat::ChatOpen>,
    settings_open: Res<crate::settings::SettingsOpen>,
    auth_state: Res<AuthState>,
    mut windows: Query<&mut CursorOptions, With<PrimaryWindow>>,
) {
    if !mode.is_changed()
        && !menu_open.is_changed()
        && !auth_state.is_changed()
        && !chat_open.is_changed()
        && !settings_open.is_changed()
    {
        return;
    }
    let Ok(mut cursor) = windows.single_mut() else {
        return;
    };
    let want_free = !matches!(*auth_state, AuthState::LoggedIn)
        || menu_open.0
        || chat_open.0
        || settings_open.0
        || *mode == ControlMode::Car
        // A tank's (or a manually-operated turret's) own turret aims at
        // wherever the real cursor is pointing on screen (`tank.rs`'s own
        // raycast, the same technique `building_placement.rs` uses) — same
        // "needs a real, free cursor position, not the locked/hidden
        // mouse-look crosshair" reasoning `Car` gets this for.
        || *mode == ControlMode::Tank
        || *mode == ControlMode::TurretOperator;
    if want_free {
        cursor.grab_mode = CursorGrabMode::None;
        cursor.visible = true;
    } else {
        cursor.grab_mode = CursorGrabMode::Locked;
        cursor.visible = false;
    }
}

/// Where mouse-raycast systems (`building_placement.rs`'s placement ghost,
/// `selection.rs`'s click-to-select) should aim from: the real cursor
/// position whenever it's actually free and visible (`Car`, or `MenuOpen`
/// — see `manage_cursor_confinement`), or the window's center while it's
/// locked and hidden for mouse-look instead, since a locked cursor's own
/// reported position is frozen/meaningless and, more to the point, the
/// crosshair drawn at screen center (`hud.rs`'s `Crosshair`) *is* where
/// the player is visually aiming once their own cursor is gone.
pub fn aim_position(mode: ControlMode, menu_open: bool, window: &Window) -> Option<Vec2> {
    if mode == ControlMode::Car || mode == ControlMode::Tank || mode == ControlMode::TurretOperator || menu_open {
        window.cursor_position()
    } else {
        Some(Vec2::new(window.width() / 2.0, window.height() / 2.0))
    }
}

/// Steers the avatar's facing (yaw, mouse X) and the camera's independent
/// look angle (pitch, mouse Y — see `PilotPitch`/`update_pilot_camera`)
/// with the mouse, replacing the old A/D tank-turn now that A/D are free
/// for strafing (see `move_pilot`) — reads raw `MouseMotion` deltas
/// directly, same shape `camera.rs`'s `handle_orbit_input` already uses
/// for its own mouse-look, just applied to the avatar's own `Transform`
/// (yaw) and a separate resource (pitch) instead of one look-angle
/// resource, and with no button gate (unlike the orbit cam, this is
/// always-on while on foot) — except `MenuOpen` (`Alt`): the cursor is a
/// real, free pointer for clicking the build bar/selecting buildings
/// while that's on (see `manage_cursor_confinement`), and moving it there
/// shouldn't also spin the avatar's view out from under the player.
/// Runs in `Update`, not `FixedUpdate`: it's a pure rotation with no
/// collision involved, so there's no reason to tie it to the physics step.
fn look_pilot(
    mode: Res<ControlMode>,
    menu_open: Res<MenuOpen>,
    chat_open: Res<crate::chat::ChatOpen>,
    mut motion: MessageReader<MouseMotion>,
    mut pitch: ResMut<PilotPitch>,
    mut pilot_q: Query<&mut Transform, With<Pilot>>,
) {
    if *mode != ControlMode::OnFoot || menu_open.0 || chat_open.0 {
        // Drains the buffered mouse-motion messages even while gated off —
        // otherwise they'd just queue up and all apply at once, as one
        // large jump, the instant chat closes.
        motion.clear();
        return;
    }
    let Ok(mut transform) = pilot_q.single_mut() else {
        motion.clear();
        return;
    };
    for event in motion.read() {
        transform.rotate_y(-event.delta.x * MOUSE_SENSITIVITY);
        pitch.0 = (pitch.0 - event.delta.y * MOUSE_SENSITIVITY).clamp(-PILOT_PITCH_LIMIT, PILOT_PITCH_LIMIT);
    }
}

/// Mouse-relative strafe walk: W/S/A/D move forward/back/left/right
/// relative to wherever the mouse is currently facing you (`look_pilot`
/// owns rotation now, this system only ever reads it), plus gravity and a
/// jump — all fed into a real Rapier `KinematicCharacterController` rather
/// than a bare `Transform` write, so this is also where the avatar actually
/// gets its collision with terrain, buildings, and parked vehicles (see
/// this module's top-level docs). `PilotMotion` (see its own docs) doubles
/// as the persistent vertical-speed accumulator across ticks, since a
/// kinematic body has no `Velocity` component of its own to hold it.
fn move_pilot(
    mode: Res<ControlMode>,
    chat_open: Res<crate::chat::ChatOpen>,
    time: Res<Time>,
    keyboard: Res<ButtonInput<KeyCode>>,
    mut pilot_q: Query<
        (
            &Transform,
            &mut KinematicCharacterController,
            &mut PilotMotion,
            Option<&KinematicCharacterControllerOutput>,
        ),
        With<Pilot>,
    >,
) {
    if *mode != ControlMode::OnFoot || chat_open.0 {
        return;
    }
    let Ok((transform, mut controller, mut motion, output)) = pilot_q.single_mut() else {
        return;
    };
    let dt = time.delta_secs();
    if dt <= 0.0 {
        return;
    }

    // No output yet on the very first tick after spawning (Rapier hasn't
    // run a character-controller step for this entity at all yet) — treat
    // that one tick as airborne, which just means one frame of negligible
    // gravity accumulation before the very next step re-detects the ground
    // it was already spawned standing on.
    let grounded = output.is_some_and(|o| o.grounded);

    let mut throttle = 0.0;
    let mut strafe = 0.0;
    if keyboard.pressed(KeyCode::KeyW) || keyboard.pressed(KeyCode::ArrowUp) {
        throttle += 1.0;
    }
    if keyboard.pressed(KeyCode::KeyS) || keyboard.pressed(KeyCode::ArrowDown) {
        throttle -= 1.0;
    }
    if keyboard.pressed(KeyCode::KeyD) || keyboard.pressed(KeyCode::ArrowRight) {
        strafe += 1.0;
    }
    if keyboard.pressed(KeyCode::KeyA) || keyboard.pressed(KeyCode::ArrowLeft) {
        strafe -= 1.0;
    }

    let forward = *transform.forward();
    let right = *transform.right();
    let horizontal = (forward * throttle + right * strafe).normalize_or_zero() * WALK_SPEED;

    let mut vertical_speed = motion.0.y;
    if grounded {
        if keyboard.just_pressed(KeyCode::Space) {
            vertical_speed = JUMP_SPEED;
        } else if vertical_speed <= 0.0 {
            vertical_speed = -GROUND_STICK_SPEED;
        }
    } else {
        vertical_speed -= GRAVITY * dt;
    }

    // `snap_to_ground` (see `spawn_pilot`) exists so walking down a gentle
    // slope or a curb doesn't read as falling for one frame at every small
    // dip — but it does that by pulling the character back down onto
    // whatever surface is within `SNAP_TO_GROUND_DISTANCE` *underneath*
    // it, with no concept of "the player just asked to go up." A single
    // tick's worth of jump velocity (`JUMP_SPEED * dt`, a few centimeters)
    // is well inside that same distance, so Rapier's own character
    // controller was snapping the jump straight back down almost as soon
    // as it started — reported live as jumping being "glitchy" and
    // sticking you to the floor. Disabling snapping for exactly as long as
    // you're actively ascending (and only then) fixes that without
    // touching the normal walking-downhill case at all: it's back on the
    // instant `vertical_speed` stops being positive, i.e. at the very top
    // of the jump's arc, before you're falling again.
    controller.snap_to_ground = if vertical_speed > 0.0 {
        None
    } else {
        Some(CharacterLength::Absolute(SNAP_TO_GROUND_DISTANCE))
    };

    let desired = Vec3::new(horizontal.x, vertical_speed, horizontal.z) * dt;
    controller.translation = Some(desired);

    // Report the *effective* (post-collision) displacement as velocity,
    // not the desired one — see `PilotMotion`'s own docs for why. Falls
    // back to `desired` on that first no-output tick.
    let effective = output.map(|o| o.effective_translation).unwrap_or(desired);
    motion.0 = effective / dt;
}

/// Rigidly-attached chase camera for the on-foot avatar — same mutual-
/// exclusion shape `aircraft.rs`'s `update_plane_camera` and `camera.rs`'s
/// own `update_camera` already use (each only acts in its own
/// `ControlMode`), so all three never fight over the same `CarCamera`
/// transform in one frame.
///
/// Deliberately *not* lerped, unlike the car's own chase camera: this one
/// exists specifically to follow mouse-look (`look_pilot`), which already
/// turns the avatar instantly every frame — smoothing the camera on top of
/// that just reintroduces the same lag one frame later, reading as the
/// camera "dragging behind" every turn instead of a snappy, direct
/// third-person rig. A car's camera has no such input to already be
/// instant relative to, so lerping it is the right call there; here it's
/// actively fighting the thing it's supposed to mirror.
fn update_pilot_camera(
    mode: Res<ControlMode>,
    pitch: Res<PilotPitch>,
    pilot_q: Query<&Transform, With<Pilot>>,
    mut camera_q: Query<&mut Transform, (With<CarCamera>, Without<Pilot>)>,
) {
    if *mode != ControlMode::OnFoot {
        return;
    }
    let Ok(pilot_tf) = pilot_q.single() else {
        return;
    };
    let Ok(mut camera_tf) = camera_q.single_mut() else {
        return;
    };

    // Yaw comes from the avatar's own facing (`look_pilot` already applied
    // it to `pilot_tf.rotation`); pitch is layered on top here rather than
    // on the avatar itself, the usual third-person split — the character
    // only ever turns to face left/right, the camera independently looks
    // up/down over/around them.
    let look_rotation = pilot_tf.rotation * Quat::from_rotation_x(pitch.0);
    let look_target = pilot_tf.translation + Vec3::Y * 1.2 + look_rotation * Vec3::NEG_Z * 10.0;
    let desired = pilot_tf.translation - *pilot_tf.forward() * 6.0 + Vec3::Y * 2.5;

    camera_tf.translation = desired;
    camera_tf.rotation =
        Transform::from_translation(desired).looking_at(look_target, Vec3::Y).rotation;
}

/// Refreshes `PlayerFocus` from whichever entity `ControlMode` currently
/// points at — see that resource's own docs for why this is the one
/// place that gets to know car/plane/on-foot are three different queries.
/// Runs last in this plugin's chain, after this frame's own movement
/// already landed (`move_pilot`) — the car's and plane's positions are
/// updated by their own plugins earlier in `Update`/`FixedUpdate`, so by
/// the time this runs every source is already this frame's real value,
/// not last frame's.
#[allow(clippy::too_many_arguments)]
fn sync_player_focus(
    mode: Res<ControlMode>,
    driving_car: Res<DrivingCarId>,
    driving_plane: Res<DrivingPlaneId>,
    passenger_car: Res<PassengerCarId>,
    driving_tank: Res<DrivingTankId>,
    driving_dropship: Res<DrivingDropshipId>,
    passenger_dropship: Res<PassengerDropshipId>,
    mut focus: ResMut<PlayerFocus>,
    car_q: Query<(&Transform, &CarChassis, &CarSnapshot), With<LocalCar>>,
    plane_q: Query<(&Transform, &PlaneSnapshot), With<LocalPlane>>,
    foreign_car_q: Query<(&Transform, &CarChassis, &CarSnapshot), Without<LocalCar>>,
    tank_q: Query<(&Transform, &TankChassis, &TankSnapshot), With<LocalTank>>,
    // No `With<LocalDropship>` filter — unlike the tank/car queries above,
    // this same query has to answer both "which dropship am I piloting"
    // (always your own) *and* "which dropship am I riding in as a
    // passenger" (never your own, see `PassengerDropshipId`'s own docs),
    // so ownership can't be baked into the query itself the way it can for
    // the other two.
    dropship_q: Query<(&Transform, &DropshipSnapshot)>,
    // Just the turret's own cached local position (set once, on entry —
    // see `handle_turret_key`), not a fresh `BuildingSnapshot` query +
    // `WorldOrigin` conversion every frame — a turret never moves, so
    // there's nothing to re-derive, and this function is already at
    // Bevy's 16-parameter `SystemParam` tuple limit with no room to spare
    // for two more params that would only ever recompute the same
    // unchanging value.
    occupied_turret_local: Res<crate::turret_control::OccupiedTurretLocal>,
    pilot_q: Query<(&Transform, &PilotMotion), With<Pilot>>,
) {
    match *mode {
        // Matches `driving_car`/`driving_plane` specifically — *not*
        // `.iter().next()` (an arbitrary owned car/plane, possibly parked
        // far away). `.iter().next()` was exactly the multi-vehicle-owner
        // bug `DrivingCarId`/`DrivingPlaneId` exist to prevent (see their
        // own docs, and `camera.rs`'s `update_camera`/`handle_vehicle_key`,
        // which already filter this way) — this system had been missed,
        // so `PlayerFocus` (and therefore `terrain.rs`'s `stream_chunks`,
        // which centers chunk streaming on it) could silently snap to a
        // *different* owned car's position the instant you boarded one
        // while owning more than one: reported live as terrain vanishing
        // the moment you get in a vehicle, because it was streaming around
        // wherever your *other* car happened to be parked instead of you.
        // Velocity comes from the replicated `CarSnapshot`/`PlaneSnapshot`
        // now, not a local Rapier `Velocity` component — neither vehicle
        // has one client-side anymore (both are purely server-
        // authoritative, see `car.rs`'s top-level docs).
        ControlMode::Car => {
            if let Some((transform, _, snapshot)) =
                car_q.iter().find(|(_, chassis, _)| Some(chassis.car_id) == driving_car.0)
            {
                focus.translation = transform.translation;
                focus.forward = *transform.forward();
                focus.linear_velocity = snapshot.linear_velocity;
            }
        }
        ControlMode::Plane => {
            if let Some((transform, snapshot)) =
                plane_q.iter().find(|(_, snapshot)| Some(snapshot.plane_id) == driving_plane.0)
            {
                focus.translation = transform.translation;
                focus.forward = *transform.forward();
                focus.linear_velocity = snapshot.linear_velocity;
            }
        }
        // Reads `foreign_car_q`, not `car_q` — the ridden car is by
        // definition someone else's (see `PassengerCarId`'s own docs), so
        // it's never tagged `LocalCar`. Reporting the car's own position
        // here (not some fixed local seat offset) is what keeps
        // `terrain.rs`'s `stream_chunks` and distance-gated server actions
        // (via `send_player_position` below) correctly centered on a
        // passenger too, exactly as it already does for a driver.
        ControlMode::Passenger => {
            if let Some((transform, _, snapshot)) =
                foreign_car_q.iter().find(|(_, chassis, _)| Some(chassis.car_id) == passenger_car.0)
            {
                focus.translation = transform.translation;
                focus.forward = *transform.forward();
                focus.linear_velocity = snapshot.linear_velocity;
            }
        }
        ControlMode::Tank => {
            if let Some((transform, _, snapshot)) =
                tank_q.iter().find(|(_, chassis, _)| Some(chassis.tank_id) == driving_tank.0)
            {
                focus.translation = transform.translation;
                focus.forward = *transform.forward();
                focus.linear_velocity = snapshot.linear_velocity;
            }
        }
        ControlMode::DropshipPilot => {
            if let Some((transform, snapshot)) =
                dropship_q.iter().find(|(_, snapshot)| Some(snapshot.dropship_id) == driving_dropship.0)
            {
                focus.translation = transform.translation;
                focus.forward = *transform.forward();
                focus.linear_velocity = snapshot.linear_velocity;
            }
        }
        ControlMode::DropshipPassenger => {
            if let Some((transform, snapshot)) =
                dropship_q.iter().find(|(_, snapshot)| Some(snapshot.dropship_id) == passenger_dropship.0)
            {
                focus.translation = transform.translation;
                focus.forward = *transform.forward();
                focus.linear_velocity = snapshot.linear_velocity;
            }
        }
        // Stationary — velocity is always zero, position is whatever was
        // cached on entry (see `handle_turret_key`). Forward stays at
        // whatever it already was (this focus doesn't drive any camera,
        // only the HUD/`PlayerPositionMsg`, neither of which needs a
        // meaningful facing here).
        ControlMode::TurretOperator => {
            focus.translation = occupied_turret_local.0;
            focus.linear_velocity = Vec3::ZERO;
        }
        ControlMode::OnFoot => {
            if let Ok((transform, motion)) = pilot_q.single() {
                focus.translation = transform.translation;
                focus.forward = *transform.forward();
                focus.linear_velocity = motion.0;
            }
        }
    }
}

/// Sends the player's own true-space position to the server every
/// `FixedUpdate` tick, unconditionally regardless of `ControlMode` — see
/// `PlayerPositionMsg`'s own docs for why this exists at all (distance-
/// bound server actions had no way to know where an on-foot player is,
/// which is exactly what broke `PlaceBuildingMsg` the moment you stepped
/// out of a car or plane). Reads `PlayerFocus` from the *previous* frame's
/// `Update` pass (one tick stale at most) rather than re-deriving
/// car/plane/pilot position itself — that's already this exact resource's
/// entire purpose, see its own docs.
fn send_player_position(
    focus: Res<PlayerFocus>,
    mode: Res<ControlMode>,
    origin: Res<WorldOrigin>,
    mut commands: Commands,
) {
    let true_pos = origin.to_true(focus.translation);
    let rotation_y = focus.forward.x.atan2(focus.forward.z);
    commands.client_trigger(PlayerPositionMsg {
        true_x: true_pos.x,
        true_z: true_pos.z,
        rotation_y,
        on_foot: *mode == ControlMode::OnFoot,
    });
}

#[cfg(test)]
mod turret_key_tests {
    use super::*;
    use crate::auth_ui::LocalPlayerId;
    use crate::turret_control::{OccupiedTurretId, OccupiedTurretLocal};
    use bevy::app::App;
    use shared::buildings::BuildingKind;
    use shared::terrain_gen::TerrainNoise;

    /// Reproduces the reported "F doesn't let me into the turret" bug in
    /// isolation — spawns exactly the entities the real game would have
    /// (an owned, unoccupied, completed `Turret` building near the
    /// on-foot `Pilot`) and runs `handle_turret_key` directly, with no
    /// client/networking/rendering involved at all.
    #[test]
    fn f_near_an_owned_unoccupied_turret_enters_it() {
        let mut app = App::new();
        app.add_plugins((bevy::state::app::StatesPlugin, bevy_replicon::prelude::RepliconPlugins));
        shared::protocol::register_protocol(&mut app);
        app.init_resource::<Time>();
        app.init_resource::<ButtonInput<KeyCode>>();
        app.insert_resource(ControlMode::OnFoot);
        app.init_resource::<OccupiedTurretId>();
        app.init_resource::<OccupiedTurretLocal>();
        app.init_resource::<WorldOrigin>();
        app.init_resource::<TerrainNoise>();
        app.init_resource::<Assets<Mesh>>();
        app.init_resource::<Assets<StandardMaterial>>();

        let player_id = Uuid::new_v4();
        app.insert_resource(LocalPlayerId(Some(player_id)));

        app.world_mut().spawn((Pilot, PilotMotion::default(), Transform::from_xyz(0.0, 0.0, 0.0)));
        app.world_mut().spawn((
            BuildingSnapshot {
                id: Uuid::new_v4(),
                kind: BuildingKind::Turret,
                owner_player_id: player_id,
                true_x: 3.0,
                true_z: 0.0,
                build_complete_at: 0.0,
                rotation_y: 0.0,
                ground_y: 0.0,
            },
            TurretSnapshot::default(),
        ));

        app.add_systems(Update, handle_turret_key);
        app.world_mut().resource_mut::<ButtonInput<KeyCode>>().press(KeyCode::KeyF);
        app.update();

        // `ControlMode` doesn't derive `Debug` (see its own definition),
        // so a plain boolean assertion rather than `assert_eq!`.
        assert!(
            *app.world().resource::<ControlMode>() == ControlMode::TurretOperator,
            "F should have entered the turret"
        );
        assert!(app.world().resource::<OccupiedTurretId>().0.is_some());
    }

    /// Regression test for the actual reported bug: a turret sitting on
    /// elevated terrain (`ground_y` far from `0.0`) must still be
    /// enterable from right next to it. Before the fix, the entry check
    /// computed the turret's detection point at a hardcoded `y = 0.0`
    /// instead of `ground_y`, so on any terrain not near true (0,0,0)'s
    /// own height, that point floated far above or below the turret's
    /// real, visually-correct position — reported live as "the detection
    /// point is floating underground or above ground while the turret
    /// itself is snapped onto the ground."
    #[test]
    fn f_near_a_turret_on_elevated_terrain_still_enters_it() {
        let mut app = App::new();
        app.add_plugins((bevy::state::app::StatesPlugin, bevy_replicon::prelude::RepliconPlugins));
        shared::protocol::register_protocol(&mut app);
        app.init_resource::<Time>();
        app.init_resource::<ButtonInput<KeyCode>>();
        app.insert_resource(ControlMode::OnFoot);
        app.init_resource::<OccupiedTurretId>();
        app.init_resource::<OccupiedTurretLocal>();
        app.init_resource::<WorldOrigin>();
        app.init_resource::<TerrainNoise>();
        app.init_resource::<Assets<Mesh>>();
        app.init_resource::<Assets<StandardMaterial>>();

        let player_id = Uuid::new_v4();
        app.insert_resource(LocalPlayerId(Some(player_id)));

        // A hill, not flat ground near zero — the pilot stands at the same
        // elevated height the turret actually settled at, exactly like a
        // player standing next to it on a real hillside.
        const HILL_HEIGHT: f32 = 47.0;
        app.world_mut().spawn((Pilot, PilotMotion::default(), Transform::from_xyz(0.0, HILL_HEIGHT, 0.0)));
        app.world_mut().spawn((
            BuildingSnapshot {
                id: Uuid::new_v4(),
                kind: BuildingKind::Turret,
                owner_player_id: player_id,
                true_x: 3.0,
                true_z: 0.0,
                build_complete_at: 0.0,
                rotation_y: 0.0,
                ground_y: HILL_HEIGHT,
            },
            TurretSnapshot::default(),
        ));

        app.add_systems(Update, handle_turret_key);
        app.world_mut().resource_mut::<ButtonInput<KeyCode>>().press(KeyCode::KeyF);
        app.update();

        assert!(
            *app.world().resource::<ControlMode>() == ControlMode::TurretOperator,
            "F should have entered the turret even though it's on elevated terrain"
        );
        assert!(app.world().resource::<OccupiedTurretId>().0.is_some());
    }
}
