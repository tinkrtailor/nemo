# Adversarial Review: Round 22 (OpenCode GPT-5.4, read-only)

2 findings.

## FINDINGS

N77. **HIGH** - Auth expiry detection unreliable. job_to_status collapses failed Jobs to generic reasons like "BackoffLimitExceeded". Exit code 42 auth convention never inspected from pod status. Loops go FAILED instead of AWAITING_REAUTH (k8s/client.rs:102, 123, driver.rs:767). Fix: in job_to_status, inspect the pod's container termination state for exit code. K8s Job status -> pod status -> container status -> terminated.exitCode. If 42, return a distinct AuthExpired variant instead of generic Failed.

N78. **MEDIUM** - Restarting on a branch with a CLOSED PR fails. create_branch recreates locally but push is non-fast-forward because remote branch from closed PR still exists (git/mod.rs:122, 321). Fix: use `git push --force-with-lease origin <branch>` for branches being restarted after CLOSED/MERGED PRs. Or delete the remote branch before recreating.
