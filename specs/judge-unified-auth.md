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

How agent pods do it (the pattern to copy): every agent Job pod has an `auth-sidecar` native init container (`restartPolicy: Always`) built from `images/sidecar/Dockerfile`. It:
- Mounts `nautiloop-creds-<engineer>` Secret at `/secrets/model-credentials/`.
- Listens on `localhost:9090`, proxies `/anthropic/v1/...` → `https://api.anthropic.com/v1/...`.
- Injects the engineer's Claude OAuth `Authorization: Bearer <token>`.
- Refreshes the OAuth token on expiry.
- Egress is logged to stderr (sidecar logs).

## Problem Statement

### Problem 1: Two parallel auth systems

Agents talk to Anthropic via the sidecar (OAuth bundles, auto-refresh, egress logging, common code path). The judge was spec'd to talk to Anthropic via a raw API key in a separate secret, bypassing all the above. Two paths, two credential types, two refresh strategies, two egress-visibility modes. That's a flaw.

### Problem 2: Raw API keys are a regression

The product's existing model is "operators push their Claude OAuth bundle (via `nemo auth --claude`), we refresh on their behalf." The API key path forces operators to provision a second credential type, with different rotation semantics, for a feature that should just work off the same credentials. Raw API keys aren't bad — but they're a DIFFERENT contract.

### Problem 3: Token refresh is re-implemented

The sidecar already handles OAuth token refresh correctly. A separate `DirectAnthropicClient` would need to re-implement the refresh logic, or refuse to refresh and fail 8 hours after deploy when the OAuth expires. Either way, duplicated code.

### Problem 4: No egress logging on judge calls

Agent model calls appear in the sidecar's egress log. Judge calls via the direct client would not. That's a compliance / audit gap that gets worse over time.

## Functional Requirements

### FR-1: Add auth-sidecar to control-plane pods

**FR-1a.** Both control-plane Deployments (`nautiloop-api-server` and `nautiloop-loop-engine`) add a native sidecar initContainer (`restartPolicy: Always`) running `nautiloop-sidecar:<version>` — same image the agent pods use. Specifically the loop-engine pod is what needs it (judge runs there); the api-server pod also gets it for consistency and future use (e.g., dashboard-side judge-like features).

**FR-1b.** The sidecar in control-plane pods serves the same API surface as in agent pods: `localhost:9090/anthropic/*`, `localhost:9090/openai/*`, health probe, egress logger. No feature flags, no mode switches, no "control-plane variant" of the sidecar. The image is identical.

**FR-1c.** Mount `nautiloop-judge-creds` secret at `/secrets/model-credentials/` in the control-plane sidecar. This secret contains the Claude OAuth bundle to use for judge calls — SAME shape as `nautiloop-creds-<engineer>` but cluster-scoped (not per-engineer), named to reflect its purpose.

**FR-1d.** `nautiloop-judge-creds` Secret format:
```yaml
apiVersion: v1
kind: Secret
metadata:
  name: nautiloop-judge-creds
  namespace: nautiloop-system
data:
  claude: <base64 of Claude OAuth bundle JSON, matching ~/.claude/.credentials.json shape>
```

No `api_key` field. No nested `credentials.json` wrapper. Just the claude bundle, mirroring what agent pods already consume.

### FR-2: Delete the DirectAnthropicClient path

**FR-2a.** Remove `DirectAnthropicClient` (if shipped) from `control-plane/src/loop_engine/judge.rs`. Remove `NAUTILOOP_JUDGE_API_KEY` env-var handling. Remove the `/secrets/judge/credentials.json` mount path branch.

**FR-2b.** The `with_judge` constructor in `ConvergentLoopDriver` unconditionally wires `SidecarJudgeClient::new("http://localhost:9090")`. No alternative clients, no conditional construction.

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

**FR-3a.** Rename module variable `judge_api_key` (spec #177, v0.7.0+) → `judge_claude_credentials`, typed as `string` (sensitive), containing the raw Claude OAuth bundle JSON (same format `nemo auth --claude` pushes).

**FR-3b.** Keep `judge_api_key` as a deprecation alias for one release — when set, emit a terraform warning: `"judge_api_key is deprecated; use judge_claude_credentials with a Claude OAuth bundle"`. After this one release, remove.

**FR-3c.** For dev setup (`dev/setup.sh`): the script already has `nemo auth --claude` logic and stores the bundle. Extend it to ALSO create `nautiloop-judge-creds` from the same bundle on first run. Zero extra operator config for dev.

**FR-3d.** For production: operators generate a dedicated Claude account (or reuse one) and push its OAuth bundle into `judge_claude_credentials`. Terraform creates the secret. Separate account is recommended (attribution, rate-limit isolation) but not enforced.

### FR-4: Egress + observability

**FR-4a.** Judge calls now appear in the control-plane pod's sidecar egress log with host `api.anthropic.com`. Operators get one consistent place to inspect "what model calls did nautiloop make" across every pod type.

**FR-4b.** Grafana dashboards / metrics that track sidecar throughput automatically include judge traffic. No new meters.

**FR-4c.** Remove any judge-specific auth logging from `control-plane/src/` — everything is below the sidecar now.

### FR-5: Migration path

**FR-5a.** Operators on v0.7.0 or v0.7.1 with `judge_api_key` set:
1. `terraform apply` on the new version emits a deprecation warning.
2. They set `judge_claude_credentials` to the OAuth bundle, unset `judge_api_key`.
3. Next `terraform apply` deletes the old `nautiloop-judge-creds` and recreates with the new shape.
4. Control-plane deployment pod rollout picks up the new mount.

**FR-5b.** Operators who never set `judge_api_key` (judge was effectively disabled): they set `judge_claude_credentials` for the first time, terraform creates the secret, deployments roll. Judge starts firing.

## Non-Functional Requirements

### NFR-1: No new Docker images

The existing `nautiloop-sidecar` image is used as-is. No new build target, no new tag.

### NFR-2: No new secret types

`nautiloop-judge-creds` continues to exist but its SHAPE changes (`claude` key instead of `api_key` key). Migration is a one-release deprecation. No other new secret.

### NFR-3: Sidecar startup cost on control-plane

Each control-plane pod now takes ~3-5 seconds longer to start (sidecar init + ready probe). Acceptable — pod startup is infrequent. Steady-state CPU/memory overhead: ~20 MiB memory + negligible CPU (Go static binary).

### NFR-4: Graceful degrade unchanged

When the sidecar can't reach Anthropic (network, expired OAuth the sidecar can't refresh, upstream outage), `SidecarJudgeClient::invoke()` returns an error. The existing judge-failure-falls-through-to-heuristic logic (spec #128 NFR-1) handles it. Loop keeps running; judge decisions for that invocation are absent.

### NFR-5: Tests

- **Unit** (`control-plane/src/main.rs`): loop-engine startup constructs `SidecarJudgeClient` unconditionally when `judge_enabled = true`.
- **Unit** (`control-plane/src/loop_engine/driver.rs`): existing tests pass after removing the `DirectAnthropicClient` variant.
- **Integration** (`control-plane/tests/judge_sidecar.rs`): loop engine with a mock sidecar responding to `/anthropic/v1/messages` — judge invocations route through the mock.
- **Manual**: deploy with `judge_claude_credentials` set, observe judge decisions populating `judge_decisions` table, observe egress log entries.

## Acceptance Criteria

A reviewer can verify by:

1. **Control-plane pods have the sidecar**: `kubectl --context=... get pod <loop-engine-pod> -o yaml | grep -c 'name: auth-sidecar'` returns `1`.
2. **Judge uses sidecar**: trigger a judge invocation; sidecar egress log (`kubectl logs <pod> -c auth-sidecar`) shows `POST /anthropic/v1/messages` within the same second.
3. **API key path removed**: `NAUTILOOP_JUDGE_API_KEY` env var is not set on any control-plane pod. `grep -r DirectAnthropicClient control-plane/src/` returns nothing.
4. **Terraform rename**: `terraform apply` with `judge_claude_credentials = <bundle>` creates `nautiloop-judge-creds` with `data.claude` key. With `judge_api_key` (deprecated), warning is emitted but existing behavior still works for one release.
5. **Dev setup auto-provisions**: `dev/setup.sh` on a fresh cluster (with `nemo auth --claude` having run) creates `nautiloop-judge-creds` from the local Claude bundle. Judge starts firing on the first loop without manual intervention.
6. **Token refresh works**: leave the cluster running past the 8hr OAuth expiry. Judge calls continue working without operator re-auth (sidecar refreshed the token transparently, same as agent pods).

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
- `terraform/modules/nautiloop/variables.tf` — rename `judge_api_key` → `judge_claude_credentials`, deprecation alias.
- `dev/setup.sh` — auto-provision `nautiloop-judge-creds` from the local Claude bundle.
- Tests per NFR-5.

## Baseline Branch

`main` at PR #181 (v0.7.1) merge.
