# Adversarial Review: Round 5 (OpenCode GPT-5.4)

5 findings. No CRITICALs. Getting narrow. Fix these and we converge.

## FINDINGS

N14. **HIGH** - main.rs boots with NemoConfig::default(), never loads nemo.toml. Repo config ignored in production: ship gating, model overrides, namespaces, timeouts all wrong (main.rs:28). Fix: load config from NEMO_CONFIG_PATH env var or default path, fail if invalid.

N15. **HIGH** - CLI always enables danger_accept_invalid_certs(true). TLS verification disabled in all environments. Bearer tokens vulnerable to MITM (cli/src/client.rs:14). Fix: only accept invalid certs when NEMO_INSECURE=true or --insecure flag, default to strict TLS.

N16. **MEDIUM** - AWAITING_REAUTH clears active_job_name but doesn't delete the K8s Job. Resume recreates same deterministic name, cleanup skipped because active_job_name is None, redispatch hits AlreadyExists (driver.rs:705, 709, 920, job_builder.rs:22). Fix: delete the failed Job before clearing active_job_name, or store it for cleanup on resume.

N17. **MEDIUM** - write_file() checks if git add/commit could be launched, not if they succeeded. Failed commits treated as success, worktree removed, loop proceeds with missing file (git/mod.rs:143, 153). Fix: check exit status of git add and git commit, propagate errors.

N18. **MEDIUM** - Branch names are deterministic. Restarting same spec after terminal run fails with "branch already exists" because old branch was never deleted (handlers.rs:35, 45, types/mod.rs:228, git/mod.rs:93). Fix: on /start, if branch exists but no active loop, delete and recreate. Or use create-or-checkout.
