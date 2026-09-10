use bevy::prelude::*;
use shared::protocol::PlayerInfo;

use crate::auth_ui::LocalPlayerId;

/// Tags the local player's own account entity — `Wallet`, `PlayerInfo`,
/// `VillagerQueue` all live here now (see `shared::protocol`'s docs on
/// why those moved off the car: a car is something you build, not a free
/// login gift, so it can no longer be assumed to exist at all, let alone
/// carry the player's account state). Every connected client's account
/// entity replicates to everyone (same as it did living on the car before
/// this — nothing new is exposed), so `hud.rs`/`building_ui.rs` need this
/// tag to pick out *their own* one specifically.
pub struct PlayerAccountPlugin;

impl Plugin for PlayerAccountPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, tag_local_player_account);
    }
}

#[derive(Component)]
pub struct LocalPlayerAccount;

/// A retried `Update` system, not a one-shot `On<Insert, PlayerInfo>`
/// observer — this account entity is spawned by the server in the very
/// same handler that sends the `AuthResultMsg` this client's own
/// `LocalPlayerId` gets set from (see `auth_ui.rs`'s `apply_auth_result`),
/// but replication and that message travel over entirely separate renet
/// channels with no ordering guarantee between them. An insert-time-only
/// check loses that race whenever the account entity's replication happens
/// to arrive first: `LocalPlayerId` still reads `None` at that instant, the
/// comparison fails, and — since an `On<Insert>` observer only ever fires
/// once — it *never* gets tagged, silently. That's exactly what live
/// testing hit ("i have no wallet", "no way to build anything"): the HUD's
/// wallet readout and the entire build bar both key off this tag, so
/// missing it doesn't just hide one field, it hides the ability to build
/// at all. Filtering on `Without<LocalPlayerAccount>` keeps this cheap
/// (already-tagged accounts, including every other connected player's,
/// are skipped every frame) while guaranteeing it keeps retrying until it
/// actually lands, whichever order the two race arms finish in.
fn tag_local_player_account(
    mut commands: Commands,
    local_player_id: Res<LocalPlayerId>,
    accounts: Query<(Entity, &PlayerInfo), Without<LocalPlayerAccount>>,
) {
    let Some(player_id) = local_player_id.0 else {
        return;
    };
    for (entity, info) in &accounts {
        if info.player_id == player_id {
            commands.entity(entity).insert(LocalPlayerAccount);
        }
    }
}
