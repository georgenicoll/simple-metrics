# Copy this to server.tfvars (which is not checked in) and fill it in:
#
#   cp deploy/example.server.tfvars deploy/server.tfvars
#   ./sms init
#   ./sms plan
#   ./sms apply

# The machine to deploy to, reachable over SSH from here.
host = "your.server.name"

# Who to log in as. Must be able to run docker (be in the docker group).
ssh_user = "your-ssh-user"

# Optional: a private key for ssh_user. Leave out to use your SSH agent / ssh config.
# ssh_private_key_path = "~/.ssh/id_ed25519"

# Optional: insist on a CPU architecture ("amd64" or "arm64"). Normally leave
# this out: the image supports both and the machine's Docker picks the one
# that matches it (`uname -m`: x86_64 is amd64, aarch64 is arm64).
# architecture = "amd64"

# The image to run, pinned by digest. A release's "image" job prints the line
# to copy (in its summary on the Actions page).
image = "ghcr.io/georgenicoll/simple-metrics:v0.5.0@sha256:REPLACE_WITH_THE_DIGEST"

# The network interfaces to chart. "all" (the default if you leave this out)
# charts every interface the machine has when the collector starts, except lo
# and the ones in ignore_interfaces. Or list them: see `ip -br link` on the
# machine, e.g. ["eth0", "wg0"].
# interfaces = ["all"]

# With "all": skip interfaces whose names START with these. The default skips
# the "veth" interfaces Docker makes for each container.
# ignore_interfaces = ["veth"]

# Where to serve the page. There is no login, so choose who can reach it: the
# machine's LAN address (or "0.0.0.0" for every address), or "127.0.0.1" for
# the machine only.
http_bind_address = "0.0.0.0"
http_port         = 8080

# Optional. How the page is headed, how often to sample, how long to keep.
# title     = "my server"
# interval  = "5s"
# retention = "7d"
