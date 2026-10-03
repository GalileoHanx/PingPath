variable "relay" {
  description = "The relay's configuration; see relay/relay.example.toml for what each field means."
  type = object({
    # Registry ID; must match the control plane's [[relays]] entry.
    id   = number
    name = string
    # "entry", "exit" or both.
    roles = list(string)
    # Exit role: the game regions this relay serves, e.g. ["eu-central"].
    regions = optional(list(string), [])
    # The control plane's token-signing public keys (pingpath-control keygen).
    issuer_public_keys = list(string)
    # The other relays: exits an entry forwards to, and peers to probe.
    peers = optional(list(object({
      id      = number
      name    = string
      address = string
      roles   = list(string)
    })), [])
    # Where to report relay-to-relay latency; empty for none.
    control_url = optional(string, "")
    # A Linux x86-64 pingpath-relay binary and its SHA-256 (docs/relay-operator.md).
    binary_url    = string
    binary_sha256 = string
    port          = optional(number, 51820)
    # Overrides for [limits]; anything left out keeps the relay's default.
    limits              = optional(map(number), {})
    probe_interval_secs = optional(number, 10)
  })

  validation {
    condition     = length(var.relay.roles) > 0 && alltrue([for r in var.relay.roles : contains(["entry", "exit"], r)])
    error_message = "roles must be \"entry\", \"exit\" or both."
  }
  validation {
    condition     = !contains(var.relay.roles, "exit") || length(var.relay.regions) > 0
    error_message = "The exit role needs regions, the game regions it serves."
  }
  validation {
    condition     = !contains(var.relay.roles, "entry") || anytrue([for p in var.relay.peers : contains(p.roles, "exit")])
    error_message = "The entry role needs at least one peer with the exit role."
  }
  validation {
    condition     = length(var.relay.issuer_public_keys) > 0
    error_message = "issuer_public_keys must list at least one control plane key."
  }
  validation {
    condition     = can(regex("^[0-9a-f]{64}$", var.relay.binary_sha256))
    error_message = "binary_sha256 must be 64 lowercase hex digits."
  }
}

variable "control_api_key" {
  description = "This relay's API key for reporting latency (pingpath-control api-key). Ends up in the instance's user data."
  type        = string
  default     = ""
  sensitive   = true
}
