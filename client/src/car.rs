use bevy::math::DVec3;
use bevy::prelude::*;
use bevy_rapier3d::prelude::*;
use bevy_replicon::prelude::ClientTriggerExt;
pub use shared::car_physics::{CarChassis, CarInput};
use shared::car_physics::{compute_wheel_forces, Wheel, WheelStepInput};
use shared::combat::{Health, DEFAULT_MAX_HEALTH};
pub use shared::protocol::LocalCar;
use shared::terrain_gen::{find_flat_spawn, height_at, TerrainNoise};

use crate::terrain::{RegenerateWorldEvent, TerrainTracker};
use crate::worldspace::WorldOrigin;

/// How far around a candidate spawn point to search for flat ground.
const SPAWN_SEARCH_RADIUS: f64 = 300.0;

pub struct CarPlugin;

impl Plugin for CarPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CarInput>()
            .add_message::<CarResetEvent>()
            .add_systems(Startup, spawn_car)
            .add_systems(Update, (read_car_input, reset_car).chain())
            // Physics lives in FixedUpdate (alongside Rapier itself, see
            // main.rs's `in_fixed_schedule()`) rather than Update, so the
            // suspension/drive step advances by a constant dt regardless of
            // render framerate. That determinism is what will let a future
            // server run the identical step and a client replay buffered
            // inputs during reconciliation without drifting from render-rate
            // jitter. `read_car_input`/`reset_car` stay in `Update`: they
            // only read continuous key state or a `just_pressed` edge, and
            // FixedUpdate can run zero or several times per rendered frame,
            // which would either miss that edge or double-fire it.
            .add_systems(
                FixedUpdate,
                car_suspension_and_drive.before(PhysicsSet::SyncBackend),
            );
    }
}

/// Fired the frame the car is reset, so the camera can snap to it instantly
/// instead of smoothly chasing a car that just teleported across the map.
#[derive(Message)]
pub struct CarResetEvent;

fn read_car_input(keyboard: Res<ButtonInput<KeyCode>>, mut input: ResMut<CarInput>) {
    let mut throttle = 0.0;
    let mut steer = 0.0;
    if keyboard.pressed(KeyCode::KeyW) || keyboard.pressed(KeyCode::ArrowUp) {
        throttle += 1.0;
    }
    if keyboard.pressed(KeyCode::KeyS) || keyboard.pressed(KeyCode::ArrowDown) {
        throttle -= 1.0;
    }
    if keyboard.pressed(KeyCode::KeyA) || keyboard.pressed(KeyCode::ArrowLeft) {
        steer += 1.0;
    }
    if keyboard.pressed(KeyCode::KeyD) || keyboard.pressed(KeyCode::ArrowRight) {
        steer -= 1.0;
    }
    input.throttle = throttle;
    input.steer = steer;
    input.brake = keyboard.pressed(KeyCode::Space);
}

/// Spawns the physics body for the local player's own car — cosmetics
/// (chassis mesh, wheels, cabin) are handled generically for every car,
/// local or remote, by car_render.rs's `On<Insert, CarChassis>` observer,
/// which fires for this same spawn once `CarChassis` lands below.
///
/// Tagged `LocalCar` (requires `Signature::of::<LocalCar>()`, see
/// protocol.rs) so that once the server's authoritative version of this
/// same car replicates in, bevy_replicon merges it into this entity instead
/// of spawning a visible duplicate — this spawn *is* the client-side
/// prediction: it runs immediately on Startup, before any server
/// connection necessarily exists yet, so driving never waits on the
/// network.
fn spawn_car(
    mut commands: Commands,
    noise: Res<TerrainNoise>,
    client_id: Res<crate::net::LocalClientId>,
) {
    // WorldOrigin always starts at true (0, 0, 0), so local and true
    // coordinates coincide for this very first spawn. Terrain can be
    // genuinely extreme now, so search nearby for flat-ish ground rather
    // than trusting true (0, 0) itself not to be a cliff face.
    let spawn_true = find_flat_spawn(&noise, DVec3::ZERO, SPAWN_SEARCH_RADIUS);
    let ground_y = height_at(&noise, spawn_true.x, spawn_true.z);
    let spawn_x = spawn_true.x as f32;
    let spawn_z = spawn_true.z as f32;

    let mut chassis = shared::car_physics::default_chassis();
    // Matches what the server will assign this same connection (see
    // CarChassis::color_seed's docs) — just a placeholder guess until the
    // server's authoritative chassis replicates back onto this same
    // entity, but since both sides use the same connection id, it's
    // already correct and there's no visible color pop.
    chassis.color_seed = client_id.0 as u32;
    let half_extents = chassis.half_extents;

    commands.spawn((
        Transform::from_xyz(spawn_x, ground_y + 2.0, spawn_z),
        RigidBody::Dynamic,
        Collider::cuboid(half_extents.x, half_extents.y, half_extents.z),
        AdditionalMassProperties::Mass(shared::car_physics::CAR_MASS),
        Velocity::zero(),
        ExternalForce::default(),
        Damping {
            linear_damping: shared::car_physics::CAR_LINEAR_DAMPING,
            angular_damping: shared::car_physics::CAR_ANGULAR_DAMPING,
        },
        Ccd::enabled(),
        chassis,
        // Matches the server's own spawn value (see CarChassis::color_seed's
        // docs for why the same "predict a matching guess so there's no
        // visible pop once the authoritative echo merges in" reasoning
        // applies here too) — every car starts at full health, so this
        // guess is always right at connect time.
        Health::full(DEFAULT_MAX_HEALTH),
        TerrainTracker,
        LocalCar(client_id.0),
    ));
}

/// R: snap the car back to (local) spawn, upright and stationary — for when
/// it ends up flipped, buried in a mountain, or off in a ravine somewhere.
/// Also fires on a RegenerateWorldEvent (N, see terrain.rs), which resets
/// WorldOrigin to zero first, so "local spawn" and "true spawn" coincide
/// again right after a regenerate.
fn reset_car(
    keyboard: Res<ButtonInput<KeyCode>>,
    noise: Res<TerrainNoise>,
    origin: Res<WorldOrigin>,
    mut regenerated: MessageReader<RegenerateWorldEvent>,
    mut chassis_q: Query<(&mut Transform, &mut Velocity, &mut ExternalForce), With<LocalCar>>,
    mut reset_events: MessageWriter<CarResetEvent>,
    mut commands: Commands,
    mut local_reset: ResMut<crate::prediction::LocalResetGeneration>,
) {
    let was_regenerated = regenerated.read().next().is_some();
    if !keyboard.just_pressed(KeyCode::KeyR) && !was_regenerated {
        return;
    }
    let Ok((mut transform, mut velocity, mut ext_force)) = chassis_q.single_mut() else {
        return;
    };
    // Reset near the *current* local origin, not literally true (0,0,0) —
    // if the world has rebased since spawn, that's thousands of meters of
    // driving away, and "unstick me" should mean "near where I am now."
    // Search for flat ground around it rather than trusting that exact
    // point isn't a cliff.
    let spawn_true = find_flat_spawn(&noise, origin.offset, SPAWN_SEARCH_RADIUS);
    let ground_y = height_at(&noise, spawn_true.x, spawn_true.z);
    let local_spawn = (spawn_true - origin.offset).as_vec3();
    transform.translation = Vec3::new(local_spawn.x, ground_y + 2.0, local_spawn.z);
    transform.rotation = Quat::IDENTITY;
    *velocity = Velocity::zero();
    *ext_force = ExternalForce::default();
    reset_events.write(CarResetEvent);

    // Tell the server to reset this car too (same search point, so it
    // independently finds the same flat ground — see CarResetMsg's docs),
    // and remember we did this locally so reconcile_with_server doesn't
    // treat the server's still-in-flight pre-reset snapshot as a real
    // desync and yank the car back. Without this, R would fight the
    // server's reconciliation and produce exactly the "weird" teleporting
    // back toward the old position that this fixes.
    commands.client_trigger(shared::protocol::CarResetMsg {
        near_true_x: origin.offset.x,
        near_true_z: origin.offset.z,
    });
    local_reset.0 = local_reset.0.wrapping_add(1);
}

fn car_suspension_and_drive(
    time: Res<Time>,
    input: Res<CarInput>,
    rapier_context: ReadRapierContext,
    mut chassis_q: Query<
        (
            Entity,
            &GlobalTransform,
            &Velocity,
            &mut ExternalForce,
            &CarChassis,
        ),
        With<LocalCar>,
    >,
    mut wheels_q: Query<(&ChildOf, &mut Transform, &mut Wheel)>,
) {
    let Ok(context) = rapier_context.single() else {
        return;
    };
    let dt = time.delta_secs();
    if dt <= 0.0 {
        return;
    }

    for (chassis_entity, chassis_gt, velocity, mut ext_force, chassis) in &mut chassis_q {
        let chassis_transform = chassis_gt.compute_transform();
        let center_of_mass = chassis_transform.translation;
        let up = chassis_transform.up();
        let forward = chassis_transform.forward();
        let right = chassis_transform.right();

        let mut total_force = Vec3::ZERO;
        let mut total_torque = Vec3::ZERO;
        let mut wheels_grounded = 0;

        for (child_of, mut wheel_tf, mut wheel) in &mut wheels_q {
            if child_of.parent() != chassis_entity {
                continue;
            }

            let ray_origin = chassis_transform.transform_point(wheel.local_offset);
            let ray_dir = -up;
            let max_toi = chassis.rest_length + chassis.wheel_radius;

            let hit = context.cast_ray_and_get_normal(
                ray_origin,
                *ray_dir,
                max_toi,
                true,
                QueryFilter::new().exclude_rigid_body(chassis_entity),
            );

            let steer_angle = if wheel.is_front {
                input.steer * chassis.max_steer_rad
            } else {
                0.0
            };
            let wheel_forward = Quat::from_axis_angle(*up, steer_angle) * *forward;
            let wheel_right = Quat::from_axis_angle(*up, steer_angle) * *right;

            let mut suspension_len = max_toi;

            if let Some((_entity, intersection)) = hit {
                wheels_grounded += 1;
                suspension_len = (intersection.time_of_impact - chassis.wheel_radius)
                    .max(0.0)
                    .min(chassis.rest_length);
                let compression = chassis.rest_length - suspension_len;

                let point_velocity =
                    velocity.linear_velocity_at_point(intersection.point, center_of_mass);
                let closing_speed = point_velocity.dot(*up);
                let arm = intersection.point - center_of_mass;

                // Suspension/drive/traction/brake math lives in
                // shared::car_physics as a pure function so client and
                // server can never quietly diverge on how a wheel behaves,
                // and so it's unit-testable in isolation (see that module's
                // tests, particularly the friction-circle clamp regression
                // test for the old oscillating-roll flip bug).
                let out = compute_wheel_forces(
                    chassis,
                    &WheelStepInput {
                        compression,
                        closing_speed,
                        point_velocity,
                        up: *up,
                        wheel_forward,
                        wheel_right,
                        arm,
                        throttle: input.throttle,
                        brake: input.brake,
                    },
                );
                total_force += out.force;
                total_torque += out.torque;
                wheel.spin += out.forward_speed / chassis.wheel_radius.max(0.01) * dt;
            }

            wheel_tf.translation = Vec3::new(
                wheel.local_offset.x,
                -chassis.half_extents.y - suspension_len,
                wheel.local_offset.z,
            );
            wheel_tf.rotation = Quat::from_rotation_y(steer_angle)
                * Quat::from_rotation_z(std::f32::consts::FRAC_PI_2)
                * Quat::from_rotation_y(wheel.spin);
        }

        // No gravity hack: rely on Rapier's own gravity for the fall, we only
        // add suspension/drive/traction on top of it.
        let _ = wheels_grounded;
        ext_force.force = total_force;
        ext_force.torque = total_torque;
    }
}
