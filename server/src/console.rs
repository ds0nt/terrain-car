use std::io::{self, BufRead};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Mutex;
use std::thread;

use bevy::prelude::*;
use bevy_rapier3d::prelude::*;
use bevy_replicon::prelude::*;
use shared::protocol::CarSnapshot;
use shared::terrain_gen::TerrainNoise;
use shared::worldspace::WorldOrigin;

use crate::car_sim::{do_world_regen, CurrentWorldState, OpList, OwnedBy, PlayerRegistry, SpawnAnchor};
use crate::terrain_phys::ServerTerrainEntity;

/// A simple operator console on the server's own stdin — since this is a
/// headless process with no window, this is the entire "admin UI." Reading
/// stdin directly in a system would block the whole simulation, so a
/// background OS thread does the actual blocking read and forwards
/// complete lines to the main app over a channel; `process_console_input`
/// just drains whatever's arrived each frame.
pub struct ConsolePlugin;

impl Plugin for ConsolePlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<ConsoleRegenRequested>()
            .add_systems(Startup, spawn_console_thread)
            .add_systems(Update, (process_console_input, apply_console_regen));
        println!("Console ready. Type `help` for commands.");
    }
}

/// `Receiver` is `Send` but not `Sync`, and Bevy resources need to be both
/// by default — wrapping it in a `Mutex` (whose access is inherently
/// serialized anyway, matching how `try_recv` is only ever called from one
/// system) is simpler here than switching to a non-send resource.
#[derive(Resource)]
struct ConsoleLines(Mutex<Receiver<String>>);

/// Written by `process_console_input` when the `regen` command is typed,
/// consumed by `apply_console_regen` — split into two systems purely to
/// keep each one's parameter list manageable; `regen` needs the full
/// world-regeneration parameter set (see `do_world_regen`), which doesn't
/// belong alongside the lightweight list/kick/op bookkeeping the rest of
/// the console handles.
#[derive(Message)]
struct ConsoleRegenRequested;

fn spawn_console_thread(mut commands: Commands) {
    let (tx, rx) = channel();
    thread::spawn(move || {
        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(line) => {
                    if tx.send(line).is_err() {
                        break; // app shut down, stop reading
                    }
                }
                Err(_) => break,
            }
        }
    });
    commands.insert_resource(ConsoleLines(Mutex::new(rx)));
}

fn process_console_input(
    lines: Res<ConsoleLines>,
    mut app_exit: MessageWriter<AppExit>,
    mut disconnects: MessageWriter<DisconnectRequest>,
    mut regen_requests: MessageWriter<ConsoleRegenRequested>,
    mut op_list: ResMut<OpList>,
    registry: Res<PlayerRegistry>,
    positions: Query<(&OwnedBy, &CarSnapshot)>,
) {
    let Ok(receiver) = lines.0.lock() else {
        return;
    };
    while let Ok(line) = receiver.try_recv() {
        let line = line.trim();
        let mut parts = line.split_whitespace();
        let Some(cmd) = parts.next() else { continue };

        match cmd {
            "help" => {
                println!(
                    "Commands:\n  \
                     list                 - show connected players\n  \
                     op <#>               - grant a player admin privileges\n  \
                     deop <#>             - revoke admin privileges\n  \
                     kick <#>             - disconnect a player\n  \
                     regen                - regenerate the world (new terrain, everyone repositioned)\n  \
                     quit / stop          - shut down the server"
                );
            }
            "list" => {
                let mut any = false;
                for (index, entity) in registry.iter() {
                    any = true;
                    let is_op = op_list.is_op(entity);
                    let pos = positions
                        .iter()
                        .find(|(owner, _)| owner.0 == entity)
                        .map(|(_, snap)| snap.translation);
                    println!("  #{index}  client={entity}  op={is_op}  pos={pos:?}");
                }
                if !any {
                    println!("  (no players connected)");
                }
            }
            "op" | "deop" => {
                let Some(n) = parts.next().and_then(|s| s.parse::<u32>().ok()) else {
                    println!("usage: {cmd} <player #> (see `list`)");
                    continue;
                };
                let Some(entity) = registry.entity_for(n) else {
                    println!("no player #{n}");
                    continue;
                };
                if cmd == "op" {
                    op_list.grant(entity);
                    println!("player #{n} is now op");
                } else {
                    op_list.revoke(entity);
                    println!("player #{n} is no longer op");
                }
            }
            "kick" => {
                let Some(n) = parts.next().and_then(|s| s.parse::<u32>().ok()) else {
                    println!("usage: kick <player #> (see `list`)");
                    continue;
                };
                let Some(entity) = registry.entity_for(n) else {
                    println!("no player #{n}");
                    continue;
                };
                disconnects.write(DisconnectRequest { client: entity });
                println!("kicking player #{n}");
            }
            "regen" => {
                regen_requests.write(ConsoleRegenRequested);
            }
            "quit" | "stop" => {
                println!("shutting down...");
                app_exit.write(AppExit::Success);
            }
            other => {
                println!("unknown command: `{other}` (try `help`)");
            }
        }
    }
}

/// The console is inherently authorized (whoever runs the server), so this
/// calls the same core regen logic the op-gated in-game request uses
/// (`car_sim::apply_world_regen_request`), bypassing the op check entirely.
fn apply_console_regen(
    mut regen_requests: MessageReader<ConsoleRegenRequested>,
    mut commands: Commands,
    mut noise: ResMut<TerrainNoise>,
    mut origin: ResMut<WorldOrigin>,
    mut anchor: ResMut<SpawnAnchor>,
    mut world_state: ResMut<CurrentWorldState>,
    terrain_entities: Query<Entity, With<ServerTerrainEntity>>,
    mut cars: Query<(&mut Transform, &mut Velocity, &mut ExternalForce, &mut CarSnapshot), With<OwnedBy>>,
) {
    if regen_requests.read().next().is_none() {
        return;
    }
    let msg = do_world_regen(
        &mut commands,
        &mut noise,
        &mut origin,
        &mut anchor,
        &mut world_state,
        &terrain_entities,
        &mut cars,
    );
    commands.server_trigger(ToClients {
        targets: SendTargets::All,
        message: msg,
    });
    println!("world regenerated (seed={})", msg.seed);
}
