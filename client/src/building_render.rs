use bevy::math::DVec3;
use bevy::prelude::*;
use bevy_rapier3d::prelude::{Collider, Friction, RigidBody};
use shared::buildings::{self, BuildingKind};
use shared::protocol::BuildingSnapshot;
use shared::terrain_gen::{height_at, TerrainNoise};
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

    let (mesh, base_color, half_height) = match kind {
        BuildingKind::Hangar => {
            (meshes.add(Cuboid::new(4.0, 2.5, 5.0)), Color::srgb(0.45, 0.45, 0.5), 1.25)
        }
        BuildingKind::EnergyGenerator => {
            (meshes.add(Cylinder::new(1.2, 3.0)), Color::srgb(0.9, 0.8, 0.2), 1.5)
        }
        BuildingKind::ExtractionFacility => {
            (meshes.add(Cylinder::new(0.8, 4.0)), Color::srgb(0.75, 0.4, 0.2), 2.0)
        }
        BuildingKind::LandFactory => {
            (meshes.add(Cuboid::new(6.0, 3.5, 6.0)), Color::srgb(0.3, 0.35, 0.32), 1.75)
        }
        BuildingKind::Ramp => unreachable!("handled above"),
    };
    let transform = Transform::from_xyz(local_x, ground_y + half_height, local_z)
        .with_rotation(Quat::from_rotation_y(rotation_y));
    (mesh, base_color, transform)
}

fn init_building_visuals(
    insert: On<Insert, BuildingSnapshot>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    snapshots: Query<&BuildingSnapshot>,
    noise: Res<TerrainNoise>,
    origin: Res<WorldOrigin>,
) {
    let Ok(snapshot) = snapshots.get(insert.entity) else {
        return;
    };
    let ground_y = height_at(&noise, snapshot.true_x, snapshot.true_z);
    let local = (DVec3::new(snapshot.true_x, 0.0, snapshot.true_z) - origin.offset).as_vec3();
    let (mesh, base_color, transform) =
        building_mesh_and_transform(snapshot.kind, &mut meshes, local.x, local.z, ground_y, snapshot.rotation_y);

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

    // Ramp is a real physics object (a car needs to drive on it) — the
    // exact same collider dimensions/pose the server uses for its own
    // copy, so client-side prediction and server-authoritative physics
    // can never disagree about where the surface is. Every other kind
    // here is purely decorative (no collider at all).
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
