use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
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
    pub rotation_y: f64,
    pub ground_y: f64,
}

/// Sent from Bevy systems to the persistence thread. Saves are
/// fire-and-forget from the caller's perspective — a failed save just logs
/// a warning on the thread and gets superseded by the next periodic save,
/// never propagates an error back into the game loop. `Register`/`Login`
/// are the one exception that isn't fire-and-forget — see `AuthResult`.
pub enum PersistenceCommand {
    SaveWallet(WalletRow),
    SaveBuilding(BuildingRow),
    /// `client_entity` rides along purely so the reply (`AuthResult`) can
    /// be routed back to the right connection — the persistence thread has
    /// no other notion of "which client asked this."
    Register { client_entity: Entity, username: String, password: String },
    Login { client_entity: Entity, username: String, password: String },
}

/// Sent back from the persistence thread once loaded — currently only
/// happens once, right after connecting/migrating at startup.
pub enum PersistenceEvent {
    Loaded { wallets: Vec<WalletRow>, buildings: Vec<BuildingRow> },
    Unavailable,
    AuthResult { client_entity: Entity, outcome: AuthOutcome },
}

/// Result of a `Register`/`Login` command — see `server::auth`, which
/// drains these and turns them into an `AuthResultMsg` back to the client.
pub enum AuthOutcome {
    Success(Uuid),
    UsernameTaken,
    /// Deliberately covers both "no such username" and "wrong password"
    /// identically — see `LoginMsg`'s own docs on why.
    InvalidCredentials,
    Error(String),
}

/// `Receiver` is `Send` but not `Sync`; wrapping in a `Mutex` is simpler
/// than a non-send resource, matching `console.rs`'s own `ConsoleLines`.
#[derive(Resource)]
pub struct Persistence {
    tx: Sender<PersistenceCommand>,
    rx: Mutex<Receiver<PersistenceEvent>>,
    /// Set by the persistence thread itself right after it connects and
    /// migrates successfully — a separate signal from the `mpsc` channel
    /// (not something read via `try_recv()`) so `server::auth` can check
    /// it synchronously before ever sending a `Register`/`Login` command,
    /// without competing with `economy::apply_loaded_state` (the channel's
    /// sole drain point) for events.
    available: Arc<AtomicBool>,
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

    /// Whether the background thread is actually connected — accounts are
    /// impossible without a real database (unlike wallets/buildings, which
    /// degrade gracefully to in-memory-only), so `server::auth` checks
    /// this up front and rejects immediately rather than sending a
    /// `Register`/`Login` command that would otherwise vanish silently
    /// once the thread has already given up and exited.
    pub fn is_available(&self) -> bool {
        self.available.load(Ordering::Relaxed)
    }
}

fn spawn_persistence_thread(mut commands: Commands) {
    let (cmd_tx, cmd_rx) = channel::<PersistenceCommand>();
    let (event_tx, event_rx) = channel::<PersistenceEvent>();
    let available = Arc::new(AtomicBool::new(false));

    let db_url = std::env::var("SUPABASE_DB_URL").ok();
    let thread_available = available.clone();
    thread::spawn(move || run_persistence_thread(db_url, cmd_rx, event_tx, thread_available));

    commands.insert_resource(Persistence { tx: cmd_tx, rx: Mutex::new(event_rx), available });
}

fn run_persistence_thread(
    db_url: Option<String>,
    cmd_rx: Receiver<PersistenceCommand>,
    event_tx: Sender<PersistenceEvent>,
    available: Arc<AtomicBool>,
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
        available.store(true, Ordering::Relaxed);

        if let Ok(loaded) = load_all(&pool).await {
            let _ = event_tx.send(loaded);
        }

        while let Ok(command) = cmd_rx.recv() {
            match command {
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
                PersistenceCommand::Register { client_entity, username, password } => {
                    let outcome = register(&pool, &username, &password).await;
                    let _ = event_tx.send(PersistenceEvent::AuthResult { client_entity, outcome });
                }
                PersistenceCommand::Login { client_entity, username, password } => {
                    let outcome = login(&pool, &username, &password).await;
                    let _ = event_tx.send(PersistenceEvent::AuthResult { client_entity, outcome });
                }
            }
        }
    });
}

/// Argon2 with its own library defaults (a strong, non-trivial cost —
/// deliberately not tuned lighter for "speed," since this only ever runs
/// once per register/login, never in a hot path) and a fresh random salt
/// per password, PHC-string-encoded so the salt/params travel with the
/// hash itself — `verify_password` needs nothing but that one string back.
fn hash_password(password: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| e.to_string())
}

fn verify_password(password: &str, hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok()
}

async fn register(pool: &PgPool, username: &str, password: &str) -> AuthOutcome {
    let password_hash = match hash_password(password) {
        Ok(hash) => hash,
        Err(e) => return AuthOutcome::Error(e),
    };
    let player_id = Uuid::new_v4();
    let result = sqlx::query(
        "insert into players (id, username, password_hash, first_seen) \
         values ($1, $2, $3, extract(epoch from now()))",
    )
    .bind(player_id)
    .bind(username)
    .bind(&password_hash)
    .execute(pool)
    .await;

    match result {
        Ok(_) => AuthOutcome::Success(player_id),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => AuthOutcome::UsernameTaken,
        Err(e) => {
            warn!("persistence: register failed for `{username}`: {e}");
            AuthOutcome::Error("registration failed".to_string())
        }
    }
}

async fn login(pool: &PgPool, username: &str, password: &str) -> AuthOutcome {
    let row = sqlx::query_as::<_, (Uuid, String)>(
        "select id, password_hash from players where username = $1",
    )
    .bind(username)
    .fetch_optional(pool)
    .await;

    match row {
        Ok(Some((player_id, password_hash))) => {
            if verify_password(password, &password_hash) {
                AuthOutcome::Success(player_id)
            } else {
                AuthOutcome::InvalidCredentials
            }
        }
        Ok(None) => AuthOutcome::InvalidCredentials,
        Err(e) => {
            warn!("persistence: login lookup failed for `{username}`: {e}");
            AuthOutcome::Error("login failed".to_string())
        }
    }
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
        "insert into buildings (id, owner_player_id, kind, true_x, true_z, build_complete_at, rotation_y, ground_y) \
         values ($1, $2, $3, $4, $5, $6, $7, $8) \
         on conflict (id) do update set build_complete_at = excluded.build_complete_at",
    )
    .bind(building.id)
    .bind(building.owner_player_id)
    .bind(&building.kind)
    .bind(building.true_x)
    .bind(building.true_z)
    .bind(building.build_complete_at)
    .bind(building.rotation_y)
    .bind(building.ground_y)
    .execute(pool)
    .await?;
    Ok(())
}

async fn load_all(pool: &PgPool) -> sqlx::Result<PersistenceEvent> {
    let wallets = sqlx::query_as::<_, WalletRow>("select player_id, energy, ore from wallets")
        .fetch_all(pool)
        .await?;
    let buildings = sqlx::query_as::<_, BuildingRow>(
        "select id, owner_player_id, kind, true_x, true_z, build_complete_at, rotation_y, ground_y from buildings",
    )
    .fetch_all(pool)
    .await?;
    Ok(PersistenceEvent::Loaded { wallets, buildings })
}
