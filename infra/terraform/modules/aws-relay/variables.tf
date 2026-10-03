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

variable "instance_type" {
  description = "x86-64 instance type. Network-optimized types (c6in, c7i) keep added latency lowest."
  type        = string
  default     = "c7i.large"
}

variable "ami_id" {
  description = "Machine image; defaults to the latest Ubuntu 24.04 LTS from Canonical."
  type        = string
  default     = ""
}

variable "subnet_id" {
  description = "Public subnet to launch in; defaults to a subnet of the default VPC."
  type        = string
  default     = ""
}

variable "ssh_key_name" {
  description = "EC2 key pair for SSH; empty for none (use SSM Session Manager)."
  type        = string
  default     = ""
}

variable "admin_cidrs" {
  description = "Addresses allowed to SSH in. Empty closes SSH."
  type        = list(string)
  default     = []
}

variable "monitoring_cidrs" {
  description = "Addresses allowed to reach health and metrics on 9100 (the relay must also listen beyond localhost)."
  type        = list(string)
  default     = []
}

variable "tags" {
  description = "Tags for every resource."
  type        = map(string)
  default     = {}
}
