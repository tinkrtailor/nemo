# Adversarial Review: Round 17 (OpenCode GPT-5.4, read-only)

3 findings.

## FINDINGS

N59. **HIGH** - PR creation not idempotent. If anything after create_pr() fails, next tick retries gh pr create, gets "PR already exists", treats as retryable error, loops forever (driver.rs:525, git/mod.rs:288, error.rs:66). Fix: check if PR already exists before creating. If it does, retrieve the PR URL and continue. Or: make "PR already exists" a non-retryable success, not an error.

N60. **HIGH** - AWAITING_REAUTH recovery broken. nemo auth stores credentials but resumed jobs never read/mount/inject them into pods. build_job() only passes loop metadata (driver.rs:687, job_builder.rs:36, handlers.rs:397). Fix: build_job must read the engineer's credential_ref from the DB and mount it as a volume/env var in the job pod spec.

N61. **MEDIUM** - Branch name collision across engineers after slug normalization. Hash only uses spec path/content, not engineer name. "Alice" and "alice" produce the same branch (types/mod.rs:232, handlers.rs:42). Fix: include the raw (pre-slugified) engineer name in the hash input, not the slugified version.
