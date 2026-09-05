use bevy::prelude::*;
use bevy_replicon::prelude::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::car_physics::CarChassis;
use crate::combat::Health;

/// Client-only marker: the client inserts this on the car entity it spawns
/// immediately on connecting, before the server's authoritative car for
/// that same client has had time to arrive — so the local car appears and
/// responds to input with zero added network latency (see the "client-side
/// prediction with server reconciliation" section of the multiplayer plan).
///
/// Requires `Signature::of::<LocalCar>()`, which is bevy_replicon's own
/// built-in mechanism for exactly this: when the server later spawns the
/// authoritative car for this client with a matching signature (see
/// `spawn_car_signature` below), replicon merges it into this same client
/// entity instead of spawning a visible duplicate "ghost" car.
///
/// Carries the client's own renet connection id (see
/// `bevy_replicon_renet`'s `NetworkId`, which the server can read straight
/// off the `ConnectedClient` entity — same value the client generated for
/// itself before ever connecting, so both sides embed it identically with
/// no extra round trip). This field is *not* cosmetic: `Signature`'s hash
/// covers a component's full `Hash` impl, and this component was originally
/// a bare marker with no fields at all — meaning every player's car hashed
/// to the exact same value. `bevy_replicon`'s internal signature registry
/// is a single global `hash -> entity` map with no per-client scoping of
/// its own (it relies on callers giving it an actually-unique hash), so
/// every second-and-later connection's registration silently collided with
/// the first and was dropped — the likely cause of two players' cars
/// getting confused with each other. Embedding a real per-client value
/// fixes the collision at the root, the same way bevy_replicon's own
/// "predicting a projectile" example gives its signature uniqueness via a
/// real field rather than relying on `for_client` scoping alone.
#[derive(Component, Hash, Clone, Copy)]
#[require(Signature::of::<LocalCar>())]
pub struct LocalCar(pub u64);

/// Server-side helper: the signature a newly-connected client's own
/// authoritative car must carry so it merges into that client's
/// already-locally-spawned `LocalCar` entity instead of duplicating it.
/// `network_id` must be that client's own renet connection id (its
/// `NetworkId` component) so the hash matches what the client computed for
/// itself — see `LocalCar`'s docs for why this can no longer be a bare
/// marker.
pub fn spawn_car_signature(client_entity: Entity, network_id: u64) -> (LocalCar, Signature) {
    (
        LocalCar(network_id),
        Signature::of::<LocalCar>().for_client(client_entity),
    )
}

/// Sent client -> server every `FixedUpdate` tick with the local player's
/// current input. Sequence-numbered so the client can track which inputs
/// the server has actually applied (echoed back in `CarSnapshot`) for
/// reconciliation, and replay any newer, not-yet-applied inputs after a
/// correction.
///
/// `Unreliable`: each message fully supersedes the last (only "current
/// input" matters), so a dropped one is harmless and paying for
/// retransmission would only add latency for no benefit.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct CarInputMsg {
    pub sequence: u32,
    pub throttle: f32,
    pub steer: f32,
    pub brake: bool,
}

/// Sent client -> server when the player presses R to unstick their car.
/// The client also applies an identical reset locally, immediately (zero
/// latency) — this message is what makes the *server's* authoritative
/// position agree, so a later `CarSnapshot` doesn't read as "a huge desync"
/// and yank the car back to where it was before the reset (reconciliation
/// otherwise has no way to know a teleport was intentional). Carries the
/// search center in true space (rather than a target position) so the
/// server runs the exact same `find_flat_spawn` search the client already
/// did — same reasoning as terrain/obstacles never being replicated: two
/// sides computing the same deterministic answer beats sending it.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct CarResetMsg {
    pub near_true_x: f64,
    pub near_true_z: f64,
}

/// Replicated snapshot of a car's authoritative physical state — a plain
/// data component rather than Rapier's own `Transform`/`Velocity` types, so
/// `shared` (and the headless server) never need to depend on bevy_rapier3d
/// or rendering machinery just to describe "where is this car and how fast
/// is it moving." `last_input_sequence` is the reconciliation anchor: the
/// client compares its own predicted state at that same sequence number
/// against this snapshot to decide whether, and how hard, to correct.
///
/// `reset_generation` is bumped by the server every time it processes a
/// `CarResetMsg` for this car. The client remembers the generation at which
/// *it* last reset locally and ignores any snapshot older than that — those
/// necessarily reflect the car's pre-reset position, arriving after the
/// local reset just because of ordinary network latency. Without this, a
/// stale snapshot would look exactly like a huge, real desync and get
/// corrected against, yanking the car back to where it was before you
/// pressed R.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default)]
pub struct CarSnapshot {
    pub translation: Vec3,
    pub rotation: Quat,
    pub linear_velocity: Vec3,
    pub angular_velocity: Vec3,
    pub last_input_sequence: u32,
    pub reset_generation: u32,
}

/// Sent client -> server when the player presses N, requesting a full world
/// regeneration (new terrain seed). Only takes effect if the server
/// independently confirms the sender holds op privileges — never trust
/// client-side gating alone for authorization, the client only uses its own
/// (unsynced) belief about op status to decide whether pressing N is worth
/// sending at all.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct RegenRequestMsg;

/// Sent server -> all clients (individually to each newly-connecting client
/// too, as a catch-up, in case a regen already happened before they joined)
/// whenever the world regenerates. Carries what every client needs to
/// reseed identically: `shared::terrain_gen`/`shared::obstacles` are pure
/// functions of `(seed, coordinates)`, so this one small message is enough
/// for every client to independently rebuild matching terrain and
/// obstacles — no need for the heavier machinery of full resource
/// replication for something that only changes on a rare, explicit
/// operator action.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct WorldRegenMsg {
    pub seed: u32,
    pub origin_x: f64,
    pub origin_z: f64,
}

/// Sent client -> server repeatedly (every couple of seconds — see client's
/// `net.rs`) rather than exactly once, since there's no explicit "you are
/// now connected" signal this game currently listens for and a single
/// fire-at-Startup attempt could race the transport actually being ready.
/// The server treats every receipt as idempotent (just records/overwrites
/// the mapping — see `car_sim.rs`'s `PlayerIdentities`), so re-sending is
/// harmless. `player_id` is a UUID generated once and saved to a local
/// file on first launch (see `net.rs`'s `load_or_create_player_id`) rather
/// than derived from anything connection-specific — it's the one piece of
/// "who is this, across every future session" identity buildings/wallets
/// (a later addition) key off, unlike `LocalCar`'s per-connection id.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct IdentifyMsg {
    pub player_id: Uuid,
}

/// Sent client -> server from the live tuning panel (`Tab`, see client's
/// `tuning_ui.rs`) whenever the player drags a slider. Carries every
/// tunable field at once (not a delta) — simplest to reason about, and
/// cheap enough at `Ordered` since sliders don't fire every frame. The
/// server clamps every field to the same ranges the UI itself enforces
/// before writing them into the sender's own `CarChassis` (never trust
/// client-side clamping alone) — see `car_sim.rs`'s `apply_car_tune`.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct TuneCarMsg {
    pub spring_stiffness: f32,
    pub damper: f32,
    pub engine_force: f32,
    pub brake_force: f32,
    pub traction: f32,
    pub max_steer_rad: f32,
}

/// Sent client -> server when the player fires the front-mounted gun.
/// Carries nothing: the server already knows who's shooting (the sending
/// client's own car) and reads that car's current `Transform` for the
/// muzzle position and aim direction — same reasoning as `RegenRequestMsg`
/// carrying no data of its own. `Unreliable`: a dropped fire request just
/// means that shot never happened, no state to reconcile either way.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct FireGunMsg;

/// Sent server -> all clients after resolving a `FireGunMsg` — the
/// authoritative muzzle and end points (either the actual hit point, or a
/// point at the gun's max range if nothing was hit), so every client draws
/// the identical tracer/muzzle-flash/impact regardless of who fired or
/// whether it connected. Deliberately carries no shooter/target `Entity`:
/// every cosmetic effect this drives (flash, smoke, shells, tracer, impact)
/// only needs these two points plus the direction between them, which
/// sidesteps needing entity-id mapping across the network for a message
/// that's otherwise purely visual. `y` is plain local-space (never
/// affected by a `WorldOrigin` rebase, unlike x/z — see `WorldOrigin`'s
/// docs), so only x/z need true-space f64 precision.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct GunFiredMsg {
    pub muzzle_true_x: f64,
    pub muzzle_y: f32,
    pub muzzle_true_z: f64,
    pub end_true_x: f64,
    pub end_y: f32,
    pub end_true_z: f64,
    pub hit: bool,
}

/// Sent server -> all clients whenever a lightning strike lands (see
/// server's `lightning.rs`). Carries only the strike's true-space location
/// and blast radius — every client independently spawns the same cosmetic
/// flash/shockwave effect at that position, converting to local coordinates
/// with its own current `WorldOrigin` the same way `WorldRegenMsg` and
/// `CarResetMsg` already do. The actual physics knockback happens only on
/// the server (a direct `Velocity` kick to any car in range) and reaches
/// clients through the existing `CarSnapshot` replication rather than this
/// message — this is purely "so every screen shows the same boom."
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct LightningStrikeMsg {
    pub true_x: f64,
    pub true_z: f64,
    pub radius: f32,
}

/// Registers everything that must be identical between client and server:
/// which components replicate (and how often), and the client -> server /
/// server -> client event channels. Both binaries call this exact function
/// on startup so registration order — which determines wire IDs — can never
/// drift between them.
pub fn register_protocol(app: &mut App) {
    app
        // Was replicate_once: with the live tuning panel (TuneCarMsg), a
        // car's chassis stats can now change after spawn, and every
        // connected client needs to see that change too, not just the
        // player who tuned it.
        .replicate::<CarChassis>()
        .replicate::<CarSnapshot>()
        // Health changes constantly once guns exist — every client needs
        // to see it live, not just once at spawn.
        .replicate::<Health>()
        .add_client_event::<CarInputMsg>(Channel::Unreliable)
        .add_client_event::<CarResetMsg>(Channel::Ordered)
        .add_client_event::<RegenRequestMsg>(Channel::Ordered)
        .add_client_event::<FireGunMsg>(Channel::Unreliable)
        .add_server_event::<GunFiredMsg>(Channel::Unreliable)
        .add_client_event::<TuneCarMsg>(Channel::Ordered)
        .add_client_event::<IdentifyMsg>(Channel::Ordered)
        .add_server_event::<WorldRegenMsg>(Channel::Ordered)
        // Unreliable: purely cosmetic, and another strike is at most 10s
        // away anyway, so a dropped one is never worth retransmitting.
        .add_server_event::<LightningStrikeMsg>(Channel::Unreliable)
        // WorldRegenMsg carries no entity/component references, so it's
        // safe (and necessary) to deliver even to a client whose
        // replication handshake isn't fully established yet — this is what
        // lets the server catch a newly-connecting client up on a regen
        // that already happened (see car_sim.rs's spawn_car_on_connect),
        // sent in the same observer that spawns their car. Without this,
        // replicon silently drops that catch-up message.
        .make_event_independent::<WorldRegenMsg>();
}
