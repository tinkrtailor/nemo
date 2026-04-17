-- Judge decisions table for orchestrator judge (Stage 1 self-learning roadmap).
-- Stores every judge invocation with input context, decision, and downstream outcome.
-- Rows are NEVER deleted — failure cases are needed for Stage 2 fine-tuning.

CREATE TABLE judge_decisions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    loop_id UUID NOT NULL REFERENCES loops(id),
    round INTEGER NOT NULL,
    phase TEXT NOT NULL,            -- 'review' | 'harden'
    trigger TEXT NOT NULL,          -- 'not_clean' | 'max_rounds' | 'recurring_findings'
    input_json JSONB NOT NULL,
    decision TEXT NOT NULL,         -- 'continue' | 'exit_clean' | 'exit_escalate' | 'exit_fail'
    confidence REAL,
    reasoning TEXT,
    hint TEXT,
    duration_ms INTEGER NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- populated later by the outcome reconciler when the loop terminates
    loop_final_state TEXT,          -- NULL until the loop terminates
    loop_terminated_at TIMESTAMPTZ
);

CREATE INDEX idx_judge_decisions_loop ON judge_decisions (loop_id, round);

-- FR-7a safety net: DB-level constraint to enforce at most one exit_clean per loop.
-- The application layer checks this via judge_decision_stats, but in a future multi-replica
-- deployment the TOCTOU window between the stats query and insert could allow two concurrent
-- ticks to both issue exit_clean. This partial unique index prevents that at the DB level.
CREATE UNIQUE INDEX idx_judge_decisions_one_exit_clean_per_loop
    ON judge_decisions (loop_id) WHERE decision = 'exit_clean';
