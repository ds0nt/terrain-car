use bevy::prelude::*;
pub use shared::worldspace::WorldOrigin;
use shared::worldspace::REBASE_THRESHOLD;

use crate::camera::CarCamera;
use crate::car::LocalCar;
use crate::terrain::TerrainChunk;

pub struct WorldSpacePlugin;

impl Plugin for WorldSpacePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<WorldOrigin>()
            .add_systems(Update, rebase_world);
    }
}

/// Re-centers the local coordinate frame on the car once it wanders far
/// enough, moving every affected Transform by the same amount so nothing
/// visibly jumps. This is the classic "floating origin" technique real
/// large-world games (space sims especially) use to dodge f32 precision
/// loss far from the origin, rather than trying to make the physics/render
/// pipeline itself work in higher precision.
fn rebase_world(
    mut origin: ResMut<WorldOrigin>,
    mut chassis_q: Query<&mut Transform, With<LocalCar>>,
    mut camera_q: Query<&mut Transform, (With<CarCamera>, Without<LocalCar>)>,
    mut chunk_q: Query<
        &mut Transform,
        (With<TerrainChunk>, Without<LocalCar>, Without<CarCamera>),
    >,
) {
    let Ok(mut chassis_tf) = chassis_q.single_mut() else {
        return;
    };
    let local = chassis_tf.translation;
    if local.x.abs() < REBASE_THRESHOLD && local.z.abs() < REBASE_THRESHOLD {
        return;
    }

    // Only shift horizontally — altitude never grows large enough (terrain
    // height is bounded) to need rebasing, and leaving it alone means one
    // less thing to keep in sync.
    let shift = Vec3::new(local.x, 0.0, local.z);
    origin.offset += shift.as_dvec3();
    chassis_tf.translation -= shift;

    for mut tf in &mut camera_q {
        tf.translation -= shift;
    }
    for mut tf in &mut chunk_q {
        tf.translation -= shift;
    }
}
