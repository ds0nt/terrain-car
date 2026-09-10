use std::collections::HashMap;

use bevy::math::DVec3;
use bevy::prelude::*;
use bevy_rapier3d::prelude::*;
use shared::obstacles::{obstacles_for_chunk, ObstacleKind};
use shared::terrain_gen::{
    height_at, world_to_chunk, ChunkCoord, TerrainNoise, CHUNK_RESOLUTION, CHUNK_SIZE,
};
use shared::worldspace::WorldOrigin;

use crate::car_sim::PlayerPositions;

/// Server-side terrain: physics colliders only, built from the same
/// `shared::terrain_gen::height_at` the client's visual mesh samples, so
/// server and client agree on collision shape byte-for-byte without ever
/// sending heightfield data over the network — only the noise seed needs to
/// match (kept in sync across a regen via `WorldRegenMsg`, see car_sim.rs).
///
/// Streams dynamically around every connected *player's* own position (see
/// `stream_terrain_chunks`, keyed off `PlayerPositions` — car, plane,
/// on-foot, or passenger, whichever a given player currently is) — a
/// genuine multi-tracker version of the client's single-tracker
/// `client::terrain::stream_chunks`, one radius per player, unioned
/// together. Used to be a single fixed-radius preload around world origin
/// at startup and never touched again; that meant a car that ever drove
/// more than `LOAD_RADIUS_CHUNKS` chunks from spawn (easy now that `Ramp`s
/// launch you a long way) hit terrain with no collider under it at all and
/// fell straight through into the void, even though the client's own
/// terrain — which *does* stream dynamically — kept rendering normally the
/// whole time, giving no visual warning anything was wrong.
///
/// Was keyed off `Query<&Transform, With<CarChassis>>` alone for a while
/// after that fix — which quietly reintroduced the identical bug for a
/// *plane*: a plane is a real Rapier body (`aircraft.rs`'s `fly_planes`
/// applies force/torque to it same as a car), so flying it far enough from
/// wherever its owner's car happened to be parked meant no terrain
/// collider existed under the flight path at all — the plane would just
/// physically pass through a mountain with zero collision response,
/// reported live as "terrain stops having collision... from the airplane,"
/// while the client's own terrain kept rendering fine the whole time
/// (rendering and physics are entirely separate systems — see this
/// project's other floating-origin/streaming docs). `PlayerPositions`
/// already tracks every connected player's position regardless of what
/// they're currently in (`client::pilot`'s `send_player_position` reports
/// it every tick unconditionally on `ControlMode`), so keying off that
/// instead covers car, plane, on-foot, and passenger alike with no risk of
/// a future vehicle kind quietly repeating this same class of bug again.
const LOAD_RADIUS_CHUNKS: i64 = 6;
/// A little wider than `LOAD_RADIUS_CHUNKS` so a car sitting right at the
/// edge of its load radius doesn't thrash a chunk in and out every tick.
const UNLOAD_RADIUS_CHUNKS: i64 = 8;
/// Spawning every newly-needed chunk in one tick (e.g. after a world regen
/// or a Hangar recall drops a car somewhere entirely new) would be a real
/// physics-tick hitch — same reasoning as the client's own
/// `CHUNKS_SPAWNED_PER_FRAME`, just budgeted for a 64Hz tick instead of a
/// render frame.
const CHUNKS_SPAWNED_PER_TICK: usize = 4;

pub struct ServerTerrainPlugin;

impl Plugin for ServerTerrainPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<TerrainNoise>()
            .init_resource::<WorldOrigin>()
            .init_resource::<LoadedChunks>()
            .add_systems(
                Startup,
                |mut commands: Commands,
                 noise: Res<TerrainNoise>,
                 origin: Res<WorldOrigin>,
                 mut loaded: ResMut<LoadedChunks>| {
                    let count = preload_around_origin(&mut commands, &noise, &origin, &mut loaded);
                    info!("server: preloaded {count} terrain chunk colliders");
                },
            )
            .add_systems(Update, stream_terrain_chunks);
    }
}

/// Marks every top-level entity `spawn_chunk` creates — just the terrain
/// heightfield itself now (obstacles are its children, see `spawn_chunk`,
/// and despawn along with it automatically), so a world regeneration can
/// find and despawn all of them before respawning fresh ones for the new
/// seed.
#[derive(Component)]
pub struct ServerTerrainEntity;

/// Which chunk coords currently have a live collider, plus a queue of
/// coords that are wanted but not spawned yet (see `stream_terrain_chunks`
/// on why spawning is throttled rather than immediate).
#[derive(Resource, Default)]
pub struct LoadedChunks {
    chunks: HashMap<ChunkCoord, Entity>,
    pending: Vec<ChunkCoord>,
}

/// Despawns every existing terrain/obstacle entity and spawns a fresh
/// preload around the current `WorldOrigin` — used for a world
/// regeneration (car_sim.rs's `apply_world_regen`), which reseeds
/// `TerrainNoise` and resets `WorldOrigin` first and then calls this to
/// rebuild collision to match. Ordinary dynamic streaming
/// (`stream_terrain_chunks`) takes back over the very next tick.
pub fn regenerate_terrain_colliders(
    commands: &mut Commands,
    noise: &TerrainNoise,
    origin: &WorldOrigin,
    loaded: &mut LoadedChunks,
) {
    for (_, entity) in loaded.chunks.drain() {
        commands.entity(entity).despawn();
    }
    loaded.pending.clear();
    let count = preload_around_origin(commands, noise, origin, loaded);
    info!("server: regenerated {count} terrain chunk colliders");
}

/// Centers on the chunk containing the *current* origin, not literal true
/// chunk (0,0) — chunk coordinates are true-space grid indices (see
/// shared::terrain_gen), so hard-coding (0,0) here meant that after any
/// world regen (which moves `origin.offset` by up to +/-10,000 units to
/// land somewhere new) every preloaded collider stayed exactly where it
/// was, while newly-spawned cars appeared near the *new* origin — up to
/// several thousand units outside any collider at all.
fn preload_around_origin(
    commands: &mut Commands,
    noise: &TerrainNoise,
    origin: &WorldOrigin,
    loaded: &mut LoadedChunks,
) -> u32 {
    let (center_x, center_z) = world_to_chunk(origin.offset);
    let mut count = 0;
    for x in -LOAD_RADIUS_CHUNKS..=LOAD_RADIUS_CHUNKS {
        for z in -LOAD_RADIUS_CHUNKS..=LOAD_RADIUS_CHUNKS {
            let coord = (center_x + x, center_z + z);
            let entity = spawn_chunk(commands, noise, origin, coord);
            loaded.chunks.insert(coord, entity);
            count += 1;
        }
    }
    count
}

/// Every connected player's own position — car, plane, on-foot, or
/// passenger, see this module's top-level docs — gets its own
/// `LOAD_RADIUS_CHUNKS` neighborhood kept loaded — several players far
/// apart each keep their own area collidable rather than only whoever's
/// nearest world origin. A chunk unloads only once it's outside
/// `UNLOAD_RADIUS_CHUNKS` of *every* player, not just the nearest one, so
/// two players who wander apart don't fight over the same chunk's
/// residency.
fn stream_terrain_chunks(
    mut commands: Commands,
    noise: Res<TerrainNoise>,
    origin: Res<WorldOrigin>,
    mut loaded: ResMut<LoadedChunks>,
    positions: Res<PlayerPositions>,
) {
    let center_chunks: Vec<ChunkCoord> = positions
        .positions()
        .map(|(true_x, true_z)| world_to_chunk(DVec3::new(true_x, 0.0, true_z)))
        .collect();
    if center_chunks.is_empty() {
        return;
    }

    let min_dist2 = |c: ChunkCoord| {
        center_chunks
            .iter()
            .map(|center| {
                let dx = c.0 - center.0;
                let dz = c.1 - center.1;
                dx * dx + dz * dz
            })
            .min()
            .unwrap_or(i64::MAX)
    };

    let mut wanted = Vec::new();
    for &center in &center_chunks {
        for x in -LOAD_RADIUS_CHUNKS..=LOAD_RADIUS_CHUNKS {
            for z in -LOAD_RADIUS_CHUNKS..=LOAD_RADIUS_CHUNKS {
                let coord = (center.0 + x, center.1 + z);
                if !loaded.chunks.contains_key(&coord)
                    && !loaded.pending.contains(&coord)
                    && !wanted.contains(&coord)
                {
                    wanted.push(coord);
                }
            }
        }
    }
    wanted.sort_by_key(|c| min_dist2(*c));
    loaded.pending.extend(wanted);
    loaded.pending.sort_by_key(|c| min_dist2(*c));

    let spawn_count = loaded.pending.len().min(CHUNKS_SPAWNED_PER_TICK);
    let to_spawn: Vec<ChunkCoord> = loaded.pending.drain(..spawn_count).collect();
    for coord in to_spawn {
        let entity = spawn_chunk(&mut commands, &noise, &origin, coord);
        loaded.chunks.insert(coord, entity);
    }

    let stale: Vec<ChunkCoord> = loaded
        .chunks
        .keys()
        .filter(|coord| {
            center_chunks.iter().all(|center| {
                (coord.0 - center.0).abs() > UNLOAD_RADIUS_CHUNKS
                    || (coord.1 - center.1).abs() > UNLOAD_RADIUS_CHUNKS
            })
        })
        .copied()
        .collect();
    for coord in stale {
        if let Some(entity) = loaded.chunks.remove(&coord) {
            commands.entity(entity).despawn();
        }
        loaded.pending.retain(|c| *c != coord);
    }
}

fn spawn_chunk(commands: &mut Commands, noise: &TerrainNoise, origin: &WorldOrigin, coord: ChunkCoord) -> Entity {
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

    commands
        .spawn((
            Transform::from_translation(local_center),
            RigidBody::Fixed,
            Collider::heightfield(heights, n, n, Vec3::new(size, 1.0, size)),
            Friction::coefficient(1.0),
            ServerTerrainEntity,
        ))
        .with_children(|parent| {
            // Children of the chunk entity, same as the client's own
            // `spawn_chunk` — they stream in/out and despawn together with
            // it (Bevy despawns a hierarchy recursively by default), so
            // unloading one stale chunk from `stream_terrain_chunks` never
            // needs to separately hunt down its obstacles by position.
            //
            // Same deterministic placement the client computes
            // independently (see shared::obstacles docs) — collider
            // dimensions bake in `spec.scale` directly rather than via
            // `Transform::scale` (which the client relies on Rapier's own
            // `apply_scale` system for), since the server has no other use
            // for a rendering-shaped Transform on these.
            for spec in obstacles_for_chunk(noise, coord) {
                let local_x = (spec.true_x - true_center.x) as f32;
                let local_z = (spec.true_z - true_center.z) as f32;
                let ground_y = height_at(noise, spec.true_x, spec.true_z);
                let translation = Vec3::new(local_x, ground_y, local_z);
                let rotation = Quat::from_rotation_y(spec.rotation_y);

                match spec.kind {
                    ObstacleKind::Rock => {
                        parent.spawn((
                            Transform::from_translation(translation).with_rotation(rotation),
                            RigidBody::Fixed,
                            Collider::ball(0.8 * spec.scale),
                            Friction::coefficient(1.0),
                        ));
                    }
                    ObstacleKind::Tree => {
                        let trunk_half_height = 1.5 * spec.scale;
                        parent.spawn((
                            Transform::from_translation(translation).with_rotation(rotation),
                            RigidBody::Fixed,
                            // Trunk-only collision, matching the client's visual —
                            // see that side's comment on why the canopy isn't solid.
                            Collider::cylinder(trunk_half_height, 0.25 * spec.scale),
                            Friction::coefficient(1.0),
                        ));
                    }
                }
            }
        })
        .id()
}
