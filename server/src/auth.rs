use bevy::prelude::*;
use bevy_replicon::prelude::*;
use bevy_replicon::shared::backend::connected_client::NetworkId;
use shared::auth::{validate_password, validate_username};
use shared::protocol::{AuthResultMsg, LoginMsg, RegisterMsg, Wallet};
use shared::terrain_gen::TerrainNoise;
use shared::worldspace::WorldOrigin;

use crate::car_sim::{spawn_car_for, CurrentWorldState, PlayerIdentities, PlayerRegistry, SpawnAnchor};
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
        message: AuthResultMsg { ok: false, player_id: None, message: message.to_string() },
    });
}

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
    network_ids: Query<&NetworkId>,
    villagers: Query<&VillagerAi>,
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

        commands.server_trigger(ToClients {
            targets: SendTargets::Single(ClientId::Client(client_entity)),
            message: AuthResultMsg {
                ok,
                player_id: identity.as_ref().map(|(id, _)| *id),
                message,
            },
        });

        let Some((player_id, username)) = identity else {
            continue;
        };
        identities.insert(client_entity, player_id);
        let (energy, ore) = wallets.get_or_seed(player_id);
        persistence.send(PersistenceCommand::SaveWallet(WalletRow {
            player_id,
            energy: energy as f64,
            ore: ore as f64,
        }));
        let spawn_true = spawn_car_for(
            &mut commands,
            &noise,
            &origin,
            &registry,
            &mut anchor,
            &world_state,
            &network_ids,
            client_entity,
            player_id,
            username,
            Wallet { energy, ore },
        );
        spawn_villager_for_new_player(&mut commands, &villagers, player_id, spawn_true.x, spawn_true.z);
    }
}
