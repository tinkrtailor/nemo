# Adversarial Review: Round 27 (OpenCode GPT-5.4, read-only)

3 findings.

## FINDINGS

N90. **HIGH** - write_file() leaks temp worktrees on create_dir_all/write failures. Only cleans up on git add/commit failures, not filesystem errors (git/mod.rs:177, 181). Fix: wrap the entire write_file body in a cleanup-on-error block. If ANY step fails, remove the temp worktree before returning the error.

N91. **HIGH** - ci_status() treats any gh output containing "error" as CI failed. Transient gh auth/network errors can permanently stop auto-merge (git/mod.rs:310). Fix: only classify as failed if exit code is 0 AND output contains failure indicators. Non-zero exit = unknown (transient), not failed (permanent). This is a different logic path than N89.

N92. **MEDIUM** - Completed K8s Jobs never cleaned up. No ttlSecondsAfterFinished set. Terminal paths clear active_job_name without deleting the Job (job_builder.rs:143, driver.rs:614, 856). Fix: set ttlSecondsAfterFinished: 300 (5 min) on all Job specs. K8s auto-cleans completed Jobs after TTL.
