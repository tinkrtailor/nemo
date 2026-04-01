# Example: Hetzner VPS + Tailscale + Nautiloop
#
# Single terraform apply — no kubernetes/helm provider needed.
# The module handles k8s resource creation via SSH+kubectl on the server.
#
# Provisions a Hetzner server with:
# - Hetzner firewall (SSH + Tailscale UDP for bootstrap, optional 80/443)
# - Tailscale for private access (API only reachable via tailnet)
# - Optional public HTTPS via domain + cert-manager
#
# Bootstrap flow:
# 1. Hetzner creates server with cloud-init (installs Tailscale, hardens SSH)
# 2. Terraform SSHes to public IP to wait for Tailscale and get the tailnet IP
# 3. Nautiloop module provisions over the Tailscale IP (not public)
# 4. After apply, API is only reachable via tailnet: http://nautiloop:8080
#
# SSH is open in the firewall for bootstrap provisioning. After Tailscale is up,
# engineers should use `ssh root@nautiloop` (Tailscale SSH) instead of the public IP.
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
  }
}

provider "hcloud" {
  token = var.hetzner_api_token
}

# --- Hetzner firewall ---
# SSH open for terraform bootstrap provisioning.
# No public 8080 — API only reachable via Tailscale.
# 80/443 only when domain is set (public HTTPS).

resource "hcloud_firewall" "nautiloop" {
  name = "nautiloop-${var.server_location}"

  # SSH (needed for terraform provisioning over public IP during bootstrap)
  rule {
    description = "SSH (bootstrap)"
    direction   = "in"
    protocol    = "tcp"
    port        = "22"
    source_ips  = ["0.0.0.0/0", "::/0"]
  }

  # HTTP (only if domain is set for public HTTPS)
  dynamic "rule" {
    for_each = var.domain != null && var.domain != "" ? [1] : []
    content {
      description = "HTTP (ACME + redirect)"
      direction   = "in"
      protocol    = "tcp"
      port        = "80"
      source_ips  = ["0.0.0.0/0", "::/0"]
    }
  }

  # HTTPS (only if domain is set)
  dynamic "rule" {
    for_each = var.domain != null && var.domain != "" ? [1] : []
    content {
      description = "HTTPS"
      direction   = "in"
      protocol    = "tcp"
      port        = "443"
      source_ips  = ["0.0.0.0/0", "::/0"]
    }
  }

  # Tailscale direct connections (WireGuard UDP)
  rule {
    description = "Tailscale"
    direction   = "in"
    protocol    = "udp"
    port        = "41641"
    source_ips  = ["0.0.0.0/0", "::/0"]
  }

  # Outbound (unrestricted)
  rule {
    description     = "Outbound TCP"
    direction       = "out"
    protocol        = "tcp"
    port            = "any"
    destination_ips = ["0.0.0.0/0", "::/0"]
  }

  rule {
    description     = "Outbound UDP"
    direction       = "out"
    protocol        = "udp"
    port            = "any"
    destination_ips = ["0.0.0.0/0", "::/0"]
  }

  rule {
    description     = "Outbound ICMP"
    direction       = "out"
    protocol        = "icmp"
    source_ips      = []
    destination_ips = ["0.0.0.0/0", "::/0"]
  }
}

# --- SSH key (for Hetzner initial cloud-init access) ---

resource "hcloud_ssh_key" "nautiloop" {
  count      = length(var.ssh_public_keys)
  name       = "nautiloop-${count.index}"
  public_key = var.ssh_public_keys[count.index]
}

# --- Server ---

resource "hcloud_server" "nautiloop" {
  name        = "nautiloop-${var.server_location}"
  server_type = var.server_type
  location    = var.server_location
  image       = "ubuntu-22.04"
  ssh_keys    = hcloud_ssh_key.nautiloop[*].id

  firewall_ids = [hcloud_firewall.nautiloop.id]

  labels = { app = "nautiloop" }

  user_data = templatefile("${path.module}/templates/cloud-init.yaml", {
    tailscale_auth_key = var.tailscale_auth_key
    hostname           = "nautiloop"
  })
}

# --- Wait for Tailscale, get IPv4 tailnet address ---

resource "null_resource" "tailscale_wait" {
  depends_on = [hcloud_server.nautiloop]

  connection {
    type        = "ssh"
    host        = hcloud_server.nautiloop.ipv4_address
    user        = "root"
    private_key = file(pathexpand(var.ssh_private_key_path))
  }

  provisioner "remote-exec" {
    inline = [
      "cloud-init status --wait || true",
      # Wait for Tailscale to get an IP (up to 120s)
      "TRIES=0; until tailscale status --json 2>/dev/null | jq -e '.Self.TailscaleIPs[0]' >/dev/null 2>&1 || [ $TRIES -ge 60 ]; do sleep 2; TRIES=$((TRIES+1)); done",
      # Extract IPv4 specifically (filter out IPv6)
      "tailscale status --json | jq -r '[.Self.TailscaleIPs[] | select(test(\"^[0-9]+\\\\.\"))] | .[0] // empty' > /tmp/tailscale_ip",
      # Validate we got an IP
      "IP=$(cat /tmp/tailscale_ip); [ -n \"$IP\" ] && echo \"Tailscale IPv4: $IP\" || { echo 'ERROR: Tailscale did not get an IPv4 address'; exit 1; }",
    ]
  }
}

data "external" "tailscale_ip" {
  depends_on = [null_resource.tailscale_wait]

  program = ["bash", "-c", <<-EOT
    IP=$(ssh -o StrictHostKeyChecking=accept-new \
      -o UserKnownHostsFile=/dev/null \
      -i ${pathexpand(var.ssh_private_key_path)} \
      root@${hcloud_server.nautiloop.ipv4_address} \
      'cat /tmp/tailscale_ip' 2>/dev/null)
    if [ -z "$IP" ] || [ "$IP" = "null" ]; then
      echo '{"error": "Tailscale IP not available"}' >&2
      exit 1
    fi
    echo "{\"ip\": \"$IP\"}"
  EOT
  ]
}

# --- Install Nautiloop on the server via Tailscale IP ---

module "nautiloop" {
  source = "../../modules/nautiloop"

  # SSH over Tailscale — not the public IP
  server_ip       = data.external.tailscale_ip.result["ip"]
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

output "public_ip" {
  description = "Public IP (HTTP/HTTPS only — API and daily SSH via Tailscale)"
  value       = hcloud_server.nautiloop.ipv4_address
}

output "tailscale_ip" {
  description = "Tailscale IP (use this for SSH and API access)"
  value       = data.external.tailscale_ip.result["ip"]
}

output "nautiloop_server_url" {
  description = "URL of the Nautiloop control plane"
  value       = module.nautiloop.server_url
}

output "ssh_command" {
  description = "SSH into the server via Tailscale MagicDNS"
  value       = "ssh root@nautiloop"
}

output "nautiloop_api_key" {
  description = "API key for CLI authentication"
  value       = module.nautiloop.api_key
  sensitive   = true
}

output "nautiloop_deploy_key_public" {
  description = "Public key to add as a deploy key (null if you provided your own)"
  value       = module.nautiloop.deploy_key_public
}

output "nautiloop_post_apply_instructions" {
  description = "Post-apply next steps"
  value       = module.nautiloop.post_apply_instructions
}
