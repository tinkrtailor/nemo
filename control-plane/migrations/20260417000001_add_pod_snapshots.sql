-- Pod introspection snapshot recording (FR-6).
-- Gated behind [observability] record_introspection = true in nemo.toml.
-- Each row is a point-in-time snapshot of the agent container's runtime state.
CREATE TABLE pod_snapshots (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    loop_id UUID NOT NULL REFERENCES loops(id),
    pod_name TEXT NOT NULL,
    snapshot JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_pod_snapshots_loop ON pod_snapshots (loop_id, created_at DESC);
