use std::collections::{HashMap, HashSet};

use bevy::math::DVec3;
use bevy::prelude::*;
use bevy_rapier3d::prelude::*;
use bevy_replicon::prelude::*;
use shared::buildings::BuildingKind;
use shared::car_physics::{
    compute_wheel_forces, default_chassis, wheel_mounts, CarChassis, CarInput, CarInputState,
    WheelStepInput, CAR_ANGULAR_DAMPING, CAR_LINEAR_DAMPING, CAR_MASS,
};
use shared::combat::{Health, DEFAULT_MAX_HEALTH};
use shared::protocol::{
    BoardPassengerMsg, BuildingSnapshot, CarCosmetics, CarInputMsg, CarResetMsg, CarSnapshot,
    ExitPassengerMsg, FlipUprightMsg, PlayerOnFootSnapshot, PlayerPositionMsg, RecallPlayerMsg, RecallToHangarMsg,
    RegenRequestMsg, SetCosmeticsMsg, TeleportMsg, WorldRegenMsg,
};
use uuid::Uuid;
use shared::terrain_gen::{find_flat_spawn, height_at, random_seed, TerrainNoise};
use shared::time::now_unix;
use shared::worldspace::WorldOrigin;

use crate::persistence::{Persistence, PersistenceCommand, PositionRow};
use crate::terrain_phys::{regenerate_terrain_colliders, LoadedChunks};
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
            .init_resource::<PlayerPositions>()
            .init_resource::<SpawnedCarsFor>()
            .add_observer(register_client_on_connect)
            .add_observer(despawn_player_on_disconnect)
            .add_observer(apply_car_input)
            .add_observer(apply_player_position)
            .add_observer(apply_car_reset)
            .add_observer(apply_flip_upright)
            .add_observer(apply_board_passenger)
            .add_observer(apply_exit_passenger)
            .add_observer(apply_set_cosmetics)
            .add_observer(apply_recall_to_hangar)
            .add_observer(apply_recall_player)
            .add_observer(apply_world_regen_request)
            .add_systems(Update, spawn_cars_from_hangars)
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

    pub fn index_for(&self, client_entity: Entity) -> Option<u32> {
        self.by_entity.get(&client_entity).copied()
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

    /// Reverse of `get` — which connection (if any) a durable `player_id`
    /// is currently logged in as. Small enough player counts that a linear
    /// scan is fine; used by `server::chat` to find a teleported player's
    /// own connection to notify (`TeleportMsg`), which only matters if
    /// they're actually online right now.
    pub fn entity_for(&self, player_id: Uuid) -> Option<Entity> {
        self.0.iter().find_map(|(&entity, &id)| (id == player_id).then_some(entity))
    }

    /// Called once by `server::auth` right after a successful
    /// login/register — the sole place a client entity ever earns a
    /// `PersistentPlayerId` now that nothing is simply trusted off the
    /// wire (see `AuthResultMsg`'s docs).
    pub(crate) fn insert(&mut self, client_entity: Entity, player_id: Uuid) {
        self.0.insert(client_entity, player_id);
    }

    fn remove(&mut self, client_entity: Entity) {
        self.0.remove(&client_entity);
    }
}

/// Server-side counterpart to `client::pilot::PlayerFocus` — the latest
/// true-space position each identified player reported via
/// `PlayerPositionMsg` (see that message's own docs), keyed by their
/// durable `player_id` like `Wallets`/`VillagerQueues`, not by connection
/// entity: this is what distance-bound actions (`economy::apply_place_building`)
/// check now instead of guessing from a parked car/plane, and it's also
/// what gets persisted so a returning player resumes where they left off
/// (see `server::auth`'s login flow).
#[derive(Resource, Default)]
pub struct PlayerPositions(HashMap<Uuid, (f64, f64)>);

impl PlayerPositions {
    pub fn get(&self, player_id: Uuid) -> Option<(f64, f64)> {
        self.0.get(&player_id).copied()
    }

    /// Every currently-known player position, car/plane/on-foot/passenger
    /// alike (see this resource's own docs) — `terrain_phys.rs`'s
    /// `stream_terrain_chunks` uses this to decide which terrain colliders
    /// need to exist, instead of only tracking cars the way it originally
    /// did (which left a flying plane with no terrain collision at all once
    /// it wandered far enough from wherever its owner's car happened to be
    /// parked — reported live as "terrain stops having collision... from
    /// the airplane").
    pub fn positions(&self) -> impl Iterator<Item = (f64, f64)> + '_ {
        self.0.values().copied()
    }

    /// `pub(crate)` — only `apply_player_position` (below) and
    /// `server::auth`'s initial seed-on-login ever need to write this.
    pub(crate) fn set(&mut self, player_id: Uuid, position: (f64, f64)) {
        self.0.insert(player_id, position);
    }
}

/// Updates `PlayerPositions` whenever a `PlayerPositionMsg` arrives —
/// unconditional on `ControlMode`, since the client sends this every tick
/// regardless of car/plane/on-foot (see that message's own docs). Also
/// mirrors it onto the sender's own `PlayerOnFootSnapshot` (on their
/// account entity, found via `OwnedBy` the same way `pings.rs` already
/// finds a player's car) so every *other* client can render their on-foot
/// avatar too — see that component's own docs on why this didn't exist at
/// all before, reported live as not being able to see a friend walking
/// around, or any distance/nametag indicator for them, at all.
fn apply_player_position(
    position_msg: On<FromClient<PlayerPositionMsg>>,
    identities: Res<PlayerIdentities>,
    mut positions: ResMut<PlayerPositions>,
    mut accounts: Query<(&OwnedBy, &mut PlayerOnFootSnapshot)>,
) {
    let Some(client_entity) = position_msg.client_id.entity() else {
        return;
    };
    let Some(player_id) = identities.get(client_entity) else {
        return;
    };
    positions.set(player_id, (position_msg.true_x, position_msg.true_z));

    for (owner, mut snapshot) in &mut accounts {
        if owner.0 == client_entity {
            snapshot.true_x = position_msg.true_x;
            snapshot.true_z = position_msg.true_z;
            snapshot.rotation_y = position_msg.rotation_y;
            snapshot.on_foot = position_msg.on_foot;
            break;
        }
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
    pub(crate) seed: Option<u32>,
}

/// Server-only bookkeeping (never replicated): which client entity a car
/// belongs to, so a disconnect can find and despawn the right car and an
/// incoming `CarInputMsg` can find the right car to update.
#[derive(Component)]
pub(crate) struct OwnedBy(pub(crate) Entity);

/// Picks where a *player* (not a car — see module docs on why those are no
/// longer the same thing) should start: a small ring around the current
/// `SpawnAnchor` for every player after the first, or a wide search from
/// `origin.offset` for the very first — and, if this is the first spawn
/// since a regen (or ever), records the result as the new anchor for
/// subsequent calls. `pub(crate)` so `server::auth` can call it directly
/// for a fresh login's starter villager and on-foot spawn point.
pub(crate) fn pick_spawn_point(
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

/// Just assigns the operator-friendly player number (see `PlayerRegistry`)
/// the moment a connection is established — independent of login, since an
/// admin should be able to see/kick a connection that's just sitting on
/// the login screen. No car exists yet; see `spawn_car_for`, called once
/// this client actually authenticates (`server::auth`).
fn register_client_on_connect(add: On<Add, ConnectedClient>, mut registry: ResMut<PlayerRegistry>) {
    let client_entity = add.entity;
    let index = registry.assign(client_entity);
    info!("server: client `{client_entity}` connected (player #{index}, not yet logged in)");
}

/// Which `BuildingSnapshot::id`s have already produced their one car — same
/// in-memory-only, per-restart tradeoff `aircraft.rs`'s `SpawnedPlanesFor`
/// already accepts for planes (see that resource's own docs).
#[derive(Resource, Default)]
struct SpawnedCarsFor(HashSet<Uuid>);

/// How far above a Hangar's own roof (its `ground_y` plus its full
/// collider height — see `shared::buildings::collider_shape`) a spawned
/// car's *center* sits — matches the flat `+ 2.0` ground clearance every
/// other car-placement path here already uses (`apply_car_reset`,
/// `apply_flip_upright`, `apply_recall_to_hangar`), just measured from the
/// roof instead of raw terrain.
const CAR_ROOF_CLEARANCE: f32 = 2.0;

/// One car per completed `Hangar`, full parity with
/// `aircraft.rs::spawn_planes_from_air_factories` — see `BuildingKind::Hangar`'s
/// docs for why a car is no longer a free, automatic login gift. A player
/// can own several, exactly like several planes (see `CarChassis::car_id`'s
/// docs on how driving/recall target one specific owned car once there's
/// more than one).
///
/// Spawns directly on the Hangar's own roof (its footprint center, at roof
/// height) rather than beside it at ground level — reported live as
/// wanting the car to actually appear on top of the building it came out
/// of. No clearance search against other cars needed here the way there
/// used to be: `SpawnedCarsFor` already guarantees each Hangar produces
/// its one car exactly once, so there's never a second car spawning onto
/// the same roof to collide with.
fn spawn_cars_from_hangars(
    mut spawned: ResMut<SpawnedCarsFor>,
    mut commands: Commands,
    origin: Res<WorldOrigin>,
    buildings: Query<&BuildingSnapshot>,
) {
    let now = now_unix();
    for building in &buildings {
        if building.kind != BuildingKind::Hangar
            || building.build_complete_at > now
            || spawned.0.contains(&building.id)
        {
            continue;
        }
        spawned.0.insert(building.id);

        let roof_y = building.ground_y
            + 2.0 * shared::buildings::collider_shape(BuildingKind::Hangar).half_height()
            + CAR_ROOF_CLEARANCE;
        spawn_car_for(&mut commands, &origin, building.owner_player_id, building.true_x, building.true_z, roof_y);
        info!("server: spawned a car for `{}` on top of their Hangar", building.owner_player_id);
    }
}

/// Spawns the authoritative car for a completed Hangar at `(true_x,
/// true_z, spawn_y)` — the caller's job to resolve (see
/// `spawn_cars_from_hangars`, its only caller, which places this on the
/// Hangar's own roof) — also records `home_true_x`/`home_true_z` on its
/// `CarSnapshot`, `apply_recall_to_hangar`'s (`H`) fallback if every
/// Hangar this player owns has since been destroyed (see that function's
/// own docs — its primary target is now whichever owned Hangar is nearest
/// at recall time, not this fixed spawn point).
///
/// No `OwnedBy`/connection-entity dependency at all: a Hangar can complete
/// while its owner isn't even connected (unlike the old login-time spawn,
/// which always had a live client entity in hand), so ownership is purely
/// `CarChassis::owner_player_id` — the same durable-id-based ownership
/// `PlaneSnapshot` already uses, not the ephemeral-per-connection `OwnedBy`
/// every other car-owning system in this file used to key off of (see
/// `apply_car_input`'s own docs for the knock-on effect: no more signature-
/// based client-side predicted spawn either, since there's no longer a
/// connection to predict *for* at spawn time).
/// `pub(crate)` — `server::ai`'s patrol starter reuses this exact bundle
/// for an unowned/AI-controlled car rather than duplicating it, the same
/// way `aircraft::spawn_plane_at` is shared with a real Air Factory spawn.
pub(crate) fn spawn_car_for(
    commands: &mut Commands,
    origin: &WorldOrigin,
    owner_player_id: Uuid,
    true_x: f64,
    true_z: f64,
    spawn_y: f32,
) -> Entity {
    let mut chassis = default_chassis();
    // Hashed from the account id — see `CarChassis::color_seed`'s docs.
    chassis.color_seed = shared::owner::seed_from_uuid(owner_player_id);
    chassis.owner_player_id = owner_player_id;
    chassis.car_id = Uuid::new_v4();

    let local_spawn = (DVec3::new(true_x, 0.0, true_z) - origin.offset).as_vec3();

    commands
        .spawn((
            Transform::from_xyz(local_spawn.x, spawn_y, local_spawn.z),
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
            CarSnapshot { home_true_x: true_x, home_true_z: true_z, ..Default::default() },
            (
                Health::full(DEFAULT_MAX_HEALTH),
                LastFired::default(),
                Replicated,
                // Starts at defaults (automatic owner-hash color, no bow) —
                // see `apply_set_cosmetics` for how a player changes this.
                CarCosmetics::default(),
            ),
        ))
        .id()
}

/// Teleports the sender's currently-driven car (`recall.car_id`, see
/// `CarChassis::car_id`'s docs) to whichever `Hangar` they own is nearest
/// to it right now — `H`. Falls back to the car's own recorded spawn
/// point (`CarSnapshot::home_true_x`/`home_true_z`, set once at
/// `spawn_car_for`) only if every owned Hangar has since been destroyed,
/// so recall still does *something* sensible rather than silently
/// failing. See `RecallToHangarMsg`'s own docs for why "nearest" replaced
/// the original "back to whichever Hangar spawned this car" behavior —
/// reported live as wanting the nearest one, especially once a player can
/// own several Hangars (and, via repeated dev-restart Hangar respawns,
/// several cars whose recorded home may by now be far away). Same
/// position/velocity/force reset `apply_car_reset` uses. `break`s the
/// instant it finds the matching car since `car_id` is unique — nothing
/// else in the query could ever match again.
fn apply_recall_to_hangar(
    recall: On<FromClient<RecallToHangarMsg>>,
    identities: Res<PlayerIdentities>,
    noise: Res<TerrainNoise>,
    origin: Res<WorldOrigin>,
    rapier_context: ReadRapierContext,
    buildings: Query<&BuildingSnapshot>,
    mut cars: Query<(Entity, &CarChassis, &mut Transform, &mut Velocity, &mut ExternalForce, &CarSnapshot)>,
) {
    let Some(client_entity) = recall.client_id.entity() else {
        return;
    };
    let Some(player_id) = identities.get(client_entity) else {
        return;
    };

    for (entity, chassis, mut transform, mut velocity, mut ext_force, snapshot) in &mut cars {
        if chassis.owner_player_id != player_id || chassis.car_id != recall.car_id {
            continue;
        }

        let car_true = origin.to_true(transform.translation);
        let nearest_hangar = buildings
            .iter()
            .filter(|b| b.kind == BuildingKind::Hangar && b.owner_player_id == player_id)
            .min_by(|a, b| {
                let da = (a.true_x - car_true.x).powi(2) + (a.true_z - car_true.z).powi(2);
                let db = (b.true_x - car_true.x).powi(2) + (b.true_z - car_true.z).powi(2);
                da.total_cmp(&db)
            });

        // Landing back on the Hangar's own roof (matching where it
        // originally spawned — see `spawn_cars_from_hangars`) if one still
        // exists, at that exact building's own `ground_y` rather than a
        // fresh raycast — no risk of the recall target disagreeing with
        // where the roof actually is. Falls back to a real physics
        // raycast (`economy::surface_height_at`, not a bare `height_at`)
        // only when every owned Hangar is gone: this point might now be
        // anything (bare terrain, another player's structure), so this is
        // the one case where a fresh raycast is the right call, same
        // reasoning `apply_flip_upright` uses.
        let (target_true_x, target_true_z, target_y) = match nearest_hangar {
            Some(hangar) => (
                hangar.true_x,
                hangar.true_z,
                hangar.ground_y + 2.0 * shared::buildings::collider_shape(BuildingKind::Hangar).half_height(),
            ),
            None => {
                let y = match rapier_context.single() {
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
                (snapshot.home_true_x, snapshot.home_true_z, y)
            }
        };

        let local = (DVec3::new(target_true_x, 0.0, target_true_z) - origin.offset).as_vec3();
        transform.translation = Vec3::new(local.x, target_y + CAR_ROOF_CLEARANCE, local.z);
        transform.rotation = Quat::IDENTITY;
        *velocity = Velocity::zero();
        *ext_force = ExternalForce::default();
        break;
    }
}

/// `H` while on foot — recalls the player themself (not a car or plane;
/// see `RecallPlayerMsg`'s own docs) to whichever owned Hangar, Land
/// Factory, or Air Factory is nearest right now — any of the three,
/// unlike `apply_recall_to_hangar`/`aircraft::apply_recall_plane`, which
/// each only ever look at their own matching kind (a car/plane recalling
/// to a *building* of some unrelated kind wouldn't make sense the way it
/// does for a walking player, who has no vehicle-specific reason to
/// prefer one over the other). Reuses `TeleportMsg`, the exact same
/// mechanism `server::chat`'s `/tp`/`/respawn` already use to relocate an
/// on-foot avatar — see that message's own docs on why cars/planes don't
/// need it but a walking player does. A no-op if the player owns none of
/// the three yet, or if their position isn't known yet (the same brief
/// post-login window `economy::apply_place_building`'s own docs describe).
fn apply_recall_player(
    recall: On<FromClient<RecallPlayerMsg>>,
    identities: Res<PlayerIdentities>,
    mut positions: ResMut<PlayerPositions>,
    buildings: Query<&BuildingSnapshot>,
    mut commands: Commands,
) {
    let Some(client_entity) = recall.client_id.entity() else {
        return;
    };
    let Some(player_id) = identities.get(client_entity) else {
        return;
    };
    let Some((current_x, current_z)) = positions.get(player_id) else {
        return;
    };

    let nearest = buildings
        .iter()
        .filter(|b| {
            b.owner_player_id == player_id
                && matches!(b.kind, BuildingKind::Hangar | BuildingKind::LandFactory | BuildingKind::AirFactory)
        })
        .min_by(|a, b| {
            let da = (a.true_x - current_x).powi(2) + (a.true_z - current_z).powi(2);
            let db = (b.true_x - current_x).powi(2) + (b.true_z - current_z).powi(2);
            da.total_cmp(&db)
        });
    let Some(building) = nearest else {
        return;
    };

    positions.set(player_id, (building.true_x, building.true_z));
    commands.server_trigger(ToClients {
        targets: SendTargets::Single(ClientId::Client(client_entity)),
        message: TeleportMsg { true_x: building.true_x, true_z: building.true_z },
    });
}

/// Despawns the disconnecting client's `PlayerAccount` entity (`Wallet`,
/// `PlayerInfo`, `VillagerQueue` — see `server::auth`'s docs on why those
/// now live on their own entity, ephemeral-per-connection exactly like a
/// car used to be) — *not* their car(s), which are a possession now, not a
/// session-scoped prediction target, and stay parked right where they were,
/// full parity with how a `Scout Plane` already survives its pilot logging
/// off.
fn despawn_player_on_disconnect(
    remove: On<Remove, ConnectedClient>,
    mut commands: Commands,
    accounts: Query<(Entity, &OwnedBy)>,
    mut cars: Query<&mut CarSnapshot>,
    mut registry: ResMut<PlayerRegistry>,
    mut op_list: ResMut<OpList>,
    mut identities: ResMut<PlayerIdentities>,
    positions: Res<PlayerPositions>,
    persistence: Res<Persistence>,
) {
    let client_entity = remove.entity;
    registry.remove(client_entity);
    op_list.revoke(client_entity);
    // Read before `identities.remove` below erases the mapping — this is
    // the last moment this disconnecting connection's `player_id` is
    // knowable at all, and the last chance to persist wherever
    // `PlayerPositions` last saw them (car, plane, or on foot alike) so
    // the next login can resume there instead of a fresh spawn point.
    if let Some(player_id) = identities.get(client_entity) {
        if let Some((true_x, true_z)) = positions.get(player_id) {
            persistence.send(PersistenceCommand::SavePosition(PositionRow { player_id, true_x, true_z }));
        }
        // A passenger who disconnects mid-ride never sends
        // `ExitPassengerMsg` — without this, their seat would stay
        // permanently occupied (from every other client's point of view,
        // and blocking anyone else from boarding) until the server
        // restarts.
        for mut snapshot in &mut cars {
            if snapshot.passenger_player_id == Some(player_id) {
                snapshot.passenger_player_id = None;
            }
        }
    }
    // Bevy can recycle a despawned Entity id for a later, unrelated
    // connection — leaving a stale mapping here would let that new
    // connection silently inherit a previous player's identity.
    identities.remove(client_entity);
    for (account_entity, owner) in &accounts {
        if owner.0 == client_entity {
            commands.entity(account_entity).despawn();
            info!("server: despawned player account for disconnected client `{client_entity}`");
        }
    }
}

/// Updates the owning car's buffered input whenever a client sends one.
/// Applied on the next `step_cars` tick rather than immediately — inputs
/// arrive at network speed, physics steps at a fixed 64 Hz, so there's
/// always a small buffer between "received" and "applied." Matched via
/// `CarChassis::owner_player_id` *and* `car_id`, not `OwnedBy` — see
/// `spawn_car_for`'s docs on why a car can no longer assume a live
/// connection entity, and `CarChassis::car_id`'s docs on why matching
/// `owner_player_id` alone (the original shape here) isn't enough once a
/// player can own more than one car: it drove every owned car at once,
/// reported live as "when I join a car it should be the correct car."
/// Checking `car_id` isn't itself a security hole even though `CarChassis`
/// (and its `car_id`) is visible to every client via replication — the
/// `owner_player_id` check still requires it to be *your own* car_id, so
/// naming someone else's only ever matches nothing.
fn apply_car_input(
    input_msg: On<FromClient<CarInputMsg>>,
    identities: Res<PlayerIdentities>,
    mut cars: Query<(&CarChassis, &mut CarInputState)>,
) {
    let Some(client_entity) = input_msg.client_id.entity() else {
        return;
    };
    let Some(player_id) = identities.get(client_entity) else {
        return;
    };
    for (chassis, mut state) in &mut cars {
        if chassis.owner_player_id == player_id && chassis.car_id == input_msg.car_id {
            state.input = CarInput {
                throttle: input_msg.throttle,
                steer: input_msg.steer,
                brake: input_msg.brake,
                boost: input_msg.boost,
            };
        }
    }
}

/// Authoritative half of the R-key "unstick me" reset — searches for flat
/// ground the same way a fresh connect does, near the point the client
/// asked to search from. The client no longer applies any reset locally at
/// all (see `CarSnapshot`'s docs) — it just waits for this to replicate
/// back.
fn apply_car_reset(
    reset_msg: On<FromClient<CarResetMsg>>,
    identities: Res<PlayerIdentities>,
    noise: Res<TerrainNoise>,
    origin: Res<WorldOrigin>,
    mut cars: Query<(&CarChassis, &mut Transform, &mut Velocity, &mut ExternalForce)>,
) {
    let Some(client_entity) = reset_msg.client_id.entity() else {
        return;
    };
    let Some(player_id) = identities.get(client_entity) else {
        return;
    };
    for (chassis, mut transform, mut velocity, mut ext_force) in &mut cars {
        if chassis.owner_player_id != player_id {
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
    }
}

/// Authoritative half of R's "right the car in place" — see client's
/// `flip_car_upright` docs for why this is a separate message/handler
/// from `apply_car_reset` rather than reusing it: no search, just
/// recompute ground height at the exact point the client gave (its own
/// current position) and correct orientation there.
///
/// Raycasts against the real physics world (`economy::surface_height_at`)
/// rather than a bare `height_at` terrain-noise lookup — the car might be
/// resting on a Ramp or a building's roof, not raw ground, and a bare
/// `height_at` there ignored that entirely and dropped the car to the
/// terrain height *underneath* whatever it was actually parked on,
/// reported live as ending up under terrain after driving somewhere and
/// pressing R. Falls back to `height_at` itself only if there's no physics
/// context to raycast against at all (shouldn't normally happen — the
/// server always runs one — but `surface_height_at` already has to handle
/// it for `economy.rs`'s own caller, so this just does the same).
fn apply_flip_upright(
    flip_msg: On<FromClient<FlipUprightMsg>>,
    identities: Res<PlayerIdentities>,
    noise: Res<TerrainNoise>,
    origin: Res<WorldOrigin>,
    rapier_context: ReadRapierContext,
    mut cars: Query<(Entity, &CarChassis, &mut Transform, &mut Velocity, &mut ExternalForce)>,
) {
    let Some(client_entity) = flip_msg.client_id.entity() else {
        return;
    };
    let Some(player_id) = identities.get(client_entity) else {
        return;
    };
    for (entity, chassis, mut transform, mut velocity, mut ext_force) in &mut cars {
        if chassis.owner_player_id != player_id {
            continue;
        }
        // Excludes this same car's own collider — the ray is cast
        // straight down through the exact point it's currently sitting
        // (on its roof/side, since that's when you'd press R at all), so
        // without this it hits the car itself first instead of the real
        // ground beneath it. Computed per-car (not once before the loop)
        // for exactly that reason — the exclusion target depends on which
        // car this is.
        let ground_y = match rapier_context.single() {
            Ok(context) => crate::economy::surface_height_at(
                &context,
                &noise,
                &origin,
                flip_msg.true_x,
                flip_msg.true_z,
                Some(entity),
            ),
            Err(_) => height_at(&noise, flip_msg.true_x, flip_msg.true_z),
        };
        transform.translation.y = ground_y + 2.0;
        transform.rotation = Quat::IDENTITY;
        *velocity = Velocity::zero();
        *ext_force = ExternalForce::default();
    }
}

/// Seats a player in an empty passenger seat — unlike `apply_car_input`,
/// there's deliberately no ownership check: riding along in someone else's
/// car is the whole point. Rejects only if the car doesn't exist or its
/// seat is already taken (including by this same sender re-sending —
/// idempotent would double-seat nobody, but there's nothing to overwrite
/// either way).
fn apply_board_passenger(
    board: On<FromClient<BoardPassengerMsg>>,
    identities: Res<PlayerIdentities>,
    mut cars: Query<(&CarChassis, &mut CarSnapshot)>,
) {
    let Some(client_entity) = board.client_id.entity() else {
        return;
    };
    let Some(player_id) = identities.get(client_entity) else {
        return;
    };
    let Some((_, mut snapshot)) = cars.iter_mut().find(|(chassis, _)| chassis.car_id == board.car_id)
    else {
        warn!("car_sim: board-passenger request for unknown car `{}`", board.car_id);
        return;
    };
    if snapshot.passenger_player_id.is_some() {
        warn!("car_sim: rejected board-passenger — car `{}` already has a passenger", board.car_id);
        return;
    }
    snapshot.passenger_player_id = Some(player_id);
}

/// Clears the passenger seat — only the current occupant can vacate it (a
/// stray or duplicate `ExitPassengerMsg` from anyone else is silently
/// ignored, not an error condition: the two-message client flow in
/// `client::pilot`'s `handle_vehicle_key` only ever sends this for the
/// car it just recorded boarding, but a slow network could still deliver
/// it after the seat's already changed hands).
fn apply_exit_passenger(
    exit: On<FromClient<ExitPassengerMsg>>,
    identities: Res<PlayerIdentities>,
    mut cars: Query<(&CarChassis, &mut CarSnapshot)>,
) {
    let Some(client_entity) = exit.client_id.entity() else {
        return;
    };
    let Some(player_id) = identities.get(client_entity) else {
        return;
    };
    let Some((_, mut snapshot)) = cars.iter_mut().find(|(chassis, _)| chassis.car_id == exit.car_id)
    else {
        return;
    };
    if snapshot.passenger_player_id == Some(player_id) {
        snapshot.passenger_player_id = None;
    }
}

/// Writes a player's chosen cosmetics onto their own car(s) — matched via
/// `CarChassis::owner_player_id`, not `OwnedBy` (see `apply_car_input`'s
/// docs on why). No further validation: color/bow choices cost nothing and
/// affect no game state, so there's nothing to cheat by lying about them.
fn apply_set_cosmetics(
    set_msg: On<FromClient<SetCosmeticsMsg>>,
    identities: Res<PlayerIdentities>,
    mut cars: Query<(&CarChassis, &mut CarCosmetics)>,
) {
    let Some(client_entity) = set_msg.client_id.entity() else {
        return;
    };
    let Some(player_id) = identities.get(client_entity) else {
        return;
    };
    for (chassis, mut cosmetics) in &mut cars {
        if chassis.owner_player_id != player_id {
            continue;
        }
        cosmetics.custom_color = set_msg.custom_color;
        cosmetics.has_bow = set_msg.has_bow;
    }
}


/// Below this altitude (or a non-finite position/velocity), a car is
/// considered lost rather than legitimately airborne — real terrain never
/// goes anywhere near this low (worst case is roughly
/// `-(CONTINENTAL_SCALE + CANYON_DEPTH)`, a bit over -1500) — and gets
/// recovered rather than left to fall forever. This is a hard safety net
/// against *any* cause of "no collider under this car" (a missing-collider
/// bug like the terrain preload not following a world regen — or, as
/// reported live, dynamic chunk streaming not keeping up with a car moving
/// faster than it can stream in new chunks — a one-tick gap between
/// queuing new colliders via `Commands` and Rapier actually syncing them,
/// or anything else we haven't thought of) rather than a fix for one
/// specific trigger — a car that's merely fallen off a cliff will keep
/// falling past this line too, but "briefly airborne, then reset" beats
/// what an actual missing-collider bug did last time: an unbounded fall
/// whose ever-more-extreme position also broke client-side reconciliation,
/// which corrects *toward* the server's snapshot no matter how absurd it
/// is, producing the wild altitude oscillation this was built to catch.
///
/// Was `-5_000.0` — comfortably below the real worst case, but *so* far
/// below it that with this car's own damped fall speed (`CAR_LINEAR_DAMPING`
/// caps terminal velocity well under 40 units/sec) reaching it took the
/// better part of a minute. That reads exactly like "falling through the
/// planet forever," not "briefly airborne, then reset" — the net was
/// firing, just far too late to feel like a save. Tightened to still clear
/// the documented worst-case terrain depth with real margin, while
/// recovering several times faster.
const LOST_CAR_ALTITUDE_FLOOR: f32 = -2_000.0;

fn recover_lost_cars(
    noise: Res<TerrainNoise>,
    origin: Res<WorldOrigin>,
    mut cars: Query<(&mut Transform, &mut Velocity, &mut ExternalForce)>,
) {
    for (mut transform, mut velocity, mut ext_force) in &mut cars {
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
    mut loaded_chunks: ResMut<LoadedChunks>,
    mut cars: Query<(&mut Transform, &mut Velocity, &mut ExternalForce), With<CarChassis>>,
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
        &mut loaded_chunks,
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
    loaded_chunks: &mut LoadedChunks,
    cars: &mut Query<(&mut Transform, &mut Velocity, &mut ExternalForce), With<CarChassis>>,
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

    regenerate_terrain_colliders(commands, noise, origin, loaded_chunks);

    anchor.0 = None;
    for (index, (mut transform, mut velocity, mut ext_force)) in cars.iter_mut().enumerate() {
        let spawn_true = pick_spawn_point(index as u32, anchor, noise, origin);
        let ground_y = height_at(noise, spawn_true.x, spawn_true.z);
        let local = (spawn_true - origin.offset).as_vec3();

        transform.translation = Vec3::new(local.x, ground_y + 2.0, local.z);
        transform.rotation = Quat::IDENTITY;
        *velocity = Velocity::zero();
        *ext_force = ExternalForce::default();
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
                        boost: input.boost,
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
    }
}
