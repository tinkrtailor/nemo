# Module outputs — all machine-readable for `terraform output -json`

output "server_url" {
  description = "URL of the Nautiloop control plane (http://IP:8080 or https://domain)"
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
  value       = "nautiloop-system"
}

output "namespace_jobs" {
  description = "K8s namespace for agent jobs"
  value       = "nautiloop-jobs"
}

output "post_apply_instructions" {
  description = "Post-apply next steps for the user"
  value       = local.deploy_public_key != null ? local.post_apply_instructions_with_key : local.post_apply_instructions_no_key
}

# Copy-paste block that the operator runs inside their consumer repo to
# point the nemo CLI at this nautiloop. See `specs/per-repo-config.md` FR-16.
#
# This output is sensitive because it embeds the generated API key. Retrieve
# with: terraform output -raw nemo_setup_instructions
#
# The terraform module does NOT write to arbitrary filesystem paths outside
# its own state (FR-17) — we only print instructions for the operator to run.
output "nemo_setup_instructions" {
  description = "Copy-paste instructions to point your nemo CLI at this nautiloop (per-repo config)"
  value       = <<-EOT
    # 1. Add to <your-repo>/nemo.toml:
    [server]
    url = "${local.server_url}"

    # 2. From your repo root, create the credentials file:
    mkdir -p .nemo
    echo "${random_password.api_key.result}" > .nemo/credentials
    chmod 600 .nemo/credentials

    # 3. Make sure the credentials file is git-ignored (nemo init does this
    #    automatically, but if you already have a .gitignore):
    grep -qxF ".nemo/credentials" .gitignore 2>/dev/null || echo ".nemo/credentials" >> .gitignore
  EOT
  sensitive   = true
}
