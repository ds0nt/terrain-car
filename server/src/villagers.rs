use std::collections::HashMap;

use bevy::math::DVec3;
use bevy::prelude::*;
use bevy_replicon::prelude::*;
use rand::Rng;
use uuid::Uuid;

use shared::buildings::{BuildingKind, MAX_VILLAGERS_PER_PLAYER};
use shared::deposits::is_near_deposit;
use shared::protocol::{BuildingSnapshot, QueueVillagerMsg, VillagerQueue, VillagerSnapshot};
use shared::time::now_unix;
use shared::worldspace::WorldOrigin;

use crate::car_sim::{OwnedBy, PlayerIdentities};
use crate::economy::Wallets;
use crate::persistence::{Persistence, PersistenceCommand, WalletRow};

/// Every player's own free resource-gathering helper — see
/// `VillagerSnapshot`'s docs for why this exists independent of any
/// building. Wanders a small radius around wherever its owner's car
/// currently is (no real pathfinding — just re-picks a random point
/// within `WANDER_RADIUS` of the owner every `RETARGET_SECS` and walks
/// straight toward it), gathering ore when that happens to land near a
/// deposit and a slow energy trickle otherwise.
pub struct VillagersPlugin;

impl Plugin for VillagersPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(GatherTimer(Timer::from_seconds(
            GATHER_TICK_SECS,
            TimerMode::Repeating,
        )))
        .insert_resource(LandFactoryTimer(Timer::from_seconds(
            LAND_FACTORY_INTERVAL_SECS,
            TimerMode::Repeating,
        )))
        .init_resource::<VillagerQueues>()
        .add_observer(apply_queue_villager)
        .add_systems(
            Update,
            (wander_villagers, gather_resources, spawn_from_land_factories, sync_villager_queue_components),
        );
    }
}

/// How many villagers each player has asked a Land Factory to build but
/// hasn't gotten yet — see `VillagerQueue` (the replicated mirror of this
/// a player's own client actually reads) and `apply_queue_villager` (how
/// entries get added). One shared queue per player, not per building: any
/// of their completed Land Factories can pull from it (see
/// `spawn_from_land_factories`), so building more than one speeds up
/// throughput rather than requiring the player to split attention between
/// separate queues for a single unit type.
#[derive(Resource, Default)]
struct VillagerQueues(HashMap<Uuid, u32>);

const WANDER_RADIUS: f64 = 30.0;
const RETARGET_SECS: f32 = 4.0;
const VILLAGER_SPEED: f64 = 4.0;
const GATHER_TICK_SECS: f32 = 1.0;
/// Deliberately well below a built Extraction Facility's own rate (1.0
/// ore/sec) or Energy Generator's (1.5 energy/sec) — this is a safety net
/// against being stuck at zero, not a replacement for actually building.
const ORE_GATHER_RATE: f32 = 0.3;
const ENERGY_GATHER_RATE: f32 = 0.3;
/// How often each completed Land Factory produces one more villager for
/// its owner.
const LAND_FACTORY_INTERVAL_SECS: f32 = 60.0;

#[derive(Component)]
pub(crate) struct VillagerAi {
    owner_player_id: Uuid,
    target_true_x: f64,
    target_true_z: f64,
    retarget_timer: f32,
}

#[derive(Resource)]
struct GatherTimer(Timer);

#[derive(Resource)]
struct LandFactoryTimer(Timer);

fn spawn_villager(commands: &mut Commands, owner_player_id: Uuid, true_x: f64, true_z: f64) {
    commands.spawn((
        VillagerAi {
            owner_player_id,
            target_true_x: true_x,
            target_true_z: true_z,
            retarget_timer: 0.0,
        },
        VillagerSnapshot { owner_player_id, true_x, true_z },
        Replicated,
    ));
}

/// Spawns exactly one villager per player, the first time they ever log
/// in — called directly from `server::auth`'s login/register success
/// handler (which already knows exactly where their car just spawned, so
/// no separate lookup is needed), rather than reacting to a network
/// message of its own. Idempotent by checking for an existing
/// `VillagerAi` with this `player_id`: a returning player logging in
/// again on a later session must not get a second one.
pub(crate) fn spawn_villager_for_new_player(
    commands: &mut Commands,
    existing: &Query<&VillagerAi>,
    owner_player_id: Uuid,
    true_x: f64,
    true_z: f64,
) {
    if existing.iter().any(|v| v.owner_player_id == owner_player_id) {
        return;
    }
    spawn_villager(commands, owner_player_id, true_x, true_z);
}

/// Every `LAND_FACTORY_INTERVAL_SECS`, each completed Land Factory pulls
/// one villager off its owner's queue (`VillagerQueues`, filled by
/// `apply_queue_villager` — see that function's docs on why the queue is
/// shared per-player rather than per-building) and spawns it — see
/// `BuildingKind::LandFactory`'s v1 scope note (villagers are the only
/// part of "a factory for villagers, tanks, and AA tanks" actually
/// implemented so far; tanks/AA tanks are a real unit/combat-AI system
/// deserving their own pass). Used to spawn one automatically every
/// interval regardless of player input; now it only ever spawns something
/// the player actually queued, still capped so the queue itself can't be
/// used to blow past `MAX_VILLAGERS_PER_PLAYER`.
fn spawn_from_land_factories(
    time: Res<Time>,
    mut timer: ResMut<LandFactoryTimer>,
    mut queues: ResMut<VillagerQueues>,
    mut commands: Commands,
    buildings: Query<&BuildingSnapshot>,
    villagers: Query<&VillagerAi>,
) {
    if !timer.0.tick(time.delta()).just_finished() {
        return;
    }
    let now = now_unix();
    for building in &buildings {
        if building.kind != BuildingKind::LandFactory || building.build_complete_at > now {
            continue;
        }
        let queued = queues.0.get(&building.owner_player_id).copied().unwrap_or(0);
        if queued == 0 {
            continue;
        }
        let count =
            villagers.iter().filter(|v| v.owner_player_id == building.owner_player_id).count() as u32;
        if count >= MAX_VILLAGERS_PER_PLAYER {
            continue;
        }
        queues.0.insert(building.owner_player_id, queued - 1);
        spawn_villager(&mut commands, building.owner_player_id, building.true_x, building.true_z);
    }
}

/// Adds one villager build to the sender's own queue — the only
/// validation is "do you actually have a completed Land Factory to ever
/// fulfill this" and "is your queue+live-villager total already at the
/// cap," same trust boundary every other client -> server message here
/// enforces (never trust the client on whether an action should succeed).
fn apply_queue_villager(
    _queue: On<FromClient<QueueVillagerMsg>>,
    identities: Res<PlayerIdentities>,
    mut queues: ResMut<VillagerQueues>,
    buildings: Query<&BuildingSnapshot>,
    villagers: Query<&VillagerAi>,
) {
    let Some(client_entity) = _queue.client_id.entity() else {
        return;
    };
    let Some(player_id) = identities.get(client_entity) else {
        warn!("villagers: queue request from an unidentified client — ignoring");
        return;
    };

    let now = now_unix();
    let has_factory = buildings
        .iter()
        .any(|b| b.kind == BuildingKind::LandFactory && b.owner_player_id == player_id && b.build_complete_at <= now);
    if !has_factory {
        warn!("villagers: rejected queue request from `{player_id}` — no completed Land Factory");
        return;
    }

    let alive = villagers.iter().filter(|v| v.owner_player_id == player_id).count() as u32;
    let queued = queues.0.get(&player_id).copied().unwrap_or(0);
    if alive + queued >= MAX_VILLAGERS_PER_PLAYER {
        warn!("villagers: rejected queue request from `{player_id}` — already at the cap");
        return;
    }

    queues.0.insert(player_id, queued + 1);
}

/// Mirrors `VillagerQueues` onto each connected player's own car as a
/// replicated `VillagerQueue` — same "cheap self-healing sync every
/// frame" shape `economy::sync_wallet_components` already uses for
/// `Wallet`.
fn sync_villager_queue_components(
    queues: Res<VillagerQueues>,
    identities: Res<PlayerIdentities>,
    mut cars: Query<(&OwnedBy, &mut VillagerQueue)>,
) {
    for (owner, mut queue) in &mut cars {
        let Some(player_id) = identities.get(owner.0) else {
            continue;
        };
        let queued = queues.0.get(&player_id).copied().unwrap_or(0);
        if queue.queued != queued {
            queue.queued = queued;
        }
    }
}

fn wander_villagers(
    time: Res<Time>,
    origin: Res<WorldOrigin>,
    cars: Query<(&OwnedBy, &Transform)>,
    identities: Res<PlayerIdentities>,
    mut villagers: Query<(&mut VillagerAi, &mut VillagerSnapshot)>,
) {
    let mut rng = rand::thread_rng();
    let dt = time.delta_secs();

    for (mut ai, mut snapshot) in &mut villagers {
        ai.retarget_timer -= dt;
        if ai.retarget_timer <= 0.0 {
            ai.retarget_timer = RETARGET_SECS;
            let home = cars
                .iter()
                .find(|(owner, _)| identities.get(owner.0) == Some(ai.owner_player_id))
                .map(|(_, transform)| origin.to_true(transform.translation))
                .unwrap_or(DVec3::new(snapshot.true_x, 0.0, snapshot.true_z));
            let angle = rng.gen_range(0.0..std::f64::consts::TAU);
            let radius = rng.gen_range(0.0..WANDER_RADIUS);
            ai.target_true_x = home.x + angle.cos() * radius;
            ai.target_true_z = home.z + angle.sin() * radius;
        }

        let dx = ai.target_true_x - snapshot.true_x;
        let dz = ai.target_true_z - snapshot.true_z;
        let dist = (dx * dx + dz * dz).sqrt();
        if dist > 0.5 {
            let step = (VILLAGER_SPEED * dt as f64).min(dist);
            snapshot.true_x += dx / dist * step;
            snapshot.true_z += dz / dist * step;
        }
    }
}

fn gather_resources(
    time: Res<Time>,
    mut timer: ResMut<GatherTimer>,
    mut wallets: ResMut<Wallets>,
    persistence: Res<Persistence>,
    villagers: Query<&VillagerSnapshot>,
) {
    if !timer.0.tick(time.delta()).just_finished() {
        return;
    }
    for snapshot in &villagers {
        let (energy_gain, ore_gain) = if is_near_deposit(snapshot.true_x, snapshot.true_z) {
            (0.0, ORE_GATHER_RATE * GATHER_TICK_SECS)
        } else {
            (ENERGY_GATHER_RATE * GATHER_TICK_SECS, 0.0)
        };
        wallets.credit(snapshot.owner_player_id, energy_gain, ore_gain);
        if let Some((energy, ore)) = wallets.get(snapshot.owner_player_id) {
            persistence.send(PersistenceCommand::SaveWallet(WalletRow {
                player_id: snapshot.owner_player_id,
                energy: energy as f64,
                ore: ore as f64,
            }));
        }
    }
}
