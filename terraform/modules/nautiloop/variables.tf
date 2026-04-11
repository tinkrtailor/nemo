# Module inputs — provider-agnostic Nautiloop installation
# The caller provisions the server; this module installs everything on it.

# --- Required: server access ---

variable "server_ip" {
  description = "IP address of the server to install Nautiloop on"
  type        = string

  validation {
    condition     = can(regex("^((25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\\.){3}(25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)$", var.server_ip))
    error_message = "server_ip must be a valid IPv4 address (e.g., 100.64.0.1). IPv6 and hostnames are not supported."
  }
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

  validation {
    condition     = can(regex("^git@", var.git_repo_url))
    error_message = "git_repo_url must be in SSH format (git@host:user/repo.git). HTTPS URLs are not supported."
  }
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

# --- Optional: domain + TLS ---

variable "domain" {
  description = "Domain for the control plane. null = HTTP on raw IP, no TLS."
  type        = string
  default     = null

  validation {
    condition     = var.domain == null || var.domain == "" || can(regex("^[a-zA-Z0-9][a-zA-Z0-9.-]+[a-zA-Z0-9]$", var.domain))
    error_message = "domain must be a valid hostname (e.g., nautiloop.example.com). Do not include http:// or https://."
  }
}

variable "acme_email" {
  description = "Email for Let's Encrypt certificate registration. Required if domain is set."
  type        = string
  default     = null

  validation {
    condition     = var.domain == null || var.domain == "" || (var.acme_email != null && var.acme_email != "")
    error_message = "acme_email is required when domain is set."
  }
}

# --- Optional: images ---

variable "control_plane_image" {
  description = "Control plane container image"
  type        = string
  default     = "ghcr.io/tinkrtailor/nautiloop-control-plane:0.4.3"
}

variable "agent_base_image" {
  description = "Agent base container image"
  type        = string
  default     = "ghcr.io/tinkrtailor/nautiloop-agent-base:0.4.3"
}

variable "sidecar_image" {
  description = "Auth sidecar container image"
  type        = string
  default     = "ghcr.io/tinkrtailor/nautiloop-sidecar:0.4.3"
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

  validation {
    condition     = can(regex("^v[0-9]+\\.[0-9]+\\.[0-9]+$", var.cert_manager_version))
    error_message = "cert_manager_version must be a semver tag (e.g., v1.14.0). No extra flags or characters."
  }
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

variable "kubeconfig_output_path" {
  description = "Path to write the generated kubeconfig. If null, writes to <module>/.state/kubeconfig.yaml."
  type        = string
  default     = null

  validation {
    condition     = var.kubeconfig_output_path == null || length(var.kubeconfig_output_path) > 0
    error_message = "kubeconfig_output_path must be null (use default) or a non-empty path."
  }
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
