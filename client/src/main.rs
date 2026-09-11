mod aircraft;
mod audio;
mod auth_ui;
mod building_placement;
mod building_render;
mod building_ui;
mod camera;
mod car;
mod car_render;
mod chat;
mod cosmetics_ui;
mod dropship;
mod dropship_render;
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
mod render_scale;
mod selection;
mod settings;
mod stars;
mod tank;
mod tank_render;
mod terrain;
mod terrain_material;
mod thrusters;
mod turret_control;
mod turret_render;
mod villager_render;
mod weapon_fx;
mod worldspace;

use bevy::app::{TaskPoolOptions, TaskPoolPlugin, TaskPoolThreadAssignmentPolicy};
use bevy::prelude::*;
use bevy::window::{PresentMode, Window, WindowPlugin};
use bevy_rapier3d::prelude::*;
use bevy_replicon::prelude::*;
use shared::protocol::register_protocol;

// Default `TaskPoolOptions::compute` uses *every* logical core left over
// after io/async_compute are assigned (percent: 1.0, max_threads: usize::MAX)
// — on a 24-thread machine that's a 16-thread compute pool. Profiling a real
// play session (`samply`, see the conversation this was added from) showed
// ~73% of *all* sampled CPU time went into that pool's own executor
// machinery (futex syscalls, mutex contention, work-stealing) rather than
// the actual parallel work (frustum culling etc.) it exists to do.
//
// Tried both 8 and 2 threads. Actual in-game FPS/frame-time (not just the
// profiler's CPU-time breakdown) showed capping at all cuts jitter a lot
// (frame-time stdev roughly halved vs. uncapped), but going from 8 -> 2
// threads bought nothing further — same mean/stdev FPS despite the
// profiler showing CPU time shift off the pool. 8 is the settled default,
// but `TERRAIN_CAR_COMPUTE_THREADS=<n>` overrides it at runtime (no
// rebuild) — for a remote player (weaker/fewer-core hardware than this was
// tuned on) to try different caps directly, same spirit as
// `TERRAIN_CAR_VSYNC`. Unlike pinning the whole process with `taskset`,
// this only changes the compute pool specifically — `taskset` also shrinks
// what `available_parallelism()` reports, which resizes the io/async_compute
// pools too and conflates "fewer cores" with "smaller compute cap."
const DEFAULT_COMPUTE_POOL_MAX_THREADS: usize = 8;

fn main() {
    let mut app = App::new();

    // Bevy's own default (`PresentMode::AutoVsync`) locks every frame to the
    // display's refresh interval — a frame that does 3ms of real work and
    // one that does 15ms both report ~16.7ms on a 60Hz screen, since the GPU
    // just idles for the rest of the vblank period either way. That masks
    // real perf data behind whatever the monitor's refresh rate happens to
    // be, which is exactly backwards for a perf-triage build. `TERRAIN_CAR_VSYNC=off`
    // switches to `AutoNoVsync` so frame_time reports actual work instead.
    let vsync_off = std::env::var("TERRAIN_CAR_VSYNC").as_deref() == Ok("off");
    let present_mode = if vsync_off { PresentMode::AutoNoVsync } else { PresentMode::AutoVsync };

    let compute_pool_max_threads = std::env::var("TERRAIN_CAR_COMPUTE_THREADS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n: &usize| n >= 1)
        .unwrap_or(DEFAULT_COMPUTE_POOL_MAX_THREADS);

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
            DefaultPlugins
                .set(WindowPlugin {
                    primary_window: Some(Window {
                        present_mode,
                        ..default()
                    }),
                    ..default()
                })
                .set(TaskPoolPlugin {
                    task_pool_options: TaskPoolOptions {
                        compute: TaskPoolThreadAssignmentPolicy {
                            min_threads: 1,
                            max_threads: compute_pool_max_threads,
                            percent: 0.5,
                            on_thread_spawn: None,
                            on_thread_destroy: None,
                        },
                        ..TaskPoolOptions::default()
                    },
                }),
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
            render_scale::RenderScalePlugin,
            // Feeds `hud.rs`'s FPS readout — `DiagnosticsStore` stays empty
            // without this registered somewhere.
            bevy::diagnostic::FrameTimeDiagnosticsPlugin::default(),
            // Lightweight perf-triage aid: prints one line to the console
            // every second with fps/frame-time/entity-count — cheap enough
            // to ship in every build (unlike `--features profiling`'s
            // `trace_chrome`, which dumps a multi-GB span-per-system JSON
            // for even a short session, way too big for a friend to send
            // over chat). A remote player just needs to reproduce the lag
            // and paste back the console lines from that window.
            bevy::diagnostic::EntityCountDiagnosticsPlugin::default(),
            bevy::diagnostic::LogDiagnosticsPlugin::default(),
        ))
        .add_plugins((
            tank::TankPlugin,
            tank_render::TankRenderPlugin,
            dropship::DropshipPlugin,
            dropship_render::DropshipRenderPlugin,
            turret_control::TurretControlPlugin,
            audio::AudioPlugin,
            thrusters::ThrustersPlugin,
        ));

    // Registers replicated components/events — must use the exact same
    // function the server calls, so wire IDs (assigned in registration
    // order) can never drift between them.
    register_protocol(&mut app);

    app.run();
}
