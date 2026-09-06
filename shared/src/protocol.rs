use bevy::prelude::*;
use bevy_replicon::prelude::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::buildings::BuildingKind;
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

/// Sent client -> server when the player presses R to right a flipped car
/// — deliberately separate from `CarResetMsg` (world-regen's "find me
/// flat ground somewhere near here" reset): R no longer searches for
/// anywhere else to go, it just corrects orientation and drops the car
/// back onto the ground at its *own current* position. Carries that exact
/// point (true-space) so the server independently recomputes the same
/// ground height rather than trusting a client-supplied Y.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct FlipUprightMsg {
    pub true_x: f64,
    pub true_z: f64,
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

/// Sent client -> server to create a new account. The server is the sole
/// authority on both fields (`shared::auth`'s validators are run on both
/// sides — client-side only for instant UI feedback, server-side because
/// nothing client-supplied is ever trusted); on success the server
/// generates a fresh `Uuid` for this account, hashes the password, and
/// replies with `AuthResultMsg`. Replaces the old `IdentifyMsg`, which let
/// any client simply *claim* a `player_id` with zero verification.
#[derive(Event, Serialize, Deserialize, Clone, Debug)]
pub struct RegisterMsg {
    pub username: String,
    pub password: String,
}

/// Sent client -> server to log into an existing account. Wrong username
/// and wrong password both map to the same `AuthResultMsg` failure
/// (`InvalidCredentials` — see `server::persistence::AuthOutcome`) so a
/// failed attempt never reveals whether the username itself exists.
#[derive(Event, Serialize, Deserialize, Clone, Debug)]
pub struct LoginMsg {
    pub username: String,
    pub password: String,
}

/// Sent server -> the requesting client only (never broadcast — this is
/// private to whoever sent the `RegisterMsg`/`LoginMsg`), in response to
/// either. `player_id` is `Some` only when `ok` is true; the client stores
/// it as its identity for the rest of this session — nothing is generated
/// or trusted client-side anymore. `message` is a short human-readable
/// reason on failure ("username taken", "invalid credentials", ...) shown
/// directly in the login UI.
#[derive(Event, Serialize, Deserialize, Clone, Debug)]
pub struct AuthResultMsg {
    pub ok: bool,
    pub player_id: Option<Uuid>,
    pub message: String,
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

/// A mirror of the sender's own server-side wallet (`server::economy::
/// Wallets`, keyed by the durable `PersistentPlayerId`), attached to their
/// car entity purely so it replicates and the HUD can show it — same
/// "server holds the real authoritative state, a component on the car is
/// just how it reaches clients" relationship `Health` already has. Kept as
/// its own component (not folded into `Health` or `CarChassis`) since it's
/// conceptually per-*player*, not per-car; it happens to ride on the car
/// entity today only because that's the one entity every client already
/// has a replicated handle to.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default)]
pub struct Wallet {
    pub energy: f32,
    pub ore: f32,
}

/// Purely cosmetic per-car choices — kept as its own component (not folded
/// into `CarChassis`) because `CarChassis` is `replicate_once` (nothing
/// mutates a car's tuning after spawn now that client-side tuning is
/// gone), while these genuinely change mid-session whenever a player picks
/// a new color or toggles the bow, and need every other client to see
/// that change, not just the value at spawn — the same "replicated,
/// changes over time" shape `Wallet` already has.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default)]
pub struct CarCosmetics {
    /// Overrides `CarChassis::color_seed`'s automatic owner-hash color
    /// when set — `None` means "just use the automatic color," which is
    /// also every car's starting state.
    pub custom_color: Option<[f32; 3]>,
    pub has_bow: bool,
}

/// Sent client -> server whenever the player changes their own cosmetics
/// (see client's `cosmetics_ui.rs`) — carries the full desired state, not
/// a delta, same reasoning `TuneCarMsg` used to. The server writes it
/// straight onto the sender's own car (found via `OwnedBy`, same
/// authorization shape `FlipUprightMsg`'s handler already uses) with no
/// further validation: there's no cost, no balance implication, nothing
/// to cheat by picking a color, so there's nothing to check beyond "is
/// this genuinely your own car."
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct SetCosmeticsMsg {
    pub custom_color: Option<[f32; 3]>,
    pub has_bow: bool,
}

/// A player's own free resource-gathering helper — spawned automatically
/// the moment they're first identified, independent of any building, so
/// there's always *some* way to recover energy/ore even starting from a
/// wallet of zero (see `server::villagers`). "Glowing orb" is a
/// deliberate v1 placeholder visual (the user's own words) — purely
/// `client::villager_render`'s concern, nothing about this component
/// implies a particular look. Continuously replicated: `true_x`/`true_z`
/// update every server tick as it wanders, same as `CarSnapshot`.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct VillagerSnapshot {
    pub owner_player_id: Uuid,
    pub true_x: f64,
    pub true_z: f64,
}

/// Sent client -> server to place a building. `true_x`/`true_z` is where
/// the player wants it (their car's current position — see client's
/// `building_ui.rs`); the server is the sole authority on whether it's
/// actually affordable and legally placed (funds, and for
/// `ExtractionFacility`, proximity to a deposit — see
/// `shared::deposits::is_near_deposit`), same trust boundary every other
/// client -> server message in this game already enforces.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct PlaceBuildingMsg {
    pub kind: BuildingKind,
    pub true_x: f64,
    pub true_z: f64,
    /// The placing car's own yaw at the moment of placement — mainly
    /// meaningful for `Ramp` (which way it faces determines which way you
    /// drive up it), but carried for every kind so a future kind can use
    /// it too without another protocol change.
    pub rotation_y: f32,
}

/// Replicated per placed building — every client sees every player's
/// buildings, not just their own. `build_complete_at` is a real Unix-epoch
/// timestamp (seconds, matching `server::persistence`'s own choice of
/// representation and this project's existing wall-clock-time precedent —
/// see `shared::terrain_gen::random_seed`), not an elapsed-time-since-
/// startup value: client and server otherwise have no shared time origin
/// to compare against, since they don't start their processes at the same
/// moment. A client considers a building still under construction while
/// its own current Unix time is earlier than this.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct BuildingSnapshot {
    pub kind: BuildingKind,
    pub owner_player_id: Uuid,
    pub true_x: f64,
    pub true_z: f64,
    pub build_complete_at: f64,
    pub rotation_y: f32,
    /// The actual surface height the server placed this on — a downward
    /// raycast against its own authoritative physics world at placement
    /// time (`server::economy::surface_height_at`), which for a `Ramp`
    /// might be another `Ramp`'s own surface, not raw terrain. Carried
    /// here (rather than every client re-deriving it independently via
    /// `shared::terrain_gen::height_at`) so the rendered mesh can never
    /// visually disagree with the real collider the server actually
    /// placed — re-deriving from raw terrain height was exactly the bug
    /// that made a stacked Ramp appear to "snap" back down to ground
    /// level despite its physics staying correctly elevated.
    pub ground_y: f32,
}

/// Sent client -> server: teleport the sender's own car to their Hangar —
/// the simplified v1 Hangar behavior (see the base-building plan's scope
/// note on why this isn't a multi-car garage yet). No payload: like
/// `RegenRequestMsg`, the server already knows both who's asking and,
/// once it looks up their `Hangar`, where "their Hangar" actually is.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct RecallToHangarMsg;

/// Who owns this car, replicated alongside everything else on the same
/// entity — lets every client build a "who's currently connected" list
/// purely from ordinary car replication (see client's `players_ui.rs`)
/// instead of a separate roster broadcast with its own lifecycle to keep
/// in sync; a player's row naturally appears/disappears exactly when
/// their car does. `replicate_once`: a username never changes after
/// login.
#[derive(Component, Serialize, Deserialize, Clone, Debug)]
pub struct PlayerInfo {
    pub player_id: Uuid,
    pub username: String,
}

/// Sent client -> server to ping a location for every other player — no
/// payload, like `FireGunMsg`: the server already knows who's asking and
/// resolves their own car's current position as the ping location.
/// `Unreliable`: a dropped ping just means try again, nothing to
/// reconcile either way.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct PingMsg;

/// Sent server -> every client once a `PingMsg` resolves — carries the
/// pinging player's identity (so every client can render "who pinged" in
/// their pings list, not just where) and the true-space location it
/// resolved to.
#[derive(Event, Serialize, Deserialize, Clone, Debug)]
pub struct PingBroadcastMsg {
    pub player_id: Uuid,
    pub username: String,
    pub true_x: f64,
    pub true_z: f64,
}

/// Registers everything that must be identical between client and server:
/// which components replicate (and how often), and the client -> server /
/// server -> client event channels. Both binaries call this exact function
/// on startup so registration order — which determines wire IDs — can never
/// drift between them.
pub fn register_protocol(app: &mut App) {
    app
        // replicate_once: a car's tuning stats are fixed for its whole
        // life (default_chassis(), the same for every car — see that
        // function's docs on why client-side tuning was removed), so
        // there's nothing to update after the initial spawn.
        .replicate_once::<CarChassis>()
        .replicate::<CarSnapshot>()
        // Health changes constantly once guns exist — every client needs
        // to see it live, not just once at spawn.
        .replicate::<Health>()
        .add_client_event::<CarInputMsg>(Channel::Unreliable)
        .add_client_event::<CarResetMsg>(Channel::Ordered)
        .add_client_event::<FlipUprightMsg>(Channel::Ordered)
        .add_client_event::<RegenRequestMsg>(Channel::Ordered)
        .add_client_event::<FireGunMsg>(Channel::Unreliable)
        .add_server_event::<GunFiredMsg>(Channel::Unreliable)
        .add_client_event::<RegisterMsg>(Channel::Ordered)
        .add_client_event::<LoginMsg>(Channel::Ordered)
        .add_server_event::<AuthResultMsg>(Channel::Ordered)
        .replicate::<Wallet>()
        .replicate::<CarCosmetics>()
        .add_client_event::<SetCosmeticsMsg>(Channel::Ordered)
        .replicate::<BuildingSnapshot>()
        .replicate::<VillagerSnapshot>()
        .add_client_event::<PlaceBuildingMsg>(Channel::Ordered)
        .add_client_event::<RecallToHangarMsg>(Channel::Ordered)
        .replicate::<PlayerInfo>()
        .add_client_event::<PingMsg>(Channel::Unreliable)
        .add_server_event::<PingBroadcastMsg>(Channel::Unreliable)
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
