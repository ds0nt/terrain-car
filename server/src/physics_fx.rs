use bevy::prelude::*;
use bevy_rapier3d::prelude::Velocity;
use rand::Rng;
use shared::car_physics::CarChassis;

/// Applies an outward-and-upward `Velocity` kick plus a random spin to
/// every car within `radius` of `center_local` (local/render-space,
/// matching whatever frame `cars`' `Transform`s are already in), falling
/// off quadratically with distance. Shared by lightning strikes
/// (`lightning.rs`) and the gun's hit knockback (`weapons.rs`) so the
/// "explosion shove" falloff math exists in exactly one place rather than
/// being re-derived per feature. Direct `Velocity` mutation, same pattern
/// `car_sim.rs`'s `recover_lost_cars` already uses — Rapier picks it up on
/// the very next physics step regardless of which schedule called this.
pub fn apply_radial_impulse(
    cars: &mut Query<(&Transform, &mut Velocity), With<CarChassis>>,
    center_local: Vec3,
    radius: f32,
    max_delta_v: f32,
    upward_delta_v: f32,
    max_angular_delta: f32,
    rng: &mut impl Rng,
) {
    for (transform, mut velocity) in cars.iter_mut() {
        let delta = Vec3::new(
            transform.translation.x - center_local.x,
            0.0,
            transform.translation.z - center_local.z,
        );
        let dist = delta.length();
        if dist >= radius {
            continue;
        }
        let falloff = {
            let t = 1.0 - dist / radius;
            t * t
        };
        let away = if dist > 0.01 { delta / dist } else { Vec3::X };

        velocity.linear += away * max_delta_v * falloff + Vec3::Y * upward_delta_v * falloff;

        let spin_axis = Vec3::new(
            rng.gen_range(-1.0..1.0),
            rng.gen_range(-1.0..1.0),
            rng.gen_range(-1.0..1.0),
        )
        .normalize_or_zero();
        velocity.angular += spin_axis * max_angular_delta * falloff;
    }
}
