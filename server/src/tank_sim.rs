use std::collections::HashSet;

use bevy::math::DVec3;
use bevy::prelude::*;
use bevy_rapier3d::prelude::*;
use bevy_replicon::prelude::*;
use shared::buildings::BuildingKind;
use shared::car_physics::{compute_wheel_forces, wheel_mounts, WheelStepInput};
use shared::combat::{Combatant, Health, DEFAULT_MAX_HEALTH};
use shared::protocol::{BuildingSnapshot, RecallTankMsg, TankFlipUprightMsg, TankInputMsg, TankSnapshot};
use shared::tank_physics::{
    default_tank_chassis, TankChassis, TankInput, TankInputState, TANK_ANGULAR_DAMPING, TANK_LINEAR_DAMPING,
    TANK_MASS,
};
use shared::terrain_gen::{height_at, TerrainNoise};
use shared::time::now_unix;
use shared::worldspace::WorldOrigin;
use uuid::Uuid;

use crate::car_sim::PlayerIdentities;
use crate::weapons::LastFired;

/// Same roof-clearance shape `car_sim::CAR_ROOF_CLEARANCE` uses for a
/// Hangar — a tank spawns on top of its `WarFactory`'s own roof.
const TANK_ROOF_CLEARANCE: f32 = 2.0;

pub struct TankSimPlugin;

impl Plugin for TankSimPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SpawnedTanksFor>()
            .add_observer(apply_tank_input)
            .add_observer(apply_tank_flip_upright)
            .add_observer(apply_recall_tank)
            .add_systems(Update, spawn_tanks_from_war_factories)
            .add_systems(FixedUpdate, step_tanks.before(PhysicsSet::SyncBackend));
    }
}

/// Which `BuildingSnapshot::id`s have already produced their one tank —
/// same in-memory-only, per-restart tradeoff `car_sim::SpawnedCarsFor`
/// already accepts.
#[derive(Resource, Default)]
struct SpawnedTanksFor(HashSet<Uuid>);

/// One tank per completed `WarFactory`, full parity with
/// `car_sim::spawn_cars_from_hangars` — see `BuildingKind::WarFactory`'s
/// own docs.
fn spawn_tanks_from_war_factories(
    mut spawned: ResMut<SpawnedTanksFor>,
    mut commands: Commands,
    origin: Res<WorldOrigin>,
    buildings: Query<&BuildingSnapshot>,
) {
    let now = now_unix();
    for building in &buildings {
        if building.kind != BuildingKind::WarFactory
            || building.build_complete_at > now
            || spawned.0.contains(&building.id)
        {
            continue;
        }
        spawned.0.insert(building.id);

        let roof_y = building.ground_y
            + 2.0 * shared::buildings::collider_shape(BuildingKind::WarFactory).half_height()
            + TANK_ROOF_CLEARANCE;
        spawn_tank_for(&mut commands, &origin, building.owner_player_id, building.true_x, building.true_z, roof_y);
        info!("server: spawned a tank for `{}` on top of their War Factory", building.owner_player_id);
    }
}

/// Spawns the authoritative tank at `(true_x, true_z, spawn_y)` — same
/// bundle shape as `car_sim::spawn_car_for`, just heavier tuning and an
/// extra `turret_yaw` field on the snapshot. `pub(crate)` for the same
/// "an AI patrol starter might reuse this later" reason `spawn_car_for` is.
pub(crate) fn spawn_tank_for(
    commands: &mut Commands,
    origin: &WorldOrigin,
    owner_player_id: Uuid,
    true_x: f64,
    true_z: f64,
    spawn_y: f32,
) -> Entity {
    let mut chassis = default_tank_chassis();
    chassis.color_seed = shared::owner::seed_from_uuid(owner_player_id);
    chassis.owner_player_id = owner_player_id;
    chassis.tank_id = Uuid::new_v4();

    let local_spawn = (DVec3::new(true_x, 0.0, true_z) - origin.offset).as_vec3();

    commands
        .spawn((
            Transform::from_xyz(local_spawn.x, spawn_y, local_spawn.z),
            RigidBody::Dynamic,
            Collider::cuboid(chassis.half_extents.x, chassis.half_extents.y, chassis.half_extents.z),
            AdditionalMassProperties::Mass(TANK_MASS),
            Velocity::zero(),
            ExternalForce::default(),
            Damping {
                linear_damping: TANK_LINEAR_DAMPING,
                angular_damping: TANK_ANGULAR_DAMPING,
            },
            Ccd::enabled(),
            chassis,
            TankInputState::default(),
            TankSnapshot { home_true_x: true_x, home_true_z: true_z, ..Default::default() },
            (
                Health::full(DEFAULT_MAX_HEALTH),
                Combatant { owner_player_id },
                LastFired::default(),
                bevy_replicon::prelude::Replicated,
            ),
        ))
        .id()
}

/// Same id-matched buffering shape `car_sim::apply_car_input` uses — see
/// that function's own docs on why matching `tank_id` (not just
/// `owner_player_id`) is required once a player can own more than one.
fn apply_tank_input(
    input_msg: On<FromClient<TankInputMsg>>,
    identities: Res<PlayerIdentities>,
    mut tanks: Query<(&TankChassis, &mut TankInputState)>,
) {
    let Some(client_entity) = input_msg.client_id.entity() else {
        return;
    };
    let Some(player_id) = identities.get(client_entity) else {
        return;
    };
    for (chassis, mut state) in &mut tanks {
        if chassis.owner_player_id == player_id && chassis.tank_id == input_msg.tank_id {
            state.input = TankInput {
                throttle: input_msg.throttle,
                steer: input_msg.steer,
                brake: input_msg.brake,
                turret_yaw: input_msg.turret_yaw,
            };
        }
    }
}

/// Same "recompute ground height at the tank's own current position, don't
/// search elsewhere" shape `car_sim::apply_flip_upright` uses — see that
/// function's own docs for why the raycast excludes the tank's own
/// collider.
fn apply_tank_flip_upright(
    flip_msg: On<FromClient<TankFlipUprightMsg>>,
    identities: Res<PlayerIdentities>,
    noise: Res<TerrainNoise>,
    origin: Res<WorldOrigin>,
    rapier_context: ReadRapierContext,
    mut tanks: Query<(Entity, &TankChassis, &mut Transform, &mut Velocity, &mut ExternalForce)>,
) {
    let Some(client_entity) = flip_msg.client_id.entity() else {
        return;
    };
    let Some(player_id) = identities.get(client_entity) else {
        return;
    };
    for (entity, chassis, mut transform, mut velocity, mut ext_force) in &mut tanks {
        if chassis.owner_player_id != player_id || chassis.tank_id != flip_msg.tank_id {
            continue;
        }
        let ground_y = match rapier_context.single() {
            Ok(context) => crate::economy::surface_height_at(
                &context,
                &noise,
                &origin,
                flip_msg.true_x,
                flip_msg.true_z,
                Some(entity),
            ),
            Err(_) => height_at(&noise, flip_msg.true_x, flip_msg.true_z),
        };
        transform.translation.y = ground_y + 2.0;
        transform.rotation = Quat::IDENTITY;
        *velocity = Velocity::zero();
        *ext_force = ExternalForce::default();
        break;
    }
}

/// Same "nearest owned factory of the matching kind, fall back to recorded
/// spawn point" shape `car_sim::apply_recall_to_hangar` uses.
fn apply_recall_tank(
    recall: On<FromClient<RecallTankMsg>>,
    identities: Res<PlayerIdentities>,
    noise: Res<TerrainNoise>,
    origin: Res<WorldOrigin>,
    rapier_context: ReadRapierContext,
    buildings: Query<&BuildingSnapshot>,
    mut tanks: Query<(Entity, &TankChassis, &mut Transform, &mut Velocity, &mut ExternalForce, &TankSnapshot)>,
) {
    let Some(client_entity) = recall.client_id.entity() else {
        return;
    };
    let Some(player_id) = identities.get(client_entity) else {
        return;
    };

    for (entity, chassis, mut transform, mut velocity, mut ext_force, snapshot) in &mut tanks {
        if chassis.owner_player_id != player_id || chassis.tank_id != recall.tank_id {
            continue;
        }

        let tank_true = origin.to_true(transform.translation);
        let nearest = buildings
            .iter()
            .filter(|b| b.kind == BuildingKind::WarFactory && b.owner_player_id == player_id)
            .min_by(|a, b| {
                let da = (a.true_x - tank_true.x).powi(2) + (a.true_z - tank_true.z).powi(2);
                let db = (b.true_x - tank_true.x).powi(2) + (b.true_z - tank_true.z).powi(2);
                da.total_cmp(&db)
            });

        let (target_true_x, target_true_z, target_y) = match nearest {
            Some(factory) => (
                factory.true_x,
                factory.true_z,
                factory.ground_y + 2.0 * shared::buildings::collider_shape(BuildingKind::WarFactory).half_height(),
            ),
            None => {
                let y = match rapier_context.single() {
                    Ok(context) => crate::economy::surface_height_at(
                        &context,
                        &noise,
                        &origin,
                        snapshot.home_true_x,
                        snapshot.home_true_z,
                        Some(entity),
                    ),
                    Err(_) => height_at(&noise, snapshot.home_true_x, snapshot.home_true_z),
                };
                (snapshot.home_true_x, snapshot.home_true_z, y)
            }
        };

        let local = (DVec3::new(target_true_x, 0.0, target_true_z) - origin.offset).as_vec3();
        transform.translation = Vec3::new(local.x, target_y + TANK_ROOF_CLEARANCE, local.z);
        transform.rotation = Quat::IDENTITY;
        *velocity = Velocity::zero();
        *ext_force = ExternalForce::default();
        break;
    }
}

/// Authoritative suspension/drive step, run once per tank per fixed tick —
/// identical shape to `car_sim::step_cars`, just through `TankChassis` (see
/// `shared::car_physics::WheeledChassis`) and with `boost` always `false`
/// (a tank has no boost input) and an extra pass-through of `turret_yaw`
/// onto the snapshot, unrelated to the hull's own physics.
fn step_tanks(
    time: Res<Time>,
    rapier_context: ReadRapierContext,
    mut tanks_q: Query<(
        Entity,
        &GlobalTransform,
        &Velocity,
        &mut ExternalForce,
        &TankChassis,
        &TankInputState,
        &mut TankSnapshot,
    )>,
) {
    let Ok(context) = rapier_context.single() else {
        return;
    };
    let dt = time.delta_secs();
    if dt <= 0.0 {
        return;
    }

    for (entity, chassis_gt, velocity, mut ext_force, chassis, input_state, mut snapshot) in &mut tanks_q {
        let transform = chassis_gt.compute_transform();
        let center_of_mass = transform.translation;
        let up = transform.up();
        let forward = transform.forward();
        let right = transform.right();
        let input = input_state.input;

        let mut total_force = Vec3::ZERO;
        let mut total_torque = Vec3::ZERO;

        for (offset, is_front) in wheel_mounts(chassis.half_extents) {
            let ray_origin = transform.transform_point(offset);
            let ray_dir = -up;
            let max_toi = chassis.rest_length + chassis.wheel_radius;

            let hit = context.cast_ray_and_get_normal(
                ray_origin,
                *ray_dir,
                max_toi,
                true,
                QueryFilter::new().exclude_rigid_body(entity),
            );

            let steer_angle = if is_front { input.steer * chassis.max_steer_rad } else { 0.0 };
            let wheel_forward = Quat::from_axis_angle(*up, steer_angle) * *forward;
            let wheel_right = Quat::from_axis_angle(*up, steer_angle) * *right;

            if let Some((_hit_entity, intersection)) = hit {
                let suspension_len =
                    (intersection.time_of_impact - chassis.wheel_radius).max(0.0).min(chassis.rest_length);
                let compression = chassis.rest_length - suspension_len;
                let point_velocity = velocity.linear_velocity_at_point(intersection.point, center_of_mass);
                let closing_speed = point_velocity.dot(*up);
                let arm = intersection.point - center_of_mass;

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
                        boost: false,
                    },
                );
                total_force += out.force;
                total_torque += out.torque;
            }
        }

        ext_force.force = total_force;
        ext_force.torque = total_torque;

        snapshot.translation = transform.translation;
        snapshot.rotation = transform.rotation;
        snapshot.linear_velocity = velocity.linear;
        snapshot.angular_velocity = velocity.angular;
        snapshot.turret_yaw = input.turret_yaw;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::app::App;
    use uuid::Uuid;

    /// Reproduces the reported "tank doesn't spawn when the War Factory
    /// completes" bug in isolation, with no client/networking/physics
    /// plugin involved at all — just the one system actually responsible
    /// (`spawn_tanks_from_war_factories`) against a bare `App`, so a real
    /// logic bug here can't hide behind "well it compiles."
    #[test]
    fn a_completed_war_factory_spawns_a_tank() {
        let mut app = App::new();
        app.init_resource::<SpawnedTanksFor>();
        app.insert_resource(WorldOrigin::default());
        app.add_systems(Update, spawn_tanks_from_war_factories);

        app.world_mut().spawn(BuildingSnapshot {
            id: Uuid::new_v4(),
            kind: BuildingKind::WarFactory,
            owner_player_id: Uuid::new_v4(),
            true_x: 10.0,
            true_z: 20.0,
            build_complete_at: 0.0, // already in the past
            rotation_y: 0.0,
            ground_y: 5.0,
        });

        app.update();

        let tank_count = app.world_mut().query::<&TankChassis>().iter(app.world()).count();
        assert_eq!(tank_count, 1, "expected exactly one tank to spawn for the completed War Factory");
    }

    /// Same scenario as `a_completed_war_factory_spawns_a_tank`, but going
    /// through the exact same `economy::spawn_building` production helper
    /// a real `PlaceBuildingMsg` placement calls, instead of hand-building
    /// a `BuildingSnapshot` — rules out "the test's synthetic building
    /// isn't representative of a real one" as an explanation for a report
    /// that a completed real War Factory produced no tank in practice.
    #[test]
    fn a_production_spawned_war_factory_also_spawns_a_tank() {
        use bevy::ecs::world::CommandQueue;

        let mut app = App::new();
        app.init_resource::<SpawnedTanksFor>();
        app.insert_resource(WorldOrigin::default());
        app.add_systems(Update, spawn_tanks_from_war_factories);

        let mut queue = CommandQueue::default();
        {
            let mut commands = Commands::new(&mut queue, app.world());
            let origin = WorldOrigin::default();
            crate::economy::spawn_building(
                &mut commands,
                &origin,
                Uuid::new_v4(),
                BuildingKind::WarFactory,
                Uuid::new_v4(),
                10.0,
                20.0,
                0.0, // already completed
                0.0,
                5.0,
            );
        }
        queue.apply(app.world_mut());

        app.update();

        let tank_count = app.world_mut().query::<&TankChassis>().iter(app.world()).count();
        assert_eq!(tank_count, 1, "a War Factory spawned via the real production helper should still produce a tank");
    }

    /// A still-under-construction War Factory must not spawn a tank yet.
    #[test]
    fn an_incomplete_war_factory_spawns_nothing() {
        let mut app = App::new();
        app.init_resource::<SpawnedTanksFor>();
        app.insert_resource(WorldOrigin::default());
        app.add_systems(Update, spawn_tanks_from_war_factories);

        app.world_mut().spawn(BuildingSnapshot {
            id: Uuid::new_v4(),
            kind: BuildingKind::WarFactory,
            owner_player_id: Uuid::new_v4(),
            true_x: 0.0,
            true_z: 0.0,
            build_complete_at: f64::MAX, // far in the future
            rotation_y: 0.0,
            ground_y: 0.0,
        });

        app.update();

        let tank_count = app.world_mut().query::<&TankChassis>().iter(app.world()).count();
        assert_eq!(tank_count, 0);
    }
}
