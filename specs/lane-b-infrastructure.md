# Lane B: Infrastructure Layer

## Overview

Postgres schema, git operations, and config loading for the Nemo control plane. These are the foundational modules that every other component depends on: the loop engine writes state to Postgres, dispatches jobs against git worktrees, and reads merged configuration to determine model preferences and limits.

> **Eng review (2026-03-27) decided Postgres over SQLite. Design doc updated.**
>
> **Schema design principle (from Lane A convergence learnings): Persist-then-dispatch.** All DB state must be written before K8s Job creation. If job creation fails, the DB row exists and can be retried. If the control plane crashes after creating a job but before persisting, the job is orphaned. Always persist first.

## Dependencies

- **Requires:** [Design doc](../docs/design.md) (architecture, resource model, configuration layers)
- **Required by:** Loop engine, job scheduler, API server, CLI

## Requirements

### Functional Requirements

#### Postgres Schema

- FR-1: The `loops` table shall store all loop state including phase (HARDEN/IMPLEMENT), stage, `state` (full lifecycle: pending through shipped/cancelled), `sub_state` (dispatched/running/completed), round counter, current SHA, `harden_only` flag, `auto_approve` flag, `paused_from_state`, `reauth_from_state`, `failure_reason`, ship mode fields, request flags (`cancel_requested`, `approve_requested`, `resume_requested`), `needs_human_review`, and the engineer who owns it. When harden converges: if `harden_only`, state transitions to `hardened` and phase stays `harden`; if not `harden_only` and `auto_approve = true`, skip `awaiting_approval` and transition directly to `implementing` (phase transitions to `implement`); if not `harden_only` and `auto_approve = false`, state transitions to `awaiting_approval` until engineer approves, then phase transitions to `implement`. The `auto_approve` flag is set by `nemo ship --harden` (always true) or `nemo start --harden --auto-approve` (explicit opt-in).
- FR-2: The `jobs` table shall store every K8s job dispatched, linked to its parent loop, with status, timing, verdict JSON, `output_json` (stage output including affected_services, test results, new SHA), `attempt` (retry tracking), `feedback_path`, and token usage.
- FR-3: The `engineers` table shall store registered engineers with their git identity, model preferences, and concurrency limits.
- FR-4: The `egress_logs` table shall store all outbound network traffic logged by the auth sidecar, linked to the originating job.
- FR-4a: The `log_events` table shall store structured log events (id, loop_id, timestamp, stage, round, level, message) persisted from pod logs by the loop engine. This is the source for `GET /logs/:id`.
- FR-4b: The `engineer_credentials` table shall store per-engineer, per-provider credential references (id, engineer_id, provider, credential_ref, valid, created_at, updated_at). Unique on `(engineer_id, provider)`. The `credential_ref` is always `nemo-creds-{engineer}` (one K8s Secret per engineer). The `provider` (`claude` or `openai`) maps to a key within that Secret. Secret keys: `claude` (contains `~/.claude/` session data), `openai` (contains opencode auth data). Mount path in sidecar: `/secrets/model-credentials/` (directory, files named by provider key).
- FR-4c: The `cluster_credentials` table shall store cluster-level credentials used by the control plane itself (id, type [`api_key`, `mtls_cert`, `git_host_token`], credential_ref pointing to a K8s Secret, description, created_at). These are not per-engineer credentials; they are cluster-wide (e.g., `NEMO_API_KEY` for CLI authentication, `GIT_HOST_TOKEN` GitHub PAT for PR creation/merge operations). The control plane reads these on startup to configure API auth and git host integration.
- FR-5: All schema changes shall be managed via `sqlx migrate` with sequential, timestamped migration files checked into the repo.
- FR-5a: Migrations shall run as a separate K8s Job (`helm.sh/hook: pre-upgrade`) BEFORE either API server or loop engine Deployment starts. Both binaries verify schema version on startup but do not run migrations themselves. This ensures schema consistency across split deployments.
- FR-6: The schema shall enforce referential integrity: jobs reference loops, loops reference engineers, egress_logs reference jobs, log_events reference loops, engineer_credentials reference engineers.

#### Git Operations

- FR-7: `BareRepo::prepare_worktree(branch, base_ref) -> PathBuf` shall acquire the worktree mutex, run `git fetch --prune`, resolve the target ref to a SHA, create a worktree at that SHA in detached HEAD mode, then `git checkout -b {branch}` inside the worktree, then release the mutex and return the worktree path. **`branch` is the full branch name as returned by `branch_name()` (e.g., `agent/alice/invoice-cancel-a1b2c3d4`).** The checkout command uses the branch value directly -- no additional `agent/` prefix is added. This replaces the old two-step `fetch_and_resolve()` + `create_worktree()` API.
- FR-8: (Subsumed by FR-7.) The agent commits to the named branch (e.g., `agent/alice/invoice-cancel-a1b2c3d4`) inside the worktree, not detached HEAD.
- FR-9: `BareRepo::cleanup_worktree(path)` shall acquire the worktree mutex, remove the worktree, run `git worktree prune`, then release the mutex.
- FR-10: The worktree mutex is only held during `prepare_worktree` (create) and `cleanup_worktree` (delete), NOT for the job's entire lifetime. Multiple jobs can run concurrently on different worktrees. The mutex serializes only the git worktree create/delete operations to prevent `git worktree` lock contention and fetch/worktree race conditions. There is no separate fetch CronJob; all fetches are per-job via `prepare_worktree()`.
- FR-11: Branch creation shall follow the pattern `agent/{engineer}/{spec-slug}-{short-hash}` where `short-hash` is the first 8 hex chars of SHA-256 of the ORIGINAL spec file content at submission time, making branch names immutable across harden rounds. (Note: design doc examples are being updated to include hash suffix.)
- FR-12: `BareRepo::detect_divergence()` shall compare the local branch tip SHA against the remote tracking branch and classify the result into three variants: `RemoteAhead` (engineer pushed additional commits, fast-forward possible), `ForceDeviated` (histories diverged due to force push), or `LocalAhead` (normal agent operation, not a divergence).
- FR-13: On `RemoteAhead`: pause (state becomes `paused_remote_ahead`, `paused_from_state` records prior state), notify engineer. On `ForceDeviated`: pause (state becomes `paused_force_deviated`, `paused_from_state` records prior state), notify engineer. On `LocalAhead`: normal operation, no action. On `RemoteGone`: cancel only (branch is deleted on remote, work is lost, no resume possible). **Note on Lane A alignment:** Lane A's state machine uses the same two concrete pause states (`paused_remote_ahead`, `paused_force_deviated`). The resume endpoint checks which variant and applies the right behavior (fast-forward for `paused_remote_ahead`, force-reset for `paused_force_deviated` with `--force` flag required).
- FR-13a: `paused_remote_ahead` state machine: `nemo resume <loop-id>` fast-forwards to remote SHA (no work lost), re-dispatches current stage. `nemo cancel <loop-id>` transitions to `cancelled`. No other transitions valid.
- FR-13b: `paused_force_deviated` state machine: `nemo resume --force <loop-id>` shows what commits will be discarded, then resets to remote SHA. Without `--force`, the command is rejected with an explanation of data loss. `nemo cancel <loop-id>` transitions to `cancelled`. No other transitions valid.

#### Config Loading

- FR-14: Config shall merge three layers in order: cluster (lowest priority) -> repo (`nemo.toml`) -> engineer (`~/.nemo/config.toml`, highest priority).
- FR-15: `nemo.toml` shall be parsed from the monorepo root using the `toml` crate. **Config loading timing (fix #16):** The CLI validates `nemo.toml` locally before `nemo submit` (fail fast). The API revalidates on receipt. The loop engine treats a missing repo config as a terminal failure at dispatch time (loop transitions to `failed` with `failure_reason = "nemo.toml not found in worktree"`). Missing file is not an error at the cluster level (bare cluster config suffices); missing file IS an error at the repo level for `nemo submit`.
- FR-16: Engineer config at `~/.nemo/config.toml` shall be optional. Missing file means no overrides.
- FR-17: Cluster config shall be read from a K8s ConfigMap mounted as a file, or from environment variables prefixed with `NEMO_CLUSTER_`.
- FR-18: Model resolution order: engineer override > repo default > cluster default. If no model is configured at any layer, fail with an explicit error naming which role (implementor/reviewer) is unconfigured.
- FR-19: `nemo init` shall scan the monorepo root for build system markers (`Cargo.toml`, `package.json`, `go.mod`, `pyproject.toml`, `build.sbt`, `foundry.toml`, `composer.json`, `Makefile`) and generate a `nemo.toml` with auto-detected `[services.*]` entries.
- FR-20: Engineer limits (e.g., `max_parallel_loops`) shall be capped by cluster limits. If an engineer sets `max_parallel_loops = 10` but the cluster cap is 5, the effective value is 5.

### Non-Functional Requirements

- NFR-1: Postgres queries on `loops` and `jobs` tables shall complete in < 5ms for single-row lookups by primary key (indexed).
- NFR-2: `create_worktree` shall complete in < 2s for repos up to 5 GB bare size, on the target CCX43 NVMe disk with warm filesystem cache.
- NFR-3: `git fetch` shall time out after 120s. Fetch failure shall not crash the control plane; the job is retried with backoff.
- NFR-4: Config parsing shall fail fast on startup with clear error messages naming the exact field and layer that failed validation.
- NFR-5: All Postgres operations shall use connection pooling (sqlx `PgPool`, max 20 connections shared across API server and loop engine via connection string config, matching Lane A NFR-3).
- NFR-6: Schema migrations shall be forward-only. No down migrations. Breaking changes require a new migration that transforms data.

## Behavior

### Stage Name Mapping

Job names, API query parameters, log labels, and prompt template filenames use **short stage names**: `implement`, `test`, `review`, `audit`, `revise`. The Postgres `loop_stage` enum stores **full names**: `implementing`, `testing`, `reviewing`, `spec_audit`, `spec_revise`. Full mapping:

| Short name (jobs, API, logs) | DB enum value | Prompt template filename |
|------------------------------|---------------|--------------------------|
| `implement` | `implementing` | `implement.md` |
| `test` | `testing` | `test.md` |
| `review` | `reviewing` | `review.md` |
| `audit` | `spec_audit` | `spec-audit.md` |
| `revise` | `spec_revise` | `spec-revise.md` |

Short names (`audit`, `revise`) are used everywhere except the DB enum. The `spec_` prefix appears ONLY in Postgres `loop_stage` enum values. Config references, job names, API parameters, and log labels always use `audit`/`revise` (no `spec_` prefix).

### Postgres Schema Detail

```sql
-- 001_initial_schema.sql

CREATE TYPE loop_phase AS ENUM ('harden', 'implement');
CREATE TYPE loop_stage AS ENUM (
    'spec_audit', 'spec_revise',           -- harden phase
    'implementing', 'testing', 'reviewing' -- implement phase
);
-- Full state enum matching Lane A implementation.
-- Replaces the old "loop_status" which lacked approval, reauth, and granular pause states.
CREATE TYPE loop_state AS ENUM (
    'pending',                  -- submitted, not yet dispatched
    'hardening',                -- harden phase active
    'awaiting_approval',        -- harden converged, waiting for engineer to approve implement
    'implementing',             -- implement phase: code generation
    'testing',                  -- implement phase: running tests
    'reviewing',                -- implement phase: review verdict
    'converged',                -- implement phase converged (clean verdict)
    'hardened',                 -- harden_only loop completed
    'shipped',                  -- PR merged (ship mode)
    'failed',                   -- terminal failure
    'paused_remote_ahead',      -- engineer pushed to branch (fast-forward possible)
    'paused_force_deviated',    -- branch histories diverged (force push)
    'awaiting_reauth',          -- credential expired, waiting for engineer re-auth
    'cancelled'                 -- cancelled by engineer
);
CREATE TYPE loop_sub_state AS ENUM (
    'dispatched',   -- K8s Job created, not yet running
    'running',      -- K8s Job running
    'completed'     -- K8s Job completed, result ingested
);
CREATE TYPE job_status AS ENUM (
    'pending', 'running', 'succeeded', 'failed',
    'errored'  -- malformed results (e.g., unparseable verdict JSON)
);

CREATE TABLE engineers (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name        TEXT NOT NULL UNIQUE,
    email       TEXT NOT NULL UNIQUE,
    model_preferences JSONB NOT NULL DEFAULT '{}',
    max_parallel_loops INTEGER NOT NULL DEFAULT 5,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE loops (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    engineer_id     UUID NOT NULL REFERENCES engineers(id),
    spec_path       TEXT NOT NULL,
    branch          TEXT NOT NULL,          -- NOT globally unique; see partial index below
    phase           loop_phase NOT NULL,
    stage           loop_stage NOT NULL,
    state           loop_state NOT NULL DEFAULT 'pending',
    sub_state       loop_sub_state,         -- NULL when no job is active
    harden_only     BOOLEAN NOT NULL DEFAULT false,
    round           INTEGER NOT NULL DEFAULT 0,
    sha             TEXT NOT NULL,           -- current branch tip; set to base branch tip at loop creation
    expected_sha    TEXT,                    -- last SHA the control plane dispatched against
    actual_sha      TEXT,                    -- remote SHA observed on divergence detection
    paused_from_state loop_state,           -- state before pausing (for resume)
    reauth_from_state loop_state,           -- state before reauth pause (for resume after re-auth)
    failure_reason  TEXT,                    -- human-readable reason when state = 'failed'
    active_job_name TEXT,                    -- K8s job name; set BEFORE job creation, cleared on completion
    stage_retry_count INTEGER NOT NULL DEFAULT 0, -- resets to 0 on stage transition
    feedback_path   TEXT,                    -- path to feedback file for current stage (not reconstructed)
    -- Ship mode fields (from Lane A learnings)
    ship_mode       BOOLEAN NOT NULL DEFAULT false,
    max_rounds_for_auto_merge INTEGER,      -- NULL = no auto-merge; e.g., 5 for confident results
    merge_strategy  TEXT,                    -- 'squash', 'merge', 'rebase'; NULL = no auto-merge
    pr_url          TEXT,                    -- GitHub PR URL once created
    merge_sha       TEXT,                    -- SHA of the merge commit once shipped
    ci_check_started_at TIMESTAMPTZ,        -- when CI check polling began (non-blocking)
    -- Request flags: set by API server, read by loop engine on next tick
    auto_approve        BOOLEAN NOT NULL DEFAULT false,  -- skip AWAITING_APPROVAL gate
    cancel_requested    BOOLEAN NOT NULL DEFAULT false,
    approve_requested   BOOLEAN NOT NULL DEFAULT false,
    resume_requested    BOOLEAN NOT NULL DEFAULT false,
    force_resume        BOOLEAN NOT NULL DEFAULT false,  -- requires --force for paused_force_deviated resume
    -- Human review flag: set when max_rounds exceeded or CI fails in ship mode.
    -- Queryable by `nemo status` to highlight loops needing attention.
    needs_human_review  BOOLEAN NOT NULL DEFAULT false,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Phase/stage validity constraint (fix #6)
    CONSTRAINT chk_phase_stage CHECK (
        (phase = 'harden' AND stage IN ('spec_audit', 'spec_revise'))
        OR (phase = 'implement' AND stage IN ('implementing', 'testing', 'reviewing'))
    ),
    -- Terminal state protection (from Lane A learnings): prevent overwrites of terminal states
    -- Enforced at the application layer with a pre-update check:
    --   UPDATE loops SET state = $new WHERE id = $id AND state NOT IN ('converged','hardened','shipped','failed','cancelled')
    -- The CHECK constraint below is a safety net (triggers on direct SQL).
    CONSTRAINT chk_terminal_state_immutable CHECK (
        -- This constraint is documentation; actual enforcement is via the UPDATE WHERE clause.
        -- Postgres CHECK constraints cannot reference OLD values, so this is application-enforced.
        true
    )
);

-- Fix #2: Partial unique index. Completed loops don't block branch resubmission.
-- Replaces global UNIQUE(branch).
CREATE UNIQUE INDEX idx_loops_active_branch ON loops(branch)
    WHERE state NOT IN ('converged', 'hardened', 'shipped', 'failed', 'cancelled');

CREATE INDEX idx_loops_engineer_id ON loops(engineer_id);
CREATE INDEX idx_loops_state ON loops(state);
CREATE INDEX idx_loops_active ON loops(state)
    WHERE state NOT IN ('converged', 'hardened', 'shipped', 'failed', 'cancelled');
CREATE INDEX idx_loops_engineer_state ON loops(engineer_id, state);

CREATE TABLE jobs (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    loop_id         UUID NOT NULL REFERENCES loops(id) ON DELETE CASCADE,
    stage           loop_stage NOT NULL,
    round           INTEGER NOT NULL,
    attempt         INTEGER NOT NULL DEFAULT 1,  -- retry tracking; increments per retry within same stage+round
    k8s_job_name    TEXT NOT NULL,                -- format: nemo-{loop_id_short}-{stage}-r{round}-t{attempt}
    status          job_status NOT NULL DEFAULT 'pending',
    started_at      TIMESTAMPTZ,
    completed_at    TIMESTAMPTZ,
    verdict_json    JSONB,        -- review/audit verdict, NULL for non-review jobs
    output_json     JSONB,        -- stage output: verdict, test results, affected_services, new SHA, session_id
    token_usage     JSONB,        -- {"input": N, "output": N}
    exit_code       INTEGER,
    error_message   TEXT,
    feedback_path   TEXT,         -- path to feedback file for this job (stored, not reconstructed)
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Fix #9: k8s_job_name unique per (loop_id, stage, round, attempt), not globally unique.
-- This prevents collision on retry while still enabling idempotent reconciliation.
CREATE UNIQUE INDEX idx_jobs_k8s_job_name ON jobs(k8s_job_name);
CREATE UNIQUE INDEX idx_jobs_loop_stage_round_attempt ON jobs(loop_id, stage, round, attempt);
CREATE INDEX idx_jobs_loop_id ON jobs(loop_id);
CREATE INDEX idx_jobs_status ON jobs(status);

CREATE TABLE egress_logs (
    id              BIGSERIAL PRIMARY KEY,
    job_id          UUID NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    timestamp       TIMESTAMPTZ NOT NULL DEFAULT now(),
    host            TEXT NOT NULL,          -- hostname or IP
    port            INTEGER NOT NULL,       -- destination port
    protocol        TEXT NOT NULL,          -- 'HTTP', 'HTTPS', 'TCP', etc.
    status_code     INTEGER,               -- HTTP status code, NULL for raw TCP
    bytes_sent      BIGINT NOT NULL DEFAULT 0,
    bytes_received  BIGINT NOT NULL DEFAULT 0,
    method          TEXT                   -- HTTP method, NULL for raw TCP
);

CREATE INDEX idx_egress_logs_job_id ON egress_logs(job_id);
CREATE INDEX idx_egress_logs_timestamp ON egress_logs(timestamp);

-- Structured log events persisted from pod logs. Pod logs are ephemeral
-- (deleted with the Job), so the loop engine streams them into this table
-- in near-real-time. `GET /logs/:id` reads from here, not pod logs.
CREATE TABLE log_events (
    id              BIGSERIAL PRIMARY KEY,
    loop_id         UUID NOT NULL REFERENCES loops(id) ON DELETE CASCADE,
    timestamp       TIMESTAMPTZ NOT NULL DEFAULT now(),
    stage           loop_stage NOT NULL,
    round           INTEGER NOT NULL,
    level           TEXT NOT NULL DEFAULT 'info',  -- 'debug', 'info', 'warn', 'error'
    message         TEXT NOT NULL
);

CREATE INDEX idx_log_events_loop_id ON log_events(loop_id);
CREATE INDEX idx_log_events_loop_id_round ON log_events(loop_id, round);

-- Per-engineer, per-provider credential references. The actual secrets are
-- stored as K8s Secrets; this table tracks validity and metadata so the
-- loop engine can check credential status before dispatching.
CREATE TABLE engineer_credentials (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    engineer_id     UUID NOT NULL REFERENCES engineers(id) ON DELETE CASCADE,
    provider        TEXT NOT NULL,           -- 'claude' or 'openai'
    credential_ref  TEXT NOT NULL,           -- K8s Secret name: 'nemo-creds-{engineer}' (one secret per engineer, provider is a key within the secret)
    valid           BOOLEAN NOT NULL DEFAULT true,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX idx_engineer_credentials_engineer_provider
    ON engineer_credentials(engineer_id, provider);
CREATE INDEX idx_engineer_credentials_valid
    ON engineer_credentials(valid) WHERE valid = false;

-- Auto-update updated_at on row modification
CREATE OR REPLACE FUNCTION update_updated_at()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = now();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER trg_loops_updated_at
    BEFORE UPDATE ON loops
    FOR EACH ROW EXECUTE FUNCTION update_updated_at();

CREATE TRIGGER trg_jobs_updated_at
    BEFORE UPDATE ON jobs
    FOR EACH ROW EXECUTE FUNCTION update_updated_at();

CREATE TRIGGER trg_engineer_credentials_updated_at
    BEFORE UPDATE ON engineer_credentials
    FOR EACH ROW EXECUTE FUNCTION update_updated_at();

-- Cluster-level credentials for control plane operations (API auth, git host tokens).
-- These are NOT per-engineer; they are cluster-wide credentials used by the control
-- plane itself (e.g., to create/merge PRs, authenticate CLI requests).
CREATE TYPE credential_type AS ENUM ('api_key', 'mtls_cert', 'git_host_token');

CREATE TABLE cluster_credentials (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    type            credential_type NOT NULL,
    credential_ref  TEXT NOT NULL,           -- K8s Secret name or reference
    description     TEXT,                    -- human-readable label (e.g., "GitHub PAT for PR operations")
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_cluster_credentials_type ON cluster_credentials(type);

-- Retention: egress_logs older than 30 days auto-pruned by a scheduled task.
-- Implemented as a pg_cron job or control-plane scheduled task:
--   DELETE FROM egress_logs WHERE timestamp < now() - interval '30 days';
```

#### Column Design Rationale

- `loops.branch` uses a **partial unique index** on active states only (`WHERE state NOT IN ('converged', 'hardened', 'shipped', 'failed', 'cancelled')`). This replaces the old global `UNIQUE(branch)` which blocked resubmission of completed branches. Only one active loop may exist per branch; completed loops don't block new submissions.
- `loops.state` replaces the old `loop_status`. The full enum matches Lane A's implementation: pending, hardening, awaiting_approval, implementing, testing, reviewing, converged, hardened, shipped, failed, paused_remote_ahead, paused_force_deviated, awaiting_reauth, cancelled. The `sub_state` (dispatched/running/completed) tracks K8s Job lifecycle within a state.
- `loops.paused_from_state` and `loops.reauth_from_state` record which state the loop was in before pausing, enabling correct resume transitions.
- `loops.expected_sha` and `loops.actual_sha` store the SHA pair for divergence tracking. `expected_sha` is the SHA the control plane last dispatched against; `actual_sha` is the remote SHA observed during divergence detection.
- `loops.active_job_name` is set BEFORE K8s Job creation and cleared on completion. Lifecycle: (1) write job row + set active_job_name in DB, (2) create K8s Job, (3) on completion, clear active_job_name. This follows the persist-then-dispatch pattern.
- `loops.stage_retry_count` resets to 0 on stage transition. This is the per-stage retry count for infrastructure failures (OOM, timeout, eviction). `max_retries` comes from config (default 2). Separately, `jobs.attempt` is the attempt number for a specific job dispatch within the same stage+round. Both exist for different purposes: `stage_retry_count` tracks retries across the stage (reset on transition), while `jobs.attempt` identifies which try of a particular dispatch this is (for job naming and idempotency).
- `loops.feedback_path` stores the feedback file path directly rather than reconstructing it from convention. This avoids stale-path bugs discovered in Lane A (rounds 7, 8, 13).
- `loops.sha` is `NOT NULL`. When a loop is created, the branch is created from the base branch tip, and `sha` is set to that SHA immediately.
- `loops.ship_mode`, `loops.max_rounds_for_auto_merge`, `loops.merge_strategy`: ship mode fields from Lane A. When `ship_mode = true` and the loop converges within `max_rounds_for_auto_merge` rounds, the control plane auto-merges using `merge_strategy`.
- `loops.pr_url` and `loops.merge_sha`: persisted to avoid re-querying GitHub. Set when PR is created / merged respectively.
- `loops.ci_check_started_at`: timestamp for non-blocking CI polling. Set when CI check is initiated; the reconciler polls GitHub until CI completes or times out.
- `jobs.output_json` is JSONB containing stage output: verdict, test results, `affected_services` (computed from git diff by the control plane per Lane C decision), new SHA, session ID. This is the structured result of the job, separate from `verdict_json` (which is the review verdict specifically).
- `jobs.attempt` tracks retry attempts within the same stage+round. Job name format: `nemo-{loop_id_short}-{stage}-r{round}-t{attempt}`. The unique index on `(loop_id, stage, round, attempt)` prevents collision on retry.
- `jobs.feedback_path` stores the feedback file path for this specific job.
- `jobs.verdict_json` is JSONB (not a separate table) because the verdict schema is owned by the agent image and may evolve. Structured querying of verdicts is not a V1 requirement.
- `jobs.k8s_job_name` is `UNIQUE` to enable idempotent reconciliation: if the control plane restarts, it can match running K8s jobs back to DB rows.
- `engineers.model_preferences` is JSONB (`{"implementor": "claude-opus-4", "reviewer": "gpt-5.4"}`) because model names are free-form strings that change frequently.
- `loops.auto_approve`: set at loop creation time from CLI flags (`nemo ship --harden` implies `auto_approve = true`; `nemo start --harden --auto-approve` sets it explicitly). When the harden phase converges and `auto_approve = true`, the loop engine skips `awaiting_approval` and transitions directly to `implementing`. When `false`, the loop enters `awaiting_approval` as normal.
- `loops.cancel_requested`, `loops.approve_requested`, `loops.resume_requested`, `loops.force_resume`: boolean flags set by the API server and read by the loop engine on the next reconciliation tick. This is the communication mechanism between the two deployments (they share only Postgres, no direct RPC). Flags are reset by the loop engine after processing. `force_resume` is set alongside `resume_requested` when `nemo resume --force` is used; the loop engine checks `force_resume` before allowing resume from `paused_force_deviated` (rejects if `force_resume = false`).
- `loops.needs_human_review`: set when max_rounds exceeded or CI fails in ship mode. Queryable by `nemo status` to highlight loops that need engineer attention. Distinct from terminal state -- a loop can be `converged` (PR created) with `needs_human_review = true`.
- `log_events`: structured log events persisted from pod logs. Pod logs are ephemeral and disappear after K8s Job deletion, so the loop engine streams them into this table in near-real-time. `GET /logs/:id` reads from here, not from pod logs. Columns: `stage` and `round` enable filtering (`?round=N&stage=implement`). `level` supports filtering by severity.
- `engineer_credentials`: tracks per-engineer, per-provider credential references and validity. The actual secrets are stored in a single K8s Secret per engineer (`nemo-creds-{engineer}`), with keys named by provider (`claude`, `openai`). The `credential_ref` column stores the K8s Secret name (always `nemo-creds-{engineer}`). This table enables the loop engine to check credential status before dispatching (avoiding wasted job starts with expired creds) and enables the `awaiting_reauth` -> resume flow when `nemo auth` updates credentials.
- `egress_logs` uses `BIGSERIAL` because it is append-only, high-volume, and never updated. Split into `host`, `port`, `bytes_sent`, `bytes_received`, `protocol`, `status_code` for structured querying and alerting.

#### Terminal State Protection

Terminal states (`converged`, `hardened`, `shipped`, `failed`, `cancelled`) must never be overwritten. Enforced at the application layer:

```sql
-- All state transitions use this pattern:
UPDATE loops SET state = $new_state, updated_at = now()
WHERE id = $loop_id AND state NOT IN ('converged', 'hardened', 'shipped', 'failed', 'cancelled')
RETURNING id;
-- If RETURNING yields no rows, the transition is rejected (loop already terminal).
```

This was a Lane A convergence learning (round 18): cancel/fail transitions could overwrite terminal states, causing ghost loops.

#### Migration Strategy

Migrations live in `control-plane/migrations/` as `{timestamp}_{description}.sql` files.

1. `cargo sqlx prepare` generates offline query metadata (checked into repo).
2. No separate migration binary or manual step for single-deployment setups.

**Split deployment safety:** The API server and loop engine are separate k3s Deployments that share a Postgres database. Migrations must run BEFORE either deployment starts to avoid schema mismatches. Implementation: migrations run as a K8s Job with `helm.sh/hook: pre-upgrade` (or equivalently, an init container on one of the deployments). The migration Job runs `sqlx migrate run` and exits. Both the API server and loop engine Deployments wait for the migration Job to complete before starting (via `helm.sh/hook-weight` ordering or init container dependency). Neither binary runs migrations on its own startup -- they only verify the schema version matches expectations. This prevents races where one deployment starts with the new schema while the other is still on the old one.

### Git Operations Module

```
control-plane/src/git/
    mod.rs          -- pub mod bare_repo; pub mod branch;
    bare_repo.rs    -- BareRepo struct
    branch.rs       -- branch naming, divergence detection
```

#### BareRepo Struct

```rust
pub struct BareRepo {
    path: PathBuf,              // e.g., /data/bare-repo.git
    remote_url: String,
    worktree_mutex: tokio::sync::Mutex<()>,
}
```

**Lifecycle of a job's git operations:**

1. Loop engine calls `bare_repo.prepare_worktree(branch, base_ref)` (fix #7: atomic API combining fetch_and_resolve + create_worktree):
   - Acquires worktree mutex
   - Runs `git fetch --prune`
   - Resolves `base_ref` to a SHA
   - Creates worktree at the resolved SHA in detached HEAD mode: `git worktree add --detach <path> <sha>`
   - Inside the worktree, creates the named branch: `git checkout -b {branch}` (where `branch` is already the full name, e.g., `agent/alice/invoice-cancel-a1b2c3d4`)
   - Releases worktree mutex
   - Returns the worktree path and resolved SHA
2. K8s job runs inside the worktree. The mutex is NOT held during job execution. Multiple jobs run concurrently on different worktrees.
3. On job completion, loop engine calls `bare_repo.cleanup_worktree(path)` -- acquires mutex, runs `git worktree remove --force`, then `git worktree prune`, releases mutex

**Why `prepare_worktree` is a single atomic API (fix #7):** The old two-step `fetch_and_resolve()` + `create_worktree()` required the caller to hold the mutex correctly. `prepare_worktree(branch, base_ref) -> (PathBuf, String)` encapsulates the entire sequence: acquire mutex, fetch, resolve, create worktree at detached HEAD, checkout named branch, release mutex. This eliminates the class of bugs where the caller forgets to hold the mutex or drops it between steps.

**No mutex during job execution:** The mutex is only needed for git worktree create/delete operations (which take a file lock on `.git/worktrees/`). Jobs run on independent worktrees and do not contend on the bare repo. Holding the mutex for the full job lifetime would serialize all jobs, defeating the purpose of parallel execution.

**Branch name used directly (fix #3, round 2 fix #1):** `branch_name()` returns the full branch including the `agent/` prefix (e.g., `agent/alice/invoice-cancel-a1b2c3d4`). The checkout command is `git checkout -b {branch}` -- NOT `git checkout -b agent/{branch}`. Adding an extra `agent/` prefix would create `agent/agent/alice/...`.

**Detached HEAD then named branch (fix #3):** The worktree is created at the resolved SHA in detached HEAD mode (`git worktree add --detach`), then `git checkout -b {branch}` creates the named branch inside the worktree. This ensures: (1) the worktree starts at the exact resolved SHA, (2) the agent commits to a named branch (not detached HEAD), and (3) `git push origin {branch}` works without extra refspec configuration.

**Why mutex instead of async semaphore:** `git worktree add` takes a file lock on the bare repo (`.git/worktrees/`). Concurrent calls block at the filesystem level anyway. The mutex makes the serialization explicit and avoidable (no spawning N processes that all block on the same file lock).

**Fetch strategy (fix #12):** All fetches happen per-job via `prepare_worktree()`. There is no background CronJob for fetching. Lane C's fetch CronJob design is superseded by this per-job fetch approach. This ensures the worktree always reflects the latest remote state at dispatch time.

#### Branch Naming

```rust
pub fn branch_name(engineer: &str, spec_path: &str, original_spec_content: &[u8]) -> String {
    let slug = spec_slug(spec_path);       // "invoice-cancel" from "specs/billing/invoice-cancel.md"
    let hash = short_hash(original_spec_content); // first 8 hex chars of SHA-256(original spec file content at submission time)
    format!("agent/{engineer}/{slug}-{hash}")
}
```

The short-hash is computed from the ORIGINAL submitted spec file content (at submission time), making branch names immutable across harden rounds. The hash disambiguates when two specs produce the same slug (unlikely but possible across categories). Note: design doc examples are being updated to include hash suffix.

#### Divergence Detection

Called by the loop engine before dispatching each job:

```rust
pub enum DivergenceResult {
    /// Normal operation: agent committed, local is ahead of remote. Not a divergence.
    LocalAhead,
    /// Engineer pushed additional commits. Fast-forward is possible, no work lost.
    /// Always pauses (status → paused_remote_ahead). Engineer decides via `nemo resume`.
    RemoteAhead { local_sha: String, remote_sha: String },
    /// Histories diverged (force push or rebase). Resuming discards local commits.
    /// Always pauses (status → paused_force_deviated). Requires `nemo resume --force`.
    ForceDeviated { local_sha: String, remote_sha: String },
    /// Branch deleted on remote. Recovery: cancel only (branch is gone, work is lost).
    RemoteGone,
}

impl BareRepo {
    pub async fn detect_divergence(&self, branch: &str) -> Result<DivergenceResult>;
}
```

Detection method: compare `refs/heads/{branch}` against `refs/remotes/origin/{branch}` after fetch. Use `git merge-base --is-ancestor` to classify:
- If local is ancestor of remote: `RemoteAhead` (fast-forward possible).
- If remote is ancestor of local: `LocalAhead` (normal agent operation).
- If neither is ancestor: `ForceDeviated` (histories diverged).
- If the remote ref doesn't exist: `RemoteGone`.

On `RemoteAhead`: set `loops.state = 'paused_remote_ahead'`, `loops.paused_from_state` to the current state, write the SHA mismatch to the loop record. The API exposes this so the CLI can show "Engineer pushed new commits. `nemo resume <loop-id>` to fast-forward or `nemo cancel <loop-id>`."

On `ForceDeviated`: set `loops.state = 'paused_force_deviated'`, `loops.paused_from_state` to the current state, write the SHA mismatch to the loop record. The API exposes this so the CLI can show "Branch histories diverged. `nemo resume --force <loop-id>` (discards agent work) or `nemo cancel <loop-id>`."

On `RemoteGone` (fix #8): the branch was deleted on the remote. The work is gone. Recovery: cancel only. `nemo cancel <loop-id>` transitions to `cancelled`. Resume is not possible because the branch no longer exists. The CLI shows: "Branch '{branch}' was deleted on the remote. Use `nemo cancel <loop-id>` to cancel this loop."

**paused_remote_ahead resume flow:**
- `nemo resume <loop-id>`: re-fetches, fast-forwards `loops.sha` to current remote branch tip (no agent work is lost), re-dispatches the current stage. Transitions back to `paused_from_state`.
- `nemo cancel <loop-id>`: transitions to `cancelled`.
- No other transitions are valid from `paused_remote_ahead`.

**paused_force_deviated resume flow:**
- `nemo resume --force <loop-id>`: shows which local commits will be discarded, then re-fetches and resets `loops.sha` to current remote branch tip, re-dispatches the current stage. Transitions back to `paused_from_state`. Without `--force`, the command is rejected with an explanation of what will be lost.
- `nemo cancel <loop-id>`: transitions to `cancelled`.
- No other transitions are valid from `paused_force_deviated`.

### Config Loading Module

```
control-plane/src/config/
    mod.rs          -- pub mod cluster; pub mod repo; pub mod engineer; pub mod merged;
    cluster.rs      -- ClusterConfig
    repo.rs         -- RepoConfig (nemo.toml)
    engineer.rs     -- EngineerConfig (~/.nemo/config.toml)
    merged.rs       -- MergedConfig, merge logic
```

#### Structs

```rust
// Fix #15: Cluster config TOML has a [cluster] wrapper.
// File format:
//   [cluster]
//   domain = "nemo.example.com"
//   max_cluster_jobs = 20
#[derive(Deserialize)]
pub struct ClusterFile {
    pub cluster: ClusterConfig,
}

#[derive(Deserialize)]
pub struct ClusterConfig {
    pub node_size: Option<String>,
    pub provider: Option<String>,
    pub domain: String,
    pub default_implementor: Option<String>,
    pub default_reviewer: Option<String>,
    pub max_parallel_loops_cap: Option<u32>,  // hard ceiling per engineer
    pub max_cluster_jobs: Option<u32>,        // hard ceiling cluster-wide; enforced via pg_advisory_xact_lock (fix #11)
}

#[derive(Deserialize)]
pub struct RepoConfig {
    pub repo: RepoMeta,        // name, default_branch
    pub models: Option<ModelConfig>,
    pub limits: Option<LimitsConfig>,
    pub services: HashMap<String, ServiceConfig>,
    pub ship: Option<ShipConfig>,
    pub harden: Option<HardenConfig>,
    pub timeouts: Option<TimeoutsConfig>,
}

#[derive(Deserialize)]
pub struct ShipConfig {
    pub allowed: Option<bool>,                     // enable nemo ship (default: false)
    pub require_passing_ci: Option<bool>,          // wait for CI before merge (default: true)
    pub require_harden: Option<bool>,              // force --harden on nemo ship (default: false)
    pub max_rounds_for_auto_merge: Option<u32>,    // threshold (default: 5)
    pub merge_strategy: Option<String>,            // "squash" | "merge" | "rebase" (default: "squash")
}

#[derive(Deserialize)]
pub struct HardenConfig {
    pub auto_merge_spec_pr: Option<bool>,          // auto-merge the hardened spec PR (default: true)
    pub merge_strategy: Option<String>,            // "squash" | "merge" | "rebase" for spec PRs (default: "squash")
}

#[derive(Deserialize)]
pub struct TimeoutsConfig {
    pub implement_timeout_min: Option<u32>,        // implement stage timeout in minutes (default: 30)
    pub review_timeout_min: Option<u32>,           // review stage timeout in minutes (default: 15)
    pub test_timeout_min: Option<u32>,             // test stage timeout in minutes (default: 30)
    pub audit_timeout_min: Option<u32>,            // spec-audit stage timeout in minutes (default: 15)
    pub revise_timeout_min: Option<u32>,           // spec-revise stage timeout in minutes (default: 15)
}

#[derive(Deserialize)]
pub struct EngineerConfig {
    pub identity: Option<IdentityConfig>,   // name, email
    pub models: Option<ModelConfig>,
    pub limits: Option<LimitsConfig>,
}

pub struct MergedConfig {
    pub implementor_model: String,
    pub reviewer_model: String,
    pub max_parallel_loops: u32,
    pub max_rounds_harden: u32,
    pub max_rounds_implement: u32,
    pub services: HashMap<String, ServiceConfig>,
    // Ship settings (from [ship] in nemo.toml, all with defaults)
    pub ship_allowed: bool,                    // default: false
    pub ship_require_passing_ci: bool,         // default: true
    pub ship_require_harden: bool,             // default: false
    pub ship_max_rounds_for_auto_merge: u32,   // default: 5
    pub ship_merge_strategy: String,           // default: "squash"
    // Harden settings (from [harden] in nemo.toml, all with defaults)
    pub harden_auto_merge_spec_pr: bool,       // default: true
    pub harden_merge_strategy: String,         // default: "squash"
    // Timeouts (from [timeouts] in nemo.toml, all with defaults)
    pub implement_timeout_min: u32,            // default: 30
    pub review_timeout_min: u32,               // default: 15
    pub test_timeout_min: u32,                 // default: 30
    pub audit_timeout_min: u32,                // default: 15
    pub revise_timeout_min: u32,               // default: 15
}
```

#### Merge Algorithm

```rust
impl MergedConfig {
    pub fn merge(
        cluster: &ClusterConfig,
        repo: &RepoConfig,
        engineer: Option<&EngineerConfig>,
    ) -> Result<Self, ConfigError>;
}
```

For each scalar field, take the highest-priority non-None value. For limits, apply `min(engineer_value, cluster_cap)`. If a required field (like `implementor_model`) is None at all three layers, return `ConfigError::MissingField { field, role }`.

**Collection merge rules:**
- `services` HashMap: deep merge. Repo defines services; engineer cannot override existing service configs, only add new services. If engineer defines a service with the same key as one already defined in the repo config, it is ignored (repo wins for services) **with a validation warning surfaced by `nemo config` and `nemo start`** (fix #17: no longer silent).
- `models`: last-writer-wins. Engineer overrides repo, repo overrides cluster.

**Model preferences authority:** `~/.nemo/config.toml` is authoritative for model preferences. The `engineers` table stores a JSONB cache that is synced on `nemo auth`. On conflict, the config file wins.

#### Cluster Config Loading

Two sources, checked in order:

1. File at path `$NEMO_CLUSTER_CONFIG` (K8s ConfigMap mounted as a file, e.g., `/etc/nemo/cluster.toml`)
2. Environment variables: `NEMO_CLUSTER_DOMAIN`, `NEMO_CLUSTER_DEFAULT_IMPLEMENTOR`, etc.

If the file exists, it takes precedence. Environment variables fill in any fields the file doesn't set.

#### Service Detection (`nemo init`)

Scan rules (each produces a `ServiceConfig` entry):

| Marker File | Service Type | Default Test Command |
|---|---|---|
| `Cargo.toml` | rust | `cargo test` |
| `package.json` | node | `npm test` |
| `go.mod` | go | `go test ./...` |
| `pyproject.toml` | python | `pytest` |
| `build.sbt` | jvm | `sbt test` |
| `foundry.toml` | solidity | `forge test` |
| `composer.json` | php | `composer test` |
| `Makefile` (alone) | generic | `make test` |

Scan depth: configurable via `nemo init --depth N` (default 2, i.e., monorepo root + one level of subdirectories). Each directory containing a marker becomes a service. The service name is the directory name (or the repo name if the marker is at root). Warns when zero services are detected.

`nemo init` writes the generated `nemo.toml` to stdout and prompts the engineer to review before writing to disk. It never overwrites an existing `nemo.toml` without `--force`.

### Dispatch Locking (fix #11)

The `max_cluster_jobs` limit must be enforced atomically. The dispatch transaction uses a Postgres advisory lock:

```sql
BEGIN;
SELECT pg_advisory_xact_lock(42);  -- only one dispatcher at a time
SELECT COUNT(*) FROM jobs WHERE status IN ('pending', 'running') AS active_count;
-- If active_count >= max_cluster_jobs, abort (ROLLBACK)
-- Otherwise, INSERT the new job row, set loops.active_job_name, COMMIT
-- Advisory lock released automatically on COMMIT/ROLLBACK
```

This replaces the old `SELECT COUNT(*) ... FOR UPDATE` approach which required a dedicated "dispatch lock" row. The advisory lock is simpler and prevents TOCTOU races between counting and inserting.

### Branch Lookup for Inspect (Lane A `/inspect` endpoint)

The `GET /inspect?branch=...` endpoint accepts the full branch name as a query parameter (not a path segment) because branch names contain slashes (e.g., `agent/alice/slug-hash`). Since branches can be resubmitted after terminal states, multiple loops may share the same branch.

```sql
-- get_loop_by_branch_any(): returns the most recent loop for a branch.
-- Active loops are preferred; if none active, returns the most recent terminal loop.
SELECT * FROM loops
WHERE branch = $1
ORDER BY
  CASE WHEN state NOT IN ('converged', 'hardened', 'shipped', 'failed', 'cancelled') THEN 0 ELSE 1 END,
  created_at DESC
LIMIT 1;

-- With ?all=true: returns ALL loops for a branch, ordered by created_at DESC.
SELECT * FROM loops
WHERE branch = $1
ORDER BY created_at DESC;
```

### Persist-Then-Dispatch Pattern (Lane A learning)

All dispatch operations follow this sequence:

1. **Persist:** Write the job row to `jobs` table, set `loops.active_job_name` and `loops.sub_state = 'dispatched'` in a single transaction.
2. **Create:** Create the K8s Job. If creation fails, the DB row exists and the reconciler will retry.
3. **Never:** create a K8s Job without a corresponding DB row. Orphaned jobs are unrecoverable.

This was the #1 systemic bug pattern in Lane A (round 19: 8 call sites created K8s resources before persisting state).

## Edge Cases

| Scenario | Expected Behavior |
|---|---|
| Bare repo path does not exist on startup | Control plane fails to start with clear error: "Bare repo not found at {path}. Run initial clone first." |
| `git fetch` fails (network error, auth failure) | Job dispatch is retried with backoff (30s, 120s). After 3 failures, loop is marked `failed` with `error_message = "fetch failed: {reason}"`. |
| Disk full during `create_worktree` | `git worktree add` returns non-zero. Control plane logs the error, marks the job `failed`, retries once after 60s (in case temp files were cleaned). On second failure, loop fails. |
| Bare repo corruption (bad objects, broken refs) | Detected by non-zero exit from git commands. Control plane logs error and marks loop `failed`. Recovery: manual re-clone of bare repo (out of scope for V1 auto-recovery). |
| Two loops submitted for the same spec by the same engineer | Second submission rejected by partial unique index `idx_loops_active_branch` (active states only). API returns 409 Conflict with message "Active loop already exists for branch {branch}". Completed/failed/cancelled loops do not block resubmission (fix #2, #18). |
| Engineer pushes to agent branch during active loop (fast-forward) | Detected as `RemoteAhead` by `detect_divergence()`. Loop paused (`paused_remote_ahead`). Engineer uses `nemo resume <loop-id>` to fast-forward (no work lost). |
| Engineer force-pushes to agent branch during active loop | Detected as `ForceDeviated` by `detect_divergence()`. Loop paused (`paused_force_deviated`). Engineer must `nemo resume --force <loop-id>` (discards agent commits, accepts remote state) or `nemo cancel <loop-id>`. |
| Engineer deletes agent branch on remote | Detected as `RemoteGone` by `detect_divergence()`. Loop transitions to `cancelled`. No resume possible (branch is gone, work is lost). |
| Resubmitting a spec after previous loop completed | Allowed: partial unique index only blocks active loops. New loop created with fresh branch. |
| K8s Job returns malformed/unparseable results | Job status set to `errored`. Loop engine treats as retryable up to `stage_retry_count` limit. |
| Credential expires during active loop | State transitions to `awaiting_reauth`, `reauth_from_state` records prior state. Engineer re-auths via `nemo auth`, loop resumes from `reauth_from_state`. |
| `nemo.toml` has unknown fields | `toml` deserialization with `#[serde(deny_unknown_fields)]` returns a parse error naming the unknown field. This catches typos early. |
| `nemo.toml` references a service path that doesn't exist | Validated at config load time. Error: "Service '{name}' path '{path}' does not exist in the repo." |
| Engineer config sets model to empty string | Treated as None (not set). The merge algorithm skips empty strings. |
| Postgres connection lost during loop execution | `sqlx` PgPool retries connections automatically. If the pool is exhausted for > 30s, pending DB operations fail and the loop engine logs the error. Loops resume from last known state when the connection recovers (state is already persisted). |
| Worktree mutex held while control plane receives SIGTERM | `tokio::sync::Mutex` is dropped on process exit. The lock file left by `git worktree add` is cleaned up by `git worktree prune` on next startup. |
| Migration fails mid-apply | `sqlx migrate` runs each migration in a transaction. Failed migration rolls back. Control plane refuses to start until the migration issue is resolved manually. |

## Error Handling

| Error | Detection | Response |
|---|---|---|
| Postgres connection refused on startup | `PgPool::connect` returns error | Control plane exits with code 1 and message "Cannot connect to Postgres at {url}" |
| Migration version conflict (two developers add same timestamp) | `sqlx migrate` detects duplicate | Startup fails. Developer must renumber the migration. |
| `git worktree add` returns non-zero | Exit code check after `Command::new("git")` | Release mutex, return `GitError::WorktreeCreateFailed { stderr }` |
| TOML parse error in any config layer | `toml::from_str` returns error | Return `ConfigError::ParseFailed { layer, path, detail }` with the exact line and column |
| Branch name collision (two specs produce same slug-hash) | Partial unique index `idx_loops_active_branch` | API returns 409 for active loops only. Astronomically unlikely with 8-char hex hash (4 billion combinations) but handled. |
| Worktree path already exists (stale from crash) | `git worktree add` fails | Delete stale path, run `git worktree prune`, retry once. If retry fails, return error. |

## Out of Scope

- Automatic bare repo re-clone on corruption (V2)
- Down migrations / schema rollback (forward-only by policy)
- Multi-cluster config federation
- Git LFS support
- Partial clone / shallow clone optimizations
- Config hot-reload without control plane restart (V2)
- Postgres replication or HA (single-node V1)
- `nemo init` for polyglot monorepos with nested build systems beyond the configured depth

## Acceptance Criteria

- [ ] `sqlx migrate run` applies all migrations to a fresh Postgres 15+ database without errors
- [ ] `cargo sqlx prepare --check` passes in CI (offline query verification)
- [ ] `BareRepo::prepare_worktree(branch, base_ref)` acquires mutex, fetches, resolves SHA, creates worktree at detached HEAD, checks out named branch using `git checkout -b {branch}` (no double `agent/` prefix), releases mutex, and returns worktree path
- [ ] Worktree is created at exact resolved SHA; agent commits to the full branch name (e.g., `agent/alice/slug-hash`), not detached HEAD
- [ ] Worktree mutex is released after `prepare_worktree` returns (NOT held during job execution)
- [ ] Multiple jobs run concurrently on different worktrees without blocking each other
- [ ] `BareRepo::cleanup_worktree()` acquires mutex, removes the worktree directory, cleans up `.git/worktrees` metadata, releases mutex
- [ ] Concurrent `prepare_worktree` calls are serialized (second call waits for first to complete, no git lock errors)
- [ ] Branch names match pattern `agent/{engineer}/{slug}-{hash}` for all valid inputs
- [ ] `detect_divergence()` returns `RemoteAhead` when engineer fast-forward-pushed, `ForceDeviated` when histories diverged, `LocalAhead` for normal agent operation, and `RemoteGone` when branch is deleted
- [ ] `RemoteAhead` sets state to `paused_remote_ahead` with `paused_from_state` recorded; `ForceDeviated` sets state to `paused_force_deviated`
- [ ] `RemoteGone` transitions to `cancelled` (no resume possible)
- [ ] `nemo resume <loop-id>` fast-forwards on `paused_remote_ahead` (no `--force` required)
- [ ] `nemo resume --force <loop-id>` required for `paused_force_deviated`; without `--force`, command is rejected with explanation of data loss
- [ ] `nemo resume` on `paused_force_deviated` without `--force` shows which commits will be discarded
- [ ] Partial unique index `idx_loops_active_branch` allows resubmission of completed/failed/cancelled branches
- [ ] Phase/stage CHECK constraint rejects invalid combinations (e.g., harden + implementing)
- [ ] Terminal state protection: UPDATE with `WHERE state NOT IN (terminal states)` returns 0 rows when loop is already terminal
- [ ] `ON DELETE CASCADE` propagates from loops to jobs and from jobs to egress_logs
- [ ] `egress_logs` retention: records older than 30 days are pruned by scheduled task
- [ ] `egress_logs` columns: host, port, protocol, status_code, bytes_sent, bytes_received, method
- [ ] `updated_at` triggers fire on row updates for both loops and jobs tables
- [ ] `max_cluster_jobs` in ClusterConfig is enforced via `pg_advisory_xact_lock(42)` in the dispatch transaction
- [ ] `jobs.output_json` stores stage output including `affected_services` (computed from git diff)
- [ ] `jobs.attempt` increments on retry; job name format `nemo-{loop_id_short}-{stage}-r{round}-t{attempt}`
- [ ] `jobs.feedback_path` stores feedback file path (not reconstructed)
- [ ] `loops.stage_retry_count` resets to 0 on stage transition
- [ ] `loops.active_job_name` is set BEFORE K8s Job creation, cleared on completion (persist-then-dispatch)
- [ ] `loops.ship_mode`, `max_rounds_for_auto_merge`, `merge_strategy` control auto-merge behavior
- [ ] `loops.pr_url` set on PR creation; `loops.merge_sha` set on merge
- [ ] `loops.ci_check_started_at` set when CI polling begins
- [ ] `loops.expected_sha` and `actual_sha` set on divergence detection
- [ ] `job_status` enum includes `errored` variant for malformed results
- [ ] Cluster config TOML parsed with `[cluster]` wrapper (`ClusterFile { cluster: ClusterConfig }`)
- [ ] CLI validates `nemo.toml` locally before submit; API revalidates; missing repo config at dispatch is terminal failure
- [ ] Service key collision in engineer config produces a warning in `nemo config` and `nemo start` (not silent)
- [ ] When `harden_only` loop converges harden phase, state transitions to `hardened` (phase stays `harden`)
- [ ] When non-`harden_only` loop converges harden phase, state transitions to `awaiting_approval` until engineer approves
- [ ] `MergedConfig::merge()` correctly applies three-layer override: engineer > repo > cluster
- [ ] `MergedConfig::merge()` warns on engineer-defined services that collide with repo-defined service keys (repo wins)
- [ ] `MergedConfig::merge()` caps engineer `max_parallel_loops` at cluster `max_parallel_loops_cap`
- [ ] `MergedConfig::merge()` returns `ConfigError::MissingField` when no model is configured for a required role
- [ ] `nemo init` detects at least `Cargo.toml`, `package.json`, and `go.mod` in a test monorepo and generates correct `[services.*]` TOML
- [ ] `nemo init` refuses to overwrite existing `nemo.toml` without `--force`
- [ ] Control plane starts successfully with only cluster config (no repo or engineer config loaded at boot)
- [ ] Control plane refuses to start if Postgres is unreachable or migrations fail
- [ ] No background fetch CronJob; all fetches are per-job via `prepare_worktree()`
- [ ] `loops.cancel_requested`, `approve_requested`, `resume_requested`, `force_resume` flags exist and default to false
- [ ] `force_resume` is set to true only when `nemo resume --force` is called; loop engine rejects resume from `paused_force_deviated` when `force_resume = false`
- [ ] `loops.needs_human_review` flag set when max_rounds exceeded or CI fails in ship mode
- [ ] `nemo status` highlights loops with `needs_human_review = true`
- [ ] `log_events` table stores structured log events with loop_id, stage, round, level, message
- [ ] `GET /logs/:id` reads from `log_events` table, not pod logs
- [ ] `engineer_credentials` table tracks per-engineer, per-provider credential references with validity flag
- [ ] `engineer_credentials` unique on `(engineer_id, provider)`
- [ ] `updated_at` trigger fires on `engineer_credentials` row updates
- [ ] Migrations run as a K8s Job (pre-upgrade hook) BEFORE either API server or loop engine starts
- [ ] Neither API server nor loop engine runs migrations on its own startup
- [ ] `loops.stage_retry_count` is per-stage retry budget (resets on stage transition); `jobs.attempt` is per-dispatch attempt number; both exist independently

## Open Questions

- [x] Should `egress_logs` be stored in Postgres or shipped to a separate log sink (e.g., file-based, rotated)? **Decision: Postgres, with 30-day retention.** `egress_logs` rows older than 30 days are auto-pruned by a scheduled task. `ON DELETE CASCADE` from jobs ensures cleanup on loop deletion.
- [x] Should the `loops` table track `affected_services` (JSONB array) to enable filtering loops by service on the dashboard? **Decision: `affected_services` is computed from git diff by the control plane (Lane C decision) and stored in `jobs.output_json`, not as a top-level loops column.** The control plane computes it at job completion time from the git diff.
