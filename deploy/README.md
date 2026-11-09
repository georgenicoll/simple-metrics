# Deploying simple-metrics with Docker

An OpenTofu (or Terraform) configuration that runs simple-metrics on one
machine, over SSH, from the container image the release workflow publishes.
It starts two containers:

| Container | Runs | |
|---|---|---|
| `simple-metrics-collector` | `simple-metrics` | Samples the **host's** CPU, memory, temperature and network use. Shares the host's network namespace (the interfaces to chart are the machine's, not the container's) and sees the host's `/proc` and `/sys` read-only. Keeps its history in a volume, so it survives restarts and upgrades. |
| `simple-metrics-web` | `smweb` | The page, published on `http_bind_address:http_port` as plain HTTP. Reads from the collector over a Unix socket in a volume the two containers share, so the collector itself is not on the network. |

Both run as an unprivileged user with a read-only root filesystem, all
capabilities dropped, `no-new-privileges` and a memory limit.

**There is no login on the page.** Anyone who can reach `http_bind_address:http_port`
sees the metrics. Serve it on a trusted LAN address, or `127.0.0.1`.

## What you need

- **On this machine:** [OpenTofu](https://opentofu.org) (or Terraform) and an
  `ssh` client that can log in to the target without a password prompt
  (an agent, or `ssh_private_key_path`).
- **On the target machine:** Docker (with the `docker` CLI; tested with 29),
  and an SSH user who can run it, i.e. is in the `docker` group. Nothing else
  is installed there, and **no `sudo` or sudoers entry is needed**. Note that
  membership of the `docker` group is equivalent to root on that machine.
- **An image to run:** a release's image, `ghcr.io/georgenicoll/simple-metrics:<version>`.
  Its package must be public, or the target must be logged in to ghcr.io.

## The machine is configuration, not code

Which machine to deploy to is **not** in the repository. Copy the example and
fill it in:

```bash
cd deploy
cp example.server.tfvars server.tfvars     # server.tfvars is git-ignored
$EDITOR server.tfvars
```

| Setting | |
|---|---|
| `host` | The machine's hostname or IP, e.g. `your.server.name`. |
| `ssh_user` | The user to log in as. Must be able to run `docker`. |
| `ssh_private_key_path` | Optional. Leave out to use your agent or ssh config. |
| `architecture` | Optional, normally left out. The image supports `amd64` and `arm64` and the machine's Docker picks the one that matches it. Set it to insist on one (`uname -m`: `x86_64` is `amd64`, `aarch64` is `arm64`); a mismatch then fails the pull. |
| `image` | The image, pinned by digest: `ghcr.io/georgenicoll/simple-metrics:v0.5.0@sha256:...`. A release's `image` job prints this line in its summary. |
| `interfaces` | The network interfaces to chart. The default, `["all"]`, charts every interface the machine has when the collector starts, except `lo` and the `ignore_interfaces`. Or list names, as `ip -br link` shows them (`["eth0", "wg0"]`), to fix the set. A name that doesn't exist is a gap in the chart, not an error. See "Which interfaces" below. |
| `ignore_interfaces` | With `all`: leave out interfaces whose names **start with** any of these. Default `["veth"]`, the virtual interface Docker creates for each container. An interface named in `interfaces` is kept regardless. |
| `http_bind_address`, `http_port` | Where the page is served. Default `127.0.0.1:8080`. Use the machine's LAN address, or `0.0.0.0` for every address. |
| `title`, `interval`, `retention`, `name_prefix` | Optional: the page's heading (default: the host), how often to sample (`5s`), how long to keep (`7d`), and a prefix for the container and volume names. |

To deploy to a second machine, make a second file (e.g. `other.tfvars`) and
give it its own state, since each machine's containers are tracked in the
state: add `-state=other.tfstate` to every `tofu` command for it. Both are
git-ignored.

## Which interfaces

With the default (`interfaces = ["all"]`) the collector looks at the machine's
interfaces **once, when it starts**, and charts every one except `lo` and those
whose names start with an `ignore_interfaces` prefix (`veth` by default, so the
interface Docker makes for each container is left out; add `docker`, `br-` and
so on for the bridges). The list it settled on is in the collector's log
(`docker logs simple-metrics-collector`).

- An interface that appears **after** the collector started isn't charted
  until it restarts (`tofu apply` doesn't restart it if nothing changed; use
  `ssh <user>@<host> docker restart simple-metrics-collector`).
- If the set of interfaces differs after a restart, **the saved history is
  discarded** and starts afresh, because the history file belongs to one exact
  set of metrics. Ignoring the interfaces that come and go (`veth`) is what
  keeps it stable. If you need it absolutely fixed, list the interfaces
  instead of using `all`.
- Each interface adds about 2 MB of memory; the collector's container is
  limited to 256 MB, enough for the fixed metrics and dozens of interfaces
  (at most 64 are charted).

## Deploy

```bash
cd deploy
tofu init                                  # once
tofu plan  -var-file=server.tfvars
tofu apply -var-file=server.tfvars
```

`apply` prints the page's URL and a command for reading the containers' logs.

## Upgrade

Change `image` in `server.tfvars` to the new release's (with its digest) and
`tofu apply -var-file=server.tfvars`. The containers are replaced; the history
lives in a volume and carries on, with a short gap in the charts.

## Remove

```bash
tofu destroy -var-file=server.tfvars
```

That removes the containers, the volumes **(including the history)** and the
local copy of the image on the machine.

## Troubleshooting

- **`permission denied` / can't reach the Docker daemon:** `ssh_user` isn't in
  the `docker` group on the machine (`ssh <user>@<host> docker ps` should work).
- **Pulling the image fails with `unauthorized` or `denied`:** the ghcr.io
  package is private. Make it public in the package's settings on GitHub, or
  run `docker login ghcr.io` on the machine.
- **`no matching manifest for linux/...`:** the image has no variant for this
  machine's architecture, or `architecture` is set and isn't what the machine
  is (`uname -m`).
- **The page says "No metrics available":** the collector isn't up; see
  `docker logs simple-metrics-collector` on the machine.
- **An interface is missing from the chart:** it appeared after the collector
  started (restart it, see "Which interfaces"), or its name starts with an
  `ignore_interfaces` prefix. Compare `ip -br link` on the machine with the
  collector's log.
- **The collector exits saying `--interface all found N network interfaces`:**
  more than 64; add prefixes to `ignore_interfaces`.
- **The history started over after a restart:** the set of interfaces changed
  (see "Which interfaces").
- **Temperature is blank:** the machine has no `thermal_zone0`; that is a gap,
  not a fault.
