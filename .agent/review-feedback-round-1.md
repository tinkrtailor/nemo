# Adversarial Review: Round 1 (OpenCode GPT-5.4)

Fix ALL findings below. This is a cross-model adversarial review of your implementation.

## CRITICAL

1. **control-plane/src/main.rs:26-65**: Binary boots MemoryStateStore + MockJobDispatcher + MockGitOperations. No real DB, no migrations, no real job execution. Fix: wire PgStateStore, call run_migrations(), build real kube::Client/KubeJobDispatcher.

2. **control-plane/src/api/auth.rs:17-25**: Any non-empty Bearer token accepted. Effectively unauthenticated. Fix: validate API keys against stored credentials or configured secret; reject unknown keys.

3. **control-plane/src/loop_engine/driver.rs:173-443**: Completed jobs never write back output to RoundRecord. create_round_record() writes output: None, nothing calls update_round(). Loops stall because verdicts are never populated. Fix: on job success, fetch and persist stage artifacts, update round record, then evaluate transitions.

## HIGH

4. **control-plane/src/loop_engine/driver.rs:355-399, git/mod.rs, types/mod.rs**: Ship/harden terminal paths are fiction. SHIPPED set without merge/CI/PR ops. merge_event uses current_sha.unwrap_or_default() which is never populated. Fix: add real git/PR/merge operations, populate current_sha from implement output.

5. **control-plane/src/api/handlers.rs:37-42**: Race condition on /start. has_active_loop_for_branch() + create_loop() not atomic. Concurrent requests create duplicate loops. Fix: DB uniqueness constraint on active branch + atomic insert.

6. **control-plane/src/loop_engine/driver.rs:43-70, state/postgres.rs:318-353**: Driver reads full LoopRecord, mutates clone, writes back. API flag writes (cancel/approve/resume_requested) can be lost between read and overwrite. Fix: use optimistic locking or narrow patch updates.

7. **control-plane/src/loop_engine/driver.rs:62-63, 459-476**: PAUSED state exists but nothing transitions into it. Interrupt state required by spec is unreachable. Fix: add divergence detection that triggers PAUSED, persist paused_from_state.

8. **cli/src/commands/auth.rs:5-40**: nemo auth is placeholder. AWAITING_REAUTH has no recovery path. Fix: implement credential upload or mark as known gap.

## MEDIUM

9. **driver.rs:734-736**: redispatch_current_stage() always maps Hardening to audit. If revise job fails, incorrectly dispatches audit instead of revise. Fix: track current harden sub-stage from latest round.

10. **state/postgres.rs:267-270**: get_active_loops() excludes only CONVERGED/FAILED/CANCELLED but not HARDENED/SHIPPED. Reconciler ticks terminal loops forever. Fix: match LoopState::is_terminal() fully.

## LOW

11. **handlers.rs:33-35, git/mod.rs:14-15**: Branch created in DB but never in git. create_branch() is dead code. Fix: create branch during /start.

12. **state/postgres.rs:35-68**: Unknown DB enum values silently coerce to Pending/Dispatched. Hides corruption. Fix: make parsing fallible.

13. **handlers.rs:219-229**: /cancel returns CANCELLED but only sets flag. Loop still running. Fix: return current state + cancel_requested: true.

14. **driver.rs:1213-1383**: Tests inject round outputs manually, never exercise real output ingestion. Biggest failure mode untested. Fix: add integration tests with real job completion artifacts.
