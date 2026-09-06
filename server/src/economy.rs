use std::collections::HashMap;
use shared::time::now_unix;

use bevy::prelude::*;
use bevy_rapier3d::prelude::*;
use bevy_replicon::prelude::*;
use uuid::Uuid;

use shared::buildings::{BuildingKind, STARTING_ENERGY, STARTING_ORE};
use shared::deposits::is_near_deposit;
use shared::protocol::{
    BuildingSnapshot, CarSnapshot, IdentifyMsg, PlaceBuildingMsg, RecallToHangarMsg, Wallet,
};
use shared::terrain_gen::{height_at, TerrainNoise};
use shared::worldspace::WorldOrigin;

use crate::car_sim::{OwnedBy, PlayerIdentities};
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
            .add_observer(seed_wallet_on_identify)
            .add_observer(apply_place_building)
            .add_observer(apply_recall_to_hangar)
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
    fn get_or_seed(&mut self, player_id: Uuid) -> (f32, f32) {
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

/// Gives a brand-new player their starting wallet the moment they're
/// first identified — nothing else would ever seed one otherwise (no
/// production without a building, no building without funds to place it).
/// A player already known (either from a previous `IdentifyMsg` this
/// session, or loaded from Postgres at startup — see `apply_loaded_state`)
/// is left untouched.
fn seed_wallet_on_identify(
    identify: On<FromClient<IdentifyMsg>>,
    mut wallets: ResMut<Wallets>,
    persistence: Res<Persistence>,
) {
    if wallets.0.contains_key(&identify.player_id) {
        return;
    }
    let (energy, ore) = wallets.get_or_seed(identify.player_id);
    persistence.send(PersistenceCommand::SaveWallet(WalletRow {
        player_id: identify.player_id,
        energy: energy as f64,
        ore: ore as f64,
    }));
}

/// Applies whatever the persistence thread has loaded (in practice: once,
/// shortly after server startup) into the live in-memory `Wallets` map and
/// spawns a replicated `BuildingSnapshot` per loaded building. The sole
/// consumer of `Persistence::try_recv` — see that method's own docs on why
/// `persistence.rs` itself has no opinion on what the loaded rows mean.
fn apply_loaded_state(
    mut commands: Commands,
    persistence: Res<Persistence>,
    mut wallets: ResMut<Wallets>,
    origin: Res<WorldOrigin>,
) {
    while let Some(event) = persistence.try_recv() {
        match event {
            PersistenceEvent::Loaded { wallets: loaded_wallets, buildings } => {
                for wallet in loaded_wallets {
                    wallets.0.insert(wallet.player_id, (wallet.energy as f32, wallet.ore as f32));
                }
                for building in buildings {
                    spawn_building_from_row(&mut commands, &origin, &building);
                }
                info!(
                    "economy: applied {} loaded wallet(s)",
                    wallets.0.len()
                );
            }
            PersistenceEvent::Unavailable => {
                warn!("economy: running without a database — wallets/buildings will not persist");
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
fn spawn_building(
    commands: &mut Commands,
    origin: &WorldOrigin,
    kind: BuildingKind,
    owner_player_id: Uuid,
    true_x: f64,
    true_z: f64,
    build_complete_at: f64,
    rotation_y: f32,
    ground_y: f32,
) -> Entity {
    let mut entity = commands.spawn((
        BuildingSnapshot { kind, owner_player_id, true_x, true_z, build_complete_at, rotation_y, ground_y },
        Replicated,
    ));

    if kind.is_drivable_structure() {
        let local = (bevy::math::DVec3::new(true_x, 0.0, true_z) - origin.offset).as_vec3();
        let (translation, rotation) =
            shared::buildings::ramp_transform(local.x, local.z, ground_y, rotation_y);
        entity.insert((
            Transform::from_translation(translation).with_rotation(rotation),
            RigidBody::Fixed,
            Collider::cuboid(
                shared::buildings::RAMP_HALF_WIDTH,
                shared::buildings::RAMP_HALF_THICKNESS,
                shared::buildings::RAMP_HALF_LENGTH,
            ),
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
fn surface_height_at(
    context: RapierContext<'_>,
    noise: &TerrainNoise,
    origin: &WorldOrigin,
    true_x: f64,
    true_z: f64,
) -> f32 {
    let local = (bevy::math::DVec3::new(true_x, 0.0, true_z) - origin.offset).as_vec3();
    const RAY_START_HEIGHT: f32 = 10_000.0;
    let ray_origin = Vec3::new(local.x, RAY_START_HEIGHT, local.z);
    match context.cast_ray(ray_origin, Vec3::NEG_Y, RAY_START_HEIGHT * 2.0, true, QueryFilter::default())
    {
        Some((_, toi)) => RAY_START_HEIGHT - toi,
        None => height_at(noise, true_x, true_z),
    }
}

fn spawn_building_from_row(commands: &mut Commands, origin: &WorldOrigin, row: &BuildingRow) {
    let Some(kind) = BuildingKind::from_db_str(&row.kind) else {
        warn!("economy: ignoring building {} with unknown kind `{}`", row.id, row.kind);
        return;
    };
    spawn_building(
        commands,
        origin,
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
    origin: Res<WorldOrigin>,
    noise: Res<TerrainNoise>,
    rapier_context: ReadRapierContext,
    mut wallets: ResMut<Wallets>,
    persistence: Res<Persistence>,
    mut commands: Commands,
    cars: Query<(&OwnedBy, &Transform)>,
) {
    let Some(client_entity) = place.client_id.entity() else {
        return;
    };
    let Some(player_id) = identities.get(client_entity) else {
        warn!("economy: build request from an unidentified client — ignoring");
        return;
    };

    // The sender's own car's current *true* position (not the local
    // Transform directly — those are only equal before the world's first
    // rebase, see WorldOrigin's docs), to bound how far "where I am" is
    // allowed to claim to be.
    let Some((_, car_transform)) = cars.iter().find(|(owner, _)| owner.0 == client_entity) else {
        return;
    };
    let car_true = origin.to_true(car_transform.translation);
    let dx = place.true_x - car_true.x;
    let dz = place.true_z - car_true.z;
    if (dx * dx + dz * dz).sqrt() > MAX_PLACEMENT_DISTANCE {
        warn!("economy: rejected placement — too far from the sender's own car");
        return;
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
    let ground_y = match rapier_context.single() {
        Ok(context) => surface_height_at(context, &noise, &origin, place.true_x, place.true_z),
        Err(_) => height_at(&noise, place.true_x, place.true_z),
    };
    spawn_building(
        &mut commands,
        &origin,
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

/// Teleports the sender's own car to their own completed Hangar — the
/// simplified v1 Hangar behavior (see the base-building plan's scope
/// note). Same reset mechanics `car_sim.rs`'s `apply_car_reset` uses
/// (position/velocity/force reset, `reset_generation` bump), just anchored
/// at a chosen point instead of "wherever the client's local reset already
/// searched from."
fn apply_recall_to_hangar(
    _recall: On<FromClient<RecallToHangarMsg>>,
    identities: Res<PlayerIdentities>,
    noise: Res<TerrainNoise>,
    origin: Res<WorldOrigin>,
    buildings: Query<&BuildingSnapshot>,
    mut cars: Query<(&OwnedBy, &mut Transform, &mut Velocity, &mut ExternalForce, &mut CarSnapshot)>,
) {
    let Some(client_entity) = _recall.client_id.entity() else {
        return;
    };
    let Some(player_id) = identities.get(client_entity) else {
        return;
    };

    let Some(hangar) = buildings.iter().find(|b| {
        b.kind == BuildingKind::Hangar && b.owner_player_id == player_id && b.build_complete_at <= now_unix()
    }) else {
        return;
    };

    let ground_y = height_at(&noise, hangar.true_x, hangar.true_z);
    let local = (bevy::math::DVec3::new(hangar.true_x, 0.0, hangar.true_z) - origin.offset).as_vec3();

    for (owner, mut transform, mut velocity, mut ext_force, mut snapshot) in &mut cars {
        if owner.0 != client_entity {
            continue;
        }
        transform.translation = Vec3::new(local.x, ground_y + 2.0, local.z);
        transform.rotation = Quat::IDENTITY;
        *velocity = Velocity::zero();
        *ext_force = ExternalForce::default();
        snapshot.reset_generation = snapshot.reset_generation.wrapping_add(1);
        break;
    }
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

    let mut changed: Vec<Uuid> = Vec::new();
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
        changed.push(building.owner_player_id);
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
