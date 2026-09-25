# simple-metrics

A small, efficient metrics collector for a single Linux machine.

It samples a fixed set of variables at a regular interval, keeps only the most
recent records in memory (older ones are discarded as new ones arrive), and
serves them over a Unix socket. Nothing is written to disk.

Written in Rust with no dependencies beyond the standard library, and built as
a single static binary, so it is cheap to run on something like a Raspberry Pi
and has nothing to install alongside it.

> **Status:** early. The project scaffolding, the bounded in-memory store and
> the command line are in place. Sampling and the socket API are next.

## Building

You need [rustup](https://rustup.rs). `rust-toolchain.toml` selects the right
compiler and adds the components and target this project uses, so there is
nothing else to set up.

```bash
cargo build --release                                     # for this machine
cargo build --release --target aarch64-unknown-linux-musl # static, for a Raspberry Pi
```

The second command produces a statically linked binary
(`target/aarch64-unknown-linux-musl/release/simple-metrics`) that runs on any
64-bit ARM Linux, whatever its glibc version.

## Development

```bash
./check.sh   # everything CI runs: rustfmt, clippy, tests, docs, the static build
```

Or individually:

```bash
cargo fmt --all
cargo clippy --all-targets
cargo test
```

| Path | Purpose |
|---|---|
| `src/store.rs` | `RingBuffer<T>`: fixed capacity, drops the oldest item when full |
| `src/cli.rs` | Argument handling, kept apart from `main` so it can be unit tested |
| `src/main.rs` | A thin wrapper around the library |
| `tests/` | Integration tests that run the real binary |

Lint rules live in `Cargo.toml` (`[lints]`) and `clippy.toml`: `unsafe` is
forbidden, and `unwrap`/`expect`/`panic` are warned about outside tests, since
a daemon that runs for weeks should report errors rather than crash.

CI (`.github/workflows/ci.yml`) runs the same checks on x86_64 and aarch64,
plus a check that the minimum supported Rust version (`rust-version` in
`Cargo.toml`) still builds.

## Releasing

Bump `version` in `Cargo.toml`, commit, then push a matching tag:

```bash
git tag v0.1.0 && git push origin v0.1.0
```

The release workflow refuses a tag that doesn't match `Cargo.toml`, builds
static `aarch64` and `x86_64` binaries, runs the tests, and publishes them as a
GitHub release with a `SHA256SUMS` file.

## Licence

[MIT](LICENSE)
