# simple-metrics

A small, efficient metrics collector for a single Linux machine.

It samples a fixed set of variables at a regular interval, keeps only the most
recent records in memory (older ones are discarded as new ones arrive), and
serves them over a Unix socket as JSON. Nothing is written to disk, and no
root is needed: everything it reads is world-readable under `/proc` and `/sys`.

Written in Rust with a single dependency (`serde_json`, for the protocol), and
built as a static binary of about 600 KB, so it is cheap to run on something
like a Raspberry Pi and has nothing to install alongside it.

## What it measures

Every record holds the same set of values, taken together:

| Metric | Unit | Source |
|---|---|---|
| `cpu_percent` | % | `/proc/stat` (change since the last sample) |
| `load1` | | `/proc/loadavg` (1-minute load average) |
| `mem_used_bytes` | bytes | `/proc/meminfo` (`MemTotal - MemAvailable`, so reclaimable cache doesn't count as used) |
| `swap_used_bytes` | bytes | `/proc/meminfo` |
| `cpu_temp_celsius` | °C | `/sys/class/thermal/thermal_zone0/temp` |
| `net_<iface>_rx_bytes_per_sec` | bytes/s | `/proc/net/dev` (change since the last sample) |
| `net_<iface>_tx_bytes_per_sec` | bytes/s | `/proc/net/dev` |

The network interfaces are `eth0`, `wlan0`, `wlan1`, `br-ap` and `wg0` unless
you say otherwise. A value that can't be worked out (an interface that isn't
there, a counter that was reset, the first sample's rates, a file that can't be
read) is a **gap**: `null` in the JSON, never a misleading zero.

## Memory

Room for every record is allocated once, when it starts, and never grows. The
default is one sample every 5 seconds for 7 days, which is 120,960 records of
15 values plus a timestamp: about **15 MB**. (Measured: 14.5 MB resident when
full, 0.4 MB when empty.) A full read of the whole history streams out as
about 11 MB of JSON in around 100 ms; ask for a subset, a range or a
downsampled result (see below) to get far less.

## Running it

```bash
simple-metrics --socket /run/simple-metrics/simple-metrics.sock
```

| Option | Default | |
|---|---|---|
| `--socket <PATH>` | `/run/simple-metrics/simple-metrics.sock` | Where to listen. |
| `--socket-mode <MODE>` | `660` | The socket's permissions, in octal. The default lets the owner and group connect, and no one else. |
| `--interval <TIME>` | `5s` | How often to sample. |
| `--retention <TIME>` | `7d` | How much history to keep. The number of records is this divided by the interval. |
| `--interface <NAME>` | `eth0 wlan0 wlan1 br-ap wg0` | A network interface to report on. Repeat for several; giving any replaces the defaults. |
| `--root <DIR>` | `/` | Read `proc/` and `sys/` under this directory. For testing. |

A `TIME` is a whole number with an optional unit (`ms`, `s`, `m`, `h`, `d`;
seconds if none): `500ms`, `5s`, `90m`, `7d`. Settings that would need more
than 1 GiB for the store are refused.

It logs to standard error (so, to the journal under systemd), and exits with
an error if the socket can't be set up. It replaces a stale socket left by an
earlier run, but refuses to replace a live one, or anything that isn't a
socket. It needs no signal handling: stopping it (`SIGTERM`) just ends it.

This repository only builds the binary. Installing it and running it as a
service is up to whoever deploys it.

## The socket protocol

JSON lines: send one JSON object per line, get exactly one line of JSON back
for each. A connection can carry any number of requests. Every response has
`"ok"` and `"v"` (the protocol version, currently `1`); a failure is
`{"ok":false,"v":1,"error":"..."}`. A request may include `"v"` to insist on a
version. Fields the server doesn't know are ignored, so later versions can add
to a request without breaking older clients.

```console
$ echo '{"op":"info"}' | socat - UNIX-CONNECT:/run/simple-metrics/simple-metrics.sock
{"capacity":120960,"interval_ms":5000,"len":42,"metrics":15,"ok":true,"v":1,"version":"0.1.0"}
```

| `op` | Response fields |
|---|---|
| `info` | `version`, `interval_ms`, `capacity`, `len` (records held now), `metrics` (a count) |
| `metrics` | `metrics`: a list of `{name, label, unit}`, in record order |
| `latest` | `timestamp` (milliseconds since the Unix epoch, or `null` if nothing is recorded yet) and `values`: `{name: number or null}` |
| `read` | Records: `timestamps` (oldest first) and `series`: `{name: [number or null, ...]}`, each the same length as `timestamps`. See below. |

The `read` response is column-oriented, which is the shape a chart wants. With
nothing else in the request it returns every record. These optional fields
narrow and summarise it:

| Field | Meaning |
|---|---|
| `from`, `to` | Only records from `from` to `to`, **both inclusive**, in milliseconds since the Unix epoch. Either may be left out. A range with no records is an empty result, not an error. |
| `metrics` | Only these metrics, by name (from the `metrics` op). `series` then holds just these. An unknown name is an error. |
| `step_ms` | Group the records into buckets this many milliseconds wide, and return the **mean** of each metric per bucket. |
| `max_points` | The same, but the server picks the bucket width so that there are at most this many buckets (2 to 200,000). The width is a whole number of samples. |
| `extremes` | With `step_ms` or `max_points`: also return the smallest and largest value per bucket, as `min` and `max`, laid out like `series`. Lets a chart show spikes that averaging would hide. |

`step_ms` and `max_points` can't be combined. A downsampled response also has
`step_ms`, the width used, and its `timestamps` are the buckets' **start**
times. Buckets are aligned to multiples of the width since the Unix epoch, not
to the query, so asking again for a later window gives the same edges. Every
bucket from the first record's to the last's is present: a stretch with no
records, or no usable values, is `null` in each series.

```console
$ echo '{"op":"read","metrics":["cpu_percent"],"from":1790380000000,"max_points":60,"extremes":true}' \
    | socat - UNIX-CONNECT:/run/simple-metrics/simple-metrics.sock
{"max":{"cpu_percent":[...]},"min":{"cpu_percent":[...]},"ok":true,"series":{"cpu_percent":[...]},"step_ms":60000,"timestamps":[...],"v":1}
```

Downsampling is cheap: over a full 7 days (120,960 records), one metric down to
600 points takes under a millisecond, and all fifteen about 2 ms, so the store
is locked for a negligible time. (`cargo test --release -- --ignored --nocapture`
repeats the timing.) A query that would need more than 200,000 buckets is
refused, with a hint to use a wider `step_ms` or a `max_points`.

Limits: a request line may be at most 64 KiB; at most 16 connections are
served at once (a 17th is told "too many connections" and closed); a
connection silent for 30 seconds is closed. Timestamps are strictly
increasing even if the system clock steps backwards.

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
| `src/config.rs`, `src/cli.rs` | The command line: options, defaults and validation |
| `src/proc.rs` | Reading and parsing `/proc` and `/sys` files |
| `src/sampler.rs` | Turning successive readings into rows, including rates |
| `src/metrics.rs` | The list of metrics every record holds |
| `src/store.rs` | The bounded in-memory store: one preallocated buffer, oldest record discarded when full |
| `src/protocol.rs`, `src/server.rs` | The JSON-lines protocol and the connection handling |
| `src/daemon.rs` | Putting it together: the sampler thread, the socket, startup checks |
| `src/main.rs` | A thin wrapper around the library |
| `tests/daemon.rs` | End to end: runs the real binary on a real socket |
| `tests/fixtures/root/` | Real `/proc` and `/sys` files captured from a Raspberry Pi 5, used by the tests |

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
