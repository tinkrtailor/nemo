# Adversarial Review: Round 10 (OpenCode GPT-5.4)

5 findings. Fix all.

## FINDINGS

N37. **CRITICAL** - Unrecoverable driver errors (gh/git/K8s failures) never transition loop to FAILED. Reconciler logs and retries forever (reconciler.rs:79, driver.rs:492, 523). Fix: catch non-retryable errors in tick(), transition to FAILED with failure_reason. Distinguish retryable (timeout, transient) from fatal (auth denied, binary not found, git corrupt).

N38. **HIGH** - /start validates spec and hashes content from HEAD, but creates branch from origin/main. After fetch, these can differ. System validates one revision, branches from another (handlers.rs:27, 37, git/mod.rs:83, 98). Fix: read spec content from origin/main (not HEAD), or resolve the base ref first and use it consistently for both validation and branching.

N39. **HIGH** - Failed K8s jobs collapse to "Pod failure" for all agent exits. Auth expiry not distinguishable from other failures, so AWAITING_REAUTH never triggers (k8s/client.rs:120, driver.rs:705, 1141). Fix: parse job pod logs or exit code to detect auth failures. Convention: exit code 42 = auth expired, or parse stderr for "auth", "unauthorized", "expired".

N40. **MEDIUM** - nemo status --team blocked when no engineer configured, even though team-wide query doesn't need one. Fresh users can't check team status (main.rs:175, 183, status.rs:24). Fix: skip engineer validation for --team flag.

N41. **MEDIUM** - nemo inspect sends raw user input but server expects /inspect/{user}/{branch}. Copy-pasting the printed branch name (agent/alice/slug-hash) doesn't match the route (start.rs:43, inspect.rs:6, mod.rs:44, handlers.rs:327). Fix: either split the input on first / in CLI, or change the API route to /inspect/{branch_path} with a wildcard.
