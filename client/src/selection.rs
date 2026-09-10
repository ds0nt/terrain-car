use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use bevy_egui::EguiContexts;
use bevy_rapier3d::prelude::*;
use bevy_replicon::prelude::ClientTriggerExt;
use shared::car_physics::CarChassis;
use shared::protocol::{BuildingSnapshot, PlaneSnapshot, SetCarPatrolMsg, SetPlanePatrolMsg};
use uuid::Uuid;

use crate::building_placement::PlacementState;
use crate::camera::CarCamera;
use crate::car::LocalCar;
use crate::worldspace::WorldOrigin;

/// Click-to-select — the RTS half of "click things to control them."
/// Selecting a building doesn't do anything by itself; it's read by
/// `building_ui.rs`, which swaps the bottom build bar for that building's
/// own info/actions while something's selected. Selecting an AI-controlled
/// car/plane (`server::ai`'s patrol starter — real player-owned vehicles
/// aren't selectable at all, see `handle_click`'s own docs) additionally
/// lets a right-click give it a new patrol point, the standard RTS
/// "select, then right-click to command" pattern.
pub struct SelectionPlugin;

impl Plugin for SelectionPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Selected>()
            .add_systems(Update, (handle_click, send_patrol_order, clear_if_gone, sync_marker).chain());
    }
}

/// No player owns an AI-controlled vehicle — see `server::ai::AI_OWNER`'s
/// own docs. Checked client-side purely to decide whether right-clicking a
/// selected car/plane should look like a valid command at all; the server
/// re-checks this itself before ever actually acting on one (see
/// `ai::apply_set_car_patrol`'s own docs) since nothing client-side is
/// ever trusted for that.
const AI_OWNER: Uuid = Uuid::nil();

/// The currently-selected entity, if any — a building, or (see this
/// module's own top-level docs) an AI-controlled car/plane. Everything
/// downstream (`building_ui.rs`, `sync_marker`, `send_patrol_order`) just
/// reads this and re-queries the entity for whichever components actually
/// apply to figure out what selecting it should let you do.
#[derive(Resource, Default)]
pub struct Selected(pub Option<Entity>);

#[derive(Component)]
struct SelectionMarker;

/// Anything `Selected` may point at — a building or an AI-controlled
/// car/plane. Named so `handle_click`'s and `clear_if_gone`'s identical
/// "does this entity still qualify" filters can't silently drift apart.
type SelectableFilter = Or<(With<BuildingSnapshot>, With<CarChassis>, With<PlaneSnapshot>)>;

/// Left-click selects whatever's under the cursor — a building, or an
/// AI-controlled car/plane — or clears selection if the click hit
/// anything else (bare terrain, someone's own player-driven car, ...).
/// Only reacts while nothing is being placed
/// (`building_placement::PlacementState::is_idle`) — left-click is
/// otherwise unused outside placement, so there's no real conflict, just
/// an ordering one: a placement click shouldn't also reselect whatever's
/// underneath the new ghost.
#[allow(clippy::too_many_arguments)]
fn handle_click(
    mouse: Res<ButtonInput<MouseButton>>,
    keyboard: Res<ButtonInput<KeyCode>>,
    mut contexts: EguiContexts,
    placement_state: Res<PlacementState>,
    mode: Res<crate::pilot::ControlMode>,
    menu_open: Res<crate::pilot::MenuOpen>,
    windows: Query<&Window, With<PrimaryWindow>>,
    camera_q: Query<(&Camera, &GlobalTransform), With<CarCamera>>,
    rapier_context: ReadRapierContext,
    local_car_q: Query<Entity, With<LocalCar>>,
    selectable: Query<(), SelectableFilter>,
    mut selected: ResMut<Selected>,
) -> Result {
    if keyboard.just_pressed(KeyCode::Escape) {
        selected.0 = None;
        return Ok(());
    }
    if !placement_state.is_idle() || !mouse.just_pressed(MouseButton::Left) {
        return Ok(());
    }
    // A click that lands on an egui window (the build bar, the selected-
    // building panel with its Destroy/Queue Villager buttons, ...) must
    // not *also* fire this world-space raycast — without this check,
    // confirming the two-click Destroy button would immediately have this
    // same click re-raycast into the 3D scene, typically hitting nothing
    // (the panel sits over empty sky/ground, not the building itself) and
    // clearing `Selected` back to `None` right as the confirm click
    // landed — reported live as "the destroy button arms, but the confirm
    // click does nothing."
    if contexts.ctx_mut()?.egui_wants_pointer_input() {
        return Ok(());
    }

    let Ok(window) = windows.single() else { return Ok(()) };
    let Some(cursor) = crate::pilot::aim_position(*mode, menu_open.0, window) else { return Ok(()) };
    let Ok((camera, camera_transform)) = camera_q.single() else { return Ok(()) };
    let Ok(ray) = camera.viewport_to_world(camera_transform, cursor) else { return Ok(()) };
    let Ok(context) = rapier_context.single() else { return Ok(()) };

    // Excludes the local car's own collider — same reasoning
    // `building_placement.rs`'s `cursor_world_hit` already applies to its
    // placement raycast. Without it, trying to select a building standing
    // right next to your own parked car (the exact situation you're in
    // when you actually want to click it — a Land Factory or Hangar you
    // just drove up to) could have the ray clip your own car's body first
    // and silently clear the selection instead of ever reaching the
    // building behind it.
    let mut filter = QueryFilter::default();
    if let Some(local_car) = local_car_q.iter().next() {
        filter = filter.exclude_rigid_body(local_car);
    }
    let hit = context.cast_ray(ray.origin, *ray.direction, 2000.0, true, filter);
    selected.0 = match hit {
        Some((entity, _)) if selectable.contains(entity) => Some(entity),
        _ => None,
    };
    Ok(())
}

/// If the selected entity stops qualifying (despawned — nothing does this
/// yet for a building, but a recalled/destroyed AI vehicle is a real,
/// already-possible case), fall back to no selection rather than
/// `building_ui.rs`/`send_patrol_order` having to also handle a dangling
/// `Entity`.
fn clear_if_gone(selectable: Query<(), SelectableFilter>, mut selected: ResMut<Selected>) {
    if let Some(entity) = selected.0
        && !selectable.contains(entity)
    {
        selected.0 = None;
    }
}

/// Right-click gives the currently-selected AI car/plane a new patrol
/// point — the standard RTS "select, then right-click to command"
/// pattern. A no-op for anything else selected (a building, or a real
/// player's own vehicle — `owner_player_id != AI_OWNER`, see this
/// module's own docs on why that's checked client-side purely for UX, the
/// server re-checks it for real). Only reacts while nothing is being
/// placed, same as `handle_click` — `building_placement.rs` already owns
/// right-click for canceling a placement in progress.
#[allow(clippy::too_many_arguments)]
fn send_patrol_order(
    mouse: Res<ButtonInput<MouseButton>>,
    mut contexts: EguiContexts,
    placement_state: Res<PlacementState>,
    selected: Res<Selected>,
    mode: Res<crate::pilot::ControlMode>,
    menu_open: Res<crate::pilot::MenuOpen>,
    windows: Query<&Window, With<PrimaryWindow>>,
    camera_q: Query<(&Camera, &GlobalTransform), With<CarCamera>>,
    rapier_context: ReadRapierContext,
    local_car_q: Query<Entity, With<LocalCar>>,
    origin: Res<WorldOrigin>,
    vehicles: Query<(Option<&CarChassis>, Option<&PlaneSnapshot>)>,
    mut commands: Commands,
) -> Result {
    if !placement_state.is_idle() || !mouse.just_pressed(MouseButton::Right) {
        return Ok(());
    }
    let Some(entity) = selected.0 else { return Ok(()) };
    let Ok((chassis, plane_snapshot)) = vehicles.get(entity) else { return Ok(()) };
    let is_ai_car = chassis.is_some_and(|c| c.owner_player_id == AI_OWNER);
    let is_ai_plane = plane_snapshot.is_some_and(|p| p.owner_player_id == AI_OWNER);
    if !is_ai_car && !is_ai_plane {
        return Ok(());
    }

    if contexts.ctx_mut()?.egui_wants_pointer_input() {
        return Ok(());
    }
    let Ok(window) = windows.single() else { return Ok(()) };
    let Some(cursor) = crate::pilot::aim_position(*mode, menu_open.0, window) else { return Ok(()) };
    let Ok((camera, camera_transform)) = camera_q.single() else { return Ok(()) };
    let Ok(ray) = camera.viewport_to_world(camera_transform, cursor) else { return Ok(()) };
    let Ok(context) = rapier_context.single() else { return Ok(()) };
    let mut filter = QueryFilter::default();
    if let Some(local_car) = local_car_q.iter().next() {
        filter = filter.exclude_rigid_body(local_car);
    }
    let Some((_, toi)) = context.cast_ray(ray.origin, *ray.direction, 2000.0, true, filter) else {
        return Ok(());
    };
    let hit_local = ray.origin + *ray.direction * toi;
    let hit_true = origin.to_true(hit_local);

    if let Some(chassis) = chassis
        && is_ai_car
    {
        commands.client_trigger(SetCarPatrolMsg {
            car_id: chassis.car_id,
            target_true_x: hit_true.x,
            target_true_z: hit_true.z,
        });
    } else if let Some(snapshot) = plane_snapshot
        && is_ai_plane
    {
        commands.client_trigger(SetPlanePatrolMsg {
            plane_id: snapshot.plane_id,
            target_true_x: hit_true.x,
            target_true_z: hit_true.z,
        });
    }

    Ok(())
}

/// Keeps a small glowing ring at the base of whichever building, or on
/// whichever AI car/plane, is selected — clicking something with zero
/// visual feedback would just look broken. Only touches anything on an
/// actual change, not every frame. Uses `GlobalTransform` generically
/// (every selectable kind has one client-side, whatever it actually
/// renders as) rather than each kind's own position field, so this needs
/// no per-kind branch at all.
fn sync_marker(
    selected: Res<Selected>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    transforms: Query<&GlobalTransform>,
    marker_q: Query<Entity, With<SelectionMarker>>,
) {
    if !selected.is_changed() {
        return;
    }
    for marker in &marker_q {
        commands.entity(marker).despawn();
    }
    let Some(entity) = selected.0 else { return };
    let Ok(transform) = transforms.get(entity) else { return };

    let translation = transform.translation();
    commands.spawn((
        Mesh3d(meshes.add(Torus::new(2.6, 3.0))),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::srgb(1.0, 0.95, 0.4),
            emissive: LinearRgba::rgb(3.0, 2.8, 0.8),
            unlit: true,
            ..default()
        })),
        Transform::from_xyz(translation.x, translation.y + 0.1, translation.z),
        SelectionMarker,
    ));
}
