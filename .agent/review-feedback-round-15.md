Starting a full read-only pass over the source tree and recent diff context, then I’ll report only concrete findings.
I’ve got the full Rust source inventory and current modified files. Next pass is a whole-codebase review for correctness and regression risk, with extra scrutiny on the touched files.
One substantive regression stands out in the touched files. I’m checking the lock contract and call sites so the report is precise about impact and scope.
1. Critical: `control-plane/src/state/postgres.rs:591-602`, with the broken call path in `control-plane/src/loop_engine/reconciler.rs:79-92` and `:142-143`
`try_advisory_lock()` now uses `pg_try_advisory_xact_lock($1)` on the pool without keeping a transaction open across `driver.tick()`. That lock is transaction-scoped, so it is released as soon as that single statement finishes. The reconciler then runs `tick()` effectively unlocked, while `advisory_unlock()` is a no-op. Result: two control-plane instances can reconcile the same loop concurrently, causing duplicate job dispatches, conflicting state transitions, and duplicate PR/merge attempts.

Not clean. Not converged.

Residual testing gap: current reconciler tests use `MemoryStateStore`, so they cannot catch Postgres advisory-lock lifetime regressions like this one.
