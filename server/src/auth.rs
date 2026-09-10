use bevy::prelude::*;
use bevy_replicon::prelude::*;
use shared::auth::{validate_password, validate_username};
use shared::protocol::{
    AuthResultMsg, LoginMsg, PlayerInfo, PlayerOnFootSnapshot, RegisterMsg, VillagerQueue, Wallet, WorldRegenMsg,
};
use shared::terrain_gen::TerrainNoise;
use shared::worldspace::WorldOrigin;

use bevy::math::DVec3;
use crate::car_sim::{
    pick_spawn_point, CurrentWorldState, OwnedBy, PlayerIdentities, PlayerPositions, PlayerRegistry,
    SpawnAnchor,
};
use crate::economy::Wallets;
use crate::persistence::{AuthOutcome, Persistence, PersistenceCommand, WalletRow};
use crate::villagers::{spawn_villager_for_new_player, VillagerAi};

/// Replaces the old trust-whatever-`player_id`-the-client-claims model
/// (`IdentifyMsg`) with real server-verified accounts. A client can't do
/// anything — no car, no wallet, no buildings — until a `RegisterMsg` or
/// `LoginMsg` round-trips through here successfully; see the plan this
/// shipped under for why (unique usernames + passwords).
pub struct AuthPlugin;

impl Plugin for AuthPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<AuthOutcomeReceived>()
            .add_observer(apply_register)
            .add_observer(apply_login)
            .add_systems(Update, apply_auth_outcome);
    }
}

/// Bridges `persistence.rs`'s channel-based `PersistenceEvent::AuthResult`
/// into a plain Bevy message. Necessary because `Persistence::try_recv()`
/// drains one shared channel — only one system may ever call it (see
/// `economy::apply_loaded_state`, the sole drain point), so that system
/// forwards anything auth-shaped here instead of this module polling the
/// channel itself and silently stealing `Loaded`/`Unavailable` events (or
/// vice versa).
#[derive(Message)]
pub struct AuthOutcomeReceived {
    pub client_entity: Entity,
    pub outcome: AuthOutcome,
}

fn apply_register(
    register: On<FromClient<RegisterMsg>>,
    mut commands: Commands,
    persistence: Res<Persistence>,
) {
    let Some(client_entity) = register.client_id.entity() else {
        return;
    };
    if let Err(reason) = validate_username(&register.username) {
        reject(&mut commands, client_entity, reason);
        return;
    }
    if let Err(reason) = validate_password(&register.password) {
        reject(&mut commands, client_entity, reason);
        return;
    }
    if !persistence.is_available() {
        reject(&mut commands, client_entity, "server has no database configured");
        return;
    }
    persistence.send(PersistenceCommand::Register {
        client_entity,
        username: register.username.clone(),
        password: register.password.clone(),
    });
}

fn apply_login(login: On<FromClient<LoginMsg>>, mut commands: Commands, persistence: Res<Persistence>) {
    let Some(client_entity) = login.client_id.entity() else {
        return;
    };
    if !persistence.is_available() {
        reject(&mut commands, client_entity, "server has no database configured");
        return;
    }
    persistence.send(PersistenceCommand::Login {
        client_entity,
        username: login.username.clone(),
        password: login.password.clone(),
    });
}

/// Immediate rejection with no DB round trip at all — bad input, or no
/// database configured. Same message shape a real `AuthOutcome::Error`
/// produces, just skipping the channel entirely since there's nothing to
/// wait on.
fn reject(commands: &mut Commands, client_entity: Entity, message: &str) {
    commands.server_trigger(ToClients {
        targets: SendTargets::Single(ClientId::Client(client_entity)),
        message: AuthResultMsg {
            ok: false,
            player_id: None,
            message: message.to_string(),
            spawn_true_x: 0.0,
            spawn_true_z: 0.0,
        },
    });
}

/// On success, spawns this connection's `PlayerAccount` entity — `Wallet`,
/// `PlayerInfo`, `VillagerQueue`, ephemeral-per-connection via `OwnedBy`
/// exactly like a car used to be (see `car_sim::despawn_player_on_disconnect`).
/// No car spawns here at all anymore: a car is something you build (a
/// completed `Hangar`, see `car_sim::spawn_cars_from_hangars`), not a free
/// login gift — the player starts on foot, at the same spot this picks for
/// the starter villager, which the response's `spawn_true_x`/`spawn_true_z`
/// tells the client so it can place the on-foot avatar there too (see
/// `client::pilot`'s `spawn_pilot_after_login`).
#[allow(clippy::too_many_arguments)]
fn apply_auth_outcome(
    mut commands: Commands,
    mut events: MessageReader<AuthOutcomeReceived>,
    mut identities: ResMut<PlayerIdentities>,
    mut wallets: ResMut<Wallets>,
    persistence: Res<Persistence>,
    noise: Res<TerrainNoise>,
    origin: Res<WorldOrigin>,
    registry: Res<PlayerRegistry>,
    mut anchor: ResMut<SpawnAnchor>,
    world_state: Res<CurrentWorldState>,
    villagers: Query<&VillagerAi>,
    positions: Res<PlayerPositions>,
) {
    for event in events.read() {
        let client_entity = event.client_entity;
        let (ok, identity, message) = match &event.outcome {
            AuthOutcome::Success { player_id, username } => {
                (true, Some((*player_id, username.clone())), "welcome".to_string())
            }
            AuthOutcome::UsernameTaken => (false, None, "username already taken".to_string()),
            AuthOutcome::InvalidCredentials => (false, None, "invalid username or password".to_string()),
            AuthOutcome::Error(e) => (false, None, e.clone()),
        };

        let Some((player_id, username)) = identity else {
            commands.server_trigger(ToClients {
                targets: SendTargets::Single(ClientId::Client(client_entity)),
                message: AuthResultMsg { ok, player_id: None, message, spawn_true_x: 0.0, spawn_true_z: 0.0 },
            });
            continue;
        };

        identities.insert(client_entity, player_id);
        let (energy, ore) = wallets.get_or_seed(player_id);
        persistence.send(PersistenceCommand::SaveWallet(WalletRow {
            player_id,
            energy: energy as f64,
            ore: ore as f64,
        }));

        let index = registry
            .index_for(client_entity)
            .unwrap_or_else(|| panic!("client `{client_entity}` logged in without ever connecting"));
        // A returning player resumes wherever `PlayerPositionMsg` last saw
        // them (loaded from Postgres at startup, or updated live if they've
        // been online since — see `car_sim::PlayerPositions`); a genuinely
        // new player has no entry yet, so falls back to a freshly computed
        // spawn-anchor point exactly as before.
        let spawn_true = match positions.get(player_id) {
            Some((true_x, true_z)) => DVec3::new(true_x, 0.0, true_z),
            None => pick_spawn_point(index, &mut anchor, &noise, &origin),
        };

        commands.spawn((
            OwnedBy(client_entity),
            Replicated,
            PlayerInfo { player_id, username },
            Wallet { energy, ore },
            VillagerQueue::default(),
            // Starts `on_foot: false` (correct — the player hasn't sent a
            // real `PlayerPositionMsg` yet) — `car_sim::apply_player_position`
            // fills this in for real the moment one arrives.
            PlayerOnFootSnapshot::default(),
        ));

        commands.server_trigger(ToClients {
            targets: SendTargets::Single(ClientId::Client(client_entity)),
            message: AuthResultMsg {
                ok,
                player_id: Some(player_id),
                message,
                spawn_true_x: spawn_true.x,
                spawn_true_z: spawn_true.z,
            },
        });

        // Catch-up: if the world was already regenerated before this client
        // connected, they'd otherwise never learn the new seed (a broadcast
        // WorldRegenMsg doesn't retroactively reach clients who weren't
        // connected when it was sent). Skipped when the world is still on
        // its untouched default — every fresh client already starts there.
        if let Some(seed) = world_state.seed {
            commands.server_trigger(ToClients {
                targets: SendTargets::Single(ClientId::Client(client_entity)),
                message: WorldRegenMsg { seed, origin_x: origin.offset.x, origin_z: origin.offset.z },
            });
        }

        info!("server: spawned player account for client `{client_entity}` (player #{index})");
        spawn_villager_for_new_player(&mut commands, &villagers, player_id, spawn_true.x, spawn_true.z);
    }
}
