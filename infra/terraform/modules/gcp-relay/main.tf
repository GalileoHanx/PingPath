# A PingPath relay on one Compute Engine VM with a fixed public address,
# allocated separately so relays can list each other as peers.

module "node" {
  source          = "../relay-node"
  relay           = merge(var.relay, { id = var.id, name = var.name, port = var.port })
  control_api_key = var.control_api_key
}

locals {
  region = join("-", slice(split("-", var.zone), 0, 2))
  tag    = "pingpath-${var.name}"
  labels = merge({ pingpath-relay-id = tostring(var.id) }, var.labels)
}

resource "google_compute_address" "relay" {
  project = var.project
  name    = "pingpath-${var.name}"
  region  = local.region
  labels  = local.labels
}

resource "google_compute_firewall" "udp" {
  project       = var.project
  name          = "${local.tag}-udp"
  network       = var.network
  description   = "PingPath relay ${var.name}: clients and other relays"
  direction     = "INGRESS"
  source_ranges = ["0.0.0.0/0"]
  target_tags   = [local.tag]
  allow {
    protocol = "udp"
    ports    = [tostring(var.port)]
  }
}

resource "google_compute_firewall" "ssh" {
  count         = length(var.admin_cidrs) > 0 ? 1 : 0
  project       = var.project
  name          = "${local.tag}-ssh"
  network       = var.network
  description   = "PingPath relay ${var.name}: SSH from admin addresses"
  direction     = "INGRESS"
  source_ranges = var.admin_cidrs
  target_tags   = [local.tag]
  allow {
    protocol = "tcp"
    ports    = ["22"]
  }
}

resource "google_compute_firewall" "metrics" {
  count         = length(var.monitoring_cidrs) > 0 ? 1 : 0
  project       = var.project
  name          = "${local.tag}-metrics"
  network       = var.network
  description   = "PingPath relay ${var.name}: health and metrics from monitoring"
  direction     = "INGRESS"
  source_ranges = var.monitoring_cidrs
  target_tags   = [local.tag]
  allow {
    protocol = "tcp"
    ports    = ["9100"]
  }
}

resource "google_compute_instance" "relay" {
  project      = var.project
  name         = "pingpath-${var.name}"
  zone         = var.zone
  machine_type = var.machine_type
  tags         = [local.tag]
  labels       = local.labels

  boot_disk {
    initialize_params {
      image = var.image
      size  = 10
      type  = "pd-balanced"
    }
  }

  network_interface {
    network    = var.network
    subnetwork = var.subnetwork != "" ? var.subnetwork : null
    access_config {
      nat_ip       = google_compute_address.relay.address
      network_tier = "PREMIUM" # Google's backbone between regions
    }
  }

  metadata = {
    user-data              = module.node.user_data
    block-project-ssh-keys = "true"
  }

  shielded_instance_config {
    enable_secure_boot          = true
    enable_vtpm                 = true
    enable_integrity_monitoring = true
  }

  # No service_account block: the relay calls no Google APIs.
}
