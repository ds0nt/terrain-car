#!/usr/bin/env bash
# Run after a `cargo run --release --features profiling` session that
# reproduced the lag: finds the newest trace-*.json it wrote, summarizes it
# into a small trace-summary.txt (top systems by total time + per-second
# frame times), then deletes the raw trace — that file can be multiple GB
# for even a short session, way too big to send, but the summary is a few
# KB of plain text.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"

TRACE_FILE=$(ls -t trace-*.json 2>/dev/null | head -n1 || true)
if [ -z "$TRACE_FILE" ]; then
  echo "No trace-*.json found in $(pwd) — run the game with --features profiling first." >&2
  exit 1
fi

echo "==> Summarizing ${TRACE_FILE} (this can take a couple minutes for a big trace)..."
python3 scripts/analyze_trace.py "$TRACE_FILE" trace-summary.txt

echo "==> Deleting ${TRACE_FILE} (raw trace, not needed anymore)..."
rm -f "$TRACE_FILE"

echo "==> Done — send back trace-summary.txt (it's plain text, tiny)."
