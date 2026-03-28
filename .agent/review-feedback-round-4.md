# Adversarial Review: Round 4 (OpenCode GPT-5.4)

Most prior findings FIXED. 1 still broken, 7 new findings. Reviewer is getting pickier.

## STILL BROKEN

R3-N4. **MEDIUM** - /credentials still stores `req.engineer.unwrap_or_default()` (handlers.rs:370). `engineer` is still optional in api.rs:145. Fix: make engineer required in the request struct (not Option), reject if empty.

## NEW FINDINGS

N7. **HIGH** - Retry/resume can re-create the same K8s Job name and hit AlreadyExists. Deterministic name in job_builder.rs:21, redispatch without cleanup in driver.rs:891. Fix: append retry count to job name, or delete the old Job before redispatching.

N8. **HIGH** - Feedback files are modeled and paths passed, but nothing writes the JSON file to the worktree before redispatch. Feedback constructed in driver.rs:445 and driver.rs:574, ignored in driver.rs:853. Fix: write feedback JSON to the worktree (via git ops) before dispatching the next implement job.

N9. **MEDIUM** - ship.require_passing_ci is dead config. Ship merge ignores it (config/mod.rs:27, driver.rs:498). Fix: check CI status before merging, or remove the config field.

N10. **MEDIUM** - ship_mode + require_harden uses implement round limits because max_rounds computed before effective_harden (handlers.rs:62, 69). Fix: compute effective_harden first, then pick the right max_rounds.

N11. **MEDIUM** - CLI approve/resume always print success even if server didn't act (approve.rs:17, resume.rs:17). Fix: check response status field before printing success.

N12. **MEDIUM** - CLI config prints raw API keys to stdout (config.rs:11, 35). Fix: mask sensitive values when printing.

N13. **LOW** - Job watcher exists but is never started from main, so wakeups are polling-only (watcher.rs:9, main.rs:81). Fix: start the watcher task in main alongside the reconciler.
