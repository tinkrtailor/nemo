# Nautiloop Local Dev Environment

Run a full nautiloop cluster on your machine using k3d. No Terraform, no Hetzner.

## Prerequisites

- [k3d](https://k3d.io/) >= 5.0
- [kubectl](https://kubernetes.io/docs/tasks/tools/)
- [Docker](https://docs.docker.com/get-docker/)
- [cargo](https://www.rust-lang.org/tools/install) (for building the control plane)
- `nemo` CLI on your PATH (`cargo install --path cli` from the repo root)

## Quick Start

```bash
# 1. Set required env vars (see setup.sh for the full list)
export NAUTILOOP_GIT_REPO_URL="git@github.com:your-org/your-repo.git"
export NAUTILOOP_GITHUB_TOKEN="ghp_..."
export NAUTILOOP_ANTHROPIC_KEY="sk-ant-..."

# 2. Bring up the cluster
./dev/setup.sh

# 3. Run a smoke test job
./dev/smoke-test.sh
```

## What the Smoke Test Does

Submits a minimal harden job using `dev/test-spec.md`, then streams logs until the
loop reaches HARDENED or FAILED. The test spec asks the agent to add a trivial
`hello_world()` function — it exists to exercise the full pipeline (dispatch,
sidecar, agent, review) without modifying anything meaningful.

## Resetting

```bash
./dev/teardown.sh   # Destroys the k3d cluster and registry
./dev/setup.sh      # Recreates from scratch
```

Postgres data and the bare repo live on ephemeral k3d node storage, so teardown
wipes everything.

## Rebuilding Images

```bash
./dev/build.sh                          # All images
./dev/build.sh --control-plane         # Only the control plane
./dev/build.sh --sidecar               # Only the sidecar
./dev/build.sh --agent-base            # Only the agent base

# Then redeploy:
kubectl rollout restart deployment/nautiloop-api-server deployment/nautiloop-loop-engine \
  -n nautiloop-system
```
