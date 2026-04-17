# Shared Cargo Cache

## Overview

Install `sccache` in the agent image, mount a shared persistent cache volume on every agent pod, and point `RUSTC_WRAPPER=sccache` at it. First loop on a repo fills the cache. Every loop after that hits 90%+ cache and compiles a Rust workspace in minutes instead of tens of minutes.

Dogfood impact: the hot bottleneck that cost 1.5 hours of wall time on the first convergence attempt for `specs/health-json-body.md` (three 30-min `cargo` cold-compile timeouts) is fully resolved. Every subsequent loop — and every engineer's first loop on a warm cluster — becomes <5 min.

Scope: Rust. Other languages (Node, Python) have their own caching stories; this spec is solely about `sccache` for Rust.

## Baseline

Main at PR #129 merge.

Current state in `feat/local-dev-env` (not yet merged; basis for this spec):
- Agent image includes Rust toolchain (`cargo`, `rustc`, clippy) via `INCLUDE_RUST=true` build-arg. Image size ~2.5 GB.
- Each agent job gets a per-worktree subpath on the bare-repo PVC at `/work`. `target/` lives in `/work/target/` and persists across retries within the same worktree.
- No shared compilation cache across worktrees, loops, or engineers. Every `agent/<engineer>/<spec>-<hash>` branch starts with an empty `target/`.
- Claude sets `CARGO_HOME=/tmp/cargo-home`. `/tmp` is a pod-local EmptyDir, so the crates.io registry cache ALSO rebuilds per-pod. Even retries (`t1`→`t2`→`t3`) within the same job redownload crate tarballs.

Measured baseline on the 2026-04-17 dogfood run:
- Cold `cargo clippy --workspace` on the nautiloop workspace: ~25-28 min for the first complete compile pass.
- 4 overlapping Claude-spawned cargo invocations serialized on the target-dir lock → effective single-threaded compile.
- Three 30-min job deadlines consecutively exceeded → loop FAILED without ever producing a commit.

## Problem Statement

### Problem 1: Cold compile dwarfs every other cost in the loop

Model inference for an implement round: ~30-60s. Git operations: <1s. Cold compile of the workspace: **20-30 min**. That is 95%+ of implement-round wall time for any Rust-targeted repo. It also causes per-stage deadline exhaustion, which is a more serious failure mode than slowness: the whole loop loses state and the operator has to debug.

### Problem 2: Cache cost is paid once per worktree, not once per repo

Current architecture makes cache reuse scale with worktrees, not with repos. Ten loops on one repo = ten cold compiles. For a team hardening multiple specs in parallel, this is a catastrophic waste — each loop duplicates 90%+ of the compilation of its sibling loops because they all pull from the same `main`.

### Problem 3: CARGO_HOME is pod-local

Even with a warm `target/`, re-entering a worktree in a new pod (e.g., after the 30-min deadline kills the first pod and `t2` starts) re-downloads every crate tarball from crates.io. At 700+ crates for this workspace that's minutes of network time before rustc even starts.

### Problem 4: Convergence throughput is capped by compile wall time

An engineer waiting 30+ min per round will not iterate. The product promise — "hand it a draft, get a PR back" — requires sub-5-min cycle time to feel autonomous. Shared cache is the load-bearing change that gets us there.

## Functional Requirements

### FR-1: Install sccache in the agent image

**FR-1a.** Agent base Dockerfile installs `sccache` binary when `INCLUDE_RUST=true`. Version pinned (e.g., `0.8.2`), SHA256-verified, same pattern as the `opencode` binary install at lines 40-53.

**FR-1b.** Architecture handling: fetch the correct `sccache-v{ver}-{arch}-unknown-linux-musl.tar.gz` from the official `mozilla/sccache` GitHub release, verify SHA256, extract to `/usr/local/bin/sccache`, `chmod +x`.

**FR-1c.** At image build time the verification step runs `sccache --version` so a broken binary fails the build, not a pod.

### FR-2: Shared cache PVC

**FR-2a.** New PVC `nautiloop-cargo-cache` in the `nautiloop-jobs` namespace:

```yaml
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: nautiloop-cargo-cache
  namespace: nautiloop-jobs
spec:
  accessModes:
    - ReadWriteOnce           # dev k3d (single-node); see FR-6 for multi-node prod
  resources:
    requests:
      storage: 20Gi
  storageClassName: local-path # k3d default; prod override via helm values
```

**FR-2b.** `storage: 20Gi` is an upper bound; `sccache`'s own `SCCACHE_CACHE_SIZE` caps actual usage (see FR-3c) well under this. The extra headroom absorbs burst writes and fragmentation.

**FR-2c.** The PVC is provisioned once at cluster setup. `dev/setup.sh` and the production Terraform module both create it.

### FR-3: Mount and environment in agent pods

**FR-3a.** `build_volumes` in `control-plane/src/k8s/job_builder.rs` adds a new volume referencing the `nautiloop-cargo-cache` PVC, for implement/revise stages only (review/audit/test stages don't invoke `cargo`; no need to mount).

**FR-3b.** `build_agent_mounts` adds a mount for that volume at `/cache/sccache` (writable).

**FR-3c.** `build_agent_env_vars` sets the following env vars for implement/revise stages:

| Env var                | Value                                                  |
| ---------------------- | ------------------------------------------------------ |
| `RUSTC_WRAPPER`        | `sccache`                                              |
| `SCCACHE_DIR`          | `/cache/sccache`                                       |
| `SCCACHE_CACHE_SIZE`   | `15G`                                                  |
| `SCCACHE_IDLE_TIMEOUT` | `0`     (don't auto-exit the daemon inside short jobs) |

`SCCACHE_DIR` is the local filesystem backend. Switching to S3/Redis is FR-6.

**FR-3d.** `CARGO_HOME` stays at its current default (which Claude overrides to `/tmp/cargo-home`). sccache caches compiled-object outputs independently of the crates.io registry cache, so registry-redownload is a separate optimization (out of scope for this spec; tracked in Problem 3 for future work).

### FR-4: Cache hygiene

**FR-4a.** Nothing in this spec deletes cache contents proactively. `sccache` self-manages via `SCCACHE_CACHE_SIZE` (LRU eviction when over the limit).

**FR-4b.** Cache inspection: `nemo cache stats` new CLI command invokes `sccache --show-stats` via a one-shot job and returns the output. Hit rate, cache size, bytes in/out. Useful for verifying the cache is actually being hit.

**FR-4c.** Nuking the cache: `nemo cache reset` deletes all contents. Uses a one-shot job with the PVC mounted and runs `rm -rf /cache/sccache/*`. Requires interactive confirmation (`--yes` to skip). Expected to be rare — once at cluster setup, maybe after a toolchain bump.

### FR-5: Observability

**FR-5a.** When the implement stage emits its `NAUTILOOP_RESULT`, the entrypoint script ALSO emits a `SCCACHE_STATS:{...json...}` line captured from `sccache --show-stats --stats-format=json` (sccache supports this). The loop engine stores this alongside the round output for later analysis. Fields of interest: `cache_hits`, `cache_misses`, `cache_hit_rate`, `compile_time_ms`.

**FR-5b.** `nemo inspect` renders a new `cache_stats` summary per round when present: `"r2 implement: 94% cache hits (1,247/1,323 invocations, 38s compile time)"`.

**FR-5c.** The orchestrator judge (#128 when it lands) can read sccache stats as an input signal. Falling cache hit rate round-over-round signals the implementor is touching deep dependency graphs; a hint about that may help future rounds.

### FR-6: Production scale-out (explicitly deferred)

**Not implemented in this spec, but required to not paint into a corner:**

**FR-6a.** The PVC's `accessModes: ReadWriteOnce` works in k3d (single-node) and for small clusters with all agent pods pinned to one node. Production deployments with multi-node agent pools need either:
- `ReadWriteMany` storage class (NFS, EFS, Filestore, Ceph)
- OR sccache's S3/Redis backend (`SCCACHE_BUCKET`, `SCCACHE_REDIS`)

**FR-6b.** The Terraform module adds a variable `cargo_cache_backend: "local" | "s3" | "redis"` that toggles the mount/env shape. Default `local` for dev; `s3` for prod deployments. Implementation of the `s3` backend is a follow-up spec; the variable and the `[observability]`/`[cache]` config section structure are laid down here so that follow-up is a drop-in.

**FR-6c.** This is called out in Out of Scope below as well. Listing it here so the reviewer sees the architecture allows for it.

## Non-Functional Requirements

### NFR-1: Correctness

`sccache` with a local filesystem backend is a Mozilla-maintained, battle-tested tool. The local backend uses file locks for concurrent safety. We rely on upstream correctness. No custom cache logic is written.

### NFR-2: Cache coherence

sccache keys entries by: source hash, rustc version, crate metadata, compiler flags, environment variables. Two agent pods running the same rustc invocation will hit the same cache entry. Two pods running DIFFERENT rustc invocations (different flags, different source) will miss — correctly. No false sharing.

### NFR-3: Failure mode

If `/cache/sccache` is unmountable or read-only, sccache falls back to direct compilation (no caching). The stage does NOT fail — it just loses the speedup. A warning is logged.

### NFR-4: Security

The cache stores compiled-object files. They are NOT secrets. They ARE potentially a cross-engineer side channel (engineer A's compile outputs visible to engineer B's pods). This is acceptable for self-hosted deployments where all engineers are trusted with the codebase. Multi-tenant SaaS deployments would need per-tenant cache isolation, not addressed here.

### NFR-5: Dev vs. prod parity

Dev uses local-path PVC. Prod uses either RWX PVC or S3 (FR-6). Same env var contract either way. `nemo cache stats` / `reset` commands work in both modes.

### NFR-6: Tests

- **Unit** (`control-plane/src/k8s/job_builder.rs`): assert implement/revise pods get the cargo-cache volume/mount + env vars; audit/review/test pods do NOT.
- **Integration** (`control-plane/tests/sccache_mount.rs`): build a job spec, verify the pod spec contains the expected mount, PVC claim name, and env vars.
- **Manual** (documented in a new `docs/cache-verification.md` or inline in `dev/README.md`): first-loop measurement, second-loop measurement, hit-rate check via `nemo cache stats`.

## Acceptance Criteria

A reviewer can verify by:

1. **First loop fills the cache:** on a fresh cluster with an empty `nautiloop-cargo-cache` PVC, run `nemo harden specs/health-json-body.md` (or similar Rust-touching spec). First implement stage takes >10 min (baseline). After completion, `nemo cache stats` shows non-zero `cache_writes`.
2. **Second loop hits the cache:** start a second loop on the SAME or a different spec that touches the same dependency tree. The implement stage completes in <5 min. `nemo cache stats` shows `cache_hit_rate > 0.8` for round 1.
3. **Inspect visibility:** `nemo inspect <branch>` for a converged loop shows per-round cache-stats lines.
4. **Failure fallback:** manually make `/cache/sccache` read-only in a test pod. Loop still succeeds (slow); warning logged.
5. **No regression:** non-Rust-touching stages (review, audit, test) pass without the cache volume. Job spec inspection (`kubectl get job <review-job> -o yaml`) shows no `nautiloop-cargo-cache` volume.
6. **Hygiene:** `nemo cache reset --yes` empties the PVC. Subsequent `nemo cache stats` shows zeros. Next loop re-fills.

## Out of Scope

- **S3/Redis sccache backend for multi-node production.** Architected for via FR-6 but implementation is a follow-up spec. Most self-hosted deployments start on a single agent node anyway.
- **CARGO_HOME registry cache sharing.** Separate optimization; sccache doesn't help here. A sibling PVC mounted at `~/.cargo` would solve it, but it's additive to this spec, not load-bearing.
- **Node/npm cache** (same pattern but for `npm ci`).
- **Python pip cache** (same pattern again).
- **Pre-warming the cache in the image itself** (bake a `target/` + sccache cache into the agent image at build time). Image-size / staleness tradeoff. Possible later but mutually exclusive with the runtime-PVC approach; this spec picks PVC for cleaner dev ergonomics.
- **Cache eviction policy beyond sccache's built-in LRU.** No age-based sweep, no per-repo sharding. Revisit if real usage shows pathological eviction.
- **Rewriting Claude's behavior to not spawn 4 overlapping `cargo` invocations.** Separate prompt-engineering fix; orthogonal to caching. Noted as a sibling improvement, tracked outside this spec.

## Files Likely Touched

- `images/base/Dockerfile` — add sccache install stage (FR-1), gated by `INCLUDE_RUST`.
- `dev/k8s/04-storage.yaml` (or equivalent) — new PVC (FR-2).
- `dev/setup.sh` — ensure PVC exists; re-apply safe.
- `control-plane/src/k8s/job_builder.rs` — volume, mount, env vars for implement/revise (FR-3).
- `control-plane/src/types/api.rs` — extend per-round output or add `SccacheStats` field.
- `control-plane/src/loop_engine/driver.rs` — parse `SCCACHE_STATS:` line from pod logs (FR-5a).
- `images/base/nautiloop-agent-entry` — after implement/revise, emit `SCCACHE_STATS:` line.
- `cli/src/commands/cache.rs` — new `nemo cache stats` / `nemo cache reset` (FR-4b, FR-4c).
- `cli/src/main.rs` — wire the `cache` subcommand.
- `cli/src/commands/inspect.rs` — render cache-stats summary.
- `terraform/modules/nautiloop/` — add PVC + `cargo_cache_backend` variable (FR-6b).
- Tests per NFR-6.

## Baseline Branch

`main` at PR #129 merge.
