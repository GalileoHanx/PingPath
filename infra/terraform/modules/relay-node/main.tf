# Renders a relay's config and a cloud-init document that installs and
# starts it on a fresh Ubuntu or Debian machine. Cloud-specific modules
# (aws-relay, gcp-relay) pass the result as user data.
#
# The exit's WireGuard private key is created on the machine and never
# leaves it; the relay logs the public key at startup, for the control
# plane's registry.

locals {
  is_exit = contains(var.relay.roles, "exit")
  repo    = "${path.module}/../../../.."
  # Whether a key is set is not itself secret.
  has_link = var.relay.control_url != "" && nonsensitive(var.control_api_key != "")

  config = templatefile("${path.module}/templates/relay.toml.tftpl", {
    relay       = var.relay
    is_exit     = local.is_exit
    has_control = local.has_link
  })

  user_data = templatefile("${path.module}/templates/cloud-init.yaml.tftpl", {
    relay           = var.relay
    is_exit         = local.is_exit
    has_control     = local.has_link
    config          = local.config
    control_api_key = var.control_api_key
    service         = file("${local.repo}/infra/systemd/pingpath-relay.service")
    sysctl          = file("${local.repo}/infra/systemd/99-pingpath-relay.conf")
  })
}
