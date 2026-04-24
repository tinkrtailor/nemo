-- Per-loop `[cache.env]` overrides plumbed from repo-level nemo.toml
-- by the CLI at submit time. Merged with the cluster default at
-- stage-dispatch time; per-loop keys win on collisions. NULL = no
-- override (cluster default stands as-is).
--
-- Stored as JSONB rather than split columns because the shape is an
-- arbitrary env-var map (tool names, version-specific vars, repo
-- conventions). Enforcing a fixed column list would defeat the
-- purpose of operator-supplied overrides.
ALTER TABLE loops
    ADD COLUMN cache_env_overrides JSONB;
