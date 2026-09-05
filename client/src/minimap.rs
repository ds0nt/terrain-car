use bevy::prelude::*;
use shared::protocol::{CarSnapshot, LocalCar};
use shared::terrain_gen::{height_at, TerrainNoise};

use crate::worldspace::WorldOrigin;

/// Grid is `GRID_SIZE x GRID_SIZE`, odd so there's a true center cell for
/// the local car.
const GRID_SIZE: usize = 13;
/// Each cell represents this many meters of world space.
const CELL_METERS: f32 = 20.0;
/// Each cell is a fixed-size UI node — this (not font metrics) is what
/// keeps the "ASCII" grid aligned regardless of whether the default font is
/// actually monospace.
const CELL_PX: f32 = 11.0;
/// Redrawn a few times a second rather than every frame — a radar-sweep
/// feel fits a minimap anyway, and it avoids resampling terrain height for
/// every cell (each a handful of noise evaluations) at full framerate.
const REFRESH_HZ: f32 = 4.0;

pub struct MinimapPlugin;

impl Plugin for MinimapPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_minimap)
            .add_systems(Update, update_minimap);
    }
}

#[derive(Component)]
struct MinimapCell {
    row: usize,
    col: usize,
}

fn cell_text_style() -> (TextFont, TextColor) {
    (
        TextFont {
            font_size: bevy::text::FontSize::Px(CELL_PX * 1.6),
            ..default()
        },
        TextColor(Color::srgb(0.7, 0.95, 0.7)),
    )
}

fn spawn_minimap(mut commands: Commands) {
    let (font, color) = cell_text_style();
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                top: px(16.0),
                right: px(16.0),
                flex_direction: FlexDirection::Column,
                padding: UiRect::all(px(6.0)),
                ..default()
            },
            BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.4)),
        ))
        .with_children(|parent| {
            for row in 0..GRID_SIZE {
                parent
                    .spawn(Node {
                        flex_direction: FlexDirection::Row,
                        ..default()
                    })
                    .with_children(|row_node| {
                        for col in 0..GRID_SIZE {
                            row_node.spawn((
                                Node {
                                    width: px(CELL_PX),
                                    height: px(CELL_PX * 1.3),
                                    justify_content: JustifyContent::Center,
                                    align_items: AlignItems::Center,
                                    ..default()
                                },
                                children![(Text::new("."), font.clone(), color)],
                                MinimapCell { row, col },
                            ));
                        }
                    });
            }
        });
}

/// Character bands are by *local relief* (height relative to the local
/// car), not absolute height — so the minimap reads as "terrain near me,"
/// consistent with how hud.rs's world-type readout and
/// shared::obstacles::MAX_SLOPE-style checks already treat relief as the
/// meaningful quantity rather than raw elevation.
fn terrain_glyph(relief: f32) -> &'static str {
    if relief < -8.0 {
        " "
    } else if relief < -2.0 {
        "."
    } else if relief < 2.0 {
        "+"
    } else if relief < 8.0 {
        "^"
    } else {
        "#"
    }
}

#[allow(clippy::type_complexity)]
fn update_minimap(
    time: Res<Time>,
    mut last_update: Local<f32>,
    noise: Res<TerrainNoise>,
    origin: Res<WorldOrigin>,
    local_car_q: Query<&Transform, With<LocalCar>>,
    remote_cars_q: Query<&CarSnapshot, Without<LocalCar>>,
    cell_q: Query<(&MinimapCell, &Children)>,
    mut text_q: Query<&mut Text>,
) {
    *last_update += time.delta_secs();
    if *last_update < 1.0 / REFRESH_HZ {
        return;
    }
    *last_update = 0.0;

    let Ok(local_transform) = local_car_q.single() else {
        return;
    };
    let local_true = origin.to_true(local_transform.translation);
    let center = GRID_SIZE / 2;

    // Which cell each remote car currently falls into, if any — computed
    // once per refresh rather than per-cell, since there are far fewer
    // cars than grid cells.
    let mut remote_cells = std::collections::HashSet::new();
    for snapshot in &remote_cars_q {
        let true_pos = snapshot.translation.as_dvec3();
        let dx = ((true_pos.x - local_true.x) as f32 / CELL_METERS).round() as i32;
        let dz = ((true_pos.z - local_true.z) as f32 / CELL_METERS).round() as i32;
        let col = center as i32 + dx;
        let row = center as i32 + dz;
        if (0..GRID_SIZE as i32).contains(&col) && (0..GRID_SIZE as i32).contains(&row) {
            remote_cells.insert((row as usize, col as usize));
        }
    }

    for (cell, children) in &cell_q {
        let Some(&child) = children.first() else {
            continue;
        };
        let Ok(mut text) = text_q.get_mut(child) else {
            continue;
        };

        let glyph = if cell.row == center && cell.col == center {
            "@"
        } else if remote_cells.contains(&(cell.row, cell.col)) {
            "O"
        } else {
            let world_x = local_true.x + (cell.col as f32 - center as f32) as f64 * CELL_METERS as f64;
            let world_z = local_true.z + (cell.row as f32 - center as f32) as f64 * CELL_METERS as f64;
            let ground_y = height_at(&noise, world_x, world_z);
            terrain_glyph(ground_y - local_transform.translation.y)
        };

        if text.0 != glyph {
            text.0 = glyph.to_string();
        }
    }
}
