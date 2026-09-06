use bevy::math::DVec3;
use bevy::prelude::*;
use bevy_rapier3d::prelude::{Collider, Friction, RigidBody};
use shared::buildings::{self, BuildingKind, ColliderShape};
use shared::protocol::BuildingSnapshot;
use shared::time::now_unix;

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
            .add_systems(Update, update_construction_tint);
    }
}

/// Mesh + base color + local-space transform for one building kind at a
/// given placement — the actual per-kind geometry, shared by the real
/// spawn (`init_building_visuals`) and the ghost preview
/// (`building_placement.rs`) so the ghost always looks/sits exactly like
/// what will actually be placed. `ground_y` and `rotation_y` come from the
/// caller (a live raycast for the ghost, the replicated snapshot for the
/// real thing).
pub(crate) fn building_mesh_and_transform(
    kind: BuildingKind,
    meshes: &mut Assets<Mesh>,
    local_x: f32,
    local_z: f32,
    ground_y: f32,
    rotation_y: f32,
) -> (Handle<Mesh>, Color, Transform) {
    if kind == BuildingKind::Ramp {
        let (translation, rotation) = buildings::ramp_transform(local_x, local_z, ground_y, rotation_y);
        let mesh = meshes.add(Cuboid::new(
            buildings::RAMP_HALF_WIDTH * 2.0,
            buildings::RAMP_HALF_THICKNESS * 2.0,
            buildings::RAMP_HALF_LENGTH * 2.0,
        ));
        return (
            mesh,
            Color::srgb(0.5, 0.45, 0.4),
            Transform::from_translation(translation).with_rotation(rotation),
        );
    }

    // Cuboid/Cylinder dimensions and the collider used to actually block a
    // car (see `init_building_visuals`) come from the exact same
    // `collider_shape` per kind — mesh and collider can never drift apart.
    let shape = buildings::collider_shape(kind);
    let mesh = match shape {
        ColliderShape::Cuboid { half_x, half_y, half_z } => {
            meshes.add(Cuboid::new(half_x * 2.0, half_y * 2.0, half_z * 2.0))
        }
        ColliderShape::Cylinder { half_height, radius } => {
            meshes.add(Cylinder::new(radius, half_height * 2.0))
        }
    };
    let base_color = match kind {
        BuildingKind::Hangar => Color::srgb(0.45, 0.45, 0.5),
        BuildingKind::EnergyGenerator => Color::srgb(0.9, 0.8, 0.2),
        BuildingKind::ExtractionFacility => Color::srgb(0.75, 0.4, 0.2),
        BuildingKind::LandFactory => Color::srgb(0.3, 0.35, 0.32),
        BuildingKind::Ramp => unreachable!("handled above"),
    };
    let transform = Transform::from_xyz(local_x, ground_y + shape.half_height(), local_z)
        .with_rotation(Quat::from_rotation_y(rotation_y));
    (mesh, base_color, transform)
}

fn init_building_visuals(
    insert: On<Insert, BuildingSnapshot>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    snapshots: Query<&BuildingSnapshot>,
    origin: Res<WorldOrigin>,
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

    // Every building is a real physics object now — the exact same
    // collider dimensions/pose the server uses for its own copy (`Ramp`
    // via `RAMP_HALF_*`/`ramp_transform`, everything else via
    // `collider_shape`, both shared with `economy.rs`), so client-side
    // prediction and server-authoritative physics can never disagree
    // about where a car is actually blocked.
    if snapshot.kind == BuildingKind::Ramp {
        entity.insert((
            RigidBody::Fixed,
            Collider::cuboid(
                buildings::RAMP_HALF_WIDTH,
                buildings::RAMP_HALF_THICKNESS,
                buildings::RAMP_HALF_LENGTH,
            ),
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

/// Fades a building translucent while its `build_complete_at` is still in
/// the future, opaque once construction actually completes — re-evaluated
/// every frame (not decided once at spawn) so a building that's already
/// under construction when a client connects still finishes visibly.
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
    }
}
