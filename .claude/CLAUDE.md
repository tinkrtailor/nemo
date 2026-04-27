## Commit behavior (high priority)

- **Verify branch state before branching**: ALWAYS run `git status` and `git branch --show-current` immediately before `git checkout -b` or any branch-creating operation. Do not assume you are on main. Branch labels can shift between commands (hooks, scripts, prior `cd`s, worktrees). If the current branch is unexpected, stop and investigate — do NOT commit. Committing onto the wrong branch can contaminate an open PR and is not recoverable without force-push.
- **Branch before work**: Always create and push a branch before starting any feature/task/spec implementation (see `.claude/rules/branch-before-work.md`).
- **NEVER push to main**: All changes go through branches and PRs. Never commit or push directly to main, even for single-line changes.
- **Auto-commit on success**: After completing a task (tests pass, build succeeds), commit automatically without waiting to be asked (see `.claude/rules/auto-commit-on-success.md`).
- **Conventional commits required**: All commits must follow conventional commits format - enforced by hook (see `.claude/rules/conventional-commits.md`).

## Primary source before changing a contract (high priority)

Before changing how we shape requests to or parse responses from an upstream (HTTP API, CLI, database protocol, file format), find at least one **primary source** before writing code:

- the upstream's open-source client (most authoritative — it's what they actually ship),
- the official API spec / docs (current version, not last year's),
- an upstream issue tracker entry confirming behavior,
- or a live probe through the actual sidecar/proxy demonstrating the contract.

Do NOT infer from our own code comments, our own tests, or a third-party analysis — those are derivative and rot fastest at exactly the contracts they describe. Code comments routinely lag upstream changes by months. The v0.7.18 regression (`max_tokens` symptom moved from one field name to another) shipped because the fix was inferred from "OpenAI probably unified the wire format" instead of read from OpenAI's own Codex CLI source. v0.7.19's fix (verified from [`codex-rs/core/src/client.rs`](https://github.com/openai/codex/blob/main/codex-rs/core/src/client.rs) + a live probe + an upstream bug report) landed correct on the first try.

Pattern that works: **read upstream source first → probe live endpoint to confirm → write code third**. The probe is forensic, not the source of truth — it tells you what the endpoint accepts *today*, but the upstream client tells you what they intend the contract to be.

## Release skew (high priority)

- **`/health` `version` is the control-plane binary only.** The Nautiloop stack ships three independently-tagged images (control-plane, sidecar, agent-base). They MUST be deployed at the same tag. A control plane at vN paired with a sidecar at vN-1 silently misses fixes — this is what caused the v0.7.13/.14/.15 audit failures with `Unsupported parameter: max_tokens`: the sidecar's `max_tokens → max_output_tokens` rewrite (commit 108c426) was already in source but the running sidecar pod was older.
- **Always check `sidecar_image` and `agent_image` from `/health`.** Since v0.7.16, `GET /health` returns those fields. If any of the three differs from what you expect, halt the operation and resolve the skew before continuing — do not run `nemo harden`/`start` against a skewed cluster.
- **Release process must update all three together.** When cutting a release, bump the workspace `Cargo.toml` version AND every `terraform/**/variables.tf` default tag (control_plane_image, sidecar_image, agent_base_image) AND `docs/deploy.md`'s example `terraform apply` snippet. The `/release` skill does this automatically; if you do it by hand, miss any one of those five and you ship skew.
- **For local k3d dev, `dev/build.sh` builds and imports all three from the same checkout** — it cannot produce skew. The trap is remote/prod deploys where someone updates one image without the others. Check `/health` after every prod deploy.

## Rust development (high priority)

- **Workspace layout**: Cargo workspace with two crates: `control-plane/` (library + binary) and `cli/` (binary).
- **Clippy before commit**: Run `cargo clippy --workspace -- -D warnings` before every commit. Fix all warnings.
- **Tests must pass**: Run `cargo test --workspace` before every commit. Never commit with failing tests.
- **Error types**: Use `thiserror` for all error enums. No `unwrap()` in library code.
- **Serialization**: Use `serde` + `serde_json` for all data structures that cross boundaries (API, config, state).
- **Database**: `sqlx` with Postgres. Compile-time query checking where possible.
- **Kubernetes**: `kube-rs` for all k8s API interaction. Job templates defined in YAML, applied via `kube-rs` client.

## Project structure

```
control-plane/src/
  api/          # REST API (axum): job submission, status, logs
  loop/         # Convergent loop engine: dispatch -> wait -> evaluate -> loop/exit
  state/        # Postgres state machine (sqlx): PENDING -> IMPLEMENTING -> REVIEWING -> CONVERGED | FAILED
  git/          # Git operations: bare repo, worktree management, identity
  config/       # Config loading: nemo.toml (repo) + ~/.nemo/config.toml (engineer) + cluster
cli/src/        # nemo CLI: submit, status, logs, cancel, init, auth
terraform/      # Hetzner + k3s provisioning
images/         # Dockerfiles for agent job images (base + per-monorepo extension)
.nemo/prompts/  # Agent prompt templates for implement/review/harden stages
```

## Architecture decisions

- **Split control plane**: API server (axum, handles CLI requests) + loop engine (background task, drives convergent loops). Same binary, two async tasks.
- **Postgres not SQLite**: Multi-pod future, concurrent access from API + loop engine. Use sqlx with migrations in `control-plane/migrations/`.
- **Auth sidecar per job pod**: Agent containers get open internet but NO secrets. Model API auth + git push proxy through a localhost sidecar. Secrets never touch agent filesystem.
- **Headless agent execution**: `claude --print --output-format stream-json` for Claude. `opencode run --format json` for OpenAI. No interactive terminals.
- **Job output**: Branch tip SHA (not patches). Agent commits directly to worktree branch.

## Testing

- **Unit tests**: `cargo test --workspace`. Inline `#[cfg(test)]` modules.
- **Trait-based mocking**: Define traits for external boundaries (e.g., `JobDispatcher`, `GitOperations`, `ModelClient`). Implement mock versions in tests. No mocking frameworks.
- **Integration tests**: `tests/` directory in each crate. Use testcontainers for Postgres. Use `k8s-openapi` fixtures for kube-rs tests.

## Implementation behavior (high priority)

- **Search before implementing**: NEVER implement functionality without first searching the codebase thoroughly. Use multiple search strategies and subagents. Do NOT assume something is not implemented.
- **No placeholders**: Every implementation must be complete and production-ready. No TODOs, no stubs, no "implement later".
- **Subagent strategy**: Use subagents for expensive operations (search, read) to preserve main context. Limit build/test to single subagent to avoid backpressure.
- **Capture learnings**: Document general discoveries in `.claude/learnings.md` (persists across all work). Document spec-specific findings in impl-plan.md.

## Design System
Always read DESIGN.md before making any visual or UI decisions.
All font choices, colors, spacing, and aesthetic direction are defined there.
Do not deviate without explicit user approval.
In QA mode, flag any code that doesn't match DESIGN.md.

## Adding new enforced rules

We use a two-layer pattern:

1. **Guidance**: add/adjust a rule in `.claude/rules/*.md` (use `paths:` to scope it).
2. **Enforcement**: update `.claude/hooks/deny_misplaced_rules.sh` and (if needed) `.claude/settings.json` to deny tool calls with a clear reason that links to the relevant rule doc.

When denying, the hook must:

- Return `permissionDecision: "deny"` and a `permissionDecisionReason`
- Include the rule path (e.g., `.claude/rules/<rule>.md`) and an exact corrective action

Prefer:

- Lightweight global reminder in `.claude/CLAUDE.md`
- Detailed behavior in scoped `.claude/rules/...`
- Hooks only for hard constraints (locations, forbidden files/tools, required checks)
