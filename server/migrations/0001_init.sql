-- Persistent player identity, per-player resource wallets, and placed
-- buildings — see server/src/persistence.rs and the base-building plan.
--
-- `build_complete_at`/`first_seen` are stored as plain double-precision
-- unix-epoch seconds rather than `timestamptz`, deliberately: this project
-- already represents time as f64 seconds everywhere else (Bevy's `Time`,
-- `shared::terrain_gen::random_seed`'s use of `SystemTime`), and it avoids
-- pulling a chrono/time crate feature into sqlx just for one column type.

create table if not exists players (
    id uuid primary key,
    first_seen double precision not null
);

create table if not exists wallets (
    player_id uuid primary key references players(id),
    energy double precision not null default 0,
    ore double precision not null default 0
);

create table if not exists buildings (
    id uuid primary key,
    owner_player_id uuid not null references players(id),
    kind text not null,
    true_x double precision not null,
    true_z double precision not null,
    -- NULL means "not under construction" (shouldn't normally persist mid-
    -- build, but nullable rather than a sentinel value either way).
    build_complete_at double precision
);
