# Pluggable Compilation Cache

## Overview

Generalize the sccache PVC (spec #130) into a pluggable cache layer that supports multiple backends — `sccache` (default for Rust), `ccache` (C/C++), language-native caches (npm, pip, maven, gradle), or `none` — configurable per repo in nemo.toml and provisioned end-to-end by the terraform module. Ships sccache-as-default for Rust out of the box; supports a common interface for adding more backends without touching the control plane.

Goal: operators running nautiloop against Go / Node / Python / Java / mixed repos can turn on the right cache for their codebase with one config line and one `terraform apply`, not a patchset to the control plane.

## Baseline

Main at PR #147 merge.

Current state (from #130 implementation):
- `nautiloop-cargo-cache` PVC (dev: 20Gi, terraform prod: `cargo_cache_volume_size` default 50Gi) mounted at `/cache/sccache` on implement/revise pods
- sccache binary baked into the agent image behind `INCLUDE_RUST=true` build-arg
- Env vars `RUSTC_WRAPPER=sccache`, `SCCACHE_DIR=/cache/sccache`, `SCCACHE_CACHE_SIZE=15G`, `SCCACHE_IDLE_TIMEOUT=0` hardcoded for implement/revise stages
- No other cache backends. No way to disable sccache without editing source.

What's working: cold Rust compile ~25min → warm ~2min (measured on nautiloop's own codebase).

What's not:
- sccache is hardcoded; a C++ target repo gets the variable overhead with no benefit
- Node / Python / Java repos get nothing
- No way to configure cache size, path, or backend per-repo
- Terraform variable is cargo-specific (`cargo_cache_volume_size`), leaking the abstraction
- No story for operators who want rustc-wrapper alternatives (`rust-analyzer`, Turbo, Nix-build-cache) — they'd fork the image

## Problem Statement

### Problem 1: Cache layer is mono-backend

The job builder hard-wires sccache env vars. A Rust repo that uses `rust-analyzer`'s disk cache, or prefers `ccache` for its embedded-C bits, or wants Turbo for a mixed TS+Rust monorepo, has no configuration surface. Changing the backend means editing Rust code in the control plane.

### Problem 2: Non-Rust repos get nothing

Most interesting value-add: Node (`npm ci`) and Python (`pip install`) are dominated by dependency install, which has no build-cache today. The same PVC pattern that works for sccache would work for `npm` cache at `~/.npm` and `pip` cache at `~/.cache/pip`. But the current architecture has no way to express that.

### Problem 3: Terraform surface leaks implementation

`cargo_cache_volume_size` in `variables.tf` locks the terraform API to one specific backend. An operator adopting nautiloop for a Go codebase sees a "cargo" variable and either disables it (losing the pattern's value) or learns to ignore the name.

### Problem 4: No path to add a backend without a release

Today, supporting a new cache backend = patch control-plane + patch agent image + patch terraform + cut a release. That's appropriate for runtime primitives but wrong for what's effectively configuration. Adding `ccache` support should be a nemo.toml stanza, not a PR.

## Functional Requirements

### FR-1: Cache backends enumerated in nemo.toml

**FR-1a.** New section `[cache]` in nemo.toml:

```toml
[cache]
# Enabled cache backends. Order matters — if multiple backends write to the same
# mount point, later entries win (but in practice each backend has its own path).
backends = ["sccache"]

# Per-backend configuration via [cache.<backend>] subsections:

[cache.sccache]
size = "15G"                  # SCCACHE_CACHE_SIZE
path = "/cache/sccache"       # where to mount the PVC; sets SCCACHE_DIR

[cache.ccache]
size = "10G"                  # CCACHE_MAXSIZE
path = "/cache/ccache"        # CCACHE_DIR
compiler_check = "content"    # CCACHE_COMPILERCHECK

[cache.npm]
path = "/cache/npm"           # npm config set cache <path>

[cache.pip]
path = "/cache/pip"           # PIP_CACHE_DIR

[cache.custom]
# Escape hatch: any env + mount combo the operator wants.
env = { RUST_TURBO_CACHE_DIR = "/cache/turbo" }
mounts = [{ pvc = "nautiloop-turbo-cache", path = "/cache/turbo" }]
```

**FR-1b.** Default `backends = ["sccache"]` if the section is missing (backward-compatible with #130). Empty list = no caching.

**FR-1c.** Validation at control-plane startup: unknown backend names produce a startup warning but don't fatal the server (forward-compat for future backends shipped in newer agent images).

### FR-2: Per-backend mount + env contract

**FR-2a.** Each backend declares a fixed contract in the control plane:

| Backend   | Default mount path    | Env vars set                                                              | Image binary required |
|-----------|------------------------|---------------------------------------------------------------------------|-----------------------|
| `sccache` | `/cache/sccache`       | `RUSTC_WRAPPER=sccache`, `SCCACHE_DIR`, `SCCACHE_CACHE_SIZE`, `SCCACHE_IDLE_TIMEOUT=0` | `sccache`             |
| `ccache`  | `/cache/ccache`        | `CCACHE_DIR`, `CCACHE_MAXSIZE`, `CCACHE_COMPILERCHECK`                    | `ccache`              |
| `npm`     | `/cache/npm`           | `NPM_CONFIG_CACHE`                                                        | `npm` (via node base) |
| `pip`     | `/cache/pip`           | `PIP_CACHE_DIR`                                                           | `pip` (via python)    |
| `custom`  | (per-config)           | (per-config)                                                              | (per-config)          |

**FR-2b.** The job builder reads the merged nemo.toml `[cache]` section and, for implement/revise stages, adds one volume mount + one env-var bundle per enabled backend.

**FR-2c.** Mount paths can be overridden per-backend in the config (`[cache.sccache] path = "/mnt/my-cache"`), but the ENV vars that reference the path are updated accordingly. Operator never has to sync two values.

**FR-2d.** All cache volumes are backed by **one shared PVC** per enabled backend, ReadWriteOnce, provisioned by terraform (FR-4). No per-loop, per-engineer, or per-worktree splitting.

### FR-3: Agent image: multi-backend binaries behind build-args

**FR-3a.** Each backend's binary installation is gated behind a build-arg in `images/base/Dockerfile`:

```dockerfile
ARG INCLUDE_SCCACHE=false   # default off; production slim
ARG INCLUDE_CCACHE=false
# npm / pip / etc. come with the node / python base layers, no separate toggle
```

**FR-3b.** Dev `dev/build.sh` enables all reasonable defaults for a dogfood experience (`INCLUDE_SCCACHE=true INCLUDE_CCACHE=true`). Production release workflow stays lean; operators opt in via their own image build if they need ccache etc.

**FR-3c.** A one-liner `nemo cache backends` CLI command lists which backends are available in the currently-deployed agent image (introspected via the image's existing manifest / labels), vs. which backends the nemo.toml has enabled. Surfaces configuration mismatches.

**FR-3d.** At pod start, the agent entrypoint verifies each enabled-in-config backend has a matching binary installed. Missing binary → `NAUTILOOP_ERROR: cache: backend 'ccache' enabled in config but binary not found in image` and the pod exits. Fails loud, fails early.

### FR-4: Terraform module: generic cache PVC provisioning

**FR-4a.** `terraform/modules/nautiloop/variables.tf` replaces `cargo_cache_volume_size` (leaky) with:

```hcl
variable "cache_backends" {
  description = "Map of cache backend name to volume configuration. Keys: sccache, ccache, npm, pip, custom."
  type = map(object({
    enabled = bool
    size_gi = number
  }))
  default = {
    sccache = { enabled = true,  size_gi = 50 }
    ccache  = { enabled = false, size_gi = 20 }
    npm     = { enabled = false, size_gi = 10 }
    pip     = { enabled = false, size_gi = 10 }
  }
}
```

**FR-4b.** The terraform module generates one PVC per `enabled = true` backend, named `nautiloop-<backend>-cache`, in the `nautiloop-jobs` namespace. RWO, storage class inherited from `var.storage_class`.

**FR-4c.** The existing `cargo_cache_volume_size` variable becomes an alias for `cache_backends.sccache.size_gi` for one release cycle, with a deprecation warning. Removed two releases later.

**FR-4d.** The control-plane deployment's configmap includes the resolved `[cache]` section so the running control plane's view matches terraform's.

### FR-5: Job builder: iterate enabled backends

**FR-5a.** `build_volumes` iterates over `ctx.config.cache.backends` and for each backend, adds a volume referencing `nautiloop-<backend>-cache` PVC.

**FR-5b.** `build_agent_mounts` iterates and adds the mount path (backend default OR config override).

**FR-5c.** `build_agent_env_vars` iterates and adds the backend's env-var bundle. Env var NAMES are fixed per backend (see FR-2a table); VALUES are from config.

**FR-5d.** Any backend the control plane doesn't recognize is skipped with a warning log (forward-compat, see FR-1c).

### FR-6: Observability

**FR-6a.** `nemo cache stats` (introduced in #130 FR-4b) gets per-backend output:

```
sccache   462M / 15G    78% hit rate last 100 invocations
ccache    disabled
npm       1.2G / 10G    (pack extractions, no hit-rate metric available)
pip       disabled
```

**FR-6b.** Per-backend stats surface in `nemo inspect` for each round (replaces #130 FR-5).

**FR-6c.** Per-round `SCCACHE_STATS:` emission (#130 FR-5a) becomes a per-backend loop: entrypoint queries each enabled backend's stats CLI after the stage's main command and emits one `<BACKEND>_STATS:` line per.

### FR-7: Sane defaults for common stacks

**FR-7a.** A `[repo]` hint in nemo.toml lets the operator declare primary language:

```toml
[repo]
primary_languages = ["rust", "typescript"]
```

**FR-7b.** When `[cache]` section is absent, the control plane infers defaults:

| Primary language in [repo] | Default backends enabled |
|----------------------------|---------------------------|
| `rust`                      | `sccache`                 |
| `typescript` / `javascript` | `npm`                     |
| `python`                    | `pip`                     |
| `c` / `cpp`                 | `ccache`                  |
| (mixed)                    | all matching, in union    |

**FR-7c.** Operators keep full control via explicit `[cache] backends = [...]`; the inference is only the zero-config path.

## Non-Functional Requirements

### NFR-1: Backward compatibility

An operator upgrading from #130 with no config changes gets the same behavior (sccache mounted, Rust caching active). The nemo.toml schema addition is additive; missing `[cache]` uses the #130 defaults.

### NFR-2: Forward compatibility

Adding a new backend is: (a) one entry in the FR-2a contract table, (b) one dockerfile stanza, (c) one terraform block. No schema migration, no API break.

### NFR-3: Security

Each cache PVC is read-write from the agent pod. Cross-engineer visibility = same trust model as the existing `nautiloop-cargo-cache` (NFR-4 of #130). Multi-tenant isolation is explicitly out of scope; revisit if/when the threat model demands.

### NFR-4: Performance

Mount overhead per backend: negligible (empty-dir-style mount from PVC). Env-var count increase: ≤5 per backend × ≤4 backends = ≤20 extra env vars, well under k8s limits.

### NFR-5: Tests

- **Unit** (`control-plane/src/k8s/job_builder.rs`): assert each backend adds the right volume + mount + env combo; assert unknown backends are skipped with a warning.
- **Integration** (`control-plane/tests/cache_backends.rs`): build a job with `backends = ["sccache", "npm"]`, verify pod spec has both mounts and both env-var sets.
- **Manual**: `nemo cache stats` on a live cluster shows per-backend sections correctly after loops have run.
- **Terraform**: `terraform plan` with the default `cache_backends` map shows one sccache PVC; changing `npm.enabled = true` adds an npm PVC.

## Acceptance Criteria

1. **Default path works**: fresh install, no `[cache]` section in nemo.toml. Rust loop converges; sccache hit rate >80% on second run.
2. **Multi-backend**: `backends = ["sccache", "npm"]` in nemo.toml. A mixed Rust+TS monorepo loop mounts both PVCs; `nemo cache stats` shows both.
3. **Disabled**: `backends = []`. Agent pod has zero cache mounts; `nemo cache stats` reports all disabled; loop completes (no caching, slower cold build, correct behavior).
4. **Custom backend**: `[cache.custom]` with user-supplied env + mounts. Pod spec reflects exactly those env/mounts.
5. **Missing binary**: enable `ccache` in config but run against an image built with `INCLUDE_CCACHE=false`. Pod fails at startup with a clear message.
6. **Terraform migration**: apply with default `cache_backends` map; kubectl shows exactly one PVC `nautiloop-sccache-cache`. Add `ccache.enabled = true`, apply, see a second PVC appear.
7. **Backward-compat**: cluster previously provisioned with `cargo_cache_volume_size = 20` in terraform. Upgrade to this spec's terraform — no PVC destroyed/recreated, alias variable warning shown once.

## Out of Scope

- **Shared cross-repo / cross-engineer caches.** Each nautiloop deployment has its own PVC per backend. Multi-tenant cache sharing (e.g., sccache S3 backend) is a follow-up once the pluggable architecture lands.
- **Cache eviction policies beyond each backend's built-in LRU.** No age-based sweep, no size-based aggressive eviction. Each backend self-manages.
- **Language-specific build-tool integration** (maven `.m2`, gradle `.gradle`, go module cache, cargo registry cache separately from sccache). Each is addable as a backend later; not in the first pass.
- **Prewarm at image build time** (bake a partial `target/` directory into the agent image). Considered in #130 and rejected; stay with runtime-PVC.
- **Cache invalidation on toolchain version bumps.** Backend's problem (sccache already handles rustc version; ccache handles gcc version). Out of nautiloop's concern.
- **Turbo / Nix build cache**: the `custom` backend (FR-1a) is the escape hatch for these. Promoting them to first-class backends can wait for demand.

## Files Likely Touched

- `control-plane/src/config/repo.rs` + `merged.rs` — new `[cache]` section parsing + merge, new `[repo] primary_languages` parsing + merge.
- `control-plane/src/config/mod.rs` — expose `CacheConfig` to `JobBuildConfig`.
- `control-plane/src/k8s/job_builder.rs` — iterate backends in `build_volumes`, `build_agent_mounts`, `build_agent_env_vars` instead of hardcoded sccache.
- `control-plane/src/api/handlers.rs` — expose `cache` section in `/dashboard/state` (optional, for dashboard #145 integration).
- `cli/src/commands/cache.rs` — extend `nemo cache stats` / `reset` to be backend-aware. Add `nemo cache backends` (FR-3c).
- `images/base/Dockerfile` — add `INCLUDE_CCACHE` build-arg and install block; keep existing `INCLUDE_SCCACHE` (rename from `INCLUDE_RUST` for clarity, alias old name).
- `images/base/nautiloop-agent-entry` — verify each enabled backend's binary at startup (FR-3d).
- `terraform/modules/nautiloop/variables.tf` — add `cache_backends` map, deprecate `cargo_cache_volume_size` alias.
- `terraform/modules/nautiloop/k8s.tf` — `for_each` over enabled backends, generate PVCs.
- `terraform/modules/nautiloop/outputs.tf` — expose `cache_pvcs` map for downstream visibility.
- `docs/cache-backends.md` — new doc, explains backend taxonomy + how to add a new one (+ an example of the `custom` escape hatch using Turbo).
- Tests per NFR-5.

## Baseline Branch

`main` at PR #147 merge.
