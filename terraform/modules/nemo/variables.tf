# Module inputs — provider-agnostic Nemo installation
# The caller provisions the server; this module installs everything on it.

# --- Required: server access ---

variable "server_ip" {
  description = "IP address of the server to install Nemo on"
  type        = string
}

variable "ssh_private_key" {
  description = "SSH private key content for server provisioning (not a path)"
  type        = string
  sensitive   = true
}

variable "ssh_user" {
  description = "SSH user for server access"
  type        = string
  default     = "root"
}

# --- Required: repo + credentials ---

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
  description = "SSH private key for git repo access (used by repo-init and sidecar). PEM format."
  type        = string
  sensitive   = true
}

# --- Optional: domain + TLS ---

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

# --- Optional: images ---

variable "control_plane_image" {
  description = "Control plane container image"
  type        = string
  default     = "ghcr.io/tinkrtailor/nemo-control-plane:0.1.0"
}

variable "agent_base_image" {
  description = "Agent base container image"
  type        = string
  default     = "ghcr.io/tinkrtailor/nemo-agent-base:0.1.0"
}

variable "sidecar_image" {
  description = "Auth sidecar container image"
  type        = string
  default     = "ghcr.io/tinkrtailor/nemo-sidecar:0.1.0"
}

# --- Optional: tuning ---

variable "k3s_version" {
  description = "k3s version to install (v1.32+ required for Traefik v3 CRDs)"
  type        = string
  default     = "v1.32.13+k3s1"

  validation {
    condition     = can(regex("^v1\\.(3[2-9]|[4-9][0-9])", var.k3s_version))
    error_message = "k3s_version must be v1.32 or later (Traefik v3 CRDs required)."
  }
}

variable "cert_manager_version" {
  description = "cert-manager version (only used when domain is set)"
  type        = string
  default     = "v1.14.0"
}

variable "postgres_password" {
  description = "Postgres password. Auto-generated if empty."
  type        = string
  default     = ""
  sensitive   = true
}

variable "postgres_volume_size" {
  description = "Size of the Postgres data volume in Gi"
  type        = number
  default     = 20
}

variable "ssh_known_hosts" {
  description = "Known hosts entries for git remote (from ssh-keyscan). If empty, ssh-keyscan runs automatically."
  type        = string
  default     = ""
}

variable "image_pull_secret_dockerconfigjson" {
  description = "Docker config JSON for private registry access. If provided, creates nemo-registry-creds Secret."
  type        = string
  default     = null
  sensitive   = true
}
