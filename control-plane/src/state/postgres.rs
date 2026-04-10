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
        state: row.get::<LoopState, _>("state"),
        sub_state: row.get::<Option<SubState>, _>("sub_state"),
        round: row.get("round"),
        max_rounds: row.get("max_rounds"),
        harden: row.get("harden"),
        harden_only: row.get("harden_only"),
        auto_approve: row.get("auto_approve"),
        cancel_requested: row.get("cancel_requested"),
        approve_requested: row.get("approve_requested"),
        resume_requested: row.get("resume_requested"),
        paused_from_state: row.get::<Option<LoopState>, _>("paused_from_state"),
        reauth_from_state: row.get::<Option<LoopState>, _>("reauth_from_state"),
        failed_from_state: row
            .try_get::<Option<LoopState>, _>("failed_from_state")
            .ok()
            .flatten(),
        failure_reason: row.get("failure_reason"),
        current_sha: row.get("current_sha"),
        // Dual-read: prefer the typed columns, fall back to the legacy
        // session_id column for mixed-version rolling-deploy safety.
        // Old pods still write session_id; new pods write both.
        opencode_session_id: row
            .try_get::<Option<String>, _>("opencode_session_id")
            .ok()
            .flatten()
            .or_else(|| {
                row.try_get::<Option<String>, _>("session_id")
                    .ok()
                    .flatten()
                    .filter(|s| s.starts_with("ses_"))
            }),
        claude_session_id: row
            .try_get::<Option<String>, _>("claude_session_id")
            .ok()
            .flatten()
            .or_else(|| {
                row.try_get::<Option<String>, _>("session_id")
                    .ok()
                    .flatten()
                    .filter(|s| uuid::Uuid::try_parse(s).is_ok())
            }),
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
        LoopState::Testing => None,
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
                created_at, updated_at
            ) VALUES (
                $1, $2, $3, $4, $5, $6::loop_kind,
                $7::loop_state, $8::sub_state, $9, $10, $11, $12,
                $13, $14, $15, $16, $17,
                $18::loop_state, $19::loop_state, $20, $21,
                $22, $23, $24, $25, $26,
                $27, $28, $29, $30, $31,
                $32,
                $33, $34
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
                    "SELECT * FROM loops WHERE engineer = $1{terminal_filter} ORDER BY created_at DESC LIMIT 100"
                );
                sqlx::query(&q).bind(eng).fetch_all(&self.pool).await?
            }
            _ => {
                let q = format!(
                    "SELECT * FROM loops WHERE true{terminal_filter} ORDER BY created_at DESC LIMIT 100"
                );
                sqlx::query(&q).fetch_all(&self.pool).await?
            }
        };

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
}
