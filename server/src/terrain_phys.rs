use bevy::math::DVec3;
use bevy::prelude::*;
use bevy_rapier3d::prelude::*;
use shared::obstacles::{obstacles_for_chunk, ObstacleKind};
use shared::terrain_gen::{height_at, ChunkCoord, TerrainNoise, CHUNK_RESOLUTION, CHUNK_SIZE};
use shared::worldspace::WorldOrigin;

/// Server-side terrain: physics colliders only, built from the same
/// `shared::terrain_gen::height_at` the client's visual mesh samples, so
/// server and client agree on collision shape byte-for-byte without ever
/// sending heightfield data over the network — only the noise seed needs to
/// match (kept in sync across a regen via `WorldRegenMsg`, see car_sim.rs).
///
/// Unlike the client's chunk streaming (client's terrain.rs), this preloads
/// one fixed-radius area around world origin once at startup rather than
/// dynamically streaming/unloading around each connected car's position.
/// Deliberate v1 simplification: multiplayer needed to ship without also
/// solving *multi-tracker* dynamic streaming (multiple cars, each
/// potentially wanting a different loaded area) in the same pass. Fine for
/// testing driving within a few km of spawn; revisit with a proper
/// multi-tracker version of client's stream_chunks if players want to roam
/// far from spawn or from each other.
const PRELOAD_RADIUS_CHUNKS: i64 = 6;

pub struct ServerTerrainPlugin;

impl Plugin for ServerTerrainPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<TerrainNoise>()
            .init_resource::<WorldOrigin>()
            .add_systems(Startup, |mut commands: Commands, noise: Res<TerrainNoise>, origin: Res<WorldOrigin>| {
                let count = spawn_all_terrain_colliders(&mut commands, &noise, &origin);
                info!("server: preloaded {count} terrain chunk colliders");
            });
    }
}

/// Marks every entity `spawn_all_terrain_colliders` creates (terrain
/// chunks and obstacles alike), so a world regeneration can find and
/// despawn all of them before respawning fresh ones for the new seed.
#[derive(Component)]
pub struct ServerTerrainEntity;

/// Despawns every existing terrain/obstacle entity and spawns a fresh set
/// for the current `TerrainNoise`/`WorldOrigin` — used both for the initial
/// startup preload and for a world regeneration (car_sim.rs's
/// `apply_world_regen`), which reseeds `TerrainNoise` and resets
/// `WorldOrigin` first and then calls this to rebuild collision to match.
pub fn regenerate_terrain_colliders(
    commands: &mut Commands,
    noise: &TerrainNoise,
    origin: &WorldOrigin,
    existing: &Query<Entity, With<ServerTerrainEntity>>,
) {
    for entity in existing {
        commands.entity(entity).despawn();
    }
    let count = spawn_all_terrain_colliders(commands, noise, origin);
    info!("server: regenerated {count} terrain chunk colliders");
}

fn spawn_all_terrain_colliders(commands: &mut Commands, noise: &TerrainNoise, origin: &WorldOrigin) -> u32 {
    let mut count = 0;
    for x in -PRELOAD_RADIUS_CHUNKS..=PRELOAD_RADIUS_CHUNKS {
        for z in -PRELOAD_RADIUS_CHUNKS..=PRELOAD_RADIUS_CHUNKS {
            spawn_chunk_collider(commands, noise, origin, (x, z));
            count += 1;
        }
    }
    count
}

fn spawn_chunk_collider(
    commands: &mut Commands,
    noise: &TerrainNoise,
    origin: &WorldOrigin,
    coord: ChunkCoord,
) {
    let n = CHUNK_RESOLUTION;
    let size = CHUNK_SIZE;
    let true_center = DVec3::new(coord.0 as f64 * size as f64, 0.0, coord.1 as f64 * size as f64);
    let local_center = (true_center - origin.offset).as_vec3();

    // Sample layout must match parry3d's HeightField3 exactly, and must
    // match the client's build_chunk_mesh sample layout too (see that
    // function's comment) so collision shape and rendered shape agree.
    let x_at = |j: usize| (-0.5 + j as f32 / (n - 1) as f32) * size;
    let z_at = |i: usize| (-0.5 + i as f32 / (n - 1) as f32) * size;

    let mut heights = Vec::with_capacity(n * n);
    for j in 0..n {
        let x = true_center.x + x_at(j) as f64;
        for i in 0..n {
            let z = true_center.z + z_at(i) as f64;
            heights.push(height_at(noise, x, z));
        }
    }

    commands.spawn((
        Transform::from_translation(local_center),
        RigidBody::Fixed,
        Collider::heightfield(heights, n, n, Vec3::new(size, 1.0, size)),
        Friction::coefficient(1.0),
        ServerTerrainEntity,
    ));

    // Same deterministic placement the client computes independently (see
    // shared::obstacles docs) — collider dimensions bake in `spec.scale`
    // directly rather than via `Transform::scale` (which the client relies
    // on Rapier's own `apply_scale` system for), since the server has no
    // other use for a rendering-shaped Transform on these.
    for spec in obstacles_for_chunk(noise, coord) {
        let local_x = (spec.true_x - origin.offset.x) as f32;
        let local_z = (spec.true_z - origin.offset.z) as f32;
        let ground_y = height_at(noise, spec.true_x, spec.true_z);
        let translation = Vec3::new(local_x, ground_y, local_z);
        let rotation = Quat::from_rotation_y(spec.rotation_y);

        match spec.kind {
            ObstacleKind::Rock => {
                commands.spawn((
                    Transform::from_translation(translation).with_rotation(rotation),
                    RigidBody::Fixed,
                    Collider::ball(0.8 * spec.scale),
                    Friction::coefficient(1.0),
                    ServerTerrainEntity,
                ));
            }
            ObstacleKind::Tree => {
                let trunk_half_height = 1.5 * spec.scale;
                commands.spawn((
                    Transform::from_translation(translation).with_rotation(rotation),
                    RigidBody::Fixed,
                    // Trunk-only collision, matching the client's visual —
                    // see that side's comment on why the canopy isn't solid.
                    Collider::cylinder(trunk_half_height, 0.25 * spec.scale),
                    Friction::coefficient(1.0),
                    ServerTerrainEntity,
                ));
            }
        }
    }
}
