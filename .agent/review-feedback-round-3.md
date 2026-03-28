# Adversarial Review: Round 3 (OpenCode GPT-5.4)

16/17 prior findings FIXED. 1 still broken, 3 new. Almost converged.

## STILL BROKEN

4. **STILL_BROKEN** - Harden completion still just transitions state. Never creates/merges a spec PR or sets hardened-spec fields. Implement/ship path has real gh PR+merge calls, but harden path does not (driver.rs:303, 315, 326). Fix: add create_pr + merge_pr calls to the harden convergence path, same pattern as implement.

## NEW FINDINGS

N4. **MEDIUM** - nemo auth sends credentials with empty engineer name. CLI never sends engineer field, server defaults to "". Breaks reauth lookup for real users (cli/src/client.rs:93, handlers.rs:369). Fix: CLI should read engineer name from ~/.nemo/config.toml and include it in the request.

N5. **MEDIUM** - /inspect cannot inspect terminal loops because get_loop_by_branch filters out terminal states (handlers.rs:321, state/mod.rs:138, postgres.rs:267). Fix: add a get_loop_by_branch_any() that includes terminal states, use it for /inspect.

N6. **LOW** - Ship mode marks SHIPPED immediately after `gh pr merge --auto`, but --auto can defer the merge. State and merge_sha can be wrong (git/mod.rs:147, 161, driver.rs:480, 483). Fix: either use `gh pr merge` (blocking) instead of --auto, or add a polling step to verify merge completed before setting SHIPPED.
