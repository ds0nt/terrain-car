use bevy::prelude::*;
use bevy_replicon::prelude::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::buildings::BuildingKind;
use crate::car_physics::CarChassis;
use crate::combat::Health;
use crate::tank_physics::TankChassis;

/// Client-only marker: tags whichever replicated car entity belongs to the
/// local player, once its `CarChassis::owner_player_id` can be compared
/// against the local account id (see `client::car::tag_local_car`) — same
/// reactive "replicate first as an ordinary entity, then tag after the
/// fact by comparing owner id" shape `LocalPlane`'s own `tag_local_plane`
/// already uses.
///
/// *Not* a client-side-predicted pre-spawn the way this used to work: a car
/// is no longer a free, guaranteed-to-exist thing every connecting client
/// could safely guess about upfront (spawn one locally, let bevy_replicon's
/// `Signature` mechanism merge the server's authoritative echo into it) —
/// it only exists once its owner's `Hangar` actually completes, which can
/// happen while that owner isn't even connected. A car has no client-side
/// prediction at all anymore either, for the same reason a plane never
/// had any: purely server-authoritative, mirrored from `CarSnapshot` (see
/// that component's own docs on why).
#[derive(Component, Clone, Copy)]
pub struct LocalCar;

/// Sent client -> server every `FixedUpdate` tick with the local player's
/// current input, while actively driving. No sequence number and no
/// client-side prediction/reconciliation at all anymore (see
/// `CarSnapshot`'s docs) — same "each message fully supersedes the last,
/// server is sole authority" shape `PlaneInputMsg` already used.
///
/// `car_id` names exactly which owned car this drives — a multi-car owner
/// shares one `owner_player_id` across all of them, so without this the
/// server has no way to tell which specific one you're sitting in and
/// (before this field existed) applied every input to all of them at
/// once, live-reported as "when I join a car it should be the correct
/// car." See `CarChassis::car_id`'s own docs and `client::pilot`'s
/// `DrivingCarId`.
///
/// `Unreliable`: a dropped one is harmless and paying for retransmission
/// would only add latency for no benefit.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct CarInputMsg {
    pub car_id: Uuid,
    pub throttle: f32,
    pub steer: f32,
    pub brake: bool,
    pub boost: bool,
}

/// Replicated per spawned Scout Plane — see `BuildingKind::AirFactory`'s
/// docs (one spawns, already owned by the factory's builder, the moment
/// it completes). Server-authoritative only: no client-side prediction for
/// a plane you're piloting — every plane, including your own, is just
/// driven by this replicated snapshot (see
/// `client::car_render::sync_car_transforms`, which now handles a car
/// exactly the same way — see `CarSnapshot`'s own docs). That means a
/// little extra input-to-visible-movement latency while flying compared to
/// the old locally-predicted driving feel, an acceptable v1 tradeoff for
/// not needing a full prediction/reconciliation pipeline that a
/// multi-owned vehicle can't cleanly support anyway (see `CarSnapshot`'s
/// docs on why that pipeline was removed from cars too).
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct PlaneSnapshot {
    pub owner_player_id: Uuid,
    /// Unique per plane, unlike `owner_player_id` which every plane a
    /// player owns shares — same purpose as `CarChassis::car_id`: the
    /// actual key `PlaneInputMsg`/`RecallPlaneMsg` target, so flying or
    /// recalling one specific owned plane doesn't affect every other one
    /// a multi-plane owner has parked elsewhere. Generated once at spawn
    /// and never changed.
    pub plane_id: Uuid,
    pub true_x: f64,
    pub true_z: f64,
    pub altitude: f32,
    pub rotation: Quat,
    /// Carried alongside position so a client can show real speed/altitude-
    /// rate HUD readouts for whichever vehicle it's actually occupying
    /// (see `client::pilot::PlayerFocus`) without guessing one from frame-
    /// to-frame position deltas.
    pub linear_velocity: Vec3,
    /// True-space position this plane recalls to on `RecallPlaneMsg` (`H`)
    /// — the Air Factory that spawned it, set once at spawn and never
    /// changed.
    pub home_true_x: f64,
    pub home_true_z: f64,
}

/// Sent client -> server every `FixedUpdate` tick while the sender is
/// piloting their own plane (see client's `aircraft.rs`) — same "each
/// message fully supersedes the last, nothing here ever reconciles/replays"
/// shape `CarInputMsg` uses (see `PlaneSnapshot`'s/`CarSnapshot`'s docs).
///
/// `plane_id` names exactly which owned plane this flies — see
/// `PlaneSnapshot::plane_id`'s docs and `CarInputMsg::car_id`'s (the
/// identical fix for cars): without it, a multi-plane owner's input
/// applied to every owned plane at once, reported live as flying one
/// appearing to move all of them.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct PlaneInputMsg {
    pub plane_id: Uuid,
    /// Forward/back thrust along the plane's own current facing.
    pub throttle: f32,
    /// Rudder — turn rate around the plane's own local up axis.
    pub yaw: f32,
    /// Elevator — rotation rate around the plane's own local right axis
    /// (nose up/down). Climbing/descending comes from thrust following the
    /// tilted nose, same as a real aircraft, not a separate direct
    /// vertical control — see `server::aircraft::fly_planes`.
    pub pitch: f32,
    /// Ailerons — rotation rate around the plane's own local forward axis
    /// (banking left/right).
    pub roll: f32,
}

/// Sent client -> server every `FixedUpdate` tick, unconditionally,
/// regardless of `ControlMode` — the server-side counterpart to
/// `client::pilot::PlayerFocus`: "wherever the player actually is right
/// now," car, plane, or on foot alike. Exists because distance-bound
/// server actions (`PlaceBuildingMsg`'s "close enough to place this")
/// otherwise have no way to know that at all while on foot — the on-foot
/// avatar is client-local only (see `pilot.rs`'s own docs), never
/// replicated, so unlike a car or plane the server has *no* independent
/// position for it whatsoever without this. Using the parked vehicle's
/// position instead (the previous approach) actively broke placement the
/// moment you stepped away from it — reported live as "I can't build when
/// I exit my plane." True-space, not local — same reasoning every other
/// position-bearing message here already uses.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct PlayerPositionMsg {
    pub true_x: f64,
    pub true_z: f64,
    /// Facing, in the same "atan2(x, z)" bearing convention every other
    /// facing calculation in this game already uses (see
    /// `building_placement.rs`'s own docs) — only actually meaningful
    /// while `on_foot` (a car/plane's own facing already replicates via
    /// `CarSnapshot`/`PlaneSnapshot`), but always sent so this message's
    /// shape doesn't need to change if that ever stops being true.
    pub rotation_y: f32,
    /// Whether the sender is currently on foot (`ControlMode::OnFoot`) —
    /// the server mirrors this straight onto `PlayerOnFootSnapshot` so
    /// every other client can tell whether to actually render this
    /// player's on-foot avatar or not: this message keeps arriving
    /// unconditionally regardless of mode (see this struct's own docs),
    /// so without an explicit flag every other client would have no way
    /// to tell "on foot, not moving" from "driving, and this position is
    /// stale" and might render a duplicate avatar overlapping a car.
    pub on_foot: bool,
}

/// Sent client -> server when the player presses R to unstick their car —
/// the server does the actual work (find flat ground, reposition, zero
/// velocity) and the client just waits for the result to replicate back,
/// same "ask, don't locally guess" shape every car message uses now (see
/// `CarSnapshot`'s docs). Carries the search center in true space so the
/// server can bias `find_flat_spawn` near wherever the player actually is.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct CarResetMsg {
    pub near_true_x: f64,
    pub near_true_z: f64,
}

/// Replicated snapshot of a car's authoritative physical state — a plain
/// data component rather than Rapier's own `Transform`/`Velocity` types, so
/// `shared` (and the headless server) never need to depend on bevy_rapier3d
/// or rendering machinery just to describe "where is this car and how fast
/// is it moving."
///
/// No client-side prediction/reconciliation reads this anymore (a car used
/// to carry a `last_input_sequence`/`reset_generation` pair purely to
/// support that, both removed along with it) — a car is purely server-
/// authoritative now, driven only by this snapshot, exactly like
/// `PlaneSnapshot` always was. That wasn't just a simplification: a
/// player can own several cars (full parity with owning several planes),
/// and client-side prediction/reconciliation fundamentally assumes there's
/// exactly one locally-predicted body to reconcile against — every extra
/// owned car would either need its own independent prediction state (a
/// real multi-body reconciliation system this project never needed for
/// planes) or silently not be reconciled at all. Dropping prediction
/// entirely sidesteps that instead of half-solving it.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default)]
pub struct CarSnapshot {
    pub translation: Vec3,
    pub rotation: Quat,
    pub linear_velocity: Vec3,
    pub angular_velocity: Vec3,
    /// True-space position this car recalls to on `RecallToHangarMsg` (`H`)
    /// — the Hangar that spawned it, set once at spawn and never changed,
    /// same "remembers its own home" shape `PlaneSnapshot`'s
    /// `home_true_x`/`home_true_z` use for a plane's Air Factory.
    pub home_true_x: f64,
    pub home_true_z: f64,
    /// Who's riding along as a passenger right now, if anyone — `None`
    /// means the seat is empty. Set/cleared by
    /// `server::car_sim::apply_board_passenger`/`apply_exit_passenger` in
    /// response to `BoardPassengerMsg`/`ExitPassengerMsg`, *not*
    /// recomputed every physics tick the way `translation`/`rotation` are
    /// (see `server::car_sim`'s per-tick sync, which only ever touches
    /// those four fields) — a passenger has no physics of their own to
    /// derive this from, it's pure seat-occupancy state. Deliberately not
    /// restricted to the car's own owner: riding along in a friend's car
    /// is the whole point.
    pub passenger_player_id: Option<Uuid>,
}

/// Sent client -> server when the pilot of `plane_id` exits it (`E`) —
/// separate from the zeroed `PlaneInputMsg` also sent on exit
/// (`client::pilot`'s `handle_vehicle_key`), which only stops *future*
/// thrust; a plane has no gravity to eventually settle it and only modest
/// linear damping (see `PLANE_LINEAR_DAMPING`), so without this a plane
/// exited mid-glide would keep coasting under leftover momentum for a
/// while rather than actually stopping — reported live as "once you exit
/// the plane it needs to have its engines cut." The server-side handler
/// (`server::aircraft`'s `apply_exit_plane`) zeroes `Velocity` and
/// `ExternalForce` directly, an immediate hard stop rather than a fast
/// damped decay.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct ExitPlaneMsg {
    pub plane_id: Uuid,
}

/// Sent client -> server when a player on foot boards a car as a
/// passenger (`E`, near a car whose `CarSnapshot::passenger_player_id` is
/// currently `None` — see `client::pilot`'s `handle_vehicle_key`). Unlike
/// `CarInputMsg`, there's no ownership check: any car with an empty seat
/// can be ridden, driven by its owner or not.
///
/// `Ordered`, not `Unreliable` like `CarInputMsg` — this is a one-shot
/// state transition (empty seat -> occupied), not a continuously-repeated
/// value where a dropped packet is instantly superseded by the next one;
/// losing it silently would leave the passenger's own client thinking
/// they boarded while the server never learned the seat was taken.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct BoardPassengerMsg {
    pub car_id: Uuid,
}

/// Sent client -> server when the current passenger of `car_id` presses
/// `E` again to get out — see `BoardPassengerMsg`'s docs on why this is
/// `Ordered` for the same reason.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct ExitPassengerMsg {
    pub car_id: Uuid,
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
    /// True-space spot the server picked for this login (near other
    /// connected players, or the starter villager it just spawned nearby —
    /// see `server::car_sim::pick_spawn_point`) — meaningless when `ok` is
    /// `false`. The player now starts on foot with no car to predict a
    /// position from, so the client needs to be *told* where "here" is
    /// rather than guessing it locally (see `client::pilot`'s
    /// `spawn_pilot_after_login`).
    pub spawn_true_x: f64,
    pub spawn_true_z: f64,
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

/// Mirrors the sender's own server-side villager build queue
/// (`server::villagers`'s `VillagerQueues`) onto their car — same "server
/// holds the real authoritative state, a component on the car is just how
/// it reaches clients" relationship `Wallet` already has, and for the same
/// reason: it changes over time (queueing, and every time a queued
/// villager actually finishes spawning) and every client watching this
/// player's Land Factory panel needs to see that.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default)]
pub struct VillagerQueue {
    pub queued: u32,
}

/// Sent client -> server when the player queues one more villager build at
/// one of their own Land Factories (see client's `building_ui.rs`, shown
/// for a selected, owned `LandFactory`). Carries no specific building:
/// like `RecallToHangarMsg`, the server already knows whose queue this is,
/// and every one of a player's completed Land Factories draws from that
/// same shared queue (see `server::villagers`) — there's nothing else to
/// identify.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct QueueVillagerMsg;

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

/// Sent client -> server to destroy one of the sender's own buildings —
/// see client's `building_ui.rs` (the trash-icon button on a selected,
/// owned building, requiring a second confirming click). `building_id` is
/// `BuildingSnapshot::id`, not the replicated `Entity` — see that field's
/// own docs on why. The server is the sole authority on whether the
/// sender actually owns it; a client can *ask* to destroy any id, but
/// only ever succeeds against its own.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct DestroyBuildingMsg {
    pub building_id: Uuid,
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
    /// Stable identity, independent of the per-client/per-server `Entity`
    /// replication assigns — the same id `server::persistence` already
    /// generates for this building's DB row, just also carried on the
    /// live entity now so a client can name *this specific building* in a
    /// message back to the server (`DestroyBuildingMsg`) without needing
    /// entity-id network mapping.
    pub id: Uuid,
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

/// Sent client -> server: teleport the sender's currently-driven car
/// (`car_id`, see `CarChassis::car_id`'s docs) to whichever owned `Hangar`
/// is nearest right now — `H`, same key `RecallPlaneMsg` uses for a plane.
/// Deliberately *not* "back to `CarSnapshot::home_true_x`/`home_true_z`"
/// (the original design, and still the fallback if every owned Hangar has
/// since been destroyed): reported live as wanting the *nearest* Hangar,
/// not necessarily the specific one this car happened to spawn from,
/// which may by now be far away, or gone.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct RecallToHangarMsg {
    pub car_id: Uuid,
}

/// Sent client -> server: teleport the sender's currently-flown plane
/// (`plane_id`, see `PlaneSnapshot::plane_id`'s docs) back to wherever it
/// spawned (`PlaneSnapshot::home_true_x`/`home_true_z`) — `H`, same key
/// `RecallToHangarMsg` uses for a car. Needs `plane_id` for the same
/// reason `RecallToHangarMsg` needs `car_id`: without it, recall moved
/// every owned plane at once instead of just the one being flown.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct RecallPlaneMsg {
    pub plane_id: Uuid,
}

/// Sent client -> server: `H` while on foot (see client's
/// `building_ui.rs`) — recalls the *player themself*, not a car or plane
/// (`RecallToHangarMsg`/`RecallPlaneMsg` are for those), to whichever
/// owned Hangar, Land Factory, or Air Factory is nearest right now — any
/// of the three, unlike the car/plane recalls, which only ever look at
/// their own matching kind of building. No payload: the server already
/// knows who's asking and where they currently are
/// (`server::car_sim::PlayerPositions`).
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct RecallPlayerMsg;

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

/// Mirrors the sender's own `PlayerPositionMsg` onto their account entity
/// (the same one `PlayerInfo`/`Wallet` already live on) purely so it
/// replicates — the on-foot avatar itself is still client-local-only for
/// *the player controlling it* (see `client::pilot`'s top-level docs on
/// why: no server-side physics for it, just an authoritative position),
/// but every *other* client can now render a stand-in for it, the same
/// "server holds the real state, this component is just how it reaches
/// clients" relationship `Wallet`/`CarCosmetics` already have. Before this
/// existed, a player walking around on foot was invisible to everyone else
/// in every sense — no rendered avatar, no minimap dot, no
/// `player_markers.rs` nametag — since there was nothing at all replicated
/// about on-foot state; only a parked vehicle was ever visible.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default)]
pub struct PlayerOnFootSnapshot {
    pub true_x: f64,
    pub true_z: f64,
    pub rotation_y: f32,
    /// `false` whenever the sender is actually driving/flying/riding —
    /// see `PlayerPositionMsg::on_foot`'s own docs on why this has to be
    /// explicit rather than inferred. Every renderer of this component
    /// (`client::remote_players`, `player_markers.rs`, `minimap.rs`) skips
    /// entirely whenever this is `false`, rather than trying to render a
    /// stale/meaningless position.
    pub on_foot: bool,
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

/// Sent client -> server to redirect an AI-controlled car's patrol point
/// (right-click while it's selected — see client's `selection.rs`).
/// `car_id` is `CarChassis::car_id`, matched the same way every other
/// per-vehicle message here already is; the server is the sole authority
/// on whether that car is actually AI-controlled at all
/// (`server::ai::AI_OWNER`) — naming a real player's own car here simply
/// matches nothing, the identical "visible via replication but harmless to
/// name" reasoning `CarInputMsg`'s own docs already cover.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct SetCarPatrolMsg {
    pub car_id: Uuid,
    pub target_true_x: f64,
    pub target_true_z: f64,
}

/// Same as `SetCarPatrolMsg`, for an AI-controlled plane.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct SetPlanePatrolMsg {
    pub plane_id: Uuid,
    pub target_true_x: f64,
    pub target_true_z: f64,
}

/// Sent client -> server whenever the player sends a chat line (Enter, see
/// client's `chat.rs`) — carries the raw typed text. The server is the sole
/// authority on what happens next: an ordinary line gets broadcast back out
/// as a `ChatBroadcastMsg` to everyone, while a `/`-prefixed line is parsed
/// as a command (`/tp`, `/respawn` — see `server::chat`) and never
/// broadcast at all, same "client sends intent, server decides" trust
/// boundary every other message here already enforces. `Ordered`: chat
/// lines must never reorder or silently drop the way an `Unreliable`
/// message could.
#[derive(Event, Serialize, Deserialize, Clone, Debug)]
pub struct ChatMsg {
    pub text: String,
}

/// Sent server -> clients whenever a chat line should be shown — either
/// broadcast to everyone (an ordinary message) or privately to a single
/// client (a command's own feedback/errors, or a heads-up that someone
/// teleported you — see `server::chat`). `player_id` is `None` for a
/// server-originated system line, which the client renders distinctly (a
/// "server" label, no owner color) rather than trying to look up a
/// nonexistent player for it.
#[derive(Event, Serialize, Deserialize, Clone, Debug)]
pub struct ChatBroadcastMsg {
    pub player_id: Option<Uuid>,
    pub username: String,
    pub text: String,
}

/// Sent server -> a single client to force-reposition whatever on-foot
/// avatar they currently have — the on-foot half of `/tp`/`/respawn` (see
/// `server::chat`). Cars and planes need no such message at all: both are
/// already fully server-authoritative (`CarSnapshot`/`PlaneSnapshot`), so
/// `server::chat` simply writes their `Transform`/`Velocity` directly the
/// same way `apply_car_reset`/`apply_recall_plane` already do, for every
/// car/plane the teleported player owns. This message exists purely to
/// also relocate the on-foot avatar, which (unlike a car or plane) has no
/// server-side entity of its own at all — see `PlayerPositionMsg`'s docs on
/// why. Harmless no-op if the receiving client has no on-foot avatar to
/// move right now (driving/flying/riding instead) — see client's
/// `chat::apply_teleport`, which just does nothing in that case; whatever
/// they're occupying was already moved directly, and they'll see this same
/// position reflected whenever they later step out of it.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct TeleportMsg {
    pub true_x: f64,
    pub true_z: f64,
}

/// Replicated snapshot of a tank's authoritative physical state — same
/// "plain data, no Rapier types, purely server-authoritative, no client
/// prediction" shape `CarSnapshot` uses and for the identical reason (an
/// owner can have more than one). `turret_yaw` lives here, not on
/// `TankChassis`, because it changes continuously (every tick the driver is
/// aiming) while `TankChassis` is `replicate_once` — the same "changes over
/// time -> Snapshot, fixed at spawn -> Chassis" split `CarSnapshot`/
/// `CarChassis` already follow.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default)]
pub struct TankSnapshot {
    pub translation: Vec3,
    pub rotation: Quat,
    pub linear_velocity: Vec3,
    pub angular_velocity: Vec3,
    /// World-space yaw (radians) the turret is currently facing —
    /// independent of the hull's own heading. See
    /// `shared::tank_physics::TankInput::turret_yaw`'s docs on how this
    /// gets set.
    pub turret_yaw: f32,
    /// True-space position this tank recalls to on `RecallTankMsg` (`H`) —
    /// the `WarFactory` that spawned it, same shape as `CarSnapshot::
    /// home_true_x`/`home_true_z`.
    pub home_true_x: f64,
    pub home_true_z: f64,
}

/// Sent client -> server every `FixedUpdate` tick while the sender is
/// driving their own tank — same "each message fully supersedes the last,
/// nothing here ever reconciles/replays" shape `CarInputMsg` uses.
/// `tank_id` names exactly which owned tank this drives, same reason
/// `CarInputMsg::car_id` exists. `Unreliable`: a dropped one is harmless,
/// the next tick's message fully supersedes it.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct TankInputMsg {
    pub tank_id: Uuid,
    pub throttle: f32,
    pub steer: f32,
    pub brake: bool,
    /// Desired world-space turret yaw, mirrored straight onto
    /// `TankSnapshot::turret_yaw` — see `shared::tank_physics::TankInput`'s
    /// own docs.
    pub turret_yaw: f32,
}

/// Sent client -> server when the driver presses R to right a flipped tank
/// — same "correct orientation, drop back onto the ground at its own
/// current position" shape `FlipUprightMsg` gives a car, just carrying
/// `tank_id` explicitly rather than relying on an implicit "whichever
/// vehicle this player is in" lookup, the same explicit-id lesson
/// `CarInputMsg::car_id`'s own docs describe learning the hard way.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct TankFlipUprightMsg {
    pub tank_id: Uuid,
    pub true_x: f64,
    pub true_z: f64,
}

/// Sent client -> server: teleport the sender's currently-driven tank back
/// to whichever owned `WarFactory` is nearest right now — `H`, same shape
/// as `RecallToHangarMsg`/`RecallPlaneMsg`.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct RecallTankMsg {
    pub tank_id: Uuid,
}

/// Replicated snapshot of a dropship's authoritative physical state — flies
/// exactly like a `PlaneSnapshot` (no client prediction, purely server-
/// authoritative), but carries four independent passenger seats instead of
/// a car's single `passenger_player_id`. The pilot seat isn't tracked here
/// at all — piloting works exactly like a car/plane (client-local
/// `ControlMode` + ownership check on `DropshipInputMsg`), so there's
/// nothing server-side to record beyond `owner_player_id` itself.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct DropshipSnapshot {
    pub owner_player_id: Uuid,
    pub dropship_id: Uuid,
    pub translation: Vec3,
    pub rotation: Quat,
    pub linear_velocity: Vec3,
    pub home_true_x: f64,
    pub home_true_z: f64,
    /// Four passenger seats, independent of who (if anyone) is piloting —
    /// `None` means empty. Set/cleared by `server::dropship_sim`'s
    /// `apply_board_dropship`/`apply_exit_dropship_passenger`, never
    /// recomputed from physics — same "pure seat-occupancy state" shape
    /// `CarSnapshot::passenger_player_id` uses, just four of them. Not
    /// restricted to the dropship's own owner: riding along is the whole
    /// point, same as a car's passenger seat.
    pub passenger_player_ids: [Option<Uuid>; 4],
    /// Cargo currently slung underneath — up to `CARGO_SLOTS` (2) cars/
    /// tanks at once. `None` means empty. Set/cleared by
    /// `server::dropship_sim`'s `apply_pickup_vehicle`/`apply_drop_vehicle`,
    /// same "pure occupancy state, not recomputed from physics" shape
    /// `passenger_player_ids` already uses. Not restricted to the
    /// dropship's own owner, and the carried vehicle needn't be owned by
    /// anyone in particular either — a dropship can scoop up any car or
    /// tank sitting on the ground nearby, same "no ownership check"
    /// tolerance boarding a passenger seat already has.
    pub cargo: [Option<CargoVehicleId>; CARGO_SLOTS],
}

/// How many vehicles a dropship can carry at once — a small, fixed cap
/// rather than anything scaling with the craft, per the explicit "maximum
/// 2? for now" scope this shipped with.
pub const CARGO_SLOTS: usize = 2;

/// Identifies one carried vehicle by kind + id — a dropship's cargo can be
/// a mix of cars and tanks, and the two have separate id spaces
/// (`CarChassis::car_id` vs `TankChassis::tank_id`), so a bare `Uuid`
/// alone can't say which kind of entity to look up.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum CargoVehicleId {
    Car(Uuid),
    Tank(Uuid),
}

/// Sent client -> server: `G` while piloting a dropship, near an eligible
/// car/tank on the ground with a free cargo slot available — the sender
/// picks which one (the nearest in range, same "client names the specific
/// target" shape `EnterTurretMsg` uses), the server is the sole authority
/// on whether it's actually close enough and a slot is actually free.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct PickupVehicleMsg {
    pub dropship_id: Uuid,
    pub target: CargoVehicleId,
}

/// Sent client -> server: `G` while piloting a dropship that's currently
/// carrying something and has nothing new in range to pick up instead —
/// releases the named cargo (always the most recently picked up one, from
/// the client's own point of view — see `dropship::handle_pickup_key`).
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct DropVehicleMsg {
    pub dropship_id: Uuid,
    pub target: CargoVehicleId,
}

/// Sent client -> server every `FixedUpdate` tick while the sender is
/// piloting their own dropship — identical shape to `PlaneInputMsg` (see
/// its own field docs for throttle/yaw/pitch/roll), just for `dropship_id`
/// instead of `plane_id`.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct DropshipInputMsg {
    pub dropship_id: Uuid,
    pub throttle: f32,
    pub yaw: f32,
    pub pitch: f32,
    pub roll: f32,
}

/// Sent client -> server when the pilot of `dropship_id` exits it — same
/// "cut engines, let gravity take over" shape `ExitPlaneMsg` gives a plane
/// (see that message's own docs); any passengers still aboard fall with it,
/// same as they would if the pilot simply stopped flying it well.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct ExitDropshipMsg {
    pub dropship_id: Uuid,
}

/// Sent client -> server when a player on foot boards a dropship as a
/// passenger — same "no ownership check, any craft with an empty seat can
/// be ridden" shape `BoardPassengerMsg` gives a car. The server assigns the
/// sender to the first empty slot in `DropshipSnapshot::
/// passenger_player_ids`; the client never picks a specific seat index.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct BoardDropshipMsg {
    pub dropship_id: Uuid,
}

/// Sent client -> server when a current passenger of `dropship_id` gets
/// out — the server clears whichever of the four slots currently holds the
/// sender's own player id (no seat index needed client-side at all, unlike
/// boarding there's exactly one slot that can possibly match).
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct ExitDropshipPassengerMsg {
    pub dropship_id: Uuid,
}

/// Sent client -> server: teleport the sender's currently-piloted dropship
/// back to whichever owned `Dropyard` is nearest right now — `H`, same
/// shape as `RecallPlaneMsg`.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct RecallDropshipMsg {
    pub dropship_id: Uuid,
}

/// Replicated aim state for a placed `BuildingKind::Turret` — kept as its
/// own component (not folded into `BuildingSnapshot`) for the same reason
/// `TankSnapshot::turret_yaw` isn't on `TankChassis`: `BuildingSnapshot` is
/// effectively fixed after placement (only `build_complete_at` changes on
/// its own timer), while a turret's aim rotates continuously as
/// `server::turrets` tracks whatever it's currently targeting. Every
/// client renders the same rotating head from this, not just whichever
/// client happens to be nearby.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default)]
pub struct TurretSnapshot {
    pub aim_yaw: f32,
    /// Elevation, radians — `0.0` is level (aiming along the horizon),
    /// positive tilts the barrel up. Clamped server-side to
    /// `server::turrets::AIM_PITCH_RANGE`, a plausible mount elevation
    /// range rather than a full sphere.
    pub aim_pitch: f32,
    /// Who's currently manually operating this turret, if anyone — `None`
    /// means `server::turrets`'s own auto-aim/fire is in control (see that
    /// module's own docs). Set/cleared by `apply_enter_turret`/
    /// `apply_exit_turret`, same "pure seat-occupancy state" shape
    /// `CarSnapshot::passenger_player_id` uses. Only the turret's own owner
    /// can ever occupy it — unlike a car's passenger seat, this isn't
    /// "anyone can ride along," it's "you can personally take over aiming
    /// your own defense turret."
    pub occupant_player_id: Option<Uuid>,
}

/// Sent client -> server: `F` while on foot near an owned, unoccupied
/// `Turret` — takes manual control of its aim/fire away from
/// `server::turrets`'s own auto-targeting. Rejected (silently, same
/// tolerance every other boarding message here has for a stale/impossible
/// request) if the sender doesn't own it or it's already occupied.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct EnterTurretMsg {
    pub building_id: Uuid,
}

/// Sent client -> server: `F` while manually operating a turret — hands
/// control back to `server::turrets`'s own auto-aim.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct ExitTurretMsg {
    pub building_id: Uuid,
}

/// Sent client -> server every `FixedUpdate` tick while manually operating
/// a turret — the operator's desired aim yaw, mirrored straight onto
/// `TurretSnapshot::aim_yaw` with no server-side turn-rate limit (a human
/// operator aiming with the mouse is exactly as instant as a tank driver's
/// own `TankInputMsg::turret_yaw` — only the *unmanned* auto-aim slews at
/// `server::turrets::TURRET_TURN_RATE`). `Unreliable`, same reasoning every
/// other continuously-repeated input message here uses.
#[derive(Event, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct TurretAimMsg {
    pub building_id: Uuid,
    pub aim_yaw: f32,
    pub aim_pitch: f32,
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
        .replicate::<PlaneSnapshot>()
        .add_client_event::<PlaneInputMsg>(Channel::Unreliable)
        .add_client_event::<PlayerPositionMsg>(Channel::Unreliable)
        .add_client_event::<CarResetMsg>(Channel::Ordered)
        .add_client_event::<FlipUprightMsg>(Channel::Ordered)
        .add_client_event::<BoardPassengerMsg>(Channel::Ordered)
        .add_client_event::<ExitPassengerMsg>(Channel::Ordered)
        .add_client_event::<ExitPlaneMsg>(Channel::Ordered)
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
        .replicate::<VillagerQueue>()
        .add_client_event::<PlaceBuildingMsg>(Channel::Ordered)
        .add_client_event::<DestroyBuildingMsg>(Channel::Ordered)
        .add_client_event::<RecallToHangarMsg>(Channel::Ordered)
        .add_client_event::<RecallPlaneMsg>(Channel::Ordered)
        .add_client_event::<RecallPlayerMsg>(Channel::Ordered)
        .add_client_event::<QueueVillagerMsg>(Channel::Ordered)
        .replicate::<PlayerInfo>()
        .replicate::<PlayerOnFootSnapshot>()
        .add_client_event::<PingMsg>(Channel::Unreliable)
        .add_server_event::<PingBroadcastMsg>(Channel::Unreliable)
        .add_client_event::<SetCarPatrolMsg>(Channel::Ordered)
        .add_client_event::<SetPlanePatrolMsg>(Channel::Ordered)
        .add_client_event::<ChatMsg>(Channel::Ordered)
        .add_server_event::<ChatBroadcastMsg>(Channel::Ordered)
        .add_server_event::<TeleportMsg>(Channel::Ordered)
        // Tank — same replication/channel shape as its car equivalents,
        // see each type's own docs.
        .replicate_once::<TankChassis>()
        .replicate::<TankSnapshot>()
        .add_client_event::<TankInputMsg>(Channel::Unreliable)
        .add_client_event::<TankFlipUprightMsg>(Channel::Ordered)
        .add_client_event::<RecallTankMsg>(Channel::Ordered)
        // Dropship — same replication/channel shape as its plane/car
        // equivalents, see each type's own docs.
        .replicate::<DropshipSnapshot>()
        .add_client_event::<DropshipInputMsg>(Channel::Unreliable)
        .add_client_event::<ExitDropshipMsg>(Channel::Ordered)
        .add_client_event::<BoardDropshipMsg>(Channel::Ordered)
        .add_client_event::<ExitDropshipPassengerMsg>(Channel::Ordered)
        .add_client_event::<RecallDropshipMsg>(Channel::Ordered)
        .add_client_event::<PickupVehicleMsg>(Channel::Ordered)
        .add_client_event::<DropVehicleMsg>(Channel::Ordered)
        // Turret defense building.
        .replicate::<TurretSnapshot>()
        .add_client_event::<EnterTurretMsg>(Channel::Ordered)
        .add_client_event::<ExitTurretMsg>(Channel::Ordered)
        .add_client_event::<TurretAimMsg>(Channel::Unreliable)
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
