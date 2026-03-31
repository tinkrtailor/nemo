# Example: Install Nautiloop on an existing Linux server (any provider)
#
# Single terraform apply — no kubernetes/helm provider needed.
# The module handles k8s resource creation via SSH+kubectl on the server.
#
# Usage:
#   cd terraform/examples/existing-server
#   terraform init
#   terraform apply

terraform {
  required_version = ">= 1.5"
}

module "nautiloop" {
  source = "../../modules/nautiloop"

  # Required: give me a server, I'll install nemo on it
  server_ip       = var.server_ip
  ssh_private_key = file(pathexpand(var.ssh_private_key_path))
  ssh_user        = var.ssh_user

  # Required: repo + credentials
  git_repo_url         = var.git_repo_url
  git_host_token       = var.git_host_token
  repo_ssh_private_key = var.repo_ssh_private_key

  # Optional: domain + TLS (skip for IP-only)
  domain     = var.domain
  acme_email = var.acme_email

  # Optional: images (defaults to latest public GHCR)
  control_plane_image = var.control_plane_image
  agent_base_image    = var.agent_base_image
  sidecar_image       = var.sidecar_image
}

output "nemo_server_url" {
  value = module.nautiloop.server_url
}

output "nemo_api_key" {
  value     = module.nautiloop.api_key
  sensitive = true
}

output "nemo_deploy_key_public" {
  value = module.nautiloop.deploy_key_public
}

output "nemo_post_apply_instructions" {
  value = module.nautiloop.post_apply_instructions
}
