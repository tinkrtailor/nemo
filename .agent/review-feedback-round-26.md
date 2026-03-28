# Adversarial Review: Round 26 (OpenCode GPT-5.4, read-only)

1 finding.

## FINDINGS

N89. **HIGH** - ci_status() treats non-zero gh exit with empty stdout as "passed". If gh fails for auth/network/API reasons (not CI failure), ship mode auto-merges without confirming checks (git/mod.rs:289, 316, driver.rs:569). Fix: non-zero exit with empty stdout should be None (unknown/pending), not Some(true) (passed). Only return Some(true) on exit code 0. Non-zero + no failure keywords = unknown = keep polling.
