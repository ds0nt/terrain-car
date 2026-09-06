use std::collections::{HashMap, HashSet};

use bevy::math::DVec3;
use bevy::prelude::*;
use bevy_rapier3d::prelude::*;
use bevy_replicon::prelude::*;
use bevy_replicon::shared::backend::connected_client::NetworkId;
use shared::car_physics::{
    apply_tuning, compute_wheel_forces, default_chassis, wheel_mounts, CarChassis, CarInput,
    CarInputState, WheelStepInput, CAR_ANGULAR_DAMPING, CAR_LINEAR_DAMPING, CAR_MASS,
};
use shared::combat::{Health, DEFAULT_MAX_HEALTH};
use shared::protocol::{
    spawn_car_signature, CarInputMsg, CarResetMsg, CarSnapshot, FlipUprightMsg, IdentifyMsg,
    RegenRequestMsg, TuneCarMsg, Wallet, WorldRegenMsg,
};
use uuid::Uuid;
use shared::terrain_gen::{find_flat_spawn, height_at, random_seed, TerrainNoise};
use shared::worldspace::WorldOrigin;

use crate::persistence::{Persistence, PersistenceCommand};
use crate::terrain_phys::{regenerate_terrain_colliders, ServerTerrainEntity};
use crate::weapons::LastFired;

/// Search radius for the very *first* car to ever spawn — needs room to
/// actually find flat ground since there's no existing player to anchor
/// near yet, matching the client's own `SPAWN_SEARCH_RADIUS` for the same
/// reason (the client's `LocalCar` searches with this same radius before
/// the server's echo arrives — see car.rs and protocol.rs).
const FIRST_SPAWN_SEARCH_RADIUS: f64 = 300.0;
/// Search radius for every subsequent car — small and deliberately so.
/// Originally this reused `FIRST_SPAWN_SEARCH_RADIUS` from a point only
/// `SPAWN_RING_SPACING` away from the previous player, but on "insane
/// terrain" a 300-unit flatness search from a barely-shifted center can
/// still land 100+ units from where it started (whichever direction
/// happens to be flattest) — which is exactly what caused two connected
/// players to spawn too far apart (and usually behind a hill) to ever see
/// each other. Bounding subsequent searches tightly, around the *actual*
/// position of the first car rather than a theoretical center, keeps every
/// player within sight of whoever connected first.
const NEARBY_SPAWN_SEARCH_RADIUS: f64 = 40.0;
/// Ring spacing for players after the first, so simultaneous connections
/// don't land on top of each other — see `pick_spawn_point`.
const SPAWN_RING_SPACING: f64 = 12.0;

pub struct CarSimPlugin;

impl Plugin for CarSimPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PlayerRegistry>()
            .init_resource::<SpawnAnchor>()
            .init_resource::<OpList>()
            .init_resource::<CurrentWorldState>()
            .init_resource::<PlayerIdentities>()
            .add_observer(spawn_car_on_connect)
            .add_observer(despawn_car_on_disconnect)
            .add_observer(apply_car_input)
            .add_observer(apply_car_reset)
            .add_observer(apply_flip_upright)
            .add_observer(apply_car_tune)
            .add_observer(apply_identify)
            .add_observer(apply_world_regen_request)
            .add_systems(FixedUpdate, step_cars.before(PhysicsSet::SyncBackend))
            .add_systems(Update, recover_lost_cars);
    }
}

/// Maps a small, operator-friendly player number (shown by the server
/// console's `list` command, used by `kick`/`op`/`deop`) to the underlying
/// client entity — friendlier than typing Bevy's raw `260v0`-style entity
/// ids at the console.
#[derive(Resource, Default)]
pub struct PlayerRegistry {
    next_index: u32,
    by_index: HashMap<u32, Entity>,
    by_entity: HashMap<Entity, u32>,
}

impl PlayerRegistry {
    fn assign(&mut self, client_entity: Entity) -> u32 {
        let index = self.next_index;
        self.next_index += 1;
        self.by_index.insert(index, client_entity);
        self.by_entity.insert(client_entity, index);
        index
    }

    fn remove(&mut self, client_entity: Entity) {
        if let Some(index) = self.by_entity.remove(&client_entity) {
            self.by_index.remove(&index);
        }
    }

    pub fn entity_for(&self, index: u32) -> Option<Entity> {
        self.by_index.get(&index).copied()
    }

    pub fn iter(&self) -> impl Iterator<Item = (u32, Entity)> + '_ {
        self.by_index.iter().map(|(&i, &e)| (i, e))
    }
}

/// Maps a connected client's live `Entity` to their stable, cross-session
/// `PersistentPlayerId` (see client's `net.rs`) — the bridge between
/// ephemeral per-connection state (`OwnedBy(Entity)`, which stops existing
/// the moment someone disconnects) and anything that needs to survive a
/// reconnect or a server restart (buildings/wallets, once those exist —
/// see the base-building plan). Populated by `apply_identify`, which the
/// client resends every couple of seconds rather than relying on a single
/// attempt racing connection setup — see `IdentifyMsg`'s own docs.
#[derive(Resource, Default)]
pub struct PlayerIdentities(HashMap<Entity, Uuid>);

impl PlayerIdentities {
    pub fn get(&self, client_entity: Entity) -> Option<Uuid> {
        self.0.get(&client_entity).copied()
    }

    fn remove(&mut self, client_entity: Entity) {
        self.0.remove(&client_entity);
    }
}

fn apply_identify(
    identify: On<FromClient<IdentifyMsg>>,
    mut identities: ResMut<PlayerIdentities>,
    persistence: Res<Persistence>,
) {
    let Some(client_entity) = identify.client_id.entity() else {
        return;
    };
    let is_new = identities.0.insert(client_entity, identify.player_id).is_none();
    if is_new {
        // Only worth a round-trip the first time this connection
        // identifies (IdentifyMsg is otherwise resent every couple of
        // seconds for the reasons in its own docs) — an `on conflict do
        // nothing` upsert either way, so a duplicate would be harmless,
        // just wasted.
        persistence.send(PersistenceCommand::UpsertPlayer(identify.player_id));
    }
}

/// Client entities currently holding operator privileges (granted/revoked
/// via the server console's `op`/`deop` commands — see console.rs). Purely
/// server-side and per-session: there's no persistent login to remember
/// this across a reconnect. Op-gated actions (currently just world
/// regeneration) always re-check this on the server; a client's own belief
/// about its op status, if it has one, is UX only.
#[derive(Resource, Default)]
pub struct OpList(HashSet<Entity>);

impl OpList {
    pub fn grant(&mut self, entity: Entity) {
        self.0.insert(entity);
    }

    pub fn revoke(&mut self, entity: Entity) {
        self.0.remove(&entity);
    }

    pub fn is_op(&self, entity: Entity) -> bool {
        self.0.contains(&entity)
    }
}

/// True-space position of the first car ever spawned this session (or
/// since the last regen) — every later player's spawn search is centered
/// near this (see `NEARBY_SPAWN_SEARCH_RADIUS`) so everyone lands within
/// sight of each other rather than each independently searching a wide
/// radius from scratch.
#[derive(Resource, Default)]
pub(crate) struct SpawnAnchor(Option<DVec3>);

/// The world seed as of the last regeneration, if any — `None` means the
/// world is still on `TerrainNoise::default()` (never regenerated), which
/// every fresh client already starts on too, so there's nothing to catch a
/// new joiner up on. Set whenever `do_world_regen` runs; read when a new
/// client connects, to catch them up on a regen that happened before they
/// joined (see `spawn_car_on_connect`).
#[derive(Resource, Default)]
pub(crate) struct CurrentWorldState {
    seed: Option<u32>,
}

/// Server-only bookkeeping (never replicated): which client entity a car
/// belongs to, so a disconnect can find and despawn the right car and an
/// incoming `CarInputMsg` can find the right car to update.
#[derive(Component)]
pub(crate) struct OwnedBy(pub(crate) Entity);

/// Picks where a car should spawn: a small ring around the current
/// `SpawnAnchor` for every player after the first (see module docs), or a
/// wide search from `origin.offset` for the very first — and, if this is
/// the first spawn since a regen (or ever), records the result as the new
/// anchor for subsequent calls.
fn pick_spawn_point(
    index: u32,
    anchor: &mut SpawnAnchor,
    noise: &TerrainNoise,
    origin: &WorldOrigin,
) -> DVec3 {
    let (near, search_radius) = match anchor.0 {
        Some(anchor_true) => {
            // A small ring around the anchor (golden-angle-ish spread) so
            // simultaneous connections don't all search from the exact
            // same point and risk landing on top of each other.
            let angle = index as f64 * 2.4;
            let radius = SPAWN_RING_SPACING * (1.0 + index as f64 * 0.4);
            let offset = DVec3::new(angle.cos() * radius, 0.0, angle.sin() * radius);
            (anchor_true + offset, NEARBY_SPAWN_SEARCH_RADIUS)
        }
        None => (origin.offset, FIRST_SPAWN_SEARCH_RADIUS),
    };

    let spawn_true = find_flat_spawn(noise, near, search_radius);
    if anchor.0.is_none() {
        anchor.0 = Some(spawn_true);
    }
    spawn_true
}

fn spawn_car_on_connect(
    add: On<Add, ConnectedClient>,
    mut commands: Commands,
    noise: Res<TerrainNoise>,
    origin: Res<WorldOrigin>,
    mut registry: ResMut<PlayerRegistry>,
    mut anchor: ResMut<SpawnAnchor>,
    world_state: Res<CurrentWorldState>,
    network_ids: Query<&NetworkId>,
) {
    let client_entity = add.entity;
    let mut chassis = default_chassis();
    // Spawned in the same bundle as `ConnectedClient` by the renet backend
    // (see bevy_replicon_renet's server.rs), so it's already present here.
    let network_id = network_ids
        .get(client_entity)
        .unwrap_or_else(|_| panic!("client `{client_entity}` has no NetworkId"))
        .get();
    // Also this player's car color (see CarChassis::color_seed's docs) —
    // reusing the connection id means the client's own local guess for its
    // predicted car already matches what the server assigns, no pop.
    chassis.color_seed = network_id as u32;

    let index = registry.assign(client_entity);
    let spawn_true = pick_spawn_point(index, &mut anchor, &noise, &origin);
    let ground_y = height_at(&noise, spawn_true.x, spawn_true.z);
    let local_spawn = (spawn_true - origin.offset).as_vec3();

    commands.spawn((
        Transform::from_xyz(local_spawn.x, ground_y + 2.0, local_spawn.z),
        RigidBody::Dynamic,
        Collider::cuboid(chassis.half_extents.x, chassis.half_extents.y, chassis.half_extents.z),
        AdditionalMassProperties::Mass(CAR_MASS),
        Velocity::zero(),
        ExternalForce::default(),
        Damping {
            linear_damping: CAR_LINEAR_DAMPING,
            angular_damping: CAR_ANGULAR_DAMPING,
        },
        Ccd::enabled(),
        chassis,
        CarInputState::default(),
        CarSnapshot::default(),
        (
            Health::full(DEFAULT_MAX_HEALTH),
            // Real value follows shortly via economy.rs's sync_wallet_components
            // once IdentifyMsg/the loaded wallet arrives — 0/0 in the
            // meantime is honest (this connection hasn't identified yet).
            Wallet::default(),
            LastFired::default(),
            OwnedBy(client_entity),
            Replicated,
            // `LocalCar(network_id)` matches what the client embedded in
            // its own pre-spawned entity (see protocol.rs's docs on why
            // this needs a real per-client value now, not a bare marker)
            // — never replicated (not `.replicate::<LocalCar>()`'d), so
            // this never leaks to other clients; it only drives the local
            // hash-matching that merges this entity into the connecting
            // client's own.
            spawn_car_signature(client_entity, network_id),
        ),
    ));

    // Catch-up: if the world was already regenerated before this client
    // connected, they'd otherwise never learn the new seed (a broadcast
    // WorldRegenMsg doesn't retroactively reach clients who weren't
    // connected when it was sent). Skipped when the world is still on its
    // untouched default — every fresh client already starts there too.
    if let Some(seed) = world_state.seed {
        commands.server_trigger(ToClients {
            targets: SendTargets::Single(ClientId::Client(client_entity)),
            message: WorldRegenMsg {
                seed,
                origin_x: origin.offset.x,
                origin_z: origin.offset.z,
            },
        });
    }

    info!("server: spawned car for client `{client_entity}` (player #{index})");
}

fn despawn_car_on_disconnect(
    remove: On<Remove, ConnectedClient>,
    mut commands: Commands,
    cars: Query<(Entity, &OwnedBy)>,
    mut registry: ResMut<PlayerRegistry>,
    mut op_list: ResMut<OpList>,
    mut identities: ResMut<PlayerIdentities>,
) {
    let client_entity = remove.entity;
    registry.remove(client_entity);
    op_list.revoke(client_entity);
    // Bevy can recycle a despawned Entity id for a later, unrelated
    // connection — leaving a stale mapping here would let that new
    // connection silently inherit a previous player's identity.
    identities.remove(client_entity);
    for (car_entity, owner) in &cars {
        if owner.0 == client_entity {
            commands.entity(car_entity).despawn();
            info!("server: despawned car for disconnected client `{client_entity}`");
        }
    }
}

/// Updates the owning car's buffered input whenever a client sends one.
/// Applied on the next `step_cars` tick rather than immediately — inputs
/// arrive at network speed, physics steps at a fixed 64 Hz, so there's
/// always a small buffer between "received" and "applied."
fn apply_car_input(
    input_msg: On<FromClient<CarInputMsg>>,
    mut cars: Query<(&OwnedBy, &mut CarInputState)>,
) {
    let Some(client_entity) = input_msg.client_id.entity() else {
        return;
    };
    for (owner, mut state) in &mut cars {
        if owner.0 == client_entity {
            state.input = CarInput {
                throttle: input_msg.throttle,
                steer: input_msg.steer,
                brake: input_msg.brake,
            };
            state.last_applied_sequence = input_msg.sequence;
            break;
        }
    }
}

/// Authoritative half of the R-key "unstick me" reset — searches for flat
/// ground the same way a fresh connect does, near the point the client's
/// own local reset already searched from, then bumps
/// `CarSnapshot::reset_generation` so the client can recognize the
/// resulting snapshot as "post-reset" rather than a huge, real desync (see
/// `CarSnapshot`'s docs).
fn apply_car_reset(
    reset_msg: On<FromClient<CarResetMsg>>,
    noise: Res<TerrainNoise>,
    origin: Res<WorldOrigin>,
    mut cars: Query<(
        &OwnedBy,
        &mut Transform,
        &mut Velocity,
        &mut ExternalForce,
        &mut CarSnapshot,
    )>,
) {
    let Some(client_entity) = reset_msg.client_id.entity() else {
        return;
    };
    for (owner, mut transform, mut velocity, mut ext_force, mut snapshot) in &mut cars {
        if owner.0 != client_entity {
            continue;
        }

        let near = DVec3::new(reset_msg.near_true_x, 0.0, reset_msg.near_true_z);
        let spawn_true = find_flat_spawn(&noise, near, FIRST_SPAWN_SEARCH_RADIUS);
        let ground_y = height_at(&noise, spawn_true.x, spawn_true.z);
        let local_spawn = (spawn_true - origin.offset).as_vec3();

        transform.translation = Vec3::new(local_spawn.x, ground_y + 2.0, local_spawn.z);
        transform.rotation = Quat::IDENTITY;
        *velocity = Velocity::zero();
        *ext_force = ExternalForce::default();
        snapshot.reset_generation = snapshot.reset_generation.wrapping_add(1);
        break;
    }
}

/// Authoritative half of R's "right the car in place" — see client's
/// `flip_car_upright` docs for why this is a separate message/handler
/// from `apply_car_reset` rather than reusing it: no search, just
/// recompute ground height at the exact point the client gave (its own
/// current position) and correct orientation there.
fn apply_flip_upright(
    flip_msg: On<FromClient<FlipUprightMsg>>,
    noise: Res<TerrainNoise>,
    mut cars: Query<(
        &OwnedBy,
        &mut Transform,
        &mut Velocity,
        &mut ExternalForce,
        &mut CarSnapshot,
    )>,
) {
    let Some(client_entity) = flip_msg.client_id.entity() else {
        return;
    };
    for (owner, mut transform, mut velocity, mut ext_force, mut snapshot) in &mut cars {
        if owner.0 != client_entity {
            continue;
        }
        let ground_y = height_at(&noise, flip_msg.true_x, flip_msg.true_z);
        transform.translation.y = ground_y + 2.0;
        transform.rotation = Quat::IDENTITY;
        *velocity = Velocity::zero();
        *ext_force = ExternalForce::default();
        snapshot.reset_generation = snapshot.reset_generation.wrapping_add(1);
        break;
    }
}

/// Authoritative half of the live tuning panel (`Tab`, client's
/// `tuning_ui.rs`): a player can only ever tune their own car (found via
/// `OwnedBy`, same authorization shape `RegenRequestMsg`'s op-check already
/// uses, just scoped to "yourself" rather than "an op"), and every field is
/// re-clamped here via `apply_tuning` regardless of what the UI already
/// enforced — the client-side ranges are for slider feel, never trusted as
/// the actual boundary. `CarChassis` is continuously replicated (see
/// `register_protocol`), so the new values reach every other connected
/// client automatically, the same way any other component change would.
fn apply_car_tune(
    tune_msg: On<FromClient<TuneCarMsg>>,
    mut cars: Query<(&OwnedBy, &mut CarChassis)>,
) {
    let Some(client_entity) = tune_msg.client_id.entity() else {
        return;
    };
    for (owner, mut chassis) in &mut cars {
        if owner.0 != client_entity {
            continue;
        }
        apply_tuning(
            &mut chassis,
            tune_msg.spring_stiffness,
            tune_msg.damper,
            tune_msg.engine_force,
            tune_msg.brake_force,
            tune_msg.traction,
            tune_msg.max_steer_rad,
        );
        break;
    }
}

/// Below this altitude (or a non-finite position/velocity), a car is
/// considered lost rather than legitimately airborne — real terrain never
/// goes anywhere near this low (worst case is roughly
/// `-(CONTINENTAL_SCALE + CANYON_DEPTH)`, a bit over -1500) — and gets
/// recovered rather than left to fall forever. This is a hard safety net
/// against *any* cause of "no collider under this car" (a missing-collider
/// bug like the terrain preload not following a world regen, a one-tick
/// gap between queuing new colliders via `Commands` and Rapier actually
/// syncing them, or anything else we haven't thought of) rather than a fix
/// for one specific trigger — a car that's merely fallen off a cliff will
/// keep falling past this line too, but "briefly airborne, then reset"
/// beats what an actual missing-collider bug did last time: an unbounded
/// fall whose ever-more-extreme position also broke client-side
/// reconciliation, which corrects *toward* the server's snapshot no matter
/// how absurd it is, producing the wild altitude oscillation this was
/// built to catch.
const LOST_CAR_ALTITUDE_FLOOR: f32 = -5_000.0;

fn recover_lost_cars(
    noise: Res<TerrainNoise>,
    origin: Res<WorldOrigin>,
    mut cars: Query<(&mut Transform, &mut Velocity, &mut ExternalForce, &mut CarSnapshot)>,
) {
    for (mut transform, mut velocity, mut ext_force, mut snapshot) in &mut cars {
        let lost = !transform.translation.is_finite()
            || !velocity.linear.is_finite()
            || transform.translation.y < LOST_CAR_ALTITUDE_FLOOR;
        if !lost {
            continue;
        }

        warn!(
            "server: recovering a lost car (was at {:?})",
            transform.translation
        );
        let spawn_true = find_flat_spawn(&noise, origin.offset, FIRST_SPAWN_SEARCH_RADIUS);
        let ground_y = height_at(&noise, spawn_true.x, spawn_true.z);
        let local_spawn = (spawn_true - origin.offset).as_vec3();

        transform.translation = Vec3::new(local_spawn.x, ground_y + 2.0, local_spawn.z);
        transform.rotation = Quat::IDENTITY;
        *velocity = Velocity::zero();
        *ext_force = ExternalForce::default();
        snapshot.reset_generation = snapshot.reset_generation.wrapping_add(1);
    }
}

/// Op-gated: a regular player's `RegenRequestMsg` is simply denied. The
/// server console's own `regen` command (console.rs) is inherently
/// authorized (whoever runs the server) and calls `do_world_regen`
/// directly, bypassing this observer entirely.
fn apply_world_regen_request(
    request: On<FromClient<RegenRequestMsg>>,
    op_list: Res<OpList>,
    mut commands: Commands,
    mut noise: ResMut<TerrainNoise>,
    mut origin: ResMut<WorldOrigin>,
    mut anchor: ResMut<SpawnAnchor>,
    mut world_state: ResMut<CurrentWorldState>,
    terrain_entities: Query<Entity, With<ServerTerrainEntity>>,
    mut cars: Query<(&mut Transform, &mut Velocity, &mut ExternalForce, &mut CarSnapshot), With<OwnedBy>>,
) {
    let Some(client_entity) = request.client_id.entity() else {
        return;
    };
    if !op_list.is_op(client_entity) {
        warn!("server: client `{client_entity}` requested world regen without op privileges — denied");
        return;
    }

    let msg = do_world_regen(
        &mut commands,
        &mut noise,
        &mut origin,
        &mut anchor,
        &mut world_state,
        &terrain_entities,
        &mut cars,
    );
    commands.server_trigger(ToClients {
        targets: SendTargets::All,
        message: msg,
    });
    info!(
        "server: world regenerated (seed={}) by op client `{client_entity}`",
        msg.seed
    );
}

/// Core of a world regeneration, shared by the op-gated in-game request
/// (`apply_world_regen_request`) and the server console's `regen` command
/// (console.rs) — reseeds terrain, rebuilds every collider/obstacle to
/// match, and repositions every currently-connected car onto the new
/// terrain (same ring-around-an-anchor placement fresh connections use, so
/// everyone stays in sight of each other after a regen too). Returns the
/// message to broadcast; the caller decides how (see the two call sites).
pub(crate) fn do_world_regen(
    commands: &mut Commands,
    noise: &mut TerrainNoise,
    origin: &mut WorldOrigin,
    anchor: &mut SpawnAnchor,
    world_state: &mut CurrentWorldState,
    terrain_entities: &Query<Entity, With<ServerTerrainEntity>>,
    cars: &mut Query<(&mut Transform, &mut Velocity, &mut ExternalForce, &mut CarSnapshot), With<OwnedBy>>,
) -> WorldRegenMsg {
    let seed = random_seed();
    *noise = TerrainNoise::from_seed(seed);
    world_state.seed = Some(seed);

    // Land somewhere new and reset the floating origin — mirrors the
    // client's own single-player regenerate_terrain formula, so "N" feels
    // the same (a fresh random-ish region) whether it's driven locally or,
    // now, through the server.
    let spawn_x = ((seed % 20_000) as f64) - 10_000.0;
    let spawn_z = (((seed / 20_000) % 20_000) as f64) - 10_000.0;
    origin.offset = DVec3::new(spawn_x, 0.0, spawn_z);

    regenerate_terrain_colliders(commands, noise, origin, terrain_entities);

    anchor.0 = None;
    for (index, (mut transform, mut velocity, mut ext_force, mut snapshot)) in
        cars.iter_mut().enumerate()
    {
        let spawn_true = pick_spawn_point(index as u32, anchor, noise, origin);
        let ground_y = height_at(noise, spawn_true.x, spawn_true.z);
        let local = (spawn_true - origin.offset).as_vec3();

        transform.translation = Vec3::new(local.x, ground_y + 2.0, local.z);
        transform.rotation = Quat::IDENTITY;
        *velocity = Velocity::zero();
        *ext_force = ExternalForce::default();
        snapshot.reset_generation = snapshot.reset_generation.wrapping_add(1);
    }

    WorldRegenMsg {
        seed,
        origin_x: origin.offset.x,
        origin_z: origin.offset.z,
    }
}

/// The authoritative suspension/drive step, run once per car per fixed
/// tick. No child `Wheel` entities needed here (unlike the client, which
/// also needs them for wheel meshes and spin animation) — wheel mount
/// geometry is a pure function of `chassis.half_extents`
/// (`shared::car_physics::wheel_mounts`), so this just iterates it inline.
fn step_cars(
    time: Res<Time>,
    rapier_context: ReadRapierContext,
    mut cars_q: Query<(
        Entity,
        &GlobalTransform,
        &Velocity,
        &mut ExternalForce,
        &CarChassis,
        &CarInputState,
        &mut CarSnapshot,
    )>,
) {
    let Ok(context) = rapier_context.single() else {
        return;
    };
    let dt = time.delta_secs();
    if dt <= 0.0 {
        return;
    }

    for (entity, chassis_gt, velocity, mut ext_force, chassis, input_state, mut snapshot) in
        &mut cars_q
    {
        let transform = chassis_gt.compute_transform();
        let center_of_mass = transform.translation;
        let up = transform.up();
        let forward = transform.forward();
        let right = transform.right();
        let input = input_state.input;

        let mut total_force = Vec3::ZERO;
        let mut total_torque = Vec3::ZERO;

        for (offset, is_front) in wheel_mounts(chassis.half_extents) {
            let ray_origin = transform.transform_point(offset);
            let ray_dir = -up;
            let max_toi = chassis.rest_length + chassis.wheel_radius;

            let hit = context.cast_ray_and_get_normal(
                ray_origin,
                *ray_dir,
                max_toi,
                true,
                QueryFilter::new().exclude_rigid_body(entity),
            );

            let steer_angle = if is_front {
                input.steer * chassis.max_steer_rad
            } else {
                0.0
            };
            let wheel_forward = Quat::from_axis_angle(*up, steer_angle) * *forward;
            let wheel_right = Quat::from_axis_angle(*up, steer_angle) * *right;

            if let Some((_hit_entity, intersection)) = hit {
                let suspension_len = (intersection.time_of_impact - chassis.wheel_radius)
                    .max(0.0)
                    .min(chassis.rest_length);
                let compression = chassis.rest_length - suspension_len;
                let point_velocity =
                    velocity.linear_velocity_at_point(intersection.point, center_of_mass);
                let closing_speed = point_velocity.dot(*up);
                let arm = intersection.point - center_of_mass;

                let out = compute_wheel_forces(
                    chassis,
                    &WheelStepInput {
                        compression,
                        closing_speed,
                        point_velocity,
                        up: *up,
                        wheel_forward,
                        wheel_right,
                        arm,
                        throttle: input.throttle,
                        brake: input.brake,
                    },
                );
                total_force += out.force;
                total_torque += out.torque;
            }
        }

        ext_force.force = total_force;
        ext_force.torque = total_torque;

        // One tick stale (reflects position before this tick's force is
        // integrated by Rapier, which hasn't stepped yet at this point in
        // the schedule) — immaterial at network-RTT timescales, and
        // refreshed every tick regardless.
        snapshot.translation = transform.translation;
        snapshot.rotation = transform.rotation;
        snapshot.linear_velocity = velocity.linear;
        snapshot.angular_velocity = velocity.angular;
        snapshot.last_input_sequence = input_state.last_applied_sequence;
    }
}
