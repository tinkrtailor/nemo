# Push During Implement

## Overview

Push the agent branch to the remote after every agent commit during implement/revise rounds, not only on convergence. Today, mid-loop commits live only in the cluster's bare repo; if the loop hits a terminal failure (infra error, deadline, credential expiry) the work is stranded on the cluster and the engineer has to manually push from inside a pod to salvage it. This spec makes salvage the default: whatever the loop has produced is always reachable on GitHub.

Observed cost of the current behavior: 2026-04-19 session, mobile-dashboard loop produced 12 rounds of substantive commits, loop failed on review-pod init, zero commits on GitHub until a manual `kubectl exec ... git push` recovered them. Three hours of work that was one k8s hiccup away from being lost.

## Baseline

Main at PR #163 merge (which itself was a manual salvage).

Current behavior:
- Agent pod commits locally in the worktree (see `nautiloop-agent-entry` + implement/revise stage success paths).
- Control-plane's `create_pr` path in `driver.rs::evaluate_review_stage` pushes the branch only once, at convergence, immediately before calling `gh pr create`.
- The harden `spec_pr_url` path pushes when opening the spec PR.
- Any mid-loop failure (deadline, pod init, retry exhaustion, max_rounds) leaves the branch un-pushed. The commits exist in the cluster PVC's bare repo but are invisible to the engineer without pod-exec.

## Problem Statement

### Problem 1: Terminal failures strand work

A single k8s transient error during a 5-15-round loop can permanently hide hours of real commits. The engineer discovers this only when trying to open a PR from the failed loop and sees "No commits between main and branch." Recovery requires `kubectl exec` into the control-plane pod and manual `git push`. Most operators won't know that's possible.

### Problem 2: No visibility during active loops

An operator watching a long-running loop cannot inspect the diff at round N. They can see reviewer verdicts in `nemo inspect`, but the actual code change isn't available on GitHub until the loop ends. For debugging "why is the reviewer still unhappy at r8?" this is a real gap — the diff is the primary evidence.

### Problem 3: Forced reliance on convergence for salvage

`nemo salvage` (discussed and rejected earlier) only makes sense because pushes are absent during the loop. If every round's output is already on origin, `git checkout <branch> && gh pr create` is the salvage — no new CLI verb needed.

### Problem 4: Operator trust

"It crashed and lost my work" is a bad product experience regardless of whether the work is technically recoverable via obscure commands. Continuous push ensures the engineer's mental model of "I submitted this 3 hours ago, the code must be somewhere on GitHub by now" matches reality.

## Functional Requirements

### FR-1: Push after every implement/revise commit

**FR-1a.** After a successful implement or revise stage commit (the agent's `new_sha` commit, OR the `chore(agent): add .agent/<feedback>.json` commit the driver writes), the control plane executes `git push origin <branch>` on the bare repo before any further state transition.

**FR-1b.** The push is non-fatal: if it fails (network, rate limit, credential expiry), log the failure as a warning and continue the loop. The work stays in the bare repo; a later push will succeed on the next round's commit. This preserves behavior-under-failure: transient push errors do not fail loops.

**FR-1c.** The push is best-effort: no retry loop, no backoff, one attempt per commit. The next round's push catches up.

### FR-2: Where the push happens

**FR-2a.** Centralize pushes in `control-plane/src/git/mod.rs`. A new method `GitOperations::push_branch_best_effort(&self, branch: &str) -> ()` that wraps `push_branch` and swallows errors into a log line. Returns unit intentionally; callers don't branch on success/failure.

**FR-2b.** Call sites that add `push_branch_best_effort`:
1. `driver.rs::ingest_job_output` — after `update_loop` writes the new current_sha from the agent's commit. Runs for implement, revise, audit, review (review doesn't commit but is harmless).
2. `driver.rs::dispatch_implement_with_feedback` — after `write_file(&record.branch, feedback_path, ...)` commits the feedback file.
3. `driver.rs::dispatch_revise` — after the audit-feedback write commits.

**FR-2c.** Existing pushes at convergence (`evaluate_review_stage`, harden spec PR path) are retained. They become redundant on the happy path (last round's push already happened) but still correct — force-push-no-op is cheap.

### FR-3: Idempotency + fast-forward check

**FR-3a.** `push_branch_best_effort` does NOT force-push. It relies on fast-forward from the bare repo's latest commit on that branch. Agent commits are always fast-forward (we control the branch), so non-force is correct.

**FR-3b.** If the remote has commits we don't have (e.g., an engineer manually pushed something), push fails with "non-fast-forward." This is correctly treated as a transient error (FR-1b) — the next round's push includes the same agent commits plus whatever new on top; still fails. This is an intentional edge case: if a human is fighting the loop over the branch, the loop stays out of their way.

### FR-4: Observability

**FR-4a.** Every `push_branch_best_effort` call emits a log line with level INFO on success, WARN on failure. Fields: `loop_id`, `branch`, `commit_sha`, `stage` (implement/revise/etc).

**FR-4b.** `nemo inspect <branch>` output gains a per-round `pushed` boolean (`true` if the round's commit reached origin, `false` if the push failed or didn't run). Surfaces to the engineer which rounds are recoverable from GitHub vs. only in the cluster.

**FR-4c.** Dashboard (#147 when it ships) surfaces the same signal in the loop detail page.

## Non-Functional Requirements

### NFR-1: Push cost

One push per commit × ~3 commits per round × 5-15 rounds per loop = 15-45 pushes per loop. Each push is a git protocol round-trip to GitHub, ~200-500ms on a healthy network. Loop wall-clock impact: negligible (pushes happen between stages, not within them).

### NFR-2: Rate limit

GitHub allows 5000+ git operations per hour per token. 45 pushes × 50 concurrent loops = 2250/hr, well under. Not an immediate concern; revisit at 100+ concurrent loops.

### NFR-3: No behavior change on the happy path

Once a loop converges, the branch tip on origin matches the bare repo's tip. The existing final push becomes a fast-forward no-op. Zero observable change for CONVERGED loops.

### NFR-4: Credential path

Push uses the existing git remote + GH_TOKEN already configured for the bare repo (same mechanism as the final convergence push). No new credential plumbing.

### NFR-5: Tests

- **Unit** (`control-plane/src/git/mod.rs`): `push_branch_best_effort` on a mock git that fails returns unit (doesn't panic or error).
- **Integration** (`control-plane/tests/push_during_implement.rs`): run a loop, simulate a mid-round failure via a mock dispatcher, assert the branch is on the mock remote with all commits through the last successful round.

## Acceptance Criteria

1. **Happy-path push per round**: start a loop, let it run 3 rounds, terminate (cancel) mid-round-4. Branch on GitHub is at round 3's tip. No manual push needed to salvage.
2. **Push failure is non-fatal**: inject a push failure (stop the network briefly, or revoke GH_TOKEN, then restore). Loop keeps running. Next round's push succeeds and catches up.
3. **Convergence path unchanged**: loop converges normally; branch on GitHub is at final tip; PR opens; no regression vs. today's behavior.
4. **`nemo inspect` shows push state**: after a round completes, `nemo inspect <branch>` shows `pushed: true` for that round. After a manually-induced push failure on round 5, inspect shows `pushed: false` for round 5 and `pushed: true` for round 6 (recovered).

## Out of Scope

- **Mirror pushes to a secondary remote** (backup GitHub, GitLab, local Gitea). Single-remote only.
- **Signing commits**. Loop commits aren't signed today; this spec doesn't change that.
- **Selective push based on commit content**. All agent commits push. No filtering.
- **Retry backoff on push failure**. Intentionally best-effort per FR-1c.
- **Atomic "push failure pauses the loop"**. Some operators might want this; not the default here because transient push errors are common and pausing for every one would thrash.
- **Force-push on non-fast-forward** (see FR-3b rationale).

## Files Likely Touched

- `control-plane/src/git/mod.rs` — add `push_branch_best_effort`; minor mod to existing `push_branch`.
- `control-plane/src/loop_engine/driver.rs` — add calls in `ingest_job_output`, `dispatch_implement_with_feedback`, `dispatch_revise`.
- `control-plane/src/types/api.rs` — extend `RoundSummary` with optional `pushed: bool`.
- `control-plane/src/state/postgres.rs` + migration — optional `pushed_at TIMESTAMPTZ` on `rounds` table; inspect handler reads it.
- `cli/src/commands/inspect.rs` — render push state per round.
- Tests per NFR-5.

## Baseline Branch

`main` at PR #163 merge.
