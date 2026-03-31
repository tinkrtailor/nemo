# Install k3s on the provided server and fetch kubeconfig.
# The module does NOT provision the server — it takes a server IP + SSH access.

resource "null_resource" "k3s_install" {
  triggers = {
    k3s_version = var.k3s_version
    server_ip   = var.server_ip
  }

  connection {
    type        = "ssh"
    host        = var.server_ip
    user        = var.ssh_user
    private_key = var.ssh_private_key
  }

  provisioner "remote-exec" {
    inline = [
      "cloud-init status --wait 2>/dev/null || true",
      "curl -sfL https://get.k3s.io | INSTALL_K3S_VERSION=${var.k3s_version} sh -s - server --tls-san ${var.server_ip}",
      "until kubectl get nodes 2>/dev/null | grep -q ' Ready'; do sleep 2; done",
      # Configure container log rotation
      "mkdir -p /etc/rancher/k3s",
      "cat > /etc/rancher/k3s/config.yaml <<EOF",
      "tls-san:",
      "  - ${var.server_ip}",
      "kubelet-arg:",
      "  - container-log-max-size=50Mi",
      "  - container-log-max-files=5",
      "EOF",
      "systemctl restart k3s",
      "until kubectl get nodes 2>/dev/null | grep -q ' Ready'; do sleep 2; done",
      # Wait for Traefik CRDs and deployment (k3s deploys AddOns asynchronously)
      "TRIES=0; until kubectl get crd ingressroutes.traefik.io 2>/dev/null || [ $TRIES -ge 60 ]; do sleep 2; TRIES=$((TRIES+1)); done",
      "kubectl get crd ingressroutes.traefik.io || { echo 'ERROR: Traefik CRDs not registered after 120s'; exit 1; }",
      "kubectl -n kube-system rollout status deployment/traefik --timeout=120s",
      # Pre-create hostPath directories with correct ownership for non-root pods (UID 1000)
      "mkdir -p /data/nemo-bare-repo /data/nemo-postgres /data/backups",
      "chown 1000:1000 /data/nemo-bare-repo",
    ]
  }
}

# Fetch kubeconfig from server via SSH
resource "null_resource" "kubeconfig" {
  depends_on = [null_resource.k3s_install]

  triggers = {
    server_ip = var.server_ip
  }

  provisioner "local-exec" {
    command = <<-EOT
      KUBECONFIG_OUT="${local.kubeconfig_path}"
      mkdir -p "$(dirname "$KUBECONFIG_OUT")"
      ssh -o StrictHostKeyChecking=accept-new \
        -o "UserKnownHostsFile=/dev/null" \
        -i "$SSH_KEY_FILE" \
        ${var.ssh_user}@${var.server_ip} \
        'cat /etc/rancher/k3s/k3s.yaml' | \
        sed '/server:/s/127.0.0.1/${var.server_ip}/' > "$KUBECONFIG_OUT.tmp"
      chmod 600 "$KUBECONFIG_OUT.tmp"
      mv "$KUBECONFIG_OUT.tmp" "$KUBECONFIG_OUT"
    EOT

    environment = {
      SSH_KEY_FILE = local.ssh_key_file
    }
  }
}

# Write SSH key to temp file for local-exec (ssh -i needs a file, not stdin)
resource "local_sensitive_file" "ssh_key" {
  content         = var.ssh_private_key
  filename        = "${path.module}/.state/ssh_key"
  file_permission = "0600"
}

# Generate deploy key if not provided
resource "tls_private_key" "deploy_key" {
  count     = var.repo_ssh_private_key == null ? 1 : 0
  algorithm = "ED25519"
}

# Generate random passwords
resource "random_password" "postgres" {
  length  = 32
  special = false
}

resource "random_password" "api_key" {
  length  = 64
  special = false
}

locals {
  deploy_private_key = var.repo_ssh_private_key != null ? var.repo_ssh_private_key : tls_private_key.deploy_key[0].private_key_openssh
  deploy_public_key  = var.repo_ssh_private_key != null ? null : tls_private_key.deploy_key[0].public_key_openssh
  postgres_password  = var.postgres_password != "" ? var.postgres_password : random_password.postgres.result
  kubeconfig_path    = var.kubeconfig_output_path != null ? var.kubeconfig_output_path : "${path.module}/.state/kubeconfig.yaml"
  ssh_key_file       = "${path.module}/.state/ssh_key"
  has_domain         = var.domain != null && var.domain != ""
  server_url         = local.has_domain ? "https://${var.domain}" : "http://${var.server_ip}:8080"

  post_apply_instructions_with_key = <<-EOT
    Nemo deployed at ${local.server_url}

    Next steps:
    1. Add this deploy key to your repo (Settings > Deploy keys, enable write access):
       ${local.deploy_public_key != null ? trimspace(local.deploy_public_key) : ""}
    2. Re-run terraform apply to sync the repo (the repo-init job will fetch with the new key)
    3. Install the CLI: cargo install --git https://github.com/tinkrtailor/nemo nemo-cli
    4. Configure: nemo init && nemo auth
  EOT

  post_apply_instructions_no_key = <<-EOT
    Nemo deployed at ${local.server_url}

    Next steps:
    1. Install the CLI: cargo install --git https://github.com/tinkrtailor/nemo nemo-cli
    2. Configure: nemo init && nemo auth
  EOT
}
