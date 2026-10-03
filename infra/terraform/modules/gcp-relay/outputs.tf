output "public_ip" {
  description = "The relay's fixed public IPv4 address."
  value       = google_compute_address.relay.address
}

output "address" {
  description = "Where clients and other relays send UDP, as ip:port."
  value       = "${google_compute_address.relay.address}:${var.port}"
}

output "instance_id" {
  value = google_compute_instance.relay.instance_id
}

output "config" {
  description = "The relay's rendered config."
  value       = module.node.config
}
