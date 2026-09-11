use bevy::diagnostic::{DiagnosticsStore, FrameTimeDiagnosticsPlugin};
use bevy::prelude::*;

use shared::combat::Health;
use shared::protocol::Wallet;
use shared::terrain_gen::{biome_label, TerrainNoise};

use shared::tank_physics::TankChassis;

use crate::aircraft::PlaneInput;
use crate::car::{CarChassis, DrivingCarId, LocalCar};
use crate::pilot::{ControlMode, PlayerFocus};
use crate::player_account::LocalPlayerAccount;
use crate::recorder::RecordingActive;
use crate::tank::{DrivingTankId, LocalTank};
use crate::worldspace::WorldOrigin;

pub struct HudPlugin;

impl Plugin for HudPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_hud).add_systems(
            Update,
            (update_hud, update_crosshair_visibility, update_plane_stick_indicator),
        );
    }
}

/// A simple `+` reticle — shown while the cursor is locked/hidden for
/// mouse-look (`Plane`, `OnFoot`; see `pilot.rs`'s `manage_cursor_confinement`),
/// hidden in `Car` or whenever `MenuOpen` (`Alt`) hands the cursor back for
/// clicking, since there's a real, visible cursor to look at in both of
/// those cases instead. This is also, functionally, where
/// `building_placement.rs`/`selection.rs` actually aim from otherwise
/// (`pilot::aim_position`) — showing it isn't just decoration, it's the
/// only visual indication of where a click will land once the real cursor
/// is gone.
///
/// Centered by default (`Car`/`OnFoot`), but in `Plane` mode it's re-used
/// as a *stick-position* indicator instead (see
/// `update_plane_stick_indicator`) — `half_size` is stored so that system
/// can recompute each bar's centering margin plus a pixel offset without
/// needing to know each bar's own dimensions again.
#[derive(Component)]
struct Crosshair {
    half_size: Vec2,
}

/// A small, dim, always-centered reference dot — shown only in `Plane`
/// mode, marking "stick neutral" so the moving crosshair (see
/// `update_plane_stick_indicator`) has something fixed to read its offset
/// against. Meaningless in `Car`/`OnFoot`, where the crosshair itself
/// always sits dead-center already.
#[derive(Component)]
struct StickCenterMarker;

fn update_crosshair_visibility(
    mode: Res<ControlMode>,
    menu_open: Res<crate::pilot::MenuOpen>,
    mut crosshair_q: Query<&mut Visibility, With<Crosshair>>,
    mut center_q: Query<&mut Visibility, (With<StickCenterMarker>, Without<Crosshair>)>,
) {
    if !mode.is_changed() && !menu_open.is_changed() {
        return;
    }
    let crosshair_visible = if *mode == ControlMode::Car
        || *mode == ControlMode::Passenger
        || *mode == ControlMode::Tank
        || *mode == ControlMode::TurretOperator
        || menu_open.0
    {
        Visibility::Hidden
    } else {
        Visibility::Visible
    };
    for mut v in &mut crosshair_q {
        *v = crosshair_visible;
    }
    let center_visible = if *mode == ControlMode::Plane && !menu_open.0 {
        Visibility::Visible
    } else {
        Visibility::Hidden
    };
    for mut v in &mut center_q {
        *v = center_visible;
    }
}

/// How far (screen pixels) the crosshair moves off-center at full stick
/// deflection (`PlaneInput::pitch`/`roll` at ±1.0) — small enough to stay
/// comfortably inside the crosshair's usual on-screen neighborhood, large
/// enough that the offset is unambiguous at a glance even at partial
/// deflection.
const STICK_INDICATOR_RADIUS_PX: f32 = 45.0;

/// Repurposes the crosshair as a live stick-position indicator while
/// flying — see this module's top-level docs on why the fixed center
/// crosshair alone gives no sense of *how far* the mouse-driven virtual
/// stick (`aircraft::PlaneInput`) is currently pushed, only where "level"
/// is. Offsets each bar's already-centering margin by the current
/// roll/pitch, scaled to screen pixels; snaps back to a plain centered
/// offset the instant you're not flying, so the crosshair reads normally
/// again in `Car`/`OnFoot`. Mouse Y is screen-down-positive but pitching
/// the nose *up* should move the indicator *up* the screen, hence the
/// negation there; roll needs the same negation (confirmed live — pushing
/// the mouse right read as the indicator moving left) despite
/// `read_plane_input` itself needing the opposite sign to make the plane
/// actually bank the intuitive way, since the two aren't required to
/// agree: `fly_planes` reads `roll` as a *rotation rate*, so which literal
/// sign banks "right" depends on the plane's local axis convention, not on
/// which way the indicator should visually move for the same input.
fn update_plane_stick_indicator(
    mode: Res<ControlMode>,
    input: Res<PlaneInput>,
    mut crosshair_q: Query<(&Crosshair, &mut Node)>,
) {
    let offset = if *mode == ControlMode::Plane {
        Vec2::new(-input.roll, -input.pitch) * STICK_INDICATOR_RADIUS_PX
    } else {
        Vec2::ZERO
    };
    for (crosshair, mut node) in &mut crosshair_q {
        node.margin = UiRect::new(
            px(-crosshair.half_size.x + offset.x),
            Val::Auto,
            px(-crosshair.half_size.y + offset.y),
            Val::Auto,
        );
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
    Fps,
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
            parent.spawn((Text::new("FPS    0"), font.clone(), color, HudField::Fps));
            parent.spawn((
                Text::new(""),
                font,
                TextColor(Color::srgb(1.0, 0.25, 0.25)),
                HudField::Rec,
            ));
        });

    // Crosshair: two thin bars forming a `+`, pinned to the exact center of
    // the screen — `left`/`top` at 50% plus a negative margin of half the
    // bar's own width/height, the usual way to center a fixed-size
    // absolutely-positioned element regardless of screen resolution.
    // Starts visible (`Car` is `ControlMode`'s own default, but nothing
    // sets `Visibility` here) until `update_crosshair_visibility` corrects
    // it the moment it runs — resources always report as "changed" on the
    // tick they're inserted, so that happens on the very first frame.
    // `update_plane_stick_indicator` overwrites this same margin every
    // frame while flying, offsetting it by the current stick position.
    for (w, h) in [(2.0, 16.0), (16.0, 2.0)] {
        commands.spawn((
            Node {
                position_type: PositionType::Absolute,
                left: Val::Percent(50.0),
                top: Val::Percent(50.0),
                width: px(w),
                height: px(h),
                margin: UiRect::new(px(-w / 2.0), Val::Auto, px(-h / 2.0), Val::Auto),
                ..default()
            },
            BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.85)),
            Crosshair { half_size: Vec2::new(w / 2.0, h / 2.0) },
        ));
    }

    // Stick-neutral reference dot — see `StickCenterMarker`'s own docs.
    // Starts hidden (only `Plane` mode ever shows it); dim so it reads as
    // a subtle reference point, not a second competing reticle.
    const CENTER_DOT_SIZE: f32 = 5.0;
    commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: Val::Percent(50.0),
            top: Val::Percent(50.0),
            width: px(CENTER_DOT_SIZE),
            height: px(CENTER_DOT_SIZE),
            margin: UiRect::new(
                px(-CENTER_DOT_SIZE / 2.0),
                Val::Auto,
                px(-CENTER_DOT_SIZE / 2.0),
                Val::Auto,
            ),
            ..default()
        },
        BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.35)),
        Visibility::Hidden,
        StickCenterMarker,
    ));
}

/// Position/velocity readouts (`Speed`, `Altitude`, `WorldType`, `GForce`)
/// come from `PlayerFocus` — whatever the player currently occupies, car,
/// plane, or on foot (see that resource's own docs). `Health` stays a
/// direct `LocalCar` query on purpose: it's genuinely car-intrinsic state
/// (your car's own condition), not "wherever you currently are." `Wallet`
/// reads the player's own account entity (`LocalPlayerAccount`) instead —
/// it moved off the car entirely (see `shared::protocol`'s docs) so it
/// still exists even before you've ever built a Hangar.
///
/// Both are genuinely optional now, independently of each other and of
/// position: a fresh, on-foot, car-less player has a `Wallet` (their
/// account always exists once logged in) but no `Health` (no car yet) —
/// showing "no car" for the health line must never also block the
/// position/speed fields above from updating, which is exactly the bug an
/// earlier single combined `car_q.single()` early-return would reintroduce.
#[allow(clippy::too_many_arguments)]
fn update_hud(
    time: Res<Time>,
    noise: Res<TerrainNoise>,
    origin: Res<WorldOrigin>,
    focus: Res<PlayerFocus>,
    recording: Res<RecordingActive>,
    driving_car: Res<DrivingCarId>,
    driving_tank: Res<DrivingTankId>,
    diagnostics: Res<DiagnosticsStore>,
    car_q: Query<(&Health, &CarChassis), With<LocalCar>>,
    tank_q: Query<(&Health, &TankChassis), With<LocalTank>>,
    account_q: Query<&Wallet, With<LocalPlayerAccount>>,
    mut prev_vertical_speed: Local<f32>,
    mut biome_cache: Local<Option<(bevy::math::DVec3, &'static str)>>,
    mut fields_q: Query<(&mut Text, &HudField)>,
) {
    // The car you're actually driving (`DrivingCarId`), not `.iter().next()`
    // (an arbitrary owned car) — a player can own several cars now (see
    // `car.rs`'s top-level docs), and showing some other parked car's
    // health instead of the one you're sitting in was exactly the reported
    // "when I join a car it should be the correct car" bug. Falls back to
    // the driven tank's own health (same id-matched lookup) when there's no
    // driven car — the two are mutually exclusive by `ControlMode`, so at
    // most one of these ever actually finds something.
    let health = car_q
        .iter()
        .find(|(_, chassis)| Some(chassis.car_id) == driving_car.0)
        .map(|(h, _)| h)
        .or_else(|| tank_q.iter().find(|(_, chassis)| Some(chassis.tank_id) == driving_tank.0).map(|(h, _)| h));
    let wallet = account_q.single().ok();
    // `smoothed()`, not the raw instantaneous value — a per-frame FPS
    // number jitters wildly frame to frame (one slightly slower frame
    // reads as a huge dip), the smoothed rolling average is what actually
    // reads as a stable, useful number at a glance.
    let fps = diagnostics
        .get(&FrameTimeDiagnosticsPlugin::FPS)
        .and_then(|d| d.smoothed())
        .unwrap_or(0.0);
    let dt = time.delta_secs();

    let speed_kmh = focus.linear_velocity.length() * 3.6;
    let altitude = focus.translation.y;

    let true_pos = origin.to_true(focus.translation);
    // `biome_label` samples `climate_at`'s noise layers — cheap next to
    // `height_at`'s (see `minimap.rs`'s own docs on that one), but still
    // real, avoidable cost to pay every single frame for a label that only
    // needs to change when you've crossed into meaningfully different
    // terrain — the underlying noise wavelengths here span thousands of
    // meters, so 50m of slack is imperceptible.
    const BIOME_RESAMPLE_DISTANCE: f64 = 50.0;
    let world_type = match *biome_cache {
        Some((last_pos, label)) if true_pos.distance(last_pos) <= BIOME_RESAMPLE_DISTANCE => label,
        _ => {
            let label = biome_label(&noise, true_pos.x, true_pos.z);
            *biome_cache = Some((true_pos, label));
            label
        }
    };

    // A g-meter: net vertical acceleration including gravity, the same
    // quantity an accelerometer would read. Sits at 1.00 at rest (ground
    // pushing back against gravity), drops toward 0 in freefall off a
    // cliff, and spikes on landing impacts — exactly the moments "insane
    // terrain" needs a readout for.
    let vertical_speed = focus.linear_velocity.y;
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
            HudField::Health => match health {
                Some(health) => format!("HEALTH {:>3.0}%", health.fraction() * 100.0),
                None => "HEALTH  --".to_string(),
            },
            HudField::Wallet => match wallet {
                Some(wallet) => format!("ENERGY {:>4.0}  ORE {:>4.0}", wallet.energy, wallet.ore),
                None => "ENERGY  --   ORE  --".to_string(),
            },
            HudField::Fps => format!("FPS  {fps:>5.0}"),
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
