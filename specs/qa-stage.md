# QA Stage

## Overview

Insert a new stage `qa` between `review` (clean verdict) and `CONVERGED`. The QA stage exercises the implementation against the spec's `## Acceptance Criteria` section: it builds, runs, exercises each criterion, and reports pass/fail per criterion. Only when every criterion passes does the loop converge and open a PR. QA failures feed back into a new implement round with concrete runtime failures (not just static review findings).

Project-agnostic by design: works for any language, framework, or repo shape. The QA agent reads the spec's acceptance criteria and figures out how to verify them using whatever the target repo provides (test frameworks, CLI binaries, HTTP endpoints, shell commands). Zero nautiloop-specific logic.

This closes the trust-calibration gap: today, a reviewer's `clean: true` verdict means "the code looks right"; after this spec lands, it means "the code actually works."

## Baseline

Main at PR #157 merge.

Current stages: `implement → test → review → (cycle) → CONVERGED → PR`.

`test` stage runs repo-declared test commands from `nemo.toml [services.<name>].test`. It's a unit-test gate, not a functional verifier. It has no visibility into what the spec asked for; it only knows "did the repo's existing tests pass."

`review` stage is semantic — reviewer reads diff + spec, emits findings. It does NOT execute the code. A clean review signals "no issues I can spot statically."

Net result: today's converged PRs pass review without anyone proving the implementation satisfies the spec's acceptance criteria. Measured gap observed during 2026-04-18 dogfood: four converged PRs (#155, #156, #157, #158) merged today, zero of them verified to pass their own acceptance criteria at runtime.

## Problem Statement

### Problem 1: Convergence ≠ correctness

Every round produces output the reviewer grades semantically. The reviewer does not compile, run, curl, or interact with the produced code. "Clean" is "looks right." The operator has no runtime evidence the feature works, only a model's aesthetic judgment.

### Problem 2: Specs without executable acceptance criteria are unverifiable

Specs today may or may not include `## Acceptance Criteria`. When they do, the reviewer reads them but the loop engine doesn't check them. When they don't, the spec is a pure wishlist with no binding contract on "done."

### Problem 3: Fine-tune training data is weak

The orchestrator judge (#128) trains on `(context, decision, downstream_outcome)` where `outcome` is currently `CONVERGED` or `FAILED`. These are proxies for "everyone was happy for a moment" and "something timed out." Neither tells the model whether the code actually did what the spec asked. QA outcomes (`qa_passed: true` + per-criterion pass/fail) are far higher-signal for fine-tuning.

### Problem 4: False confidence compounds

Today's workflow merges machine-produced PRs that have never been verified. A regression that snuck through one loop's reviewer becomes the baseline for the next loop's diff context. Silent accumulation of "looks-right-but-broken" code eventually surfaces in a production incident — exactly the scenario self-hosted autonomous coding tools are supposed to eliminate.

## Functional Requirements

### FR-1: New stage `qa` in the state machine

**FR-1a.** Add `LoopState::QA` and `Stage::QA` to the state machine. Inserted between `Reviewing` and `Converged`:

```
Reviewing → (verdict.clean == true) → QA → (qa.all_passed == true) → Converged → PR
                                        → (qa.all_passed == false) → Implementing (next round)
                                   → (verdict.clean == false) → Implementing (next round)   [unchanged]
```

**FR-1b.** The existing `review.clean == true → Converged` transition is replaced with `review.clean == true → QA`. No existing test or handler logic needs to change semantics other than this single edge.

**FR-1c.** QA consumes a round budget slot the same way implement/test/review do. Hitting `max_rounds_implement` while in QA → `Failed` with `failure_reason: "Max rounds exceeded during QA verification"`.

### FR-2: QA stage input — the spec's Acceptance Criteria section

**FR-2a.** The QA stage reads the spec file from the branch's current worktree, parses the Markdown, and extracts the content under a heading matching `^#{1,3}\s*Acceptance Criteria\s*$` (case-insensitive). The extracted block is a numbered or bulleted list of criteria, one per item.

**FR-2b.** If the spec has NO `## Acceptance Criteria` section, QA **fails immediately** with:

```json
{
  "stage": "qa",
  "data": {
    "criteria": [],
    "all_passed": false,
    "ci_status": "failed",
    "summary": "Spec has no '## Acceptance Criteria' section; QA cannot verify. Add a list of criteria in the spec and re-run.",
    "missing_criteria": true
  }
}
```

Loop transitions to `Failed` (not back to implement — the spec itself is the blocker, and the implementor can't fix a missing section). `failure_reason` names the missing section explicitly.

**FR-2c.** Empty or whitespace-only Acceptance Criteria block is equivalent to a missing section (same failure).

**FR-2d.** The loop engine does NOT try to infer acceptance criteria from other sections. Forcing the spec to be explicit is the point.

### FR-3: QA agent — project-agnostic verifier

**FR-3a.** The QA stage dispatches a new k8s Job running the agent image (same image as implement/revise, no new image). The entrypoint runs a new prompt template `.nautiloop/prompts/qa.md` with the Claude / opencode CLI (configurable per-repo and per-engineer, same model-selection machinery as other stages).

**FR-3b.** The QA agent's prompt receives:
- The full spec content (so it knows what was asked)
- The extracted Acceptance Criteria list
- The diff between the base branch and the current branch (what was implemented)
- The branch's tip SHA
- The repo's language/framework hints from `nemo.toml [repo]` (optional, see FR-5)

**FR-3c.** The QA agent's job for each criterion:
1. Propose how to verify it (shell command, curl invocation, running a test, launching a CLI and inspecting output, reading produced files, etc.). The prompt instructs the agent to prefer **observable runtime behavior** over static inspection; if a criterion is genuinely non-executable (e.g., "documentation is clear"), the agent flags it as `untestable` with a reason.
2. Execute the verification in the sandboxed worktree. Commands have full repo access and can invoke language tooling, CLIs, local servers, curl, etc. Egress goes through the existing auth sidecar (same sandboxing as implement/revise).
3. Compare observed behavior to the expected outcome as stated in the criterion.
4. Emit a structured result per criterion.

**FR-3d.** The agent writes its final verdict as a single `NAUTILOOP_RESULT:` line matching the schema below.

### FR-4: QA verdict schema

**FR-4a.** Verdict JSON:

```json
{
  "stage": "qa",
  "data": {
    "all_passed": true,
    "ci_status": "passed",
    "criteria": [
      {
        "id": 1,
        "description": "A reviewer can verify by: running `nemo start specs/foo.md` on an unhardened spec → runs harden, emits spec PR, transitions to AWAITING_APPROVAL.",
        "passed": true,
        "untestable": false,
        "verification_method": "ran `nemo start specs/fixtures/soft-spec.md`, observed HARDEN phase output in stdout, confirmed loop state via `nemo status`",
        "evidence": "nemo status output showed state=AWAITING_APPROVAL; spec PR created at https://.../pull/999",
        "notes": null
      },
      {
        "id": 2,
        "description": "...",
        "passed": false,
        "untestable": false,
        "verification_method": "curl http://localhost:18080/dashboard/state with valid cookie",
        "evidence": "got HTTP 500, response body: {\"error\":\"unknown field 'viewer'\"}",
        "notes": "criterion expected JSON to include viewer field per FR-2b; handler does not set it"
      },
      {
        "id": 3,
        "description": "... is clearly documented",
        "passed": false,
        "untestable": true,
        "verification_method": "read docs/local-dev-quickstart.md section 'Your first loop'",
        "evidence": "documentation exists but criterion says 'clearly' which is subjective",
        "notes": "marking untestable; operator judgement required"
      }
    ],
    "summary": "2 of 3 criteria passed. Criterion 2 failed: handler does not set expected JSON field. Criterion 3 is untestable without operator judgement.",
    "token_usage": { "input": 0, "output": 0 }
  }
}
```

**FR-4b.** `all_passed` is true IFF every criterion has `passed: true`. Untestable criteria count as **neither pass nor fail** — the agent flags them and the driver's policy (FR-7) determines how to treat them.

**FR-4c.** `ci_status` mirrors the `test` stage's existing semantics for dashboard rendering: `passed`, `failed`, or `unknown` (if the agent itself crashed).

### FR-5: Language / framework hints

**FR-5a.** New optional section in nemo.toml:

```toml
[qa]
# Language hints help the QA agent pick sensible default verification tactics.
# Optional; the agent auto-detects from repo markers (Cargo.toml, package.json,
# go.mod, pyproject.toml) if this section is absent.
languages = ["rust", "typescript"]

# Commands to run before the QA agent starts. Useful for standing up
# integration dependencies the agent will exercise (e.g., docker compose up
# -d for a DB the code connects to). Run in sequence; failure aborts QA.
setup_commands = [
    "cargo build --workspace",
]

# Commands to run after QA finishes, regardless of outcome (best-effort cleanup).
teardown_commands = [
    "docker compose down",
]

# Default verification approach when the spec criterion doesn't hint at one.
# "auto" = agent decides; "tests" = prefer running test suites; "cli" = prefer
# CLI invocations; "http" = prefer HTTP requests.
default_verification = "auto"
```

**FR-5b.** All fields optional with sensible defaults. No `[qa]` section → `languages` auto-detected, `setup_commands = []`, `teardown_commands = []`, `default_verification = "auto"`.

**FR-5c.** Language-specific affordances the agent knows about at baseline: Rust, TypeScript/JavaScript, Python, Go. Others work via the `default_verification = "auto"` path (agent improvises from repo contents).

### FR-6: QA feedback loop

**FR-6a.** When `all_passed == false` AND `round < max_rounds_implement`, the driver writes `.agent/qa-feedback-round-N.json` to the branch worktree:

```json
{
  "round": N,
  "source": "qa",
  "failures": [
    { "criterion_id": 2, "description": "...", "observed": "HTTP 500: unknown field viewer" }
  ],
  "untestable": [
    { "criterion_id": 3, "description": "...", "reason": "subjective" }
  ]
}
```

Then dispatches a new `implement` stage with `FEEDBACK_PATH` pointing at the file.

**FR-6b.** The `implement.md` prompt template (existing) is updated to recognize `source: "qa"` feedback and treat it like review feedback: fix the failing criterion, preserve what's already passing.

**FR-6c.** The QA agent's session is resumed across rounds (same session-persistence model as other stages), so it builds context across iterations and gets sharper on "what verification approach actually works for this codebase."

### FR-7: Untestable criteria policy

**FR-7a.** When the QA agent flags a criterion as `untestable: true`, the default policy is **pass the loop but surface the untestable count in the PR body**. Rationale: an untestable criterion (documentation clarity, visual design) is a fail of the *spec*, not the implementation. Forcing a fix at implementation time is wrong.

**FR-7b.** Per-repo override via nemo.toml:

```toml
[qa]
# "pass" (default) — untestable criteria don't block convergence
# "fail" — untestable criteria fail QA; forces sharper Acceptance Criteria
# "ask" — transition to AWAITING_APPROVAL with untestable list for engineer decision
untestable_policy = "pass"
```

**FR-7c.** The PR body generated on convergence surfaces the QA report:

```markdown
## QA Results
- 5/5 testable acceptance criteria passed
- 2 untestable criteria (flagged by QA agent):
  - "Documentation is clear" — requires operator judgement
  - "UI looks polished" — requires visual review
```

### FR-8: Observability

**FR-8a.** `nemo inspect <branch>` gains a `qa` field per round containing the verdict.

**FR-8b.** The dashboard (#145-147) loop detail page shows a QA pane: per-criterion pass/fail with the verification_method and evidence expanded on tap.

**FR-8c.** New control-plane metric: `qa_pass_rate` (converged loops whose QA passed on first-round QA vs required multiple QA rounds). Surfaces whether the reviewer's clean verdict correlates with runtime correctness — directly measures the trust gap this spec is closing.

## Non-Functional Requirements

### NFR-1: Project-agnostic

Zero nautiloop-specific logic. The QA stage works for any target repo: Rust control planes, Go microservices, Python data pipelines, TypeScript web apps. Language hints bias the agent's tactics but don't gate the pipeline.

### NFR-2: Sandboxing

QA runs in the same k8s Job architecture as implement/revise. Same sidecar, same egress policy, same credential scoping, same resource limits (`cpu: 2 / 2000m`, `memory: 4Gi` default — configurable per repo via `[qa] resources` future extension).

### NFR-3: Cost

One additional agent Job per loop that reaches the QA stage. Budget impact: ~1-3 minutes of wall time + ~$0.05-0.30 of model cost per loop. Explicit tradeoff the operator accepts by enabling QA (FR-9 feature flag). The user's framing: "it's the cost of correctness."

### NFR-4: Feature flag

QA is gated behind `[qa] enabled = true` in nemo.toml. Default: `false` for backward compatibility. Operators opt in when they're ready for spec-discipline enforcement. Once we're confident, default flips to `true` in a later release.

### NFR-5: Fail-closed safety

If the QA agent itself crashes (pod error, model timeout, parse failure), the loop is treated as QA-failed with `ci_status: "unknown"`, NOT as QA-passed. Bugs in QA must not silently convert to false-positive convergences.

### NFR-6: Tests

- **Unit** (`control-plane/src/loop_engine/driver.rs`): state transition Reviewing(clean) → QA; QA(pass) → Converged; QA(fail, within max_rounds) → Implementing; QA(fail, max_rounds exceeded) → Failed.
- **Unit** (`control-plane/src/loop_engine/qa.rs`): spec without `## Acceptance Criteria` section produces fail-early verdict.
- **Integration** (`control-plane/tests/qa_integration.rs`): full loop with a fixture spec that has 3 criteria, fixture implementation that fails criterion 2, assert loop redispatches implement with qa-feedback, fixture r2 passes all criteria, loop converges.
- **Prompt** (manual): run QA against a real converged PR from today (e.g., #155 helm-phase2); verify agent's verdict is sensible and criterion-by-criterion.

## Acceptance Criteria

A reviewer can verify by:

1. **Missing AC section**: submit a spec with no `## Acceptance Criteria` heading, run `nemo start` with QA enabled. Loop fails at QA stage with `failure_reason` naming the missing section. Time-to-fail <5 min (no implement wasted).
2. **All criteria pass**: submit a spec with a testable AC list, implement a correct implementation manually (or via nemo start), enable QA. QA stage runs, emits per-criterion pass verdict, loop converges with a PR.
3. **One criterion fails**: submit a spec, break one acceptance criterion in the implementation, run. QA detects failure, emits structured feedback, next implement round addresses it, subsequent QA passes, loop converges.
4. **Untestable criterion passes** (default policy): spec includes "Documentation is clear" as a criterion. QA flags untestable. Loop converges. PR body lists the untestable criterion.
5. **Untestable criterion fails** (`untestable_policy = "fail"`): same spec. Loop fails. PR never opens. `failure_reason` names untestable criterion.
6. **Language agnostic**: run QA against a Go repo with an AC that says "running `go test ./...` passes." QA agent runs `go test`, reports pass. No Rust-specific assumptions involved.
7. **Feature flag off** (`[qa] enabled = false`): loop converges exactly as before (no QA stage, no behavior change).
8. **Agent crash**: inject a pod failure during QA. Loop fails with `qa.ci_status = "unknown"`. Does NOT accidentally converge.

## Out of Scope

- **UI/visual verification.** QA is command-line. A criterion like "button is blue" is untestable by default (agent flags it). Visual regression testing is a separate concern.
- **Performance benchmarking** ("response time < 100ms"). The QA agent runs a single execution; it's not a load tester. Perf criteria should be phrased as "returns HTTP 200 within T seconds" and the agent runs one attempt.
- **Auto-generated acceptance criteria.** If the spec has no AC section, QA fails — we don't try to infer from other sections. Forces the spec-author to think about "what does done mean."
- **Cross-PR regression testing.** QA verifies only the current loop's changes satisfy the current spec. Previously-merged behavior is not re-verified.
- **QA result as judge input.** The orchestrator judge (#128) could use QA outcomes as signal, but that integration is a separate spec amendment once both stages are stable.
- **Test authoring by QA.** QA runs existing tests and ad-hoc verifications; it does NOT write new test files. That's the implementor's job.

## Files Likely Touched

- `control-plane/src/types/mod.rs` — new `LoopState::QA`, `Stage::QA`.
- `control-plane/src/state/postgres.rs` + migration — new state enum value.
- `control-plane/src/loop_engine/driver.rs` — insert QA stage dispatch + transitions.
- `control-plane/src/loop_engine/qa.rs` — new module: spec AC extractor + verdict parser + feedback file writer.
- `control-plane/src/types/verdict.rs` — new `QaVerdict`, `QaCriterionResult` types.
- `control-plane/src/k8s/job_builder.rs` — QA stage config (mounts, env, resources).
- `.nautiloop/prompts/qa.md` — new: QA agent prompt template.
- `images/base/nautiloop-agent-entry` — new `qa` case in stage dispatch.
- `control-plane/src/config/repo.rs` + `merged.rs` — `[qa]` section parsing.
- `cli/src/commands/inspect.rs` — render QA pane per round.
- Tests per NFR-6.

## Baseline Branch

`main` at PR #157 merge.
