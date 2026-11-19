#!/usr/bin/env bash
# Local UI development: runs the collector and smweb together, and rebuilds and
# restarts smweb whenever something under web/ or src/ changes (the page, script
# and styles are built into the binary). Reload the browser to see the change.
# Stop with Ctrl-C.
#
# Settings (all optional):
#   SM_HTTP      where smweb listens (default 127.0.0.1:8099)
#   SM_SOCKET    collector socket (default /tmp/simple-metrics-dev.sock)
#   SM_INTERVAL  sampling interval (default 1s)
set -euo pipefail
cd "$(dirname "$(readlink -f "$0")")"

HTTP="${SM_HTTP:-127.0.0.1:8099}"
SOCKET="${SM_SOCKET:-/tmp/simple-metrics-dev.sock}"
BIN=target/release

build() { cargo build --release --locked --quiet "$@"; }
stamp() { find web src Cargo.toml -type f -printf '%T@ %p\n' 2>/dev/null | sort | md5sum; }

build
$BIN/simple-metrics --socket "$SOCKET" --interval "${SM_INTERVAL:-1s}" --retention 1d &
SM_PID=$!
WEB_PID=
cleanup() { [ -n "$WEB_PID" ] && kill "$WEB_PID" 2>/dev/null || true; kill "$SM_PID" 2>/dev/null || true; }
trap cleanup EXIT INT TERM

start_web() { $BIN/smweb --socket "$SOCKET" --http "$HTTP" & WEB_PID=$!; }
start_web
echo "UI: http://${HTTP}/   (watching web/ and src/; Ctrl-C to stop)"

last=$(stamp)
while sleep 1; do
  now=$(stamp)
  [ "$now" = "$last" ] && continue
  last=$now
  echo "change detected, rebuilding..."
  if build; then
    kill "$WEB_PID" 2>/dev/null || true; wait "$WEB_PID" 2>/dev/null || true
    start_web
    echo "reloaded - refresh the browser"
  else
    echo "build failed; fix and save again"
  fi
done
