output "public_ip" {
  description = "The relay's fixed public IPv4 address."
  value       = aws_eip.relay.public_ip
}

output "address" {
  description = "Where clients and other relays send UDP, as ip:port."
  value       = "${aws_eip.relay.public_ip}:${var.port}"
}

output "instance_id" {
  value = aws_instance.relay.id
}

output "config" {
  description = "The relay's rendered config."
  value       = module.node.config
}
