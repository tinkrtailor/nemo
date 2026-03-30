# Module outputs

output "server_url" {
  description = "URL of the Nemo control plane (http://IP:80 or https://domain)"
  value       = local.server_url
}

output "api_key" {
  description = "API key for CLI authentication"
  value       = random_password.api_key.result
  sensitive   = true
}

output "kubeconfig_path" {
  description = "Path to the generated kubeconfig file"
  value       = local.kubeconfig_path
}

output "namespace_system" {
  description = "K8s namespace for control plane components"
  value       = "nemo-system"
}

output "namespace_jobs" {
  description = "K8s namespace for agent jobs"
  value       = "nemo-jobs"
}
