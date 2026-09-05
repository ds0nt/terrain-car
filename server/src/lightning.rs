use bevy::math::DVec3;
use bevy::prelude::*;
use bevy_rapier3d::prelude::Velocity;
use bevy_replicon::prelude::*;
use rand::Rng;
use shared::car_physics::CarChassis;
use shared::protocol::LightningStrikeMsg;
use shared::worldspace::WorldOrigin;

/// How often lightning strikes, in seconds — a new random interval in this
/// range is rolled after every strike, so it never settles into a
/// predictable rhythm. Purely cosmetic chaos ("because god is angry"), so
/// there's no gameplay-authority subtlety here worth agonizing over: any
/// connected car can be the anchor, and a tick with nobody connected is a
/// harmless no-op.
const MIN_INTERVAL_SECS: f32 = 3.0;
const MAX_INTERVAL_SECS: f32 = 10.0;
/// A strike lands within this true-space distance of a randomly-chosen car
/// — near the action, not off in empty terrain nobody would ever see.
const STRIKE_SEARCH_RADIUS: f64 = 100.0;
/// Cars within this true-space distance of the strike point feel the blast.
const BLAST_RADIUS: f32 = 10.0;
/// Outward velocity kick (m/s) for a car standing exactly at the epicenter;
/// falls off quadratically to zero at `BLAST_RADIUS`.
const BLAST_MAX_DELTA_V: f32 = 14.0;
/// Extra straight-up component on top of the outward push, so a direct hit
/// visibly launches the car instead of just shoving it sideways.
const BLAST_UPWARD_DELTA_V: f32 = 9.0;
/// Angular velocity kick (rad/s) at the epicenter, around a random axis, for
/// a tumbling look rather than every hit spinning the same clean way.
const BLAST_MAX_ANGULAR_DELTA: f32 = 5.0;

pub struct LightningPlugin;

impl Plugin for LightningPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(LightningTimer::default())
            .add_systems(Update, strike_lightning);
    }
}

#[derive(Resource)]
struct LightningTimer {
    remaining_secs: f32,
}

impl Default for LightningTimer {
    fn default() -> Self {
        Self {
            remaining_secs: rand::thread_rng().gen_range(MIN_INTERVAL_SECS..=MAX_INTERVAL_SECS),
        }
    }
}

/// Ticks the strike timer on real (variable) time — this is cosmetic chaos
/// with no replay-determinism requirement, unlike car physics, so there's
/// no need for `FixedUpdate` lockstep here. When it fires: picks a strike
/// point near a random car, kicks the `Velocity` of every car within blast
/// radius directly (the same direct-mutation pattern `recover_lost_cars`
/// already uses — Rapier picks it up on the very next physics step
/// regardless of which schedule set it), and broadcasts
/// `LightningStrikeMsg` so every client renders the same boom in the same
/// place. The knockback itself needs no separate network message: it
/// reaches clients through the `CarSnapshot` replication that already
/// exists for ordinary driving.
fn strike_lightning(
    time: Res<Time>,
    mut timer: ResMut<LightningTimer>,
    origin: Res<WorldOrigin>,
    mut cars: Query<(&Transform, &mut Velocity), With<CarChassis>>,
    mut commands: Commands,
) {
    timer.remaining_secs -= time.delta_secs();
    if timer.remaining_secs > 0.0 {
        return;
    }
    let mut rng = rand::thread_rng();
    timer.remaining_secs = rng.gen_range(MIN_INTERVAL_SECS..=MAX_INTERVAL_SECS);

    let car_locals: Vec<Vec3> = cars.iter().map(|(transform, _)| transform.translation).collect();
    if car_locals.is_empty() {
        return;
    }
    let anchor_true = origin.to_true(car_locals[rng.gen_range(0..car_locals.len())]);

    let angle = rng.gen_range(0.0..std::f64::consts::TAU);
    // sqrt() of a uniform sample keeps strikes uniformly distributed over
    // the search *area* rather than clustering near the anchor (a plain
    // uniform radius sample bunches points toward the center).
    let offset_dist = STRIKE_SEARCH_RADIUS * rng.r#gen::<f64>().sqrt();
    let strike_true = DVec3::new(
        anchor_true.x + angle.cos() * offset_dist,
        0.0,
        anchor_true.z + angle.sin() * offset_dist,
    );
    let strike_local = (strike_true - origin.offset).as_vec3();

    for (transform, mut velocity) in &mut cars {
        let delta = Vec3::new(
            transform.translation.x - strike_local.x,
            0.0,
            transform.translation.z - strike_local.z,
        );
        let car_dist = delta.length();
        if car_dist >= BLAST_RADIUS {
            continue;
        }
        let falloff = {
            let t = 1.0 - car_dist / BLAST_RADIUS;
            t * t
        };
        let away = if car_dist > 0.01 { delta / car_dist } else { Vec3::X };

        velocity.linear += away * BLAST_MAX_DELTA_V * falloff + Vec3::Y * BLAST_UPWARD_DELTA_V * falloff;

        let spin_axis = Vec3::new(
            rng.gen_range(-1.0..1.0),
            rng.gen_range(-1.0..1.0),
            rng.gen_range(-1.0..1.0),
        )
        .normalize_or_zero();
        velocity.angular += spin_axis * BLAST_MAX_ANGULAR_DELTA * falloff;
    }

    commands.server_trigger(ToClients {
        targets: SendTargets::All,
        message: LightningStrikeMsg {
            true_x: strike_true.x,
            true_z: strike_true.z,
            radius: BLAST_RADIUS,
        },
    });
    info!(
        "server: lightning strike at true ({:.1}, {:.1})",
        strike_true.x, strike_true.z
    );
}
