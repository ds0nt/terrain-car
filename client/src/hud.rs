use bevy::prelude::*;
use bevy_rapier3d::prelude::Velocity;

use shared::combat::Health;
use shared::protocol::Wallet;
use shared::terrain_gen::{biome_label, TerrainNoise};

use crate::car::LocalCar;
use crate::recorder::RecordingActive;
use crate::worldspace::WorldOrigin;

pub struct HudPlugin;

impl Plugin for HudPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_hud)
            .add_systems(Update, update_hud);
    }
}

/// Tags each HUD text node with which field it displays — one query in
/// `update_hud` matches all of them and switches on this, instead of a
/// separate `Query<&mut Text, (With<X>, Without<Y>, Without<Z>, ...)>` per
/// field (the old shape, which meant every new field added one more
/// `Without` to every other field's query).
#[derive(Component, Clone, Copy, PartialEq, Eq)]
enum HudField {
    WorldType,
    Speed,
    Altitude,
    GForce,
    Health,
    Wallet,
    Rec,
}

fn hud_text_style() -> (TextFont, TextColor) {
    (
        TextFont {
            font_size: bevy::text::FontSize::Px(22.0),
            ..default()
        },
        TextColor(Color::srgb(0.85, 0.95, 1.0)),
    )
}

fn spawn_hud(mut commands: Commands) {
    let (font, color) = hud_text_style();
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                left: px(16.0),
                bottom: px(16.0),
                flex_direction: FlexDirection::Column,
                padding: UiRect::all(px(10.0)),
                row_gap: px(4.0),
                ..default()
            },
            BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.35)),
        ))
        .with_children(|parent| {
            parent.spawn((Text::new("WORLD  ---"), font.clone(), color, HudField::WorldType));
            parent.spawn((Text::new("SPD    0 km/h"), font.clone(), color, HudField::Speed));
            parent.spawn((Text::new("ALT    0 m"), font.clone(), color, HudField::Altitude));
            parent.spawn((Text::new("G      1.00"), font.clone(), color, HudField::GForce));
            parent.spawn((Text::new("HEALTH 100%"), font.clone(), color, HudField::Health));
            parent.spawn((Text::new("ENERGY 0  ORE 0"), font.clone(), color, HudField::Wallet));
            parent.spawn((
                Text::new(""),
                font,
                TextColor(Color::srgb(1.0, 0.25, 0.25)),
                HudField::Rec,
            ));
        });
}

fn update_hud(
    time: Res<Time>,
    noise: Res<TerrainNoise>,
    origin: Res<WorldOrigin>,
    recording: Res<RecordingActive>,
    chassis_q: Query<(&GlobalTransform, &Velocity, &Health, &Wallet), With<LocalCar>>,
    mut prev_vertical_speed: Local<f32>,
    mut fields_q: Query<(&mut Text, &HudField)>,
) {
    let Ok((chassis_gt, velocity, health, wallet)) = chassis_q.single() else {
        return;
    };
    let dt = time.delta_secs();

    let speed_kmh = velocity.linear.length() * 3.6;
    let altitude = chassis_gt.translation().y;

    let true_pos = origin.to_true(chassis_gt.translation());
    let world_type = biome_label(&noise, true_pos.x, true_pos.z);

    // A g-meter: net vertical acceleration including gravity, the same
    // quantity an accelerometer would read. Sits at 1.00 at rest (ground
    // pushing back against gravity), drops toward 0 in freefall off a
    // cliff, and spikes on landing impacts — exactly the moments "insane
    // terrain" needs a readout for.
    let vertical_speed = velocity.linear.y;
    let vertical_accel = if dt > 0.0 {
        (vertical_speed - *prev_vertical_speed) / dt
    } else {
        0.0
    };
    *prev_vertical_speed = vertical_speed;
    let g_force = (vertical_accel + 9.81) / 9.81;

    for (mut text, field) in &mut fields_q {
        text.0 = match field {
            HudField::WorldType => format!("WORLD  {world_type}"),
            HudField::Speed => format!("SPD  {speed_kmh:>5.0} km/h"),
            HudField::Altitude => format!("ALT  {altitude:>6.1} m"),
            HudField::GForce => format!("G    {g_force:>6.2}"),
            HudField::Health => format!("HEALTH {:>3.0}%", health.fraction() * 100.0),
            HudField::Wallet => {
                format!("ENERGY {:>4.0}  ORE {:>4.0}", wallet.energy, wallet.ore)
            }
            HudField::Rec => {
                if recording.0 {
                    "REC  ●".to_string()
                } else {
                    String::new()
                }
            }
        };
    }
}
