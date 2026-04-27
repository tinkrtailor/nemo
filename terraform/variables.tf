# Root terraform variables — Hetzner provisioning + Nautiloop module pass-through

# --- Hetzner-specific ---

variable "hetzner_api_token" {
  description = "Hetzner Cloud API token"
  type        = string
  sensitive   = true
}

variable "ssh_public_keys" {
  description = "SSH public keys for server access"
  type        = list(string)

  validation {
    condition     = length(var.ssh_public_keys) > 0
    error_message = "At least one SSH public key is required for server bootstrap."
  }
}

variable "ssh_private_key_path" {
  description = "Path to SSH private key for Hetzner server provisioning"
  type        = string
  default     = "~/.ssh/id_ed25519"
}

variable "server_type" {
  description = "Hetzner server type (e.g., cpx31, ccx23, ccx43)"
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
  description = "Git repository URL (SSH format: git@github.com:user/repo.git)"
  type        = string
}

variable "git_host_token" {
  description = "GitHub PAT with repo + PR permissions for PR creation/merge"
  type        = string
  sensitive   = true
}

variable "repo_ssh_private_key" {
  description = "SSH private key for git repo access. If null, the module generates an ED25519 key."
  type        = string
  default     = null
  sensitive   = true
}

variable "domain" {
  description = "Domain for the control plane. null = HTTP on raw IP, no TLS."
  type        = string
  default     = null
}

variable "acme_email" {
  description = "Email for Let's Encrypt certificate registration. Required if domain is set."
  type        = string
  default     = null
}

variable "control_plane_image" {
  description = "Control plane container image"
  type        = string
  default     = "ghcr.io/tinkrtailor/nautiloop-control-plane:0.7.16"
}

variable "agent_base_image" {
  description = "Agent base container image"
  type        = string
  default     = "ghcr.io/tinkrtailor/nautiloop-agent-base:0.7.16"
}

variable "sidecar_image" {
  description = "Auth sidecar container image"
  type        = string
  default     = "ghcr.io/tinkrtailor/nautiloop-sidecar:0.7.16"
}

variable "k3s_version" {
  description = "k3s version to install (v1.32+ required for Traefik v3 CRDs)"
  type        = string
  default     = "v1.32.13+k3s1"
}

variable "cert_manager_version" {
  description = "cert-manager version"
  type        = string
  default     = "v1.14.0"
}

variable "postgres_password" {
  description = "Postgres password. Generated if not provided."
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
  description = "Known hosts entries for git remote (from ssh-keyscan). If empty, ssh-keyscan runs automatically."
  type        = string
  default     = ""
}

variable "image_pull_secret_dockerconfigjson" {
  description = "Docker config JSON for private registry access. If provided, creates nautiloop-registry-creds Secret."
  type        = string
  default     = null
  sensitive   = true
}

variable "timeouts" {
  description = "Cluster-wide [timeouts] overrides for stage Jobs. See module docs for precedence."
  type = object({
    implement_secs = optional(number)
    review_secs    = optional(number)
    test_secs      = optional(number)
    audit_secs     = optional(number)
    revise_secs    = optional(number)
    watchdog_secs  = optional(number)
  })
  default = null
}
