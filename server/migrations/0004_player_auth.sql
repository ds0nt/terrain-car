-- Real accounts: unique usernames + hashed passwords, replacing the old
-- "trust whatever UUID the client sends" identity model (see
-- shared::protocol::RegisterMsg/LoginMsg). Nullable, not `not null`: this
-- is a deliberate fresh start, not a migration of existing data — rows
-- inserted before accounts existed keep a NULL username/password_hash and
-- simply become unreachable through login (their `id` no longer matches
-- any account). Every row inserted via registration always populates
-- both.
alter table players add column if not exists username text;
alter table players add column if not exists password_hash text;
create unique index if not exists players_username_key on players (username);
