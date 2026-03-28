# Adversarial Review: Round 23 (OpenCode GPT-5.4, read-only)

7 findings (5 HIGH, 2 MEDIUM). Spike. New pattern: ignored delete_job() failures.

## FINDINGS

N79. **HIGH** - /start inserts loop then later writes stale clone via update_loop(). Reconciler can advance the loop between insert and update, causing state rollback to PENDING (handlers.rs:111, 142). Fix: don't call update_loop() from /start. The insert is enough. Any post-insert updates should be done by the loop engine only.

N80. **HIGH** - Spec validation can fall back to HEAD but branch created from origin/main. Spec may exist in HEAD but not in origin/main. Job starts from wrong tree (handlers.rs:31, git/mod.rs:102). Fix: validate spec ONLY from origin/main. Remove the HEAD fallback entirely. If spec isn't in origin/main, it hasn't been pushed.

N81. **HIGH** - Force-reset only resets local branch, not remote. Stale remote branch causes non-fast-forward push forever (git/mod.rs:135, 325). Fix: also delete the remote branch when force-resetting local (`git push origin --delete <branch>` before recreating).

N82. **HIGH** - Cancel ignores delete_job() failure, marks CANCELLED. Agent job can keep running after loop is terminal (driver.rs:744). Fix: if delete_job fails, log error but still mark CANCELLED. The reconciler should have a cleanup sweep for orphaned jobs whose loops are terminal.

N83. **HIGH** - Reauth paths ignore delete_job() failure, clear active_job_name. Old job survives, resume creates second job (driver.rs:771, 798). Fix: same pattern as N82 — log but proceed, plus orphan job cleanup.

N84. **HIGH** - Redispatch ignores delete_job() failure. Two jobs running concurrently for one loop (driver.rs:1004). Fix: if delete fails, don't dispatch new job. Return error. Retry on next tick.

N85. **MEDIUM** - get_loops_for_engineer LIMIT 100 applied before terminal filter. Active loops can be pushed off the result set by newer terminal loops (postgres.rs:305, handlers.rs:183). Fix: filter terminal states in the SQL WHERE clause, then apply LIMIT.

N86. **MEDIUM** - Malformed credential JSON warned but not counted as error. CLI exits with misleading "No credentials found" instead of "corrupted credentials" (auth.rs:53, 88). Fix: count malformed files as errors, report them distinctly.

## PATTERN NOTE

N82, N83, N84 are the SAME pattern: ignoring delete_job() failure. Fix ALL call sites of delete_job() with consistent error handling: log the failure, proceed with state transition, add orphan cleanup to the reconciler.
