use bevy::math::DVec3;
use bevy::prelude::*;
use shared::protocol::{PlayerInfo, PlayerOnFootSnapshot};
use shared::terrain_gen::{height_at, TerrainNoise};

use crate::owner_color::color_for_owner;
use crate::pilot::{PILOT_CYLINDER_LENGTH, PILOT_RADIUS};
use crate::player_account::LocalPlayerAccount;
use crate::worldspace::WorldOrigin;

/// Renders every *other* player's on-foot avatar from their replicated
/// `PlayerOnFootSnapshot` (see that component's own docs on why it exists
/// at all — before this, a player walking around on foot was invisible to
/// everyone else in every sense). The local player's own on-foot avatar is
/// still `pilot.rs`'s separate, client-local-only `Pilot` entity — this
/// module only ever renders *remote* players, and only while their own
/// `on_foot` flag says they actually are one (hidden the instant they
/// board a car/plane, whose own existing render path already covers them).
pub struct RemotePlayersPlugin;

impl Plugin for RemotePlayersPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, sync_remote_pilot_visuals);
    }
}

/// Marks a player-account entity that's already had its capsule mesh
/// inserted — checked instead of a one-shot `On<Insert, PlayerOnFootSnapshot>`
/// observer specifically to sidestep the login replication race
/// `player_account.rs`'s own `tag_local_player_account` docs describe:
/// `PlayerOnFootSnapshot` can replicate to this client before
/// `LocalPlayerAccount` gets tagged onto that same entity (both race each
/// other, replication order isn't guaranteed), so an insert-time-only
/// check could wrongly treat *your own* account as a remote player for one
/// frame and leave a stray capsule stuck at your own position forever. A
/// retried `Update` system re-checks `Without<LocalPlayerAccount>` every
/// frame until it actually spawns something, so it simply never acts
/// during that race window — same fix shape `tag_local_car`/
/// `tag_local_plane` already use for the identical class of race.
#[derive(Component)]
struct RemotePilotVisual;

#[allow(clippy::type_complexity)]
fn sync_remote_pilot_visuals(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    noise: Res<TerrainNoise>,
    origin: Res<WorldOrigin>,
    mut accounts: Query<
        (
            Entity,
            &PlayerOnFootSnapshot,
            &PlayerInfo,
            Option<&RemotePilotVisual>,
            Option<&mut Transform>,
            Option<&mut Visibility>,
        ),
        Without<LocalPlayerAccount>,
    >,
) {
    for (entity, snapshot, info, has_visual, transform, visibility) in &mut accounts {
        if has_visual.is_none() {
            let color = color_for_owner(info.player_id);
            commands.entity(entity).insert((
                Mesh3d(meshes.add(Capsule3d::new(PILOT_RADIUS, PILOT_CYLINDER_LENGTH))),
                MeshMaterial3d(materials.add(StandardMaterial {
                    base_color: color,
                    perceptual_roughness: 0.8,
                    ..default()
                })),
                Transform::default(),
                Visibility::Hidden,
                RemotePilotVisual,
            ));
            // The components just queued above via `Commands` aren't
            // visible on `entity` until next frame — nothing left to do
            // for this one until then.
            continue;
        }
        let (Some(mut transform), Some(mut visibility)) = (transform, visibility) else {
            continue;
        };
        if !snapshot.on_foot {
            *visibility = Visibility::Hidden;
            continue;
        }
        *visibility = Visibility::Visible;

        let ground_y = height_at(&noise, snapshot.true_x, snapshot.true_z);
        let local = (DVec3::new(snapshot.true_x, 0.0, snapshot.true_z) - origin.offset).as_vec3();
        transform.translation = Vec3::new(local.x, ground_y + PILOT_RADIUS + PILOT_CYLINDER_LENGTH * 0.5, local.z);
        transform.rotation = Quat::from_rotation_y(snapshot.rotation_y);
    }
}
