# Ship the Orchestrator Judge

## Overview

The orchestrator judge (spec #128, implementation merged in PR #157) exists in code but is **not actually running in production**. Diagnostic run on 2026-04-20: 30+ loops completed today, `SELECT COUNT(*) FROM judge_decisions` returns `0`. The judge never fires. This spec wires it up so it actually executes, writes decisions to the dataset, and is observable.

Root cause: `control-plane/src/main.rs` constructs the driver via `ConvergentLoopDriver::new(...)` which takes no `JudgeModelClient`, so `self.judge` stays `None` in the driver, and `invoke_judge_for_phase` returns on `self.judge.as_ref()?` for every call. Additionally, the existing `SidecarJudgeClient` implementation expects a sidecar URL that doesn't exist for the control-plane pod (the control plane doesn't have its own auth sidecar — that's for agent pods).

## Baseline

Main at PR #176 merge.

Existing code:
- `control-plane/src/loop_engine/judge.rs` — full judge implementation: `OrchestratorJudge`, `JudgeModelClient` trait, `SidecarJudgeClient` concrete impl, `should_invoke` + `invoke` methods
- `control-plane/src/loop_engine/driver.rs` — `with_judge` constructor, `invoke_judge_for_phase` called from `evaluate_review_stage` (line 972) + `evaluate_harden_stage` (line 631)
- `control-plane/src/config/mod.rs` — `judge_enabled: bool` (default `true`), `judge_model: String` (default `"claude-haiku-4-5"`)
- `control-plane/migrations/*` — `judge_decisions` table exists

Missing piece: the construction wiring in `main.rs` and a model-client impl that works for the control-plane pod (no agent sidecar available).

## Problem Statement

### Problem 1: Dead code

`judge_enabled = true` is the default. `self.judge.as_ref()?` fails every call. Zero judge decisions recorded despite every loop having multiple eligible transition points. The feature is shipped in name only.

### Problem 2: Model-client abstraction mismatch

`SidecarJudgeClient::new(base_url)` targets the per-pod auth sidecar at `http://localhost:9090/anthropic/v1/messages`. That exists on AGENT pods but not the CONTROL PLANE pod. There's no existing `DirectAnthropicClient` for cases where the caller needs to talk to Anthropic without going through a sidecar.

### Problem 3: Credentials

The judge needs an Anthropic API key (or OAuth bundle). The control plane has `nautiloop-creds-<engineer>` secrets but they're per-engineer. For a judge running in-process in the control plane, it needs to either:
- Pick the engineer from the loop record and use their credentials (correct attribution)
- OR have a dedicated `nautiloop-judge-creds` cluster-level secret (simpler, one config)

Today neither is wired.

## Functional Requirements

### FR-1: Direct Anthropic client

**FR-1a.** Add a new `DirectAnthropicClient` in `control-plane/src/loop_engine/judge.rs` implementing `JudgeModelClient`. Calls `https://api.anthropic.com/v1/messages` directly with a bearer token (Anthropic API key) OR an OAuth bundle (Claude Code credentials).

**FR-1b.** Auth mode determined by which credential source is available (checked in order):
1. `NAUTILOOP_JUDGE_API_KEY` env var — raw Anthropic API key. Sent as `x-api-key` header. For prod where operators provision their own Anthropic key.
2. Credentials file at `/secrets/judge/credentials.json` (path configurable via `config.orchestrator.judge_credentials_path`, default `/secrets/judge/credentials.json`). Expected JSON schema:
   ```json
   {
     "api_key": "sk-ant-..."
   }
   ```
   When `api_key` is present, sent as `x-api-key` header (same as env var mode).
3. If neither is available, judge is disabled with a one-time startup warning. Loop continues working; heuristic fallback per existing code.

**Note:** OAuth token refresh is out of scope for v1. If OAuth-based auth is needed in the future, a new credential type can be added with `oauth_token` / `refresh_token` / `expires_at` fields and refresh logic. For now, only static API keys are supported.

**FR-1c.** Uses the existing `claude-haiku-4-5` model by default. Model name from config `[orchestrator] judge_model`. No sticker price change from the spec's original $0.05/loop ceiling.

**FR-1d.** `DirectAnthropicClient` must use the same request body format and timeout as `SidecarJudgeClient`: 30-second HTTP timeout, `Content-Type: application/json`, `anthropic-version: 2023-06-01` header, and `max_tokens=512` in the request body (matching `SidecarJudgeClient`'s existing value). Auth headers differ by design — `DirectAnthropicClient` adds `x-api-key` and targets `api.anthropic.com` instead of a localhost sidecar. No retry on failure — a failed invocation falls through to the heuristic path per NFR-1.

### FR-2: Wire the driver correctly in main.rs

**FR-2a.** `control-plane/src/main.rs` Loop Engine branch: if `config.orchestrator.judge_enabled`:
1. Check env var / secret file for judge credentials
2. If present: construct `DirectAnthropicClient`, call `ConvergentLoopDriver::with_judge(...)`.
3. If absent: call `ConvergentLoopDriver::new(...)` and log `WARN` that judge is enabled-in-config but creds missing.

**FR-2b.** Startup log line reports the final resolved state: `"Orchestrator judge: enabled, model=claude-haiku-4-5"` OR `"Orchestrator judge: disabled (judge_enabled=false)"` OR `"Orchestrator judge: enabled in config but NAUTILOOP_JUDGE_API_KEY/credentials missing; skipping"`.

### FR-3: Optional: Kubernetes secret mount

**FR-3a.** For the operator's convenience, when deploying via terraform: optional new variable `judge_api_key` (string, sensitive, default null). If set, terraform creates a `nautiloop-judge-creds` secret and mounts it at `/secrets/judge/` in the loop-engine deployment.

**FR-3b.** For dev: `dev/setup.sh` reads `NAUTILOOP_JUDGE_API_KEY` env var and creates the same secret, so dev operators can opt in with one env var.

**FR-3c.** Both paths are optional. If neither is set, behavior matches FR-2a: judge disabled with warning, heuristic fallback.

### FR-4: Observability of running judge

**FR-4a.** Verify that every judge invocation logs at INFO with: `loop_id`, `round`, `phase`, `trigger`, `decision`, `confidence`, `duration_ms`. The existing `judge.rs` already logs at INFO level — confirm these fields are all present and add any that are missing.

**FR-4b.** `nemo inspect <branch>` output includes the `judge_decisions` array (already planned in #128 FR-6c — verify it's actually rendering).

**FR-4c.** Dashboard `/dashboard/loops/:id` detail page: verify that judge decisions render in the round detail view (the existing implementation shows styled HTML blocks with decision text, confidence %, reasoning, and hint). If judge decisions are present in the data but not rendering, fix the rendering. No new gavel icon is required — the existing styled blocks are sufficient.

**FR-4d.** Emit an INFO-level `tracing::info!` event with target `"judge"` and fields `judge_decision_total = <cumulative count for this loop>` after each invocation, so operators can grep logs for `judge_decision_total` to track invocation rate. This is a structured log field, not a metrics-crate counter.

## Non-Functional Requirements

### NFR-1: Graceful degradation

Judge failure (Anthropic API down, key rotated, timeout) NEVER fails the loop. Existing heuristic path is the fallback; current `should_invoke` returns a sensible "fall through" when judge reports error. Verify by injecting a failing `JudgeModelClient` in tests.

### NFR-2: No new infrastructure

No new pod, no new service. The control plane makes outbound HTTPS calls to Anthropic directly. Same egress story as the existing sidecar proxy (calls to `api.anthropic.com`).

### NFR-3: Cost ceiling per-loop

Spec #128 capped at 10 judge invocations per loop. Verify that cap is enforced (short-circuits to heuristic after 10). Log metric when cap hits so we can see if it happens in practice.

### NFR-4: Tests

- **Unit**: `DirectAnthropicClient` constructs with env-var API key; with credentials file at configured path; errors cleanly when neither present.
- **Unit**: `main.rs` initialization picks the right driver constructor based on env/config/secret availability.
- **Integration**: loop runs end-to-end with judge enabled on a mock Anthropic endpoint; verify `judge_decisions` rows written.

## Acceptance Criteria

A reviewer can verify by:

1. **Control-plane startup**: with `NAUTILOOP_JUDGE_API_KEY` set, log shows `"Orchestrator judge: enabled, model=..."`.
2. **Dead config detection**: unset the env var; log shows `"...enabled in config but credentials missing; skipping"`. Loop runs normally.
3. **Actual decisions**: run a loop with the judge enabled. After it terminates, `SELECT COUNT(*) FROM judge_decisions WHERE loop_id = ?` is non-zero. `SELECT decision, reasoning FROM judge_decisions ...` shows structured output, not just nulls.
4. **Graceful degrade**: during a running loop, revoke the API key server-side. Loop continues. Log shows judge invocation failures as WARN. Loop still terminates via heuristic path.
5. **Cost ceiling**: instrument a loop to force 11 judge calls (manually, in test harness). The 11th short-circuits to heuristic with a log line.
6. **Observable**: `nemo inspect <branch>` shows the `judge_decisions` field. Dashboard detail page renders judge decision blocks (decision text, confidence, reasoning) for judged rounds.

## Post-Ship Validation (not required for code PR)

The following dogfooding plan is operational follow-up work, not part of the implementation PR. It should be executed after the code ships and a deployment with valid `NAUTILOOP_JUDGE_API_KEY` is available.

**V-1.** Pick one spec (small, ~5 KB) and run it THREE times:
1. `judge_enabled = false` (baseline heuristic)
2. `judge_enabled = true` on default config
3. `judge_enabled = true` with an adversarial prompt tweak to test `exit_escalate` triggering

Record: rounds-to-converge, cost in tokens, judge decisions per loop, operator-perceived convergence quality (subjective).

**V-2.** Ship results as a short note in `docs/convergence-learnings.md` or similar — even if "the judge doesn't help much yet, here's the data."

## Out of Scope

- **Per-engineer Claude credentials for the judge**. Cluster-level key for v1. Per-engineer is a follow-up if/when engineers want their own model spend attributed.
- **Stage 2 fine-tune on `judge_decisions`**. Separate spec once the dataset exists.
- **Cross-loop judgment** (judge sees other loops' outcomes). Per-loop context only in v1.
- **Judge-as-a-service endpoint** (`POST /judge/decide` for external callers). Internal-only for now.
- **Automatic judge-disable on consecutive errors**. If the judge is broken, ops disables via config. Don't silently turn off a feature the operator configured on.

## Files Likely Touched

- `control-plane/src/loop_engine/judge.rs` — add `DirectAnthropicClient`.
- `control-plane/src/main.rs` — wire `with_judge` construction based on config + creds availability.
- `control-plane/src/config/mod.rs` — add `judge_credentials_path: Option<String>` (default `/secrets/judge/credentials.json`, per FR-1b).
- `dev/setup.sh` — optional env var `NAUTILOOP_JUDGE_API_KEY` → `nautiloop-judge-creds` secret.
- `dev/k8s/05-control-plane.yaml` (or terraform equivalent) — mount `nautiloop-judge-creds` secret into loop-engine deployment.
- `terraform/modules/nautiloop/variables.tf` + `k8s.tf` — optional `judge_api_key` variable.
- Tests per NFR-4.

## Baseline Branch

`main` at PR #176 merge.
