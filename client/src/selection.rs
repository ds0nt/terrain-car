use bevy::math::DVec3;
use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use bevy_rapier3d::prelude::*;
use shared::protocol::BuildingSnapshot;

use crate::building_placement::PlacementState;
use crate::camera::CarCamera;
use crate::worldspace::WorldOrigin;

/// Click-to-select for existing buildings — the RTS half of "click things
/// to control them." Selecting doesn't do anything by itself; it's read
/// by `building_ui.rs`, which swaps the bottom build bar for that
/// building's own info/actions while something's selected.
pub struct SelectionPlugin;

impl Plugin for SelectionPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Selected>()
            .add_systems(Update, (handle_click, clear_if_gone, sync_marker).chain());
    }
}

/// The currently-selected building, if any. `building_ui.rs` and
/// `sync_marker` (below) both just read this — nothing here has any
/// opinion on what selecting something should actually let you do.
#[derive(Resource, Default)]
pub struct Selected(pub Option<Entity>);

#[derive(Component)]
struct SelectionMarker;

/// Left-click selects whatever building is under the cursor, or clears
/// selection if the click hit anything else (bare terrain, a car, ...).
/// Only reacts while nothing is being placed
/// (`building_placement::PlacementState::is_idle`) — left-click is
/// otherwise unused outside placement, so there's no real conflict, just
/// an ordering one: a placement click shouldn't also reselect whatever's
/// underneath the new ghost.
fn handle_click(
    mouse: Res<ButtonInput<MouseButton>>,
    keyboard: Res<ButtonInput<KeyCode>>,
    placement_state: Res<PlacementState>,
    windows: Query<&Window, With<PrimaryWindow>>,
    camera_q: Query<(&Camera, &GlobalTransform), With<CarCamera>>,
    rapier_context: ReadRapierContext,
    buildings: Query<(), With<BuildingSnapshot>>,
    mut selected: ResMut<Selected>,
) {
    if keyboard.just_pressed(KeyCode::Escape) {
        selected.0 = None;
        return;
    }
    if !placement_state.is_idle() || !mouse.just_pressed(MouseButton::Left) {
        return;
    }

    let Ok(window) = windows.single() else { return };
    let Some(cursor) = window.cursor_position() else { return };
    let Ok((camera, camera_transform)) = camera_q.single() else { return };
    let Ok(ray) = camera.viewport_to_world(camera_transform, cursor) else { return };
    let Ok(context) = rapier_context.single() else { return };

    let hit = context.cast_ray(ray.origin, *ray.direction, 2000.0, true, QueryFilter::default());
    selected.0 = match hit {
        Some((entity, _)) if buildings.contains(entity) => Some(entity),
        _ => None,
    };
}

/// If the selected entity stops having a `BuildingSnapshot` (despawned —
/// nothing does this yet, but nothing should ever show a stale selection
/// if it someday can), fall back to no selection rather than
/// `building_ui.rs` having to also handle a dangling `Entity`.
fn clear_if_gone(buildings: Query<(), With<BuildingSnapshot>>, mut selected: ResMut<Selected>) {
    if let Some(entity) = selected.0
        && !buildings.contains(entity)
    {
        selected.0 = None;
    }
}

/// Keeps a small glowing ring at the base of whichever building is
/// selected — clicking something with zero visual feedback would just
/// look broken. Only touches anything on an actual change, not every
/// frame.
fn sync_marker(
    selected: Res<Selected>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    buildings: Query<&BuildingSnapshot>,
    origin: Res<WorldOrigin>,
    marker_q: Query<Entity, With<SelectionMarker>>,
) {
    if !selected.is_changed() {
        return;
    }
    for marker in &marker_q {
        commands.entity(marker).despawn();
    }
    let Some(entity) = selected.0 else { return };
    let Ok(snapshot) = buildings.get(entity) else { return };

    let local = (DVec3::new(snapshot.true_x, 0.0, snapshot.true_z) - origin.offset).as_vec3();
    commands.spawn((
        Mesh3d(meshes.add(Torus::new(2.6, 3.0))),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::srgb(1.0, 0.95, 0.4),
            emissive: LinearRgba::rgb(3.0, 2.8, 0.8),
            unlit: true,
            ..default()
        })),
        Transform::from_xyz(local.x, snapshot.ground_y + 0.1, local.z),
        SelectionMarker,
    ));
}
