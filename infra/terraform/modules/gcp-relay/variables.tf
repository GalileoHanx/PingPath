# id, name and port are separate from `relay` so the address resources
# don't depend on the peer list, which is built from every relay's address.
variable "id" {
  description = "Registry ID; must match the control plane's [[relays]] entry."
  type        = number
}

variable "name" {
  description = "Relay name, e.g. fra-aws; used in resource names."
  type        = string
}

variable "port" {
  description = "UDP port for clients and other relays."
  type        = number
  default     = 51820
}

variable "relay" {
  description = "The rest of the relay's configuration, passed to the relay-node module (see its variables)."
  type = object({
    roles              = list(string)
    regions            = optional(list(string), [])
    issuer_public_keys = list(string)
    peers = optional(list(object({
      id      = number
      name    = string
      address = string
      roles   = list(string)
    })), [])
    control_url         = optional(string, "")
    binary_url          = string
    binary_sha256       = string
    limits              = optional(map(number), {})
    probe_interval_secs = optional(number, 10)
  })
}

variable "control_api_key" {
  description = "This relay's API key for reporting latency (pingpath-control api-key)."
  type        = string
  default     = ""
  sensitive   = true
}

variable "project" {
  description = "GCP project ID."
  type        = string
}

variable "zone" {
  description = "Zone to run in, e.g. europe-west3-a (Frankfurt) or me-central1-a (Doha)."
  type        = string
}

variable "machine_type" {
  description = "x86-64 machine type."
  type        = string
  default     = "n2-standard-2"
}

variable "image" {
  description = "Boot image; Ubuntu 24.04 LTS by default."
  type        = string
  default     = "ubuntu-os-cloud/ubuntu-2404-lts-amd64"
}

variable "network" {
  description = "VPC network. The auto-created \"default\" network allows SSH from anywhere; prefer a dedicated one."
  type        = string
  default     = "default"
}

variable "subnetwork" {
  description = "Subnetwork in the zone's region; empty for the network's default."
  type        = string
  default     = ""
}

variable "admin_cidrs" {
  description = "Addresses allowed to SSH in. Empty adds no SSH rule."
  type        = list(string)
  default     = []
}

variable "monitoring_cidrs" {
  description = "Addresses allowed to reach health and metrics on 9100 (the relay must also listen beyond localhost)."
  type        = list(string)
  default     = []
}

variable "labels" {
  description = "Labels for every resource that takes them."
  type        = map(string)
  default     = {}
}
