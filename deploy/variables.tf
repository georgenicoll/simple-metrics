# Which machine, and how to reach it. Set these in server.tfvars (copied from
# example.server.tfvars; never checked in).

variable "host" {
  description = "Hostname or IP address of the machine to deploy to, reachable over SSH, e.g. \"your.server.name\"."
  type        = string
}

variable "ssh_user" {
  description = "The user to log in as over SSH. Needs to be able to run docker (be in the docker group); sudo isn't used."
  type        = string
}

variable "ssh_private_key_path" {
  description = "Path to the SSH private key for ssh_user. Leave empty to use the local SSH agent or your ssh config."
  type        = string
  default     = ""
}

variable "architecture" {
  description = "Optional. Pin the CPU architecture to pull the image for: \"amd64\" (x86_64) or \"arm64\" (aarch64). Leave empty (the default) to use whatever the machine is: the image is multi-architecture, so Docker on the machine picks the right variant itself."
  type        = string
  default     = ""

  validation {
    condition     = contains(["", "amd64", "arm64"], var.architecture)
    error_message = "architecture must be \"amd64\", \"arm64\", or empty."
  }
}

# What to run.

variable "image" {
  description = "The simple-metrics image, ideally pinned by digest: ghcr.io/georgenicoll/simple-metrics:v0.5.0@sha256:... (the release's \"image\" job prints the digest)."
  type        = string
}

variable "interfaces" {
  description = "The network interfaces to chart. \"all\" (the default) means every interface the machine has when the collector starts, except lo and those named in ignore_interfaces; or list names as `ip link` shows them (that fixes the set, and keeps its history stable). One that doesn't exist is a gap in the chart, not an error."
  type        = list(string)
  default     = ["all"]

  validation {
    condition     = alltrue([for name in var.interfaces : can(regex("^[A-Za-z0-9._-]{1,15}$", name))])
    error_message = "interface names must be 1 to 15 letters, digits, dots, dashes or underscores (or \"all\")."
  }
}

variable "ignore_interfaces" {
  description = "With \"all\" in interfaces: leave out the interfaces whose names START with any of these. The default skips the virtual ethernet interface Docker creates for every container, which come and go. An interface named explicitly in interfaces is kept regardless."
  type        = list(string)
  default     = ["veth"]

  validation {
    condition     = alltrue([for prefix in var.ignore_interfaces : can(regex("^[A-Za-z0-9._-]{1,15}$", prefix))])
    error_message = "each prefix must be 1 to 15 letters, digits, dots, dashes or underscores."
  }
}

variable "interval" {
  description = "How often to sample (e.g. 5s)."
  type        = string
  default     = "5s"
}

variable "retention" {
  description = "How much history to keep (e.g. 7d). Memory use grows with this and shrinks with a longer interval: see the README."
  type        = string
  default     = "7d"
}

# Where the page is served.

variable "http_bind_address" {
  description = "The address on the machine to serve the page on. There is NO login, so anyone who can reach it sees the metrics: use the machine's LAN address, or 127.0.0.1 to keep it to the machine itself."
  type        = string
  default     = "127.0.0.1"
}

variable "http_port" {
  description = "The port to serve the page on (plain HTTP)."
  type        = number
  default     = 8080
}

variable "title" {
  description = "The page's heading. Defaults to the host."
  type        = string
  default     = ""
}

variable "name_prefix" {
  description = "Prefix for the names of the containers and volumes, so they can be told from anything else on the machine."
  type        = string
  default     = "simple-metrics"
}
