# Adversarial Review: Round 2 (OpenCode GPT-5.4)

10/14 round 1 findings FIXED. 4 still broken. 3 new issues from the fixes.

## STILL BROKEN FROM ROUND 1

1. **STILL_BROKEN** - main.rs falls back to MockJobDispatcher on kube init failure (control-plane/src/main.rs:47). Real kube client required for production. Fix: fail hard on kube init failure instead of silently falling back to mocks. A control plane that can't dispatch jobs should not start.

4. **STILL_BROKEN** - Ship/harden terminal paths still only mutate DB state. No real merge/PR/spec-PR operations exist (driver.rs:455, driver.rs:311, git/mod.rs:7). Fix: add create_pr() and merge_pr() to GitOperations trait. These can shell out to `gh pr create` and `gh pr merge`. For V1, this is acceptable.

5. **STILL_BROKEN** - /start still non-atomic: check + git branch + create_loop are separate operations (handlers.rs:37, 44, 112). Fix: wrap the entire sequence in a Postgres transaction. Use a DB unique constraint on (branch, status != terminal) as the ultimate guard.

8. **STILL_BROKEN** - nemo auth CLI posts to /credentials but the router has no such route (api/mod.rs:37). Fix: add POST /credentials route, or mark auth as a known V1 gap in the spec.

## NEW FINDINGS FROM FIXES

N1. **HIGH** - /start creates a git branch BEFORE the DB insert. If DB insert fails, orphan branch left in git with no cleanup (handlers.rs:44, 112, git/mod.rs:84). Fix: create branch inside the same transaction, or add a cleanup path that deletes the branch on DB failure.

N2. **HIGH** - nemo cancel CLI expects {loop_id, state, reason} but server now returns {loop_id, state, cancel_requested} (types/api.rs:91 vs cancel.rs:6). Deserialization will fail. Fix: align the response struct.

N3. **LOW** - nemo auth prints wrong recovery command: "nemo start resume <loop-id>" instead of "nemo resume <loop-id>" (auth.rs:60). Fix: correct the string.
