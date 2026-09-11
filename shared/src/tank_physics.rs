use bevy::prelude::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::car_physics::WheeledChassis;

/// A tank's hull is driven by the exact same wheel/suspension math a car
/// uses (`car_physics::compute_wheel_forces`/`wheel_mounts`) — nothing
/// about that model is car-specific, it's just "four downward-raycast
/// contact points with spring/damper/drive/traction," which a tracked
/// vehicle needs just as much as a wheeled one. What actually makes this a
/// tank rather than a reskinned car: heavier, slower, tankier tuning
/// (`default_tank_chassis`), and an independently-aimed turret
/// (`turret_yaw` — see `shared::protocol::TankSnapshot`'s own docs on why
/// that lives there instead of here).
#[derive(Component, Serialize, Deserialize, Clone, Copy)]
pub struct TankChassis {
    pub half_extents: Vec3,
    pub wheel_radius: f32,
    pub rest_length: f32,
    pub spring_stiffness: f32,
    pub damper: f32,
    pub max_steer_rad: f32,
    pub engine_force: f32,
    pub brake_force: f32,
    pub traction: f32,
    pub color_seed: u32,
    pub owner_player_id: Uuid,
    /// Unique per tank — the actual key `TankInputMsg`/`RecallTankMsg`
    /// target, same reason `CarChassis::car_id` exists.
    pub tank_id: Uuid,
}

/// Lets `car_physics::compute_wheel_forces` drive a tank's hull directly —
/// see that trait's own docs.
impl WheeledChassis for TankChassis {
    fn spring_stiffness(&self) -> f32 {
        self.spring_stiffness
    }
    fn damper(&self) -> f32 {
        self.damper
    }
    fn engine_force(&self) -> f32 {
        self.engine_force
    }
    fn brake_force(&self) -> f32 {
        self.brake_force
    }
    fn traction(&self) -> f32 {
        self.traction
    }
}

/// Continuous driver input for one tank — `throttle`/`steer`/`brake` drive
/// the hull exactly like `car_physics::CarInput`; `turret_yaw` is the
/// *desired* world-space turret facing (not a rate), sent fresh every tick
/// from wherever the driver is currently aiming (a ground-plane raycast
/// under the cursor, the same technique `building_placement.rs` already
/// uses) — the server just assigns it directly to `TankSnapshot::turret_yaw`
/// with no smoothing of its own (client-side rendering interpolates the
/// *visual* turret toward it instead, so a sudden aim snap doesn't pop).
#[derive(Resource, Serialize, Deserialize, Clone, Copy, Default, Debug)]
pub struct TankInput {
    pub throttle: f32,
    pub steer: f32,
    pub brake: bool,
    pub turret_yaw: f32,
}

/// Server-side per-tank input state — see `CarInputState`'s own docs for
/// why this wraps `TankInput` in a `Component` rather than deriving both
/// traits on one type.
#[derive(Component, Clone, Copy, Default, Debug)]
pub struct TankInputState {
    pub input: TankInput,
}

/// Twice a car's mass (see `car_physics::CAR_MASS`) — a tank should feel
/// like it's shoving real weight around, not a heavy car.
pub const TANK_MASS: f32 = 2600.0;
pub const TANK_LINEAR_DAMPING: f32 = 0.4;
pub const TANK_ANGULAR_DAMPING: f32 = 4.5;

/// The one set of tuning constants every tank uses — same "server and
/// client must never quietly diverge" reasoning `car_physics::default_chassis`
/// documents.
pub fn default_tank_chassis() -> TankChassis {
    let half_extents = Vec3::new(1.5, 0.45, 2.6);
    TankChassis {
        half_extents,
        wheel_radius: 0.5,
        rest_length: 0.4,
        spring_stiffness: 140_000.0,
        damper: 16_000.0,
        // Tanks pivot tighter than they "steer" — a wide swept-wheel turn
        // radius would read as a car with a heavy skin, not a tracked
        // vehicle.
        max_steer_rad: 55f32.to_radians(),
        // Terminal speed roughly engine_force / (linear_damping * mass):
        // 26_000 / (0.4 * 2600) =~ 25 m/s (~90 km/h) — noticeably slower
        // than a car's ~257 km/h top end, on purpose.
        engine_force: 26_000.0,
        brake_force: 40_000.0,
        traction: 24_000.0,
        color_seed: 0,
        owner_player_id: Uuid::nil(),
        tank_id: Uuid::nil(),
    }
}

/// Local (chassis-space) turret pivot — roughly the hull's own center,
/// raised to sit on top of the deck. `shared` (not just client render)
/// because the server needs this same point as the cannon's firing origin
/// (`server::weapons`'s tank-fire handler).
pub fn turret_pivot_offset(half_extents: Vec3) -> Vec3 {
    Vec3::new(0.0, half_extents.y * 1.4, 0.0)
}

/// Local (turret-space, i.e. already past `turret_pivot_offset`) muzzle
/// point — out along whichever way the turret is currently facing, at
/// barrel height above the pivot.
pub fn turret_muzzle_offset(half_extents: Vec3) -> Vec3 {
    Vec3::new(0.0, half_extents.y * 0.3, -(half_extents.z * 0.9))
}

/// Combines a turret's yaw and pitch into one world-space rotation — shared
/// by every aim/fire/render call site (`server::turrets`, `weapons.rs`'s
/// tank-fire handler, `client::tank_render`/`building_render`'s head
/// visuals) so they can never quietly disagree on how the two angles
/// compose. Yaw first, then pitch around the *already-yawed* local X axis
/// — matches how a real gun mount works (traverse, then elevate), and
/// means `turret_aim_direction`'s `-pitch` sign (positive pitch tilts the
/// barrel up) holds regardless of which way the turret is currently
/// facing.
pub fn turret_aim_rotation(yaw: f32, pitch: f32) -> Quat {
    Quat::from_rotation_y(yaw) * Quat::from_rotation_x(-pitch)
}

/// World-space direction a turret is aiming, given its current yaw/pitch —
/// `turret_aim_rotation(yaw, pitch) * Vec3::Z`, named so call sites read as
/// "the aim direction" rather than re-deriving which local axis is forward
/// each time.
pub fn turret_aim_direction(yaw: f32, pitch: f32) -> Vec3 {
    turret_aim_rotation(yaw, pitch) * Vec3::Z
}
