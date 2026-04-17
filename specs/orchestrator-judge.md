# Orchestrator Judge

## Overview

Insert a lightweight LLM "judge" at the three transition points where the loop engine today applies brittle heuristics to decide `continue | exit | escalate`: end-of-review, end-of-audit, and end-of-round-that-hit-max_rounds. The judge reads the full round history plus the current verdict and returns a structured decision. The Rust driver stays in charge of executing the decision (dispatch next stage, exit, escalate) — the LLM only decides *which*.

This is Stage 1 of the self-learning roadmap. Every judge invocation logs `(context, decision, downstream_outcome)` to a new `judge_decisions` table, building the dataset that a future Stage 2 fine-tune will train on.

## Baseline

Main at PR #127 merge.

Current orchestration at transition points in `control-plane/src/loop_engine/driver.rs`:

- **`evaluate_review_stage`** (~line 888): `verdict.clean == true` → create PR + `Reviewing → Converged`. `clean == false` AND `round < max_rounds` → `dispatch_implement_with_feedback`. `round >= max_rounds` → `Failed`.
- **`evaluate_harden_stage`** (~line 543): `audit.clean == true` → spec PR + `Hardening → Hardened`. `clean == false` AND `round < max_rounds` → `dispatch_revise`. `round >= max_rounds` → `Failed`.
- **`evaluate_test_stage`** (~line 799): pass → proceed to review. Fail AND `round < max_rounds` → `dispatch_implement_with_feedback`. Fail AND `round >= max_rounds` → `Failed`. (No judgment needed here — tests are binary.)

The hardcoded heuristic ignores three signals an LLM can weigh:

1. **Severity distribution.** A review with five `low`-severity nits blocks shipping the same way as five `high`-severity correctness bugs. Current code can't distinguish.
2. **Churn detection.** If rounds 3 and 4 surface the exact same finding the implementor is clearly not addressing it. Current code dispatches round 5 anyway.
3. **Reviewer drift.** If the reviewer introduces *new* unrelated findings each round (scope creep), the driver can't detect it.

## Problem Statement

### Problem 1: Shipping blocked by triviality

Every review must return `clean: true` to converge. A reviewer that flags a cosmetic `to_str().unwrap()` or a missing docstring comment holds up a PR that has already solved the spec's functional requirements. The engineer either re-runs with different prompts or cancels and ships by hand — both defeat the product's promise.

### Problem 2: Churn burns rounds

`max_rounds = 15` per phase. If the first three rounds already exposed the same root-cause finding repeatedly, rounds 4–15 are wasted compute. Current engine cannot detect "we are not making progress" and halt early.

### Problem 3: Reviewer expansion

A reviewer round can introduce findings unrelated to the spec (typos, style). The driver has no way to dismiss out-of-scope findings and will loop until the implementor accidentally satisfies them.

### Problem 4: No dataset for Stage 2

There is no ground-truth record of "given this round history, the right call was X". Without it, fine-tuning a resident orchestrator (Stage 2) is impossible.

## Functional Requirements

### FR-1: Judge invocation

**FR-1a.** A new component `OrchestratorJudge` is invoked from `evaluate_review_stage` and `evaluate_harden_stage` IMMEDIATELY AFTER the verdict is parsed AND BEFORE any state transition. It is NOT invoked on the happy clean=true-round-1 path (no ambiguity there).

**FR-1b.** The judge is invoked when ANY of these holds:

| Trigger                  | Reason                                           |
| ------------------------ | ------------------------------------------------ |
| `verdict.clean == false` | Decide continue vs. override-accept vs. escalate |
| `round >= max_rounds`    | Decide final disposition before FAILED           |
| `same_finding_repeat(>=2)` | Decide whether to keep trying or halt           |

`same_finding_repeat` = any finding in the current round whose `(category, file, line±2)` matches a finding in a previous round.

**FR-1c.** The judge is NOT invoked on the `verdict.clean == true AND round == 1` path. First-round clean verdicts skip the judge entirely (keeps the hot path cheap).

**FR-1d.** Judge timeout is 30s. On timeout or error, the driver falls back to the current heuristic behavior and logs a warning. Judge failures MUST NEVER block the loop.

### FR-2: Judge input

The judge receives a single JSON context object:

```json
{
  "loop_id": "uuid",
  "spec_path": "specs/foo.md",
  "spec_content": "...",               // full spec text
  "phase": "review" | "harden",
  "round": 4,
  "max_rounds": 15,
  "rounds": [
    {
      "round": 1,
      "stage": "implement" | "test" | "review" | "audit" | "revise",
      "verdict": { ... },              // full verdict JSON from that stage
      "duration_secs": 42
    },
    ...
  ],
  "current_verdict": { ... },          // the verdict just produced
  "recurring_findings": [              // pre-computed
    { "category": "...", "file": "...", "line": 116, "seen_in_rounds": [2, 3, 4] }
  ]
}
```

### FR-3: Judge output

The judge returns a structured decision:

```json
{
  "decision": "continue" | "exit_clean" | "exit_escalate" | "exit_fail",
  "confidence": 0.0..1.0,
  "reasoning": "short human-readable summary",
  "hint": "optional short string to inject into next agent prompt as an orchestrator note"
}
```

Decisions map to driver actions:

| Decision         | Driver action                                                                  |
| ---------------- | ------------------------------------------------------------------------------ |
| `continue`       | Dispatch next stage (implement-with-feedback or revise) as normal              |
| `exit_clean`     | Treat verdict as clean. Create PR (review) or spec PR (harden). Converge.       |
| `exit_escalate`  | Stop at `AWAITING_APPROVAL` with a judge-authored note in `failure_reason`.    |
| `exit_fail`      | Transition to `FAILED` immediately, with judge reasoning as `failure_reason`.   |

`hint` (when present) is written into the next round's feedback file as a new field `orchestrator_hint` alongside the existing `issues` / `failures` fields. Agents are instructed in their prompts to weight orchestrator hints heavily.

### FR-4: Judge model

**FR-4a.** The judge runs via the same model-proxy sidecar architecture used by review/audit agents. Model is configurable per-repo in `nemo.toml`:

```toml
[orchestrator]
judge_model = "claude-haiku-4-5"   # default
judge_enabled = true                # default true; false falls back to pure heuristic
```

**FR-4b.** Default model is the cheapest capable model (Haiku-tier). A judge call is a single short prompt → single short JSON response. Token budget: 8K input, 512 output max.

**FR-4c.** Judge runs as an in-process call from the loop engine's model-proxy, NOT as a k8s Job. Dispatching a whole pod for a sub-second LLM call is wasteful. The loop engine already has the sidecar networking in its own pod — reuse it.

### FR-5: Decision logging

**FR-5a.** Every judge invocation writes a row to a new `judge_decisions` table:

```sql
CREATE TABLE judge_decisions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    loop_id UUID NOT NULL REFERENCES loops(id),
    round INTEGER NOT NULL,
    phase TEXT NOT NULL,           -- 'review' | 'harden'
    trigger TEXT NOT NULL,          -- 'not_clean' | 'max_rounds' | 'recurring_findings'
    input_json JSONB NOT NULL,
    decision TEXT NOT NULL,         -- 'continue' | 'exit_clean' | 'exit_escalate' | 'exit_fail'
    confidence REAL,
    reasoning TEXT,
    hint TEXT,
    duration_ms INTEGER NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- populated later by the outcome reconciler
    loop_final_state TEXT,          -- NULL until the loop terminates
    loop_terminated_at TIMESTAMPTZ
);
CREATE INDEX idx_judge_decisions_loop ON judge_decisions (loop_id, round);
```

**FR-5b.** When a loop reaches a terminal state (`Converged`, `Hardened`, `Failed`, `Cancelled`, `Shipped`), the driver back-fills `loop_final_state` and `loop_terminated_at` on all `judge_decisions` rows for that loop. This gives Stage 2 a per-decision label: was this the right call?

**FR-5c.** `judge_decisions` rows are NEVER deleted, even when a loop is cancelled or fails. The training set needs failure cases.

### FR-6: Observability

**FR-6a.** Every judge invocation logs at INFO level with `loop_id`, `round`, `decision`, `confidence`, `duration_ms`.

**FR-6b.** A new Grafana-friendly metric (tracing span attribute) `judge_decision_total{decision=...}` tracks call volume. Not blocking; the logs are sufficient for Stage 1.

**FR-6c.** `nemo inspect` output gains a `judge_decisions` array (one entry per round that triggered the judge), so engineers can see why the loop made non-obvious transitions.

### FR-7: Safety

**FR-7a.** `exit_clean` can be returned at most ONCE per loop. A second `exit_clean` attempt is treated as `continue` and logged. This prevents a loop where the judge yo-yos between acceptance and rejection.

**FR-7b.** `exit_escalate` always transitions to `AWAITING_APPROVAL`. The engineer's subsequent `nemo approve` re-enters the loop at the next round (NOT convergence). This preserves human-in-the-loop override.

**FR-7c.** The judge CANNOT skip phases. It cannot, e.g., `exit_clean` out of the harden phase directly into `Converged` (skipping implement). The `exit_clean` decision in `harden` means "accept this spec as hardened"; in `review` means "accept this implementation as converged".

## Non-Functional Requirements

### NFR-1: Cost ceiling

Per-loop judge cost MUST stay under `$0.05` using Haiku-tier. Budget: at most 10 judge calls per loop × `$0.005`/call. If a loop exceeds 10 judge calls, subsequent calls short-circuit to heuristic fallback and log a warning.

### NFR-2: Latency

Judge invocation adds ≤3s to a state-transition tick at p95. Timeout (FR-1d) caps tail.

### NFR-3: Backward compatibility

`judge_enabled = false` (per-repo or default) produces byte-identical behavior to today. A single feature flag, fully reversible. All existing tests continue to pass unchanged when the flag is off.

### NFR-4: Migration

Add the `judge_decisions` table via a new migration file. No backfill; table starts empty.

### NFR-5: Tests

- **Unit** (`control-plane/src/loop_engine/judge.rs`): mock `ModelClient`; verify prompt assembly, JSON parsing, timeout handling, cost-ceiling enforcement, one-shot-`exit_clean` guard (FR-7a).
- **Integration** (`control-plane/tests/judge_integration.rs`): full driver tick with mock judge returning each decision variant; assert correct state transitions and `judge_decisions` rows written.
- **Fallback**: mock judge returns error → driver uses heuristic; assert identical behavior to the feature-flag-off case.

## Acceptance Criteria

A reviewer can verify by:

1. **Churn halt:** start a loop on a deliberately unstable spec (e.g. `specs/impossible-contradiction.md`). Confirm the judge exits as `exit_escalate` within 3–4 rounds of detecting recurring findings, rather than burning all 15.
2. **Triviality override:** start a normal loop, manually inject a `low`-severity nit as the only remaining finding in round 3. Confirm the judge returns `exit_clean` with a reasoning note and the PR opens.
3. **Fallback:** set `judge_enabled = false`; run the same scenarios. Pure heuristic behavior returns (byte-for-byte identical to pre-spec main).
4. **Dataset populated:** after a converged loop, `SELECT * FROM judge_decisions WHERE loop_id = ?` shows one row per judged transition with `loop_final_state` set.
5. **Cost cap:** artificially force 11 judge calls in a single loop; the 11th short-circuits to heuristic and a warning log is emitted.
6. **Inspect visibility:** `nemo inspect <branch>` shows `judge_decisions` inline with each round.

## Out of Scope

- **Stage 2 fine-tune.** Training a resident model on the collected `judge_decisions` dataset is a follow-up spec. This spec only builds the data-collection infrastructure and the Claude/GPT-powered judge.
- **Judging the implementor mid-stream.** The judge runs only at stage evaluation points, not inside the implementor's thought process. No premature halting of a running agent.
- **Judge in the test evaluator.** Test stage is binary pass/fail; no judgment needed (noted in Baseline).
- **Cross-loop judgment.** The judge sees only the current loop's history, not other loops' outcomes. Repo-level learnings (the `.nautiloop/learnings.md` idea) would be a parallel spec.
- **Multi-model judge ensembles.** One judge, one decision. No quorum logic.

## Files Likely Touched

- `control-plane/migrations/<timestamp>_add_judge_decisions.sql` — new table.
- `control-plane/src/loop_engine/judge.rs` — new module: `OrchestratorJudge` struct, prompt assembly, model client wrapper, fallback handling.
- `control-plane/src/loop_engine/driver.rs` — wire the judge into `evaluate_review_stage` and `evaluate_harden_stage`; add recurring-finding detector; back-fill outcome on terminal transition.
- `control-plane/src/config/merged.rs` — add `[orchestrator]` section (judge_model, judge_enabled).
- `control-plane/src/types/api.rs` — extend `InspectResponse` with `judge_decisions`.
- `cli/src/commands/inspect.rs` — render new section.
- `.nautiloop/prompts/judge.md` — new prompt template for the judge.
- Tests per NFR-5.

## Baseline Branch

`main` at PR #127 merge.
