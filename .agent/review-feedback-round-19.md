# Adversarial Review: Round 19 (OpenCode GPT-5.4, read-only)

5 findings. Recurring patterns: create-before-persist, error swallowing, Job name collision.

## FINDINGS

N68. **HIGH** - poll_ci() treats all non-zero gh exit as "keep polling". gh returns non-zero for both pending AND failed checks. Definitively failed PR hangs for 30 min timeout (driver.rs:1107). Fix: parse gh pr checks output. Distinguish "pending" (keep polling) from "failed" (stop, fall back to CONVERGED). Check for "fail" or "error" in output.

N69. **HIGH** - Resume/reauth redispatch recreates same Job name without bumping retry_count. Async deletion + recreate = AlreadyExists. Loops stuck unable to resume (driver.rs:975, job_builder.rs:22). Fix: always increment retry_count on resume/reauth redispatch, not just on failure retries.

N70. **MEDIUM** - remove_path() swallows git rm and commit failures, returns Ok(()). .agent/ cleanup silently fails, artifacts leak into PRs (git/mod.rs:236). Fix: propagate errors from git rm and git commit. If cleanup fails, log error but still create PR (don't block convergence on cleanup failure).

N71. **HIGH** - Driver creates K8s Job BEFORE persisting active_job_name and round state. If DB write fails after job creation, loop looks undispatched, launches duplicate/orphaned jobs on retry. Pattern appears at driver.rs:100, 439, 865, 912, 957. Fix: persist state FIRST (active_job_name set, round created), THEN create the K8s Job. If K8s create fails, clear the state. DB is source of truth.

N72. **MEDIUM** - Harden-only with auto_merge=false ends in CONVERGED not HARDENED. Breaks the harden-only contract (driver.rs:317). Fix: harden-only should ALWAYS end in HARDENED regardless of auto_merge. The PR is the spec deliverable. Whether it's merged or not is separate from whether hardening converged.
