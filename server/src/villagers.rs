use bevy::math::DVec3;
use bevy::prelude::*;
use bevy_replicon::prelude::*;
use rand::Rng;
use uuid::Uuid;

use shared::deposits::is_near_deposit;
use shared::protocol::{IdentifyMsg, VillagerSnapshot};
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
        .add_observer(spawn_villager_on_identify)
        .add_systems(Update, (wander_villagers, gather_resources));
    }
}

const WANDER_RADIUS: f64 = 30.0;
const RETARGET_SECS: f32 = 4.0;
const VILLAGER_SPEED: f64 = 4.0;
const GATHER_TICK_SECS: f32 = 1.0;
/// Deliberately well below a built Extraction Facility's own rate (1.0
/// ore/sec) or Energy Generator's (1.5 energy/sec) — this is a safety net
/// against being stuck at zero, not a replacement for actually building.
const ORE_GATHER_RATE: f32 = 0.3;
const ENERGY_GATHER_RATE: f32 = 0.3;

#[derive(Component)]
struct VillagerAi {
    owner_player_id: Uuid,
    target_true_x: f64,
    target_true_z: f64,
    retarget_timer: f32,
}

#[derive(Resource)]
struct GatherTimer(Timer);

/// Spawns exactly one villager per player, the first time they're
/// identified. Idempotent by checking for an existing `VillagerAi` with
/// this `player_id` rather than a separate "already seeded" set — the
/// same reasoning applies once villagers persist across restarts as it
/// does for `seed_wallet_on_identify`'s wallet check.
fn spawn_villager_on_identify(
    identify: On<FromClient<IdentifyMsg>>,
    mut commands: Commands,
    origin: Res<WorldOrigin>,
    cars: Query<(&OwnedBy, &Transform)>,
    existing: Query<&VillagerAi>,
) {
    if existing.iter().any(|v| v.owner_player_id == identify.player_id) {
        return;
    }
    let Some(client_entity) = identify.client_id.entity() else {
        return;
    };
    let Some((_, car_transform)) = cars.iter().find(|(owner, _)| owner.0 == client_entity) else {
        return;
    };
    let true_pos = origin.to_true(car_transform.translation);

    commands.spawn((
        VillagerAi {
            owner_player_id: identify.player_id,
            target_true_x: true_pos.x,
            target_true_z: true_pos.z,
            retarget_timer: 0.0,
        },
        VillagerSnapshot {
            owner_player_id: identify.player_id,
            true_x: true_pos.x,
            true_z: true_pos.z,
        },
        Replicated,
    ));
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
