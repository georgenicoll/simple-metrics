#!/usr/bin/env bash
# Builds simple-metrics and runs it in the foreground for local development,
# on the socket that `smq --local` (and the wireguard-ap dev_webapp.sh) use.
# Stop it with Ctrl-C.
#
# It samples this machine's /proc, so under WSL some values are gaps: no
# CPU temperature, and only the network interfaces that exist.
#
# Settings (all optional):
#   SM_SOCKET     where to listen (default /tmp/simple-metrics.sock)
#   SM_INTERVAL   sampling interval (default 1s, so charts fill quickly;
#                 the deployed daemon uses 5s)
#   SM_RETENTION  how much history to keep (default 1d)
# Anything else on the command line is passed to simple-metrics, e.g.
#   ./run_local.sh --interface eth0
set -euo pipefail
cd "$(dirname "$(readlink -f "$0")")"

command -v cargo >/dev/null || { echo "cargo not found - install rustup from https://rustup.rs" >&2; exit 1; }

cargo build --release --locked --quiet

SOCKET="${SM_SOCKET:-/tmp/simple-metrics.sock}"
echo "simple-metrics listening on ${SOCKET} (Ctrl-C to stop)"
echo "try:  target/release/smq --socket ${SOCKET} latest"
exec target/release/simple-metrics \
  --socket "$SOCKET" \
  --interval "${SM_INTERVAL:-1s}" \
  --retention "${SM_RETENTION:-1d}" \
  "$@"
