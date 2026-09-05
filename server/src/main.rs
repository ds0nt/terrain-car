mod car_sim;
mod console;
mod lightning;
mod net;
mod physics_fx;
mod terrain_phys;

use std::time::Duration;

use bevy::app::ScheduleRunnerPlugin;
use bevy::log::LogPlugin;
use bevy::prelude::*;
use bevy::state::app::StatesPlugin;
use bevy_rapier3d::prelude::*;
use bevy_replicon::prelude::*;
use shared::protocol::register_protocol;

fn main() {
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
        terrain_phys::ServerTerrainPlugin,
        car_sim::CarSimPlugin,
        console::ConsolePlugin,
        lightning::LightningPlugin,
    ));

    // Registers replicated components/events — must use the exact same
    // function the client calls, so wire IDs (assigned in registration
    // order) can never drift between them.
    register_protocol(&mut app);

    app.run();
}
