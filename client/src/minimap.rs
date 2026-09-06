use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts, EguiPrimaryContextPass};
use shared::car_physics::CarChassis;
use shared::protocol::{BuildingSnapshot, CarSnapshot, LocalCar, VillagerSnapshot};
use shared::terrain_gen::{height_at, TerrainNoise};

use crate::owner_color::{color_for_owner, color_from_seed, to_egui_color32};
use crate::worldspace::WorldOrigin;

/// A 2D radar drawn with `egui`'s painter — replaces the old ASCII
/// character grid with owner-colored blips (readable at a glance: whose
/// is whose, not just "something is there") plus a coarse terrain-relief
/// wash so nearby cliffs are still visible, same information the ASCII
/// version gave, just prettier. Doesn't register `bevy_egui::EguiPlugin`
/// itself — `auth_ui.rs` already does.
pub struct MinimapPlugin;

impl Plugin for MinimapPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(EguiPrimaryContextPass, draw_minimap);
    }
}

/// Half-width of the area actually shown, in world meters.
const RADAR_METERS: f32 = 120.0;
/// On-screen size (diameter) of the radar circle.
const RADAR_PX: f32 = 170.0;
const RADAR_RADIUS_PX: f32 = RADAR_PX / 2.0;
/// Coarse relief background grid — much cheaper than sampling per-pixel,
/// and a handful of soft-edged bands reads better than a sharp grid
/// anyway at this size.
const RELIEF_GRID: i32 = 9;

fn world_to_screen(center: egui::Pos2, dx: f32, dz: f32) -> Option<egui::Pos2> {
    let scale = RADAR_RADIUS_PX / RADAR_METERS;
    let offset = egui::vec2(dx * scale, dz * scale);
    if offset.length() > RADAR_RADIUS_PX {
        return None;
    }
    Some(center + offset)
}

/// Muted color bands by *local relief* (height relative to the local
/// car) — same "terrain near me, not absolute elevation" reasoning the
/// old ASCII `terrain_glyph` used.
fn relief_color(relief: f32) -> egui::Color32 {
    if relief < -8.0 {
        egui::Color32::from_rgb(20, 24, 32)
    } else if relief < -2.0 {
        egui::Color32::from_rgb(35, 42, 38)
    } else if relief < 2.0 {
        egui::Color32::from_rgb(55, 68, 50)
    } else if relief < 8.0 {
        egui::Color32::from_rgb(90, 95, 60)
    } else {
        egui::Color32::from_rgb(150, 145, 120)
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_minimap(
    mut contexts: EguiContexts,
    noise: Res<TerrainNoise>,
    origin: Res<WorldOrigin>,
    local_car_q: Query<&Transform, With<LocalCar>>,
    remote_cars_q: Query<(&CarSnapshot, &CarChassis), Without<LocalCar>>,
    buildings_q: Query<&BuildingSnapshot>,
    villagers_q: Query<&VillagerSnapshot>,
) -> Result {
    let Ok(local_transform) = local_car_q.single() else {
        return Ok(());
    };
    let local_true = origin.to_true(local_transform.translation);
    let forward = local_transform.forward();

    egui::Area::new(egui::Id::new("minimap"))
        .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-16.0, 16.0))
        .show(contexts.ctx_mut()?, |ui| {
            let (response, painter) =
                ui.allocate_painter(egui::vec2(RADAR_PX, RADAR_PX), egui::Sense::hover());
            let center = response.rect.center();

            painter.circle_filled(center, RADAR_RADIUS_PX, egui::Color32::from_black_alpha(200));

            // Relief wash.
            let cell_px = RADAR_PX / RELIEF_GRID as f32;
            let cell_m = (RADAR_METERS * 2.0) / RELIEF_GRID as f32;
            for row in 0..RELIEF_GRID {
                for col in 0..RELIEF_GRID {
                    let dx = (col as f32 - (RELIEF_GRID - 1) as f32 / 2.0) * cell_m;
                    let dz = (row as f32 - (RELIEF_GRID - 1) as f32 / 2.0) * cell_m;
                    let Some(cell_center) = world_to_screen(center, dx, dz) else { continue };
                    let world_x = local_true.x + dx as f64;
                    let world_z = local_true.z + dz as f64;
                    let ground_y = height_at(&noise, world_x, world_z);
                    let relief = ground_y - local_transform.translation.y;
                    let rect = egui::Rect::from_center_size(cell_center, egui::vec2(cell_px, cell_px));
                    painter.rect_filled(rect, 0.0, relief_color(relief));
                }
            }
            painter.circle_stroke(
                center,
                RADAR_RADIUS_PX,
                egui::Stroke::new(1.5, egui::Color32::from_gray(90)),
            );

            // Buildings — small owner-colored squares.
            for building in &buildings_q {
                let dx = (building.true_x - local_true.x) as f32;
                let dz = (building.true_z - local_true.z) as f32;
                if let Some(pos) = world_to_screen(center, dx, dz) {
                    let color = to_egui_color32(color_for_owner(building.owner_player_id));
                    let rect = egui::Rect::from_center_size(pos, egui::vec2(6.0, 6.0));
                    painter.rect_filled(rect, 1.0, color);
                }
            }

            // Villagers — tiny owner-colored dots.
            for villager in &villagers_q {
                let dx = (villager.true_x - local_true.x) as f32;
                let dz = (villager.true_z - local_true.z) as f32;
                if let Some(pos) = world_to_screen(center, dx, dz) {
                    let color = to_egui_color32(color_for_owner(villager.owner_player_id));
                    painter.circle_filled(pos, 2.0, color);
                }
            }

            // Remote cars — owner-colored dots, a bit larger than a villager's.
            for (snapshot, chassis) in &remote_cars_q {
                let true_pos = snapshot.translation.as_dvec3();
                let dx = (true_pos.x - local_true.x) as f32;
                let dz = (true_pos.z - local_true.z) as f32;
                if let Some(pos) = world_to_screen(center, dx, dz) {
                    let color = to_egui_color32(color_from_seed(chassis.color_seed));
                    painter.circle_filled(pos, 4.0, color);
                }
            }

            // Local player — a small triangle pointing along its heading.
            let heading = forward.x.atan2(forward.z);
            let tip = center + egui::vec2(heading.sin(), heading.cos()) * 8.0;
            let back_angle = 2.6;
            let left = center
                + egui::vec2((heading + back_angle).sin(), (heading + back_angle).cos()) * 6.0;
            let right = center
                + egui::vec2((heading - back_angle).sin(), (heading - back_angle).cos()) * 6.0;
            painter.add(egui::Shape::convex_polygon(
                vec![tip, left, right],
                egui::Color32::WHITE,
                egui::Stroke::NONE,
            ));
        });

    Ok(())
}
