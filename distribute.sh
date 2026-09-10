#!/usr/bin/env bash
# Builds a release client, packages it with its assets into a tarball under
# dist/, then opens a file manager on the folder holding the tarball and
# Google Drive in the browser so it's one copy-paste away from being shared.
#
# Ships the client only, not the server — this is meant to hand to other
# players who'll connect to a server you run separately (see README.md).
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"

STAGE_DIR="dist/terrain-car"
ARCHIVE_NAME="terrain-car-linux-x64-$(date +%Y%m%d-%H%M%S).tar.gz"
ARCHIVE_PATH="dist/${ARCHIVE_NAME}"

echo "==> Building client (release)..."
cargo build -p client --release

echo "==> Staging ${STAGE_DIR}..."
mkdir -p "$STAGE_DIR"
cp target/release/terrain_car "$STAGE_DIR/"
# Only the assets folder is replaced wholesale — a hand-written README.txt
# from a previous distribute, if one's already sitting in $STAGE_DIR, is
# left alone rather than clobbered.
rm -rf "$STAGE_DIR/assets"
cp -r client/assets "$STAGE_DIR/assets"

echo "==> Archiving ${ARCHIVE_PATH}..."
tar -czf "$ARCHIVE_PATH" -C dist terrain-car
echo "==> Done: $(pwd)/${ARCHIVE_PATH}"

echo "==> Opening file manager..."
pcmanfm "$(pwd)/dist" >/dev/null 2>&1 &
disown

echo "==> Opening Google Drive..."
google-chrome-stable "https://drive.google.com/drive/u/0/folders/1cjPLhBofvC2tyrR7jP7PpfEiH_zblJpu" >/dev/null 2>&1 &
disown
