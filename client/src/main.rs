mod building_render;
mod building_ui;
mod camera;
mod car;
mod car_render;
mod fx;
mod hud;
mod lighting;
mod lightning_fx;
mod minimap;
mod net;
mod prediction;
mod recorder;
mod tectonic;
mod terrain;
mod terrain_material;
mod tuning_ui;
mod weapon_fx;
mod worldspace;

use bevy::prelude::*;
use bevy_rapier3d::prelude::*;
use bevy_replicon::prelude::*;
use shared::protocol::register_protocol;

fn main() {
    let mut app = App::new();

    app.init_resource::<net::LocalClientId>()
        .insert_resource(ClearColor(Color::srgb(0.55, 0.75, 0.95)))
        // `TimestepMode` is a standalone global resource (not a field of
        // `RapierConfiguration`), and `RapierPhysicsPlugin::build()` only
        // `init_resource`s it (won't overwrite an existing value) — so
        // inserting our own before `add_plugins` is how you override it.
        // Switches from the default `Variable` (steps by actual, jittery
        // render frame time) to `Fixed` at 64 Hz, matching Bevy's own
        // default `FixedUpdate` rate and the server's identical setting
        // (see server's main.rs) — both sides' physics must advance in
        // identical, deterministic steps for reconciliation to make sense.
        .insert_resource(TimestepMode::Fixed {
            dt: 1.0 / 64.0,
            substeps: 1,
        })
        .add_plugins((
            DefaultPlugins,
            // Moves Rapier's own step systems into FixedUpdate (default
            // PostUpdate otherwise), so they run in lockstep with car.rs's
            // force-application system, also moved to FixedUpdate.
            RapierPhysicsPlugin::<NoUserData>::default().in_fixed_schedule(),
            RepliconPlugins,
            net::ClientNetPlugin,
            worldspace::WorldSpacePlugin,
            terrain_material::TerrainMaterialPlugin,
            terrain::TerrainPlugin,
            car::CarPlugin,
            car_render::CarRenderPlugin,
            prediction::PredictionPlugin,
            camera::CameraPlugin,
            lighting::LightingPlugin,
            hud::HudPlugin,
        ))
        // `add_plugins` only accepts tuples up to a fixed arity, and the
        // list above is already at that limit — a second call rather than
        // one giant tuple.
        .add_plugins((
            minimap::MinimapPlugin,
            recorder::RecorderPlugin,
            tectonic::TectonicPlugin,
            fx::FxPlugin,
            lightning_fx::LightningFxPlugin,
            bevy_hanabi::HanabiPlugin,
            weapon_fx::WeaponFxPlugin,
            tuning_ui::TuningUiPlugin,
            building_ui::BuildingUiPlugin,
            building_render::BuildingRenderPlugin,
        ));

    // Registers replicated components/events — must use the exact same
    // function the server calls, so wire IDs (assigned in registration
    // order) can never drift between them.
    register_protocol(&mut app);

    app.run();
}
