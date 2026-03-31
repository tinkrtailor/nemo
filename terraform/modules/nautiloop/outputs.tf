# Module outputs — all machine-readable for `terraform output -json`

output "server_url" {
  description = "URL of the Nemo control plane (http://IP:8080 or https://domain)"
  value       = local.server_url
}

output "api_key" {
  description = "API key for CLI authentication"
  value       = random_password.api_key.result
  sensitive   = true
}

output "deploy_key_public" {
  description = "Public key to add as a deploy key in your repo settings (enable write access). Null if you provided your own key."
  value       = local.deploy_public_key
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

output "post_apply_instructions" {
  description = "Post-apply next steps for the user"
  value       = local.deploy_public_key != null ? local.post_apply_instructions_with_key : local.post_apply_instructions_no_key
}
