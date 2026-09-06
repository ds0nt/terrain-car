use bevy::prelude::*;
use serde::{Deserialize, Serialize};

/// Tire friction coefficient (max lateral grip = TIRE_GRIP * normal load).
pub const TIRE_GRIP: f32 = 1.6;

// Bottleneck: this is a raycast ("magic carpet") suspension model, not a
// joint-based one. Each wheel is a single downward ray from the chassis; it
// cannot straddle a ledge or catch on a vertical wall the way a real wheel
// collider would. That's an intentional arcade-physics tradeoff for a first
// pass — good enough to prove out terrain + driving feel, revisit if we ever
// want sim-grade wheel contact.
//
// Serialize/Deserialize + replicated (`replicate_once`, see protocol.rs):
// the server is the single source of truth for a car's tuning, sent to each
// client once on spawn rather than hard-coded twice.
#[derive(Component, Serialize, Deserialize, Clone, Copy)]
pub struct CarChassis {
    pub half_extents: Vec3,
    pub wheel_radius: f32,
    pub rest_length: f32,
    pub spring_stiffness: f32,
    pub damper: f32,
    pub max_steer_rad: f32,
    pub engine_force: f32,
    pub brake_force: f32,
    pub traction: f32,
    /// Drives this car's paint color (see client's
    /// `owner_color::color_from_seed`) — set by whoever spawns the car to
    /// a hash of that player's durable account id (`owner_color::
    /// seed_from_uuid`), so it's already known and identical to both the
    /// server and that player's own client before the server's
    /// authoritative chassis even replicates back (no visible color pop
    /// on merge), every other client sees the same color too since
    /// `CarChassis` itself is replicated, *and* it stays the same color
    /// across relaunches (account-based, not per-connection).
    pub color_seed: u32,
}

#[derive(Component)]
pub struct Wheel {
    pub local_offset: Vec3,
    pub is_front: bool,
    pub spin: f32,
}

/// Continuous driver input for one car: throttle/steer/brake. On the client
/// this lives in the global `CarInput` resource (there's only ever one
/// local driver); on the server it's wrapped per-entity in `CarInputState`
/// (one car's worth of input per connected client), updated whenever a
/// `CarInputMsg` (protocol.rs) arrives from that car's owning client. Both
/// carry the same shape so the same physics-stepping logic works unmodified
/// regardless of which side is calling it. (A single type can't derive both
/// `Resource` and `Component` in this Bevy version — hence the wrapper
/// rather than reusing one type directly as both.)
#[derive(Resource, Serialize, Deserialize, Clone, Copy, Default, Debug)]
pub struct CarInput {
    pub throttle: f32,
    pub steer: f32,
    pub brake: bool,
}

/// Server-side per-car input state — see `CarInput`'s docs for why this
/// can't just be `CarInput` itself with an extra `Component` derive.
/// `last_applied_sequence` is the reconciliation anchor echoed back to
/// clients in `CarSnapshot` (protocol.rs): the sequence number of the
/// `CarInputMsg` this state was last updated from.
#[derive(Component, Clone, Copy, Default, Debug)]
pub struct CarInputState {
    pub input: CarInput,
    pub last_applied_sequence: u32,
}

/// Rapier body-level tuning (mass, damping) that lives outside `CarChassis`
/// as separate Rapier components rather than fields on it — kept alongside
/// `default_chassis` for the same reason: server and client must spawn
/// identical cars.
pub const CAR_MASS: f32 = 1100.0;
/// Close to critical damping isn't the goal here (that's the suspension
/// spring/damper); this is aerodynamic-ish drag on the whole body, which is
/// what `engine_force`'s terminal-speed comment in `default_chassis` is
/// computed against.
pub const CAR_LINEAR_DAMPING: f32 = 0.28;
pub const CAR_ANGULAR_DAMPING: f32 = 3.5;

/// The one set of tuning constants every car uses. Shared so the server's
/// authoritative spawn and the client's locally-predicted spawn (see
/// `protocol::LocalCar`) can never quietly drift apart before the server's
/// replicated `CarChassis` echo even arrives — a client running even
/// slightly different numbers would mispredict every tick.
pub fn default_chassis() -> CarChassis {
    let half_extents = Vec3::new(0.9, 0.35, 1.9);
    CarChassis {
        half_extents,
        wheel_radius: 0.4,
        rest_length: 0.35,
        spring_stiffness: 60_000.0,
        // Close to critical damping for a ~275kg quarter-car load
        // (2*sqrt(k*m) =~ 8100); under-damped here bounced the chassis
        // hard enough to help flip it.
        damper: 7_500.0,
        max_steer_rad: 35f32.to_radians(),
        // Terminal speed is roughly engine_force / (linear_damping * mass):
        // 22000 / (0.28 * 1100) =~ 71 m/s (~257 km/h). Insane terrain wants
        // an insane top end.
        engine_force: 22_000.0,
        brake_force: 20_000.0,
        // Lowered slightly from 12_000.0 — a touch less grip reads better
        // on "insane terrain" than sticking dead to the surface.
        traction: 10_000.0,
        // Caller sets this to the actual player's connection id — this
        // placeholder only matters if something spawns a car without ever
        // overwriting it.
        color_seed: 0,
    }
}

/// Local (chassis-space) mount point and is-front flag for each of the 4
/// wheels, derived from the chassis half-extents. Car forward (the
/// direction throttle actually drives it) is local -Z, matching Bevy's
/// convention — so the *front* wheels, which steer, are the ones mounted at
/// negative z, not positive. Swapping these two was the "back wheels turn
/// instead of front" bug.
pub fn wheel_mounts(half_extents: Vec3) -> [(Vec3, bool); 4] {
    let mount_y = -half_extents.y;
    [
        (Vec3::new(-half_extents.x, mount_y, -half_extents.z + 0.3), true),
        (Vec3::new(half_extents.x, mount_y, -half_extents.z + 0.3), true),
        (Vec3::new(-half_extents.x, mount_y, half_extents.z - 0.3), false),
        (Vec3::new(half_extents.x, mount_y, half_extents.z - 0.3), false),
    ]
}

/// Local (chassis-space) mount point for the front-mounted gun's muzzle —
/// used by both the server (hitscan ray origin) and the client (gun barrel
/// visual, muzzle-flash/tracer spawn point), so they always agree on
/// exactly where "the front of the car" is without needing to send it over
/// the network. Local -Z is forward (see `wheel_mounts`'s docs), so this
/// sits ahead of the front bumper at roughly hood height.
pub fn gun_muzzle_offset(half_extents: Vec3) -> Vec3 {
    Vec3::new(0.0, -half_extents.y * 0.2, -(half_extents.z + 0.6))
}

/// Per-wheel inputs needed to compute one wheel's contribution to total
/// chassis force/torque for one physics step, once a ground contact point is
/// known. Deliberately free of ECS/Rapier types (no raycasting, no
/// `ExternalForce`) so the actual suspension/drive/traction math is
/// unit-testable in isolation from the sim loop that calls it — both the
/// client (single-player / prediction) and the server (authoritative) call
/// this exact same function so their physics can never quietly diverge.
pub struct WheelStepInput {
    /// `chassis.rest_length - suspension_len`, already clamped by the caller.
    pub compression: f32,
    /// Contact-point velocity projected onto `up` (positive = compressing).
    pub closing_speed: f32,
    /// Full contact-point velocity, in world space.
    pub point_velocity: Vec3,
    pub up: Vec3,
    /// Wheel forward/right axes, already rotated by steer angle.
    pub wheel_forward: Vec3,
    pub wheel_right: Vec3,
    /// `contact_point - center_of_mass`, i.e. the torque arm.
    pub arm: Vec3,
    pub throttle: f32,
    pub brake: bool,
}

pub struct WheelStepOutput {
    pub force: Vec3,
    pub torque: Vec3,
    /// Contact-point speed along `wheel_forward` — the caller uses this to
    /// advance the wheel's visual spin.
    pub forward_speed: f32,
}

/// Suspension spring/damper, drive force, laterally-clamped tire traction,
/// and braking for one grounded wheel. See module docs on `WheelStepInput`
/// for why this is a pure function rather than an ECS system.
pub fn compute_wheel_forces(chassis: &CarChassis, input: &WheelStepInput) -> WheelStepOutput {
    let spring_force = chassis.spring_stiffness * input.compression;
    let damper_force = chassis.damper * input.closing_speed;
    let suspension_force = (spring_force - damper_force).max(0.0);

    // Everything below acts at the real ground contact point, arm and all —
    // that's what gives real weight transfer (squat under throttle, dive
    // under braking, roll into corners). See car_physics module docs for the
    // history of the flip bug this shape fixed.
    let mut force = input.up * suspension_force;
    let mut torque = input.arm.cross(force);

    if input.throttle.abs() > f32::EPSILON {
        let drive_force = input.wheel_forward * (chassis.engine_force * 0.25 * input.throttle);
        force += drive_force;
        torque += input.arm.cross(drive_force);
    }

    // Simplified lateral traction: a stiff velocity-proportional corrective
    // force, capped to a Coulomb friction circle (mu * normal load).
    // Uncapped, this term could demand more force than a tire could ever
    // generate and overshoot every step — that was the actual source of an
    // earlier oscillating-roll flip bug, not the torque arm length.
    let lateral_speed = input.point_velocity.dot(input.wheel_right);
    let desired_traction = -input.wheel_right * (lateral_speed * chassis.traction);
    let max_friction = suspension_force * TIRE_GRIP;
    let traction_force = if desired_traction.length() > max_friction {
        desired_traction.normalize() * max_friction
    } else {
        desired_traction
    };
    force += traction_force;
    torque += input.arm.cross(traction_force);

    let forward_speed = input.point_velocity.dot(input.wheel_forward);
    if input.brake {
        let brake_force = -input.wheel_forward * (forward_speed * chassis.brake_force * 0.01);
        force += brake_force;
        torque += input.arm.cross(brake_force);
    }

    WheelStepOutput {
        force,
        torque,
        forward_speed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_chassis() -> CarChassis {
        CarChassis {
            half_extents: Vec3::new(0.9, 0.35, 1.9),
            wheel_radius: 0.4,
            rest_length: 0.35,
            spring_stiffness: 60_000.0,
            damper: 7_500.0,
            max_steer_rad: 35f32.to_radians(),
            engine_force: 22_000.0,
            brake_force: 20_000.0,
            traction: 12_000.0,
            color_seed: 0,
        }
    }

    /// At rest (no compression, no velocity, no input) a grounded wheel
    /// should contribute nothing — no phantom forces holding the car up or
    /// nudging it sideways from a function that was never told anything is
    /// happening.
    #[test]
    fn zero_input_yields_zero_force() {
        let chassis = test_chassis();
        let input = WheelStepInput {
            compression: 0.0,
            closing_speed: 0.0,
            point_velocity: Vec3::ZERO,
            up: Vec3::Y,
            wheel_forward: Vec3::NEG_Z,
            wheel_right: Vec3::X,
            arm: Vec3::ZERO,
            throttle: 0.0,
            brake: false,
        };
        let out = compute_wheel_forces(&chassis, &input);
        assert_eq!(out.force, Vec3::ZERO);
        assert_eq!(out.torque, Vec3::ZERO);
        assert_eq!(out.forward_speed, 0.0);
    }

    /// The lateral traction term must never exceed the Coulomb friction
    /// circle (suspension_force * TIRE_GRIP) no matter how large the lateral
    /// slide speed is — an earlier version of this code let this term demand
    /// unbounded force, which caused a runaway oscillating-roll flip. This
    /// pins that fix down as a regression test.
    #[test]
    fn lateral_traction_is_clamped_to_friction_circle() {
        let chassis = test_chassis();
        let compression = 0.05_f32;
        let up = Vec3::Y;
        let wheel_forward = Vec3::X;
        let wheel_right = Vec3::Z;
        let input = WheelStepInput {
            compression,
            closing_speed: 0.0, // isolate the spring term, no damper contribution
            point_velocity: wheel_right * 1000.0, // absurd lateral slide speed
            up,
            wheel_forward,
            wheel_right,
            arm: Vec3::ZERO, // isolate force from torque bookkeeping
            throttle: 0.0,
            brake: false,
        };
        let out = compute_wheel_forces(&chassis, &input);

        let suspension_force = chassis.spring_stiffness * compression;
        let max_friction = suspension_force * TIRE_GRIP;

        let vertical = out.force.dot(up);
        assert!(
            (vertical - suspension_force).abs() < 1e-2,
            "vertical component should equal suspension force alone: {vertical} vs {suspension_force}"
        );

        let lateral_component = out.force - up * vertical;
        assert!(
            (lateral_component.length() - max_friction).abs() < 1e-2,
            "lateral force should sit exactly at the friction circle limit: {} vs {max_friction}",
            lateral_component.length()
        );
        // Sliding in +right should produce a corrective force in -right.
        assert!(lateral_component.dot(wheel_right) < 0.0);
    }
}
