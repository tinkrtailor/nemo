# FR-43: Provision a Hetzner Cloud server

resource "hcloud_ssh_key" "nemo" {
  count      = length(var.ssh_public_keys)
  name       = "nemo-${count.index}"
  public_key = var.ssh_public_keys[count.index]
}

resource "hcloud_server" "nemo" {
  name        = "nemo-${var.server_location}"
  server_type = var.server_type
  location    = var.server_location
  image       = "ubuntu-22.04"
  ssh_keys    = hcloud_ssh_key.nemo[*].id

  labels = {
    "app" = "nemo"
  }

  user_data = templatefile("${path.module}/templates/cloud-init.yaml", {
    k3s_version = var.k3s_version
  })
}

# FR-44: Install k3s via remote-exec after cloud-init
resource "null_resource" "k3s_install" {
  depends_on = [hcloud_server.nemo]

  connection {
    type        = "ssh"
    host        = hcloud_server.nemo.ipv4_address
    user        = "root"
    private_key = file(pathexpand(var.ssh_private_key_path))
  }

  provisioner "remote-exec" {
    inline = [
      "cloud-init status --wait || true",
      "curl -sfL https://get.k3s.io | INSTALL_K3S_VERSION=${var.k3s_version} sh -s - server --disable traefik",
      "until kubectl get nodes 2>/dev/null | grep -q ' Ready'; do sleep 2; done",
      # FR-54: Configure container log rotation
      "mkdir -p /etc/rancher/k3s",
      "cat > /etc/rancher/k3s/config.yaml <<'EOF'",
      "kubelet-arg:",
      "  - container-log-max-size=50Mi",
      "  - container-log-max-files=5",
      "EOF",
      "systemctl restart k3s",
      "until kubectl get nodes 2>/dev/null | grep -q ' Ready'; do sleep 2; done",
    ]
  }
}

# Fetch kubeconfig from server
resource "null_resource" "kubeconfig" {
  depends_on = [null_resource.k3s_install]

  connection {
    type        = "ssh"
    host        = hcloud_server.nemo.ipv4_address
    user        = "root"
    private_key = file(pathexpand(var.ssh_private_key_path))
  }

  provisioner "local-exec" {
    command = <<-EOT
      ssh -o StrictHostKeyChecking=no root@${hcloud_server.nemo.ipv4_address} \
        'cat /etc/rancher/k3s/k3s.yaml' | \
        sed "s/127.0.0.1/${hcloud_server.nemo.ipv4_address}/" > ${path.module}/kubeconfig.yaml
    EOT
  }
}

# Generate random passwords
resource "random_password" "postgres" {
  length  = 32
  special = false
}

# FR-52c: Generate random API key
resource "random_password" "api_key" {
  length  = 64
  special = false
}

locals {
  postgres_password = var.postgres_password != "" ? var.postgres_password : random_password.postgres.result
  kubeconfig_path   = "${path.module}/kubeconfig.yaml"
}

# Configure K8s provider after k3s is ready
provider "kubernetes" {
  config_path = local.kubeconfig_path
}

provider "helm" {
  kubernetes {
    config_path = local.kubeconfig_path
  }
}
