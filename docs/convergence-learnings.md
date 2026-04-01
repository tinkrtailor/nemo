# Convergence Loop Learnings

Observations from dogfooding the Nautiloop convergent review loop by hand.
Lane A implementation: Claude (Opus 4.6) implements, OpenCode (GPT-5.4) reviews.
March 28, 2026.

## Convergence Curve

```
Round  Findings  Character
─────  ────────  ─────────
  1      14      Structural: mock stores in prod, no auth, no output ingestion
  2       7      Missed fixes + cascading issues from round 1 fixes
  3       4      Narrowing: harden path, resume semantics
  4       8      Edge cases surfaced: dead config, CLI/API mismatches
  5       5      Config loading, TLS, cleanup paths
  6       4      Stale HEAD, CI polling, branch reuse
  7       5      Open PR detection, push before PR, feedback context loss
  8       3      Branch base ref, credential content, feedback file type
  9       6      CRITICAL found at round 9 (harden_only flag bug, present since round 1)
 10       5      Fatal error handling, auth error detection, git identity
 11       5      Per-stage retry, K8s message parsing, container git identity
 12       3      Branch sanitization, inspect path, credential storage
 13       2      Revise output parsing, SSE cursor monotonicity
 14       3      spec_path persistence, SSE memory leak, harden terminal state
 15       2      max_rounds hit — engineer override to continue
 16       2      Concurrent /start race, empty cleanup commits
 17       3      PR idempotency, AWAITING_REAUTH cred mounting, branch collision
 18       6      Spike: cancel overwrites terminal, resume re-pauses, error masking
 19       5      Create-before-persist pattern (8 call sites), feedback files
 20       1      SIGTERM handling for K8s graceful shutdown
 21       3      Serial reconciler blocks on CI, ci_status edge cases, env var sanitization
 22       2      Auth exit code detection via pod termination state, remote branch cleanup
 23       7      Spike: ignored delete_job() failures (ghost jobs), stale update_loop
 24       1      Divergence check on completed jobs
 25       1      AWAITING_REAUTH SHA refresh (mirror of PAUSED fix)
 26       1      ci_status false positive on gh auth/network errors
 27       3      Temp worktree leak, ci_status error classification, Job TTL cleanup
 28       0      CLEAN — CONVERGED ✓
 ─────────────
 Total: 124 findings across 28 rounds
 Implementer: Claude (Opus 4.6)
 Reviewer: OpenCode (GPT-5.4)
 Time: ~4 hours (including fix time)
 Final: 50 tests, zero clippy warnings
```

## Key Product Learnings

### 1. Spec quality determines convergence speed

Round 1 had 3 CRITICALs that were clear spec violations (mock stores, no auth, no
output ingestion). These cascaded into every subsequent round. A tighter spec or a
better implement prompt ("follow the spec exactly, no mock implementations") would
have eliminated rounds 1-3 entirely.

**Action for Nautiloop:** The harden loop is the leverage point. More adversarial spec
hardening = fewer implementation rounds. The default implement prompt template
(.nautiloop/prompts/implement.md) must explicitly prohibit mock/placeholder implementations.

### 2. The reviewer never converges to zero on a large diff

With ~10K lines of new code, each round finds 2-5 new edge cases. The reviewer
is not repeating itself; it's finding genuinely new things each pass. This suggests
that for large implementations, a max_rounds safety valve is necessary.

**Action for Nautiloop:** max_rounds_implement = 15 is validated as the right default.
Consider: round threshold for auto-merge (ship mode) should be lower (5) to only
auto-merge confident results.

### 3. Late-round CRITICALs are real

Round 9 found a CRITICAL (harden_only never sets the harden flag) that was present
since the initial implementation. Earlier rounds missed it because they were focused
on more obvious structural issues. Different angles each round catch different things.

**Action for Nautiloop:** Don't assume early rounds catch all CRITICALs. The value of
persistent review compounds over rounds. This validates the "exit on clean verdict,
not fixed iteration count" design.

### 4. Finding character shifts over time

```
Rounds 1-3:   Structural bugs (architecture, missing features)
Rounds 4-7:   Edge cases (error paths, CLI/API contract mismatches)
Rounds 8-11:  Integration bugs (git identity, credential handling, state persistence)
Rounds 12+:   Subtle correctness (cursor monotonicity, memory leaks, terminal state semantics)
```

**Action for Nautiloop:** The review prompt could be stage-aware. Early rounds: "focus on
architecture and spec compliance." Later rounds: "focus on edge cases, state
persistence, and resource leaks."

### 5. The reviewer must be read-only

Round 12 (first attempt): OpenCode went agentic, modified 20 files with 383 insertions,
ran tests, and committed nothing. It found and fixed bugs in the same pass but:
- Fixes were entangled with the verdict (no clear separation)
- The implementer's context was broken (unexpected file changes)
- No traceability (which changes were fixes vs. refactors?)

Round 12 (re-run, read-only): Clean verdict with 3 specific findings. Traceable.

**Action for Nautiloop:** Review pods MUST mount the worktree read-only. The verdict
is a JSON file, not code changes. This is already in the spec (Lane C) but this
incident validates WHY. The permission config for the reviewer: `"permission":
{ "edit": "deny", "bash": "deny", "read": "allow" }`.

### 6. Feedback file format matters

Multiple rounds found bugs in how feedback is passed between stages:
- Round 2: Test failures not passed as feedback at all
- Round 7: Resume drops feedback context
- Round 8: Wrong feedback file type (review vs test)
- Round 13: Revise output not parsed (spec path stale)

**Action for Nautiloop:** The feedback file schema needs to be a first-class contract,
not an afterthought. The control plane should validate feedback files before
dispatching the next stage.

### 7. Retry semantics are surprisingly complex

- Round 4: Retry count per-loop not per-stage
- Round 7: Job name collision on retry (deterministic names)
- Round 10: Fatal vs retryable error classification
- Round 11: Per-stage retry reset on transition

**Action for Nautiloop:** Retry logic should be a dedicated module, not scattered
across the driver. The retry model spec (from eng review) was correct but the
implementation spread it across too many functions.

### 8. Git operations are the hardest part

More findings related to git than any other subsystem:
- Branch creation from wrong ref (HEAD vs origin/main)
- Worktree commit without git identity
- Branch force-reset into open PRs
- PR creation without pushing first
- Spec path stale after revise renames
- Branch name sanitization

**Action for Nautiloop:** The git module needs the most test coverage. Consider
integration tests against a real git repo (not mocks) as a priority.

### 9. The implement -> review latency is acceptable

Each round: ~3-5 min Claude fixing, ~2-3 min OpenCode reviewing. Total ~6-8 min
per round. 14 rounds = ~90-110 min for a 10K line implementation to go from
"compiles and passes basic tests" to "84 edge cases caught and fixed."

For comparison, a human code review cycle (submit PR, reviewer looks at it next
day, back and forth) takes days for this volume of feedback. The convergent loop
compresses review cycles from days to minutes.

### 10. max_rounds is a suggestion, not a wall

At round 15 (the default max_rounds_implement), the reviewer still found 2 real bugs
(flag clearing order, .agent artifacts in PRs). The engineer chose to keep going.
Round 16 found 2 more (concurrent /start race, empty commit on cleanup).

The system should support: `nemo extend <id> --rounds 5` to add more rounds past
the default limit. max_rounds is a safety valve for autonomous operation, but when
a human is watching, they should be able to override it.

**Action for Nautiloop:** Add `nemo extend` command. Default behavior (auto-stop at
max_rounds with NEEDS_HUMAN_REVIEW) stays the same. The extend command resets
the round budget.

### 11. Concurrent access patterns surface late

Round 16 found a race condition (concurrent /start destroys another request's branch)
that no prior round caught. Concurrency bugs require the reviewer to think about
multi-request scenarios, which only happens when the single-request bugs are fixed.

**Action for Nautiloop:** Consider a dedicated "concurrency review" prompt for later rounds
that specifically asks: "what happens if two requests hit this endpoint simultaneously?"

### 12. Final convergence data across all lanes

```
Lane A: 28 rounds, 124 findings (unhardened spec, 10K lines)
Lane B: 25 rounds,  88 findings (hardened spec, ~5K lines)
Lane C: 21 rounds, 107 findings (hardened spec, ~8K lines)
Integration: 7 rounds, 12 findings (cross-lane merge)

Total: 81 rounds, 331 findings, 106 tests, zero clippy warnings
```

Spec hardening cut Lane B/C convergence rounds vs Lane A. But the
biggest win would come from smaller specs (V2 DAG splitting). A 10K
line spec takes 28 rounds. Five 2K specs would take ~5 rounds each,
parallel, completing in ~5 rounds wall time. 5.6x speedup.

### 13. The product built itself

Nautiloop was built through the exact process it automates:
- Specs hardened through adversarial review (11 rounds)
- Implementation via Claude Code in sandboxed worktrees
- Cross-model adversarial review via OpenCode GPT-5.4
- Convergent loop: implement → review → fix → re-review → until CLEAN
- Three parallel lanes on separate git branches
- Integration review after merge

331 production bugs caught that would have shipped without the loop.
The loop works. Now automate it.

## Metrics for Nautiloop Dashboard

Track these per loop:
- Total rounds to convergence
- Findings per round (the curve shape)
- Finding severity distribution per round
- Time per round (implement + review)
- Total findings caught
- Round where last CRITICAL was found
- Whether max_rounds was hit

These metrics feed into:
- Prompt template optimization (which prompts produce fewer round-1 issues?)
- Model pairing optimization (which model pairs converge fastest?)
- Spec quality scoring (which specs converge in fewer rounds?)
