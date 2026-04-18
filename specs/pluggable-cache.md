# Shared Cache Volume

## Overview

Replace the per-backend PVC design with **one shared cache PVC** mounted at `/cache` on every implement/revise pod. Any caching tool the operator cares about — sccache, ccache, npm, pnpm, yarn, bun, pip, poetry, turbo, go build cache, maven `.m2`, gradle, whatever — writes into its own subdirectory under `/cache`. The control plane's job is narrow: mount `/cache` and set the env vars that tell each tool where its subdirectory lives.

One PVC. One mount. One terraform variable. Add a new tool = add one line in nemo.toml that sets its env var. No control-plane changes, no new PVCs, no new ACLs.

This supersedes the earlier pluggable-cache spec (merged as PR #148) which had a per-backend PVC architecture that was over-engineered. The operational surface was wrong: each new tool meant a new PVC, new terraform block, new control-plane enum entry. The right abstraction is a single writable directory and passthrough env-var config.

## Baseline

Main at PR #148 merge.

Current state (from #130 implemented):
- Single PVC `nautiloop-cargo-cache` (misnamed — sccache-specific in name but the pattern is general) mounted at `/cache/sccache` on implement/revise.
- Hardcoded env: `RUSTC_WRAPPER=sccache`, `SCCACHE_DIR=/cache/sccache`, `SCCACHE_CACHE_SIZE=15G`, `SCCACHE_IDLE_TIMEOUT=0`.
- Terraform variable `cargo_cache_volume_size`.

Net effect of this spec: rename the PVC to `nautiloop-cache`, mount at `/cache`, move sccache env into a generic `[cache.env]` table, preserve existing sccache behavior as the default. Replace the #148 terraform `cache_backends` map before it ships.

## Problem Statement

### Problem: one PVC pattern, infinite tool ecosystem

Every compile/build tool has its own cache directory convention:

- `sccache` → `SCCACHE_DIR`
- `ccache` → `CCACHE_DIR`
- `npm` → `NPM_CONFIG_CACHE`
- `pnpm` → `PNPM_STORE_PATH`
- `yarn` → `YARN_CACHE_FOLDER`
- `bun` → `BUN_INSTALL_CACHE_DIR`
- `pip` → `PIP_CACHE_DIR`
- `poetry` → `POETRY_CACHE_DIR`
- `uv` → `UV_CACHE_DIR`
- `turbo` → `TURBO_CACHE_DIR`
- `go` → `GOCACHE`, `GOMODCACHE`
- `gradle` → `GRADLE_USER_HOME`
- `maven` → `-Dmaven.repo.local=...`
- `cargo registry` → `CARGO_HOME`

These are all "give me a writable directory, I'll manage it." Nautiloop shouldn't encode each one as a special-case backend with its own PVC. It should provide the writable directory and let the operator set the env vars that map each tool to its subpath.

## Functional Requirements

### FR-1: One shared cache PVC

**FR-1a.** Terraform + dev manifests provision a **single** PVC named `nautiloop-cache` in `nautiloop-jobs` namespace. RWO. Default size: 50 GiB. Dev manifest keeps its current 20 GiB size — dev clusters don't need 50 GiB of cache, and changing the size would require deleting and recreating the PVC since local-path provisioner does not support resize. Terraform default remains 50 GiB for production clusters.

**FR-1b.** Previous PVC name `nautiloop-cargo-cache` is renamed to `nautiloop-cache`. The terraform variable is renamed `cargo_cache_volume_size` → `cache_volume_size` with a deprecation alias for one release. Note: Terraform cannot rename a PVC in-place — changing the PVC name produces a destroy-then-create plan. Migration path: use `terraform state mv` to rename the resource in Terraform state before applying, or accept cache data loss (one cold refill). See also NFR-1 and AC-5.

### FR-2: Mount at `/cache` on implement + revise

**FR-2a.** `build_agent_mounts` in `control-plane/src/k8s/job_builder.rs` mounts the `nautiloop-cache` PVC at `/cache` on implement/revise stages. Review/audit/test stages do NOT get the mount (unchanged from #130 scoping).

**FR-2b.** Mount is read-write. The agent pod (UID 1000) must be able to write all subdirectories. PVC's storage class must support RWO from that UID; dev k3d local-path already does.

### FR-3: Config-driven env vars

**FR-3a.** New section in nemo.toml:

```toml
[cache]
# If true, skip the /cache mount entirely and set no cache env vars.
# Useful for debugging or disabling caching for one cluster.
disabled = false

[cache.env]
# Every key here becomes an env var on implement/revise agent pods.
# Every value is passed through verbatim. Point each tool anywhere under /cache.
# The control plane does NOT validate tool binary presence or env name shape.

RUSTC_WRAPPER        = "sccache"
SCCACHE_DIR          = "/cache/sccache"
SCCACHE_CACHE_SIZE   = "15G"
SCCACHE_IDLE_TIMEOUT = "0"

NPM_CONFIG_CACHE      = "/cache/npm"
PNPM_STORE_PATH       = "/cache/pnpm"
YARN_CACHE_FOLDER     = "/cache/yarn"
BUN_INSTALL_CACHE_DIR = "/cache/bun"

PIP_CACHE_DIR    = "/cache/pip"
POETRY_CACHE_DIR = "/cache/poetry"
UV_CACHE_DIR     = "/cache/uv"

TURBO_CACHE_DIR = "/cache/turbo"

GOCACHE    = "/cache/go-build"
GOMODCACHE = "/cache/go-mod"

GRADLE_USER_HOME = "/cache/gradle"
```

**FR-3b.** Default: when the `[cache]` section is absent entirely from nemo.toml, the control plane injects the sccache defaults (`RUSTC_WRAPPER=sccache`, `SCCACHE_DIR=/cache/sccache`, `SCCACHE_CACHE_SIZE=15G`, `SCCACHE_IDLE_TIMEOUT=0`). Byte-identical to #130. If `[cache]` is present but `[cache.env]` is absent or empty, no cache env vars are injected — the sccache defaults only apply when the entire `[cache]` section is missing. To get sccache behavior with an explicit `[cache]` section, list the sccache env vars in `[cache.env]`.

> **Implementation note:** The `cache` field on `NautiloopConfig` must use `Option<CacheConfig>` (not `#[serde(default)]`) so that the absent-section case (`None` → inject sccache defaults) is distinguishable from the present-but-empty case (`Some` with empty `env` → no cache env vars). This diverges from the `#[serde(default)]` pattern used by other config sections in `mod.rs` — the distinction is intentional and required by the three-case semantics above.

**FR-3c.** No validation of env-var names or values. Typos are the operator's problem; a wrong env var just means that tool uses its default (usually `$HOME`-relative) path, missing the cache benefit — which shows up as a slow build, not a crash.

**FR-3d.** `[cache] disabled = true` skips both the `/cache` mount and ALL cache env vars (including the default sccache ones). Single flag for "run without caching."

**FR-3e.** Cache config is repo-level only. The `[cache]` section is read from the repo's nemo.toml; cluster and engineer configs do not contribute cache settings. If `[cache]` appears in cluster or engineer configs, it is **silently ignored** — the repo-level nemo.toml is the sole source for cache configuration. The `CacheConfig` field should only be present on the repo-layer config struct (`RepoConfig` in `config/repo.rs`), not on the cluster or engineer config structs, so serde never deserializes it from those layers. At merge time, the `Option<CacheConfig>` from `RepoConfig` is copied onto `NautiloopConfig` (the runtime config struct used by the job builder). If multi-layer cache config is needed in the future, env maps would merge with higher-priority layers winning per-key — but that is out of scope for this spec.

### FR-4: No validation of tool binary presence

**FR-4a.** The control plane does NOT check whether `sccache` / `ccache` / `pnpm` / etc. are installed in the agent image. If the operator sets `RUSTC_WRAPPER=sccache` and `sccache` isn't in the image, cargo will fail to spawn it — same as any misconfiguration. Loud failure, correct ownership: image contents are the image-builder's responsibility.

**FR-4b.** The agent base image ships sccache (behind the existing `INCLUDE_RUST` build-arg). Adding other tools is operator image customization — they extend the base image via their own Dockerfile `FROM`, or request the tool in a new build-arg via a separate PR.

### FR-5: Terraform module

**FR-5a.** `terraform/modules/nautiloop/variables.tf`:

```hcl
variable "cache_volume_size" {
  description = "Size of the shared /cache PVC in Gi. Used by any caching tool the operator configures via [cache.env] in nemo.toml."
  type        = number
  default     = 50
}

# Deprecated — keep one release for migration.
variable "cargo_cache_volume_size" {
  type        = number
  default     = null
  description = "DEPRECATED: use cache_volume_size. Will be removed in the next release."
}
```

**FR-5b.** `terraform/modules/nautiloop/k8s.tf` provisions exactly one PVC named `nautiloop-cache` with size `coalesce(var.cargo_cache_volume_size, var.cache_volume_size)`.

**FR-5c.** The #148 `cache_backends` design is superseded and will not be implemented. The `cache_backends` variable exists only in the earlier spec document — it was never implemented in terraform code. No code removal needed.

### FR-6: `nemo cache show` CLI command

**FR-6a.** New command `nemo cache show` prints the active cache configuration as seen by the control plane plus observed usage:

```
Cache volume: nautiloop-cache (50 GiB)
Disk usage:   2.1 GiB / 50 GiB (4%)

Active env vars (from control-plane config):
  RUSTC_WRAPPER        = sccache
  SCCACHE_DIR          = /cache/sccache
  SCCACHE_CACHE_SIZE   = 15G
  SCCACHE_IDLE_TIMEOUT = 0

Subdirectory sizes:
  /cache/sccache:    1.8 GiB
  /cache/npm:         340 MiB
```

PVC capacity ("50 GiB" above) is read from the PVC's `status.capacity` field via the Kubernetes API, reflecting the actual provisioned size rather than the requested size from terraform/config.

If no running agent pod with `/cache` is available for disk inspection:

```
Cache volume: nautiloop-cache (50 GiB)
Disk usage:   unavailable (no running pod)

Active env vars (from control-plane config):
  RUSTC_WRAPPER        = sccache
  ...
```

**FR-6b.** Backed by a new `GET /cache` endpoint (read-only, no new state). This follows the existing flat route pattern (`/start`, `/status`, `/logs/{id}`, etc.) rather than introducing a new `/dashboard` namespace. The endpoint returns:

1. The resolved cache env vars **as loaded by the control plane process** (i.e., the config that would be applied to the next job), or the sccache defaults if `[cache]` is absent from the loaded config. This is the config from `NAUTILOOP_CONFIG_PATH` / `/etc/nautiloop/nemo.toml`, not read live from the git repo. If the repo's nemo.toml has been updated but the control plane hasn't reloaded, the endpoint shows the stale config — this is expected and matches how other config-driven behavior works.
2. Disk usage via `du -sh /cache/*` executed on a running agent pod. **Pod selection:** list pods in `nautiloop-jobs` namespace with label `nautiloop.dev/stage` in (`implement`, `revise`) that are in `Running` phase, sorted by creation timestamp descending, and exec into the first one found. Only running pods accept `kubectl exec`; completed pods have exited containers and cannot be exec'd into. Since agent pods typically complete quickly, the window for a running pod is small — disk usage will frequently be `unavailable`. If no running pod is found (all pods completed/garbage-collected or none have run yet), the disk-usage section is omitted and the CLI displays `Disk usage: unavailable (no running pod)`. No ephemeral pod or debug container is spawned to inspect the PVC. If multiple running pods exist, the most recently created one is used (arbitrary but deterministic).
3. Per-subdirectory size breakdown from the same `du` output.

Hit-rate stats (e.g., sccache compile hit rate) are **descoped** from this spec. The log capture pipeline does not currently emit structured stats lines, and defining the format + parsing logic is non-trivial. Hit-rate display will be addressed in a follow-up spec. The example output in FR-6a is aspirational; the initial implementation omits the "Observed in recent loops" section and shows only config + disk usage.

**FR-6c.** Read-only. Does not modify anything. To change config, engineers still edit `nemo.toml` in the target repo and commit. This is deliberate: cache config is a repo-level decision that should be versioned.

**FR-6d.** Output is plain text by default; `--json` flag emits structured JSON for scripting.

## Non-Functional Requirements

### NFR-1: Backward compatibility

Operator on #130 with no nemo.toml `[cache]` section gets identical env vars and behavior after upgrade. Terraform accepts `cargo_cache_volume_size` for one release. PVC name changes from `nautiloop-cargo-cache` to `nautiloop-cache`. Terraform cannot rename a PVC in-place — a naive apply produces a destroy-then-create plan. Operators must either (a) run `terraform state mv` to remap the existing PVC to the new resource name before applying, or (b) accept cache data loss (one slow first loop to refill). Dev operators using `01-storage.yaml` can `kubectl delete pvc nautiloop-cargo-cache` and re-apply, accepting the cold refill.

### NFR-2: No backend enumeration in the control plane

Job_builder does NOT list known backends. It reads `[cache.env]`, iterates, and pushes env vars. Adding a new tool = adding a line in nemo.toml. No Rust change, no release.

### NFR-3: Tests

- **Unit** (`control-plane/src/k8s/job_builder.rs`): `[cache.env]` with N entries produces N `EnvVar` entries on implement/revise pods; zero on review/audit/test. These tests construct `JobBuildConfig` directly with resolved cache config — they test env-var/mount injection, not default resolution.
- **Unit**: `[cache] disabled = true` skips both mount and env vars (tested at `job_builder` level via `JobBuildConfig`).
- **Unit** (`control-plane/src/config/mod.rs` or `config/repo.rs`): absent `[cache]` section (`Option<CacheConfig>` is `None`) → sccache defaults are injected. This tests the default-injection logic at the config resolution layer, where `NautiloopConfig` or `JobBuildConfig` is constructed from the parsed config.
- **Unit**: `[cache]` present with empty or missing `[cache.env]` produces zero cache env vars (sccache defaults do NOT apply). Tested at the same config resolution layer.
- **Integration**: build a job with a custom `[cache.env] FOO = "/cache/foo"`, verify `FOO` env var lands in the pod spec.

## Acceptance Criteria

1. Default (no config change) → same behavior as #130. Sccache fills `/cache/sccache`, hit rate >80% on second run.
2. Add `NPM_CONFIG_CACHE = "/cache/npm"` to nemo.toml `[cache.env]`. On a TypeScript-touching spec, `npm install` inside the agent pod writes to `/cache/npm`; second run hits the cache and is faster.
3. Set `[cache] disabled = true`. Agent pods have no `/cache` mount and no cache env vars. Loops still work; slower cold builds.
4. Set `RUSTC_WRAPPER = "sccache"` without installing sccache in the image. Cargo fails to spawn sccache, loop fails at implement stage with a clear cargo error in the log. (Demonstrates FR-4a.)
5. Terraform upgrade from #130: existing `cargo_cache_volume_size = 20` still works, deprecation warning logged once. After running `terraform state mv` to remap the old PVC resource to the new name, `terraform plan` shows no destructive changes to the PVC.
6. `nemo cache show` prints volume size, disk usage (or "unavailable" if no recent pod), and active env vars from nemo.toml. `nemo cache show --json` emits structured output. Hit-rate stats are deferred to a follow-up spec.

## Implementation Notes

**FR-6 is a separable milestone.** The CLI command and dashboard endpoint (FR-6) are architecturally distinct from the core PVC/config work (FR-1 through FR-5). FR-6 can be implemented and shipped independently after the core changes land. Implementation plans should treat FR-1–FR-5 as the primary deliverable and FR-6 as a follow-on.

## Out of Scope

- **Installing cache tools in the agent image.** The base image ships sccache. Others are the operator's Dockerfile extension OR a separate build-arg PR.
- **Multi-node / RWX / S3 backends.** Single-node cluster assumption; multi-node prod uses sccache's S3 backend (separate spec) or accepts the RWO limitation.
- **Per-engineer, per-repo, per-worktree cache isolation.** One PVC, everyone shares. Same trust model as the existing shared worktree architecture.
- **Cache size accounting per-tool.** `du -sh /cache/*` is the answer. No nautiloop-managed quota per subdirectory.
- **Config sugar like `[cache.preset] = "rust"` or `"node"`.** The env-var list is short; a preset just moves the configuration to a different file. Skip until repeat-pattern emerges.
- **`nemo cache enable <tool>` / `disable <tool>`.** Sugar over editing nemo.toml. Bypasses the git discipline that keeps repo config versioned and reviewable. Skip — engineers edit nemo.toml and commit like any other config.
- **`nemo cache set-default` for cluster-wide defaults.** Mixes CLI with kubectl/terraform territory. Operators manage cluster defaults via the terraform module's ConfigMap, same as other cluster settings.
- **Deleting the #148 `[cache]` backend-map architecture from its spec file.** It's a documented design that won't ship; leaving it as a historical record is fine.

## Files Likely Touched

- `control-plane/src/config/repo.rs` — add `Option<CacheConfig>` to `RepoConfig` (serde source for repo-layer nemo.toml). `CacheConfig` struct contains `disabled: bool` and `env: HashMap<String, String>`.
- `control-plane/src/config/mod.rs` — add `Option<CacheConfig>` field to `NautiloopConfig` (runtime config, see FR-3b implementation note). Populated from `RepoConfig` during merge.
- `control-plane/src/k8s/job_builder.rs` — rename volume claim reference, change mount path to `/cache`, replace hardcoded sccache env with iterate-over-config. `JobBuildConfig` struct needs a cache config field so resolved cache env vars and disabled flag flow from `NautiloopConfig` → `JobBuildConfig` → `build_agent_env_vars()` / `build_agent_mounts()`.
- `control-plane/src/loop/driver.rs` — `job_build_config` method must populate the new cache config field on `JobBuildConfig` from `NautiloopConfig`.
- `dev/k8s/01-storage.yaml` — rename PVC.
- `terraform/modules/nautiloop/variables.tf` — rename variable, keep deprecated alias, remove #148 `cache_backends` map.
- `terraform/modules/nautiloop/k8s.tf` — rename PVC.
- `dev/setup.sh` — update any references.
- `cli/src/commands/cache.rs` — new `nemo cache show` command (FR-6).
- `cli/src/main.rs` — wire the subcommand.
- `control-plane/src/api/cache.rs` — new endpoint `GET /cache` that returns resolved env + disk usage.
- `docs/cache.md` — new doc: short explanation, full example of common env vars for Rust / Node (npm/pnpm/yarn/bun) / Python (pip/poetry/uv) / Go.
- Tests per NFR-3.

## Baseline Branch

`main` at PR #148 merge. This spec supersedes #148.
