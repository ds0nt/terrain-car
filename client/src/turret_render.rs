use bevy::prelude::*;
use shared::tank_physics::turret_pivot_offset;

/// Shared rotating-turret-head visual — a small block plus a forward-facing
/// barrel, mounted on `turret_pivot_offset` — used by both a `Tank`'s own
/// turret (`tank_render.rs`) and a placed `BuildingKind::Turret`'s head
/// (`building_render.rs`), the same reasoning `shared::tank_physics`'s own
/// module docs give for why they share their aim/fire math: this is
/// genuinely the same "small rotating armed head" concept in both places,
/// just driven by a player's mouse in one case and
/// `server::turrets`'s auto-aim in the other. Simple procedural cuboids
/// only (no external texture/model assets), matching this project's
/// existing placeholder cosmetic style.
///
/// Spawns the head/barrel as children of whatever entity `parent` is
/// building — the caller is responsible for that entity's own `Transform`
/// actually being the thing that rotates by the current aim yaw (see
/// `sync_building_turret_heads`/`tank_render`'s own sync system): because
/// `turret_pivot_offset` has no X/Z component, rotating that parent
/// entity around its own local Y axis rotates this whole assembly around
/// the pivot point correctly, with no extra offset math needed here.
pub fn spawn_turret_head_meshes(
    parent: &mut ChildSpawnerCommands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    half_extents: Vec3,
    color: Color,
) {
    let material = materials.add(StandardMaterial { base_color: color, ..default() });
    let pivot = turret_pivot_offset(half_extents);

    parent.spawn((
        Mesh3d(meshes.add(Cuboid::new(half_extents.x * 1.1, half_extents.y * 0.9, half_extents.z * 1.1))),
        MeshMaterial3d(material.clone()),
        Transform::from_translation(pivot),
    ));

    // A long thin box extending forward along local `+Z` from the pivot —
    // a `Cuboid`'s own z-extent already runs along local `+Z` with no
    // rotation needed, unlike a primitive `Cylinder` (which defaults to
    // `+Y`), so this is simpler than it would be with a "more realistic"
    // barrel shape for the same placeholder-quality payoff.
    let barrel_half_len = half_extents.z * 0.9;
    parent.spawn((
        Mesh3d(meshes.add(Cuboid::new(half_extents.x * 0.25, half_extents.y * 0.25, barrel_half_len * 2.0))),
        MeshMaterial3d(material),
        Transform::from_translation(pivot + Vec3::new(0.0, half_extents.y * 0.2, barrel_half_len)),
    ));
}
