use std::collections::HashSet;

use bevy::math::DVec3;
use bevy::prelude::*;
use bevy_rapier3d::prelude::*;
use bevy_replicon::prelude::*;
use uuid::Uuid;

use shared::buildings::BuildingKind;
use shared::car_physics::CarChassis;
use shared::protocol::{
    BoardDropshipMsg, BuildingSnapshot, CargoVehicleId, DropVehicleMsg, DropshipInputMsg, DropshipSnapshot,
    ExitDropshipMsg, ExitDropshipPassengerMsg, PickupVehicleMsg, RecallDropshipMsg, CARGO_SLOTS,
};
use shared::tank_physics::TankChassis;
use shared::terrain_gen::{height_at, TerrainNoise};
use shared::time::now_unix;
use shared::worldspace::WorldOrigin;

use crate::car_sim::PlayerIdentities;

/// Dropship spawning (`BuildingKind::Dropyard`) and flight — a bigger,
/// slower `ScoutPlane` with four passenger seats instead of none. Flight
/// model, spawn/recall shape, and every naming convention here deliberately
/// mirror `server::aircraft` field-for-field; see that module's own docs
/// for the reasoning behind each piece (hoverplane no-gravity model,
/// roof-spawn placement, id-matched input routing, ...) — this file only
/// calls out what's actually *different*: the four independent passenger
/// seats (`DropshipSnapshot::passenger_player_ids`) and their own
/// board/exit messages.
pub struct DropshipSimPlugin;

impl Plugin for DropshipSimPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SpawnedDropshipsFor>()
            .add_observer(apply_dropship_input)
            .add_observer(apply_exit_dropship)
            .add_observer(apply_board_dropship)
            .add_observer(apply_exit_dropship_passenger)
            .add_observer(apply_recall_dropship)
            .add_observer(apply_pickup_vehicle)
            .add_observer(apply_drop_vehicle)
            .add_systems(Update, spawn_dropships_from_dropyards)
            .add_systems(
                FixedUpdate,
                (move_carried_vehicles, fly_dropships).chain().before(PhysicsSet::SyncBackend),
            );
    }
}

const DROPSHIP_HALF_EXTENTS: Vec3 = Vec3::new(3.2, 1.1, 4.5);
/// Noticeably heavier than a `ScoutPlane` (see `aircraft::PLANE_MASS`) — a
/// troop transport should feel like it's hauling real weight, not zipping
/// around like a scout.
const DROPSHIP_MASS: f32 = 1900.0;
const DROPSHIP_LINEAR_DAMPING: f32 = 0.7;
const DROPSHIP_ANGULAR_DAMPING: f32 = 4.5;
/// Lower than `PLANE_THRUST_FORCE` relative to its own higher mass and
/// damping — terminal speed (same `force / (linear_damping * mass)` math)
/// comes out well below a Scout Plane's, on purpose: this is a transport,
/// not a fast recon craft.
const DROPSHIP_THRUST_FORCE: f32 = 34_000.0;
const DROPSHIP_YAW_RATE: f32 = 1.1;
const DROPSHIP_PITCH_RATE: f32 = 0.8;
const DROPSHIP_ROLL_RATE: f32 = 1.3;
const DROPSHIP_ROOF_CLEARANCE: f32 = DROPSHIP_HALF_EXTENTS.y + 0.5;

/// How close (local-space, full 3D distance) a car/tank has to be to a
/// dropship to be pickup-eligible — generous, since a dropship hovers
/// *near* a vehicle rather than needing to touch it exactly, and the craft
/// itself is considerably bigger than a car.
const PICKUP_RADIUS: f32 = 12.0;
/// Local (dropship-space) slots a carried vehicle sits at — slung
/// underneath and out to either side, clear of the dropship's own body.
/// Fixed offsets rather than per-vehicle-size placement: a reasonable v1
/// simplification for what's meant to be a fun arcade mechanic, not
/// precision cargo handling.
const CARGO_OFFSETS: [Vec3; CARGO_SLOTS] = [Vec3::new(-2.6, -2.8, 0.0), Vec3::new(2.6, -2.8, 0.0)];

/// Server-only per-dropship input — same shape and same "stays all-zero
/// while unpiloted" contract `aircraft::PlaneInputState` uses.
#[derive(Component, Default)]
struct DropshipInputState {
    throttle: f32,
    yaw: f32,
    pitch: f32,
    roll: f32,
}

/// Same in-memory-only, per-restart tradeoff `aircraft::SpawnedPlanesFor`
/// accepts.
#[derive(Resource, Default)]
struct SpawnedDropshipsFor(HashSet<Uuid>);

/// One dropship per completed `Dropyard` — same roof-spawn shape
/// `aircraft::spawn_planes_from_air_factories` uses for a Scout Plane.
fn spawn_dropships_from_dropyards(
    mut spawned: ResMut<SpawnedDropshipsFor>,
    mut commands: Commands,
    origin: Res<WorldOrigin>,
    buildings: Query<&BuildingSnapshot>,
) {
    let now = now_unix();
    for building in &buildings {
        if building.kind != BuildingKind::Dropyard
            || building.build_complete_at > now
            || spawned.0.contains(&building.id)
        {
            continue;
        }
        spawned.0.insert(building.id);

        let altitude = building.ground_y
            + 2.0 * shared::buildings::collider_shape(BuildingKind::Dropyard).half_height()
            + DROPSHIP_ROOF_CLEARANCE;
        spawn_dropship_at(
            &mut commands,
            &origin,
            building.owner_player_id,
            building.true_x,
            building.true_z,
            altitude,
        );
        info!("server: spawned a Dropship for `{}` from their Dropyard", building.owner_player_id);
    }
}

fn spawn_dropship_at(
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
            Collider::cuboid(DROPSHIP_HALF_EXTENTS.x, DROPSHIP_HALF_EXTENTS.y, DROPSHIP_HALF_EXTENTS.z),
            AdditionalMassProperties::Mass(DROPSHIP_MASS),
            Velocity::zero(),
            ExternalForce::default(),
            Damping { linear_damping: DROPSHIP_LINEAR_DAMPING, angular_damping: DROPSHIP_ANGULAR_DAMPING },
            Friction::coefficient(0.8),
            // Hoverplane model, same as a Scout Plane — see
            // `aircraft::spawn_plane_at`'s own docs.
            GravityScale(0.0),
            Ccd::enabled(),
            DropshipInputState::default(),
            DropshipSnapshot {
                owner_player_id,
                dropship_id: Uuid::new_v4(),
                translation: Vec3::new(local.x, altitude, local.z),
                rotation: Quat::IDENTITY,
                linear_velocity: Vec3::ZERO,
                home_true_x: true_x,
                home_true_z: true_z,
                passenger_player_ids: [None; 4],
                cargo: [None; shared::protocol::CARGO_SLOTS],
            },
            Replicated,
        ))
        .id()
}

/// Same id-matched routing shape `aircraft::apply_plane_input` uses.
fn apply_dropship_input(
    input: On<FromClient<DropshipInputMsg>>,
    identities: Res<PlayerIdentities>,
    mut dropships: Query<(&DropshipSnapshot, &mut DropshipInputState, &mut GravityScale)>,
) {
    let Some(client_entity) = input.client_id.entity() else {
        return;
    };
    let Some(player_id) = identities.get(client_entity) else {
        return;
    };
    for (snapshot, mut state, mut gravity) in &mut dropships {
        if snapshot.owner_player_id == player_id && snapshot.dropship_id == input.dropship_id {
            state.throttle = input.throttle.clamp(-1.0, 1.0);
            state.yaw = input.yaw.clamp(-1.0, 1.0);
            state.pitch = input.pitch.clamp(-1.0, 1.0);
            state.roll = input.roll.clamp(-1.0, 1.0);
            gravity.0 = 0.0;
        }
    }
}

/// Same "cut engines, let gravity take over" shape `aircraft::apply_exit_plane`
/// uses — any passengers still seated fall with it exactly as they would if
/// the pilot had simply stopped flying it well.
fn apply_exit_dropship(
    exit: On<FromClient<ExitDropshipMsg>>,
    identities: Res<PlayerIdentities>,
    mut dropships: Query<(
        &DropshipSnapshot,
        &mut DropshipInputState,
        &mut Velocity,
        &mut ExternalForce,
        &mut GravityScale,
    )>,
) {
    let Some(client_entity) = exit.client_id.entity() else {
        return;
    };
    let Some(player_id) = identities.get(client_entity) else {
        return;
    };
    for (snapshot, mut state, mut velocity, mut ext_force, mut gravity) in &mut dropships {
        if snapshot.owner_player_id == player_id && snapshot.dropship_id == exit.dropship_id {
            *state = DropshipInputState::default();
            *velocity = Velocity::zero();
            *ext_force = ExternalForce::default();
            gravity.0 = 1.0;
        }
    }
}

/// Seats a player in the first empty passenger slot — same "no ownership
/// check, any craft with an empty seat can be ridden" shape
/// `car_sim::apply_board_passenger` uses, just picking among four slots
/// instead of one.
fn apply_board_dropship(
    board: On<FromClient<BoardDropshipMsg>>,
    identities: Res<PlayerIdentities>,
    mut dropships: Query<&mut DropshipSnapshot>,
) {
    let Some(client_entity) = board.client_id.entity() else {
        return;
    };
    let Some(player_id) = identities.get(client_entity) else {
        return;
    };
    let Some(mut snapshot) = dropships.iter_mut().find(|d| d.dropship_id == board.dropship_id) else {
        warn!("dropship_sim: board request for unknown dropship `{}`", board.dropship_id);
        return;
    };
    // Already seated somewhere in this same dropship — don't also take a
    // second slot.
    if snapshot.passenger_player_ids.contains(&Some(player_id)) {
        return;
    }
    let Some(empty_slot) = snapshot.passenger_player_ids.iter_mut().find(|slot| slot.is_none()) else {
        warn!("dropship_sim: rejected board — dropship `{}` has no empty seats", board.dropship_id);
        return;
    };
    *empty_slot = Some(player_id);
}

/// Clears whichever of the four slots currently holds the sender's own
/// player id — no seat index needed client-side at all, see
/// `ExitDropshipPassengerMsg`'s own docs.
fn apply_exit_dropship_passenger(
    exit: On<FromClient<ExitDropshipPassengerMsg>>,
    identities: Res<PlayerIdentities>,
    mut dropships: Query<&mut DropshipSnapshot>,
) {
    let Some(client_entity) = exit.client_id.entity() else {
        return;
    };
    let Some(player_id) = identities.get(client_entity) else {
        return;
    };
    let Some(mut snapshot) = dropships.iter_mut().find(|d| d.dropship_id == exit.dropship_id) else {
        return;
    };
    for slot in &mut snapshot.passenger_player_ids {
        if *slot == Some(player_id) {
            *slot = None;
        }
    }
}

/// Same "nearest owned factory of the matching kind" shape
/// `aircraft::apply_recall_plane` uses.
fn apply_recall_dropship(
    recall: On<FromClient<RecallDropshipMsg>>,
    identities: Res<PlayerIdentities>,
    noise: Res<TerrainNoise>,
    origin: Res<WorldOrigin>,
    rapier_context: ReadRapierContext,
    mut dropships: Query<(Entity, &mut Transform, &mut Velocity, &mut DropshipSnapshot)>,
) {
    let Some(client_entity) = recall.client_id.entity() else {
        return;
    };
    let Some(player_id) = identities.get(client_entity) else {
        return;
    };

    for (entity, mut transform, mut velocity, mut snapshot) in &mut dropships {
        if snapshot.owner_player_id != player_id || snapshot.dropship_id != recall.dropship_id {
            continue;
        }
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
        let local = (DVec3::new(snapshot.home_true_x, 0.0, snapshot.home_true_z) - origin.offset).as_vec3();
        let altitude = surface_y + DROPSHIP_ROOF_CLEARANCE;

        transform.translation = Vec3::new(local.x, altitude, local.z);
        transform.rotation = Quat::IDENTITY;
        *velocity = Velocity::zero();
        snapshot.translation = transform.translation;
        snapshot.rotation = Quat::IDENTITY;
        snapshot.linear_velocity = Vec3::ZERO;
        break;
    }
}

/// Same thrust/attitude-rate model `aircraft::fly_planes` uses, except the
/// snapshot mirrors local-space `translation` directly rather than
/// converting to true-space `true_x`/`true_z` — the same
/// `CarSnapshot::translation` shape, chosen here since a dropship (like a
/// car, unlike `PlaneSnapshot`) has no HUD altitude/true-position readout
/// that needs true-space precision.
fn fly_dropships(
    mut dropships: Query<(&Transform, &DropshipInputState, &mut Velocity, &mut ExternalForce, &mut DropshipSnapshot)>,
) {
    for (transform, input, mut velocity, mut ext_force, mut snapshot) in &mut dropships {
        let forward = *transform.forward();
        ext_force.force = forward * (input.throttle * DROPSHIP_THRUST_FORCE);

        let local_angular_rate = Vec3::new(
            input.pitch * DROPSHIP_PITCH_RATE,
            input.yaw * DROPSHIP_YAW_RATE,
            input.roll * DROPSHIP_ROLL_RATE,
        );
        velocity.angular = transform.rotation * local_angular_rate;

        snapshot.translation = transform.translation;
        snapshot.rotation = transform.rotation;
        snapshot.linear_velocity = velocity.linear;
    }
}

/// Server-only marker on a carried car/tank — records which dropship (and
/// which of its `CARGO_OFFSETS` slots) it's slung under, so
/// `move_carried_vehicles` knows where to put it every tick. Removed again
/// on drop (`apply_drop_vehicle`).
#[derive(Component)]
struct Carried {
    dropship_entity: Entity,
    slot: usize,
}

/// `G`, near an eligible car/tank with a free cargo slot on the sender's
/// own currently-piloted dropship — latches it on. The sender names the
/// specific target (`PickupVehicleMsg::target`, the nearest one in range
/// from the client's own point of view — same "client names it, server
/// only validates" shape `EnterTurretMsg` uses); rejected, silently, if
/// it's already someone's cargo, out of range, or there's no free slot.
/// Switches the target's `RigidBody` to `KinematicPositionBased` — a
/// carried vehicle is no longer simulated as a free body at all, it's
/// directly positioned every tick by `move_carried_vehicles` instead
/// (Rapier simply ignores `ExternalForce`/`Velocity` on a kinematic body,
/// so whatever `step_cars`/`step_tanks` still computes for it every tick
/// harmlessly goes nowhere while carried).
fn apply_pickup_vehicle(
    pickup: On<FromClient<PickupVehicleMsg>>,
    identities: Res<PlayerIdentities>,
    mut commands: Commands,
    mut dropships: Query<(Entity, &Transform, &mut DropshipSnapshot)>,
    cars: Query<(Entity, &Transform, &CarChassis), Without<Carried>>,
    tanks: Query<(Entity, &Transform, &TankChassis), Without<Carried>>,
    carried_q: Query<&Carried>,
) {
    let Some(client_entity) = pickup.client_id.entity() else {
        return;
    };
    let Some(player_id) = identities.get(client_entity) else {
        return;
    };
    let Some((dropship_entity, dropship_tf, mut snapshot)) = dropships
        .iter_mut()
        .find(|(_, _, s)| s.dropship_id == pickup.dropship_id && s.owner_player_id == player_id)
    else {
        return;
    };
    let Some(empty_slot) = snapshot.cargo.iter().position(Option::is_none) else {
        return;
    };

    let target = match pickup.target {
        CargoVehicleId::Car(car_id) => cars.iter().find(|(_, _, c)| c.car_id == car_id).map(|(e, tf, _)| (e, tf.translation)),
        CargoVehicleId::Tank(tank_id) => {
            tanks.iter().find(|(_, _, c)| c.tank_id == tank_id).map(|(e, tf, _)| (e, tf.translation))
        }
    };
    let Some((target_entity, target_pos)) = target else {
        return;
    };
    if carried_q.get(target_entity).is_ok() || target_pos.distance(dropship_tf.translation) > PICKUP_RADIUS {
        return;
    }

    commands
        .entity(target_entity)
        .insert((RigidBody::KinematicPositionBased, Carried { dropship_entity, slot: empty_slot }));
    snapshot.cargo[empty_slot] = Some(pickup.target);
}

/// `G` again, releasing cargo the sender's own dropship is currently
/// carrying — hands the target back to ordinary dynamic physics, and
/// gives it the dropship's own current velocity (rather than zero) so it
/// falls away with the momentum it was actually moving at, not a jarring
/// dead stop in midair.
#[allow(clippy::too_many_arguments)]
fn apply_drop_vehicle(
    drop: On<FromClient<DropVehicleMsg>>,
    identities: Res<PlayerIdentities>,
    mut commands: Commands,
    mut dropships: Query<(&Velocity, &mut DropshipSnapshot)>,
    cars: Query<(Entity, &CarChassis)>,
    tanks: Query<(Entity, &TankChassis)>,
    carried_q: Query<&Carried>,
    mut velocities: Query<&mut Velocity, Without<DropshipSnapshot>>,
) {
    let Some(client_entity) = drop.client_id.entity() else {
        return;
    };
    let Some(player_id) = identities.get(client_entity) else {
        return;
    };
    let Some((dropship_velocity, mut snapshot)) = dropships
        .iter_mut()
        .find(|(_, s)| s.dropship_id == drop.dropship_id && s.owner_player_id == player_id)
    else {
        return;
    };
    let Some(slot) = snapshot.cargo.iter().position(|c| *c == Some(drop.target)) else {
        return;
    };

    let target_entity = match drop.target {
        CargoVehicleId::Car(car_id) => cars.iter().find(|(_, c)| c.car_id == car_id).map(|(e, _)| e),
        CargoVehicleId::Tank(tank_id) => tanks.iter().find(|(_, c)| c.tank_id == tank_id).map(|(e, _)| e),
    };
    let Some(target_entity) = target_entity else {
        snapshot.cargo[slot] = None;
        return;
    };
    // Only actually drop it if it's still recorded as carried by *this*
    // dropship — a stray/duplicate `DropVehicleMsg` (or one naming cargo
    // that was already dropped) is a no-op past this point rather than
    // yanking `RigidBody` off something it never touched.
    if carried_q.get(target_entity).is_ok() {
        commands.entity(target_entity).remove::<Carried>().insert(RigidBody::Dynamic);
        if let Ok(mut velocity) = velocities.get_mut(target_entity) {
            *velocity = *dropship_velocity;
        }
    }
    snapshot.cargo[slot] = None;
}

/// Positions every carried car/tank at its own `CARGO_OFFSETS` slot,
/// relative to whichever dropship is carrying it — the entire reason a
/// carried vehicle's `RigidBody` is `KinematicPositionBased` (see
/// `apply_pickup_vehicle`'s own docs): a kinematic body is moved by
/// directly setting its `Transform`, which is exactly what this does,
/// every tick, rather than needing any joint/constraint physics.
fn move_carried_vehicles(
    dropships: Query<&GlobalTransform, With<DropshipSnapshot>>,
    mut carried: Query<(&Carried, &mut Transform)>,
) {
    for (info, mut transform) in &mut carried {
        if let Ok(dropship_gt) = dropships.get(info.dropship_entity) {
            let dropship_transform = dropship_gt.compute_transform();
            let offset = CARGO_OFFSETS[info.slot];
            transform.translation = dropship_transform.transform_point(offset);
            transform.rotation = dropship_transform.rotation;
        }
    }
}
