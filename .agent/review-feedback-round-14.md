# Adversarial Review: Round 14 (OpenCode GPT-5.4, read-only)

3 findings.

## FINDINGS

N52. **HIGH** - update_loop() doesn't persist spec_path. Harden loop mutates it in driver.rs:386 after revise, but DB keeps old path. Next tick/restart dispatches against wrong spec (postgres.rs:341). Fix: add spec_path to the UPDATE SET clause in update_loop().

N53. **MEDIUM** - SSE seen_ids HashSet grows unbounded per client connection. Never pruned (sse.rs:26, 48). Fix: prune seen_ids older than 5 seconds (keep only current timestamp window), or switch to serial id cursor which eliminates the need for dedup entirely.

N54. **LOW** - Harden-only with auto_merge_spec_pr=false transitions to HARDENED even though spec PR was only created, not merged (driver.rs:329, 341). Fix: if auto_merge is false, transition to CONVERGED (PR needs human merge), not HARDENED (which implies delivered).
