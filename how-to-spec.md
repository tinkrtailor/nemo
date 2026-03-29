# How to Write Specifications

A guide to crafting effective specs that guide AI agent implementation in the Cleared codebase.

## The Golden Rule: Conversation First

**Don't start coding. Start talking.**

The specs phase is a conversation with the LLM about what you want to build. You're shaping its understanding without asking it to implement anything yet.

```
┌─────────────────────────────────────────────────────┐
│  1. DESCRIBE    →  High-level vision               │
│  2. EXPAND      →  Details, edge cases, behavior   │
│  3. REFINE      →  Remove ambiguity, add clarity   │
│  4. VERIFY      →  LLM reviews its own specs       │
│  5. IMPLEMENT   →  Only now do you start coding    │
└─────────────────────────────────────────────────────┘
```

---

## Step 1: Describe Your Vision

Start with a high-level description. Be clear that you're NOT asking for implementation yet.

### The Initial Prompt

```markdown
We are going to build [feature name].

It will:

- [Core capability 1]
- [Core capability 2]
- [Core capability 3]

Relevant context:

- [Related ADRs, existing patterns, constraints]
- [Which packages/apps are affected]

IMPORTANT: Do NOT implement anything yet. Instead, write up the
specification into specs/[category]/[feature-name].md following
our spec templates. Update specs/SPECS.md to link the new spec.
```

### Why This Works

- **Sets scope** without micromanaging implementation
- **Establishes constraints** (affected packages, related ADRs)
- **Explicitly prevents** premature coding
- **Creates structure** for organized specs

---

## Step 2: Expand Through Conversation

Ask the LLM to expand on areas that need more detail.

### Expansion Prompts

**Add requirements:**

```markdown
Look at @specs/

New requirements:

- What error handling should we have?
- What edge cases should we handle?
- What are the API contracts (request/response)?

Update the spec with this guidance.
```

**Deepen a specific area:**

```markdown
Look at @specs/

The payment flow spec is too vague. Expand it:

- What happens when the payer's wallet has insufficient funds?
- How do we handle chain reorgs?
- What's the timeout for UserOp confirmation?

Update @specs/payments/[feature].md
```

**Add non-functional requirements:**

```markdown
Look at @specs/

Add performance requirements:

- What's the maximum acceptable latency?
- How should this work on mobile viewports?
- What loading states do we need?

Update the spec.
```

### Conversation Patterns

| Pattern                     | When to Use                   |
| --------------------------- | ----------------------------- |
| "What about X?"             | Discover missing requirements |
| "Expand on Y"               | Add detail to vague areas     |
| "What happens when Z?"      | Explore edge cases            |
| "How should we handle...?"  | Define error behavior         |
| "What are the constraints?" | Surface implicit assumptions  |

---

## Step 3: Refine for Clarity

Remove ambiguity. Replace vague language with specific, testable statements.

### Before and After

**Vague:**

```markdown
The system should handle errors appropriately.
```

**Specific:**

```markdown
Error Handling:

| Error                  | HTTP | Message                               | Recovery     |
| ---------------------- | ---- | ------------------------------------- | ------------ |
| Invoice not found      | 404  | "Invoice not found: {id}"             | Check ID     |
| Already paid           | 409  | "Invoice already paid"                | No action    |
| Insufficient allowance | 400  | "Insufficient USDC allowance"         | Re-approve   |
| UserOp reverted        | 500  | "Transaction failed: {revert reason}" | Retry button |
```

**Vague:**

```markdown
The page should be fast.
```

**Specific:**

```markdown
Performance Requirements:

- NFR-1: Page load time < 2 seconds
- NFR-2: Pagination loads next page in < 1 second
- NFR-3: No layout shift when data loads (use Skeleton components)
```

### Red Flags to Fix

| Vague Term          | Ask Instead                         |
| ------------------- | ----------------------------------- |
| "appropriate"       | What specifically?                  |
| "fast"              | What latency/throughput?            |
| "handle gracefully" | What error message? What HTTP code? |
| "support"           | What operations exactly?            |
| "various"           | Which ones specifically?            |
| "etc."              | List them all                       |

---

## Step 4: Verify with Self-Review

Ask the LLM to review its own specs for completeness.

### Review Prompts

**Completeness check:**

```markdown
Look at @specs/

Review these specifications:

- What is missing?
- What is ambiguous?
- What edge cases aren't covered?
- Are there contradictions between specs?

Update the specs to address any issues found.
```

**Consistency check:**

```markdown
Look at @specs/

Check for consistency:

- Do all specs use the same terminology? (See SPECS.md glossary)
- Are error handling patterns consistent across features?
- Do dependencies between features align?

Fix any inconsistencies found.
```

**Testability check:**

```markdown
Look at @specs/

For each requirement, ask: "How would I test this?"
If a requirement can't be tested, it's too vague.

Rewrite untestable requirements to be specific and verifiable.
```

---

## Spec File Types

Every feature starts with a **spec**. Complex features also get an **impl-plan**. Some get additional companion files.

| File Type                 | When to Create                                | Example                                      |
| ------------------------- | --------------------------------------------- | -------------------------------------------- |
| `feature.md`              | Always — every piece of work needs a spec     | `ux/dashboard-improvements.md`               |
| `feature-impl-plan.md`    | Complex features (3+ steps), AI agent specs   | `ux/dashboard-improvements-impl-plan.md`     |
| `feature-design-notes.md` | When design decisions need separate rationale | `mcp/api-key-design-notes.md`                |
| `feature-report.md`       | Audit/review output (generated deliverable)   | `security/contract-security-audit-report.md` |

---

## Feature Spec Template

````markdown
# Feature Name

## Overview

Brief description (1-2 sentences) of what this feature does and why it exists.

## Dependencies

- **Requires:** [other spec](../category/other-spec.md)
- **Required by:** [downstream spec]

## Requirements

### Functional Requirements

- FR-1: The system shall [specific, testable behavior]
- FR-2: The system shall [specific, testable behavior]

### Non-Functional Requirements

- NFR-1: [Performance/security/usability requirement with number]

## Behavior

### Normal Flow

1. User does X
2. System responds with Y
3. Result is Z

### Alternative Flows

- If [condition], then [behavior]

## Edge Cases

| Scenario         | Expected Behavior        |
| ---------------- | ------------------------ |
| Empty input      | Return empty output      |
| Invalid input    | Error with message "..." |
| Very large input | Paginate, max N per page |

## Error Handling

| Error        | Code | Message                   | Recovery |
| ------------ | ---- | ------------------------- | -------- |
| Not found    | 404  | "Not found: {id}"         | Check ID |
| Unauthorized | 401  | "Authentication required" | Re-login |

## API Changes

### `POST /api/v1/resource`

**Request:**

```json
{ "field": "value" }
```
````

**Response (200):**

```json
{ "id": "uuid", "status": "created" }
```

## UI Components

(ASCII mockups or wireframe descriptions)

## Out of Scope

- [Exclusion 1]
- [Exclusion 2]

## Acceptance Criteria

- [ ] Criterion that can be checked
- [ ] Another criterion

## Open Questions

- [ ] Question that needs answering before implementation

````

---

## Implementation Plan Template

Impl-plans are created when a spec is ready for implementation. They contain codebase analysis and step-by-step instructions for the implementing agent.

```markdown
# Implementation Plan: Feature Name

**Spec:** `specs/category/feature-name.md`
**Branch:** `feat/feature-name`
**Status:** Pending | In Progress | Complete
**Created:** YYYY-MM-DD

## Codebase Analysis

### Existing Implementations Found

| Component       | Location                                    | Status    |
| --------------- | ------------------------------------------- | --------- |
| Related hook    | `apps/web/src/hooks/example/useExample.ts`  | Complete  |
| API endpoint    | `apps/api/src/routes/v1/resource.ts`        | Needs mod |

### Patterns to Follow

| Pattern           | Location                          | Description                  |
| ----------------- | --------------------------------- | ---------------------------- |
| React Query hooks | `apps/web/src/hooks/.../useX.ts`  | Query with auth, pagination  |
| API routes        | `apps/api/src/routes/v1/...`      | Zod validation, auth middle  |

### Files to Modify

| File   | Change                        |
| ------ | ----------------------------- |
| `path` | Description of modification   |

### Files to Create

| File   | Purpose                       |
| ------ | ----------------------------- |
| `path` | What this new file does       |

### Risks & Considerations

1. Risk description and mitigation

## Plan

### Step 1: [Title]

**Why this first:** [Reason for priority]
**Files:** `path/to/file.ts`
**Approach:** Description of what to do
**Tests:** What tests to write/run
**Depends on:** nothing | Step N
**Blocks:** Step N

### Step 2: [Title]

(Repeat for each step)

## Acceptance Criteria Status

| Criterion           | Status |
| ------------------- | ------ |
| From the spec       | ⬜     |

## Progress Log

| Date | Step | Status | Notes |
| ---- | ---- | ------ | ----- |
````

---

## Variant: Fix / Operational Specs

For **bug fixes, config changes, hardening tasks, and operational runbooks**, use this lighter variant.

### When to Use This Variant

| Use feature spec template          | Use fix/ops spec template     |
| ---------------------------------- | ----------------------------- |
| New user-facing feature            | Bug fix across multiple files |
| Complex flows with branching logic | Config/hardening changes      |
| Needs UX wireframes or mockups     | Deployment runbooks           |
| Has alternative user flows         | Migration or upgrade tasks    |
| Requires product decisions         | Agent-delegatable code fixes  |

### Fix Spec Template

````markdown
# Title

> **Type:** AI agent | Human guide
> **Priority:** Critical | High | Medium
> **Branch:** `fix/short-name`

## Overview

1-2 sentences: what's broken/missing and why it matters.

## Requirements

### FR-1: Short imperative title

**File:** `path/to/file.ts:LINE`

Current (BROKEN/MISSING):

```typescript
// exact current code
```
````

**Required change:** Precise description of the fix. Include target code
if the change is mechanical. Include the "why" if not obvious.

### FR-2: ...

(Repeat for each discrete fix. One FR per logical change.)

### NFR-1: No regression

All existing tests must pass. Run `make ci` before opening PR.

## Edge Cases

| Scenario | Expected Behavior |
| -------- | ----------------- |
| ...      | ...               |

## Out of Scope

- Things the agent should NOT touch (prevents scope creep)

## Verification

```bash
# Runnable commands that prove the fix worked
grep -rn "broken_pattern" path/
# Expected: 0 results

make ci
```

## Dependencies

- **Requires:** other specs or nothing
- **Required by:** downstream specs

````

### Key Differences from Feature Specs

| Feature spec                              | Fix/ops spec                                    |
| ----------------------------------------- | ----------------------------------------------- |
| Abstract requirements ("system shall...") | Exact file:line references with current code    |
| Normal/Alternative flows                  | No flows — just "current → required change"     |
| Acceptance criteria checkboxes            | Runnable shell commands in Verification section |
| UX mockups, wireframes                    | N/A                                             |
| Open questions section                    | Omit if none (don't pad)                        |
| Behavior section                          | Replaced by precise FR diffs                    |

### Metadata Header

Always start fix specs with a blockquote header:

```markdown
> **Type:** AI agent | Human guide
> **Priority:** Critical | High | Medium
> **Branch:** `fix/short-name`
````

This tells the reader at a glance:

- **Type** — who executes this (agent in worktree, or human following steps)
- **Priority** — triage order when multiple specs compete for attention
- **Branch** — the git branch name to use (agents create this automatically)

---

## Writing for AI Agents

When a spec will be handed to an AI agent:

1. **Include exact file paths and line numbers** — saves the agent from searching
2. **Show the current broken code** — so the agent can verify it's editing the right thing
3. **Show the correct pattern if one exists elsewhere** — "replicate the pattern from `file.ts:115-124`"
4. **Reference ADRs** — link to relevant `docs/adr/ADR-NNN-*.md` in preamble
5. **List what to grep for** in Verification — agents can run these to self-check
6. **Be explicit about Out of Scope** — agents will try to fix adjacent issues if you don't tell them not to

### ADR Reference Block

For implementation-heavy specs, include ADR references in the overview:

```markdown
> **BEFORE IMPLEMENTING: Review Architecture Decisions**
>
> | ADR                                                          | Why It Matters                            |
> | ------------------------------------------------------------ | ----------------------------------------- |
> | [ADR-024](../../docs/adr/ADR-024-prepare-submit-userop.md)   | Prepare/submit UserOp pattern             |
> | [ADR-020](../../docs/adr/ADR-020-single-source-addresses.md) | Address resolution via @cleared/addresses |
```

## Writing for Humans (Runbooks)

When a spec is a guide for human execution:

1. **Use numbered checklists** with `- [ ]` checkboxes
2. **Include exact commands** to copy-paste (no pseudocode)
3. **Add a "Pre-requisites" section** listing what must be true before starting
4. **Add a "Rollback Plan"** for anything that touches production
5. **Include a timeline estimate** for each phase

---

## Verification & Acceptance

Specs track verification through multiple mechanisms (no external task files needed):

| Mechanism                      | Where                     | Purpose                               |
| ------------------------------ | ------------------------- | ------------------------------------- |
| **Acceptance Criteria**        | Feature spec (checkboxes) | What "done" looks like                |
| **Acceptance Criteria Status** | Impl-plan (table)         | Track progress during implementation  |
| **Verification commands**      | Fix spec (bash block)     | Runnable proof the fix worked         |
| **Progress Log**               | Impl-plan (table)         | Chronological record with commit refs |
| **`make ci`**                  | Always, before PR         | Automated regression check            |

### Writing Good Acceptance Criteria

Good criteria are specific, testable, and independent:

| Bad               | Good                                                        |
| ----------------- | ----------------------------------------------------------- |
| "Login works"     | "POST /api/v1/login with valid Privy JWT returns 200"       |
| "Handles errors"  | "Invalid invoice ID returns 404 with `InvoiceNotFound` msg" |
| "Is fast"         | "Dashboard page loads in < 2 seconds"                       |
| "Validates input" | "Amount=0 returns 400 with ZeroAmount error"                |

---

## Multi-Spec Consistency

When a system is defined across multiple specs, cross-spec consistency becomes the dominant source of implementation bugs. These lessons were learned during spec hardening of a multi-lane system.

### 1. Shared contracts must be defined once

When multiple specs share a data structure (state enum, output schema, verdict format, job naming), define it in ONE spec and have others reference it. Don't copy the definition -- reference it. Example: "See Lane A SS Review Verdict Schema for the canonical definition."

If you duplicate a definition, it will drift. When it drifts, the implementer builds to whichever copy they find first, and the integration fails.

### 2. Cross-spec review is mandatory

After writing related specs, do a cross-spec consistency review. Check: do all specs agree on field names, enum values, error handling paths, and data flow boundaries? A checklist:

- Field names match exactly (not `affected_services` in one spec and `services` in another)
- Enum values are identical strings (not `implementing` vs `implement` vs `impl`)
- Error handling paths converge to the same terminal states
- Data flow inputs/outputs match at every boundary

### 3. Stage boundaries are contracts

The input/output format between stages (implement -> test -> review) is a contract. Define it once with exact field names, types, and examples. Every spec that touches a stage boundary must reference the same contract.

If Lane A says the implement stage outputs `{ new_sha, token_usage }` and Lane C says it outputs `{ sha, tokens }`, the integration fails silently.

### 4. Mount modes constrain outputs

If a stage mounts the worktree read-only, it cannot write output files to the worktree. Output must go to stdout or a separate writable volume. Check mount modes against output requirements for every stage.

Example mistake: spec says "review agent writes verdict to `.agent/review-verdict.json` in the worktree" but the review stage mounts the worktree read-only. The actual output contract is stdout via a prefix line.

### 5. Naming conventions must be explicit

If the DB stores `implementing` but the API uses `implement` and the job name uses `impl`, state the mapping once and reference it everywhere. Create a "Stage Name Mapping" section that is identical across all specs (or defined in one and referenced by others).

Without an explicit mapping table, each spec invents its own convention and they diverge.

### 6. Retry/error models must be unified

One retry model, one error classification, shared across all specs. If Spec A says "2 retries" and Spec C says "1 retry", the implementer can't build it. Define the retry budget, backoff intervals, and failure classification in one place. Other specs reference it.

This applies to all cross-cutting concerns: timeouts, retry counts, error codes, log formats, and credential handling.

---

## Common Mistakes

### 1. Too Vague

**Problem:**

```markdown
The system should handle invoice payments.
```

**Fix:** Break down into specific behaviors:

```markdown
## Payment Flow

### FR-1: Pay invoice

- Payer calls `payInvoice(invoiceId)` on InvoiceRegistry
- Contract transfers USDC from payer to issuer's Safe (minus fee)
- Fee = min(2% × amount, $50 USDC) sent to treasury
- Invoice status transitions: Issued → Paid (terminal)

### FR-2: Reject invalid payments

- Already-paid invoice: revert with `InvoiceAlreadyPaid(invoiceId)`
- Canceled invoice: revert with `InvalidStatus(invoiceId, current, expected)`
- Insufficient allowance: revert (SafeERC20 handles this)
```

### 2. Too Detailed (Implementation Leakage)

**Problem:**

```markdown
Use a Map<string, Invoice> in the React hook, iterate with
.entries() and filter with .filter() to find pending invoices.
```

**Fix:** Describe behavior, not implementation:

```markdown
Dashboard displays pending invoices filtered from the user's invoice list.
Invoices load via React Query with cursor-based pagination.
```

### 3. Missing Edge Cases

**Problem:**

```markdown
## Withdraw Funds

User withdraws USDC from their Safe.
```

**Fix:** Enumerate edge cases:

```markdown
## Edge Cases

| Scenario                   | Expected Behavior                           |
| -------------------------- | ------------------------------------------- |
| Balance = 0                | Disable button, show "No funds to withdraw" |
| Amount > balance           | Validation error before submission          |
| Withdrawal address not set | Redirect to Settings with prompt            |
| UserOp reverts on-chain    | Show error, allow retry                     |
| Gas estimation fails       | Show error with "try again later"           |
| Multiple rapid submissions | Debounce, disable button during pending tx  |
```

### 4. Contradictions

**Problem:**

```markdown
# In payments/payment-flow.md:

All payments go through the InvoiceRegistry contract.

# In funds/withdraw.md:

Withdrawals transfer directly from Safe to recipient.
```

**Fix:** Both are correct — payments and withdrawals are different flows. Make this explicit in each spec to avoid confusion.

### 5. Scope Creep

**Problem:** Adding features during implementation because "it would be nice."

**Fix:** Specs define scope. If it's not in specs, it's not in scope.

```markdown
## Out of Scope

- Multi-currency support (future — USDC only for MVP)
- Partial payments
- Recurring invoices
- Invoice templates
```

---

## When Are Specs "Done"?

Specs are ready for implementation when:

| Criterion       | Check                            |
| --------------- | -------------------------------- |
| **Complete**    | All features have a spec         |
| **Unambiguous** | No vague language                |
| **Consistent**  | No contradictions                |
| **Testable**    | Each requirement can be verified |
| **Scoped**      | Clear in/out of scope            |
| **Reviewed**    | LLM has self-reviewed for gaps   |

### Final Checklist

```markdown
- [ ] Spec file exists in specs/[category]/
- [ ] SPECS.md updated with link to new spec
- [ ] All specs follow the template structure
- [ ] No "appropriate", "fast", "various", etc.
- [ ] Edge cases documented for each feature
- [ ] Error handling defined with codes and messages
- [ ] Out of scope section present
- [ ] Dependencies between specs documented
- [ ] ADR references included where relevant
- [ ] LLM has reviewed specs for gaps
```

---

## Example: Full Spec Conversation

### Turn 1: Initial Vision

```
User: We want to add an Agent Dashboard page where users can manage
their API integrations — see which agents have access, grant new access,
revoke credentials.

It should:
- Show a list of named agents with status indicators
- Allow creating new agents (name, description, icon, scopes)
- Allow disabling/enabling agents
- Show last activity for each agent

Relevant: specs/mcp/api-key-provisioning.md is implemented.
Reference: docs/adr/ADR-024 for UserOp patterns.

IMPORTANT: Don't implement yet. Write specs to specs/agents/agent-dashboard.md
```

### Turn 2: Expand Details

```
User: Look at @specs/

Expand the agent dashboard spec:
- What's the data model? Do agents get their own table or reuse api_keys?
- What API endpoints do we need?
- What does the create agent flow look like step by step?
```

### Turn 3: Add Edge Cases

```
User: Look at @specs/

For the agent management spec, add:
- What happens when disabling an agent with active sessions?
- Can an agent be deleted or only disabled?
- What if the user has no agents yet?
- Maximum number of agents per studio?
```

### Turn 4: Review

```
User: Look at @specs/

Review for completeness:
- What's missing?
- What's ambiguous?
- Any contradictions with the existing API key specs?

Update specs to fix issues.
```

### Turn 5: Create Implementation Plan

```
User: Specs look good.

Create an impl-plan at specs/agents/agent-dashboard-impl-plan.md.
Analyze the codebase for existing patterns, list files to modify/create,
and break it into priority-ordered steps.
```

---

## Quick Reference

### Expansion Questions

- "What happens when...?"
- "What if the user...?"
- "How should we handle...?"
- "What are the limits for...?"
- "What error should...?"

### Clarity Triggers

Replace these words with specifics:

- appropriate → [exact behavior]
- fast → [latency/throughput number]
- handle → [specific action]
- support → [list of operations]
- various → [exhaustive list]

### Review Questions

- Can each requirement be tested?
- Are there contradictions?
- What's missing?
- What's ambiguous?
- Is scope clear?
