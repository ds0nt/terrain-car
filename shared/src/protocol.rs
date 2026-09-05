use bevy::prelude::*;
use bevy_replicon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::car_physics::CarChassis;

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
/// entity instead of spawning a visible duplicate "ghost" car. This mirrors
/// the `ClientPlayer`/`Signature::of::<ClientPlayer>()` pattern from
/// bevy_replicon's own tic_tac_toe example, and the "predicting a
/// projectile on the client" pattern documented directly on `Signature`.
#[derive(Component, Hash, Clone, Copy)]
#[require(Signature::of::<LocalCar>())]
pub struct LocalCar;

/// Server-side helper: the signature a newly-connected client's own
/// authoritative car must carry so it merges into that client's
/// already-locally-spawned `LocalCar` entity instead of duplicating it.
/// `LocalCar` has no fields (every client hashes identically), so
/// `for_client` alone is what scopes the match to the right connection.
pub fn spawn_car_signature(client_entity: Entity) -> Signature {
    Signature::of::<LocalCar>().for_client(client_entity)
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

/// Registers everything that must be identical between client and server:
/// which components replicate (and how often), and the client -> server /
/// server -> client event channels. Both binaries call this exact function
/// on startup so registration order — which determines wire IDs — can never
/// drift between them.
pub fn register_protocol(app: &mut App) {
    app.replicate_once::<CarChassis>()
        .replicate::<CarSnapshot>()
        .add_client_event::<CarInputMsg>(Channel::Unreliable)
        .add_client_event::<CarResetMsg>(Channel::Ordered)
        .add_client_event::<RegenRequestMsg>(Channel::Ordered)
        .add_server_event::<WorldRegenMsg>(Channel::Ordered)
        // WorldRegenMsg carries no entity/component references, so it's
        // safe (and necessary) to deliver even to a client whose
        // replication handshake isn't fully established yet — this is what
        // lets the server catch a newly-connecting client up on a regen
        // that already happened (see car_sim.rs's spawn_car_on_connect),
        // sent in the same observer that spawns their car. Without this,
        // replicon silently drops that catch-up message.
        .make_event_independent::<WorldRegenMsg>();
}
