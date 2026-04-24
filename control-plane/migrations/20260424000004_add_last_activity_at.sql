-- Activity heartbeat for `nemo status`. Updated by the reconciler
-- whenever it observes any signal of forward progress on a loop's
-- pod (new log bytes, K8s status transition, fresh dispatch, etc.).
-- NULL = no activity yet (e.g. PENDING loop awaiting first dispatch).
--
-- Operators today have to kubectl-exec into the agent pod to tell
-- "still working" from "wedged on dead credentials" — a 90+ minute
-- diagnostic gap that compounds every other failure mode. One
-- timestamp column closes most of that gap without touching the log
-- stream or tailing the opencode session DB.
ALTER TABLE loops
    ADD COLUMN last_activity_at TIMESTAMPTZ;
