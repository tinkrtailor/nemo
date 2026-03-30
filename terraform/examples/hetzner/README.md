# Hetzner + Tailscale Example

Provisions a Hetzner VPS with hardened networking and installs Nemo via the reusable module.

## Networking model

- **SSH**: Open in firewall for terraform bootstrap. After setup, use Tailscale SSH (`ssh root@nemo`).
- **API (8080)**: Not exposed publicly. Engineers reach it at `http://nemo:8080` (MagicDNS) or `http://100.x.x.x:8080`.
- **HTTP/HTTPS (80/443)**: Public, only when `domain` is set. Traefik serves HTTPS with Let's Encrypt.
- **Tailscale UDP (41641)**: Public, required for WireGuard direct connections.

## Bootstrap flow

1. Hetzner creates the server with cloud-init (installs Tailscale, hardens SSH)
2. Terraform SSHes to the public IP to wait for Tailscale and capture the tailnet IPv4
3. The Nemo module provisions over the Tailscale IP (k3s, postgres, control plane)
4. After apply, the API is only reachable via the tailnet

## Hardening

- Hetzner firewall: no public 8080 (API), SSH open for bootstrap only
- fail2ban (SSH brute-force protection)
- unattended-upgrades (automatic security patches)
- Password auth disabled (key-only SSH + Tailscale SSH)

## Prerequisites

1. [Hetzner Cloud account](https://www.hetzner.com/cloud)
2. [Tailscale account](https://tailscale.com) with an [auth key](https://login.tailscale.com/admin/settings/keys) (ephemeral recommended)
3. Tailscale installed on your local machine (to reach the server after bootstrap)

## Usage

```bash
terraform init
terraform apply \
  -var="hetzner_api_token=$HETZNER_TOKEN" \
  -var="tailscale_auth_key=$TS_AUTHKEY" \
  -var='ssh_public_keys=["ssh-ed25519 AAAA..."]' \
  -var="git_repo_url=git@github.com:me/repo.git" \
  -var="git_host_token=$GITHUB_PAT"
```

After apply, the module outputs the Tailscale IP and post-apply instructions.

## Not using Tailscale?

This example uses Tailscale. If you use a different VPN or want public access, pass the appropriate IP to `server_ip` and manage firewall rules in your own terraform. The module itself is network-agnostic.
