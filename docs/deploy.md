# Deploying Nemo

Nemo ships as a reusable Terraform module that installs on any Linux server with SSH access. You provision the server (Hetzner, AWS, DigitalOcean, bare metal) — the module handles k3s, Postgres, and the control plane.

## Quick start (module)

```hcl
module "nemo" {
  source = "github.com/tinkrtailor/nemo//terraform/modules/nemo"

  # Required: give me a server, I'll install nemo on it
  server_ip       = "203.0.113.10"          # or hcloud_server.x.ipv4_address, aws_instance.x.public_ip, etc.
  ssh_private_key = file("~/.ssh/id_ed25519")
  ssh_user        = "root"

  # Required: repo + credentials
  git_repo_url         = "git@github.com:me/myproject.git"
  git_host_token       = var.github_pat
  repo_ssh_private_key = var.deploy_key

  # Optional: domain + TLS (skip for IP-only)
  domain     = "nemo.mydomain.com"   # or null for http://IP
  acme_email = "me@mydomain.com"     # required if domain is set

  # Optional: images (defaults to latest public GHCR)
  control_plane_image = "ghcr.io/tinkrtailor/nemo-control-plane:0.1.0"
  agent_base_image    = "ghcr.io/tinkrtailor/nemo-agent-base:0.1.0"
  sidecar_image       = "ghcr.io/tinkrtailor/nemo-sidecar:0.1.0"
}

output "nemo_server_url" {
  value = module.nemo.server_url  # http://IP or https://domain
}

output "nemo_api_key" {
  value     = module.nemo.api_key
  sensitive = true
}
```

**Important:** The module needs `kubernetes` and `helm` providers configured in the caller. Point them at the kubeconfig the module generates:

```hcl
provider "kubernetes" {
  config_path = "${path.module}/<path-to-module>/.state/kubeconfig.yaml"
}

provider "helm" {
  kubernetes {
    config_path = "${path.module}/<path-to-module>/.state/kubeconfig.yaml"
  }
}
```

## Module inputs

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `server_ip` | yes | — | IP of the target server |
| `ssh_private_key` | yes | — | SSH private key content (not path) |
| `ssh_user` | no | `root` | SSH user |
| `git_repo_url` | yes | — | Git repo URL (SSH format) |
| `git_host_token` | yes | — | GitHub PAT for PR creation/merge |
| `repo_ssh_private_key` | yes | — | SSH deploy key (PEM format) |
| `domain` | no | `null` | Domain for TLS. null = HTTP on raw IP |
| `acme_email` | no | `null` | Let's Encrypt email. Required if domain is set |
| `control_plane_image` | no | `ghcr.io/tinkrtailor/nemo-control-plane:0.1.0` | Control plane image |
| `agent_base_image` | no | `ghcr.io/tinkrtailor/nemo-agent-base:0.1.0` | Agent base image |
| `sidecar_image` | no | `ghcr.io/tinkrtailor/nemo-sidecar:0.1.0` | Auth sidecar image |
| `k3s_version` | no | `v1.32.13+k3s1` | k3s version (v1.32+ required) |
| `postgres_password` | no | auto-generated | Postgres password |
| `postgres_volume_size` | no | `20` | Postgres volume size (Gi) |

## Module outputs

| Output | Description |
|--------|-------------|
| `server_url` | `http://IP` or `https://domain` |
| `api_key` | Generated API key for CLI auth (sensitive) |
| `kubeconfig_path` | Path to the kubeconfig file |
| `namespace_system` | `nemo-system` |
| `namespace_jobs` | `nemo-jobs` |

## Examples

### Hetzner Cloud

See `terraform/examples/hetzner/` for a complete example that provisions a Hetzner VPS and calls the module. Supports 1Password for secrets management.

```bash
cd terraform/examples/hetzner
terraform init
op run --env-file=.env.1password -- terraform apply   # with 1Password
# or
terraform apply                                        # with TF_VAR_* env vars
```

### Existing server (any provider)

See `terraform/examples/existing-server/` — bring your own IP.

```bash
cd terraform/examples/existing-server
terraform init
terraform apply -var="server_ip=203.0.113.10" -var="git_repo_url=git@github.com:me/repo.git" ...
```

### IP-only (no domain)

Set `domain = null` (the default). The control plane runs on HTTP at `http://IP`. No cert-manager, no TLS.

### With domain + TLS

Set `domain = "nemo.mydomain.com"` and `acme_email = "you@example.com"`. The module installs cert-manager, provisions a Let's Encrypt certificate, and configures Traefik with HTTPS.

## Prerequisites

- [Terraform](https://developer.hashicorp.com/terraform/install) >= 1.5
- [Docker](https://docs.docker.com/get-docker/) with buildx (for building images)
- A Linux server with SSH access (Ubuntu 22.04 recommended)
- GitHub PAT with repo + PR permissions
- SSH deploy key for your repo
- Optional: [1Password CLI](https://developer.1password.com/docs/cli) for secrets

## Build and push images

```bash
./build-images.sh --tag 0.1.0
```

Builds 3 images (control-plane, agent-base, sidecar), pushes to GHCR.

Options:
- `--no-push` — build locally, don't push
- `--only control-plane` — build a single image
- `--platform linux/arm64` — override platform (default: linux/amd64)

## Set up engineers

Each engineer runs once:

```bash
cd ~/your-monorepo
nemo init                    # generates nemo.toml
nemo auth                    # pushes credentials (Claude, OpenAI, SSH) to cluster
```

## Update (new images)

```bash
./build-images.sh --tag 0.2.0
terraform apply \
  -var="control_plane_image=ghcr.io/tinkrtailor/nemo-control-plane:0.2.0" \
  -var="agent_base_image=ghcr.io/tinkrtailor/nemo-agent-base:0.2.0" \
  -var="sidecar_image=ghcr.io/tinkrtailor/nemo-sidecar:0.2.0"
```

All three images must be updated together to avoid version skew.

## Legacy: root terraform

The `terraform/` root directory still contains a Hetzner-specific deployment that calls the module. This is the original deployment path and works identically to before:

```bash
cd terraform
op run --env-file=.env.1password -- terraform init
op run --env-file=.env.1password -- terraform apply
```

## Teardown

```bash
terraform destroy
```

If using Hetzner with a volume, the volume persists (Postgres data survives). Next `terraform apply` reattaches it.
