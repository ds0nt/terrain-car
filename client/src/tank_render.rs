use bevy::prelude::*;
use shared::car_physics::wheel_mounts;
use shared::protocol::TankSnapshot;
use shared::tank_physics::{turret_aim_rotation, TankChassis, TankInput};
use shared::worldspace::WorldOrigin;
use uuid::Uuid;

use crate::owner_color::color_from_seed;
use crate::pilot::ControlMode;
use crate::tank::DrivingTankId;
use crate::turret_render::spawn_turret_head_meshes;

/// Renders every replicated `TankChassis`/`TankSnapshot` pair — yours or
/// anyone else's, same "purely cosmetic, driven off replicated data, no
/// client-side prediction" relationship `car_render.rs` has to `CarChassis`/
/// `CarSnapshot`. Hull + wheels mirror a car's own placeholder look almost
/// exactly (reusing `wheel_mounts` directly, since it's already just a pure
/// function of `half_extents`); the one real visual difference is the
/// separately-rotating turret head, built from the exact same shared
/// `turret_render::spawn_turret_head_meshes` a placed `Turret` building
/// uses.
pub struct TankRenderPlugin;

impl Plugin for TankRenderPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(init_tank_visuals)
            .add_systems(Update, (sync_tank_transforms, sync_tank_turret_heads));
    }
}

const SYNC_SMOOTHING_RATE: f32 = 20.0;

fn init_tank_visuals(
    insert: On<Insert, TankChassis>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    chassis_q: Query<&TankChassis>,
) {
    let Ok(chassis) = chassis_q.get(insert.entity) else {
        return;
    };
    let half_extents = chassis.half_extents;
    let color = color_from_seed(chassis.color_seed);

    // No tank is ever spawned locally with its own `Transform` — every one
    // arrives via replication with it unset, same as a car (see
    // `car_render.rs`'s own docs); `sync_tank_transforms` corrects it the
    // very next frame.
    commands.entity(insert.entity).insert_if_new(Transform::IDENTITY);

    commands.entity(insert.entity).insert((
        Mesh3d(meshes.add(Cuboid::from_size(half_extents * 2.0))),
        MeshMaterial3d(materials.add(StandardMaterial { base_color: color, ..default() })),
    ));

    let wheel_mesh = meshes.add(Cylinder::new(chassis.wheel_radius, 0.3));
    let wheel_material = materials.add(StandardMaterial { base_color: Color::srgb(0.05, 0.05, 0.05), ..default() });
    commands.entity(insert.entity).with_children(|parent| {
        for (offset, _is_front) in wheel_mounts(half_extents) {
            parent.spawn((
                Mesh3d(wheel_mesh.clone()),
                MeshMaterial3d(wheel_material.clone()),
                Transform::from_translation(offset).with_rotation(Quat::from_rotation_z(std::f32::consts::FRAC_PI_2)),
            ));
        }

        parent
            .spawn((
                Transform::default(),
                Visibility::default(),
                TankTurretHead { tank_entity: insert.entity, tank_id: chassis.tank_id },
            ))
            .with_children(|head| {
                spawn_turret_head_meshes(head, &mut meshes, &mut materials, half_extents, color);
            });
    });
}

/// Same "ease toward the latest snapshot every frame" shape
/// `car_render.rs`'s `sync_car_transforms` uses — see that function's own
/// docs on why smoothed rather than a direct snap.
///
/// `TankSnapshot.translation` is server-space, which is always true-space
/// (the server never rebases its own `WorldOrigin` — see
/// `car_render.rs`'s identical docs on `CarSnapshot`). Converting through
/// the client's own current `WorldOrigin` here is what keeps a tank
/// positioned correctly relative to local terrain once *this* client has
/// rebased away from true (0,0,0) — which is essentially always, starting
/// from login itself (`pilot::spawn_pilot_after_login` sets the client's
/// origin to the real spawn point, never exactly zero). Missing this
/// conversion was the actual bug behind "the tank doesn't spawn": it did
/// spawn, just rendered offset by however far this client's own origin had
/// drifted from server-space zero.
fn sync_tank_transforms(time: Res<Time>, origin: Res<WorldOrigin>, mut tanks: Query<(&TankSnapshot, &mut Transform)>) {
    let lerp_factor = 1.0 - (-SYNC_SMOOTHING_RATE * time.delta_secs()).exp();
    for (snapshot, mut transform) in &mut tanks {
        let target = (snapshot.translation.as_dvec3() - origin.offset).as_vec3();
        transform.translation = transform.translation.lerp(target, lerp_factor);
        transform.rotation = transform.rotation.slerp(snapshot.rotation, lerp_factor);
    }
}

/// Tags a tank's own rotating-head child entity — see
/// `building_render::BuildingTurretHead`'s identical shape/reasoning, just
/// keyed to a tank instead of a `Turret` building. `tank_id` (mirroring
/// `BuildingTurretHead::building_id`) is how `sync_tank_turret_heads` tells
/// whether this is the tank the local player is actually driving, without
/// an extra query.
#[derive(Component)]
struct TankTurretHead {
    tank_entity: Entity,
    tank_id: Uuid,
}

/// Directly assigned, not lerped like the hull's own `sync_tank_transforms`
/// — a driver's own mouse-aim (`tank.rs`'s `read_tank_input`) already feels
/// instantaneous, and smoothing the *visual* turret on top of an already-
/// instant input source would just reintroduce a frame of lag between
/// "where I'm aiming" and "where the turret visibly points," the same
/// reasoning `pilot::update_pilot_camera`'s own docs give for skipping a
/// lerp on a camera that's supposed to mirror an already-instant look input.
///
/// For the tank you're actually driving, this reads the local `TankInput`
/// resource directly rather than the replicated `TankSnapshot` — the exact
/// same "your own instant local input, not a network round-trip" fix
/// `building_render::sync_turret_heads`/`turret_control.rs` apply for a
/// manually-operated `Turret` building, for the identical reported jitter.
/// Every other (remote) tank still reads the replicated snapshot, same as
/// before — there's no local input to prefer for someone else's tank.
fn sync_tank_turret_heads(
    mode: Res<ControlMode>,
    driving: Res<DrivingTankId>,
    input: Res<TankInput>,
    mut heads: Query<(&TankTurretHead, &mut Transform)>,
    tanks: Query<&TankSnapshot>,
) {
    let driven_tank_id = (*mode == ControlMode::Tank).then_some(driving.0).flatten();
    for (head, mut transform) in &mut heads {
        let turret_yaw = if driven_tank_id == Some(head.tank_id) {
            input.turret_yaw
        } else if let Ok(snapshot) = tanks.get(head.tank_entity) {
            snapshot.turret_yaw
        } else {
            continue;
        };
        transform.rotation = turret_aim_rotation(turret_yaw, 0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::app::App;
    use shared::tank_physics::default_tank_chassis;
    use uuid::Uuid;

    /// Reproduces "a tank was spawned server-side, does it actually render
    /// client-side" in isolation — spawns exactly the two components
    /// replication would deliver (`TankChassis` + `TankSnapshot`, no
    /// networking involved at all) into a bare `App` running
    /// `TankRenderPlugin`, and checks a `Mesh3d` shows up.
    #[test]
    fn a_replicated_tank_chassis_gets_a_mesh() {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>();
        app.init_resource::<Assets<StandardMaterial>>();
        app.init_resource::<Time>();
        app.init_resource::<WorldOrigin>();
        app.init_resource::<ControlMode>();
        app.init_resource::<DrivingTankId>();
        app.init_resource::<TankInput>();
        app.add_plugins(TankRenderPlugin);

        let mut chassis = default_tank_chassis();
        chassis.owner_player_id = Uuid::new_v4();
        chassis.tank_id = Uuid::new_v4();
        let entity = app.world_mut().spawn((chassis, TankSnapshot::default())).id();

        app.update();

        assert!(
            app.world().get::<Mesh3d>(entity).is_some(),
            "expected init_tank_visuals to have attached a Mesh3d to the tank entity"
        );
    }

    /// Regression test for the actual reported bug: with the client's own
    /// `WorldOrigin` already rebased away from server-space zero (exactly
    /// what happens the moment anyone logs in — see
    /// `sync_tank_transforms`'s own docs), a tank's rendered position must
    /// still land at the correct *local* spot, not at its raw server-space
    /// `TankSnapshot.translation` value.
    #[test]
    fn sync_tank_transforms_accounts_for_the_clients_own_world_origin() {
        use bevy::math::DVec3;

        let mut app = App::new();
        app.init_resource::<Time>();
        app.insert_resource(WorldOrigin { offset: DVec3::new(500.0, 0.0, -300.0) });
        app.add_systems(Update, sync_tank_transforms);

        let server_space_pos = Vec3::new(510.0, 6.0, -290.0);
        let entity = app
            .world_mut()
            .spawn((TankSnapshot { translation: server_space_pos, ..Default::default() }, Transform::IDENTITY))
            .id();

        // A huge delta on the very first tick makes the exponential lerp
        // converge to (effectively) exactly its target in one `update()` —
        // `Time` never auto-advances in a bare `App` with no `TimePlugin`,
        // so it has to be nudged forward manually.
        app.world_mut().resource_mut::<Time>().advance_by(std::time::Duration::from_secs(1000));
        app.update();

        let transform = app.world().get::<Transform>(entity).unwrap();
        let expected_local = (server_space_pos.as_dvec3() - DVec3::new(500.0, 0.0, -300.0)).as_vec3();
        assert!(
            transform.translation.distance(expected_local) < 0.01,
            "expected the tank to render at the local position {expected_local:?} (server-space \
             {server_space_pos:?} minus the client's own origin), got {:?}",
            transform.translation
        );
    }

    /// Regression test for the reported "jitter while aiming" bug: the
    /// tank you're actually driving must render its turret head from the
    /// local `TankInput` (instant, this-frame) rather than the replicated
    /// `TankSnapshot` (a network round-trip behind) — see
    /// `sync_tank_turret_heads`'s own docs.
    #[test]
    fn driven_tanks_turret_uses_local_input_not_the_replicated_snapshot() {
        let mut app = App::new();
        app.insert_resource(ControlMode::Tank);
        let tank_id = Uuid::new_v4();
        app.insert_resource(DrivingTankId(Some(tank_id)));
        app.insert_resource(TankInput { turret_yaw: 1.0, ..Default::default() });
        app.add_systems(Update, sync_tank_turret_heads);

        let tank_entity = app.world_mut().spawn(TankSnapshot { turret_yaw: -2.0, ..Default::default() }).id();
        let head_entity = app
            .world_mut()
            .spawn((Transform::IDENTITY, TankTurretHead { tank_entity, tank_id }))
            .id();

        app.update();

        let head_transform = app.world().get::<Transform>(head_entity).unwrap();
        let expected = turret_aim_rotation(1.0, 0.0);
        assert!(
            head_transform.rotation.angle_between(expected) < 0.01,
            "driven tank's own turret head should follow the local TankInput.turret_yaw (1.0), \
             not the stale replicated TankSnapshot.turret_yaw (-2.0)"
        );
    }

    /// Same scenario, but for a *remote* tank (not the one being driven) —
    /// must still fall back to the replicated snapshot, since there's no
    /// local input for anyone else's tank.
    #[test]
    fn remote_tanks_turret_uses_the_replicated_snapshot() {
        let mut app = App::new();
        app.insert_resource(ControlMode::OnFoot);
        app.init_resource::<DrivingTankId>();
        app.init_resource::<TankInput>();
        app.add_systems(Update, sync_tank_turret_heads);

        let remote_tank_id = Uuid::new_v4();
        let tank_entity = app.world_mut().spawn(TankSnapshot { turret_yaw: 0.7, ..Default::default() }).id();
        let head_entity = app
            .world_mut()
            .spawn((Transform::IDENTITY, TankTurretHead { tank_entity, tank_id: remote_tank_id }))
            .id();

        app.update();

        let head_transform = app.world().get::<Transform>(head_entity).unwrap();
        let expected = turret_aim_rotation(0.7, 0.0);
        assert!(head_transform.rotation.angle_between(expected) < 0.01, "remote tank's turret should follow its replicated snapshot");
    }
}
