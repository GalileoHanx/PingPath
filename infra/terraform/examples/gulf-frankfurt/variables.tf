variable "gcp_project" {
  description = "GCP project ID for the Google relays."
  type        = string
}

variable "issuer_public_keys" {
  description = "The control plane's token-signing public keys (pingpath-control keygen)."
  type        = list(string)
}

variable "control_url" {
  description = "The control plane's public URL, for latency reports; empty for none."
  type        = string
  default     = ""
}

variable "control_api_keys" {
  description = "API key per relay name (pingpath-control api-key), for latency reports."
  type        = map(string)
  default     = {}
  sensitive   = true
}

variable "binary_url" {
  description = "Where the relays download a Linux x86-64 pingpath-relay binary."
  type        = string
}

variable "binary_sha256" {
  description = "SHA-256 of that binary."
  type        = string
}

variable "admin_cidrs" {
  description = "Addresses allowed to SSH into the relays."
  type        = list(string)
  default     = []
}
