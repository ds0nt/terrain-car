use bevy::prelude::*;
use bevy_rapier3d::prelude::*;
use bevy_replicon::prelude::*;
use shared::car_physics::{gun_muzzle_offset, CarChassis};
use shared::combat::{Combatant, Health, FIRE_COOLDOWN_SECS};
use shared::protocol::{FireGunMsg, GunFiredMsg, TankSnapshot};
use shared::tank_physics::{turret_muzzle_offset, turret_pivot_offset, TankChassis};
use shared::worldspace::WorldOrigin;

use crate::car_sim::PlayerIdentities;
use crate::economy::Wallets;
use crate::persistence::{Persistence, PersistenceCommand, WalletRow};

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
/// A hit on another player steals this much ore from them (capped at
/// whatever they actually have) straight into the shooter's own wallet —
/// a deliberate PvP tie-in between combat and the economy.
const ORE_STOLEN_PER_HIT: f32 = 5.0;

/// A tank's cannon hits noticeably harder than a car's gun — same range
/// and ore-steal amount, just a heavier shell.
const TANK_GUN_DAMAGE: f32 = 30.0;
const TANK_HIT_KNOCKBACK_DELTA_V: f32 = 9.0;
const TANK_HIT_UPWARD_DELTA_V: f32 = 3.0;
const TANK_HIT_ANGULAR_DELTA: f32 = 3.5;

pub struct WeaponsPlugin;

impl Plugin for WeaponsPlugin {
    fn build(&self, app: &mut App) {
        // Two independent observers on the same `FireGunMsg` trigger — see
        // `handle_tank_fire`'s own docs for why this is a separate handler
        // rather than folding a tank shooter into `handle_fire_gun` itself.
        app.add_observer(handle_fire_gun).add_observer(handle_tank_fire);
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
/// Shooter matched via `CarChassis::owner_player_id`, not `OwnedBy` — see
/// `car_sim::apply_car_input`'s docs on why a car's ownership no longer
/// keys off a live connection entity. Targets are matched via `Combatant`
/// (see that component's own docs), not `CarChassis` specifically — a
/// car's gun can hit a `Tank` just as it can another car; `handle_tank_fire`
/// hits the exact same target set for the same reason.
#[allow(clippy::too_many_arguments)]
fn handle_fire_gun(
    fire: On<FromClient<FireGunMsg>>,
    time: Res<Time>,
    origin: Res<WorldOrigin>,
    rapier_context: ReadRapierContext,
    mut commands: Commands,
    identities: Res<PlayerIdentities>,
    mut wallets: ResMut<Wallets>,
    persistence: Res<Persistence>,
    mut shooters: Query<(Entity, &CarChassis, &Transform, &mut LastFired)>,
    mut targets: Query<(&mut Velocity, &mut Health, &Combatant)>,
) {
    let Some(client_entity) = fire.client_id.entity() else {
        return;
    };
    let Some(shooter_id) = identities.get(client_entity) else {
        return;
    };
    let Some((shooter_entity, chassis, shooter_transform, mut last_fired)) =
        shooters.iter_mut().find(|(_, chassis, ..)| chassis.owner_player_id == shooter_id)
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
            if let Ok((mut velocity, mut health, target_combatant)) = targets.get_mut(hit_entity) {
                health.apply_damage(GUN_DAMAGE);
                velocity.linear += forward * HIT_KNOCKBACK_DELTA_V + Vec3::Y * HIT_UPWARD_DELTA_V;
                velocity.angular += Vec3::new(forward.z, 0.0, -forward.x) * HIT_ANGULAR_DELTA;

                // Ore steal — a target's `owner_player_id` is always a real
                // account id (unlike the old `OwnedBy`-based lookup, this
                // never needs a live connection to resolve one), so this no
                // longer needs to skip an "unidentified target" case.
                let target_id = target_combatant.owner_player_id;
                let stolen = wallets.steal_ore(target_id, shooter_id, ORE_STOLEN_PER_HIT);
                if stolen > 0.0 {
                    for player_id in [shooter_id, target_id] {
                        if let Some((energy, ore)) = wallets.get(player_id) {
                            persistence.send(PersistenceCommand::SaveWallet(WalletRow {
                                player_id,
                                energy: energy as f64,
                                ore: ore as f64,
                            }));
                        }
                    }
                }
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

/// Same `FireGunMsg` a car's own gun handles, resolved a second time
/// independently for a tank shooter — see `WeaponsPlugin::build`'s own
/// docs for why this is a separate observer rather than one handler
/// covering both: a car-driving sender simply matches nothing in
/// `tank_shooters` here (and vice versa in `handle_fire_gun`), so there's
/// no risk of double-firing for either — each shot only ever resolves
/// once, through whichever handler's shooter query actually matches.
///
/// The one real difference from `handle_fire_gun`: the ray fires from the
/// turret's own aimed muzzle (`shared::tank_physics::turret_pivot_offset`/
/// `turret_muzzle_offset`, rotated by the tank's current `turret_yaw` —
/// see that field's own docs on its world-space-yaw convention), not the
/// hull's forward direction — a tank's cannon points wherever the driver
/// is aiming the turret, independent of which way the hull itself is
/// currently facing.
#[allow(clippy::too_many_arguments)]
fn handle_tank_fire(
    fire: On<FromClient<FireGunMsg>>,
    time: Res<Time>,
    origin: Res<WorldOrigin>,
    rapier_context: ReadRapierContext,
    mut commands: Commands,
    identities: Res<PlayerIdentities>,
    mut wallets: ResMut<Wallets>,
    persistence: Res<Persistence>,
    mut tank_shooters: Query<(Entity, &TankChassis, &TankSnapshot, &Transform, &mut LastFired)>,
    mut targets: Query<(&mut Velocity, &mut Health, &Combatant)>,
) {
    let Some(client_entity) = fire.client_id.entity() else {
        return;
    };
    let Some(shooter_id) = identities.get(client_entity) else {
        return;
    };
    let Some((shooter_entity, chassis, snapshot, shooter_transform, mut last_fired)) =
        tank_shooters.iter_mut().find(|(_, chassis, ..)| chassis.owner_player_id == shooter_id)
    else {
        return;
    };

    let now = time.elapsed_secs();
    if now - last_fired.0 < FIRE_COOLDOWN_SECS {
        return;
    }
    last_fired.0 = now;

    let pivot_world = shooter_transform.transform_point(turret_pivot_offset(chassis.half_extents));
    let turret_rotation = Quat::from_rotation_y(snapshot.turret_yaw);
    let muzzle_local = pivot_world + turret_rotation * turret_muzzle_offset(chassis.half_extents);
    let forward = turret_rotation * Vec3::Z;

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
            if let Ok((mut velocity, mut health, target_combatant)) = targets.get_mut(hit_entity) {
                health.apply_damage(TANK_GUN_DAMAGE);
                velocity.linear += forward * TANK_HIT_KNOCKBACK_DELTA_V + Vec3::Y * TANK_HIT_UPWARD_DELTA_V;
                velocity.angular += Vec3::new(forward.z, 0.0, -forward.x) * TANK_HIT_ANGULAR_DELTA;

                let target_id = target_combatant.owner_player_id;
                let stolen = wallets.steal_ore(target_id, shooter_id, ORE_STOLEN_PER_HIT);
                if stolen > 0.0 {
                    for player_id in [shooter_id, target_id] {
                        if let Some((energy, ore)) = wallets.get(player_id) {
                            persistence.send(PersistenceCommand::SaveWallet(WalletRow {
                                player_id,
                                energy: energy as f64,
                                ore: ore as f64,
                            }));
                        }
                    }
                }
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
