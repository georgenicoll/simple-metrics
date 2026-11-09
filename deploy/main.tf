# simple-metrics on one machine, as two containers from one image:
#
#   collector  simple-metrics, sampling the HOST's /proc and /sys. It shares
#              the host's network namespace, because /proc/net/dev is per
#              namespace and the interfaces to chart are the machine's, not
#              the container's. It keeps its history in a volume, so it
#              survives restarts and upgrades.
#   web        smweb, the page. Published on http_bind_address:http_port.
#              It reaches the collector over a Unix socket in a volume the
#              two share, so nothing about the collector is on the network.
#
# Both run as an unprivileged user, with a read-only root filesystem, no
# capabilities and a memory limit.

locals {
  title = var.title != "" ? var.title : var.host

  # Where the collector's pieces are, inside the containers.
  socket = "/run/simple-metrics/simple-metrics.sock"
  user   = "65532:65532" # the user the image's directories belong to

  ssh_opts = concat(
    ["-o", "StrictHostKeyChecking=accept-new"],
    var.ssh_private_key_path != "" ? ["-i", var.ssh_private_key_path] : [],
  )
}

# Docker on the target machine, over SSH: nothing is installed there and no
# sudo is used, but ssh_user does need to be allowed to run docker.
provider "docker" {
  host     = "ssh://${var.ssh_user}@${var.host}"
  ssh_opts = local.ssh_opts
}

resource "docker_image" "simple_metrics" {
  name = var.image
  # Normally unset: the machine's Docker picks the variant that matches it.
  # Set architecture to insist on one (a mismatch then fails the pull).
  platform     = var.architecture != "" ? "linux/${var.architecture}" : null
  keep_locally = true
}

# The collector's socket, shared by both containers.
resource "docker_volume" "run" {
  name = "${var.name_prefix}-run"
}

# The collector's history file (see "Keeping the history" in the README).
resource "docker_volume" "state" {
  name = "${var.name_prefix}-state"
}

resource "docker_container" "collector" {
  name    = "${var.name_prefix}-collector"
  image   = docker_image.simple_metrics.image_id
  restart = "unless-stopped"

  command = concat(
    [
      "/simple-metrics",
      "--socket", local.socket,
      "--interval", var.interval,
      "--retention", var.retention,
      "--state-dir", "/state",
      # The host's /proc and /sys, mounted below.
      "--root", "/host",
    ],
    flatten([for name in var.interfaces : ["--interface", name]]),
    # Only meaningful with "all"; the collector refuses it otherwise.
    contains(var.interfaces, "all") ? flatten([for prefix in var.ignore_interfaces : ["--ignore-interface", prefix]]) : [],
  )

  # See the comment at the top: the machine's interfaces, not a container's.
  network_mode = "host"

  user      = local.user
  read_only = true
  # About 15 MB of history, and about 2 MB more for each interface charted
  # (at the default interval and retention), with room to copy a read out.
  memory        = 256
  memory_swap   = 256
  security_opts = ["no-new-privileges:true"]
  capabilities {
    drop = ["ALL"]
  }

  volumes {
    host_path      = "/proc"
    container_path = "/host/proc"
    read_only      = true
  }
  volumes {
    host_path      = "/sys"
    container_path = "/host/sys"
    read_only      = true
  }
  volumes {
    volume_name    = docker_volume.run.name
    container_path = "/run/simple-metrics"
  }
  volumes {
    volume_name    = docker_volume.state.name
    container_path = "/state"
  }
}

resource "docker_container" "web" {
  name    = "${var.name_prefix}-web"
  image   = docker_image.simple_metrics.image_id
  restart = "unless-stopped"

  command = [
    "/smweb",
    "--socket", local.socket,
    "--http", "0.0.0.0:8080", # inside the container; published below
    "--title", local.title,
  ]

  user          = local.user
  read_only     = true
  memory        = 64
  memory_swap   = 64
  security_opts = ["no-new-privileges:true"]
  capabilities {
    drop = ["ALL"]
  }

  ports {
    internal = 8080
    external = var.http_port
    ip       = var.http_bind_address
  }

  volumes {
    volume_name    = docker_volume.run.name
    container_path = "/run/simple-metrics"
  }

  # Not needed for it to start (it asks the collector per request), but it
  # makes a deploy bring them up in a sensible order.
  depends_on = [docker_container.collector]
}
