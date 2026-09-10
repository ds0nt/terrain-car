use bevy::math::DVec3;
use bevy::prelude::*;
use bevy_rapier3d::prelude::*;
use bevy_replicon::prelude::*;
use shared::car_physics::CarChassis;
use shared::protocol::{ChatBroadcastMsg, ChatMsg, PlaneSnapshot, PlayerInfo, TeleportMsg};
use shared::terrain_gen::{find_flat_spawn, height_at, TerrainNoise};
use shared::worldspace::WorldOrigin;
use uuid::Uuid;

use crate::car_sim::{OpList, PlayerIdentities, PlayerPositions};

/// In-game chat (Enter, see client's `chat.rs`) doubling as the command
/// line for `/tp` and `/respawn` — same "no separate admin protocol, just
/// gate on `OpList` the way `RegenRequestMsg` already does" shape the rest
/// of this project's op-only actions use.
pub struct ChatPlugin;

impl Plugin for ChatPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(handle_chat);
    }
}

/// Search radius for `/respawn`, centered on true-space `(0, 0)` — matches
/// `car_sim::FIRST_SPAWN_SEARCH_RADIUS` (private to that module), reused
/// here as its own constant since a fresh flat-ground search from scratch
/// (no existing anchor to stay near) needs the same wide radius a brand
/// new player's very first spawn does.
const RESPAWN_SEARCH_RADIUS: f64 = 300.0;

/// Mirrors `aircraft::PLANE_HALF_EXTENTS.y + 0.5` (also private) — how far
/// above ground a teleported plane needs to sit so it doesn't spawn
/// clipped into the terrain it's landing on.
const PLANE_ALTITUDE_MARGIN: f32 = 1.1;

const MAX_CHAT_CHARS: usize = 240;

fn handle_chat(
    chat: On<FromClient<ChatMsg>>,
    mut commands: Commands,
    identities: Res<PlayerIdentities>,
    op_list: Res<OpList>,
    noise: Res<TerrainNoise>,
    origin: Res<WorldOrigin>,
    mut positions: ResMut<PlayerPositions>,
    players: Query<&PlayerInfo>,
    mut cars: Query<(&CarChassis, &mut Transform, &mut Velocity, &mut ExternalForce), Without<PlaneSnapshot>>,
    mut planes: Query<(&mut Transform, &mut Velocity, &mut PlaneSnapshot), Without<CarChassis>>,
) {
    let Some(sender_entity) = chat.client_id.entity() else {
        return;
    };
    let Some(sender_id) = identities.get(sender_entity) else {
        return;
    };
    let Some(sender_name) = players.iter().find(|p| p.player_id == sender_id).map(|p| p.username.clone())
    else {
        return;
    };

    let text: String = chat.text.trim().chars().take(MAX_CHAT_CHARS).collect();
    if text.is_empty() {
        return;
    }

    let Some(command) = text.strip_prefix('/') else {
        info!("chat: <{sender_name}> {text}");
        commands.server_trigger(ToClients {
            targets: SendTargets::All,
            message: ChatBroadcastMsg { player_id: Some(sender_id), username: sender_name, text },
        });
        return;
    };

    let mut parts = command.split_whitespace();
    let verb = parts.next().unwrap_or("");
    let reply = |commands: &mut Commands, text: String| {
        commands.server_trigger(ToClients {
            targets: SendTargets::Single(ClientId::Client(sender_entity)),
            message: ChatBroadcastMsg { player_id: None, username: "server".to_string(), text },
        });
    };

    match verb {
        "tp" => {
            if !op_list.is_op(sender_entity) {
                reply(&mut commands, "you don't have permission to use /tp (op only)".to_string());
                return;
            }
            let (Some(name1), Some(name2)) = (parts.next(), parts.next()) else {
                reply(&mut commands, "usage: /tp <player> <player>".to_string());
                return;
            };
            let Some(target_id) = find_player_id(&players, name1) else {
                reply(&mut commands, format!("no such player '{name1}'"));
                return;
            };
            let Some(dest_id) = find_player_id(&players, name2) else {
                reply(&mut commands, format!("no such player '{name2}'"));
                return;
            };
            let Some((dest_x, dest_z)) = positions.get(dest_id) else {
                reply(&mut commands, format!("no known position for '{name2}' yet"));
                return;
            };

            teleport_player_to(
                target_id,
                DVec3::new(dest_x, 0.0, dest_z),
                &identities,
                &noise,
                &origin,
                &mut positions,
                &mut commands,
                &mut cars,
                &mut planes,
            );
            info!("chat: {sender_name} teleported {name1} to {name2}");
            reply(&mut commands, format!("teleported {name1} to {name2}"));
            if let Some(target_entity) = identities.entity_for(target_id)
                && target_entity != sender_entity
            {
                commands.server_trigger(ToClients {
                    targets: SendTargets::Single(ClientId::Client(target_entity)),
                    message: ChatBroadcastMsg {
                        player_id: None,
                        username: "server".to_string(),
                        text: format!("{sender_name} teleported you to {name2}"),
                    },
                });
            }
        }
        "respawn" => {
            let spawn_true = find_flat_spawn(&noise, DVec3::new(0.0, 0.0, 0.0), RESPAWN_SEARCH_RADIUS);
            teleport_player_to(
                sender_id,
                spawn_true,
                &identities,
                &noise,
                &origin,
                &mut positions,
                &mut commands,
                &mut cars,
                &mut planes,
            );
            info!("chat: {sender_name} respawned");
            reply(&mut commands, "respawned at world origin".to_string());
        }
        other => {
            reply(&mut commands, format!("unknown command: /{other} (try /respawn or /tp <player> <player>)"));
        }
    }
}

fn find_player_id(players: &Query<&PlayerInfo>, username: &str) -> Option<Uuid> {
    players.iter().find(|p| p.username.eq_ignore_ascii_case(username)).map(|p| p.player_id)
}

/// Moves every car and plane `target_player_id` owns to `target_true`
/// (ground-snapped, velocity zeroed — same reset shape
/// `car_sim::apply_car_reset`/`aircraft::apply_recall_plane` already use),
/// records it as their new known position, and — if they're actually
/// online right now — tells their own client to relocate their on-foot
/// avatar too (see `TeleportMsg`'s own docs on why that needs a separate
/// message while cars/planes don't).
#[allow(clippy::too_many_arguments)]
fn teleport_player_to(
    target_player_id: Uuid,
    target_true: DVec3,
    identities: &PlayerIdentities,
    noise: &TerrainNoise,
    origin: &WorldOrigin,
    positions: &mut PlayerPositions,
    commands: &mut Commands,
    cars: &mut Query<(&CarChassis, &mut Transform, &mut Velocity, &mut ExternalForce), Without<PlaneSnapshot>>,
    planes: &mut Query<(&mut Transform, &mut Velocity, &mut PlaneSnapshot), Without<CarChassis>>,
) {
    let ground_y = height_at(noise, target_true.x, target_true.z);
    let local = (target_true - origin.offset).as_vec3();

    for (chassis, mut transform, mut velocity, mut ext_force) in cars.iter_mut() {
        if chassis.owner_player_id != target_player_id {
            continue;
        }
        transform.translation = Vec3::new(local.x, ground_y + 2.0, local.z);
        transform.rotation = Quat::IDENTITY;
        *velocity = Velocity::zero();
        *ext_force = ExternalForce::default();
    }

    for (mut transform, mut velocity, mut snapshot) in planes.iter_mut() {
        if snapshot.owner_player_id != target_player_id {
            continue;
        }
        let altitude = ground_y + PLANE_ALTITUDE_MARGIN;
        transform.translation = Vec3::new(local.x, altitude, local.z);
        transform.rotation = Quat::IDENTITY;
        *velocity = Velocity::zero();
        snapshot.true_x = target_true.x;
        snapshot.true_z = target_true.z;
        snapshot.altitude = altitude;
        snapshot.rotation = Quat::IDENTITY;
        snapshot.linear_velocity = Vec3::ZERO;
    }

    positions.set(target_player_id, (target_true.x, target_true.z));

    if let Some(client_entity) = identities.entity_for(target_player_id) {
        commands.server_trigger(ToClients {
            targets: SendTargets::Single(ClientId::Client(client_entity)),
            message: TeleportMsg { true_x: target_true.x, true_z: target_true.z },
        });
    }
}
