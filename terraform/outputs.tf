# Root terraform outputs

output "control_plane_url" {
  description = "URL of the running control plane"
  value       = module.nemo.server_url
}

output "server_ip" {
  description = "Public IP of the Hetzner server"
  value       = hcloud_server.nemo.ipv4_address
}

output "namespace_jobs" {
  description = "K8s namespace for agent jobs"
  value       = module.nemo.namespace_jobs
}

output "namespace_system" {
  description = "K8s namespace for control plane components"
  value       = module.nemo.namespace_system
}

output "api_key" {
  description = "API key for CLI authentication"
  value       = module.nemo.api_key
  sensitive   = true
}

output "deploy_key_public" {
  description = "Public key to add as a deploy key (null if you provided your own)"
  value       = module.nemo.deploy_key_public
}

output "post_apply_instructions" {
  description = "Post-apply next steps"
  value       = module.nemo.post_apply_instructions
}
