# V2: DAG-Based Spec Splitting & Parallel Execution

## Overview

The spec hardening loop becomes a planner that analyzes spec scope and automatically splits large specs into a DAG of sub-specs. Each sub-spec targets 500-2K lines of implementation, convergeable in 3-5 adversarial review rounds. Sub-specs with no shared modules run in parallel. The system executes the DAG, merges results, and produces an integration PR.

This is the evolution from V1's single convergent loop to V2's parallel convergent DAG.

## Problem Statement

From dogfooding V1:
- Lane A (10K lines, unhardened spec): 28 rounds, 124 findings
- Lane B (~5K lines, hardened spec): 12+ rounds, ~57 findings
- Lane C (~8K lines, hardened spec): 12+ rounds, ~83 findings

Convergence rounds scale with diff size. The reviewer has more surface to attack on large diffs. Each round finds 2-8 new issues regardless of round number, creating a long tail.

**The fix is not better reviewing. The fix is smaller diffs.**

A 10K line spec split into 5 x 2K sub-specs, each converging in 5 rounds, completes in 25 total review rounds (5 parallel x 5 sequential) vs 28 sequential rounds. But the calendar time is 5 rounds (parallel), not 28 (sequential). That's a 5.6x speedup.

## Dependencies

- **Requires:** V1 convergent loop (Lane A), spec hardening loop
- **Enables:** Multi-implementer racing (V2+), model-role optimization

## Requirements

### Functional Requirements

- FR-1: The spec audit prompt shall include a scope analysis section that estimates expected diff size from the spec
- FR-2: If estimated diff exceeds `max_estimated_diff_lines` (default: 2000, configurable in nemo.toml), the auditor shall FLAG the spec as "scope too large for single convergent loop"
- FR-3: The auditor shall propose a split into 2-6 sub-specs, each targeting 500-2000 lines
- FR-4: For each proposed sub-spec, the auditor shall specify: name, scope description, estimated line count, list of modules/directories touched, and dependencies on other sub-specs
- FR-5: Sub-specs with no shared modules shall be marked as parallelizable
- FR-6: The engineer can accept the split (proceeds with DAG) or reject it (proceeds with single loop, accepting slower convergence)
- FR-7: `nemo start` with an accepted split creates one convergent loop per sub-spec, orchestrated as a DAG
- FR-8: Parallel sub-spec loops run simultaneously on separate branches
- FR-9: Sequential sub-spec loops wait for their dependencies to converge before starting
- FR-10: When all sub-specs converge, the system merges all sub-spec branches into a single integration branch
- FR-11: An integration test round runs on the merged branch (all tests, full review) to catch cross-sub-spec issues
- FR-12: If integration review finds issues, affected sub-spec(s) are identified and their loops are re-entered with the integration feedback
- FR-13: `nemo status` shows the DAG with per-sub-spec progress

### Non-Functional Requirements

- NFR-1: DAG execution shall not require more cluster resources than running the sub-specs sequentially would (just scheduled differently)
- NFR-2: A DAG of 5 sub-specs with 3 parallel lanes shall complete in <2x the time of the longest single lane (overhead for merge + integration review)

## Architecture

```
SINGLE SPEC (V1):
  spec.md ──> [convergent loop] ──> PR
                 28 rounds for 10K lines

DAG (V2):
  spec.md ──> [harden + split] ──> DAG:
                                     ├── sub-spec-a.md ──> [loop] ──> branch-a (5 rounds)
                                     ├── sub-spec-b.md ──> [loop] ──> branch-b (5 rounds)  } parallel
                                     ├── sub-spec-c.md ──> [loop] ──> branch-c (5 rounds)  }
                                     │                        │
                                     │                   (c depends on a)
                                     │
                                     └── [merge a+b+c] ──> [integration review] ──> PR
```

## Spec Audit Additions

The default `spec-audit.md` prompt template gains:

```
SCOPE ANALYSIS (required for every audit):

1. Estimate the implementation scope:
   - Number of new/modified files
   - Estimated lines of new code
   - Number of new modules/services/components

2. If estimated lines > 2000:
   FLAG: "This spec is too large for a single convergent loop.
   Recommended: split into sub-specs for faster convergence."

3. Proposed split (if flagged):
   For each sub-spec:
   - Name: descriptive slug
   - Scope: what it implements (1-2 sentences)
   - Estimated lines: target 500-2000
   - Modules touched: directories/files
   - Dependencies: which other sub-specs must complete first
   - Parallelizable: yes/no (no shared modules with concurrent sub-specs)

4. DAG visualization:
   Show which sub-specs can run in parallel and which are sequential.
```

## State Machine Extension

```
PENDING ──> SPLITTING ──> SUB_SPECS_RUNNING ──> INTEGRATING ──> CONVERGED/SHIPPED
                              │
                    ┌─────────┼─────────┐
                    │         │         │
               [sub-a]   [sub-b]   [sub-c]
               (each is a V1 convergent loop)
```

New states:
- SPLITTING: spec is being analyzed and split by the hardener
- SUB_SPECS_RUNNING: one or more sub-spec loops are active
- INTEGRATING: all sub-specs converged, running integration review on merged branch

## Database Schema Extension

```sql
-- Parent loop tracks the DAG
ALTER TABLE loops ADD COLUMN parent_loop_id UUID REFERENCES loops(id);
ALTER TABLE loops ADD COLUMN sub_spec_name TEXT;
ALTER TABLE loops ADD COLUMN dag_position INTEGER; -- ordering within the DAG

-- DAG edges
CREATE TABLE loop_dependencies (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    loop_id UUID NOT NULL REFERENCES loops(id) ON DELETE CASCADE,
    depends_on_loop_id UUID NOT NULL REFERENCES loops(id) ON DELETE CASCADE,
    UNIQUE(loop_id, depends_on_loop_id)
);
```

## Configuration

```toml
# nemo.toml
[scope]
max_estimated_diff_lines = 2000    # flag specs above this
auto_split = false                  # V2: auto-accept splits (default: ask engineer)
max_sub_specs = 6                   # cap on number of sub-specs per DAG
```

## CLI Extension

```
nemo start spec.md
  # If spec is flagged for splitting during harden:
  # "This spec is estimated at ~8000 lines. Split into 4 sub-specs?"
  # [Y] Accept split → DAG execution
  # [N] Proceed as single loop (slower convergence expected)

nemo status
  ENGINEER  LOOP              SUB-SPEC        STAGE        ROUND  STATUS
  alice     feat/payment-flow  (parent)        SPLITTING     -     running
  alice     feat/payment-flow  schema          IMPLEMENTING  2     running
  alice     feat/payment-flow  api-endpoints   IMPLEMENTING  1     running
  alice     feat/payment-flow  cli-commands    PENDING       -     waiting on: schema
  alice     feat/payment-flow  (integration)   PENDING       -     waiting on: all
```

## Convergence Data (from dogfooding)

| Diff size | Expected rounds | With DAG split |
|-----------|----------------|----------------|
| 500-1K    | 3-5            | N/A (no split) |
| 1K-2K     | 5-8            | N/A (borderline) |
| 2K-5K     | 8-15           | 2-3 sub-specs, 5 rounds each |
| 5K-10K    | 15-28          | 3-5 sub-specs, 5 rounds each |
| 10K+      | 25+            | 5-6 sub-specs, 5 rounds each |

Calendar time with DAG (parallel execution):
- 10K line spec, single loop: ~28 rounds x 8 min = ~3.7 hours
- 10K line spec, 5 sub-specs (3 parallel lanes): ~5 rounds x 8 min x 2 (merge + integration) = ~1.3 hours

**2.8x speedup from parallelization alone.**

## Open Questions

1. How does the integration review handle cross-sub-spec bugs? Does it re-enter specific sub-spec loops or fix directly on the integration branch?
2. Should sub-specs share session context (so the implementer in sub-spec-c knows what sub-spec-a built)?
3. How are merge conflicts between parallel sub-specs resolved?

## Out of Scope (V3+)

- Multiple implementers racing on the same sub-spec
- Multiple reviewers with a judge selecting the best patch
- Model-role optimization (learning which model pairs converge fastest per stack)
- Self-improving prompts (review feedback → better implement prompts)
