#!/usr/bin/env bash
# Runs the same checks CI does, so a push doesn't fail on something that was
# findable locally. Usage: ./check.sh
set -euo pipefail
cd "$(dirname "$(readlink -f "$0")")"

export RUSTFLAGS="-D warnings"
export RUSTDOCFLAGS="-D warnings"

step() { printf '\n==> %s\n' "$*"; }

step "rustfmt"
cargo fmt --all --check
step "clippy"
cargo clippy --all-targets --locked
step "tests"
cargo test --locked
step "docs"
cargo doc --no-deps --locked
step "static binary (the Pi's target)"
cargo build --release --locked --target aarch64-unknown-linux-musl
printf '\nall checks passed\n'
