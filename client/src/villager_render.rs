use bevy::prelude::*;
use shared::protocol::VillagerSnapshot;
use shared::terrain_gen::{height_at, TerrainNoise};

use crate::worldspace::WorldOrigin;

/// Renders every replicated `VillagerSnapshot` — every player's, not just
/// the local one, same "purely cosmetic, driven off replicated data"
/// relationship `car_render.rs`/`building_render.rs` already have to their
/// own snapshot types. A glowing orb is a deliberate v1 placeholder visual
/// (the user's own words) for what's meant to eventually be a real
/// gathering unit — a gentle bob is the only embellishment, to read as
/// "alive" rather than a static prop.
pub struct VillagerRenderPlugin;

impl Plugin for VillagerRenderPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(init_villager_visuals)
            .add_systems(Update, sync_villager_transform);
    }
}

const ORB_RADIUS: f32 = 0.6;
/// How far above the ground it hovers, before the bob offset.
const HOVER_HEIGHT: f32 = 1.5;
const ORB_COLOR: Color = Color::srgb(0.5, 0.9, 1.0);
const BOB_AMPLITUDE: f32 = 0.15;
const BOB_HZ: f32 = 0.5;

#[derive(Component)]
struct VillagerOrb {
    bob_phase: f32,
}

fn init_villager_visuals(
    insert: On<Insert, VillagerSnapshot>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    snapshots: Query<&VillagerSnapshot>,
    noise: Res<TerrainNoise>,
    origin: Res<WorldOrigin>,
) {
    let Ok(snapshot) = snapshots.get(insert.entity) else {
        return;
    };
    let ground_y = height_at(&noise, snapshot.true_x, snapshot.true_z);
    let local = (bevy::math::DVec3::new(snapshot.true_x, 0.0, snapshot.true_z) - origin.offset).as_vec3();

    commands.entity(insert.entity).insert((
        Mesh3d(meshes.add(Sphere::new(ORB_RADIUS))),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: ORB_COLOR,
            emissive: LinearRgba::rgb(1.0, 3.5, 4.0),
            unlit: true,
            ..default()
        })),
        Transform::from_xyz(local.x, ground_y + HOVER_HEIGHT, local.z),
        PointLight {
            color: ORB_COLOR,
            intensity: 40_000.0,
            range: 8.0,
            shadow_maps_enabled: false,
            ..default()
        },
        VillagerOrb { bob_phase: (snapshot.true_x + snapshot.true_z) as f32 },
    ));
}

/// Re-samples ground height every frame as the villager wanders (unlike a
/// static building, its x/z genuinely changes over time, so a height
/// baked in once at spawn would have it floating or clipping underground
/// within a few seconds).
fn sync_villager_transform(
    time: Res<Time>,
    noise: Res<TerrainNoise>,
    origin: Res<WorldOrigin>,
    mut villagers_q: Query<(&VillagerSnapshot, &VillagerOrb, &mut Transform)>,
) {
    let t = time.elapsed_secs();
    for (snapshot, orb, mut transform) in &mut villagers_q {
        let ground_y = height_at(&noise, snapshot.true_x, snapshot.true_z);
        let local = (bevy::math::DVec3::new(snapshot.true_x, 0.0, snapshot.true_z) - origin.offset).as_vec3();
        let bob = (t * BOB_HZ * std::f32::consts::TAU + orb.bob_phase).sin() * BOB_AMPLITUDE;
        transform.translation = Vec3::new(local.x, ground_y + HOVER_HEIGHT + bob, local.z);
    }
}
