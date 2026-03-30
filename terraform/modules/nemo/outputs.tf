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
  value       = <<-EOT
    Nemo deployed at ${local.server_url}

    Next steps:
    ${local.deploy_public_key != null ? "1. Add this deploy key to your repo (Settings > Deploy keys, enable write access):\n   ${trimspace(local.deploy_public_key)}\n2" : "1"}. Install the CLI: cargo install --git https://github.com/tinkrtailor/nemo nemo-cli
    ${local.deploy_public_key != null ? "3" : "2"}. Configure: nemo init && nemo auth
  EOT
}
