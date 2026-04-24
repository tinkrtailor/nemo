-- Per-stage `activeDeadlineSeconds` overrides plumbed from repo-level
-- `nemo.toml` [timeouts]. Each column mirrors a stage_config entry in
-- the driver. NULL means "fall through to the uniform stage_timeout_secs
-- override (if set by --stage-timeout) or the cluster default".
--
-- Distinct from `stage_timeout_secs` (migration 20260424000001), which
-- is a uniform-across-all-stages value set by `--stage-timeout`. The
-- per-stage columns win when set, so a user can pin audit=3600 but
-- still let implement inherit the cluster default.
ALTER TABLE loops
    ADD COLUMN implement_timeout_secs INTEGER,
    ADD COLUMN test_timeout_secs      INTEGER,
    ADD COLUMN review_timeout_secs    INTEGER,
    ADD COLUMN audit_timeout_secs     INTEGER,
    ADD COLUMN revise_timeout_secs    INTEGER;
