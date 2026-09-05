use bevy::prelude::*;
use bevy_rapier3d::prelude::*;
use bevy_replicon::prelude::*;
use shared::car_physics::{gun_muzzle_offset, CarChassis};
use shared::combat::{Health, FIRE_COOLDOWN_SECS};
use shared::protocol::{FireGunMsg, GunFiredMsg};
use shared::worldspace::WorldOrigin;

use crate::car_sim::OwnedBy;

/// Max hitscan distance — past this a miss just draws a tracer into the
/// distance rather than resolving a hit against anything.
const GUN_RANGE: f32 = 300.0;
const GUN_DAMAGE: f32 = 12.0;
/// A direct hit shoves the target, but only that one car — unlike
/// lightning's area blast (`physics_fx::apply_radial_impulse`), a precise
/// hitscan hit has no falloff radius to speak of, so this just kicks
/// `Velocity` straight along the shot's forward direction.
const HIT_KNOCKBACK_DELTA_V: f32 = 6.0;
const HIT_UPWARD_DELTA_V: f32 = 2.0;
const HIT_ANGULAR_DELTA: f32 = 2.5;

pub struct WeaponsPlugin;

impl Plugin for WeaponsPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(handle_fire_gun);
    }
}

/// Per-car server-side fire-rate gate — `f32::MIN` so a freshly-spawned car
/// can always fire immediately rather than waiting out a phantom cooldown.
/// Server-only bookkeeping, not replicated: clients only need to know
/// *that* a shot happened (`GunFiredMsg`), never this internal timer.
#[derive(Component)]
pub(crate) struct LastFired(f32);

impl Default for LastFired {
    fn default() -> Self {
        Self(f32::MIN)
    }
}

/// Resolves one `FireGunMsg`: rate-limits, then hitscans from the shooter's
/// own muzzle point (`gun_muzzle_offset`, the same pure function the
/// client's gun-barrel visual uses, so ray origin and visible barrel tip
/// always agree) along chassis-forward. A hit applies damage + a knockback
/// kick directly (see `HIT_KNOCKBACK_DELTA_V`'s docs on why this doesn't go
/// through `physics_fx::apply_radial_impulse`); either way, broadcasts the
/// resolved muzzle/end points so every client renders the same shot.
fn handle_fire_gun(
    fire: On<FromClient<FireGunMsg>>,
    time: Res<Time>,
    origin: Res<WorldOrigin>,
    rapier_context: ReadRapierContext,
    mut commands: Commands,
    mut shooters: Query<(Entity, &OwnedBy, &Transform, &CarChassis, &mut LastFired)>,
    mut targets: Query<(&mut Velocity, &mut Health), With<CarChassis>>,
) {
    let Some(client_entity) = fire.client_id.entity() else {
        return;
    };
    let Some((shooter_entity, _, shooter_transform, chassis, mut last_fired)) =
        shooters.iter_mut().find(|(_, owner, ..)| owner.0 == client_entity)
    else {
        return;
    };

    let now = time.elapsed_secs();
    if now - last_fired.0 < FIRE_COOLDOWN_SECS {
        return;
    }
    last_fired.0 = now;

    let muzzle_local = shooter_transform.transform_point(gun_muzzle_offset(chassis.half_extents));
    let forward = *shooter_transform.forward();

    let Ok(context) = rapier_context.single() else {
        return;
    };
    let hit = context.cast_ray(
        muzzle_local,
        forward,
        GUN_RANGE,
        true,
        QueryFilter::new().exclude_rigid_body(shooter_entity),
    );

    let (end_local, did_hit) = match hit {
        Some((hit_entity, toi)) => {
            if let Ok((mut velocity, mut health)) = targets.get_mut(hit_entity) {
                health.apply_damage(GUN_DAMAGE);
                velocity.linear += forward * HIT_KNOCKBACK_DELTA_V + Vec3::Y * HIT_UPWARD_DELTA_V;
                velocity.angular += Vec3::new(forward.z, 0.0, -forward.x) * HIT_ANGULAR_DELTA;
            }
            (muzzle_local + forward * toi, true)
        }
        None => (muzzle_local + forward * GUN_RANGE, false),
    };

    let muzzle_true = origin.to_true(muzzle_local);
    let end_true = origin.to_true(end_local);

    commands.server_trigger(ToClients {
        targets: SendTargets::All,
        message: GunFiredMsg {
            muzzle_true_x: muzzle_true.x,
            muzzle_y: muzzle_local.y,
            muzzle_true_z: muzzle_true.z,
            end_true_x: end_true.x,
            end_y: end_local.y,
            end_true_z: end_true.z,
            hit: did_hit,
        },
    });
}
