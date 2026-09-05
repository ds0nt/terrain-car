use bevy::light::atmosphere::ScatteringMedium;
use bevy::light::light_consts::lux;
use bevy::light::{Atmosphere, CascadeShadowConfigBuilder, DirectionalLightShadowMap};
use bevy::prelude::*;

pub struct LightingPlugin;

impl Plugin for LightingPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(DirectionalLightShadowMap { size: 4096 })
            // Real atmospheric scattering (see spawn_atmosphere, and the
            // AtmosphereEnvironmentMapLight on the camera in camera.rs)
            // drives ambient/IBL lighting from actual sky color instead of a
            // flat fill tint, which reads much better for telling elevation
            // and terrain features apart than a uniform ambient ever could.
            .insert_resource(GlobalAmbientLight::NONE)
            .add_systems(Startup, (spawn_sun, spawn_atmosphere));
    }
}

fn spawn_sun(mut commands: Commands) {
    commands.spawn((
        DirectionalLight {
            shadow_maps_enabled: true,
            // RAW_SUNLIGHT is the illuminance Bevy's atmosphere scattering
            // expects: the atmosphere itself is what dims/colors it into
            // believable daylight, so feeding it an already-tonemapped
            // "sunny day" value would double-darken everything.
            illuminance: lux::RAW_SUNLIGHT,
            ..default()
        },
        // Low-ish angle: longer, more dramatic shadows than a straight-down
        // noon sun, and easier to read the terrain's shape by.
        Transform::from_xyz(-0.5, 0.35, -0.35).looking_at(Vec3::ZERO, Vec3::Y),
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
