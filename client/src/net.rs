use std::net::{Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::SystemTime;

use bevy::prelude::*;
use bevy_replicon::prelude::*;
use bevy_replicon_renet::netcode::{ClientAuthentication, NetcodeClientTransport};
use bevy_replicon_renet::renet::ConnectionConfig;
use bevy_replicon_renet::{RenetChannelsExt, RenetClient, RepliconRenetPlugins};

/// Must match server/src/net.rs's constant exactly — a client with a
/// different value gets a clean rejection instead of garbled replication.
const PROTOCOL_ID: u64 = 1;
const DEFAULT_SERVER_ADDR: &str = "127.0.0.1:5000";

pub struct ClientNetPlugin;

impl Plugin for ClientNetPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(RepliconRenetPlugins).add_systems(Startup, connect_to_server);
    }
}

/// This client's own renet connection id, generated once up front (see
/// `main.rs`, inserted before any Startup system runs) rather than as a
/// local variable inside `connect_to_server` — car.rs's `spawn_car` also
/// needs this exact value to embed in `LocalCar` (see protocol.rs's docs on
/// why that component needs a real per-client value), and Startup systems
/// across different plugins have no guaranteed relative order, so both
/// sides reading one pre-computed resource is simpler than trying to order
/// two Startup systems against each other.
///
/// Deliberately still time-based, *not* derived from the account's
/// `player_id` (learned only after login — see `client::auth_ui`) — tried
/// deriving it from a stable id once already, for the old anonymous
/// identity, and it broke reconnecting: renet's own replay/duplicate-
/// connection protection rejects a new connection attempt that reuses the
/// same transport-level `client_id` too soon after the previous one using
/// it disconnected. This field's only job is "distinguish concurrent
/// transport-level connections," which a fresh value every launch does
/// correctly; genuine cross-session player identity is entirely
/// server-verified now (`RegisterMsg`/`LoginMsg`), kept deliberately
/// decoupled from this one.
#[derive(Resource, Clone, Copy)]
pub struct LocalClientId(pub u64);

impl Default for LocalClientId {
    fn default() -> Self {
        let millis = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("system clock is set before the Unix epoch")
            .as_millis();
        Self(millis as u64)
    }
}

fn connect_to_server(
    mut commands: Commands,
    channels: Res<RepliconChannels>,
    client_id: Res<LocalClientId>,
) {
    // `TERRAIN_CAR_SERVER=host:port cargo run` to connect elsewhere;
    // defaults to loopback for local testing (server + one client on the
    // same machine). Resolved via `ToSocketAddrs` (does a real DNS lookup)
    // rather than `SocketAddr::parse` (which only accepts a literal IP) —
    // the latter silently falling back to the loopback default on any
    // hostname was a real bug: it meant a mistyped or unresolvable address
    // connected to *localhost* instead of failing loudly.
    let server_env = std::env::var("TERRAIN_CAR_SERVER").unwrap_or_default();
    let target = if server_env.is_empty() {
        DEFAULT_SERVER_ADDR
    } else {
        server_env.as_str()
    };
    let resolved: Vec<SocketAddr> = target
        .to_socket_addrs()
        .unwrap_or_else(|e| panic!("failed to resolve TERRAIN_CAR_SERVER `{target}`: {e}"))
        .collect();
    // Prefer IPv4: the server only binds an IPv4 unspecified address (see
    // server/src/net.rs), but a hostname lookup (Tailscale MagicDNS names
    // in particular, which have both a 100.x.y.z IPv4 and an fd7a:...
    // IPv6 address) can return the IPv6 result first — silently trying
    // that would just hang against a server that was never listening on
    // it.
    let server_addr = resolved
        .iter()
        .find(|addr| addr.is_ipv4())
        .or_else(|| resolved.first())
        .copied()
        .unwrap_or_else(|| panic!("`{target}` resolved to no addresses"));

    let client = RenetClient::new(ConnectionConfig {
        server_channels_config: channels.server_configs(),
        client_channels_config: channels.client_configs(),
        ..Default::default()
    });

    let current_time = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("system clock is set before the Unix epoch");
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .expect("failed to bind a local UDP socket for the client");
    // Time-based id: fine for "trusted friends over a known address," not
    // a real identity system — see the plan's risk callouts on
    // ServerAuthentication::Unsecure. Shared with car.rs's spawn_car via
    // the `LocalClientId` resource — see its docs.
    let authentication = ClientAuthentication::Unsecure {
        client_id: client_id.0,
        protocol_id: PROTOCOL_ID,
        server_addr,
        user_data: None,
    };
    let transport = NetcodeClientTransport::new(current_time, authentication, socket)
        .expect("failed to start netcode client transport");

    commands.insert_resource(client);
    commands.insert_resource(transport);

    info!("client: connecting to {server_addr} (protocol {PROTOCOL_ID})");
}
