# Variables for bring-your-own-server deployment

variable "server_ip" {
  description = "IP address of your Linux server"
  type        = string
}

variable "ssh_private_key_path" {
  description = "Path to SSH private key for server access"
  type        = string
  default     = "~/.ssh/id_ed25519"
}

variable "ssh_user" {
  description = "SSH user for server access"
  type        = string
  default     = "root"
}

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
  description = "Domain for the control plane. null = HTTP on raw IP."
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
  default     = "ghcr.io/tinkrtailor/nautiloop-control-plane:0.2.7"
}

variable "agent_base_image" {
  description = "Agent base container image"
  type        = string
  default     = "ghcr.io/tinkrtailor/nautiloop-agent-base:0.2.7"
}

variable "sidecar_image" {
  description = "Auth sidecar container image"
  type        = string
  default     = "ghcr.io/tinkrtailor/nautiloop-sidecar:0.2.7"
}
