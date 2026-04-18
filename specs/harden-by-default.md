# Harden by Default

## Overview

Flip `nemo start` to harden the spec before implementation by default. Add `--no-harden` for explicit opt-out. Rationale: a hardened spec converges in 1-2 audit rounds (near-zero cost), while an unhardened spec causes 5+ wasted implement rounds when the reviewer invents findings against ambiguous requirements.

Today's dogfood session validated the cost: seven specs submitted without `--harden`, several needed post-submission amendments (helm-phase2 got FR-9+10 mid-implement, mobile-dashboard got FR-9-14 mid-implement, pluggable-cache was fully superseded), and loops hit 8-15 rounds of churn that a 2-round harden would have prevented.

## Baseline

Main at PR #150 merge.

Current behavior:
- `nemo start <spec>` → implement only; engineer is responsible for having hardened the spec beforehand.
- `nemo start <spec> --harden` → harden → AWAITING_APPROVAL gate → implement.
- `nemo harden <spec>` → harden-only, terminates at HARDENED.
- `nemo ship <spec>` → implement + auto-merge; `--harden` adds harden first.

Default `nemo start` = "trust my spec, go implement."

Proposed: `nemo start` = "harden first (fast if already clean), gate at AWAITING_APPROVAL so I can review the hardened spec, then implement." Opt out with `--no-harden` for the "I know what I'm doing" path.

## Problem Statement

### Problem 1: Engineer forgets the flag, wastes a loop

Today's session: seven `nemo start` invocations, zero with `--harden`. Every single spec went into implement without an audit pass. Multiple loops burned 5-15 rounds because the reviewer kept finding real ambiguity the auditor would have caught in round 1.

Engineers will not remember to type `--harden` every time. Even the person who wrote the feature forgot.

### Problem 2: The cost of harden on a good spec is negligible

Harden is 2-stage (audit + optional revise). When the spec is clean, audit returns `clean: true` in round 1 and the phase ends in ~30s — one model call, no compile, no push. The marginal cost of "accidentally" hardening a good spec is a rounding error.

### Problem 3: The cost of NOT hardening a soft spec is huge

A soft spec produces a reviewer that keeps inventing findings each round. At 5-15 implement rounds × 3 stages × ~$0.10/stage minimum, that's $1.50-$4.50 of wasted model spend per loop. Measured on today's loops, this is the dominant cost.

### Problem 4: Post-submission spec amendments poison the in-flight implementation

When an operator amends a spec mid-implement (as we did three times today), the in-flight loop keeps working on the pre-amendment version. Output diverges from current intent. Hardening before implement means the spec-drift-during-implement problem disappears for whole classes of edits.

## Functional Requirements

### FR-1: `nemo start` defaults to harden

**FR-1a.** Flip the default in `cli/src/main.rs` `Commands::Start`:

```rust
Start {
    spec_path: String,
    /// Skip the harden phase (audit + optional revise) before implement.
    /// Default: harden runs first. Use when you've already hardened the spec
    /// or when audit-in-the-loop is not wanted for this run.
    #[arg(long)]
    no_harden: bool,
    // ... rest unchanged
}
```

**FR-1b.** The existing `--harden` flag is kept as a no-op with a deprecation warning for one release cycle: `--harden is now the default; this flag has no effect`. Remove in the release after.

**FR-1c.** Control-plane `StartRequest.harden` flag semantic is unchanged — the CLI computes `harden = !no_harden` before sending. No API change.

### FR-2: Preserve the AWAITING_APPROVAL gate post-harden

**FR-2a.** After harden finishes (HARDENED state on the spec PR), the loop transitions to AWAITING_APPROVAL. Engineer reviews the hardened spec, runs `nemo approve <id>` (or taps approve on the dashboard), and implement begins.

**FR-2b.** The existing `--auto-approve` flag bypasses the gate. Engineers who want full autonomy chain the flags: `nemo start spec.md --auto-approve` → harden → auto-approve → implement.

**FR-2c.** `nemo ship` already auto-approves (its whole point). Unchanged.

### FR-3: Fast path for already-hardened specs

**FR-3a.** When the audit stage returns `clean: true` on round 1 AND the revise stage has not run, the harden phase emits the spec PR immediately. Engineer gets a notification like `Spec hardened in 1 round (no changes)` in the CLI output.

**FR-3b.** Optional spec frontmatter marker (informational only, not gating):

```markdown
---
nautiloop:
  hardened_at: <commit-sha-of-spec-on-main>
  hardened_model: claude-opus-4-6
  hardened_rounds: 1
---

# Spec title
...
```

Auto-added by the harden loop on the merged spec PR. Lets engineers see at a glance "this spec was hardened." Does NOT skip the harden phase — audit still runs — but the run is near-free if the marker is current.

**FR-3c.** Engineers can delete the marker to force a fresh harden. Normal audit behavior handles whether a re-harden finds anything.

### FR-4: Clear CLI output

**FR-4a.** `nemo start <spec>` default output:

```
Started loop 8cb88352...
  Spec:   specs/foo.md (local, 1,689 bytes)
  Branch: agent/dev/foo-abc123
  Phase:  HARDEN → AWAITING_APPROVAL → IMPLEMENT (add --no-harden to skip harden)
  State:  PENDING
```

The `--no-harden` hint surfaces the opt-out for engineers who want the old behavior.

**FR-4b.** `nemo start <spec> --no-harden` output:

```
Started loop 8cb88352...
  Spec:   specs/foo.md (local, 1,689 bytes)
  Branch: agent/dev/foo-abc123
  Phase:  IMPLEMENT (harden skipped)
  State:  PENDING
```

### FR-5: Docs + migration note

**FR-5a.** `docs/local-dev-quickstart.md` section "Your first loop" is updated to reflect the new default: the example shows `nemo start` without `--harden` and explains the harden phase will run first.

**FR-5b.** Release notes for the release containing this change include a prominent callout: `BREAKING (behavior): nemo start now hardens before implement. Add --no-harden for the prior behavior.`

## Non-Functional Requirements

### NFR-1: No server-side changes

The control plane keeps accepting the existing `StartRequest.harden` bool. CLI is where the default flips. Existing HTTP clients (CI scripts hitting the API directly) see no change.

### NFR-2: Backward-compat for CI scripts that use the CLI

CI automation calling `nemo start` will now auto-harden. If their specs are already hardened, audit returns clean instantly; marginal latency. If not, they catch real spec issues earlier (net win). If they truly want the old behavior, they add `--no-harden` to their scripts.

### NFR-3: Tests

- **Unit** (`cli/src/commands/start.rs`): default invocation sends `harden: true`; `--no-harden` sends `harden: false`; deprecated `--harden` sends `harden: true` with stderr warning.
- **Integration**: full harden → approval → implement cycle with default flags.

## Acceptance Criteria

1. `nemo start specs/foo.md` on an unhardened spec → runs harden, emits spec PR, transitions to AWAITING_APPROVAL.
2. `nemo start specs/foo.md` on a clean, already-hardened spec → harden converges in round 1, spec PR opens with no content changes, transitions to AWAITING_APPROVAL. Wall time ~60s.
3. `nemo start specs/foo.md --no-harden` → skips harden, transitions directly to IMPLEMENTING.
4. `nemo start specs/foo.md --harden` → works, emits deprecation warning, same behavior as default.
5. CLI output shows the phase plan (`HARDEN → AWAITING_APPROVAL → IMPLEMENT`) so engineers know what to expect.

## Out of Scope

- **Skipping harden based on the frontmatter marker** (FR-3b). Marker is informational only in v1. Skipping harden entirely based on a sha marker introduces freshness-check complexity (what if main moved?); not worth the complexity when a clean re-harden is ~60s.
- **Reverse default for `nemo harden`**. Harden-only is a distinct verb and stays harden-only. No changes.
- **Interactive prompting** (`Spec not hardened, run harden first? [Y/n]`). Harden-by-default makes the prompt unnecessary.
- **Changing `nemo ship` behavior**. Ship already supports `--harden`; leave it as an explicit flag there since ship-mode's auto-approve makes "harden then auto-approve then implement then auto-merge" a bigger leap than ship-mode operators might expect.

## Files Likely Touched

- `cli/src/main.rs` — flip default; add `--no-harden`; keep `--harden` as deprecated no-op.
- `cli/src/commands/start.rs` — update output strings to show phase plan.
- `docs/local-dev-quickstart.md` — update first-loop example.
- `docs/release-notes.md` (or equivalent) — prominent behavior-change callout.
- Tests per NFR-3.

## Baseline Branch

`main` at PR #150 merge.
