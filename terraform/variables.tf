# FR-51: Required input variables

variable "hetzner_api_token" {
  description = "Hetzner Cloud API token"
  type        = string
  sensitive   = true
}

variable "domain" {
  description = "Domain for the control plane (must have DNS A record pointing to server IP)"
  type        = string
}

variable "git_repo_url" {
  description = "Git repository URL (SSH format: git@github.com:user/repo.git)"
  type        = string
}

variable "ssh_public_keys" {
  description = "SSH public keys for server access"
  type        = list(string)
}

variable "acme_email" {
  description = "Email for Let's Encrypt certificate registration"
  type        = string
}

variable "ssh_known_hosts" {
  description = "Known hosts entries for git remote (from ssh-keyscan). If empty, ssh-keyscan runs automatically."
  type        = string
  default     = ""
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

variable "ssh_private_key_path" {
  description = "Path to SSH private key for Hetzner server provisioning"
  type        = string
  default     = "~/.ssh/id_ed25519"
}

# FR-52: Optional input variables

variable "server_type" {
  description = "Hetzner server type (e.g., cpx31, ccx23, ccx43)"
  type        = string
  # Common types:
  # - cpx31: 4 vCPU (Shared), 8 GB RAM   (~€15/mo) - Good for testing
  # - ccx23: 4 vCPU (Dedicated), 16 GB RAM (~€46/mo) - Solid performance
  # - ccx33: 8 vCPU (Dedicated), 32 GB RAM (~€92/mo) - Heavy lifting
  # - ccx43: 16 vCPU (Dedicated), 64 GB RAM (~€185/mo) - Maximum scale
  default     = "ccx23"
}

variable "server_location" {
  description = "Hetzner server location"
  type        = string
  default     = "fsn1"
}

variable "node_count" {
  description = "Number of nodes (1 for V1 single-node)"
  type        = number
  default     = 1
}

variable "postgres_password" {
  description = "Postgres password. Generated if not provided."
  type        = string
  default     = ""
  sensitive   = true
}

variable "control_plane_image" {
  description = "Control plane container image"
  type        = string
  default     = "ghcr.io/nemo/control-plane:latest"
}

variable "agent_base_image" {
  description = "Agent base container image"
  type        = string
  default     = "ghcr.io/nemo/agent-base:latest"
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



variable "image_pull_secret_dockerconfigjson" {
  description = "Docker config JSON for private registry access. If provided, creates nemo-registry-creds Secret."
  type        = string
  default     = null
  sensitive   = true
}
