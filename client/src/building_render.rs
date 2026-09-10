use std::f32::consts::TAU;

use bevy::math::DVec3;
use bevy::prelude::*;
use bevy_rapier3d::prelude::{Collider, Friction, RigidBody};
use shared::buildings::{self, BuildingKind, ColliderShape};
use shared::car_physics::CarChassis;
use shared::protocol::{BuildingSnapshot, CarCosmetics};
use shared::time::now_unix;

use crate::owner_color::color_from_seed;
use crate::worldspace::WorldOrigin;

/// Renders every replicated `BuildingSnapshot` — every player's, not just
/// the local one, same "purely cosmetic, driven off replicated data"
/// relationship `car_render.rs` has to `CarChassis`. Simple procedural
/// placeholder meshes per kind (no external texture/model assets, matching
/// this project's existing cosmetic style), tinted translucent while under
/// construction.
pub struct BuildingRenderPlugin;

impl Plugin for BuildingRenderPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(init_building_visuals)
            .add_systems(Update, (sync_building_transform, update_construction_tint));
    }
}

/// Mesh + base color + local-space transform for one building kind at a
/// given placement — the actual per-kind geometry, shared by the real
/// spawn (`init_building_visuals`) and the ghost preview
/// (`building_placement.rs`) so the ghost always looks/sits exactly like
/// what will actually be placed. `ground_y` and `rotation_y` come from the
/// caller (a live raycast for the ghost, the replicated snapshot for the
/// real thing).
/// The undecorated per-kind color — also used standalone by
/// `building_ui.rs`'s bottom build bar as each kind's icon swatch, so the
/// icon you click always matches what you actually place.
pub(crate) fn base_color_for_kind(kind: BuildingKind) -> Color {
    match kind {
        BuildingKind::Ramp => Color::srgb(0.5, 0.45, 0.4),
        BuildingKind::Road => Color::srgb(0.25, 0.25, 0.27),
        BuildingKind::Platform => Color::srgb(0.55, 0.5, 0.42),
        BuildingKind::Wall => Color::srgb(0.45, 0.45, 0.48),
        BuildingKind::Hangar => Color::srgb(0.45, 0.45, 0.5),
        BuildingKind::EnergyGenerator => Color::srgb(0.9, 0.8, 0.2),
        BuildingKind::ExtractionFacility => Color::srgb(0.75, 0.4, 0.2),
        BuildingKind::LandFactory => Color::srgb(0.3, 0.35, 0.32),
        BuildingKind::AirFactory => Color::srgb(0.35, 0.42, 0.5),
    }
}

/// Just the pose half of `building_mesh_and_transform` — pulled out on its
/// own so `sync_building_transform` can recompute a building's full
/// translation+rotation from scratch every frame without also having to
/// duplicate (and risk drifting from) the slab anchor-offset math in
/// `buildings::slab_transform`. A slab kind's translation isn't simply
/// `(local_x, ground_y, local_z)` — `slab_transform` adds a
/// rotation-dependent `half_length_offset` on top, so recomputing only
/// x/z from a fresh origin and leaving that offset out would visibly
/// mis-place every Ramp/Road/Platform/Wall the moment `WorldOrigin` ever
/// changes.
fn building_transform(kind: BuildingKind, local_x: f32, local_z: f32, ground_y: f32, rotation_y: f32) -> Transform {
    if kind.uses_slab_geometry() {
        let dims = buildings::slab_dims(kind);
        let (translation, rotation) = buildings::slab_transform(dims, local_x, local_z, ground_y, rotation_y);
        return Transform::from_translation(translation).with_rotation(rotation);
    }
    let shape = buildings::collider_shape(kind);
    Transform::from_xyz(local_x, ground_y + shape.half_height(), local_z).with_rotation(Quat::from_rotation_y(rotation_y))
}

pub(crate) fn building_mesh_and_transform(
    kind: BuildingKind,
    meshes: &mut Assets<Mesh>,
    local_x: f32,
    local_z: f32,
    ground_y: f32,
    rotation_y: f32,
) -> (Handle<Mesh>, Color, Transform) {
    let base_color = base_color_for_kind(kind);
    let transform = building_transform(kind, local_x, local_z, ground_y, rotation_y);
    if kind.uses_slab_geometry() {
        let dims = buildings::slab_dims(kind);
        let mesh = meshes.add(Cuboid::new(dims.half_width * 2.0, dims.half_height * 2.0, dims.half_length * 2.0));
        return (mesh, base_color, transform);
    }

    // Cuboid/Cylinder dimensions and the collider used to actually block a
    // car (see `init_building_visuals`) come from the exact same
    // `collider_shape` per kind — mesh and collider can never drift apart.
    let mesh = match buildings::collider_shape(kind) {
        ColliderShape::Cuboid { half_x, half_y, half_z } => {
            meshes.add(Cuboid::new(half_x * 2.0, half_y * 2.0, half_z * 2.0))
        }
        ColliderShape::Cylinder { half_height, radius } => {
            meshes.add(Cylinder::new(radius, half_height * 2.0))
        }
    };
    (mesh, base_color, transform)
}

fn init_building_visuals(
    insert: On<Insert, BuildingSnapshot>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    snapshots: Query<&BuildingSnapshot>,
    origin: Res<WorldOrigin>,
    cars: Query<(&CarChassis, &CarCosmetics)>,
) {
    let Ok(snapshot) = snapshots.get(insert.entity) else {
        return;
    };
    // Uses the snapshot's own ground_y (the server's actual placement-time
    // surface raycast) rather than re-deriving via height_at — re-deriving
    // from raw terrain was exactly the bug that made a Ramp placed on top
    // of another Ramp render back down at ground level despite its real
    // collider staying correctly elevated (see ground_y's own docs).
    let local = (DVec3::new(snapshot.true_x, 0.0, snapshot.true_z) - origin.offset).as_vec3();
    let (mesh, base_color, transform) = building_mesh_and_transform(
        snapshot.kind,
        &mut meshes,
        local.x,
        local.z,
        snapshot.ground_y,
        snapshot.rotation_y,
    );
    // Blended (not replaced) with the owner's color — a Hangar still
    // reads as "a Hangar" at a glance, just visibly tinted by whose it
    // is, same idea as `villager_render.rs`'s orb tint.
    let owner_color = crate::owner_color::color_for_owner(snapshot.owner_player_id);
    let base_color = base_color.mix(&owner_color, 0.5);

    // The owner's *actual* car paint — their custom color if they've set
    // one (`cosmetics_ui.rs`), else the same automatic owner-hash color
    // `owner_color` above already is. Falls back to `owner_color` itself
    // when the owner's car hasn't replicated to this client yet (e.g. a
    // building loaded from persistence before its owner reconnects) —
    // this only ever drives the side-logo tint below, never the body
    // color, so a stale fallback here is a color mismatch, never a
    // missing/broken building.
    let car_color = cars
        .iter()
        .find(|(chassis, _)| chassis.owner_player_id == snapshot.owner_player_id)
        .map(|(chassis, cosmetics)| {
            cosmetics
                .custom_color
                .map(|c| Color::srgb(c[0], c[1], c[2]))
                .unwrap_or_else(|| color_from_seed(chassis.color_seed))
        })
        .unwrap_or(owner_color);

    let mut entity = commands.entity(insert.entity);
    entity.insert((
        Mesh3d(mesh),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color,
            alpha_mode: AlphaMode::Blend,
            ..default()
        })),
        transform,
    ));

    // Side logos — a kind-specific pictograph (bolt for `EnergyGenerator`,
    // etc.) mounted on every outward face of the structure, tinted with
    // the owner's actual car color so it reads as "your" building at a
    // glance. Every `uses_slab_geometry` kind (Ramp, Road, Platform, Wall)
    // has no faces worth badging (see `BuildingKind::uses_slab_geometry`)
    // and is skipped.
    if !snapshot.kind.uses_slab_geometry() {
        let shape = buildings::collider_shape(snapshot.kind);
        entity.with_children(|parent| {
            spawn_kind_logos(parent, &mut meshes, &mut materials, snapshot.kind, shape, car_color);
        });
    }

    // Every building is a real physics object now — the exact same
    // collider dimensions/pose the server uses for its own copy
    // (`uses_slab_geometry` kinds via `slab_dims`/`slab_transform`,
    // everything else via `collider_shape`, both shared with
    // `economy.rs`), so client-side prediction and server-authoritative
    // physics can never disagree about where a car is actually blocked.
    if snapshot.kind.uses_slab_geometry() {
        let dims = buildings::slab_dims(snapshot.kind);
        entity.insert((
            RigidBody::Fixed,
            Collider::cuboid(dims.half_width, dims.half_height, dims.half_length),
            Friction::coefficient(1.0),
        ));
    } else {
        let collider = match buildings::collider_shape(snapshot.kind) {
            ColliderShape::Cuboid { half_x, half_y, half_z } => Collider::cuboid(half_x, half_y, half_z),
            ColliderShape::Cylinder { half_height, radius } => Collider::cylinder(half_height, radius),
        };
        entity.insert((RigidBody::Fixed, collider, Friction::coefficient(1.0)));
    }
}

/// Re-derives every building's local x/z from its replicated true position
/// and the *current* `WorldOrigin` every frame — the same "recompute from
/// true state, never trust a value baked in at spawn" pattern
/// `car_render.rs`'s `sync_car_transforms` and `villager_render.rs`'s
/// `sync_villager_transform` already use, extended to buildings too.
///
/// Before this, `init_building_visuals` computed `local` once, at insert
/// time, and nothing ever touched it again except `worldspace.rs`'s
/// `rebase_world` — which only shifts on an ordinary in-play threshold
/// crossing, not the much larger one-time jump `pilot.rs`'s
/// `spawn_pilot_after_login` makes to `WorldOrigin` at login. A building
/// that replicated in even slightly before (or during) that login-time
/// reset — a real race, `AuthResultMsg` and replicated inserts travel over
/// separate channels with no ordering guarantee (see `aircraft.rs`'s
/// `tag_local_plane` docs) — would stay rendered at its old, now-wrong
/// local position forever: reported live as "I can still see stuff from
/// the start area" despite standing nowhere near it. Recomputing every
/// frame instead of relying on a shift-list makes buildings immune to that
/// race entirely, the same way cars/planes/villagers already are — no
/// entry in `rebase_world`'s query list is needed for this to stay correct
/// no matter when `WorldOrigin` changes or why.
///
/// The full pose is recomputed via `building_transform`, not just x/z
/// patched in place — a slab kind's (Ramp/Road/Platform/Wall) translation
/// includes a rotation-dependent anchor offset on top of the raw local
/// position (see that function's own docs), so only ever touching x/z
/// directly here would silently drop that offset the moment `WorldOrigin`
/// changes.
fn sync_building_transform(
    origin: Res<WorldOrigin>,
    mut buildings_q: Query<(&BuildingSnapshot, &mut Transform)>,
) {
    for (snapshot, mut transform) in &mut buildings_q {
        let local = (DVec3::new(snapshot.true_x, 0.0, snapshot.true_z) - origin.offset).as_vec3();
        *transform = building_transform(snapshot.kind, local.x, local.z, snapshot.ground_y, snapshot.rotation_y);
    }
}

/// One flat, thin box making up part of a side logo — several of these
/// stacked (see `logo_parts_for_kind`) form each kind's pictograph, the
/// same "build the shape out of primitive meshes" approach every other
/// cosmetic in this project already uses (compare `car_render.rs`'s bow,
/// made of spheres and cuboids). Coordinates are in badge-local space:
/// X/Y lay out the icon, Z is thickness — `spawn_kind_logos` rotates and
/// offsets each part onto the real building face afterward.
struct LogoPart {
    half_extents: Vec3,
    transform: Transform,
}

const LOGO_THICKNESS: f32 = 0.04;

fn bar(half_len: f32, half_width: f32, x: f32, y: f32, rotation_z: f32) -> LogoPart {
    LogoPart {
        half_extents: Vec3::new(half_len, half_width, LOGO_THICKNESS),
        transform: Transform::from_xyz(x, y, 0.0).with_rotation(Quat::from_rotation_z(rotation_z)),
    }
}

/// Per-kind pictograph, laid out flat facing +Z, roughly filling a 1x1
/// square — deliberately simple placeholder shapes (no external texture
/// assets, matching this project's existing "cosmetic style"), just
/// distinct enough to read as "power," "ore," etc. at a glance. `Ramp`
/// never calls this (see its own docs).
fn logo_parts_for_kind(kind: BuildingKind) -> Vec<LogoPart> {
    match kind {
        BuildingKind::Ramp | BuildingKind::Road | BuildingKind::Platform | BuildingKind::Wall => Vec::new(),
        // A bolt: two parallel diagonal strokes forming a lightning-bolt
        // zigzag.
        BuildingKind::EnergyGenerator => vec![
            bar(0.32, 0.07, 0.08, 0.22, -0.6),
            bar(0.32, 0.07, -0.08, -0.22, -0.6),
        ],
        // Ore: a square rotated into a diamond.
        BuildingKind::ExtractionFacility => {
            vec![bar(0.28, 0.28, 0.0, 0.0, std::f32::consts::FRAC_PI_4)]
        }
        // "H" for Hangar: two uprights and a crossbar.
        BuildingKind::Hangar => vec![
            bar(0.06, 0.32, -0.22, 0.0, 0.0),
            bar(0.06, 0.32, 0.22, 0.0, 0.0),
            bar(0.28, 0.06, 0.0, 0.0, std::f32::consts::FRAC_PI_2),
        ],
        // An upward arrow: a shaft with a chevron head, for "production."
        BuildingKind::LandFactory => vec![
            bar(0.25, 0.07, 0.0, -0.1, std::f32::consts::FRAC_PI_2),
            bar(0.22, 0.07, -0.12, 0.22, 0.8),
            bar(0.22, 0.07, 0.12, 0.22, -0.8),
        ],
        // A wing: a fuselage bar crossed by a wider, thinner wingspan bar.
        BuildingKind::AirFactory => vec![
            bar(0.3, 0.05, 0.0, 0.0, std::f32::consts::FRAC_PI_2),
            bar(0.3, 0.06, 0.0, 0.02, 0.0),
        ],
    }
}

/// Mounts `logo_parts_for_kind(kind)` on every outward-facing point of
/// `shape`'s real footprint — every side for a `Cuboid`, four points
/// around the rim for a `Cylinder` — each rotated to sit flush against
/// that face and tinted with `color` (the owner's actual car paint; see
/// `init_building_visuals`).
fn spawn_kind_logos(
    parent: &mut ChildSpawnerCommands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    kind: BuildingKind,
    shape: ColliderShape,
    color: Color,
) {
    let parts = logo_parts_for_kind(kind);
    if parts.is_empty() {
        return;
    }
    let material = materials.add(StandardMaterial { base_color: color, ..default() });

    // (outward direction in the XZ plane, distance from center to that
    // face along it) for each mount point.
    let mounts: Vec<(Vec2, f32)> = match shape {
        ColliderShape::Cuboid { half_x, half_z, .. } => vec![
            (Vec2::new(1.0, 0.0), half_x),
            (Vec2::new(-1.0, 0.0), half_x),
            (Vec2::new(0.0, 1.0), half_z),
            (Vec2::new(0.0, -1.0), half_z),
        ],
        ColliderShape::Cylinder { radius, .. } => (0..4)
            .map(|i| {
                let angle = i as f32 / 4.0 * TAU;
                (Vec2::new(angle.cos(), angle.sin()), radius)
            })
            .collect(),
    };

    for (dir, distance) in mounts {
        // Same yaw convention `building_placement.rs`'s ramp-facing math
        // uses: local +Z rotated by `Quat::from_rotation_y(yaw)` points
        // at world (sin(yaw), cos(yaw)) in (x, z), so this is
        // `atan2(dir.x, dir.y)` to make it point along `dir`.
        let yaw = dir.x.atan2(dir.y);
        let mount_transform = Transform::from_translation(Vec3::new(dir.x, 0.0, dir.y) * (distance + 0.02))
            .with_rotation(Quat::from_rotation_y(yaw));

        for part in &parts {
            parent.spawn((
                Mesh3d(meshes.add(Cuboid::new(
                    part.half_extents.x * 2.0,
                    part.half_extents.y * 2.0,
                    part.half_extents.z * 2.0,
                ))),
                MeshMaterial3d(material.clone()),
                mount_transform * part.transform,
            ));
        }
    }
}

/// Fades a building translucent while its `build_complete_at` is still in
/// the future, opaque once construction actually completes — re-evaluated
/// every frame (not decided once at spawn) so a building that's already
/// under construction when a client connects still finishes visibly.
///
/// Also flips `alpha_mode` back to `Opaque` the moment it's no longer
/// actually translucent — every building spawns as `AlphaMode::Blend`
/// (needed while genuinely see-through under construction), but Bevy
/// renders `Blend` materials through a separate pass sorted back-to-front
/// by each *entity's* distance from the camera rather than real per-pixel
/// depth. Two completed, fully-opaque buildings left on `Blend` forever
/// (a finished building's alpha is 1.0, but its `alpha_mode` never
/// changed) could still draw in the wrong order relative to each other —
/// exactly the reported "a Ramp renders behind a Ramp it's actually in
/// front of" and "ExtractionFacility vs. its ore deposit beacon" glitches,
/// both nearby-and-both-still-technically-translucent pairs. Once actually
/// opaque, `Opaque` puts it back through the ordinary depth-tested pass,
/// which sorts correctly against everything else, including other
/// buildings and any real `Blend` object (like the beacon) still nearby.
fn update_construction_tint(
    mut materials: ResMut<Assets<StandardMaterial>>,
    buildings: Query<(&BuildingSnapshot, &MeshMaterial3d<StandardMaterial>)>,
) {
    let now = now_unix();
    for (snapshot, material) in &buildings {
        let Some(mut mat) = materials.get_mut(&material.0) else {
            continue;
        };
        let under_construction = snapshot.build_complete_at > now;
        mat.base_color.set_alpha(if under_construction { 0.35 } else { 1.0 });
        mat.alpha_mode = if under_construction { AlphaMode::Blend } else { AlphaMode::Opaque };
    }
}
