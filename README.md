# simple-metrics

A small, efficient metrics collector for a single Linux machine.

It samples a fixed set of variables at a regular interval, keeps only the most
recent records in memory (older ones are discarded as new ones arrive), and
serves them over a Unix socket as JSON. By default nothing is written to disk;
with `--state-dir` the history is also kept in a file, so it survives a restart
or a reboot. No root is needed: everything it reads is world-readable under `/proc` and `/sys`.

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
you say otherwise: name others with `--interface`, or chart every interface
the machine has with `--interface all` (see "Choosing the network interfaces").
A value that can't be worked out (an interface that isn't
there, a counter that was reset, the first sample's rates, a file that can't be
read) is a **gap**: `null` in the JSON, never a misleading zero.

## Querying it: `smq`

`smq` is a small command-line client, built alongside the daemon and shipped in
the same release archive. It sends one query and prints the answer as a table
(or, with `--json`, as the JSON the daemon sent).

```console
$ smq info
$ smq metrics
$ smq latest
$ smq read --metric cpu_percent --from 1h --points 60 --extremes
$ smq raw '{"op":"read","metrics":["load1"],"from":1790380000000}'
```

By default it uses the daemon's default socket,
`/run/simple-metrics/simple-metrics.sock`. `--local` uses
`/tmp/simple-metrics.sock`, where `./run_local.sh` (which builds and runs the
daemon in the foreground, sampling every second) puts it; `--socket <PATH>`
or the `SIMPLE_METRICS_SOCKET` environment variable choose any other. A time
for `--from` and `--to` is `now`, how long ago (`90s`, `5m`, `2h`, `7d`), or
milliseconds since the Unix epoch. `smq --help` lists everything.

It needs to be allowed to use the socket: the daemon's default is the owner
and group only.

## Seeing it: `smweb`

`smweb` is a small read-only web server, built alongside the daemon and shipped
in the same release archive and image. It serves the chart of any metric over
the last hour, 6 hours, day or week, refreshing every 5 seconds: the line is
the average and the shaded band the smallest-to-largest value in each time
bucket, so a short spike still shows on a week-long chart. The page, its
script and styles and the chart library (uPlot) are built into the binary, so
there is nothing else to install.

```bash
smweb --http 127.0.0.1:8080     # then open http://127.0.0.1:8080/
```

| Option | Default | |
|---|---|---|
| `--socket <PATH>` | `/run/simple-metrics/simple-metrics.sock` | The collector to read from. |
| `--http <ADDR:PORT>` | `127.0.0.1:8080` (if no `--unix`) | Serve HTTP here, e.g. `0.0.0.0:80`. |
| `--unix <PATH>` | none | Also (or instead) serve HTTP on this Unix socket, for a reverse proxy on the same machine. |
| `--unix-mode <MODE>` | `660` | That socket's permissions, in octal. |
| `--base-path <PATH>` | none | Where a reverse proxy mounts it, e.g. `/metrics`, so the links in its pages work. |
| `--title <TEXT>` | `Metrics` | The heading of the page. |

| Path | |
|---|---|
| `/` | The chart as a complete page. |
| `/fragment` | Just the chart's markup (pickers, chart, script and style links), for another web app to embed in its own page. |
| `/data?metric=NAME&range=1h` | The chart's data as JSON: `timestamps`, `avg`, `min`, `max`. |
| `/assets/NAME` | The script, styles and chart library. |
| `/healthz` | `ok`. |

**There is no login.** Anyone who can connect sees the metrics, so listen only
where that is acceptable: a Unix socket with restricted permissions, the
machine's own address, or a network you trust. It serves plain HTTP only, one
request per connection, and only `GET` and `HEAD`.

To embed the chart in a web app that has its own login and styling, run `smweb`
on a Unix socket with `--base-path`, have the app proxy that path to it, and
place `/fragment` in the app's page. The fragment's styles use the page's own
CSS variables (Pico's, if it has them) with fallbacks, so it takes on the
page's look.

## Memory

Room for every record is allocated once, when it starts, and never grows, and
all of it is touched at startup so it is really taken then, not gradually over
days as it fills (a shortage shows immediately). The default is one sample
every 5 seconds for 7 days, which is 120,960 records of 15 values plus a
timestamp: about **15 MB**. (Measured: 16.9 MB resident from the first second,
whether the history is empty or full.) A full read of the whole history streams out as
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
| `--interface <NAME>` | `eth0 wlan0 wlan1 br-ap wg0` | A network interface to report on. Repeat for several; giving any replaces the defaults. `all` means every interface the machine has (see "Choosing the network interfaces"). |
| `--ignore-interface <PREFIX>` | none | With `--interface all`: skip the interfaces whose names start with `PREFIX`. Repeat for several. |
| `--state-dir <DIR>` | none | Also keep the history in `DIR/history.bin`, so it survives restarts and reboots. See "Keeping the history". Created if need be. |
| `--flush-interval <TIME>` | `1m` | How often new records are written to that file. |
| `--root <DIR>` | `/` | Read `proc/` and `sys/` under this directory. For testing. |

A `TIME` is a whole number with an optional unit (`ms`, `s`, `m`, `h`, `d`;
seconds if none): `500ms`, `5s`, `90m`, `7d`. Settings that would need more
than 1 GiB for the store are refused.

## Choosing the network interfaces

By default the daemon reports on five named interfaces (the ones a particular
Raspberry Pi has). Name your own with `--interface`, once for each:

```bash
simple-metrics --interface eth0 --interface wg0
```

or, on a machine whose interfaces you don't want to list, chart all of them:

```bash
simple-metrics --interface all --ignore-interface veth --ignore-interface docker
```

`--interface all` lists the machine's interfaces **once, when the daemon
starts**, and charts each of them, in name order, except:

- the loopback, `lo`, which is never charted;
- any whose name **starts with** an `--ignore-interface` prefix. This is how
  to skip the virtual ones that come and go, such as the `veth…` interface
  Docker makes for every container, or `docker0` and `br-…` bridges. (It
  only applies together with `all`, and the daemon refuses it otherwise.)

An interface also given by name with `--interface` is always kept, even if its
prefix is ignored, and even if it isn't there yet (its chart is a gap until it
appears). The daemon logs the list it settled on when it starts.

Things to know:

- **The set is fixed while the daemon runs.** An interface that appears later
  isn't charted until the daemon restarts; one that goes away is a gap.
- **Changing the set starts the history afresh.** The history file records the
  metrics it holds (see "Keeping the history"), so if the set of interfaces
  differs after a restart, the saved history is ignored. That is why the
  virtual interfaces that come and go are worth ignoring: they would change
  the set, and so reset the history, every time one was added or removed
  before a restart. If the set must stay stable, name the interfaces instead
  of using `all`.
- **Each interface costs memory.** Two metrics (received and sent bytes per
  second), about 2 MB at the default 5 second interval and 7 days: charting 10
  interfaces takes about 35 MB in all, against 15 MB for the fixed metrics
  and five interfaces. At most 64 interfaces are charted; with more, the
  daemon refuses to start and says to ignore some. (Size any memory limit
  for the process accordingly.)

## Keeping the history

Without `--state-dir` the history is memory only and starts empty on every run.
With it, the history is also written to `history.bin` in that directory
(created if need be; the file is readable by its owner only) and loaded again
at startup, so a restart, an upgrade or a reboot leaves a gap in the charts
for as long as the daemon was down, not an empty history.

- **Gentle on an SD card.** The file is an append-only log of fixed-size
  records. New records are collected and appended in one small write every
  `--flush-interval` (about 2 MB a day at the defaults), never the whole
  history rewritten. When the file has grown to twice the store's capacity
  (about every 7 days at the defaults) it is rewritten from memory to a
  temporary file and renamed over the old one, so a complete file always
  exists.
- **Nothing to do at shutdown.** A power cut or a crash loses at most the last
  flush interval's records. Each record has a checksum, so a torn or damaged
  end of the file costs only the damaged records; the rest load, and the file
  is repaired.
- **Safe to change settings.** The file records the metrics it holds. If the
  set changes (different `--interface`s), it is ignored and started afresh,
  never misread. A different `--interval` or `--retention` is fine: only the
  newest records that fit are loaded, and any older than the retention are
  dropped.
- **A clock that hasn't been set.** A Pi has no battery clock, so after a reboot
  its clock can be behind the newest saved record until it reaches the
  network. Records must stay in time order, so the daemon holds back new
  samples while the clock is behind the saved history (for up to 15 minutes),
  and logs it, rather than record wrong timestamps. The clock is normally
  right within seconds, so that is a short extra gap.
- **If writing fails** (disk full, say) it logs once and keeps trying; the
  history in memory carries on unaffected.
- The directory has to be writable by the daemon and is checked at startup: a
  directory it can't use is an error, not a silent loss of history.

It logs to standard error (so, to the journal under systemd), and exits with
an error if the socket can't be set up. It replaces a stale socket left by an
earlier run, but refuses to replace a live one, or anything that isn't a
socket. It needs no signal handling: stopping it (`SIGTERM`) just ends it.

This repository builds the binaries and a container image, and has a ready-made
way to deploy the image (see "Deploying with Docker" below); installing the
binaries any other way, for instance as a systemd service, is up to whoever
deploys them. For trying it out on a development machine, `./run_local.sh`
builds it and runs it in the foreground.

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

## Deploying with Docker

Each release is also published as a multi-architecture (amd64 and arm64)
container image, `ghcr.io/georgenicoll/simple-metrics:<version>`, holding
`simple-metrics`, `smq` and `smweb` as static binaries and nothing else (about
2 MB). Run one container per program, sharing a volume for the collector's
socket. `deploy/` is an OpenTofu (or Terraform) configuration that does exactly
that on a remote machine over SSH, using the Docker provider:

- **The machine is not in the repository.** Copy `deploy/example.server.tfvars`
  to `deploy/server.tfvars` (which is git-ignored) and set its hostname, SSH
  user, the image to run, the network interfaces to chart and where to serve
  the page (and, optionally, to pin the CPU architecture, `amd64` or `arm64`;
  by default the machine's Docker picks the matching variant of the image).
- **Nothing is installed on the machine and no `sudo` is used**; it only needs
  Docker, and an SSH user who can run it (in the `docker` group).

See [deploy/README.md](deploy/README.md) for the steps, what it creates, how to
upgrade and how to remove it.

Other ways of running it, such as a systemd service on a Raspberry Pi, use the
binaries from the release archive; [wireguard-ap](https://github.com/georgenicoll/wireguard-ap)
does that for its Pi.

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
| `src/persist.rs` | The history file: loading it, appending to it, rewriting it |
| `src/store.rs` | The bounded in-memory store: one preallocated buffer, oldest record discarded when full |
| `src/protocol.rs`, `src/server.rs` | The JSON-lines protocol and the connection handling |
| `src/daemon.rs` | Putting it together: the sampler thread, the socket, startup checks |
| `src/query.rs`, `src/bin/smq.rs` | The `smq` client: arguments, the request, printing the answer |
| `src/web.rs`, `src/web_cli.rs`, `src/bin/smweb.rs` | The `smweb` web server: HTTP, routing, the page, and its command line |
| `web/` | What `smweb` serves, built into the binary: the page script and styles, and uPlot (MIT; see `web/uPlot.LICENSE`) |
| `src/main.rs` | A thin wrapper around the library |
| `Dockerfile` | The image: the release's static binaries in an empty image (built by the release workflow, not by hand) |
| `deploy/` | OpenTofu configuration that runs the image on a remote machine over SSH (see "Deploying with Docker") |
| `run_local.sh` | Builds and runs the daemon in the foreground for development (used with `smq --local`) |
| `tests/daemon.rs` | End to end: runs the real binary on a real socket, including kill-and-restart with a state directory |
| `tests/smq.rs` | End to end: runs the real `smq` against the real daemon |
| `tests/smweb.rs` | End to end: runs the real `smweb` against the real daemon, over TCP and a Unix socket |
| `tests/fixtures/root/` | Real `/proc` and `/sys` files captured from a Raspberry Pi 5, used by the tests |

Lint rules live in `Cargo.toml` (`[lints]`) and `clippy.toml`: `unsafe` is
forbidden, and `unwrap`/`expect`/`panic` are warned about outside tests, since
a daemon that runs for weeks should report errors rather than crash.

CI (`.github/workflows/ci.yml`) runs the same checks on x86_64 and aarch64,
using the current stable Rust. There is no supported-older-compiler promise:
this is built as a static binary by CI and the release workflow, not from
source elsewhere (edition 2024 already means Rust 1.85 or newer).

## Releasing

Bump `version` in `Cargo.toml`, commit, then push a matching tag:

```bash
git tag v0.3.0 && git push origin v0.3.0
```

The release workflow refuses a tag that doesn't match `Cargo.toml`, builds
static `aarch64` and `x86_64` archives (each with `simple-metrics`, `smq` and `smweb`),
runs the tests, and publishes them as a GitHub release with a `SHA256SUMS` file.
A last job then builds the multi-architecture image from those same binaries
and pushes it to `ghcr.io/<owner>/simple-metrics:<tag>`; its digest is in that
job's summary, which is the line to put in `deploy/server.tfvars`. The first
time, the package is private: make it public (the package's settings on
GitHub), or the machine being deployed to will need a registry login.

## Licence

[MIT](LICENSE)
