# Two providers, each with an entry relay near Gulf players and an exit
# relay in Frankfurt for European game servers (region "eu-central"). Each
# entry is paired with the exit on its own provider so the long leg rides
# that provider's backbone (PLAN.md section 10); the other exit is listed
# too, for when it measures faster.
#
#   AWS:    bah-aws (Bahrain, entry) -> fra-aws (Frankfurt, exit)
#   Google: doh-gcp (Doha, entry)    -> fra-gcp (Frankfurt, exit)
#
# After `terraform apply`, read each exit's WireGuard public key from its
# log (`journalctl -u pingpath-relay | grep "public key"`) and add every
# relay to the control plane's config; the `control_plane_relays` output
# has the entries to start from.

locals {
  common = {
    issuer_public_keys = var.issuer_public_keys
    control_url        = var.control_url
    binary_url         = var.binary_url
    binary_sha256      = var.binary_sha256
  }
  relays = {
    fra-aws = { id = 1, roles = ["entry", "exit"], regions = ["eu-central"], provider = "aws", location = "Frankfurt" }
    bah-aws = { id = 2, roles = ["entry"], regions = [], provider = "aws", location = "Bahrain" }
    fra-gcp = { id = 3, roles = ["entry", "exit"], regions = ["eu-central"], provider = "gcp", location = "Frankfurt" }
    doh-gcp = { id = 4, roles = ["entry"], regions = [], provider = "gcp", location = "Doha" }
  }
  addresses = {
    fra-aws = module.fra_aws.address
    bah-aws = module.bah_aws.address
    fra-gcp = module.fra_gcp.address
    doh-gcp = module.doh_gcp.address
  }
  # Every relay lists every other relay: exits to forward to, and peers to probe.
  peers = {
    for name, relay in local.relays : name => [
      for other, o in local.relays : {
        id      = o.id
        name    = other
        address = local.addresses[other]
        roles   = o.roles
      } if other != name
    ]
  }
  keys = nonsensitive(toset(keys(var.control_api_keys)))
}

module "fra_aws" {
  source    = "../../modules/aws-relay"
  providers = { aws = aws.frankfurt }
  id        = local.relays.fra-aws.id
  name      = "fra-aws"
  relay = merge(local.common, {
    roles   = local.relays.fra-aws.roles
    regions = local.relays.fra-aws.regions
    peers   = local.peers.fra-aws
  })
  control_api_key = contains(local.keys, "fra-aws") ? var.control_api_keys["fra-aws"] : ""
  admin_cidrs     = var.admin_cidrs
}

module "bah_aws" {
  source    = "../../modules/aws-relay"
  providers = { aws = aws.bahrain }
  id        = local.relays.bah-aws.id
  name      = "bah-aws"
  relay = merge(local.common, {
    roles = local.relays.bah-aws.roles
    peers = local.peers.bah-aws
  })
  control_api_key = contains(local.keys, "bah-aws") ? var.control_api_keys["bah-aws"] : ""
  admin_cidrs     = var.admin_cidrs
}

module "fra_gcp" {
  source  = "../../modules/gcp-relay"
  project = var.gcp_project
  zone    = "europe-west3-a"
  id      = local.relays.fra-gcp.id
  name    = "fra-gcp"
  relay = merge(local.common, {
    roles   = local.relays.fra-gcp.roles
    regions = local.relays.fra-gcp.regions
    peers   = local.peers.fra-gcp
  })
  control_api_key = contains(local.keys, "fra-gcp") ? var.control_api_keys["fra-gcp"] : ""
  admin_cidrs     = var.admin_cidrs
}

module "doh_gcp" {
  source  = "../../modules/gcp-relay"
  project = var.gcp_project
  zone    = "me-central1-a"
  id      = local.relays.doh-gcp.id
  name    = "doh-gcp"
  relay = merge(local.common, {
    roles = local.relays.doh-gcp.roles
    peers = local.peers.doh-gcp
  })
  control_api_key = contains(local.keys, "doh-gcp") ? var.control_api_keys["doh-gcp"] : ""
  admin_cidrs     = var.admin_cidrs
}
