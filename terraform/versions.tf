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

# Providers configured with kubeconfig that the module generates.
# The file won't exist until k3s_install runs, but terraform only
# evaluates the provider when it creates k8s resources (which all
# depend_on the kubeconfig null_resource in the module).
provider "kubernetes" {
  config_path = "${path.module}/modules/nemo/.state/kubeconfig.yaml"
}

provider "helm" {
  kubernetes {
    config_path = "${path.module}/modules/nemo/.state/kubeconfig.yaml"
  }
}
