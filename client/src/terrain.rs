use bevy::asset::RenderAssetUsages;
use bevy::math::DVec3;
use bevy::mesh::Indices;
use bevy::platform::collections::HashMap;
use bevy::prelude::*;
use bevy::render::render_resource::PrimitiveTopology;
use bevy_rapier3d::prelude::*;
use bevy_replicon::prelude::ClientTriggerExt;
use shared::deposits::deposit_for_chunk;
use shared::obstacles::{obstacles_for_chunk, ObstacleKind};
use shared::protocol::{RegenRequestMsg, WorldRegenMsg};
use shared::terrain_gen::{
    climate_at, detail_at, height_at, slope_at, terrain_color, world_to_chunk, ChunkCoord,
    TerrainNoise, CHUNK_RESOLUTION, CHUNK_SIZE,
};

use crate::pilot::PlayerFocus;
use crate::terrain_material::TerrainMaterial;
use crate::worldspace::WorldOrigin;

// Chunked, streamed terrain: chunks spawn/despawn around `PlayerFocus`
// (wherever the player currently is — car, plane, or on foot), not a
// single hardcoded entity. The height/color/climate math itself
// lives in `shared::terrain_gen` as pure functions of true (world-origin-
// relative) (x, z) — that's what lets chunk edges agree exactly with no
// stitching, content survive a `WorldOrigin` rebase unchanged, and (once
// the server exists) client and server independently regenerate identical
// terrain from just a shared seed instead of sending mesh/collider data
// over the network. This module is just the client-side streaming +
// visual-mesh half of that; the noise/height functions themselves are in
// `shared`.
const CHUNKS_SPAWNED_PER_FRAME: usize = 3;

/// How many chunks out `stream_chunks` keeps loaded around the player —
/// live-adjustable (see `settings.rs`'s view-distance slider), unlike the
/// fixed radius this used to be. Obstacles (rocks/trees) are spawned as
/// children of their own chunk (see `spawn_chunk`), so this one setting
/// already controls *their* streamed-in range too — there's no separate
/// "object view distance" to wire up. Changing this needs no special
/// handling anywhere: `stream_chunks` and its "stale" cleanup both already
/// re-read this fresh every frame, so raising it just naturally queues
/// more chunks over the next few frames and lowering it naturally unloads
/// whatever's now outside the new radius, the exact same as when the
/// player's *position* changes.
#[derive(Resource)]
pub struct ViewDistance {
    pub chunks: i64,
}

impl Default for ViewDistance {
    fn default() -> Self {
        Self { chunks: 4 }
    }
}

impl ViewDistance {
    /// A little wider than `chunks` so a chunk right at the edge of load
    /// range doesn't thrash in and out every frame — same margin the old
    /// fixed `UNLOAD_RADIUS_CHUNKS` constant used relative to
    /// `LOAD_RADIUS_CHUNKS`.
    fn unload_radius(&self) -> i64 {
        self.chunks + 1
    }
}

pub struct TerrainPlugin;

impl Plugin for TerrainPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<TerrainNoise>()
            .init_resource::<LoadedChunks>()
            .init_resource::<ViewDistance>()
            .add_message::<RegenerateWorldEvent>()
            .add_observer(apply_world_regen)
            .add_systems(Startup, spawn_initial_chunks)
            .add_systems(Update, (regenerate_terrain, stream_chunks));
    }
}

// Marker only — which coord an entity is lives in `LoadedChunks.chunks`,
// so despawning by coord never needs to query this back out. `pub` because
// worldspace.rs needs to shift every loaded chunk's Transform on rebase.
#[derive(Component)]
pub struct TerrainChunk;

// pub(crate): pilot.rs's login handler needs to wipe and immediately
// re-seed this around the real spawn point (see `respawn_chunks_immediate`)
// — the `Startup` batch below is spawned around true chunk (0,0), which is
// only correct if the player's actual spawn point also happens to be near
// true (0,0).
#[derive(Resource, Default)]
pub(crate) struct LoadedChunks {
    chunks: HashMap<ChunkCoord, Entity>,
    pending: Vec<ChunkCoord>,
}

/// N: reseed the world with a fresh random terrain and snap the car back to
/// (the new) spawn. Consumed by car.rs's reset_car, which does the actual
/// repositioning (and in turn fires CarResetEvent so the camera snaps too).
#[derive(Message)]
pub struct RegenerateWorldEvent;

fn build_chunk_mesh(
    noise: &TerrainNoise,
    origin: &WorldOrigin,
    coord: ChunkCoord,
) -> (Mesh, Vec<f32>, Vec3) {
    let n = CHUNK_RESOLUTION;
    let size = CHUNK_SIZE;
    // True (f64) position of this chunk's center — chunk identity and its
    // sampled content never change, no matter how many times the world has
    // rebased. Only the *rendered* position (local_center, f32) depends on
    // the current origin.
    let true_center = DVec3::new(coord.0 as f64 * size as f64, 0.0, coord.1 as f64 * size as f64);
    let local_center = (true_center - origin.offset).as_vec3();
    let cell = size / (n - 1) as f32;

    // Local x_at(j)/z_at(i) match parry3d's HeightField3 sample layout
    // exactly (see Collider::heightfield docs), so the visual mesh and the
    // physics collider agree on every vertex.
    let x_at = |j: usize| (-0.5 + j as f32 / (n - 1) as f32) * size;
    let z_at = |i: usize| (-0.5 + i as f32 / (n - 1) as f32) * size;

    let mut heightfield_heights = Vec::with_capacity(n * n);
    for j in 0..n {
        let x = true_center.x + x_at(j) as f64;
        for i in 0..n {
            let z = true_center.z + z_at(i) as f64;
            heightfield_heights.push(height_at(noise, x, z));
        }
    }

    let mut positions = Vec::with_capacity(n * n);
    let mut normals: Vec<[f32; 3]> = Vec::with_capacity(n * n);
    let mut uvs = Vec::with_capacity(n * n);
    let mut colors = Vec::with_capacity(n * n);

    for i in 0..n {
        for j in 0..n {
            let local_x = x_at(j);
            let local_z = z_at(i);
            let world_x = true_center.x + local_x as f64;
            let world_z = true_center.z + local_z as f64;
            let y = heightfield_heights[i + j * n];

            let normal = slope_at(noise, world_x, world_z, cell * 0.5);
            let slope = 1.0 - normal.y;
            let detail = detail_at(noise, world_x, world_z);
            let (temperature, moisture) = climate_at(noise, world_x, world_z);

            positions.push([local_x, y, local_z]);
            normals.push(normal.into());
            uvs.push([(world_x / 8.0) as f32, (world_z / 8.0) as f32]);
            colors.push(
                terrain_color(y, slope, detail, temperature, moisture)
                    .to_srgba()
                    .to_f32_array(),
            );
        }
    }

    let idx = |i: usize, j: usize| (i * n + j) as u32;
    let mut indices = Vec::with_capacity((n - 1) * (n - 1) * 6);
    for i in 0..n - 1 {
        for j in 0..n - 1 {
            let a = idx(i, j);
            let b = idx(i, j + 1);
            let c = idx(i + 1, j);
            let d = idx(i + 1, j + 1);
            indices.extend_from_slice(&[a, c, b, b, c, d]);
        }
    }

    let mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
    )
    .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
    .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
    .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, uvs)
    .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, colors)
    .with_inserted_indices(Indices::U32(indices));

    (mesh, heightfield_heights, local_center)
}

fn spawn_chunk(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    terrain_material: Handle<TerrainMaterial>,
    noise: &TerrainNoise,
    origin: &WorldOrigin,
    coord: ChunkCoord,
) -> Entity {
    let (mesh, heights, local_center) = build_chunk_mesh(noise, origin, coord);
    let true_center = DVec3::new(
        coord.0 as f64 * CHUNK_SIZE as f64,
        0.0,
        coord.1 as f64 * CHUNK_SIZE as f64,
    );
    let obstacles = obstacles_for_chunk(noise, coord);
    let deposit = deposit_for_chunk(coord);

    commands
        .spawn((
            Mesh3d(meshes.add(mesh)),
            MeshMaterial3d(terrain_material),
            Transform::from_translation(local_center),
            RigidBody::Fixed,
            Collider::heightfield(
                heights,
                CHUNK_RESOLUTION,
                CHUNK_RESOLUTION,
                Vec3::new(CHUNK_SIZE, 1.0, CHUNK_SIZE),
            ),
            Friction::coefficient(1.0),
            TerrainChunk,
        ))
        .with_children(|parent| {
            // Children of the chunk entity so they stream in/out and
            // despawn together with it — obstacle placement itself is a
            // pure function of chunk coord (shared::obstacles), so this
            // never needs to be reconciled with the server; both sides
            // compute the identical set independently.
            //
            // `spec`'s position is true-space; the child's own `Transform`
            // must be chunk-local (the parent chunk's Transform already
            // carries `local_center`), hence subtracting `true_center`
            // here rather than `origin.offset` — matches the same local
            // coordinate convention `build_chunk_mesh`'s vertices use.
            for spec in &obstacles {
                let local_x = (spec.true_x - true_center.x) as f32;
                let local_z = (spec.true_z - true_center.z) as f32;
                let ground_y = height_at(noise, spec.true_x, spec.true_z);

                let transform = Transform::from_xyz(local_x, ground_y, local_z)
                    .with_rotation(Quat::from_rotation_y(spec.rotation_y))
                    .with_scale(Vec3::splat(spec.scale));

                match spec.kind {
                    ObstacleKind::Rock => {
                        parent.spawn((
                            Mesh3d(meshes.add(Sphere::new(0.8))),
                            MeshMaterial3d(materials.add(StandardMaterial {
                                base_color: Color::srgb(0.38, 0.37, 0.35),
                                perceptual_roughness: 0.95,
                                ..default()
                            })),
                            transform,
                            RigidBody::Fixed,
                            Collider::ball(0.8),
                            Friction::coefficient(1.0),
                        ));
                    }
                    ObstacleKind::Tree => {
                        let trunk_half_height = 1.5;
                        parent
                            .spawn((
                                Mesh3d(meshes.add(Cylinder::new(0.25, trunk_half_height * 2.0))),
                                MeshMaterial3d(materials.add(StandardMaterial {
                                    base_color: Color::srgb(0.32, 0.22, 0.14),
                                    perceptual_roughness: 0.9,
                                    ..default()
                                })),
                                transform,
                                RigidBody::Fixed,
                                // Trunk-only collision — you can't
                                // realistically drive through a tree trunk,
                                // but colliding with the leafy canopy above
                                // isn't worth modeling.
                                Collider::cylinder(trunk_half_height, 0.25),
                                Friction::coefficient(1.0),
                            ))
                            .with_children(|trunk| {
                                trunk.spawn((
                                    Mesh3d(meshes.add(Sphere::new(1.1))),
                                    MeshMaterial3d(materials.add(StandardMaterial {
                                        base_color: Color::srgb(0.16, 0.38, 0.18),
                                        perceptual_roughness: 0.9,
                                        ..default()
                                    })),
                                    Transform::from_xyz(0.0, trunk_half_height + 0.7, 0.0),
                                ));
                            });
                    }
                }
            }

            // A visible in-world beacon at a deposit — purely cosmetic
            // (no collider, cars drive straight through it), replacing an
            // earlier text-only "Xm away" HUD hint with something you can
            // actually see and drive toward while exploring. Streams
            // in/out with the chunk exactly like obstacles, for the same
            // "pure function of chunk coord, never replicated" reason.
            if let Some(spec) = deposit {
                let local_x = (spec.true_x - true_center.x) as f32;
                let local_z = (spec.true_z - true_center.z) as f32;
                let ground_y = height_at(noise, spec.true_x, spec.true_z);
                const BEACON_HEIGHT: f32 = 18.0;
                const BEACON_COLOR: Color = Color::srgb(1.0, 0.75, 0.25);

                parent.spawn((
                    Mesh3d(meshes.add(Cylinder::new(0.35, BEACON_HEIGHT))),
                    MeshMaterial3d(materials.add(StandardMaterial {
                        base_color: BEACON_COLOR,
                        emissive: LinearRgba::rgb(3.0, 2.0, 0.3),
                        alpha_mode: AlphaMode::Blend,
                        unlit: true,
                        ..default()
                    })),
                    Transform::from_xyz(local_x, ground_y + BEACON_HEIGHT * 0.5, local_z),
                    PointLight {
                        color: BEACON_COLOR,
                        intensity: 200_000.0,
                        range: 40.0,
                        shadow_maps_enabled: false,
                        ..default()
                    },
                ));
            }
        })
        .id()
}

/// Spawns a full `LOAD_RADIUS_CHUNKS` grid immediately (not trickled via
/// `stream_chunks`'s per-frame budget) around `center`. Shared by the
/// `Startup` pre-login backdrop (`spawn_initial_chunks`, centered on true
/// chunk (0,0)) and `respawn_chunks_immediate` (centered on wherever the
/// player actually spawns) — both need "solid ground right now," not a
/// multi-second trickle-in.
#[allow(clippy::too_many_arguments)]
fn spawn_chunk_grid(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    terrain_material: Handle<TerrainMaterial>,
    noise: &TerrainNoise,
    origin: &WorldOrigin,
    loaded: &mut LoadedChunks,
    view_distance: &ViewDistance,
    center: ChunkCoord,
) {
    for x in -view_distance.chunks..=view_distance.chunks {
        for z in -view_distance.chunks..=view_distance.chunks {
            let coord = (center.0 + x, center.1 + z);
            let entity = spawn_chunk(commands, meshes, materials, terrain_material.clone(), noise, origin, coord);
            loaded.chunks.insert(coord, entity);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_initial_chunks(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut terrain_materials: ResMut<Assets<TerrainMaterial>>,
    noise: Res<TerrainNoise>,
    origin: Res<WorldOrigin>,
    mut loaded: ResMut<LoadedChunks>,
    view_distance: Res<ViewDistance>,
) {
    let terrain_material = terrain_materials.add(TerrainMaterial {
        base: StandardMaterial {
            base_color: Color::WHITE,
            perceptual_roughness: 0.95,
            ..default()
        },
        extension: default(),
    });

    spawn_chunk_grid(
        &mut commands,
        &mut meshes,
        &mut materials,
        terrain_material,
        &noise,
        &origin,
        &mut loaded,
        &view_distance,
        (0, 0),
    );
}

/// Wipes every currently-loaded chunk (the `Startup` batch spawned around
/// true chunk (0,0), stale the instant the player's actual spawn point is
/// anywhere else) and immediately spawns a fresh grid around `origin`'s new
/// offset — called once at login, right after `origin.offset` is set to the
/// server-assigned true spawn position (see `pilot.rs`'s
/// `spawn_pilot_after_login`).
///
/// Immediate, not trickled through `stream_chunks`: the on-foot avatar
/// spawns this same frame with a real Rapier collider dependency (unlike a
/// car/plane, which are purely server-authoritative and don't need a local
/// collider under them) — leaving it to `stream_chunks`'s
/// `CHUNKS_SPAWNED_PER_FRAME` budget would mean falling through empty air
/// for the many frames it takes to reach the chunk actually under the
/// avatar's feet.
#[allow(clippy::too_many_arguments)]
pub(crate) fn respawn_chunks_immediate(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    terrain_materials: &mut Assets<TerrainMaterial>,
    noise: &TerrainNoise,
    origin: &WorldOrigin,
    loaded: &mut LoadedChunks,
    view_distance: &ViewDistance,
) {
    for (_, entity) in loaded.chunks.drain() {
        commands.entity(entity).despawn();
    }
    loaded.pending.clear();

    let terrain_material = terrain_materials.add(TerrainMaterial {
        base: StandardMaterial {
            base_color: Color::WHITE,
            perceptual_roughness: 0.95,
            ..default()
        },
        extension: default(),
    });
    let center = world_to_chunk(origin.offset);
    spawn_chunk_grid(commands, meshes, materials, terrain_material, noise, origin, loaded, view_distance, center);
}

/// N: request a world regeneration. Regeneration is now server-authoritative
/// and op-gated (see the multiplayer plan's admin/player model) — pressing
/// N no longer does anything by itself, it just asks. If the server denies
/// the request (the player isn't op), nothing visibly happens; if approved,
/// `apply_world_regen` below does the actual work once the server's
/// `WorldRegenMsg` comes back — the same single code path a *console*-
/// triggered regen or another op player's regen also goes through, so this
/// client's terrain can never desync from what the server actually decided.
fn regenerate_terrain(
    keyboard: Res<ButtonInput<KeyCode>>,
    chat_open: Res<crate::chat::ChatOpen>,
    mut commands: Commands,
) {
    if chat_open.0 || !keyboard.just_pressed(KeyCode::KeyN) {
        return;
    }
    commands.client_trigger(RegenRequestMsg);
}

/// Applies a server-confirmed world regeneration: reseeds terrain, wipes
/// and re-streams every loaded chunk, resets the floating origin, and hands
/// off to car.rs's `reset_car` (via `RegenerateWorldEvent`) to reposition
/// the local car onto the new terrain.
fn apply_world_regen(
    regen: On<WorldRegenMsg>,
    mut commands: Commands,
    mut noise: ResMut<TerrainNoise>,
    mut loaded: ResMut<LoadedChunks>,
    mut origin: ResMut<WorldOrigin>,
    mut regenerate_events: MessageWriter<RegenerateWorldEvent>,
) {
    *noise = TerrainNoise::from_seed(regen.seed);
    for (_, entity) in loaded.chunks.drain() {
        commands.entity(entity).despawn();
    }
    loaded.pending.clear();
    origin.offset = DVec3::new(regen.origin_x, 0.0, regen.origin_z);
    regenerate_events.write(RegenerateWorldEvent);
    info!("client: world regenerated (seed={})", regen.seed);
}

#[allow(clippy::too_many_arguments)]
fn stream_chunks(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut terrain_materials: ResMut<Assets<TerrainMaterial>>,
    noise: Res<TerrainNoise>,
    origin: Res<WorldOrigin>,
    mut loaded: ResMut<LoadedChunks>,
    focus: Res<PlayerFocus>,
    view_distance: Res<ViewDistance>,
    existing_material: Query<&MeshMaterial3d<TerrainMaterial>, With<TerrainChunk>>,
) {
    let true_pos = origin.to_true(focus.translation);
    let center_chunk = world_to_chunk(true_pos);

    let dist2 = |c: ChunkCoord| {
        let dx = c.0 - center_chunk.0;
        let dz = c.1 - center_chunk.1;
        dx * dx + dz * dz
    };

    // Queue up newly-needed chunks (nearest first) without spawning them
    // all in one frame — a full radius of new chunks appearing at once
    // (e.g. after a fast respawn, or the player just raising view distance
    // in `settings.rs`) would be a visible hitch.
    let mut wanted = Vec::new();
    for x in -view_distance.chunks..=view_distance.chunks {
        for z in -view_distance.chunks..=view_distance.chunks {
            let coord = (center_chunk.0 + x, center_chunk.1 + z);
            if !loaded.chunks.contains_key(&coord) && !loaded.pending.contains(&coord) {
                wanted.push(coord);
            }
        }
    }
    wanted.sort_by_key(|c| dist2(*c));
    loaded.pending.extend(wanted);
    loaded.pending.sort_by_key(|c| dist2(*c));

    let terrain_material = existing_material
        .iter()
        .next()
        .map(|m| m.0.clone())
        .unwrap_or_else(|| {
            terrain_materials.add(TerrainMaterial {
                base: StandardMaterial {
                    base_color: Color::WHITE,
                    perceptual_roughness: 0.95,
                    ..default()
                },
                extension: default(),
            })
        });

    let spawn_count = loaded.pending.len().min(CHUNKS_SPAWNED_PER_FRAME);
    let to_spawn: Vec<ChunkCoord> = loaded.pending.drain(..spawn_count).collect();
    for coord in to_spawn {
        let entity = spawn_chunk(
            &mut commands,
            &mut meshes,
            &mut materials,
            terrain_material.clone(),
            &noise,
            &origin,
            coord,
        );
        loaded.chunks.insert(coord, entity);
    }

    let stale: Vec<ChunkCoord> = loaded
        .chunks
        .keys()
        .filter(|coord| {
            let dx = coord.0 - center_chunk.0;
            let dz = coord.1 - center_chunk.1;
            dx.abs() > view_distance.unload_radius() || dz.abs() > view_distance.unload_radius()
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

