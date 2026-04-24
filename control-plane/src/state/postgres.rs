use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::pool::PoolConnection;
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Postgres, Row};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

use super::{LoopFlag, StateStore};
use crate::error::Result;
use crate::types::{
    EngineerCredential, LogEvent, LoopKind, LoopRecord, LoopState, RoundRecord, SubState,
};

/// Postgres-backed state store.
#[derive(Debug, Clone)]
pub struct PgStateStore {
    pool: PgPool,
    /// Dedicated connections holding session-scoped advisory locks.
    /// Keyed by advisory lock key (derived from loop UUID).
    lock_conns: Arc<Mutex<HashMap<i64, PoolConnection<Postgres>>>>,
}

impl PgStateStore {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            lock_conns: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Run database migrations.
    pub async fn run_migrations(&self) -> std::result::Result<(), sqlx::migrate::MigrateError> {
        sqlx::migrate!("./migrations").run(&self.pool).await
    }
}

fn loop_state_str(s: LoopState) -> &'static str {
    match s {
        LoopState::Pending => "PENDING",
        LoopState::Hardening => "HARDENING",
        LoopState::AwaitingApproval => "AWAITING_APPROVAL",
        LoopState::Implementing => "IMPLEMENTING",
        LoopState::Testing => "TESTING",
        LoopState::Reviewing => "REVIEWING",
        LoopState::Converged => "CONVERGED",
        LoopState::Failed => "FAILED",
        LoopState::Cancelled => "CANCELLED",
        LoopState::Paused => "PAUSED",
        LoopState::AwaitingReauth => "AWAITING_REAUTH",
        LoopState::Hardened => "HARDENED",
        LoopState::Shipped => "SHIPPED",
    }
}

fn sub_state_str(s: SubState) -> &'static str {
    match s {
        SubState::Dispatched => "DISPATCHED",
        SubState::Running => "RUNNING",
        SubState::Completed => "COMPLETED",
    }
}

fn loop_kind_str(k: LoopKind) -> &'static str {
    match k {
        LoopKind::Harden => "harden",
        LoopKind::Implement => "implement",
    }
}

fn row_to_loop_record(row: &PgRow) -> Result<LoopRecord> {
    let state = row.get::<LoopState, _>("state");
    let paused_from_state = row.get::<Option<LoopState>, _>("paused_from_state");
    let reauth_from_state = row.get::<Option<LoopState>, _>("reauth_from_state");
    let failed_from_state = row
        .try_get::<Option<LoopState>, _>("failed_from_state")
        .ok()
        .flatten();
    let typed_opencode_session_id = row
        .try_get::<Option<String>, _>("opencode_session_id")
        .ok()
        .flatten();
    let typed_claude_session_id = row
        .try_get::<Option<String>, _>("claude_session_id")
        .ok()
        .flatten();
    let legacy_session_id = row
        .try_get::<Option<String>, _>("session_id")
        .ok()
        .flatten();
    let (opencode_session_id, claude_session_id) = resolve_session_columns_for_record(
        state,
        paused_from_state,
        reauth_from_state,
        failed_from_state,
        typed_opencode_session_id,
        typed_claude_session_id,
        legacy_session_id,
    );

    Ok(LoopRecord {
        id: row.get("id"),
        engineer: row.get("engineer"),
        spec_path: row.get("spec_path"),
        spec_content_hash: row.get("spec_content_hash"),
        branch: row.get("branch"),
        // Postgres enum columns (loop_kind, loop_state, sub_state) decode
        // through the sqlx::Type derives on LoopKind/LoopState/SubState in
        // crate::types. Decoding them as `String` panics with
        // "mismatched types ... is not compatible with SQL type `loop_kind`".
        kind: row.get::<LoopKind, _>("kind"),
        state,
        sub_state: row.get::<Option<SubState>, _>("sub_state"),
        round: row.get("round"),
        max_rounds: row.get("max_rounds"),
        harden: row.get("harden"),
        harden_only: row.get("harden_only"),
        auto_approve: row.get("auto_approve"),
        cancel_requested: row.get("cancel_requested"),
        approve_requested: row.get("approve_requested"),
        resume_requested: row.get("resume_requested"),
        paused_from_state,
        reauth_from_state,
        failed_from_state,
        failure_reason: row.get("failure_reason"),
        current_sha: row.get("current_sha"),
        // Dual-read: during a rolling deploy old pods still write only the
        // legacy session_id column. For active/resumable states, prefer a
        // stage-compatible legacy value over stale typed columns so a new pod
        // can continue from the freshest session written by an old pod.
        opencode_session_id,
        claude_session_id,
        active_job_name: row.get("active_job_name"),
        retry_count: row.get("retry_count"),
        ship_mode: row.get("ship_mode"),
        model_implementor: row.get("model_implementor"),
        model_reviewer: row.get("model_reviewer"),
        merge_sha: row.get("merge_sha"),
        merged_at: row.get("merged_at"),
        hardened_spec_path: row.get("hardened_spec_path"),
        spec_pr_url: row.get("spec_pr_url"),
        resolved_default_branch: row.try_get("resolved_default_branch").ok().flatten(),
        stage_timeout_secs: row
            .try_get::<Option<i32>, _>("stage_timeout_secs")
            .ok()
            .flatten(),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}

fn row_to_round_record(row: &PgRow) -> RoundRecord {
    RoundRecord {
        id: row.get("id"),
        loop_id: row.get("loop_id"),
        round: row.get("round"),
        stage: row.get("stage"),
        input: row.get("input"),
        output: row.get("output"),
        started_at: row.get("started_at"),
        completed_at: row.get("completed_at"),
        duration_secs: row.get("duration_secs"),
        job_name: row.get("job_name"),
    }
}

fn row_to_log_event(row: &PgRow) -> LogEvent {
    LogEvent {
        id: row.get("id"),
        loop_id: row.get("loop_id"),
        round: row.get("round"),
        stage: row.get("stage"),
        timestamp: row.get("timestamp"),
        line: row.get("line"),
    }
}

fn row_to_credential(row: &PgRow) -> EngineerCredential {
    EngineerCredential {
        id: row.get("id"),
        engineer: row.get("engineer"),
        provider: row.get("provider"),
        credential_ref: row.get("credential_ref"),
        valid: row.get("valid"),
        updated_at: row.get("updated_at"),
    }
}

fn effective_session_state(
    state: LoopState,
    paused_from_state: Option<LoopState>,
    reauth_from_state: Option<LoopState>,
    failed_from_state: Option<LoopState>,
) -> LoopState {
    match state {
        LoopState::Paused => paused_from_state.unwrap_or(state),
        LoopState::AwaitingReauth => reauth_from_state.unwrap_or(state),
        LoopState::Failed => failed_from_state.unwrap_or(state),
        _ => state,
    }
}

fn split_legacy_session_id(legacy_session_id: Option<String>) -> (Option<String>, Option<String>) {
    match legacy_session_id {
        Some(session_id) if session_id.starts_with("ses_") => (Some(session_id), None),
        Some(session_id) if uuid::Uuid::try_parse(&session_id).is_ok() => (None, Some(session_id)),
        _ => (None, None),
    }
}

fn resolve_session_columns_for_record(
    state: LoopState,
    paused_from_state: Option<LoopState>,
    reauth_from_state: Option<LoopState>,
    failed_from_state: Option<LoopState>,
    typed_opencode_session_id: Option<String>,
    typed_claude_session_id: Option<String>,
    legacy_session_id: Option<String>,
) -> (Option<String>, Option<String>) {
    let effective_state = effective_session_state(
        state,
        paused_from_state,
        reauth_from_state,
        failed_from_state,
    );
    let (legacy_opencode_session_id, legacy_claude_session_id) =
        split_legacy_session_id(legacy_session_id);

    match effective_state {
        LoopState::Hardening => (
            legacy_opencode_session_id.or(typed_opencode_session_id),
            legacy_claude_session_id.or(typed_claude_session_id),
        ),
        LoopState::Implementing | LoopState::Testing => (
            typed_opencode_session_id,
            legacy_claude_session_id.or(typed_claude_session_id),
        ),
        LoopState::Reviewing => (
            legacy_opencode_session_id.or(typed_opencode_session_id),
            typed_claude_session_id,
        ),
        _ => (
            typed_opencode_session_id.or(legacy_opencode_session_id),
            typed_claude_session_id.or(legacy_claude_session_id),
        ),
    }
}

fn compat_session_id_for_active_state(
    state: LoopState,
    last_stage: Option<&str>,
    opencode_session_id: Option<&str>,
    claude_session_id: Option<&str>,
) -> Option<String> {
    match state {
        LoopState::Hardening => match last_stage {
            Some("revise") => claude_session_id.map(str::to_string),
            Some("audit") | None => opencode_session_id.map(str::to_string),
            _ => None,
        },
        LoopState::Implementing => claude_session_id.map(str::to_string),
        LoopState::Reviewing => opencode_session_id.map(str::to_string),
        // TESTING itself is non-resumable, but a failed test round feeds
        // directly back into IMPLEMENTING. Keep mirroring the Claude session in
        // the legacy column so an old pod can take over the next implement round.
        LoopState::Testing => claude_session_id.map(str::to_string),
        _ => None,
    }
}

fn compat_session_id_for_record(record: &LoopRecord, last_stage: Option<&str>) -> Option<String> {
    let opencode_session_id = record.opencode_session_id.as_deref();
    let claude_session_id = record.claude_session_id.as_deref();

    match record.state {
        LoopState::Paused => record.paused_from_state.and_then(|paused_from| {
            compat_session_id_for_active_state(
                paused_from,
                last_stage,
                opencode_session_id,
                claude_session_id,
            )
        }),
        LoopState::AwaitingReauth => record.reauth_from_state.and_then(|reauth_from| {
            compat_session_id_for_active_state(
                reauth_from,
                last_stage,
                opencode_session_id,
                claude_session_id,
            )
        }),
        LoopState::Failed => record.failed_from_state.and_then(|failed_from| {
            compat_session_id_for_active_state(
                failed_from,
                last_stage,
                opencode_session_id,
                claude_session_id,
            )
        }),
        state => compat_session_id_for_active_state(
            state,
            last_stage,
            opencode_session_id,
            claude_session_id,
        ),
    }
}

#[async_trait]
impl StateStore for PgStateStore {
    async fn create_loop(&self, record: &LoopRecord) -> Result<LoopRecord> {
        let row = sqlx::query(
            r#"
            INSERT INTO loops (
                id, engineer, spec_path, spec_content_hash, branch, kind,
                state, sub_state, round, max_rounds, harden, harden_only,
                auto_approve, ship_mode, cancel_requested, approve_requested, resume_requested,
                paused_from_state, reauth_from_state, failure_reason, current_sha,
                opencode_session_id, claude_session_id, active_job_name, retry_count, model_implementor,
                model_reviewer, merge_sha, merged_at, hardened_spec_path, spec_pr_url,
                resolved_default_branch,
                stage_timeout_secs,
                created_at, updated_at
            ) VALUES (
                $1, $2, $3, $4, $5, $6::loop_kind,
                $7::loop_state, $8::sub_state, $9, $10, $11, $12,
                $13, $14, $15, $16, $17,
                $18::loop_state, $19::loop_state, $20, $21,
                $22, $23, $24, $25, $26,
                $27, $28, $29, $30, $31,
                $32,
                $33,
                $34, $35
            )
            RETURNING *
            "#,
        )
        .bind(record.id)
        .bind(&record.engineer)
        .bind(&record.spec_path)
        .bind(&record.spec_content_hash)
        .bind(&record.branch)
        .bind(loop_kind_str(record.kind))
        .bind(loop_state_str(record.state))
        .bind(record.sub_state.map(sub_state_str))
        .bind(record.round)
        .bind(record.max_rounds)
        .bind(record.harden)
        .bind(record.harden_only)
        .bind(record.auto_approve)
        .bind(record.ship_mode)
        .bind(record.cancel_requested)
        .bind(record.approve_requested)
        .bind(record.resume_requested)
        .bind(record.paused_from_state.map(loop_state_str))
        .bind(record.reauth_from_state.map(loop_state_str))
        .bind(&record.failure_reason)
        .bind(&record.current_sha)
        .bind(&record.opencode_session_id)
        .bind(&record.claude_session_id)
        .bind(&record.active_job_name)
        .bind(record.retry_count)
        .bind(&record.model_implementor)
        .bind(&record.model_reviewer)
        .bind(&record.merge_sha)
        .bind(record.merged_at)
        .bind(&record.hardened_spec_path)
        .bind(&record.spec_pr_url)
        .bind(&record.resolved_default_branch)
        .bind(record.stage_timeout_secs)
        .bind(record.created_at)
        .bind(record.updated_at)
        .fetch_one(&self.pool)
        .await?;

        row_to_loop_record(&row)
    }

    async fn get_loop(&self, id: Uuid) -> Result<Option<LoopRecord>> {
        let row = sqlx::query("SELECT * FROM loops WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;

        row.as_ref().map(row_to_loop_record).transpose()
    }

    async fn get_loop_by_branch(&self, branch: &str) -> Result<Option<LoopRecord>> {
        // #96: A FAILED loop with resume_requested=true is effectively
        // active — it will flip back to its previous stage on the next
        // reconciler tick — so it MUST count as branch-owning here.
        // Otherwise between `nemo resume` and the tick, a second
        // `/start` could acquire the same branch and corrupt the worktree.
        let row = sqlx::query(
            "SELECT * FROM loops \
             WHERE branch = $1 \
               AND (state NOT IN ('CONVERGED', 'FAILED', 'CANCELLED', 'HARDENED', 'SHIPPED') \
                    OR (state = 'FAILED' AND resume_requested = TRUE))",
        )
        .bind(branch)
        .fetch_optional(&self.pool)
        .await?;

        row.as_ref().map(row_to_loop_record).transpose()
    }

    async fn get_loop_by_branch_any(&self, branch: &str) -> Result<Option<LoopRecord>> {
        let row =
            sqlx::query("SELECT * FROM loops WHERE branch = $1 ORDER BY updated_at DESC LIMIT 1")
                .bind(branch)
                .fetch_optional(&self.pool)
                .await?;

        row.as_ref().map(row_to_loop_record).transpose()
    }

    async fn get_active_loops(&self) -> Result<Vec<LoopRecord>> {
        // Terminal states are excluded EXCEPT FAILED loops with a pending
        // resume_requested flag (#96) — those need one reconciler tick to
        // land in handle_failed and transition back to their original stage.
        let rows = sqlx::query(
            "SELECT * FROM loops \
             WHERE state NOT IN ('CONVERGED', 'FAILED', 'CANCELLED', 'HARDENED', 'SHIPPED') \
                OR (state = 'FAILED' AND resume_requested = TRUE) \
             ORDER BY created_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(row_to_loop_record).collect()
    }

    async fn get_loops_for_engineer(
        &self,
        engineer: Option<&str>,
        team: bool,
        include_terminal: bool,
    ) -> Result<Vec<LoopRecord>> {
        let terminal_filter = if include_terminal {
            ""
        } else {
            " AND state NOT IN ('CONVERGED', 'FAILED', 'CANCELLED', 'HARDENED', 'SHIPPED')"
        };

        let rows = match engineer {
            Some(eng) if !team => {
                let q = format!(
                    "SELECT * FROM loops WHERE engineer = $1{terminal_filter} ORDER BY created_at DESC LIMIT 10000"
                );
                sqlx::query(&q).bind(eng).fetch_all(&self.pool).await?
            }
            _ => {
                // Bounded to prevent unbounded memory consumption on long-running
                // deployments. 10000 rows is sufficient for aggregation (fleet
                // summary, stats, specs) while protecting the polling endpoint.
                let q = format!(
                    "SELECT * FROM loops WHERE true{terminal_filter} ORDER BY created_at DESC LIMIT 10000"
                );
                sqlx::query(&q).fetch_all(&self.pool).await?
            }
        };

        rows.iter().map(row_to_loop_record).collect()
    }

    async fn get_active_loops_for_spec(&self, spec_path: &str) -> Result<Vec<LoopRecord>> {
        let rows = sqlx::query(
            "SELECT * FROM loops \
             WHERE spec_path = $1 \
               AND state NOT IN ('CONVERGED', 'FAILED', 'CANCELLED', 'HARDENED', 'SHIPPED') \
             ORDER BY created_at DESC",
        )
        .bind(spec_path)
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(row_to_loop_record).collect()
    }

    async fn get_loops_for_aggregation(&self, since: DateTime<Utc>) -> Result<Vec<LoopRecord>> {
        let rows =
            sqlx::query("SELECT * FROM loops WHERE created_at >= $1 ORDER BY created_at DESC")
                .bind(since)
                .fetch_all(&self.pool)
                .await?;

        rows.iter().map(row_to_loop_record).collect()
    }

    async fn get_terminal_loops(
        &self,
        engineer: Option<&str>,
        spec_path: Option<&str>,
        since: Option<DateTime<Utc>>,
        cursor: Option<DateTime<Utc>>,
        limit: usize,
        states: Option<&[LoopState]>,
    ) -> Result<Vec<LoopRecord>> {
        // Build a dynamic query that filters at the DB level.
        // When `states` is provided, filter to exactly those states (DB-level).
        // Otherwise, default to all terminal states.
        let state_condition = if let Some(s) = states {
            let state_strs: Vec<String> = s
                .iter()
                .map(|st| format!("'{}'", loop_state_str(*st)))
                .collect();
            format!("state IN ({})", state_strs.join(", "))
        } else {
            "state IN ('CONVERGED', 'FAILED', 'CANCELLED', 'HARDENED', 'SHIPPED')".to_string()
        };
        let mut conditions = vec![state_condition];
        let mut bind_idx = 1u32;
        let mut engineer_val = None;
        let mut spec_val = None;
        let mut since_val = None;
        let mut cursor_val = None;

        if let Some(eng) = engineer {
            conditions.push(format!("engineer = ${bind_idx}"));
            bind_idx += 1;
            engineer_val = Some(eng.to_string());
        }
        if let Some(sp) = spec_path {
            conditions.push(format!("spec_path = ${bind_idx}"));
            bind_idx += 1;
            spec_val = Some(sp.to_string());
        }
        if since.is_some() {
            conditions.push(format!("updated_at >= ${bind_idx}"));
            bind_idx += 1;
            since_val = since;
        }
        if cursor.is_some() {
            conditions.push(format!("updated_at < ${bind_idx}"));
            bind_idx += 1;
            cursor_val = cursor;
        }

        let where_clause = conditions.join(" AND ");
        let q = format!(
            "SELECT * FROM loops WHERE {where_clause} ORDER BY updated_at DESC LIMIT ${bind_idx}"
        );

        let mut query = sqlx::query(&q);
        if let Some(eng) = &engineer_val {
            query = query.bind(eng);
        }
        if let Some(sp) = &spec_val {
            query = query.bind(sp);
        }
        if let Some(s) = since_val {
            query = query.bind(s);
        }
        if let Some(c) = cursor_val {
            query = query.bind(c);
        }
        query = query.bind(limit as i64);

        let rows = query.fetch_all(&self.pool).await?;
        rows.iter().map(row_to_loop_record).collect()
    }

    async fn update_loop_state(
        &self,
        id: Uuid,
        state: LoopState,
        sub_state: Option<SubState>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE loops SET state = $2::loop_state, sub_state = $3::sub_state, updated_at = NOW() WHERE id = $1",
        )
        .bind(id)
        .bind(loop_state_str(state))
        .bind(sub_state.map(sub_state_str))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Update the loop record. Does NOT overwrite flag columns (cancel_requested,
    /// approve_requested, resume_requested) to avoid read-modify-write race with
    /// set_loop_flag. Use set_loop_flag for flag changes.
    async fn update_loop(&self, record: &LoopRecord) -> Result<()> {
        let last_stage = match record.state {
            LoopState::Hardening
            | LoopState::Paused
            | LoopState::AwaitingReauth
            | LoopState::Failed => self
                .get_rounds(record.id)
                .await?
                .iter()
                .rfind(|round| round.round == record.round)
                .map(|round| round.stage.clone()),
            _ => None,
        };
        let legacy_session_id = compat_session_id_for_record(record, last_stage.as_deref());

        sqlx::query(
            r#"
            UPDATE loops SET
                spec_path = $2, state = $3::loop_state, sub_state = $4::sub_state, round = $5,
                paused_from_state = $6::loop_state, reauth_from_state = $7::loop_state,
                failure_reason = $8, current_sha = $9,
                opencode_session_id = $10, claude_session_id = $11,
                session_id = $12,
                active_job_name = $13, retry_count = $14,
                merge_sha = $15, merged_at = $16,
                hardened_spec_path = $17, spec_pr_url = $18,
                failed_from_state = $19::loop_state,
                max_rounds = $20,
                stage_timeout_secs = $21,
                updated_at = NOW()
            WHERE id = $1
            "#,
        )
        .bind(record.id)
        .bind(&record.spec_path)
        .bind(loop_state_str(record.state))
        .bind(record.sub_state.map(sub_state_str))
        .bind(record.round)
        .bind(record.paused_from_state.map(loop_state_str))
        .bind(record.reauth_from_state.map(loop_state_str))
        .bind(&record.failure_reason)
        .bind(&record.current_sha)
        .bind(&record.opencode_session_id)
        .bind(&record.claude_session_id)
        .bind(&legacy_session_id)
        .bind(&record.active_job_name)
        .bind(record.retry_count)
        .bind(&record.merge_sha)
        .bind(record.merged_at)
        .bind(&record.hardened_spec_path)
        .bind(&record.spec_pr_url)
        .bind(record.failed_from_state.map(loop_state_str))
        .bind(record.max_rounds)
        .bind(record.stage_timeout_secs)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn set_loop_flag(&self, id: Uuid, flag: LoopFlag, value: bool) -> Result<()> {
        let col = match flag {
            LoopFlag::Cancel => "cancel_requested",
            LoopFlag::Approve => "approve_requested",
            LoopFlag::Resume => "resume_requested",
        };
        let query = format!("UPDATE loops SET {col} = $2, updated_at = NOW() WHERE id = $1");
        sqlx::query(&query)
            .bind(id)
            .bind(value)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn set_current_sha(&self, id: Uuid, sha: &str) -> Result<()> {
        sqlx::query("UPDATE loops SET current_sha = $2, updated_at = NOW() WHERE id = $1")
            .bind(id)
            .bind(sha)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn has_active_loop_for_branch(&self, branch: &str) -> Result<bool> {
        // #96: FAILED + resume_requested counts as active — see the
        // matching comment on get_loop_by_branch.
        let row: (bool,) = sqlx::query_as(
            "SELECT EXISTS(\
                 SELECT 1 FROM loops \
                 WHERE branch = $1 \
                   AND (state NOT IN ('CONVERGED', 'FAILED', 'CANCELLED', 'HARDENED', 'SHIPPED') \
                        OR (state = 'FAILED' AND resume_requested = TRUE))\
             )",
        )
        .bind(branch)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    async fn create_round(&self, record: &RoundRecord) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO rounds (id, loop_id, round, stage, input, output, started_at, completed_at, duration_secs, job_name)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            "#,
        )
        .bind(record.id)
        .bind(record.loop_id)
        .bind(record.round)
        .bind(&record.stage)
        .bind(&record.input)
        .bind(&record.output)
        .bind(record.started_at)
        .bind(record.completed_at)
        .bind(record.duration_secs)
        .bind(&record.job_name)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn update_round(&self, record: &RoundRecord) -> Result<()> {
        sqlx::query(
            "UPDATE rounds SET output = $2, completed_at = $3, duration_secs = $4 WHERE id = $1",
        )
        .bind(record.id)
        .bind(&record.output)
        .bind(record.completed_at)
        .bind(record.duration_secs)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_rounds(&self, loop_id: Uuid) -> Result<Vec<RoundRecord>> {
        let rows = sqlx::query(
            "SELECT * FROM rounds WHERE loop_id = $1 ORDER BY round ASC, started_at ASC",
        )
        .bind(loop_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(row_to_round_record).collect())
    }

    async fn get_rounds_for_loops(
        &self,
        loop_ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, Vec<RoundRecord>>> {
        if loop_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let rows = sqlx::query(
            "SELECT * FROM rounds WHERE loop_id = ANY($1) ORDER BY round ASC, started_at ASC",
        )
        .bind(loop_ids)
        .fetch_all(&self.pool)
        .await?;

        let mut map: std::collections::HashMap<Uuid, Vec<RoundRecord>> =
            std::collections::HashMap::new();
        for row in &rows {
            let record = row_to_round_record(row);
            map.entry(record.loop_id).or_default().push(record);
        }
        Ok(map)
    }

    async fn append_log(&self, event: &LogEvent) -> Result<()> {
        sqlx::query(
            "INSERT INTO log_events (id, loop_id, round, stage, timestamp, line) VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(event.id)
        .bind(event.loop_id)
        .bind(event.round)
        .bind(&event.stage)
        .bind(event.timestamp)
        .bind(&event.line)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_logs(
        &self,
        loop_id: Uuid,
        round: Option<i32>,
        stage: Option<&str>,
    ) -> Result<Vec<LogEvent>> {
        let rows = match (round, stage) {
            (Some(r), Some(s)) => {
                sqlx::query(
                    "SELECT * FROM log_events WHERE loop_id = $1 AND round = $2 AND stage = $3 ORDER BY timestamp ASC",
                )
                .bind(loop_id)
                .bind(r)
                .bind(s)
                .fetch_all(&self.pool)
                .await?
            }
            (Some(r), None) => {
                sqlx::query(
                    "SELECT * FROM log_events WHERE loop_id = $1 AND round = $2 ORDER BY timestamp ASC",
                )
                .bind(loop_id)
                .bind(r)
                .fetch_all(&self.pool)
                .await?
            }
            (None, Some(s)) => {
                sqlx::query(
                    "SELECT * FROM log_events WHERE loop_id = $1 AND stage = $2 ORDER BY timestamp ASC",
                )
                .bind(loop_id)
                .bind(s)
                .fetch_all(&self.pool)
                .await?
            }
            (None, None) => {
                sqlx::query(
                    "SELECT * FROM log_events WHERE loop_id = $1 ORDER BY timestamp ASC",
                )
                .bind(loop_id)
                .fetch_all(&self.pool)
                .await?
            }
        };

        Ok(rows.iter().map(row_to_log_event).collect())
    }

    async fn get_logs_after(&self, loop_id: Uuid, after: DateTime<Utc>) -> Result<Vec<LogEvent>> {
        let rows = sqlx::query(
            "SELECT * FROM log_events WHERE loop_id = $1 AND timestamp >= $2 ORDER BY timestamp ASC, id ASC",
        )
        .bind(loop_id)
        .bind(after)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(row_to_log_event).collect())
    }

    async fn get_credentials(&self, engineer: &str) -> Result<Vec<EngineerCredential>> {
        let rows = sqlx::query("SELECT * FROM engineer_credentials WHERE engineer = $1")
            .bind(engineer)
            .fetch_all(&self.pool)
            .await?;

        Ok(rows.iter().map(row_to_credential).collect())
    }

    async fn upsert_credential(&self, cred: &EngineerCredential) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO engineer_credentials (id, engineer, provider, credential_ref, valid, updated_at)
            VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT (engineer, provider)
            DO UPDATE SET credential_ref = $4, valid = $5, updated_at = $6
            "#,
        )
        .bind(cred.id)
        .bind(&cred.engineer)
        .bind(&cred.provider)
        .bind(&cred.credential_ref)
        .bind(cred.valid)
        .bind(cred.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn are_credentials_valid(&self, engineer: &str, provider: &str) -> Result<bool> {
        let row: (bool,) = sqlx::query_as(
            "SELECT EXISTS(SELECT 1 FROM engineer_credentials WHERE engineer = $1 AND provider = $2 AND valid = TRUE)",
        )
        .bind(engineer)
        .bind(provider)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    async fn create_merge_event(&self, event: &crate::types::MergeEvent) -> Result<()> {
        sqlx::query(
            "INSERT INTO merge_events (id, loop_id, merge_sha, merge_strategy, ci_status, created_at) VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(event.id)
        .bind(event.loop_id)
        .bind(&event.merge_sha)
        .bind(&event.merge_strategy)
        .bind(&event.ci_status)
        .bind(event.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn try_advisory_lock(&self, loop_id: uuid::Uuid) -> Result<bool> {
        // Acquire a DEDICATED connection from the pool and hold it for the lock
        // duration. pg_try_advisory_lock is session-scoped, so the lock persists
        // as long as we keep this specific connection. advisory_unlock() will
        // unlock on the SAME connection and return it to the pool.
        let key = i64::from_be_bytes(loop_id.as_bytes()[..8].try_into().unwrap());
        let mut conn = self.pool.acquire().await?;
        let row: (bool,) = sqlx::query_as("SELECT pg_try_advisory_lock($1)")
            .bind(key)
            .fetch_one(&mut *conn)
            .await?;
        if row.0 {
            // Lock acquired — hold the connection
            let mut locks = self.lock_conns.lock().await;
            locks.insert(key, conn);
            Ok(true)
        } else {
            // Lock not acquired — return connection to pool immediately
            Ok(false)
        }
    }

    async fn advisory_unlock(&self, loop_id: uuid::Uuid) -> Result<()> {
        let key = i64::from_be_bytes(loop_id.as_bytes()[..8].try_into().unwrap());
        let mut locks = self.lock_conns.lock().await;
        if let Some(mut conn) = locks.remove(&key) {
            // Unlock on the SAME connection that acquired the lock
            let _ = sqlx::query("SELECT pg_advisory_unlock($1)")
                .bind(key)
                .execute(&mut *conn)
                .await;
            // Connection is returned to the pool when dropped
        }
        Ok(())
    }

    async fn create_judge_decision(
        &self,
        record: &crate::types::JudgeDecisionRecord,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO judge_decisions (
                id, loop_id, round, phase, trigger, input_json, decision,
                confidence, reasoning, hint, duration_ms, created_at
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
            "#,
        )
        .bind(record.id)
        .bind(record.loop_id)
        .bind(record.round)
        .bind(&record.phase)
        .bind(&record.trigger)
        .bind(&record.input_json)
        .bind(&record.decision)
        .bind(record.confidence)
        .bind(&record.reasoning)
        .bind(&record.hint)
        .bind(record.duration_ms)
        .bind(record.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_judge_decisions(
        &self,
        loop_id: Uuid,
    ) -> Result<Vec<crate::types::JudgeDecisionRecord>> {
        let rows = sqlx::query(
            "SELECT * FROM judge_decisions WHERE loop_id = $1 ORDER BY round ASC, created_at ASC",
        )
        .bind(loop_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .iter()
            .map(|row| crate::types::JudgeDecisionRecord {
                id: row.get("id"),
                loop_id: row.get("loop_id"),
                round: row.get("round"),
                phase: row.get("phase"),
                trigger: row.get("trigger"),
                input_json: row.get("input_json"),
                decision: row.get("decision"),
                confidence: row.get("confidence"),
                reasoning: row.get("reasoning"),
                hint: row.get("hint"),
                duration_ms: row.get("duration_ms"),
                created_at: row.get("created_at"),
                loop_final_state: row.get("loop_final_state"),
                loop_terminated_at: row.get("loop_terminated_at"),
            })
            .collect())
    }

    async fn count_judge_decisions(&self, loop_id: Uuid) -> Result<u32> {
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM judge_decisions WHERE loop_id = $1")
            .bind(loop_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.0 as u32)
    }

    async fn count_exit_clean_decisions(&self, loop_id: Uuid) -> Result<u32> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM judge_decisions WHERE loop_id = $1 AND decision = 'exit_clean'",
        )
        .bind(loop_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0 as u32)
    }

    async fn backfill_judge_decisions(
        &self,
        loop_id: Uuid,
        final_state: &str,
        terminated_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE judge_decisions SET loop_final_state = $2, loop_terminated_at = $3 WHERE loop_id = $1",
        )
        .bind(loop_id)
        .bind(final_state)
        .bind(terminated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn cleanup_pod_snapshots(&self, max_age_hours: u32) -> Result<u64> {
        let result = sqlx::query(
            "DELETE FROM pod_snapshots WHERE created_at < NOW() - make_interval(hours => $1::int)",
        )
        .bind(max_age_hours as i32)
        .execute(&self.pool)
        .await
        .map_err(crate::error::NautiloopError::Database)?;
        Ok(result.rows_affected())
    }

    async fn get_loop_state_counts(&self) -> Result<std::collections::HashMap<LoopState, usize>> {
        let rows = sqlx::query("SELECT state, COUNT(*) as cnt FROM loops GROUP BY state")
            .fetch_all(&self.pool)
            .await
            .map_err(crate::error::NautiloopError::Database)?;

        let mut counts = std::collections::HashMap::new();
        for row in &rows {
            let state: LoopState = sqlx::Row::get(row, "state");
            let cnt: i64 = sqlx::Row::get(row, "cnt");
            counts.insert(state, cnt as usize);
        }
        Ok(counts)
    }

    async fn get_distinct_engineers(&self) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT DISTINCT engineer FROM loops \
             WHERE state IN ('CONVERGED', 'FAILED', 'CANCELLED', 'HARDENED', 'SHIPPED') \
             ORDER BY engineer",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(crate::error::NautiloopError::Database)?;

        Ok(rows
            .iter()
            .map(|r| sqlx::Row::get::<String, _>(r, "engineer"))
            .collect())
    }

    async fn health_check(&self) -> Result<()> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .map_err(crate::error::NautiloopError::Database)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use uuid::Uuid;

    fn make_loop(state: LoopState) -> LoopRecord {
        LoopRecord {
            id: Uuid::new_v4(),
            engineer: "alice".to_string(),
            spec_path: "specs/test.md".to_string(),
            spec_content_hash: "abc12345".to_string(),
            branch: "agent/alice/test-abc12345".to_string(),
            kind: LoopKind::Implement,
            state,
            sub_state: None,
            round: 2,
            max_rounds: 5,
            harden: false,
            harden_only: false,
            auto_approve: false,
            cancel_requested: false,
            approve_requested: false,
            resume_requested: false,
            paused_from_state: None,
            reauth_from_state: None,
            failed_from_state: None,
            failure_reason: None,
            current_sha: None,
            opencode_session_id: Some("ses_abc123XYZ".to_string()),
            claude_session_id: Some("550e8400-e29b-41d4-a716-446655440000".to_string()),
            active_job_name: None,
            retry_count: 0,
            ship_mode: false,
            model_implementor: None,
            model_reviewer: None,
            merge_sha: None,
            merged_at: None,
            hardened_spec_path: None,
            spec_pr_url: None,
            resolved_default_branch: Some("main".to_string()),
            stage_timeout_secs: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn compat_session_prefers_stage_specific_value() {
        let implementing = make_loop(LoopState::Implementing);
        assert_eq!(
            compat_session_id_for_record(&implementing, None).as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );

        let reviewing = make_loop(LoopState::Reviewing);
        assert_eq!(
            compat_session_id_for_record(&reviewing, None).as_deref(),
            Some("ses_abc123XYZ")
        );
    }

    #[test]
    fn compat_session_uses_failed_stage_and_clears_cross_phase_states() {
        let mut failed_harden = make_loop(LoopState::Failed);
        failed_harden.failed_from_state = Some(LoopState::Hardening);
        assert_eq!(
            compat_session_id_for_record(&failed_harden, Some("revise")).as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );

        let awaiting_approval = make_loop(LoopState::AwaitingApproval);
        assert_eq!(compat_session_id_for_record(&awaiting_approval, None), None);
    }

    #[test]
    fn compat_session_keeps_claude_during_testing_for_old_pods() {
        let testing = make_loop(LoopState::Testing);
        assert_eq!(
            compat_session_id_for_record(&testing, None).as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
    }

    #[test]
    fn resolve_session_columns_prefers_fresh_legacy_for_active_tool() {
        let (opencode_session_id, claude_session_id) = resolve_session_columns_for_record(
            LoopState::Implementing,
            None,
            None,
            None,
            Some("ses_old_opencode".to_string()),
            Some("550e8400-e29b-41d4-a716-446655440000".to_string()),
            Some("550e8400-e29b-41d4-a716-446655440001".to_string()),
        );
        assert_eq!(opencode_session_id.as_deref(), Some("ses_old_opencode"));
        assert_eq!(
            claude_session_id.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440001")
        );

        let (opencode_session_id, claude_session_id) = resolve_session_columns_for_record(
            LoopState::Reviewing,
            None,
            None,
            None,
            Some("ses_old_review".to_string()),
            Some("550e8400-e29b-41d4-a716-446655440000".to_string()),
            Some("ses_new_review".to_string()),
        );
        assert_eq!(opencode_session_id.as_deref(), Some("ses_new_review"));
        assert_eq!(
            claude_session_id.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
    }
}
