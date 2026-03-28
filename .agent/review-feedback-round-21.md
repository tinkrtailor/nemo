# Adversarial Review: Round 21 (OpenCode GPT-5.4, read-only)

3 findings.

## FINDINGS

N74. **HIGH** - Serial reconciler blocks all loops behind one CI wait. tick() waits inline for ship-mode CI (up to 30 min), and reconcile_all() processes loops serially. One loop waiting on CI stalls cancels, approvals, resumes for all other loops (reconciler.rs:79, driver.rs:537, 1097). Fix: don't block in tick(). CI polling should be async: set a "ci_check_pending" state, return from tick(), and check CI status on the next reconciliation tick. Or: spawn CI polling as a separate tokio task per loop.

N75. **MEDIUM** - ci_status() misclassifies "no required checks" as pending. gh pr checks --required with no required checks may output differently than expected. Returns None (pending) instead of Some(true) (pass) (git/mod.rs:286). Fix: if gh exits 0 with no failure strings, treat as passed (Some(true)). Only return None for non-zero exit without failure indicators.

N76. **MEDIUM** - Unvalidated credential provider string becomes invalid K8s env var name. NEMO_CRED_{provider} with "openai-api" or "1foo" creates invalid env vars, failing Job creation (handlers.rs:401, job_builder.rs:105). Fix: sanitize provider to uppercase alphanumeric + underscore when building env var name. Reject or transform invalid chars.
