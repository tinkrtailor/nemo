# FR-53: Terraform outputs

output "control_plane_url" {
  description = "HTTPS URL of the running control plane"
  value       = "https://${var.domain}"
}

output "kubeconfig" {
  description = "Kubeconfig for the k3s cluster"
  value       = file(local.kubeconfig_path)
  sensitive   = true
}

output "server_ip" {
  description = "Public IP of the Hetzner server"
  value       = hcloud_server.nemo.ipv4_address
}

output "namespace_jobs" {
  description = "K8s namespace for agent jobs"
  value       = "nemo-jobs"
}

output "namespace_system" {
  description = "K8s namespace for control plane components"
  value       = "nemo-system"
}

# FR-52c: API key output (sensitive)
output "api_key" {
  description = "API key for CLI authentication (from nemo-api-key Secret)"
  value       = random_password.api_key.result
  sensitive   = true
}
