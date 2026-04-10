-- Issue #100: Replace the single untyped `session_id` text column with
-- per-tool typed columns so the control plane never forwards a
-- wrong-shape session ID to the wrong CLI.
--
-- opencode emits `ses_<alphanum>` IDs, Claude requires UUIDs.
-- Storing them in separate columns makes forwarding type-safe at the
-- Rust layer and lets `nemo inspect` show both independently.

ALTER TABLE loops ADD COLUMN opencode_session_id TEXT;
ALTER TABLE loops ADD COLUMN claude_session_id TEXT;

-- Migrate existing values. session_id is either NULL, `ses_<chars>`
-- (opencode), or a UUID (claude). Any other shape is a bug from
-- before #92 and lands in neither column (safe loss — the session
-- wasn't usable anyway).
UPDATE loops SET
    opencode_session_id = CASE WHEN session_id LIKE 'ses_%' THEN session_id ELSE NULL END,
    claude_session_id   = CASE
        WHEN session_id ~ '^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$'
        THEN session_id
        ELSE NULL
    END;

-- DO NOT drop session_id in this migration. During a rolling deploy,
-- old pods still SELECT/INSERT/UPDATE against session_id and would
-- fail with SQL errors. Drop it in a future migration after all
-- replicas have been updated to the new code (expand/contract pattern).
-- ALTER TABLE loops DROP COLUMN session_id;
