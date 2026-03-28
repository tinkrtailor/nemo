# Adversarial Review: Round 15 — MAX ROUNDS (OpenCode GPT-5.4, read-only)

MAX_ROUNDS_EXCEEDED. 2 remaining findings. Per spec: create PR with NEEDS_HUMAN_REVIEW.

## REMAINING FINDINGS

N55. **HIGH** - Approve/resume/cancel flags cleared before follow-up transition succeeds. If redispatch fails transiently, the flag is consumed and the loop is stuck. User must re-issue the command (driver.rs:648, 662, 685, 713). Fix: clear flags AFTER the transition succeeds, not before. Or use a transaction that clears the flag and performs the transition atomically.

N56. **HIGH** - .agent/ artifacts (feedback files, verdicts) committed to the working branch and included in PRs. No cleanup before PR creation (driver.rs:919, git/mod.rs:158, driver.rs:271, 273, 325, 521). Fix: git rm .agent/ before creating the PR, or use .gitignore, or store artifacts on a separate PVC instead of committing them.

## STATUS

MAX_ROUNDS_EXCEEDED (15/15). These 2 findings should be fixed in a follow-up PR or addressed during human review of the PR.
