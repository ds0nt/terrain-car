use bevy::prelude::*;
use bevy_rapier3d::prelude::*;
use bevy_replicon::prelude::*;
use shared::buildings::{BuildingKind, ColliderShape};
use shared::car_physics::CarChassis;
use shared::combat::{Combatant, Health, FIRE_COOLDOWN_SECS};
use shared::protocol::{
    BuildingSnapshot, EnterTurretMsg, ExitTurretMsg, FireGunMsg, GunFiredMsg, TurretAimMsg, TurretSnapshot,
};
use shared::tank_physics::{turret_aim_direction, turret_aim_rotation, turret_muzzle_offset, turret_pivot_offset, TankChassis};
use shared::time::now_unix;
use shared::worldspace::WorldOrigin;

use crate::ai::{bearing_to, normalize_angle};
use crate::car_sim::PlayerIdentities;
use crate::economy::Wallets;
use crate::persistence::{Persistence, PersistenceCommand, WalletRow};

/// Named alias purely to keep `run_turrets`'s own signature readable — see
/// clippy's `type_complexity` lint.
type EnemySearchQuery<'w, 's> = Query<'w, 's, (Entity, &'static Transform, &'static Combatant), Or<(With<CarChassis>, With<TankChassis>)>>;

/// Auto-targeting/firing for a placed `BuildingKind::Turret` — a stationary
/// defense structure, not a factory: it *is* the unit (see that kind's own
/// docs). Aims and fires exactly like a `Tank`'s own turret (reuses
/// `shared::tank_physics::turret_pivot_offset`/`turret_muzzle_offset` for
/// the identical muzzle-point math, and `server::weapons`'s own hitscan/
/// `GunFiredMsg` broadcast shape), just driven by this module's own simple
/// nearest-enemy-in-range auto-aim instead of a player's mouse.
pub struct TurretsPlugin;

impl Plugin for TurretsPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(apply_enter_turret)
            .add_observer(apply_exit_turret)
            .add_observer(apply_turret_aim)
            .add_observer(handle_turret_fire)
            .add_systems(FixedUpdate, run_turrets);
    }
}

const TURRET_RANGE: f32 = 90.0;
/// Radians/sec the turret head can slew — noticeably slower than a
/// player's own instant mouse-aim (`TankInputMsg::turret_yaw` is applied
/// directly, no rate limit) since this is meant to be reactable-to, not an
/// instant-death aimbot.
const TURRET_TURN_RATE: f32 = 2.2;
/// Same slew rate as yaw for the unmanned auto-aim's elevation — no
/// particular reason to make one axis faster than the other.
const TURRET_PITCH_RATE: f32 = 2.2;
/// Plausible mount elevation range — can't depress much below level (the
/// turret's own base is in the way) but can elevate steeply for a
/// near-overhead target (a plane, or a tank cresting a nearby rise).
const AIM_PITCH_RANGE: std::ops::RangeInclusive<f32> = -0.15..=1.4;
/// How close the aim has to be to the target's actual bearing before the
/// turret will fire at all — prevents it spraying shots into empty space
/// while still slewing onto a fast-moving target.
const AIM_TOLERANCE_RAD: f32 = 0.06;
const TURRET_FIRE_COOLDOWN_SECS: f32 = 0.9;
const TURRET_DAMAGE: f32 = 10.0;
/// Same order of magnitude as `weapons::TANK_HIT_KNOCKBACK_DELTA_V` — a
/// turret firing the same caliber shell knocks a target around about as
/// much as a tank's own cannon would. Local, not imported: `weapons.rs`'s
/// own equivalents are private to that module's `handle_tank_fire`, and
/// duplicating three tuning constants is simpler than exporting them for
/// one other caller.
const TURRET_HIT_KNOCKBACK_DELTA_V: f32 = 9.0;
const TURRET_HIT_UPWARD_DELTA_V: f32 = 3.0;
const TURRET_HIT_ANGULAR_DELTA: f32 = 3.5;
/// Same PvP ore-steal tie-in `weapons::ORE_STOLEN_PER_HIT` gives every
/// other weapon in this game.
const TURRET_ORE_STOLEN_PER_HIT: f32 = 5.0;

/// Server-only fire-rate gate — same "starts at `f32::MIN` so a freshly-
/// completed turret can fire immediately" shape `weapons::LastFired` uses.
#[derive(Component)]
pub(crate) struct TurretRuntime(pub(crate) f32);

impl Default for TurretRuntime {
    fn default() -> Self {
        Self(f32::MIN)
    }
}

/// One tick of every completed turret's auto-aim/fire: find the nearest
/// enemy car/tank in range (never the turret owner's own vehicles, and
/// never an unowned AI-patrol one — see the `owner_player_id` filter
/// below), slew `TurretSnapshot::aim_yaw` toward its bearing at
/// `TURRET_TURN_RATE`, and fire a hitscan shot once aimed closely enough
/// and off cooldown. Two separate target queries (not one with `&mut
/// Health`) for the exact same "search read-only, mutate only the one
/// entity actually hit" reason `weapons::handle_fire_gun` splits
/// `shooters`/`targets` — letting the search loop below stay a plain
/// read-only `iter()` rather than needing `iter_mut()` just to satisfy a
/// component this loop never actually needs to write.
fn run_turrets(
    time: Res<Time>,
    origin: Res<WorldOrigin>,
    rapier_context: ReadRapierContext,
    mut commands: Commands,
    mut turrets: Query<(Entity, &BuildingSnapshot, &Transform, &mut TurretSnapshot, &mut TurretRuntime)>,
    search: EnemySearchQuery,
    mut health_q: Query<&mut Health>,
) {
    let Ok(context) = rapier_context.single() else {
        return;
    };
    let now_uptime = time.elapsed_secs();
    let now_wall = now_unix();
    let dt = time.delta_secs();
    if dt <= 0.0 {
        return;
    }

    let half_extents = match shared::buildings::collider_shape(BuildingKind::Turret) {
        ColliderShape::Cuboid { half_x, half_y, half_z } => Vec3::new(half_x, half_y, half_z),
        ColliderShape::Cylinder { .. } => unreachable!("Turret's own collider_shape is always a Cuboid"),
    };

    for (turret_entity, building, transform, mut snapshot, mut runtime) in &mut turrets {
        if building.kind != BuildingKind::Turret
            || building.build_complete_at > now_wall
            || snapshot.occupant_player_id.is_some()
        {
            // A manually-occupied turret is entirely the operator's own
            // aim/fire — see `apply_turret_aim`/`handle_turret_fire`.
            continue;
        }

        let turret_pos = transform.translation;
        let nearest = search
            .iter()
            .filter(|(_, _, combatant)| {
                combatant.owner_player_id != building.owner_player_id
                    && combatant.owner_player_id != crate::ai::AI_OWNER
            })
            .map(|(entity, target_transform, _)| (entity, target_transform.translation))
            .filter(|(_, pos)| pos.distance(turret_pos) <= TURRET_RANGE)
            .min_by(|(_, a), (_, b)| a.distance(turret_pos).total_cmp(&b.distance(turret_pos)));

        let Some((target_entity, target_pos)) = nearest else {
            continue;
        };

        let turret_true = origin.to_true(turret_pos);
        let target_true = origin.to_true(target_pos);
        let desired_yaw = bearing_to(turret_true.x, turret_true.z, target_true.x, target_true.z);
        let yaw_diff = normalize_angle(desired_yaw - snapshot.aim_yaw);
        let max_yaw_step = TURRET_TURN_RATE * dt;
        snapshot.aim_yaw = normalize_angle(snapshot.aim_yaw + yaw_diff.clamp(-max_yaw_step, max_yaw_step));

        // Elevation toward the target — plain local-space trig (both
        // points already share the same reference frame, no true-space
        // conversion needed for a horizontal-distance/height comparison).
        let horizontal = ((target_pos.x - turret_pos.x).powi(2) + (target_pos.z - turret_pos.z).powi(2)).sqrt();
        let vertical = target_pos.y - turret_pos.y;
        let desired_pitch = vertical.atan2(horizontal.max(0.01)).clamp(*AIM_PITCH_RANGE.start(), *AIM_PITCH_RANGE.end());
        let pitch_diff = desired_pitch - snapshot.aim_pitch;
        let max_pitch_step = TURRET_PITCH_RATE * dt;
        snapshot.aim_pitch =
            (snapshot.aim_pitch + pitch_diff.clamp(-max_pitch_step, max_pitch_step)).clamp(*AIM_PITCH_RANGE.start(), *AIM_PITCH_RANGE.end());

        if yaw_diff.abs() > AIM_TOLERANCE_RAD
            || pitch_diff.abs() > AIM_TOLERANCE_RAD
            || now_uptime - runtime.0 < TURRET_FIRE_COOLDOWN_SECS
        {
            continue;
        }
        runtime.0 = now_uptime;

        let aim_rotation = turret_aim_rotation(snapshot.aim_yaw, snapshot.aim_pitch);
        let muzzle_local = turret_pos + turret_pivot_offset(half_extents) + aim_rotation * turret_muzzle_offset(half_extents);
        let aim_dir = turret_aim_direction(snapshot.aim_yaw, snapshot.aim_pitch);

        let hit = context.cast_ray(
            muzzle_local,
            aim_dir,
            TURRET_RANGE,
            true,
            QueryFilter::new().exclude_rigid_body(turret_entity),
        );
        let (end_local, did_hit) = match hit {
            Some((hit_entity, toi)) => {
                if hit_entity == target_entity
                    && let Ok(mut health) = health_q.get_mut(hit_entity)
                {
                    health.apply_damage(TURRET_DAMAGE);
                }
                (muzzle_local + aim_dir * toi, true)
            }
            None => (muzzle_local + aim_dir * TURRET_RANGE, false),
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
}

/// `F`, near an owned and currently-unoccupied turret — hands its aim/fire
/// away from `server::turrets`'s own auto-targeting to the sender. Only
/// the turret's own owner can ever occupy it (see `TurretSnapshot::
/// occupant_player_id`'s own docs on why this isn't "anyone with an empty
/// seat," unlike a car's passenger seat) — rejected, silently except for a
/// warning, for any other sender or an already-occupied one.
fn apply_enter_turret(
    enter: On<FromClient<EnterTurretMsg>>,
    identities: Res<PlayerIdentities>,
    mut turrets: Query<(&BuildingSnapshot, &mut TurretSnapshot)>,
) {
    let Some(client_entity) = enter.client_id.entity() else {
        return;
    };
    let Some(player_id) = identities.get(client_entity) else {
        return;
    };
    let Some((building, mut snapshot)) = turrets.iter_mut().find(|(b, _)| b.id == enter.building_id) else {
        warn!("turrets: enter request for unknown turret `{}`", enter.building_id);
        return;
    };
    if building.owner_player_id != player_id {
        warn!("turrets: rejected enter — `{player_id}` doesn't own turret `{}`", enter.building_id);
        return;
    }
    if snapshot.occupant_player_id.is_some() {
        warn!("turrets: rejected enter — turret `{}` already occupied", enter.building_id);
        return;
    }
    snapshot.occupant_player_id = Some(player_id);
}

/// `F` while manually operating a turret — hands control back to
/// `server::turrets`'s own auto-aim. Only the current occupant can vacate
/// it, same tolerance `car_sim::apply_exit_passenger` gives a stray/
/// duplicate exit from anyone else.
fn apply_exit_turret(
    exit: On<FromClient<ExitTurretMsg>>,
    identities: Res<PlayerIdentities>,
    mut turrets: Query<(&BuildingSnapshot, &mut TurretSnapshot)>,
) {
    let Some(client_entity) = exit.client_id.entity() else {
        return;
    };
    let Some(player_id) = identities.get(client_entity) else {
        return;
    };
    let Some((_, mut snapshot)) = turrets.iter_mut().find(|(b, _)| b.id == exit.building_id) else {
        return;
    };
    if snapshot.occupant_player_id == Some(player_id) {
        snapshot.occupant_player_id = None;
    }
}

/// Applies the current occupant's live mouse-aim directly, every tick —
/// no turn-rate limit at all (see `TurretAimMsg`'s own docs on why a human
/// operator's aim is instant, unlike the unmanned auto-aim's
/// `TURRET_TURN_RATE` slew). A no-op if the sender isn't this turret's
/// current occupant — the same trust boundary every other input message
/// in this game enforces.
fn apply_turret_aim(
    aim: On<FromClient<TurretAimMsg>>,
    identities: Res<PlayerIdentities>,
    mut turrets: Query<(&BuildingSnapshot, &mut TurretSnapshot)>,
) {
    let Some(client_entity) = aim.client_id.entity() else {
        return;
    };
    let Some(player_id) = identities.get(client_entity) else {
        return;
    };
    let Some((_, mut snapshot)) = turrets.iter_mut().find(|(b, _)| b.id == aim.building_id) else {
        return;
    };
    if snapshot.occupant_player_id == Some(player_id) {
        snapshot.aim_yaw = aim.aim_yaw;
        // Clamped even for a human operator — the mount's own physical
        // elevation range is a hardware limit, not a difficulty setting.
        snapshot.aim_pitch = aim.aim_pitch.clamp(*AIM_PITCH_RANGE.start(), *AIM_PITCH_RANGE.end());
    }
}

/// Manual fire for whoever's currently occupying a turret — the same
/// `FireGunMsg` a car or tank fires (see `weapons::handle_fire_gun`/
/// `handle_tank_fire`'s own docs on why multiple independent observers on
/// this one trigger is the established shape here), resolved a third time
/// for a turret shooter. Same hitscan/damage/knockback/ore-steal/broadcast
/// logic those two use, just firing from the stationary turret building's
/// own muzzle (`run_turrets`'s own math, reused here) instead of a moving
/// hull's, and keyed off `TurretSnapshot::occupant_player_id` rather than
/// a chassis' `owner_player_id`.
#[allow(clippy::too_many_arguments)]
fn handle_turret_fire(
    fire: On<FromClient<FireGunMsg>>,
    time: Res<Time>,
    origin: Res<WorldOrigin>,
    rapier_context: ReadRapierContext,
    mut commands: Commands,
    identities: Res<PlayerIdentities>,
    mut wallets: ResMut<Wallets>,
    persistence: Res<Persistence>,
    mut turrets: Query<(Entity, &Transform, &mut TurretSnapshot, &mut TurretRuntime)>,
    mut targets: Query<(&mut Velocity, &mut Health, &Combatant)>,
) {
    let Some(client_entity) = fire.client_id.entity() else {
        return;
    };
    let Some(shooter_id) = identities.get(client_entity) else {
        return;
    };
    let Some((turret_entity, transform, snapshot, mut runtime)) =
        turrets.iter_mut().find(|(_, _, snapshot, _)| snapshot.occupant_player_id == Some(shooter_id))
    else {
        return;
    };

    let now = time.elapsed_secs();
    if now - runtime.0 < FIRE_COOLDOWN_SECS {
        return;
    }
    runtime.0 = now;

    let half_extents = match shared::buildings::collider_shape(BuildingKind::Turret) {
        ColliderShape::Cuboid { half_x, half_y, half_z } => Vec3::new(half_x, half_y, half_z),
        ColliderShape::Cylinder { .. } => unreachable!("Turret's own collider_shape is always a Cuboid"),
    };
    let aim_rotation = turret_aim_rotation(snapshot.aim_yaw, snapshot.aim_pitch);
    let muzzle_local =
        transform.translation + turret_pivot_offset(half_extents) + aim_rotation * turret_muzzle_offset(half_extents);
    let forward = turret_aim_direction(snapshot.aim_yaw, snapshot.aim_pitch);

    let Ok(context) = rapier_context.single() else {
        return;
    };
    let hit = context.cast_ray(
        muzzle_local,
        forward,
        TURRET_RANGE,
        true,
        QueryFilter::new().exclude_rigid_body(turret_entity),
    );

    let (end_local, did_hit) = match hit {
        Some((hit_entity, toi)) => {
            if let Ok((mut velocity, mut health, target_combatant)) = targets.get_mut(hit_entity) {
                health.apply_damage(TURRET_DAMAGE);
                velocity.linear += forward * TURRET_HIT_KNOCKBACK_DELTA_V + Vec3::Y * TURRET_HIT_UPWARD_DELTA_V;
                velocity.angular += Vec3::new(forward.z, 0.0, -forward.x) * TURRET_HIT_ANGULAR_DELTA;

                let target_id = target_combatant.owner_player_id;
                let stolen = wallets.steal_ore(target_id, shooter_id, TURRET_ORE_STOLEN_PER_HIT);
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
        None => (muzzle_local + forward * TURRET_RANGE, false),
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
