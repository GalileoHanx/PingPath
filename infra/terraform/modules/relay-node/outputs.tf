output "config" {
  description = "The rendered /etc/pingpath/relay.toml."
  value       = local.config
}

output "user_data" {
  description = "cloud-init user data that installs and starts the relay."
  value       = local.user_data
  sensitive   = true
}

output "port" {
  description = "The UDP port to open to everyone."
  value       = var.relay.port
}
