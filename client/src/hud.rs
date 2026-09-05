use bevy::prelude::*;
use bevy_rapier3d::prelude::Velocity;

use shared::combat::Health;
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

#[derive(Component)]
struct SpeedText;
#[derive(Component)]
struct AltitudeText;
#[derive(Component)]
struct GForceText;
#[derive(Component)]
struct HealthText;
#[derive(Component)]
struct WorldTypeText;
#[derive(Component)]
struct RecText;

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
            parent.spawn((
                Text::new("WORLD  ---"),
                font.clone(),
                color,
                WorldTypeText,
            ));
            parent.spawn((Text::new("SPD    0 km/h"), font.clone(), color, SpeedText));
            parent.spawn((Text::new("ALT    0 m"), font.clone(), color, AltitudeText));
            parent.spawn((Text::new("G      1.00"), font.clone(), color, GForceText));
            parent.spawn((Text::new("HEALTH 100%"), font.clone(), color, HealthText));
            parent.spawn((
                Text::new(""),
                font,
                TextColor(Color::srgb(1.0, 0.25, 0.25)),
                RecText,
            ));
        });
}

#[allow(clippy::type_complexity)]
fn update_hud(
    time: Res<Time>,
    noise: Res<TerrainNoise>,
    origin: Res<WorldOrigin>,
    recording: Res<RecordingActive>,
    chassis_q: Query<(&GlobalTransform, &Velocity, &Health), With<LocalCar>>,
    mut prev_vertical_speed: Local<f32>,
    mut world_q: Query<
        &mut Text,
        (
            With<WorldTypeText>,
            Without<SpeedText>,
            Without<AltitudeText>,
            Without<GForceText>,
            Without<HealthText>,
            Without<RecText>,
        ),
    >,
    mut speed_q: Query<
        &mut Text,
        (
            With<SpeedText>,
            Without<AltitudeText>,
            Without<GForceText>,
            Without<HealthText>,
            Without<RecText>,
        ),
    >,
    mut alt_q: Query<
        &mut Text,
        (
            With<AltitudeText>,
            Without<SpeedText>,
            Without<GForceText>,
            Without<HealthText>,
            Without<RecText>,
        ),
    >,
    mut g_q: Query<
        &mut Text,
        (
            With<GForceText>,
            Without<SpeedText>,
            Without<AltitudeText>,
            Without<HealthText>,
            Without<RecText>,
        ),
    >,
    mut health_q: Query<
        &mut Text,
        (
            With<HealthText>,
            Without<SpeedText>,
            Without<AltitudeText>,
            Without<GForceText>,
            Without<RecText>,
        ),
    >,
    mut rec_q: Query<&mut Text, With<RecText>>,
) {
    let Ok((chassis_gt, velocity, health)) = chassis_q.single() else {
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

    if let Ok(mut text) = world_q.single_mut() {
        text.0 = format!("WORLD  {world_type}");
    }
    if let Ok(mut text) = speed_q.single_mut() {
        text.0 = format!("SPD  {speed_kmh:>5.0} km/h");
    }
    if let Ok(mut text) = alt_q.single_mut() {
        text.0 = format!("ALT  {altitude:>6.1} m");
    }
    if let Ok(mut text) = g_q.single_mut() {
        text.0 = format!("G    {g_force:>6.2}");
    }
    if let Ok(mut text) = health_q.single_mut() {
        text.0 = format!("HEALTH {:>3.0}%", health.fraction() * 100.0);
    }
    if let Ok(mut text) = rec_q.single_mut() {
        text.0 = if recording.0 { "REC  ●".to_string() } else { String::new() };
    }
}
