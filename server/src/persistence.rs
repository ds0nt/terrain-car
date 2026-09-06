use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;
use std::thread;

use bevy::prelude::*;
use sqlx::postgres::PgPoolOptions;
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

/// Cloud Postgres persistence for buildings/wallets — see the base-building
/// plan's "Postgres is for persistence, not the live tick" principle.
/// Buildings/wallets live as ordinary in-memory ECS state for actual
/// gameplay (Phase 2+); this is purely a write-behind layer, loaded once
/// at startup and saved periodically/on significant events, never read or
/// written inside the 64Hz physics loop. Mirrors `console.rs`'s existing
/// background-thread-plus-channel shape almost exactly — there it's a
/// blocking stdin read, here it's a small Tokio runtime running async
/// `sqlx` calls, but the reason is the same: never block a tick on I/O.
pub struct PersistencePlugin;

impl Plugin for PersistencePlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_persistence_thread);
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct WalletRow {
    pub player_id: Uuid,
    pub energy: f64,
    pub ore: f64,
}

#[derive(Debug, Clone, FromRow)]
pub struct BuildingRow {
    pub id: Uuid,
    pub owner_player_id: Uuid,
    pub kind: String,
    pub true_x: f64,
    pub true_z: f64,
    pub build_complete_at: Option<f64>,
}

/// Sent from Bevy systems to the persistence thread. Saves are
/// fire-and-forget from the caller's perspective — a failed save just logs
/// a warning on the thread and gets superseded by the next periodic save,
/// never propagates an error back into the game loop.
///
/// `SaveWallet`/`SaveBuilding`/`LoadAll` aren't sent by anything yet — this
/// phase is the persistence plumbing itself (connect, migrate, round-trip
/// a player upsert); real wallet/building state to save doesn't exist
/// until the base-building plan's Phase 2. Allowing dead_code deliberately
/// rather than deleting and re-adding the same API next phase.
#[allow(dead_code)]
pub enum PersistenceCommand {
    UpsertPlayer(Uuid),
    SaveWallet(WalletRow),
    SaveBuilding(BuildingRow),
    LoadAll,
}

/// Sent back from the persistence thread once a `LoadAll` completes —
/// applied once by `poll_persistence_results` (in practice, once at
/// startup; nothing else currently issues `LoadAll`).
pub enum PersistenceEvent {
    Loaded { wallets: Vec<WalletRow>, buildings: Vec<BuildingRow> },
    Unavailable,
}

/// `Receiver` is `Send` but not `Sync`; wrapping in a `Mutex` is simpler
/// than a non-send resource, matching `console.rs`'s own `ConsoleLines`.
#[derive(Resource)]
pub struct Persistence {
    tx: Sender<PersistenceCommand>,
    rx: Mutex<Receiver<PersistenceEvent>>,
}

impl Persistence {
    /// Fire-and-forget: the send only fails if the persistence thread has
    /// already shut down (e.g. no `SUPABASE_DB_URL` was set at all), which
    /// is a fine, silent no-op for a game that still needs to run without
    /// a database configured.
    pub fn send(&self, command: PersistenceCommand) {
        let _ = self.tx.send(command);
    }

    /// Drains one pending result from the persistence thread, if any —
    /// callers (currently just `economy.rs`) poll this once per frame in
    /// a loop until it returns `None`, applying `Loaded` into their own
    /// in-memory state. `persistence.rs` itself has no opinion on what
    /// that state is (wallets/buildings are Phase 2 concepts); it only
    /// owns getting bytes to and from Postgres.
    pub fn try_recv(&self) -> Option<PersistenceEvent> {
        self.rx.lock().ok()?.try_recv().ok()
    }
}

fn spawn_persistence_thread(mut commands: Commands) {
    let (cmd_tx, cmd_rx) = channel::<PersistenceCommand>();
    let (event_tx, event_rx) = channel::<PersistenceEvent>();

    let db_url = std::env::var("SUPABASE_DB_URL").ok();
    thread::spawn(move || run_persistence_thread(db_url, cmd_rx, event_tx));

    commands.insert_resource(Persistence { tx: cmd_tx, rx: Mutex::new(event_rx) });
}

fn run_persistence_thread(
    db_url: Option<String>,
    cmd_rx: Receiver<PersistenceCommand>,
    event_tx: Sender<PersistenceEvent>,
) {
    let Some(db_url) = db_url else {
        warn!("persistence: SUPABASE_DB_URL not set — buildings/wallets will not persist");
        let _ = event_tx.send(PersistenceEvent::Unavailable);
        return;
    };

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            error!("persistence: failed to start Tokio runtime: {e}");
            let _ = event_tx.send(PersistenceEvent::Unavailable);
            return;
        }
    };

    runtime.block_on(async move {
        let pool = match PgPoolOptions::new().max_connections(5).connect(&db_url).await {
            Ok(pool) => pool,
            Err(e) => {
                error!("persistence: failed to connect to database: {e}");
                let _ = event_tx.send(PersistenceEvent::Unavailable);
                return;
            }
        };

        if let Err(e) = sqlx::migrate!("./migrations").run(&pool).await {
            error!("persistence: migration failed: {e}");
            let _ = event_tx.send(PersistenceEvent::Unavailable);
            return;
        }
        info!("persistence: connected and migrated");

        if let Ok(loaded) = load_all(&pool).await {
            let _ = event_tx.send(loaded);
        }

        while let Ok(command) = cmd_rx.recv() {
            match command {
                PersistenceCommand::UpsertPlayer(player_id) => {
                    if let Err(e) = upsert_player(&pool, player_id).await {
                        warn!("persistence: failed to upsert player {player_id}: {e}");
                    }
                }
                PersistenceCommand::SaveWallet(wallet) => {
                    if let Err(e) = save_wallet(&pool, &wallet).await {
                        warn!("persistence: failed to save wallet for {}: {e}", wallet.player_id);
                    }
                }
                PersistenceCommand::SaveBuilding(building) => {
                    if let Err(e) = save_building(&pool, &building).await {
                        warn!("persistence: failed to save building {}: {e}", building.id);
                    }
                }
                PersistenceCommand::LoadAll => {
                    if let Ok(loaded) = load_all(&pool).await {
                        let _ = event_tx.send(loaded);
                    }
                }
            }
        }
    });
}

async fn upsert_player(pool: &PgPool, player_id: Uuid) -> sqlx::Result<()> {
    sqlx::query(
        "insert into players (id, first_seen) values ($1, extract(epoch from now())) \
         on conflict (id) do nothing",
    )
    .bind(player_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn save_wallet(pool: &PgPool, wallet: &WalletRow) -> sqlx::Result<()> {
    sqlx::query(
        "insert into wallets (player_id, energy, ore) values ($1, $2, $3) \
         on conflict (player_id) do update set energy = excluded.energy, ore = excluded.ore",
    )
    .bind(wallet.player_id)
    .bind(wallet.energy)
    .bind(wallet.ore)
    .execute(pool)
    .await?;
    Ok(())
}

async fn save_building(pool: &PgPool, building: &BuildingRow) -> sqlx::Result<()> {
    sqlx::query(
        "insert into buildings (id, owner_player_id, kind, true_x, true_z, build_complete_at) \
         values ($1, $2, $3, $4, $5, $6) \
         on conflict (id) do update set build_complete_at = excluded.build_complete_at",
    )
    .bind(building.id)
    .bind(building.owner_player_id)
    .bind(&building.kind)
    .bind(building.true_x)
    .bind(building.true_z)
    .bind(building.build_complete_at)
    .execute(pool)
    .await?;
    Ok(())
}

async fn load_all(pool: &PgPool) -> sqlx::Result<PersistenceEvent> {
    let wallets = sqlx::query_as::<_, WalletRow>("select player_id, energy, ore from wallets")
        .fetch_all(pool)
        .await?;
    let buildings = sqlx::query_as::<_, BuildingRow>(
        "select id, owner_player_id, kind, true_x, true_z, build_complete_at from buildings",
    )
    .fetch_all(pool)
    .await?;
    Ok(PersistenceEvent::Loaded { wallets, buildings })
}
