# Example: Provision a Hetzner VPS and install Nemo on it
#
# Usage:
#   cd terraform/examples/hetzner
#   terraform init
#   terraform apply

terraform {
  required_version = ">= 1.5"

  required_providers {
    hcloud = {
      source  = "hetznercloud/hcloud"
      version = "~> 1.45"
    }
    kubernetes = {
      source  = "hashicorp/kubernetes"
      version = "~> 2.25"
    }
    helm = {
      source  = "hashicorp/helm"
      version = "~> 2.12"
    }
  }
}

provider "hcloud" {
  token = var.hetzner_api_token
}

provider "kubernetes" {
  config_path = "${path.module}/../../modules/nemo/.state/kubeconfig.yaml"
}

provider "helm" {
  kubernetes {
    config_path = "${path.module}/../../modules/nemo/.state/kubeconfig.yaml"
  }
}

# --- Hetzner server provisioning ---

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

  labels = { app = "nemo" }

  user_data = file("${path.module}/templates/cloud-init.yaml")
}

# --- Install Nemo on the server ---

module "nemo" {
  source = "../../modules/nemo"

  server_ip       = hcloud_server.nemo.ipv4_address
  ssh_private_key = file(pathexpand(var.ssh_private_key_path))
  ssh_user        = "root"

  git_repo_url         = var.git_repo_url
  git_host_token       = var.git_host_token
  repo_ssh_private_key = var.repo_ssh_private_key

  domain     = var.domain
  acme_email = var.acme_email

  control_plane_image = var.control_plane_image
  agent_base_image    = var.agent_base_image
  sidecar_image       = var.sidecar_image

  k3s_version          = var.k3s_version
  postgres_password    = var.postgres_password
  postgres_volume_size = var.postgres_volume_size
  ssh_known_hosts      = var.ssh_known_hosts

  image_pull_secret_dockerconfigjson = var.image_pull_secret_dockerconfigjson
}

# --- Outputs ---

output "server_ip" {
  description = "Public IP of the Hetzner server"
  value       = hcloud_server.nemo.ipv4_address
}

output "nemo_server_url" {
  description = "URL of the Nemo control plane"
  value       = module.nemo.server_url
}

output "nemo_api_key" {
  description = "API key for CLI authentication"
  value       = module.nemo.api_key
  sensitive   = true
}
