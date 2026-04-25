# Deploying Nautiloop

Nautiloop ships as a reusable Terraform module that installs on any Linux server with SSH access. You provision the server (Hetzner, AWS, DigitalOcean, bare metal) — the module handles k3s, Postgres, and the control plane.

## What the module installs

- **k3s** — lightweight Kubernetes distribution
- **Postgres 16** — state store for loops, jobs, and events (Pod + PVC)
- **Control plane** — a single binary running the API server, loop engine, orchestrator judge, and web dashboard as in-process async tasks. Not separate deployments.
- **Agent base image** — the container image used for implement, test, and review job pods
- **Sidecar image** — the auth sidecar that proxies model API calls and git pushes, injecting credentials without exposing them to agent containers

## Quick start (minimal)

Four required variables. Everything else has sane defaults.

```hcl
module "nautiloop" {
  source = "github.com/tinkrtailor/nautiloop//terraform/modules/nautiloop"

  server_ip       = "203.0.113.10"
  ssh_private_key = file("~/.ssh/id_ed25519")
  git_repo_url    = "git@github.com:me/myproject.git"
  git_host_token  = var.github_pat
}

output "nautiloop_server_url" {
  value = module.nautiloop.server_url  # http://IP:8080
}

output "nautiloop_api_key" {
  value     = module.nautiloop.api_key
  sensitive = true
}

output "nautiloop_deploy_key_public" {
  value = module.nautiloop.deploy_key_public  # add to GitHub deploy keys
}
```

The module auto-generates a deploy key. After `terraform apply`, add the public key to your repo's deploy keys (with write access), then you're done.

The module is self-contained: no `kubernetes` or `helm` provider configuration needed. It provisions all k8s resources via SSH+kubectl on the server, so a single `terraform apply` works from a clean state.

## Full example (all options)

```hcl
module "nautiloop" {
  source = "github.com/tinkrtailor/nautiloop//terraform/modules/nautiloop"

  # Required: give me a server, I'll install nautiloop on it
  server_ip       = hcloud_server.x.ipv4_address  # or aws_instance, digitalocean_droplet, etc.
  ssh_private_key = file("~/.ssh/id_ed25519")
  ssh_user        = "root"

  # Required: repo + credentials
  git_repo_url   = "git@github.com:me/myproject.git"
  git_host_token = var.github_pat

  # Optional: deploy key (auto-generated ED25519 if null)
  repo_ssh_private_key = null

  # Optional: domain + TLS (skip for IP-only)
  domain     = "nautiloop.mydomain.com"   # or null for http://IP:8080
  acme_email = "me@mydomain.com"     # required if domain is set

  # Optional: images (defaults to latest public GHCR)
  control_plane_image = "ghcr.io/tinkrtailor/nautiloop-control-plane:0.7.15"
  agent_base_image    = "ghcr.io/tinkrtailor/nautiloop-agent-base:0.7.15"
  sidecar_image       = "ghcr.io/tinkrtailor/nautiloop-sidecar:0.7.15"
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
| `repo_ssh_private_key` | no | auto-generated | SSH deploy key. If null, generates ED25519 |
| `domain` | no | `null` | Domain for TLS. null = HTTP on raw IP:8080 |
| `acme_email` | no | `null` | Let's Encrypt email. Required if domain is set |
| `control_plane_image` | no | `ghcr.io/tinkrtailor/nautiloop-control-plane:0.7.15` | Control plane image |
| `agent_base_image` | no | `ghcr.io/tinkrtailor/nautiloop-agent-base:0.7.15` | Agent base image |
| `sidecar_image` | no | `ghcr.io/tinkrtailor/nautiloop-sidecar:0.7.15` | Auth sidecar image |
| `k3s_version` | no | `v1.32.13+k3s1` | k3s version (v1.32+ required) |
| `postgres_password` | no | auto-generated | Postgres password |
| `postgres_volume_size` | no | `20` | Postgres volume size (Gi) |
| `cache_volume_size` | no | `50` | Size of the shared `/cache` compiler cache PVC in GiB; used by sccache, ccache, npm, pnpm, yarn, bun, pip, turbo, go, and any tool configured via `[cache.env]` in nemo.toml. Deprecated alias: `cargo_cache_volume_size` (exists for one release cycle, then removed). |
| `ssh_known_hosts` | no | `""` | SSH known_hosts entries for the git remote. If unset, the module runs `ssh-keyscan` and manages the `nautiloop-ssh-known-hosts` ConfigMaps automatically. |

## Module outputs

All outputs are machine-readable via `terraform output -json`.

| Output | Description |
|--------|-------------|
| `server_url` | `http://IP:8080` or `https://domain` |
| `api_key` | Generated API key for CLI auth (sensitive) |
| `deploy_key_public` | Public key to add as repo deploy key. Null if you provided your own. |
| `post_apply_instructions` | Human-readable next steps |
| `kubeconfig_path` | Path to the kubeconfig file |
| `namespace_system` | `nautiloop-system` |
| `namespace_jobs` | `nautiloop-jobs` |

## Examples

### Hetzner Cloud + Tailscale (recommended)

See `terraform/examples/hetzner/` — hardened setup with Tailscale VPN.

- SSH open (key-only, fail2ban protected). Day-to-day: `ssh root@nautiloop` (Tailscale)
- API (8080) only via Tailscale — `http://nautiloop:8080` (MagicDNS)
- Hetzner firewall: no public 8080, SSH + Tailscale UDP + optional HTTPS
- unattended-upgrades, password auth disabled

```bash
cd terraform/examples/hetzner
terraform init
terraform apply \
  -var="hetzner_api_token=$HETZNER_TOKEN" \
  -var="tailscale_auth_key=$TS_AUTHKEY" \
  -var='ssh_public_keys=["ssh-ed25519 AAAA..."]' \
  -var="git_repo_url=git@github.com:me/repo.git" \
  -var="git_host_token=$GITHUB_PAT"
```

### Existing server (any provider)

See `terraform/examples/existing-server/` — bring your own IP. The module is network-agnostic: pass a Tailscale IP, WireGuard IP, or public IP.

```bash
cd terraform/examples/existing-server
terraform init
terraform apply \
  -var="server_ip=100.64.0.1" \
  -var="git_repo_url=git@github.com:me/repo.git" \
  -var="git_host_token=ghp_..."
```

### IP-only (no domain)

Set `domain = null` (the default). The control plane runs on HTTP at `http://IP:8080`. With Tailscale, this is `http://nautiloop:8080`. No cert-manager, no TLS.

### With domain + TLS

Set `domain = "nautiloop.mydomain.com"` and `acme_email = "you@example.com"`. The module installs cert-manager, provisions a Let's Encrypt certificate, and configures Traefik with HTTPS.

### Accessing the dashboard

The control plane serves a web dashboard at `/dashboard`. No separate deployment needed.

- **Default URL:** `https://<server-ip-or-hostname>/dashboard`
- **Hetzner example:** `https://<tailscale-ipv4>/dashboard` — already bound to tailnet-only by the terraform module.
- **Log in:** use the API key from the cluster:
  ```bash
  kubectl get secret nautiloop-api-key -o jsonpath='{.data.NAUTILOOP_API_KEY}' | base64 -d
  ```
  Engineer name is self-declared on login.
- **Security:** the dashboard is as private as the server. Do NOT expose to the public internet without fronting with oauth2-proxy or similar.

### Pod introspection RBAC

`nemo ps` and the `/pod-introspect/:id` endpoint require `pods/exec` permission on the `nautiloop-jobs` namespace. The terraform module provisions this RBAC by default. Operators installing manifests by hand need to grant `pods/exec` to the control plane's service account in the `nautiloop-jobs` namespace.

### Cache configuration examples

The pluggable cache mounts a shared PVC at `/cache` in implement and revise pods. Configure which tools use it via `[cache.env]` in `nemo.toml`.

**Default (Rust-only, sccache):**

```toml
[cache.env]
SCCACHE_DIR = "/cache/sccache"
RUSTC_WRAPPER = "sccache"
```

**Polyglot (Rust + TypeScript):**

```toml
[cache.env]
SCCACHE_DIR = "/cache/sccache"
RUSTC_WRAPPER = "sccache"
npm_config_cache = "/cache/npm"
PNPM_HOME = "/cache/pnpm"
```

**Disabled:**

```toml
[cache]
disabled = true
```

The examples above cover the most common configurations.

## Prerequisites

- [Terraform](https://developer.hashicorp.com/terraform/install) >= 1.5
- [Docker](https://docs.docker.com/get-docker/) with buildx (for building images)
- A Linux server with SSH access (Ubuntu 22.04 recommended)
- GitHub PAT with repo + PR permissions
- Optional: [1Password CLI](https://developer.1password.com/docs/cli) for secrets

## Build and push images

```bash
./build-images.sh --tag 0.1.1
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
  -var="control_plane_image=ghcr.io/tinkrtailor/nautiloop-control-plane:0.7.15" \
  -var="agent_base_image=ghcr.io/tinkrtailor/nautiloop-agent-base:0.7.15" \
  -var="sidecar_image=ghcr.io/tinkrtailor/nautiloop-sidecar:0.7.15"
```

All three images must be updated together to avoid version skew.

If you leave `ssh_known_hosts` unset, the module populates `nautiloop-ssh-known-hosts` from `ssh-keyscan` during apply. Older module versions could leave those ConfigMaps empty on upgrade. If you are recovering a cluster that was upgraded from an older release, patch both namespaces once, replacing `<git-host>` with your Git server host:

```bash
kubectl -n nautiloop-system create configmap nautiloop-ssh-known-hosts \
  --from-literal="known_hosts=$(ssh-keyscan <git-host>)" \
  --dry-run=client -o yaml | kubectl apply -f -

kubectl -n nautiloop-jobs create configmap nautiloop-ssh-known-hosts \
  --from-literal="known_hosts=$(ssh-keyscan <git-host>)" \
  --dry-run=client -o yaml | kubectl apply -f -
```

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
