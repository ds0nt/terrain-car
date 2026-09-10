use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts, EguiPrimaryContextPass};
use shared::car_physics::CarChassis;
use shared::protocol::{CarSnapshot, PlaneSnapshot, PlayerInfo, PlayerOnFootSnapshot};
use uuid::Uuid;

use crate::aircraft::LocalPlane;
use crate::camera::CarCamera;
use crate::car::LocalCar;
use crate::owner_color::{color_for_owner, to_egui_color32};
use crate::pilot::PlayerFocus;
use crate::player_account::LocalPlayerAccount;

/// Answers "where is everyone else" in the 3D view itself, complementing
/// the minimap's top-down dots (`minimap.rs`) with something readable
/// while actually looking around: a floating username tag over any remote
/// car/plane currently on screen, or a compass-style arrow at the screen
/// edge pointing toward one that isn't. Every remote car/plane is already
/// replicated with an owner id (`CarChassis`/`PlaneSnapshot`), and every
/// logged-in player's username is already replicated too (`PlayerInfo`, see
/// `players_ui.rs`) — this just projects the one onto the other.
///
/// A car carrying a passenger (`CarSnapshot::passenger_player_id`) gets two
/// tags side by side instead of one — the owner/driver offset to the car's
/// own left, the passenger to its own right (see `SEAT_OFFSET`) — so both
/// occupants stay identifiable rather than the passenger silently vanishing
/// behind a single "owner" label.
pub struct PlayerMarkersPlugin;

impl Plugin for PlayerMarkersPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(EguiPrimaryContextPass, draw_player_markers);
    }
}

/// How far above a car's/plane's own origin the tag floats — clear of the
/// roof/canopy without drifting so high it separates from the vehicle at a
/// distance.
const CAR_TAG_HEIGHT: f32 = 2.6;
const PLANE_TAG_HEIGHT: f32 = 1.6;
const PLAYER_TAG_HEIGHT: f32 = 0.5;
/// How far left/right of the car's own centerline a driver's/passenger's
/// tag sits when both are shown — the car's own real half-width plus a bit
/// of breathing room, so the two never crowd together into one illegible
/// cluster even when the car is close.
const SEAT_OFFSET: f32 = 1.4;
/// Kept clear of the screen's physical edge so the arrow/text never clips.
const EDGE_MARGIN: f32 = 40.0;
/// Below this, a name floating right on top of you is just noise — this is
/// close enough you can already see who it is.
const MIN_MARKER_DISTANCE: f32 = 3.0;

fn draw_player_markers(
    mut contexts: EguiContexts,
    camera_q: Query<(&Camera, &GlobalTransform), With<CarCamera>>,
    focus: Res<PlayerFocus>,
    players: Query<&PlayerInfo>,
    remote_cars: Query<(&Transform, &CarChassis, &CarSnapshot), Without<LocalCar>>,
    remote_planes: Query<(&Transform, &PlaneSnapshot), Without<LocalPlane>>,
    remote_players: Query<(&Transform, &PlayerOnFootSnapshot, &PlayerInfo), Without<LocalPlayerAccount>>,
) -> Result {
    let Ok((camera, camera_gt)) = camera_q.single() else {
        return Ok(());
    };
    let Some(viewport) = camera.logical_viewport_rect() else {
        return Ok(());
    };
    let viewport = egui::Rect::from_min_max(
        egui::pos2(viewport.min.x, viewport.min.y),
        egui::pos2(viewport.max.x, viewport.max.y),
    );
    let ctx = contexts.ctx_mut()?;
    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("player_markers")));

    let cam_transform = camera_gt.compute_transform();
    let view = CameraView {
        position: cam_transform.translation,
        forward: *cam_transform.forward(),
        right: *cam_transform.right(),
        up: *cam_transform.up(),
    };

    for (transform, chassis, snapshot) in &remote_cars {
        let Some(driver_name) = username_for(&players, chassis.owner_player_id) else { continue };
        let base = transform.translation + Vec3::Y * CAR_TAG_HEIGHT;
        let distance = focus.translation.distance(base);
        if distance < MIN_MARKER_DISTANCE {
            continue;
        }
        let driver_color = to_egui_color32(color_for_owner(chassis.owner_player_id));
        let passenger =
            snapshot.passenger_player_id.and_then(|id| username_for(&players, id).map(|name| (id, name)));

        let Some(pos) = project_on_screen(camera, camera_gt, viewport, &view, base) else {
            // Off screen (or fully behind the camera) — one combined arrow
            // rather than two overlapping ones, since a driver+passenger
            // pair only a couple of meters apart in world space would
            // otherwise project to virtually the same edge point anyway.
            let label = match &passenger {
                Some((_, passenger_name)) => format!("{driver_name} & {passenger_name}"),
                None => driver_name.clone(),
            };
            draw_edge_arrow(&painter, viewport, &view, base, &label, distance, driver_color);
            continue;
        };

        match passenger {
            Some((passenger_id, passenger_name)) => {
                let right = *transform.right();
                let passenger_color = to_egui_color32(color_for_owner(passenger_id));
                // Each seat gets its own projected point where possible —
                // right at the viewport's edge, an individual seat offset
                // can occasionally fall just outside it even though the
                // car's own center didn't, so this falls back to a plain
                // screen-space offset from `pos` rather than letting that
                // one tag simply vanish.
                let driver_pos = project_on_screen(camera, camera_gt, viewport, &view, base - right * SEAT_OFFSET)
                    .unwrap_or(pos - egui::vec2(20.0, 0.0));
                let passenger_pos = project_on_screen(camera, camera_gt, viewport, &view, base + right * SEAT_OFFSET)
                    .unwrap_or(pos + egui::vec2(20.0, 0.0));
                draw_nametag(&painter, driver_pos, &driver_name, distance, driver_color);
                draw_nametag(&painter, passenger_pos, &passenger_name, distance, passenger_color);
            }
            None => draw_nametag(&painter, pos, &driver_name, distance, driver_color),
        }
    }

    for (transform, snapshot) in &remote_planes {
        let Some(username) = username_for(&players, snapshot.owner_player_id) else { continue };
        let base = transform.translation + Vec3::Y * PLANE_TAG_HEIGHT;
        let distance = focus.translation.distance(base);
        if distance < MIN_MARKER_DISTANCE {
            continue;
        }
        let color = to_egui_color32(color_for_owner(snapshot.owner_player_id));
        match project_on_screen(camera, camera_gt, viewport, &view, base) {
            Some(pos) => draw_nametag(&painter, pos, &username, distance, color),
            None => draw_edge_arrow(&painter, viewport, &view, base, &username, distance, color),
        }
    }

    // On-foot players — see `PlayerOnFootSnapshot`'s own docs on why this
    // is a new addition: before it existed, a player walking around had no
    // replicated position at all, so there was nothing here to draw a tag
    // for in the first place (nor anything to render *as* them at all —
    // see `remote_players.rs`).
    for (transform, snapshot, info) in &remote_players {
        if !snapshot.on_foot {
            continue;
        }
        let base = transform.translation + Vec3::Y * PLAYER_TAG_HEIGHT;
        let distance = focus.translation.distance(base);
        if distance < MIN_MARKER_DISTANCE {
            continue;
        }
        let color = to_egui_color32(color_for_owner(info.player_id));
        match project_on_screen(camera, camera_gt, viewport, &view, base) {
            Some(pos) => draw_nametag(&painter, pos, &info.username, distance, color),
            None => draw_edge_arrow(&painter, viewport, &view, base, &info.username, distance, color),
        }
    }

    Ok(())
}

fn username_for(players: &Query<&PlayerInfo>, owner_player_id: Uuid) -> Option<String> {
    players.iter().find(|p| p.player_id == owner_player_id).map(|p| p.username.clone())
}

struct CameraView {
    position: Vec3,
    forward: Vec3,
    right: Vec3,
    up: Vec3,
}

/// `Some(screen_pos)` if `world_pos` is both in front of the camera and
/// actually within the viewport rect, `None` otherwise (either reason means
/// there's nothing meaningful to draw a floating tag *at* — the caller
/// falls back to `draw_edge_arrow` in that case).
fn project_on_screen(
    camera: &Camera,
    camera_gt: &GlobalTransform,
    viewport: egui::Rect,
    view: &CameraView,
    world_pos: Vec3,
) -> Option<egui::Pos2> {
    let rel = world_pos - view.position;
    if rel.dot(view.forward) <= 0.0 {
        return None;
    }
    let screen = camera.world_to_viewport(camera_gt, world_pos).ok()?;
    let pos = egui::pos2(screen.x, screen.y);
    viewport.contains(pos).then_some(pos)
}

fn draw_nametag(painter: &egui::Painter, pos: egui::Pos2, label: &str, distance: f32, color: egui::Color32) {
    painter.circle_filled(pos, 4.0, color);
    painter.text(
        pos + egui::vec2(0.0, -12.0),
        egui::Align2::CENTER_BOTTOM,
        format!("{label}  {distance:.0}m"),
        egui::FontId::proportional(13.0),
        color,
    );
}

/// A small arrow clamped to the viewport edge, pointing the direction
/// you'd need to look to find `world_pos` — same "compass ping" convention
/// many multiplayer games use for allies out of view. Used whether the
/// target is just outside the frame or fully behind the camera; the
/// camera-relative dot products below point the right way either way, no
/// extra case-handling needed for "behind."
fn draw_edge_arrow(
    painter: &egui::Painter,
    viewport: egui::Rect,
    view: &CameraView,
    world_pos: Vec3,
    label: &str,
    distance: f32,
    color: egui::Color32,
) {
    let rel = world_pos - view.position;
    let local_x = rel.dot(view.right);
    let local_y = rel.dot(view.up);
    let mut dir = egui::vec2(local_x, -local_y);
    if dir.length_sq() < 1e-6 {
        dir = egui::vec2(1.0, 0.0);
    }
    dir = dir.normalized();

    let center = viewport.center();
    let half_w = (viewport.width() / 2.0 - EDGE_MARGIN).max(1.0);
    let half_h = (viewport.height() / 2.0 - EDGE_MARGIN).max(1.0);
    let scale = (half_w / dir.x.abs().max(1e-4)).min(half_h / dir.y.abs().max(1e-4));
    let edge_pos = center + dir * scale;

    let angle = dir.y.atan2(dir.x);
    let tip = edge_pos + egui::vec2(angle.cos(), angle.sin()) * 9.0;
    let back_angle = 2.6;
    let left = edge_pos + egui::vec2((angle + back_angle).cos(), (angle + back_angle).sin()) * 7.0;
    let right = edge_pos + egui::vec2((angle - back_angle).cos(), (angle - back_angle).sin()) * 7.0;
    painter.add(egui::Shape::convex_polygon(vec![tip, left, right], color, egui::Stroke::NONE));
    painter.text(
        edge_pos + egui::vec2(0.0, 16.0),
        egui::Align2::CENTER_TOP,
        format!("{label}  {distance:.0}m"),
        egui::FontId::proportional(12.0),
        color,
    );
}
