-- Initial schema for Nemo control plane

-- Custom enum types
CREATE TYPE loop_state AS ENUM (
    'PENDING',
    'HARDENING',
    'AWAITING_APPROVAL',
    'IMPLEMENTING',
    'TESTING',
    'REVIEWING',
    'CONVERGED',
    'FAILED',
    'CANCELLED',
    'PAUSED',
    'AWAITING_REAUTH',
    'HARDENED',
    'SHIPPED'
);

CREATE TYPE sub_state AS ENUM (
    'DISPATCHED',
    'RUNNING',
    'COMPLETED'
);

CREATE TYPE loop_kind AS ENUM (
    'harden',
    'implement'
);

-- Main loops table
CREATE TABLE loops (
    id UUID PRIMARY KEY,
    engineer TEXT NOT NULL,
    spec_path TEXT NOT NULL,
    spec_content_hash TEXT NOT NULL,
    branch TEXT NOT NULL,
    kind loop_kind NOT NULL,
    state loop_state NOT NULL DEFAULT 'PENDING',
    sub_state sub_state,
    round INTEGER NOT NULL DEFAULT 0,
    max_rounds INTEGER NOT NULL DEFAULT 15,
    harden BOOLEAN NOT NULL DEFAULT FALSE,
    harden_only BOOLEAN NOT NULL DEFAULT FALSE,
    auto_approve BOOLEAN NOT NULL DEFAULT FALSE,
    ship_mode BOOLEAN NOT NULL DEFAULT FALSE,
    cancel_requested BOOLEAN NOT NULL DEFAULT FALSE,
    approve_requested BOOLEAN NOT NULL DEFAULT FALSE,
    resume_requested BOOLEAN NOT NULL DEFAULT FALSE,
    paused_from_state loop_state,
    reauth_from_state loop_state,
    failure_reason TEXT,
    current_sha TEXT,
    session_id TEXT,
    active_job_name TEXT,
    retry_count INTEGER NOT NULL DEFAULT 0,
    model_implementor TEXT,
    model_reviewer TEXT,
    merge_sha TEXT,
    merged_at TIMESTAMPTZ,
    hardened_spec_path TEXT,
    spec_pr_url TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Index for finding active loops (non-terminal states)
CREATE INDEX idx_loops_active ON loops (state)
    WHERE state NOT IN ('CONVERGED', 'FAILED', 'CANCELLED', 'HARDENED', 'SHIPPED');

-- Index for branch uniqueness check (active loops only)
CREATE UNIQUE INDEX idx_loops_active_branch ON loops (branch)
    WHERE state NOT IN ('CONVERGED', 'FAILED', 'CANCELLED', 'HARDENED', 'SHIPPED');

-- Index for engineer status queries
CREATE INDEX idx_loops_engineer ON loops (engineer);

-- Rounds table: tracks stage results within a loop
CREATE TABLE rounds (
    id UUID PRIMARY KEY,
    loop_id UUID NOT NULL REFERENCES loops(id),
    round INTEGER NOT NULL,
    stage TEXT NOT NULL,
    input JSONB,
    output JSONB,
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    duration_secs BIGINT,
    job_name TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_rounds_loop ON rounds (loop_id, round);

-- Log events table: structured log events persisted from pod logs
CREATE TABLE log_events (
    id UUID PRIMARY KEY,
    loop_id UUID NOT NULL REFERENCES loops(id),
    round INTEGER NOT NULL,
    stage TEXT NOT NULL,
    timestamp TIMESTAMPTZ NOT NULL,
    line TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_log_events_loop ON log_events (loop_id, timestamp);
CREATE INDEX idx_log_events_filter ON log_events (loop_id, round, stage);

-- Engineer credentials table
CREATE TABLE engineer_credentials (
    id UUID PRIMARY KEY,
    engineer TEXT NOT NULL,
    provider TEXT NOT NULL,
    credential_ref TEXT NOT NULL,
    valid BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE UNIQUE INDEX idx_credentials_engineer_provider ON engineer_credentials (engineer, provider);

-- Merge events table (NFR-8): log auto-merge events from nemo ship
CREATE TABLE merge_events (
    id UUID PRIMARY KEY,
    loop_id UUID NOT NULL REFERENCES loops(id),
    merge_sha TEXT NOT NULL,
    merge_strategy TEXT NOT NULL,
    ci_status TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_merge_events_loop ON merge_events (loop_id);
