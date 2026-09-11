#!/usr/bin/env bash
# Builds a release server and packages it into a tarball under dist/ — the
# server-side counterpart to distribute.sh (which ships the client only).
#
# Meant for a remote player to run the *server* locally on their own
# machine, alongside their own client, over loopback — this takes the
# network entirely out of the picture, so if it's still bad, the problem is
# purely client-side CPU/GPU; if it's suddenly fine, the problem was
# something about their connection to whatever server they'd been using.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"

STAGE_DIR="dist/terrain-car-server"
ARCHIVE_NAME="terrain-car-server-linux-x64-$(date +%Y%m%d-%H%M%S).tar.gz"
ARCHIVE_PATH="dist/${ARCHIVE_NAME}"

echo "==> Building server (release)..."
cargo build -p server --release

echo "==> Staging ${STAGE_DIR}..."
mkdir -p "$STAGE_DIR"
cp target/release/terrain_car_server "$STAGE_DIR/"

# Only written if this is the first time staging here — a README.txt a
# previous distribute-server run already dropped (or one hand-edited) is
# left alone rather than clobbered, same rule distribute.sh follows for the
# client's assets folder.
if [ ! -f "$STAGE_DIR/README.txt" ]; then
  cat > "$STAGE_DIR/README.txt" <<'EOF'
terrain-car server — run locally to test without any network involved.

    ./terrain_car_server

Listens on UDP port 5000 by default. Then point your own client at it
(it's the client's own default, so this usually needs nothing extra):

    TERRAIN_CAR_SERVER=127.0.0.1:5000 ./terrain_car

No world-persistence database is required — the server runs fine without
one, it just won't save/restore base-building state across restarts.

Console commands (typed into the server's own terminal):
    list            show connected players
    op <#>          grant a player admin privileges
    kick <#>        disconnect a player
    regen           regenerate the world
    quit / stop     shut down the server
EOF
fi

echo "==> Archiving ${ARCHIVE_PATH}..."
tar -czf "$ARCHIVE_PATH" -C dist terrain-car-server
echo "==> Done: $(pwd)/${ARCHIVE_PATH}"

echo "==> Opening file manager..."
pcmanfm "$(pwd)/dist" >/dev/null 2>&1 &
disown

echo "==> Opening Google Drive..."
google-chrome-stable "https://drive.google.com/drive/u/0/folders/1cjPLhBofvC2tyrR7jP7PpfEiH_zblJpu" >/dev/null 2>&1 &
disown
