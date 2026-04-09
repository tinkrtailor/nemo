-- Issue #96: nemo resume should support FAILED loops, reusing the worktree.
-- Mirrors paused_from_state / reauth_from_state: captures which stage was
-- running when the loop transitioned to Failed so the resume path can
-- redispatch the correct stage without guessing or losing the round.
ALTER TABLE loops ADD COLUMN IF NOT EXISTS failed_from_state loop_state;

-- Extend the partial unique index on `branch` to also cover FAILED loops
-- with a pending resume_requested flag. Without this, a concurrent
-- `/start` on the same deterministic branch between the resume request
-- and the next reconciler tick could insert a second row and both would
-- be active for the same branch. See codex round-2 review of #96.
DROP INDEX IF EXISTS idx_loops_active_branch;
CREATE UNIQUE INDEX idx_loops_active_branch ON loops (branch)
    WHERE state NOT IN ('CONVERGED', 'FAILED', 'CANCELLED', 'HARDENED', 'SHIPPED')
       OR (state = 'FAILED' AND resume_requested = TRUE);
