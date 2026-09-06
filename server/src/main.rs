mod auth;
mod car_sim;
mod console;
mod economy;
mod lightning;
mod net;
mod persistence;
mod physics_fx;
mod terrain_phys;
mod villagers;
mod weapons;

use std::time::Duration;

use bevy::app::ScheduleRunnerPlugin;
use bevy::log::LogPlugin;
use bevy::prelude::*;
use bevy::state::app::StatesPlugin;
use bevy_rapier3d::prelude::*;
use bevy_replicon::prelude::*;
use shared::protocol::register_protocol;

fn main() {
    // Loads `.env` (workspace-root-relative, matching how this binary is
    // normally run — `cargo run -p server` / `./target/debug/
    // terrain_car_server` from the repo root) into the process environment
    // before anything reads `SUPABASE_DB_URL`. Silently does nothing if no
    // `.env` exists (e.g. a real deployment setting the env var directly)
    // — `persistence.rs` already logs its own clear warning if the
    // variable ends up unset either way, so there's nothing useful to add
    // here before `LogPlugin` even exists yet to log through.
    dotenvy::dotenv().ok();

    let mut app = App::new();

    // `TimestepMode` must be set before `RapierPhysicsPlugin` (which only
    // `init_resource`s it, so it won't overwrite an existing value) —
    // matches the client's fixed-64Hz choice exactly, since client-side
    // reconciliation replay assumes both sides step by the same dt. See
    // client's main.rs for the fuller explanation.
    app.insert_resource(TimestepMode::Fixed {
        dt: 1.0 / 64.0,
        substeps: 1,
    })
    .add_plugins((
        // Headless: no window, no renderer. `ScheduleRunnerPlugin`'s
        // default (included in `MinimalPlugins`) busy-spins the main loop
        // as fast as possible; `run_loop` paces it to roughly the physics
        // tick rate instead, since nothing else (no vsync) would
        // otherwise throttle it.
        MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(Duration::from_secs_f64(
            1.0 / 64.0,
        ))),
        // `bevy_replicon` needs `States`, Rapier needs Transform
        // propagation, and without `LogPlugin` every `info!`/`warn!` call
        // silently no-ops (no tracing subscriber installed) — all three
        // normally come from `DefaultPlugins`, but `MinimalPlugins`
        // (deliberately) includes none of them.
        StatesPlugin,
        TransformPlugin,
        LogPlugin::default(),
        RapierPhysicsPlugin::<NoUserData>::default().in_fixed_schedule(),
        RepliconPlugins,
        net::ServerNetPlugin,
        persistence::PersistencePlugin,
        terrain_phys::ServerTerrainPlugin,
        car_sim::CarSimPlugin,
        console::ConsolePlugin,
        lightning::LightningPlugin,
        weapons::WeaponsPlugin,
        economy::EconomyPlugin,
        villagers::VillagersPlugin,
    ))
    .add_plugins(auth::AuthPlugin);

    // Registers replicated components/events — must use the exact same
    // function the client calls, so wire IDs (assigned in registration
    // order) can never drift between them.
    register_protocol(&mut app);

    app.run();
}
