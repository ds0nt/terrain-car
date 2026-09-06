use std::collections::VecDeque;

use bevy::math::DVec3;
use bevy::prelude::*;
use bevy_replicon::prelude::ClientTriggerExt;
use shared::protocol::{PingBroadcastMsg, PingMsg};
use shared::terrain_gen::{height_at, TerrainNoise};

use crate::fx::{FadeOut, GrowScale, Lifetime};
use crate::owner_color::color_for_owner;
use crate::worldspace::WorldOrigin;

/// `P` pings your current position for every other player — the server
/// resolves it (`server::pings`) and broadcasts `PingBroadcastMsg`, which
/// this module renders as a brief in-world beacon and records into
/// `RecentPings` for `players_ui.rs`'s pings list.
pub struct PingsPlugin;

impl Plugin for PingsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<RecentPings>()
            .add_systems(Update, send_ping_input)
            .add_observer(spawn_ping_fx);
    }
}

fn send_ping_input(keyboard: Res<ButtonInput<KeyCode>>, mut commands: Commands) {
    if keyboard.just_pressed(KeyCode::KeyP) {
        commands.client_trigger(PingMsg);
    }
}

/// One entry in the pings list — `received_at` is `Time::elapsed_secs()`
/// at the moment it arrived, so `players_ui.rs` can compute "Ns ago"
/// without this module needing to know anything about how that's
/// rendered.
pub(crate) struct PingEntry {
    pub(crate) username: String,
    pub(crate) received_at: f32,
}

/// Ring buffer of the last few pings, newest first — capped both by count
/// and by age so a quiet server's list doesn't grow forever or show stale
/// entries from many minutes ago.
#[derive(Resource, Default)]
pub(crate) struct RecentPings(pub(crate) VecDeque<PingEntry>);

const MAX_ENTRIES: usize = 8;
pub(crate) const PING_TTL_SECS: f32 = 20.0;

const BEACON_LIFETIME_SECS: f32 = 2.5;

fn spawn_ping_fx(
    ping: On<PingBroadcastMsg>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    noise: Res<TerrainNoise>,
    origin: Res<WorldOrigin>,
    time: Res<Time>,
    mut recent: ResMut<RecentPings>,
) {
    let ping_true = DVec3::new(ping.true_x, 0.0, ping.true_z);
    let local = (ping_true - origin.offset).as_vec3();
    let ground_y = height_at(&noise, ping.true_x, ping.true_z);
    let color = color_for_owner(ping.player_id);
    let emissive = color.to_linear() * 3.0;

    commands.spawn((
        Mesh3d(meshes.add(Cylinder::new(0.6, 12.0))),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: color.with_alpha(0.7),
            emissive,
            alpha_mode: AlphaMode::Blend,
            unlit: true,
            ..default()
        })),
        Transform::from_xyz(local.x, ground_y + 6.0, local.z),
        Lifetime::new(BEACON_LIFETIME_SECS),
        FadeOut { base_alpha: 0.7, base_emissive: emissive },
        // Pops in from nothing rather than appearing instantly at full
        // size — GrowScale is uniform, so growing *past* 1.0 here (like
        // lightning_fx.rs's expanding flash sphere) would stretch this
        // already-tall thin pillar absurdly; 0 -> 1 is just "grow in."
        GrowScale { start_scale: 0.0, end_scale: 1.0 },
    ));

    recent.0.push_front(PingEntry { username: ping.username.clone(), received_at: time.elapsed_secs() });
    recent.0.truncate(MAX_ENTRIES);
}
