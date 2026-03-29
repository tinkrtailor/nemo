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
  description = "Hetzner server type"
  type        = string
  default     = "ccx43"
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
  description = "k3s version to install"
  type        = string
  default     = "v1.30.4+k3s1"
}

variable "nginx_ingress_version" {
  description = "nginx-ingress controller version"
  type        = string
  default     = "v1.10.0"
}

variable "cert_manager_version" {
  description = "cert-manager version"
  type        = string
  default     = "v1.14.0"
}

variable "ingress_class" {
  description = "Ingress class name"
  type        = string
  default     = "nginx"
}

variable "image_pull_secret_dockerconfigjson" {
  description = "Docker config JSON for private registry access. If provided, creates nemo-registry-creds Secret."
  type        = string
  default     = null
  sensitive   = true
}
