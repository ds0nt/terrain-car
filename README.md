# terrain-car

A multiplayer driving game over procedurally generated, effectively unbounded
terrain — built in [Bevy](https://bevy.org) with
[Rapier3D](https://rapier.rs) physics and [bevy_replicon](https://github.com/simgine/bevy_replicon) /
[renet](https://github.com/lucaspoffo/renet) networking.

Terrain, obstacles, and physics are all pure functions of a shared seed, so
the client and server (and every connected client) independently generate
byte-identical worlds without ever sending mesh or collider data over the
network.

## Workspace layout

- `shared/` — everything client and server must agree on bit-for-bit:
  terrain/obstacle generation (`terrain_gen`, `obstacles`), car physics
  (`car_physics`), and the network protocol (`protocol`).
- `client/` — the playable game: rendering, input, camera, HUD, minimap,
  client-side prediction, and session recording.
- `server/` — a headless, authoritative simulation server with an operator
  console.

## Running

```sh
cargo build --workspace

cargo run -p server                          # headless server, port 5000 by default
cargo run -p client                          # connects to 127.0.0.1:5000 by default
```

Environment variables:

- `TERRAIN_CAR_PORT` — server listen port (default `5000`)
- `TERRAIN_CAR_SERVER` — client connect target, `host:port` (default `127.0.0.1:5000`)
- `TERRAIN_CAR_RECORD_ROOT` — session recorder output directory
- `SUPABASE_DB_URL` — Postgres connection string (`postgresql://postgres:PASSWORD@db.PROJECT.supabase.co:5432/postgres`) for the server's persistence layer (base-building state — see `server/src/persistence.rs`). URL-encode special characters in the password (e.g. `@` → `%40`, `!` → `%21`). Optional: without it, the server runs normally, just without anything persisting.

  The server loads a `.env` file from the current directory automatically (via `dotenvy`) — copy `.env.example` to `.env` and fill in your own connection string rather than exporting it by hand every time. `.env` is gitignored; only `.env.example` (no real credentials) is committed.

## Controls

| Key | Action |
| --- | --- |
| W/S, Up/Down | throttle / reverse |
| A/D, Left/Right | steer |
| Space | brake |
| C | toggle chase-cam / cockpit-cam |
| R | reset car to a nearby spawn |
| N | request a world regeneration (requires op — see below) |
| L | start/stop recording the session to disk |
| T | drop an experimental physics-settled rock pile (client-local) |

The HUD shows speed, altitude, g-force, current biome, and a small ASCII
minimap (`@` you, `O` other players, terrain relief as `. + ^ #`).

## Server console

The server reads commands from its own stdin:

```
list            show connected players
op <#>          grant a player admin privileges
deop <#>        revoke admin privileges
kick <#>        disconnect a player
regen           regenerate the world (always allowed for the console)
quit / stop     shut down the server
```

World regeneration is server-authoritative and op-gated: a regular player
pressing `N` only *requests* it, and the server only acts on that request if
the player has been granted op via the console.

## Networking model

- **Local player's car**: predicted immediately on input (zero perceived
  latency), then reconciled against the server's authoritative snapshot —
  small drift blends in smoothly, a real desync corrects faster but still as
  a glide, never a teleport.
- **Other players' cars**: simple replicated ghosts, positioned directly
  from the server's snapshot.
- **Terrain and obstacles**: never replicated — both sides compute them
  independently from the shared seed. A world regeneration broadcasts just
  the new seed, not any geometry.

## Session recording

Pressing `L` records driving data compatible with this project's own ML
pipeline: `data/drivedata2_terrain/<timestamp>-terrain/status.csv` (columns:
timestamp, steering, camera_yaw, camera_pitch, throttle, front_distance,
valid) plus a `.jpg` per captured frame.
