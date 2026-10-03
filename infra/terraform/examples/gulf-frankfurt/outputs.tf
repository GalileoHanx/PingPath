output "relay_addresses" {
  description = "Each relay's UDP address."
  value       = local.addresses
}

output "control_plane_relays" {
  description = "[[relays]] entries for the control plane's config; add each exit's public_key and each relay's api_key_sha256."
  value = join("\n", [
    for name, r in local.relays : join("\n", compact([
      "[[relays]]",
      "id = ${r.id}",
      "name = \"${name}\"",
      "address = \"${local.addresses[name]}\"",
      "roles = ${jsonencode(r.roles)}",
      length(r.regions) > 0 ? "regions = ${jsonencode(r.regions)}" : "",
      contains(r.roles, "exit") ? "public_key = \"<from the relay's log>\"" : "",
      "provider = \"${r.provider}\"",
      "location = \"${r.location}\"",
      "",
    ]))
  ])
}
