mod aircraft;
mod auth_ui;
mod building_placement;
mod building_render;
mod building_ui;
mod camera;
mod car;
mod car_render;
mod chat;
mod cosmetics_ui;
mod fx;
mod hud;
mod lighting;
mod lightning_fx;
mod minimap;
mod net;
mod owner_color;
mod pilot;
mod pings;
mod player_account;
mod player_markers;
mod players_ui;
mod recorder;
mod remote_players;
mod selection;
mod settings;
mod stars;
mod terrain;
mod terrain_material;
mod villager_render;
mod weapon_fx;
mod worldspace;

use bevy::prelude::*;
use bevy_rapier3d::prelude::*;
use bevy_replicon::prelude::*;
use shared::protocol::register_protocol;

fn main() {
    let mut app = App::new();

    app.init_resource::<net::LocalClientId>()
        // A near-black, faint red-tinted night sky (was a bright daytime
        // blue) — wherever the atmosphere/skybox doesn't cover, this is
        // what shows, and a bright clear color there would fight the
        // Mars-at-night look (`lighting.rs`) no matter how dark the actual
        // sun/ambient light are set.
        .insert_resource(ClearColor(Color::srgb(0.02, 0.015, 0.02)))
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
            fx::FxPlugin,
            lightning_fx::LightningFxPlugin,
            bevy_hanabi::HanabiPlugin,
            weapon_fx::WeaponFxPlugin,
            building_ui::BuildingUiPlugin,
            building_render::BuildingRenderPlugin,
            building_placement::BuildingPlacementPlugin,
            selection::SelectionPlugin,
            villager_render::VillagerRenderPlugin,
            auth_ui::AuthUiPlugin,
        ))
        .add_plugins((
            pings::PingsPlugin,
            players_ui::PlayersUiPlugin,
            player_markers::PlayerMarkersPlugin,
            cosmetics_ui::CosmeticsUiPlugin,
            aircraft::AircraftPlugin,
            pilot::PilotPlugin,
            player_account::PlayerAccountPlugin,
            stars::StarsPlugin,
            chat::ChatPlugin,
            remote_players::RemotePlayersPlugin,
            settings::SettingsPlugin,
            // Feeds `hud.rs`'s FPS readout — `DiagnosticsStore` stays empty
            // without this registered somewhere.
            bevy::diagnostic::FrameTimeDiagnosticsPlugin::default(),
        ));

    // Registers replicated components/events — must use the exact same
    // function the server calls, so wire IDs (assigned in registration
    // order) can never drift between them.
    register_protocol(&mut app);

    app.run();
}
