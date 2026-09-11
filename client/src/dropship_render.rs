use bevy::prelude::*;
use shared::protocol::DropshipSnapshot;
use shared::worldspace::WorldOrigin;

use crate::owner_color::color_for_owner;
use crate::thrusters::{spawn_thrusters, ThrusterAxis, ThrusterMount};

/// Renders every replicated `DropshipSnapshot` — a bigger, boxier
/// `ScoutPlane` silhouette (see `aircraft.rs`'s own `init_plane_visuals`,
/// which this closely mirrors), tinted by owner the same way. Like
/// `CarSnapshot`/`TankSnapshot`, `DropshipSnapshot.translation` is
/// server-space (always true-space, since the server never rebases its own
/// `WorldOrigin`) — `sync_dropship_transforms` converts it through the
/// client's own current origin every frame, the same "positioned correctly
/// relative to local terrain even after *this* client has rebased" shape
/// `car_render.rs`'s own docs describe.
pub struct DropshipRenderPlugin;

impl Plugin for DropshipRenderPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(init_dropship_visuals).add_systems(Update, sync_dropship_transforms);
    }
}

const SYNC_SMOOTHING_RATE: f32 = 20.0;

fn init_dropship_visuals(
    insert: On<Insert, DropshipSnapshot>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    flame_effect: Res<crate::thrusters::ThrusterFlameEffect>,
    dropships: Query<&DropshipSnapshot>,
) {
    let Ok(snapshot) = dropships.get(insert.entity) else {
        return;
    };
    let color = color_for_owner(snapshot.owner_player_id);
    let material = materials.add(StandardMaterial { base_color: color, perceptual_roughness: 0.5, ..default() });

    commands.entity(insert.entity).insert((
        Mesh3d(meshes.add(Cuboid::new(3.4, 1.6, 6.4))),
        MeshMaterial3d(material.clone()),
        Transform::IDENTITY,
        Visibility::default(),
    ));
    commands.entity(insert.entity).with_children(|parent| {
        // Stub wings — a transport, not a glider, so these read more as
        // stabilizers than real lift surfaces.
        parent.spawn((
            Mesh3d(meshes.add(Cuboid::new(7.5, 0.2, 1.6))),
            MeshMaterial3d(material.clone()),
            Transform::from_xyz(0.0, 0.0, 0.6),
        ));
        // Twin tail fins — forward is local `-Z`, so the back is `+Z` (same
        // convention `aircraft.rs`'s own tail fin docs cover).
        for side in [-1.0, 1.0] {
            parent.spawn((
                Mesh3d(meshes.add(Cuboid::new(0.15, 1.4, 1.2))),
                MeshMaterial3d(material.clone()),
                Transform::from_xyz(side * 1.5, 0.9, 2.8),
            ));
        }
        // A cargo-bay ridge along the belly — purely cosmetic, "this
        // carries something" silhouette detail.
        parent.spawn((
            Mesh3d(meshes.add(Cuboid::new(2.4, 0.5, 3.6))),
            MeshMaterial3d(material.clone()),
            Transform::from_xyz(0.0, -0.9, 0.0),
        ));

        // Directional thruster nozzles — same set of axes a plane gets
        // (see `aircraft.rs`'s own mounts), just scaled out to this bigger
        // hull's own extremities.
        spawn_thrusters(
            parent,
            &mut meshes,
            &mut materials,
            &flame_effect.0,
            &[
                ThrusterMount { offset: Vec3::new(0.0, 0.0, 3.5), axis: ThrusterAxis::ThrottleForward },
                ThrusterMount { offset: Vec3::new(0.0, 0.0, -3.5), axis: ThrusterAxis::ThrottleReverse },
                ThrusterMount { offset: Vec3::new(0.9, 0.0, -3.1), axis: ThrusterAxis::YawPositive },
                ThrusterMount { offset: Vec3::new(-0.9, 0.0, -3.1), axis: ThrusterAxis::YawNegative },
                ThrusterMount { offset: Vec3::new(0.0, 0.9, 3.0), axis: ThrusterAxis::PitchPositive },
                ThrusterMount { offset: Vec3::new(0.0, -0.9, 3.0), axis: ThrusterAxis::PitchNegative },
                ThrusterMount { offset: Vec3::new(3.7, 0.0, 0.5), axis: ThrusterAxis::RollPositive },
                ThrusterMount { offset: Vec3::new(-3.7, 0.0, 0.5), axis: ThrusterAxis::RollNegative },
            ],
        );
    });
}

fn sync_dropship_transforms(
    time: Res<Time>,
    origin: Res<WorldOrigin>,
    mut dropships: Query<(&DropshipSnapshot, &mut Transform)>,
) {
    let lerp_factor = 1.0 - (-SYNC_SMOOTHING_RATE * time.delta_secs()).exp();
    for (snapshot, mut transform) in &mut dropships {
        let target = (snapshot.translation.as_dvec3() - origin.offset).as_vec3();
        transform.translation = transform.translation.lerp(target, lerp_factor);
        transform.rotation = transform.rotation.slerp(snapshot.rotation, lerp_factor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::app::App;
    use bevy::math::DVec3;

    /// Same regression coverage `tank_render.rs`'s identical test gives —
    /// see that test's own docs for why this exact scenario (a non-zero
    /// client `WorldOrigin`, which is the normal case from the moment of
    /// login onward) is what actually reproduced "the dropship doesn't
    /// spawn."
    #[test]
    fn sync_dropship_transforms_accounts_for_the_clients_own_world_origin() {
        let mut app = App::new();
        app.init_resource::<Time>();
        app.insert_resource(WorldOrigin { offset: DVec3::new(-800.0, 0.0, 1200.0) });
        app.add_systems(Update, sync_dropship_transforms);

        let server_space_pos = Vec3::new(-750.0, 40.0, 1250.0);
        let entity = app
            .world_mut()
            .spawn((DropshipSnapshot { translation: server_space_pos, ..zero_dropship_snapshot() }, Transform::IDENTITY))
            .id();

        app.world_mut().resource_mut::<Time>().advance_by(std::time::Duration::from_secs(1000));
        app.update();

        let transform = app.world().get::<Transform>(entity).unwrap();
        let expected_local = (server_space_pos.as_dvec3() - DVec3::new(-800.0, 0.0, 1200.0)).as_vec3();
        assert!(
            transform.translation.distance(expected_local) < 0.01,
            "expected the dropship to render at the local position {expected_local:?}, got {:?}",
            transform.translation
        );
    }

    fn zero_dropship_snapshot() -> DropshipSnapshot {
        DropshipSnapshot {
            owner_player_id: uuid::Uuid::nil(),
            dropship_id: uuid::Uuid::nil(),
            translation: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            linear_velocity: Vec3::ZERO,
            home_true_x: 0.0,
            home_true_z: 0.0,
            passenger_player_ids: [None; 4],
            cargo: [None; shared::protocol::CARGO_SLOTS],
        }
    }
}
