use bevy::prelude::*;
use bevy_replicon::prelude::*;
use shared::protocol::{PingBroadcastMsg, PingMsg, PlayerInfo};
use shared::worldspace::WorldOrigin;

use crate::car_sim::OwnedBy;

/// Map pings (`P`, client's `pings.rs`) — resolves the sender's own car
/// position server-side (never trusts a client-supplied location) and
/// broadcasts it with their identity attached, same "no client payload,
/// server resolves and enriches, broadcasts a rich message" shape
/// `weapons.rs`'s `FireGunMsg` -> `GunFiredMsg` already uses.
pub struct PingsPlugin;

impl Plugin for PingsPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(handle_ping);
    }
}

fn handle_ping(
    ping: On<FromClient<PingMsg>>,
    mut commands: Commands,
    origin: Res<WorldOrigin>,
    cars: Query<(&OwnedBy, &Transform, &PlayerInfo)>,
) {
    let Some(client_entity) = ping.client_id.entity() else {
        return;
    };
    let Some((_, transform, info)) = cars.iter().find(|(owner, ..)| owner.0 == client_entity) else {
        return;
    };
    let true_pos = origin.to_true(transform.translation);

    commands.server_trigger(ToClients {
        targets: SendTargets::All,
        message: PingBroadcastMsg {
            player_id: info.player_id,
            username: info.username.clone(),
            true_x: true_pos.x,
            true_z: true_pos.z,
        },
    });
}
