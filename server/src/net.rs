use std::net::{Ipv4Addr, UdpSocket};
use std::time::SystemTime;

use bevy::prelude::*;
use bevy_replicon::prelude::*;
use bevy_replicon_renet::netcode::{NetcodeServerTransport, ServerAuthentication, ServerConfig};
use bevy_replicon_renet::renet::ConnectionConfig;
use bevy_replicon_renet::{RenetChannelsExt, RenetServer, RepliconRenetPlugins};

/// Bumped whenever the wire protocol changes incompatibly, so an old client
/// gets a clean rejection instead of garbled replication.
const PROTOCOL_ID: u64 = 1;
const DEFAULT_PORT: u16 = 5000;

pub struct ServerNetPlugin;

impl Plugin for ServerNetPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(RepliconRenetPlugins)
            .add_systems(Startup, start_listening);
    }
}

fn start_listening(mut commands: Commands, channels: Res<RepliconChannels>) {
    // "Internet-capable" per the multiplayer plan: binds 0.0.0.0, so this
    // is reachable from outside the local network with a port forwarded to
    // it. `ServerAuthentication::Unsecure` (no connect-token signing) is
    // fine for playing with trusted friends, not hardened against
    // spoofing — see the plan's risk callouts.
    let port: u16 = std::env::var("TERRAIN_CAR_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT);

    let server = RenetServer::new(ConnectionConfig {
        server_channels_config: channels.server_configs(),
        client_channels_config: channels.client_configs(),
        ..Default::default()
    });

    let current_time = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("system clock is set before the Unix epoch");
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port))
        .unwrap_or_else(|e| panic!("failed to bind UDP socket on port {port}: {e}"));
    let server_config = ServerConfig {
        current_time,
        max_clients: 16,
        protocol_id: PROTOCOL_ID,
        authentication: ServerAuthentication::Unsecure,
        public_addresses: Default::default(),
    };
    let transport = NetcodeServerTransport::new(server_config, socket)
        .expect("failed to start netcode server transport");

    commands.insert_resource(server);
    commands.insert_resource(transport);

    info!("server: listening on 0.0.0.0:{port} (protocol {PROTOCOL_ID})");
}
