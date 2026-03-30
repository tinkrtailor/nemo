# Deploying Nemo

## Prerequisites

- [1Password CLI](https://developer.1password.com/docs/cli) (`op`) for secrets management (or set `TF_VAR_*` env vars manually)
- [Terraform](https://developer.hashicorp.com/terraform/install) >= 1.5
- [Docker](https://docs.docker.com/get-docker/) with buildx
- A Hetzner Cloud account
- A domain with DNS you control
- GitHub PAT with repo + PR permissions
- SSH deploy key for your repo

## Option A: 1Password (recommended)

Create a "Nemo" vault in 1Password with these items:

| 1Password Item | Fields | Purpose |
|----------------|--------|---------|
| `hetzner-cloud` | `credential` | Hetzner Cloud API token |
| `nemo-domain` | `domain`, `email` | Control plane domain + ACME email |
| `nemo-repo` | `ssh_url` | Git repo URL (SSH format) |
| `github-pat` | `credential` | GitHub PAT for PR creation/merge |
| `nemo-deploy-key` | `private_key` | SSH deploy key for repo access |
| `github-registry` | `username`, `pat` | GHCR credentials for image push/pull |
| `ssh-public-key` | `public_key` | SSH public key for server access |

Then use `op run` to inject secrets:

```bash
cd terraform
op run --env-file=.env.1password -- terraform init
op run --env-file=.env.1password -- terraform apply
```

## Option B: Environment variables

Set `TF_VAR_*` variables directly:

```bash
export TF_VAR_hetzner_api_token="your-token"
export TF_VAR_domain="nemo.yourdomain.com"
export TF_VAR_acme_email="you@example.com"
export TF_VAR_git_repo_url="git@github.com:you/repo.git"
export TF_VAR_git_host_token="ghp_..."
export TF_VAR_repo_ssh_private_key="$(cat ~/.ssh/nemo_deploy_key)"
export TF_VAR_ssh_public_keys='["ssh-ed25519 AAAA..."]'

cd terraform
terraform init
terraform apply
```

## Build and push images

```bash
./build-images.sh --tag 0.1.0
```

Builds 3 images (control-plane, agent-base, sidecar), pushes to GHCR.

Options:
- `--no-push` — build locally, don't push
- `--only control-plane` — build a single image
- `--platform linux/arm64` — override platform (default: linux/amd64)

## Provision the cluster

```bash
cd terraform
op run --env-file=.env.1password -- terraform init
op run --env-file=.env.1password -- terraform apply
```

This provisions a Hetzner VPS, installs k3s with Traefik, deploys Postgres, the control plane (API + loop engine), and initializes the bare repo. Takes ~5 minutes on first run.

## Set up engineers

Each engineer runs once:

```bash
cd ~/your-monorepo
nemo init                    # generates nemo.toml
nemo auth                    # pushes credentials (Claude, OpenAI, SSH) to cluster
```

## Teardown (save money)

```bash
cd terraform
op run --env-file=.env.1password -- terraform destroy
```

Destroys the server but keeps the Hetzner volume (Postgres data persists). Next `terraform apply` reattaches the same volume, no data loss.

## Update (new images)

```bash
./build-images.sh --tag 0.2.0
cd terraform
op run --env-file=.env.1password -- terraform apply \
  -var="control_plane_image=ghcr.io/tinkrtailor/nemo-control-plane:0.2.0" \
  -var="agent_base_image=ghcr.io/tinkrtailor/nemo-agent-base:0.2.0" \
  -var="sidecar_image=ghcr.io/tinkrtailor/nemo-sidecar:0.2.0"
```

All three images must be updated together to avoid version skew.

## Bring your own Kubernetes

If you already have a k8s cluster, you don't need the Hetzner terraform. Deploy the control plane images directly:

1. Build images: `./build-images.sh --tag 0.1.0`
2. Create a Postgres database
3. Deploy the control plane (API server + loop engine) with the images
4. Point the CLI at your server: `server_url` in `~/.nemo/config.toml`

The terraform in this repo is a reference deployment, not a requirement.
