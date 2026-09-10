use bevy::light::atmosphere::ScatteringMedium;
use bevy::light::light_consts::lux;
use bevy::light::{Atmosphere, CascadeShadowConfigBuilder, DirectionalLightShadowMap};
use bevy::prelude::*;

pub struct LightingPlugin;

impl Plugin for LightingPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(DirectionalLightShadowMap { size: 4096 })
            // A cool "starlight" fill on top of the atmosphere's own
            // scattered light (see spawn_atmosphere/spawn_sun) — needs to
            // be strong enough that shadow-side terrain still reads as
            // "dark and moody" rather than "unlit and invisible." An
            // earlier, much lower value (12.0) plus a sun actually placed
            // below the horizon made the whole scene borderline unplayable
            // — nothing wrong with the idea, just tuned for "photo of
            // Mars at night" over "a game you can actually see," and this
            // is a game first. Blue-purple tint keeps it reading as night
            // fill, not a second sun, even at this brightness.
            .insert_resource(GlobalAmbientLight {
                color: Color::srgb(0.35, 0.38, 0.55),
                brightness: 45.0,
                affects_lightmapped_meshes: true,
            })
            .add_systems(Startup, (spawn_sun, spawn_atmosphere));
    }
}

fn spawn_sun(mut commands: Commands) {
    // `TERRAIN_CAR_LOW_GRAPHICS=1 ./terrain_car` drops real-time cascaded
    // shadows — on a weaker/integrated GPU, shadow-mapped terrain plus
    // every streamed obstacle is typically the single biggest per-frame
    // cost in this scene, well above the atmosphere/particle/shader work.
    // A one-off env var rather than an in-game settings menu: there's no
    // UI for graphics options yet, and this only ever needs setting once
    // per machine.
    let shadows_enabled = std::env::var("TERRAIN_CAR_LOW_GRAPHICS").is_err();
    commands.spawn((
        DirectionalLight {
            shadow_maps_enabled: shadows_enabled,
            // Still RAW_SUNLIGHT, not a dimmed-down "night" value: the
            // atmosphere shader expects true, undimmed sun illuminance as
            // input and does its own physically-based darkening from the
            // light's angle (see this bundle's `Transform`, now below the
            // horizon) — feeding it an already-dim value here would
            // double-darken on top of that.
            illuminance: lux::RAW_SUNLIGHT,
            // A dim, dying-ember warm tint rather than white — the only
            // light actually reaching the ground at this angle is
            // atmosphere-scattered glow along the horizon, and tinting the
            // source itself toward Mars' rust palette (rather than a cool
            // white/blue "moonlight" look) is what keeps a mostly-dark
            // scene still reading as *Mars* night, not just "night."
            color: Color::srgb(1.0, 0.55, 0.35),
            ..default()
        },
        // A shallow, just-above-the-horizon angle (positive but small y) —
        // a genuinely *below*-horizon sun (what this was originally set
        // to) leaves the atmosphere with essentially nothing to scatter
        // toward the ground at all, which combined with a modest ambient
        // fill read as "broken and unlit" rather than "night," per live
        // feedback ("too dark i cant seeeee"). This is Bevy's real
        // atmospheric scattering, so the same physically-based angle
        // falloff that makes dusk/dawn dim and warm on Earth does the
        // heavy lifting here too — a sun this close to the horizon still
        // casts real (dim, long, warm-tinted) light and shadows without
        // needing an artificial illuminance cut, while staying nowhere
        // near full daylight brightness. Same low, dramatic x/z ratio the
        // original daytime sun used.
        Transform::from_xyz(-0.5, 0.12, -0.35).looking_at(Vec3::ZERO, Vec3::Y),
        CascadeShadowConfigBuilder {
            // Terrain streams in out to ~3 chunks (480m); shadows only need
            // to cover the near/mid field the player actually looks at.
            maximum_distance: 250.0,
            ..default()
        }
        .build(),
    ));
}

fn spawn_atmosphere(mut commands: Commands, mut media: ResMut<Assets<ScatteringMedium>>) {
    let earth = media.add(ScatteringMedium::earth(128, 128));
    commands.spawn(Atmosphere::earth(earth));
}
