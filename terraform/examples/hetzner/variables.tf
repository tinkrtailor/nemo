# Hetzner + Tailscale example variables

# --- Hetzner ---

variable "hetzner_api_token" {
  description = "Hetzner Cloud API token"
  type        = string
  sensitive   = true
}

variable "tailscale_auth_key" {
  description = "Tailscale auth key (ephemeral, single-use recommended). Generate at https://login.tailscale.com/admin/settings/keys"
  type        = string
  sensitive   = true
}

variable "ssh_public_keys" {
  description = "SSH public keys for initial Hetzner server access (before Tailscale is up)"
  type        = list(string)

  validation {
    condition     = length(var.ssh_public_keys) > 0
    error_message = "At least one SSH public key is required for server bootstrap."
  }
}

variable "ssh_private_key_path" {
  description = "Path to SSH private key for server provisioning"
  type        = string
  default     = "~/.ssh/id_ed25519"
}

variable "server_type" {
  description = "Hetzner server type (e.g., cpx31, ccx23)"
  type        = string
  default     = "ccx23"
}

variable "server_location" {
  description = "Hetzner server location"
  type        = string
  default     = "fsn1"
}

# --- Nautiloop module pass-through ---

variable "git_repo_url" {
  description = "Git repository URL (SSH format)"
  type        = string
}

variable "git_host_token" {
  description = "GitHub PAT with repo + PR permissions"
  type        = string
  sensitive   = true
}

variable "repo_ssh_private_key" {
  description = "SSH private key for git repo access. If null, auto-generates ED25519 key."
  type        = string
  default     = null
  sensitive   = true
}

variable "domain" {
  description = "Domain for public HTTPS. null = API only reachable via Tailscale."
  type        = string
  default     = null
}

variable "acme_email" {
  description = "Email for Let's Encrypt (required if domain is set)"
  type        = string
  default     = null
}

variable "control_plane_image" {
  description = "Control plane container image"
  type        = string
  default     = "ghcr.io/tinkrtailor/nautiloop-control-plane:0.3.8"
}

variable "agent_base_image" {
  description = "Agent base container image"
  type        = string
  default     = "ghcr.io/tinkrtailor/nautiloop-agent-base:0.3.8"
}

variable "sidecar_image" {
  description = "Auth sidecar container image"
  type        = string
  default     = "ghcr.io/tinkrtailor/nautiloop-sidecar:0.3.8"
}

variable "k3s_version" {
  description = "k3s version to install"
  type        = string
  default     = "v1.32.13+k3s1"
}

variable "postgres_password" {
  description = "Postgres password (auto-generated if empty)"
  type        = string
  default     = ""
  sensitive   = true
}

variable "postgres_volume_size" {
  description = "Postgres volume size in Gi"
  type        = number
  default     = 20
}

variable "ssh_known_hosts" {
  description = "Known hosts entries for git remote"
  type        = string
  default     = ""
}

variable "image_pull_secret_dockerconfigjson" {
  description = "Docker config JSON for private registry access"
  type        = string
  default     = null
  sensitive   = true
}
