-- Judge decisions table for Stage 1 of the self-learning roadmap.
-- Every orchestrator judge invocation writes a row here, building the
-- dataset that a future Stage 2 fine-tune will train on.

CREATE TABLE judge_decisions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    loop_id UUID NOT NULL REFERENCES loops(id),
    round INTEGER NOT NULL,
    phase TEXT NOT NULL,           -- 'review' | 'harden'
    trigger TEXT NOT NULL,          -- 'not_clean' | 'max_rounds' | 'recurring_findings'
    input_json JSONB NOT NULL,
    decision TEXT NOT NULL,         -- 'continue' | 'exit_clean' | 'exit_escalate' | 'exit_fail'
    confidence REAL,
    reasoning TEXT,
    hint TEXT,
    duration_ms INTEGER NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- populated later by the outcome reconciler
    loop_final_state TEXT,          -- NULL until the loop terminates
    loop_terminated_at TIMESTAMPTZ
);

CREATE INDEX idx_judge_decisions_loop ON judge_decisions (loop_id, round);
