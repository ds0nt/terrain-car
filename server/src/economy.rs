use std::collections::HashMap;
use shared::time::now_unix;

use bevy::prelude::*;
use bevy_rapier3d::prelude::*;
use bevy_replicon::prelude::*;
use uuid::Uuid;

use shared::buildings::{BuildingKind, STARTING_ENERGY, STARTING_ORE};
use shared::deposits::is_near_deposit;
use shared::protocol::{BuildingSnapshot, DestroyBuildingMsg, PlaceBuildingMsg, Wallet};
use shared::terrain_gen::{height_at, TerrainNoise};
use shared::worldspace::WorldOrigin;

use crate::car_sim::{OwnedBy, PlayerIdentities, PlayerPositions};
use crate::persistence::{BuildingRow, Persistence, PersistenceCommand, PersistenceEvent, WalletRow};

/// How close (true-space) a placement request must be to the sender's own
/// current position — bounds "wherever you're pointing the camera," not an
/// arbitrary point the client could otherwise claim, while staying
/// generous enough for the mouse-raycast placement UI (client's
/// `building_placement.rs`) to actually reach a spot worth aiming at,
/// nowhere near enough to place across the map.
const MAX_PLACEMENT_DISTANCE: f64 = 60.0;
/// How often buildings actually produce resources — economy-scale time,
/// not the 64Hz physics tick.
const PRODUCTION_TICK_SECS: f32 = 1.0;

pub struct EconomyPlugin;

impl Plugin for EconomyPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Wallets>()
            .insert_resource(ProductionTimer(Timer::from_seconds(
                PRODUCTION_TICK_SECS,
                TimerMode::Repeating,
            )))
            .add_observer(apply_place_building)
            .add_observer(apply_destroy_building)
            .add_systems(Update, (apply_loaded_state, tick_production, sync_wallet_components));
    }
}

/// Server-authoritative in-memory wallets, keyed by the durable
/// `PersistentPlayerId` (not `Entity` — a wallet must outlive any single
/// connection). Postgres (`persistence.rs`) is a write-behind mirror of
/// this, never the other way around: gameplay always reads/writes this
/// map directly, same "fast in-memory state, slow persistence layer only
/// on the side" principle as the rest of the base-building plan.
#[derive(Resource, Default)]
pub struct Wallets(HashMap<Uuid, (f32, f32)>);

impl Wallets {
    /// `pub(crate)`: `server::auth` calls this once, right after a
    /// successful login/register, to learn the wallet a just-spawned car
    /// should start with — see that module's docs on why this replaced
    /// the old `seed_wallet_on_identify`.
    pub(crate) fn get_or_seed(&mut self, player_id: Uuid) -> (f32, f32) {
        *self.0.entry(player_id).or_insert((STARTING_ENERGY, STARTING_ORE))
    }

    /// Current balance for a player, if they've been seen before —
    /// `pub` so callers outside this module (currently just `weapons.rs`,
    /// reading a balance back to persist it after `steal_ore`) don't need
    /// direct access to the private map.
    pub fn get(&self, player_id: Uuid) -> Option<(f32, f32)> {
        self.0.get(&player_id).copied()
    }

    /// Adds (or subtracts, for a negative delta) to a player's balance
    /// directly — used by passive income sources (villagers' gathering,
    /// `villagers.rs`) that aren't a transfer between two players the way
    /// `steal_ore` is.
    pub fn credit(&mut self, player_id: Uuid, energy_delta: f32, ore_delta: f32) {
        let (energy, ore) = self.get_or_seed(player_id);
        self.0.insert(player_id, (energy + energy_delta, ore + ore_delta));
    }

    /// Moves up to `amount` ore from `from`'s wallet to `to`'s, capped by
    /// how much `from` actually has (never goes negative). Used by the
    /// gun's on-hit ore steal (`weapons.rs`) — a deliberate PvP tie-in
    /// between combat and the economy, not accidental scope creep: shoot
    /// another player, take a cut of their ore. Returns the amount
    /// actually transferred (0 if `from` had none).
    pub fn steal_ore(&mut self, from: Uuid, to: Uuid, amount: f32) -> f32 {
        // get_or_seed returns a copy, not a live reference — mutate via
        // insert(), same read-compute-write-back shape every other wallet
        // mutation in this file already uses (see apply_place_building).
        let (from_energy, from_ore) = self.get_or_seed(from);
        let transferred = from_ore.min(amount).max(0.0);
        self.0.insert(from, (from_energy, from_ore - transferred));

        let (to_energy, to_ore) = self.get_or_seed(to);
        self.0.insert(to, (to_energy, to_ore + transferred));

        transferred
    }
}

#[derive(Resource)]
struct ProductionTimer(Timer);

/// Applies whatever the persistence thread has loaded (in practice: once,
/// shortly after server startup) into the live in-memory `Wallets` map and
/// spawns a replicated `BuildingSnapshot` per loaded building. The sole
/// consumer of `Persistence::try_recv` — see that method's own docs on why
/// `persistence.rs` itself has no opinion on what the loaded rows mean.
fn apply_loaded_state(
    mut commands: Commands,
    persistence: Res<Persistence>,
    mut wallets: ResMut<Wallets>,
    mut positions: ResMut<PlayerPositions>,
    origin: Res<WorldOrigin>,
    mut auth_events: MessageWriter<crate::auth::AuthOutcomeReceived>,
) {
    while let Some(event) = persistence.try_recv() {
        match event {
            PersistenceEvent::Loaded { wallets: loaded_wallets, buildings, positions: loaded_positions } => {
                for wallet in loaded_wallets {
                    wallets.0.insert(wallet.player_id, (wallet.energy as f32, wallet.ore as f32));
                }
                for building in buildings {
                    spawn_building_from_row(&mut commands, &origin, &building);
                }
                let loaded_position_count = loaded_positions.len();
                for position in loaded_positions {
                    positions.set(position.player_id, (position.true_x, position.true_z));
                }
                info!(
                    "economy: applied {} loaded wallet(s), {} loaded position(s)",
                    wallets.0.len(),
                    loaded_position_count
                );
            }
            PersistenceEvent::Unavailable => {
                warn!("economy: running without a database — wallets/buildings will not persist");
            }
            // Not this module's concern — forwarded as a plain Bevy
            // message so `server::auth` can react without also polling
            // `persistence.try_recv()` itself (see that module's docs on
            // why only one system may ever drain this channel).
            PersistenceEvent::AuthResult { client_entity, outcome } => {
                auth_events.write(crate::auth::AuthOutcomeReceived { client_entity, outcome });
            }
        }
    }
}

/// Spawns a `BuildingSnapshot` and, for a drivable structure (`Ramp`), the
/// matching physics collider too — server-side collision must exist for
/// any kind a car can actually drive on, or the server's own authoritative
/// physics would let a car fall straight through it while the client's
/// local prediction (which does have the collider — see client's
/// `building_render.rs`) disagrees, fighting reconciliation constantly.
/// Uses `shared::buildings::ramp_transform` — the exact same pure function
/// the client calls for its visual mesh — so the two can never disagree
/// about where the surface actually is. `ground_y` is the caller's
/// responsibility (usually `surface_height_at`, a downward raycast against
/// the server's own authoritative Rapier world — which is what lets a
/// Ramp start on top of another existing Ramp instead of always sitting on
/// raw terrain).
#[allow(clippy::too_many_arguments)]
fn spawn_building(
    commands: &mut Commands,
    origin: &WorldOrigin,
    id: Uuid,
    kind: BuildingKind,
    owner_player_id: Uuid,
    true_x: f64,
    true_z: f64,
    build_complete_at: f64,
    rotation_y: f32,
    ground_y: f32,
) -> Entity {
    let mut entity = commands.spawn((
        BuildingSnapshot { id, kind, owner_player_id, true_x, true_z, build_complete_at, rotation_y, ground_y },
        Replicated,
    ));

    let local = (bevy::math::DVec3::new(true_x, 0.0, true_z) - origin.offset).as_vec3();
    if kind.uses_slab_geometry() {
        let dims = shared::buildings::slab_dims(kind);
        let (translation, rotation) = shared::buildings::slab_transform(dims, local.x, local.z, ground_y, rotation_y);
        entity.insert((
            Transform::from_translation(translation).with_rotation(rotation),
            RigidBody::Fixed,
            Collider::cuboid(dims.half_width, dims.half_height, dims.half_length),
            Friction::coefficient(1.0),
        ));
    } else {
        // Every other kind is a solid, upright obstacle — same shape and
        // Y-offset the client uses for its own copy (see
        // `client::building_render::building_mesh_and_transform`), so a
        // car is blocked identically on both sides.
        let shape = shared::buildings::collider_shape(kind);
        let collider = match shape {
            shared::buildings::ColliderShape::Cuboid { half_x, half_y, half_z } => {
                Collider::cuboid(half_x, half_y, half_z)
            }
            shared::buildings::ColliderShape::Cylinder { half_height, radius } => {
                Collider::cylinder(half_height, radius)
            }
        };
        entity.insert((
            Transform::from_xyz(local.x, ground_y + shape.half_height(), local.z)
                .with_rotation(Quat::from_rotation_y(rotation_y)),
            RigidBody::Fixed,
            collider,
            Friction::coefficient(1.0),
        ));
    }

    entity.id()
}

/// Straight-down raycast against the server's own Rapier world to find
/// whatever surface is actually at `(true_x, true_z)` — terrain, or an
/// existing building's collider (a Ramp, say) sitting on top of it. Falls
/// back to raw terrain height if nothing at all is hit (shouldn't happen
/// in practice — terrain always has a collider — but a placement request
/// is exactly the kind of place to not `unwrap` that assumption away).
/// Raycasts straight down from well above `(true_x, true_z)` against the
/// server's own real physics colliders — terrain, buildings, ramps alike —
/// falling back to raw terrain height (`height_at`) only if nothing was
/// hit. `pub(crate)` so `car_sim::apply_flip_upright`/`apply_recall_to_hangar`
/// and `aircraft::apply_recall_plane` can reuse it too: a bare `height_at`
/// call there had exactly this module's own reason to avoid one (see this
/// function's original docs) — pressing R while parked on a Ramp or a
/// building's roof dropped the car straight to the *raw terrain* height
/// underneath, embedding it inside/under whatever it had actually been
/// resting on, reported live as ending up under terrain after driving
/// somewhere and pressing R.
///
/// `exclude` should always be the vehicle actually being repositioned, if
/// there is one — without it, the ray (cast straight down through that
/// same vehicle's own current position) hits that vehicle's *own*
/// collider first, especially likely for a flip-upright call (the vehicle
/// is on its roof/side at exactly that point) or a recall bringing it back
/// to a spot it's already sitting on. That read as the same "ends up
/// underground" symptom as the missing-raycast bug above for a different
/// reason: `ground_y` came back as the vehicle's *own* current surface
/// height instead of the real ground beneath it. `None` is correct only
/// when nothing already occupies that exact point (a fresh building
/// placement, before it exists at all — see `settle_ground_y`).
pub(crate) fn surface_height_at(
    context: &RapierContext<'_>,
    noise: &TerrainNoise,
    origin: &WorldOrigin,
    true_x: f64,
    true_z: f64,
    exclude: Option<Entity>,
) -> f32 {
    let local = (bevy::math::DVec3::new(true_x, 0.0, true_z) - origin.offset).as_vec3();
    const RAY_START_HEIGHT: f32 = 10_000.0;
    let ray_origin = Vec3::new(local.x, RAY_START_HEIGHT, local.z);
    let mut filter = QueryFilter::default();
    if let Some(exclude) = exclude {
        filter = filter.exclude_rigid_body(exclude);
    }
    match context.cast_ray(ray_origin, Vec3::NEG_Y, RAY_START_HEIGHT * 2.0, true, filter) {
        Some((_, toi)) => RAY_START_HEIGHT - toi,
        None => height_at(noise, true_x, true_z),
    }
}

/// The `ground_y` a newly-placed building should actually use — for
/// `Ramp` this is still just the single point under its anchor (its own
/// `ramp_transform` already handles sitting a tilted box on that), but
/// every other kind samples every corner/rim point of its real footprint
/// (`shared::buildings::footprint_sample_offsets`) and settles to the
/// *lowest* of them, minus `FOOTPRINT_BURY_MARGIN`. Center-point-only
/// placement left a building's base floating or half-exposed on anything
/// but flat ground; sinking to the lowest sampled point instead means the
/// whole base ends up buried at or below the surface everywhere under it.
fn settle_ground_y(
    context: Option<RapierContext<'_>>,
    noise: &TerrainNoise,
    origin: &WorldOrigin,
    kind: BuildingKind,
    true_x: f64,
    true_z: f64,
    rotation_y: f32,
) -> f32 {
    let sample = |x: f64, z: f64| match &context {
        // `None` exclude — nothing occupies this exact spot yet, the
        // building being placed doesn't exist until after this returns.
        Some(context) => surface_height_at(context, noise, origin, x, z, None),
        None => height_at(noise, x, z),
    };

    if kind.uses_slab_geometry() {
        return sample(true_x, true_z);
    }

    let (sin, cos) = rotation_y.sin_cos();
    shared::buildings::footprint_sample_offsets(shared::buildings::collider_shape(kind))
        .into_iter()
        .map(|(ox, oz)| {
            let world_x = true_x + (ox * cos - oz * sin) as f64;
            let world_z = true_z + (ox * sin + oz * cos) as f64;
            sample(world_x, world_z)
        })
        .fold(f32::INFINITY, f32::min)
        - shared::buildings::FOOTPRINT_BURY_MARGIN
}

fn spawn_building_from_row(commands: &mut Commands, origin: &WorldOrigin, row: &BuildingRow) {
    let Some(kind) = BuildingKind::from_db_str(&row.kind) else {
        warn!("economy: ignoring building {} with unknown kind `{}`", row.id, row.kind);
        return;
    };
    spawn_building(
        commands,
        origin,
        row.id,
        kind,
        row.owner_player_id,
        row.true_x,
        row.true_z,
        row.build_complete_at.unwrap_or(0.0),
        row.rotation_y as f32,
        row.ground_y as f32,
    );
}

/// Resolves one `PlaceBuildingMsg`: validates the sender is identified,
/// affordable, close enough to their claimed position, and (for
/// `ExtractionFacility`) actually on a deposit — never trusts the client
/// on any of these, same trust boundary as every other client -> server
/// message in this game. On success, deducts the cost immediately and
/// spawns a replicated `BuildingSnapshot` with `build_complete_at` in the
/// future.
#[allow(clippy::too_many_arguments)]
fn apply_place_building(
    place: On<FromClient<PlaceBuildingMsg>>,
    identities: Res<PlayerIdentities>,
    positions: Res<PlayerPositions>,
    origin: Res<WorldOrigin>,
    noise: Res<TerrainNoise>,
    rapier_context: ReadRapierContext,
    mut wallets: ResMut<Wallets>,
    persistence: Res<Persistence>,
    mut commands: Commands,
) {
    let Some(client_entity) = place.client_id.entity() else {
        return;
    };
    let Some(player_id) = identities.get(client_entity) else {
        warn!("economy: build request from an unidentified client — ignoring");
        return;
    };

    // Bound how far "where I am" is allowed to claim to be, against
    // wherever the player *actually* is (`PlayerPositions`, updated every
    // tick from `PlayerPositionMsg` — car, plane, or on foot alike, see
    // that message's own docs). Used to check against a parked car's or
    // plane's position instead, which broke the instant you stepped away
    // from either — reported live as "I can't build when I exit my
    // plane," since the distance was being measured from the vehicle you
    // just left, not from you.
    //
    // No position yet at all only happens in the brief window right after
    // login before the client's first `PlayerPositionMsg` has arrived —
    // skipping the check entirely in that one case (rather than rejecting
    // every such placement outright) is what lets a brand-new player place
    // their very first building using `STARTING_ENERGY`/`STARTING_ORE`
    // moments after connecting, rather than racing a message that hasn't
    // landed yet.
    if let Some((true_x, true_z)) = positions.get(player_id) {
        let dx = place.true_x - true_x;
        let dz = place.true_z - true_z;
        if (dx * dx + dz * dz).sqrt() > MAX_PLACEMENT_DISTANCE {
            warn!("economy: rejected placement — too far from the sender's own position");
            return;
        }
    }

    if place.kind.requires_deposit() && !is_near_deposit(place.true_x, place.true_z) {
        warn!("economy: rejected {:?} placement — not on a deposit", place.kind);
        return;
    }

    let (cost_energy, cost_ore) = place.kind.cost();
    let (energy, ore) = wallets.get_or_seed(player_id);
    if energy < cost_energy || ore < cost_ore {
        warn!("economy: rejected {:?} placement — insufficient funds", place.kind);
        return;
    }

    let new_wallet = (energy - cost_energy, ore - cost_ore);
    wallets.0.insert(player_id, new_wallet);
    persistence.send(PersistenceCommand::SaveWallet(WalletRow {
        player_id,
        energy: new_wallet.0 as f64,
        ore: new_wallet.1 as f64,
    }));

    let building_id = Uuid::new_v4();
    let build_complete_at = now_unix() + place.kind.build_time_secs() as f64;
    let ground_y = settle_ground_y(
        rapier_context.single().ok(),
        &noise,
        &origin,
        place.kind,
        place.true_x,
        place.true_z,
        place.rotation_y,
    );
    spawn_building(
        &mut commands,
        &origin,
        building_id,
        place.kind,
        player_id,
        place.true_x,
        place.true_z,
        build_complete_at,
        place.rotation_y,
        ground_y,
    );
    persistence.send(PersistenceCommand::SaveBuilding(BuildingRow {
        id: building_id,
        owner_player_id: player_id,
        kind: place.kind.as_db_str().to_string(),
        true_x: place.true_x,
        true_z: place.true_z,
        build_complete_at: Some(build_complete_at),
        rotation_y: place.rotation_y as f64,
        ground_y: ground_y as f64,
    }));
}

/// Resolves one `DestroyBuildingMsg`: finds the building with that id,
/// checks the sender actually owns it, despawns it, and deletes its
/// persisted row so it doesn't come back on the next server restart. No
/// resource refund — same "no partial refunds if it's ever removed"
/// stance `BuildingKind::cost`'s own docs already anticipated.
fn apply_destroy_building(
    destroy: On<FromClient<DestroyBuildingMsg>>,
    identities: Res<PlayerIdentities>,
    persistence: Res<Persistence>,
    mut commands: Commands,
    buildings: Query<(Entity, &BuildingSnapshot)>,
) {
    let Some(client_entity) = destroy.client_id.entity() else {
        return;
    };
    let Some(player_id) = identities.get(client_entity) else {
        warn!("economy: destroy request from an unidentified client — ignoring");
        return;
    };

    let Some((entity, snapshot)) =
        buildings.iter().find(|(_, snapshot)| snapshot.id == destroy.building_id)
    else {
        warn!("economy: destroy request for unknown building `{}`", destroy.building_id);
        return;
    };
    if snapshot.owner_player_id != player_id {
        warn!("economy: rejected destroy — `{player_id}` doesn't own building `{}`", destroy.building_id);
        return;
    }

    commands.entity(entity).despawn();
    persistence.send(PersistenceCommand::DeleteBuilding(destroy.building_id));
}

/// Credits every completed building's owner at `PRODUCTION_TICK_SECS`
/// intervals — deliberately not every physics tick, since economy pacing
/// has no reason to run at 64Hz. Saves a wallet through `persistence.rs`
/// only when it actually changed this tick (i.e. at least one completed
/// building produced something), not unconditionally every second.
fn tick_production(
    time: Res<Time>,
    mut timer: ResMut<ProductionTimer>,
    mut wallets: ResMut<Wallets>,
    persistence: Res<Persistence>,
    buildings: Query<&BuildingSnapshot>,
) {
    if !timer.0.tick(time.delta()).just_finished() {
        return;
    }
    let now = now_unix();
    let dt = PRODUCTION_TICK_SECS;

    // A `HashSet`, not a `Vec`: a player with several producing buildings
    // (say, three EnergyGenerators) would otherwise land in here once per
    // building, and the loop below would fire that many identical
    // `SaveWallet` commands for the exact same wallet every single tick —
    // each spawned as its own concurrent task competing for
    // `persistence.rs`'s fixed-size connection pool (see its own docs on
    // why saves run concurrently, not sequentially). That's exactly what
    // was starving the pool and pushing `sqlx::pool::acquire` past its
    // slow-threshold warning during real play: not "saving 30x/sec," but
    // saving the same row several times over for no reason every second.
    let mut changed: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
    for building in &buildings {
        if building.build_complete_at > now {
            continue; // still under construction
        }
        let (energy_rate, ore_rate) = building.kind.production_rate();
        if energy_rate == 0.0 && ore_rate == 0.0 {
            continue;
        }
        let entry = wallets.0.entry(building.owner_player_id).or_insert((STARTING_ENERGY, STARTING_ORE));
        entry.0 += energy_rate * dt;
        entry.1 += ore_rate * dt;
        changed.insert(building.owner_player_id);
    }

    for player_id in changed {
        if let Some(&(energy, ore)) = wallets.0.get(&player_id) {
            persistence.send(PersistenceCommand::SaveWallet(WalletRow {
                player_id,
                energy: energy as f64,
                ore: ore as f64,
            }));
        }
    }
}

/// Mirrors the authoritative in-memory `Wallets` map onto each connected
/// player's own car as a replicated `Wallet` component — a simple,
/// self-healing "keep these in sync" pass every frame (cheap: a handful of
/// connected cars, one hashmap lookup each) rather than pushing updates
/// from every place a wallet can change (seed/placement/production), which
/// would be three separate paths to keep consistent instead of one.
fn sync_wallet_components(
    wallets: Res<Wallets>,
    identities: Res<PlayerIdentities>,
    mut cars: Query<(&OwnedBy, &mut Wallet)>,
) {
    for (owner, mut wallet) in &mut cars {
        let Some(player_id) = identities.get(owner.0) else {
            continue;
        };
        let Some(&(energy, ore)) = wallets.0.get(&player_id) else {
            continue;
        };
        if wallet.energy != energy || wallet.ore != ore {
            wallet.energy = energy;
            wallet.ore = ore;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steal_ore_moves_the_requested_amount() {
        let mut wallets = Wallets::default();
        let from = Uuid::new_v4();
        let to = Uuid::new_v4();
        wallets.get_or_seed(from); // seed both at STARTING_ORE
        wallets.get_or_seed(to);

        let transferred = wallets.steal_ore(from, to, 5.0);

        assert_eq!(transferred, 5.0);
        assert_eq!(wallets.get(from).unwrap().1, STARTING_ORE - 5.0);
        assert_eq!(wallets.get(to).unwrap().1, STARTING_ORE + 5.0);
    }

    #[test]
    fn steal_ore_caps_at_the_victims_actual_balance() {
        let mut wallets = Wallets::default();
        let from = Uuid::new_v4();
        let to = Uuid::new_v4();
        wallets.get_or_seed(from);

        // Ask for far more than STARTING_ORE has.
        let transferred = wallets.steal_ore(from, to, STARTING_ORE + 1000.0);

        assert_eq!(transferred, STARTING_ORE);
        assert_eq!(wallets.get(from).unwrap().1, 0.0);
        assert_eq!(wallets.get(to).unwrap().1, STARTING_ORE + STARTING_ORE);
    }

    #[test]
    fn steal_ore_never_makes_the_victim_go_negative() {
        let mut wallets = Wallets::default();
        let from = Uuid::new_v4();
        let to = Uuid::new_v4();
        wallets.get_or_seed(from);

        wallets.steal_ore(from, to, STARTING_ORE + 1000.0);
        // A second attempt against an already-empty wallet should
        // transfer nothing, not go negative.
        let second = wallets.steal_ore(from, to, 10.0);

        assert_eq!(second, 0.0);
        assert_eq!(wallets.get(from).unwrap().1, 0.0);
    }
}
