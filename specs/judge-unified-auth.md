# Judge Unified Auth

## Overview

Supersede the judge's bespoke `NAUTILOOP_JUDGE_API_KEY` / `nautiloop-judge-creds` auth path (introduced in spec #177) with the same auth model every other model caller in nautiloop already uses: the **auth sidecar**. The control-plane pods gain an `auth-sidecar` container (same image as agent pods use) and the judge calls Anthropic via `http://localhost:9090/anthropic/v1/messages` — byte-identical to how implement/revise/review agents call models.

No new credential path. No new secret type. No `DirectAnthropicClient`. Delete the three new concepts, reuse the one that works.

## Baseline

Main at PR #181 merge (v0.7.1).

Current (spec-described, partly-implemented) state of judge auth:
- Spec #177 introduced `NAUTILOOP_JUDGE_API_KEY` env var OR `nautiloop-judge-creds` mounted secret, read by a new `DirectAnthropicClient`.
- Terraform spec includes a `judge_api_key` variable creating `nautiloop-judge-creds` Secret (this is what v0.7.0 shipped broken; fixed in v0.7.1 but still carries the wrong architecture).
- `control-plane/src/loop_engine/judge.rs` has `SidecarJudgeClient::new(sidecar_base_url)` — which was correct! — but was deemed "not usable from the control plane" because the control plane doesn't currently run a sidecar alongside itself.

The real root cause: **the control-plane pod doesn't have a sidecar**, so the sidecar-based `JudgeModelClient` has nowhere to call. The solution introduced in #177 invented a new auth path to work around that. The correct solution is to give the control plane a sidecar.

How agent pods do it (the pattern to copy): every agent Job pod has an `auth-sidecar` native init container (`restartPolicy: Always`) built from `sidecar/Dockerfile`. It:
- Mounts `nautiloop-creds-<engineer>` Secret at `/secrets/model-credentials/`.
- Listens on `localhost:9090`, proxies `/anthropic/v1/...` → `https://api.anthropic.com/v1/...`.
- Reads the raw API key from `/secrets/model-credentials/anthropic` and injects it as `x-api-key` header.
- Egress is logged to stderr (sidecar logs).

**Current credential format** (verified): the sidecar reads raw API key strings from files at `/secrets/model-credentials/{anthropic,openai}` — see `sidecar/src/model_proxy.rs` constants `ANTHROPIC_CRED_PATH` and `OPENAI_CRED_PATH`. The sidecar does NOT currently handle OAuth bundles or token refresh. The Overview's reference to OAuth refresh describes a future capability, not current behavior. This spec uses the sidecar's CURRENT raw-API-key mechanism for the judge credential.

## Problem Statement

### Problem 1: Two parallel auth systems

Agents talk to Anthropic via the sidecar (API key injection, egress logging, common code path). The judge was spec'd to talk to Anthropic via a `DirectAnthropicClient` reading from a differently-shaped secret (`credentials.json` with embedded JSON), bypassing the sidecar entirely. Two paths, two secret formats, two egress-visibility modes. That's a flaw.

### Problem 2: Separate credential path is unnecessary

The product already provisions Anthropic API keys for agent pods (via `NAUTILOOP_ANTHROPIC_KEY` in dev, terraform in prod). The judge should use the same credential type and delivery mechanism (sidecar-mounted secret), not a separate `DirectAnthropicClient` with its own secret format (`credentials.json` wrapping). One credential type, one mount format, one code path.

### Problem 3: Direct client duplicates HTTP plumbing

The sidecar already handles injecting credentials, logging egress, and proxying to `api.anthropic.com`. A separate `DirectAnthropicClient` re-implements the HTTP client setup, header injection, and error handling. That's duplicated code with no benefit.

### Problem 4: No egress logging on judge calls

Agent model calls appear in the sidecar's egress log. Judge calls via the direct client would not. That's a compliance / audit gap that gets worse over time.

## Functional Requirements

### FR-1: Add auth-sidecar to control-plane pods

**FR-1a.** Both control-plane Deployments (`nautiloop-api-server` and `nautiloop-loop-engine`) add a native sidecar initContainer (`restartPolicy: Always`) running `nautiloop-sidecar:<version>` — same image the agent pods use. Specifically the loop-engine pod is what needs it (judge runs there); the api-server pod also gets it for consistency and future use (e.g., dashboard-side judge-like features).

**FR-1b.** The sidecar in control-plane pods serves the same API surface as in agent pods: `localhost:9090/anthropic/*`, `localhost:9090/openai/*`, health probe, egress logger. No feature flags, no mode switches, no "control-plane variant" of the sidecar. The image is identical.

**FR-1c.** Mount `nautiloop-judge-creds` secret at `/secrets/model-credentials/` in the control-plane sidecar. This secret contains the Anthropic API key for judge calls — SAME key format as `nautiloop-creds-<engineer>` (raw key string in `data.anthropic`) but cluster-scoped (not per-engineer), named to reflect its purpose.

**FR-1d.** `nautiloop-judge-creds` Secret format:
```yaml
apiVersion: v1
kind: Secret
metadata:
  name: nautiloop-judge-creds
  namespace: nautiloop-system
data:
  anthropic: <base64 of Anthropic API key string>
```

This mirrors the exact key format that agent pod secrets (`nautiloop-creds-<engineer>`) already use — the sidecar reads `/secrets/model-credentials/anthropic` as a raw API key string (see `sidecar/src/model_proxy.rs:ANTHROPIC_CRED_PATH`). No `credentials.json` wrapper, no OAuth bundle. The `openai` key is omitted since the judge is Claude-only.

**Note:** The previous spec (#177) used `data.credentials.json` containing `{"api_key": "..."}`. This spec changes to `data.anthropic` containing the raw key string, matching the sidecar's expected input format.

### FR-2: Delete the DirectAnthropicClient path

**FR-2a.** Remove `DirectAnthropicClient` (if shipped) from `control-plane/src/loop_engine/judge.rs`. Remove `NAUTILOOP_JUDGE_API_KEY` env-var handling. Remove the `/secrets/judge/credentials.json` mount path branch.

**FR-2b.** When `judge_enabled = true`, `build_loop_driver` unconditionally constructs `SidecarJudgeClient::new("http://localhost:9090")` and passes it to `ConvergentLoopDriver::with_judge()`. No credential resolution, no alternative client, no conditional construction based on credential availability. When `judge_enabled = false`, the judge is omitted entirely (existing `Option<OrchestratorJudge>` remains `None`).

**FR-2c.** Startup log simplifies to:
```
Orchestrator judge: enabled, model=<model-name>, via auth-sidecar at http://localhost:9090
```
OR (when `judge_enabled = false`):
```
Orchestrator judge: disabled
```
There is no third state ("enabled in config but creds missing"). If the sidecar can't reach Anthropic, judge calls fail at request time, and the existing graceful-degrade to heuristic fires (NFR-1 of spec #177).

### FR-3: Terraform module variable change

**FR-3a.** Rename module variable `judge_api_key` (spec #177, v0.7.0+) → `judge_anthropic_key`, typed as `string` (sensitive), containing the raw Anthropic API key string. This is the same credential type operators already provide via `NAUTILOOP_ANTHROPIC_KEY` for agent pods.

**FR-3b.** Hard break: remove `judge_api_key` entirely (no deprecation alias). Add a `validation` block on the module that checks if both `judge_api_key` and `judge_anthropic_key` are provided and errors:

```hcl
variable "judge_api_key" {
  type        = string
  default     = null
  sensitive   = true
  description = "REMOVED: use judge_anthropic_key instead. This variable exists only to produce a clear error."

  validation {
    condition     = var.judge_api_key == null
    error_message = "judge_api_key has been removed. Set judge_anthropic_key with a raw Anthropic API key instead."
  }
}
```

Rationale: a soft deprecation is infeasible because the secret shape changes (`data.anthropic` raw key vs. old `data.credentials.json` with embedded JSON). The sidecar cannot consume the old format, and maintaining the `DirectAnthropicClient` for a deprecation window contradicts the goal of this spec (single auth path). A hard break with a clear error message is the safest path.

**FR-3c.** For dev setup (`dev/setup.sh`): the script currently reads `NAUTILOOP_ANTHROPIC_KEY` env var to create per-engineer secrets (lines 123-135). ADD new logic to also create `nautiloop-judge-creds` from the same `NAUTILOOP_ANTHROPIC_KEY` value:

```bash
# Create judge credentials secret (same API key, judge-specific secret name)
kubectl create secret generic nautiloop-judge-creds \
  --namespace=nautiloop-system \
  --from-literal=anthropic="${NAUTILOOP_ANTHROPIC_KEY}" \
  --dry-run=client -o yaml | kubectl apply -f -
```

The script does NOT currently have `nemo auth --claude` integration. This spec does not add it — it reuses the existing `NAUTILOOP_ANTHROPIC_KEY` path. Zero extra operator config for dev.

**FR-3d.** For production: operators provide a dedicated Anthropic API key via `judge_anthropic_key` terraform variable. Terraform creates the `nautiloop-judge-creds` secret. Separate API key is recommended (attribution, rate-limit isolation) but not enforced — operators may reuse the same key used for agent pods.

### FR-4: Egress + observability

**FR-4a.** Judge calls now appear in the control-plane pod's sidecar egress log with host `api.anthropic.com`. Operators get one consistent place to inspect "what model calls did nautiloop make" across every pod type.

**FR-4b.** Grafana dashboards / metrics that track sidecar throughput automatically include judge traffic. No new meters.

**FR-4c.** Remove any judge-specific auth logging from `control-plane/src/` — everything is below the sidecar now.

### FR-5: Migration path

**FR-5a.** Operators on v0.7.0 or v0.7.1 with `judge_api_key` set:
1. `terraform apply` on the new version **fails with a clear error** from the `judge_api_key` validation block (FR-3b).
2. Operator removes `judge_api_key`, sets `judge_anthropic_key` to their raw Anthropic API key.
3. `terraform apply` succeeds: deletes the old `nautiloop-judge-creds` and recreates with the new shape (`data.anthropic`).
4. Control-plane deployment pod rollout picks up the new mount + sidecar.

**FR-5b.** Operators who never set `judge_api_key` (judge was effectively disabled): they set `judge_anthropic_key` for the first time, terraform creates the secret, deployments roll. Judge starts firing.

## Non-Functional Requirements

### NFR-1: No new Docker images

The existing `nautiloop-sidecar` image is used as-is. No new build target, no new tag.

### NFR-2: No new secret types

`nautiloop-judge-creds` continues to exist but its SHAPE changes (`anthropic` raw key instead of `credentials.json` with embedded JSON). This is a hard break (no deprecation period) — see FR-3b. No other new secret.

### NFR-3: Sidecar startup cost on control-plane

Each control-plane pod now takes ~3-5 seconds longer to start (sidecar init + ready probe). This is the typical case; the startup probe should use the same parameters as agent pods (`periodSeconds: 2`, `failureThreshold: 30` — 60s maximum timeout). Acceptable — pod startup is infrequent and control-plane is not in the hot request path. Steady-state CPU/memory overhead: ~20 MiB memory + negligible CPU (Go static binary).

### NFR-4: Graceful degrade unchanged

When the sidecar can't reach Anthropic (network issue, invalid API key, upstream outage), `SidecarJudgeClient::invoke()` returns an error. The existing judge-failure-falls-through-to-heuristic logic (spec #128 NFR-1) handles it. Loop keeps running; judge decisions for that invocation are absent.

### NFR-5: Tests

- **Unit** (`control-plane/src/loop_engine/mod.rs`): `build_loop_driver_with` constructs `SidecarJudgeClient` unconditionally when `judge_enabled = true`. This is where the existing builder tests live (lines 107-150); update them to remove `DirectAnthropicClient` usage and verify sidecar-only construction.
- **Unit** (`control-plane/src/loop_engine/driver.rs`): existing tests pass after removing the `DirectAnthropicClient` variant.
- **Integration** (`control-plane/tests/judge_sidecar.rs`): loop engine with a mock sidecar responding to `/anthropic/v1/messages` — judge invocations route through the mock.
- **Manual**: deploy with `judge_anthropic_key` set, observe judge decisions populating `judge_decisions` table, observe egress log entries.

## Acceptance Criteria

A reviewer can verify by:

1. **Control-plane pods have the sidecar**: `kubectl --context=... get pod <loop-engine-pod> -o yaml | grep -c 'name: auth-sidecar'` returns `1`.
2. **Judge uses sidecar**: trigger a judge invocation; sidecar egress log (`kubectl logs <pod> -c auth-sidecar`) shows `POST /anthropic/v1/messages` within the same second.
3. **API key path removed**: `NAUTILOOP_JUDGE_API_KEY` env var is not set on any control-plane pod. `grep -r DirectAnthropicClient control-plane/src/` returns nothing.
4. **Terraform rename**: `terraform apply` with `judge_anthropic_key = <api-key>` creates `nautiloop-judge-creds` with `data.anthropic` key. With `judge_api_key` set, `terraform apply` fails with a clear error directing the operator to use `judge_anthropic_key`.
5. **Dev setup auto-provisions**: `dev/setup.sh` on a fresh cluster (with `NAUTILOOP_ANTHROPIC_KEY` set) creates `nautiloop-judge-creds` from the same API key. Judge starts firing on the first loop without manual intervention.
6. **Sidecar proxies correctly**: judge model calls route through `localhost:9090/anthropic/v1/messages`, sidecar injects the API key from the mounted secret, and the call reaches `api.anthropic.com` successfully.

## Out of Scope

- **Per-engineer judge attribution** (have the judge call use the submitting engineer's OAuth bundle instead of a cluster-level bundle). Cleaner billing/rate-limit story, but more complex wiring; follow-up.
- **Judge running against OpenAI** (GPT-X as judge). Today judge is Claude-only; OpenAI support is a separate model-selection spec.
- **Sidecar sharing across control-plane replicas** (ambient sidecar). Current pattern: one sidecar per pod. Scale concern only at high replica counts.
- **Removing the sidecar from API-server pod** (if judge only runs in loop-engine). Optimization: save one container per api-server pod. Not worth the asymmetry; leave both symmetric.
- **Custom Claude models for judge vs. implementor**. `judge_model` in nemo.toml still controls which model the judge calls; that's unchanged.

## Files Likely Touched

- `control-plane/src/loop_engine/judge.rs` — delete `DirectAnthropicClient`, keep `SidecarJudgeClient` as the only impl.
- `control-plane/src/main.rs` — simplify the construction path to unconditionally use `SidecarJudgeClient::new("http://localhost:9090")`.
- `control-plane/src/config/mod.rs` — remove `NAUTILOOP_JUDGE_API_KEY` env handling.
- `dev/k8s/05-control-plane.yaml` (or equivalent) — add auth-sidecar initContainer to both deployments.
- `terraform/modules/nautiloop/k8s.tf` — rename variable, restructure the secret shape, add sidecar to deployments.
- `terraform/modules/nautiloop/variables.tf` — rename `judge_api_key` → `judge_anthropic_key`, add validation block on old variable to error.
- `dev/setup.sh` — auto-provision `nautiloop-judge-creds` from `NAUTILOOP_ANTHROPIC_KEY`.
- Tests per NFR-5.

## Baseline Branch

`main` at PR #181 (v0.7.1) merge.
