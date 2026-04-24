-- Add per-loop stage timeout override. Applies uniformly to every
-- stage Job's `activeDeadlineSeconds` when set; otherwise the cluster
-- defaults from Timeouts config apply. NULL = no override.
--
-- Motivated by v0.7.9 production bug: a 32 KB spec's opencode audit
-- exceeded the hardcoded 900s budget and retried deterministically
-- until max_retries. Operators need a knob to raise this per-loop
-- without changing cluster config, and to raise it at resume time
-- after a DeadlineExceeded FAILED transition.
ALTER TABLE loops
    ADD COLUMN stage_timeout_secs INTEGER;
