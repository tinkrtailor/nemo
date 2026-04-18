# Local Dev Quickstart

Run your own nautiloop on your laptop in ~10 minutes. No cloud, no billing, no VM. Everything runs in a k3d (Kubernetes-in-Docker) cluster on your machine.

Use this when you want to:
- Try nautiloop before committing to a cloud deployment
- Develop and test nautiloop itself (dogfooding)
- Converge specs offline, paying only your own Claude/OpenAI subscription

## Prerequisites

Install:

| Tool | Install | Check |
|------|---------|-------|
| Docker Desktop | https://docs.docker.com/get-docker/ | `docker ps` |
| k3d | `brew install k3d` | `k3d version` |
| kubectl | `brew install kubectl` | `kubectl version --client` |
| Rust + cargo | https://rustup.rs | `cargo --version` |
| GitHub CLI (optional, for nicer PR ops) | `brew install gh` | `gh auth status` |

Clone the nautiloop repo:

```bash
git clone git@github.com:tinkrtailor/nautiloop.git
cd nautiloop
```

Build and install the `nemo` CLI:

```bash
cargo install --path cli
nemo --version
```

## Environment

You need at least these env vars. Put them in your shell rc or export them in the session:

```bash
# REQUIRED
export NAUTILOOP_GIT_REPO_URL="git@github.com:YOUR-ORG/YOUR-REPO.git"   # target repo the loops work against
export NAUTILOOP_GITHUB_TOKEN="ghp_..."                                 # GitHub PAT with repo + PR permissions
export NAUTILOOP_SSH_PRIVATE_KEY_PATH="${HOME}/.ssh/id_ed25519"          # SSH key with push access to the target repo

# At least one of these — the agent needs a model provider
export NAUTILOOP_ANTHROPIC_KEY="sk-ant-..."     # Claude via Anthropic Platform
export NAUTILOOP_OPENAI_KEY="sk-..."            # OpenAI Platform

# OPTIONAL
export NAUTILOOP_ENGINEER="dev"                 # your engineer handle; lowercase, short
```

**Note**: you can target nautiloop at itself by setting `NAUTILOOP_GIT_REPO_URL` to the nautiloop repo. That's dogfooding — it works.

## One-shot setup

From the repo root:

```bash
./dev/setup.sh
```

This does everything:
1. Creates a k3d cluster named `nautiloop-dev` on port 18080
2. Builds the three images (control-plane, sidecar, agent-base) and imports them into the cluster
3. Applies Kubernetes manifests (namespaces, storage PVCs including the shared sccache cache, RBAC, Postgres, control-plane deployments, service)
4. Creates secrets from your env vars
5. Writes the `nemo.toml` ConfigMap
6. Runs the repo-init Job that clones your target repo into the bare-repo PVC

Expect it to take 3-5 minutes the first run (image builds). Subsequent runs are 30s (idempotent).

When it finishes, verify:

```bash
kubectl --context=k3d-nautiloop-dev -n nautiloop-system get pods
# nautiloop-api-server-XXX       1/1 Running
# nautiloop-loop-engine-XXX      1/1 Running
# nautiloop-postgres-XXX         1/1 Running

nemo status
# No active loops.
```

**Tip**: never run `kubectl config use-context k3d-nautiloop-dev`. Always pass `--context=k3d-nautiloop-dev` per command. This keeps your global kubectl context pointed at whatever real cluster you use for other work.

## Point `nemo` at the local control plane

Edit `~/.nemo/config.toml` (or run `nemo config`):

```toml
server_url = "http://localhost:18080"
engineer = "dev"
api_key = "dev-api-key-XXXXXXX"        # printed by dev/setup.sh; also in the nautiloop-api-key Secret
```

Get the API key:

```bash
kubectl --context=k3d-nautiloop-dev -n nautiloop-system get secret nautiloop-api-key \
  -o jsonpath='{.data.NAUTILOOP_API_KEY}' | base64 -d
```

Verify the CLI reaches the server:

```bash
nemo status                # should print "No active loops." not an error
```

## Push credentials to the cluster

The agents in the cluster need your model credentials. `nemo auth` pushes them from your local machine:

```bash
nemo auth --claude          # pushes Claude Code OAuth creds (needs Claude Code installed locally)
nemo auth --openai          # pushes OpenAI API key from env var
```

If `nemo auth --claude` warns about stale credentials, open Claude Code once (it refreshes its OAuth token), then re-run `nemo auth --claude`.

## Your first loop

Pick a spec in your target repo (must be a `.md` file on `main`):

```bash
nemo start specs/your-spec.md
# Started loop <uuid>
#   Branch: agent/dev/your-spec-<hash>
#   State:  PENDING

nemo approve <uuid>
# State: AWAITING_APPROVAL
# Implementation will start on next reconciliation tick.
```

Watch it:

```bash
nemo helm                   # k9s-style dashboard, live updates
# OR
nemo logs <uuid>            # SSE stream of persisted log events
nemo logs <uuid> --tail     # live stdout from the active pod
```

When it converges, a PR is opened against `main` in your target repo. The URL is in `nemo inspect <branch>` output and in the `spec_pr_url` of the `/inspect` endpoint.

## Common commands

```bash
nemo status                           # all your active loops
nemo start <spec>                     # submit a new loop (requires approve to run)
nemo ship <spec>                      # start + auto-merge PR once clean (requires [ship] allowed = true)
nemo harden <spec>                    # converge on the spec itself (fixes ambiguity, adds criteria)
nemo approve <id>                     # gate the agent at PENDING → dispatch
nemo cancel <id>                      # stop a running loop
nemo resume <id>                      # resume AWAITING_REAUTH or PAUSED
nemo inspect <branch>                 # detailed per-round verdicts and PR URL
nemo models                           # list authenticated providers
nemo config                           # show CLI config
```

## Troubleshooting

### `nemo status` errors with "connection refused"

The control plane isn't running. `kubectl --context=k3d-nautiloop-dev -n nautiloop-system get pods` should show `nautiloop-api-server` as `Running`. If it's `CrashLoopBackOff`, `kubectl logs` the pod and look at the first error. Most common: Postgres not ready yet — wait 30s and retry.

### Loop stuck in `AWAITING_REAUTH`

Your Claude credentials in the cluster are expired. Open Claude Code on your laptop (it refreshes the OAuth token), then:

```bash
nemo auth --claude
nemo resume <loop-id>
```

### Loop stuck in `PENDING` forever

The reconciler didn't tick yet. Normal state for a few seconds. If it persists more than a minute, check loop-engine logs:

```bash
kubectl --context=k3d-nautiloop-dev -n nautiloop-system logs deployment/nautiloop-loop-engine --tail=30
```

### Rust cold-compile takes forever

The first Rust-targeted loop fills the sccache (~20 min). Every loop after that on the same cluster hits ~80-99% cache and compiles in minutes. Verify the cache is being written:

```bash
kubectl --context=k3d-nautiloop-dev -n nautiloop-jobs exec \
  $(kubectl --context=k3d-nautiloop-dev -n nautiloop-jobs get pods -l nautiloop.dev/stage=implement --no-headers | head -1 | awk '{print $1}') \
  -c agent -- sccache --show-stats | head -10
```

### Claude in the pod can't commit / produces a fake SHA

The Rust toolchain is gated behind `INCLUDE_RUST=true` in the base Dockerfile. `dev/build.sh` sets this automatically. If you customized the build and Claude reports "no cargo binary", re-run `dev/build.sh --agent-base`.

### Tear down everything

```bash
k3d cluster delete nautiloop-dev
k3d registry delete k3d-nautiloop-registry
docker volume prune -f        # optional: clean up persisted data (sccache, Postgres, bare repo)
```

## What's next

- Drop a real spec into `specs/` and run `nemo start specs/your-spec.md` — watch convergence
- Open `nemo helm` and keep it visible while you work — live view of all your loops
- Authenticate OpenAI for cross-model review: `nemo auth --openai` (Claude implements, OpenAI reviews — the different-blind-spots model)

## Architecture in one paragraph

Your `nemo` CLI talks to the control plane (axum HTTP server) in `nautiloop-system`. The control plane schedules K8s Jobs in `nautiloop-jobs` namespace — one pod per stage (implement, test, review, audit, revise). Each pod has an agent container (Claude Code or opencode CLI) and an auth sidecar (proxies model API calls, never exposes secrets to the agent). Commits happen in a git worktree on a shared PVC; review findings drive the next round; PRs open via the GitHub API when review returns `clean: true`. The sccache PVC caches compiled rustc outputs across all agent pods so cold Rust compiles don't dominate the loop.
