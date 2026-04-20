# Nemo

**Push a spec, get a clean PR.** Nemo runs a convergent loop where AI models review each other's work until the code is right.

Claude implements. OpenAI reviews. Findings go back to Claude. Claude fixes. OpenAI reviews again. Repeat until clean. Different models have different blind spots. Claude never reviews its own work.

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

The exit condition is quality, not iteration count.

## How it works

`nemo` is the CLI. A **nautiloop** is the server environment where convergent loops run. You provision a nautiloop on any Linux server using the Terraform module, then `nemo` talks to it.

```
Your machine                         Nautiloop (k3s server)
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

Your agents keep working when you close your laptop.

## Deploy a nautiloop

The nautiloop Terraform module installs on any Linux server with SSH access. You provision the server, the module handles k3s, Postgres, and the control plane.

```hcl
module "nautiloop" {
  source = "github.com/tinkrtailor/nautiloop//terraform/modules/nautiloop"

  server_ip       = "100.64.0.1"                          # any server with SSH
  ssh_private_key = file("~/.ssh/id_ed25519")
  git_repo_url    = "git@github.com:you/your-repo.git"
  git_host_token  = var.github_pat
}
```

Four required variables. Single `terraform apply`. No extra provider configuration needed.

The module auto-generates a deploy key (or accepts yours). After apply, add the public key to your repo's deploy keys with write access.

See [docs/deploy.md](docs/deploy.md) for the full guide, examples, and all options.

## Set up your repo

```bash
nemo init                    # generates nemo.toml
nemo auth                    # pushes Claude + OpenAI + SSH credentials

nemo start spec.md           # PR appears when it converges
nemo status                  # watch progress
nemo logs <id>               # stream agent output
```

## Three verbs

| Command | What happens | Result |
|---------|-------------|--------|
| `nemo harden spec.md` | OpenAI audits the spec, Claude revises. Loop until clean. | Hardened spec merged |
| `nemo start spec.md` | Hardens the spec first, then Claude implements, tests run, OpenAI reviews. Loop until clean. | PR created |
| `nemo ship spec.md` | Same as start + auto-merge when the loop converges | Code shipped |

`nemo start` hardens by default (cheap on already-clean specs, high-value on soft ones). Add `--no-harden` to skip the harden phase.

## Watching loops

```bash
nemo status                  # list your running loops
nemo helm                    # K9s-style TUI with rounds table, diff pane, live logs
nemo logs <id>               # stream agent output (SSE)
nemo ps <id>                 # live process table inside the agent pod
nemo inspect <branch>        # full round history + per-round verdicts
```

Dashboard (web, Tailscale-native): `/dashboard` on the control plane. Card grid on mobile, loop detail with rounds table + token/cost + actions. See [docs/dashboard-setup.md](docs/dashboard-setup.md).

## Recovery

```bash
nemo approve <id>            # unblock a PENDING or AWAITING_APPROVAL loop
nemo resume <id>              # resume a PAUSED or AWAITING_REAUTH loop
nemo extend <id> --add 10     # bump max_rounds on a FAILED loop, resume where it stopped
nemo cancel <id>              # terminate a running loop
```

LLM-friendly CLI: `nemo help ai` prints a comprehensive primer an agent can read to operate the system; every command has `--json` output for scripting.

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
server_url = "http://nautiloop:8080"   # Tailscale hostname or IP
api_key = "your-api-key"
engineer = "alice"
name = "Alice"
email = "alice@example.com"
```

## Security model

Agent containers get open internet but **no direct access to secrets**. Model API auth and git push go through a localhost sidecar that injects credentials at the network level.

- **Auth sidecar** proxies model API calls and git pushes, injecting credentials without exposing them as env vars or files
- **Read-only reviewer** mounts the worktree read-only in review stage
- **Shared API key** (V1): all authenticated users have full access. Designed for single-tenant / small-team deployments. Per-engineer RBAC is planned for V2.
- **Session data**: implement/revise jobs mount Claude session data (`.claude/`) into the agent container for session continuity. This is internal tool state, not user secrets.
- **Egress logging** on all outbound traffic from agent pods
- **Tailscale recommended** for private API access (no public endpoints)

## Architecture

- **`nemo`** (Rust CLI): runs on your machine, talks to the nautiloop
- **Nautiloop** (k3s server): API server (axum) + loop engine, Postgres, Traefik
- **Agent jobs** (K8s pods): each stage runs as a separate pod with an auth sidecar
- **Auth sidecar** (Go): credential injection + git push proxy + egress logging
- **Terraform module**: `terraform/modules/nautiloop/` -- installs on any Linux server

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

terraform/
  modules/nautiloop/    Reusable module (k3s + control plane + Postgres)
  examples/hetzner/     Reference: Hetzner VPS + Tailscale + nautiloop

.nautiloop/prompts/          Agent prompt templates
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

- [Local dev quickstart](docs/local-dev-quickstart.md) — 10-min k3d setup, no cloud required
- [Deployment guide](docs/deploy.md) — production setup, terraform module reference, examples
- [Dashboard setup](docs/dashboard-setup.md) — Tailscale-native web dashboard
- [Architecture](docs/architecture.md) — system design and component interaction
- [Design document](docs/design.md) — product decisions and rationale
- [Convergence data](docs/convergence-learnings.md) — what we learned from 81 rounds

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). TL;DR: conventional commits, clippy clean, tests pass, no placeholders.

## License

[Apache 2.0](LICENSE)
