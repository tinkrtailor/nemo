# Nemo

**Push a spec, get a clean PR.** Nemo is a convergent loop that pits AI models against each other until your code is right.

Claude implements. OpenAI reviews. If the reviewer finds issues, the implementer fixes them. The loop runs until the reviewer finds nothing wrong. Then you get a PR.

Different models have different blind spots. Claude never reviews its own work.

```bash
nemo start spec.md       # implement + PR
nemo ship spec.md        # implement + auto-merge
nemo harden spec.md      # harden the spec itself
```

## The loop

```
Round 1: Claude implements  -->  tests run  -->  OpenAI reviews  -->  3 issues
Round 2: Claude fixes       -->  tests run  -->  OpenAI reviews  -->  1 issue
Round 3: Claude fixes       -->  tests run  -->  OpenAI reviews  -->  clean
--> PR created
```

The exit condition is quality, not iteration count. If it takes 2 rounds, great. If it takes 12, that's fine too. The reviewer decides when the code is ready.

## How it works

```
Your machine                         Your cluster (k3s)
+----------+                         +-----------------------------+
| nemo CLI | -------- HTTPS -------> | API Server                  |
|          |                         | Loop Engine                 |
+----------+                         | Postgres                    |
                                     |                             |
                                     |  +----------+ +----------+  |
                                     |  |Implement | | Review   |  |
                                     |  |  (Claude)| | (OpenAI) |  |
                                     |  +----------+ +----------+  |
                                     +-----------------------------+
```

Your agents keep working when you close your laptop. The control plane runs on a VPS, dispatches agent jobs as K8s pods, and watches them until convergence.

## Quick start

```bash
# Deploy (once)
./build-images.sh --tag 0.1.0
cd terraform && op run --env-file=.env.1password -- terraform apply

# Set up your repo (once)
cd ~/your-repo
nemo init                    # generates nemo.toml
nemo auth                    # pushes Claude + OpenAI + SSH credentials

# Use it (daily)
nemo start spec.md           # PR appears when it converges
nemo status                  # watch progress
nemo logs <id>               # stream agent output
```

See [docs/deploy.md](docs/deploy.md) for full deployment guide.

## Three verbs

| Command | What happens | Result |
|---------|-------------|--------|
| `nemo harden spec.md` | OpenAI audits the spec, Claude revises. Loop until clean. | Hardened spec merged |
| `nemo start spec.md` | Claude implements, tests run, OpenAI reviews. Loop until clean. | PR created |
| `nemo ship spec.md` | Same as start + auto-merge when the loop converges | Code shipped |

Add `--harden` to `start` or `ship` to harden the spec before implementing.

## Configuration

```toml
# nemo.toml (repo root, checked in)
[repo]
name = "my-project"
default_branch = "main"

[models]
implementor = "claude-opus-4"    # who writes the code
reviewer = "gpt-5.4"            # who reviews it

[services.api]
path = "api/"
test = "cd api && cargo test"

[services.web]
path = "web/"
test = "cd web && npm test"
```

```toml
# ~/.nemo/config.toml (per engineer)
server_url = "https://nemo.yourdomain.com"
api_key = "your-api-key"

[identity]
name = "Alice"
email = "alice@example.com"
```

## Security model

Agent containers get open internet but **no secrets**. Model API auth and git push go through a localhost sidecar that injects credentials at the network level. Secrets never touch the agent filesystem.

- **Auth sidecar** proxies all model API calls and git pushes
- **Read-only reviewer** mounts the worktree read-only in review stage
- **Per-engineer credentials** scoped to each engineer's jobs
- **Egress logging** on all outbound traffic from agent pods

## Architecture

- **Control plane** (Rust): API server (axum) + loop engine, two k3s Deployments
- **CLI** (Rust): `nemo` binary on your machine
- **Agent jobs** (K8s pods): each stage runs as a separate pod
- **Auth sidecar** (Go): credential injection + egress proxy
- **Terraform**: provisions Hetzner VPS, k3s, Postgres, Traefik, cert-manager

```
control-plane/          Rust: API server + loop engine
  src/api/              REST endpoints (axum)
  src/loop_engine/      Convergent loop driver + reconciler
  src/state/            Postgres state store (sqlx)
  src/git/              Git operations (bare repo, worktrees)
  src/k8s/              Job builder + K8s client (kube-rs)
  src/config/           Three-layer config (cluster, repo, engineer)
  migrations/           SQL migrations

cli/                    Rust: nemo CLI
  src/commands/         start, ship, harden, status, logs, auth, init

images/
  base/                 Agent base image (Claude Code + OpenCode + tools)
  sidecar/              Auth sidecar (Go, ~10MB static binary)
  control-plane/        Control plane image (Rust, multi-stage build)

terraform/              Hetzner + k3s + Traefik + Postgres + control plane
.nemo/prompts/          Agent prompt templates
```

## Built with Nemo

Nemo was built through the exact process it automates. Three parallel implementation lanes, each hardened by cross-model adversarial review:

| Lane | What | Rounds | Findings |
|------|------|--------|----------|
| A | Core loop engine | 28 | 124 |
| B | Infrastructure (k8s, terraform) | 25 | 88 |
| C | Agent runtime (sidecar, entrypoint) | 21 | 107 |
| Integration | Cross-lane compatibility | 7 | 12 |
| **Total** | | **81 rounds** | **331 findings** |

331 production bugs caught by cross-model review before first deploy.

## Documentation

- [Deployment guide](docs/deploy.md) -- full setup with 1Password or env vars
- [Architecture](docs/architecture.md) -- system design and component interaction
- [Design document](docs/design.md) -- product decisions and rationale
- [Convergence data](docs/convergence-learnings.md) -- what we learned from 81 rounds

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). TL;DR: conventional commits, clippy clean, tests pass, no placeholders.

## License

[Apache 2.0](LICENSE)
