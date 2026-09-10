use bevy::prelude::*;
pub use shared::worldspace::WorldOrigin;
use shared::worldspace::REBASE_THRESHOLD;

use crate::camera::CarCamera;
use crate::pilot::{Pilot, PlayerFocus};
use crate::terrain::TerrainChunk;

pub struct WorldSpacePlugin;

impl Plugin for WorldSpacePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<WorldOrigin>()
            .add_systems(Update, rebase_world);
    }
}

/// Re-centers the local coordinate frame on wherever the player currently
/// is (`PlayerFocus` — car, plane, or on foot, see that resource's own
/// docs) once it wanders far enough, moving every affected Transform by
/// the same amount so nothing visibly jumps. This is the classic "floating
/// origin" technique real large-world games (space sims especially) use to
/// dodge f32 precision loss far from the origin, rather than trying to
/// make the physics/render pipeline itself work in higher precision.
///
/// Cars, planes, villagers, and buildings are *not* in the shift list
/// below, deliberately: all four now fully recompute their `Transform`
/// every frame from a true-space replicated snapshot
/// (`car_render.rs`'s `sync_car_transforms`, `aircraft.rs`'s
/// `sync_plane_transforms`, `villager_render.rs`'s
/// `sync_villager_transform`, `building_render.rs`'s
/// `sync_building_transform`) rather than carrying a value forward from a
/// previous frame — the instant `origin.offset` changes for *any* reason,
/// anywhere (an ordinary threshold rebase here, or a one-time reset like
/// `pilot.rs`'s login handler or `terrain.rs`'s world-regen handler), that
/// recomputation already produces the correct new local position on its
/// own very next run, with no manual patching needed anywhere else. This
/// is the actual single source of truth for "where is this thing on
/// screen": true position minus current `WorldOrigin`, recomputed fresh,
/// always — not "wherever a shift list last remembered to move it."
///
/// Only genuinely persistent local state that has no true-space source to
/// re-derive from every frame — the camera's own (smoothed/lerped)
/// transform, streamed terrain chunks (expensive to rebuild, so shifted in
/// place instead), and the on-foot avatar (this client's sole local
/// authority over its own position) — still needs an explicit shift here.
/// A large one-time origin reset (login, world regen) instead wipes and
/// re-seeds terrain immediately and spawns the avatar fresh under the new
/// origin directly (see those modules), rather than routing through this
/// threshold-triggered incremental path at all.
fn rebase_world(
    mut origin: ResMut<WorldOrigin>,
    focus: Res<PlayerFocus>,
    mut camera_q: Query<&mut Transform, With<CarCamera>>,
    mut chunk_q: Query<&mut Transform, (With<TerrainChunk>, Without<CarCamera>)>,
    mut pilot_q: Query<&mut Transform, (With<Pilot>, Without<CarCamera>, Without<TerrainChunk>)>,
) {
    let local = focus.translation;
    if local.x.abs() < REBASE_THRESHOLD && local.z.abs() < REBASE_THRESHOLD {
        return;
    }

    // Only shift horizontally — altitude never grows large enough (terrain
    // height is bounded) to need rebasing, and leaving it alone means one
    // less thing to keep in sync.
    let shift = Vec3::new(local.x, 0.0, local.z);
    origin.offset += shift.as_dvec3();

    for mut tf in &mut camera_q {
        tf.translation -= shift;
    }
    for mut tf in &mut chunk_q {
        tf.translation -= shift;
    }
    for mut tf in &mut pilot_q {
        tf.translation -= shift;
    }
}
