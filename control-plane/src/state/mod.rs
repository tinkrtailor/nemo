pub mod postgres;

use async_trait::async_trait;
use uuid::Uuid;

use crate::error::Result;
use crate::types::{
    EngineerCredential, JudgeDecisionRecord, LogEvent, LoopRecord, LoopState, MergeEvent,
    RoundRecord, SubState,
};

/// Trait abstracting all database operations for the control plane.
/// Implement with Postgres for production, or with in-memory store for tests.
#[async_trait]
pub trait StateStore: Send + Sync + 'static {
    /// Create a new loop record. Returns the created record.
    async fn create_loop(&self, record: &LoopRecord) -> Result<LoopRecord>;

    /// Get a loop by ID.
    async fn get_loop(&self, id: Uuid) -> Result<Option<LoopRecord>>;

    /// Get an active (non-terminal) loop by branch.
    async fn get_loop_by_branch(&self, branch: &str) -> Result<Option<LoopRecord>>;

    /// Get the most recent loop by branch, including terminal states (for /inspect).
    async fn get_loop_by_branch_any(&self, branch: &str) -> Result<Option<LoopRecord>>;

    /// Get all active (non-terminal) loops.
    async fn get_active_loops(&self) -> Result<Vec<LoopRecord>>;

    /// Get loops for an engineer, optionally filtered by team (all engineers).
    /// If `include_terminal` is false, only returns active (non-terminal) loops.
    async fn get_loops_for_engineer(
        &self,
        engineer: Option<&str>,
        team: bool,
        include_terminal: bool,
    ) -> Result<Vec<LoopRecord>>;

    /// Get active (non-terminal) loops matching a specific spec_path.
    /// Filters at the DB level to avoid fetching all active loops when only
    /// a few match (FR-13 spec history page optimization).
    async fn get_active_loops_for_spec(&self, spec_path: &str) -> Result<Vec<LoopRecord>>;

    /// Get ALL loops created within a time window (no row LIMIT).
    /// Used by fleet summary (FR-9) and stats (FR-14) aggregation where
    /// completeness matters more than bounding result size.
    /// The `since` parameter bounds results by `created_at >= since`.
    async fn get_loops_for_aggregation(
        &self,
        since: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<LoopRecord>>;

    /// Get terminal loops ordered by updated_at DESC, with optional filters.
    /// Unlike `get_loops_for_engineer`, this has no hard row limit and filters
    /// at the DB level for efficiency (FR-12, FR-9, FR-13, FR-14).
    /// When `states` is Some, only loops matching those states are returned
    /// (DB-level filter). When None, all terminal states are included.
    async fn get_terminal_loops(
        &self,
        engineer: Option<&str>,
        spec_path: Option<&str>,
        since: Option<chrono::DateTime<chrono::Utc>>,
        cursor: Option<chrono::DateTime<chrono::Utc>>,
        limit: usize,
        states: Option<&[LoopState]>,
    ) -> Result<Vec<LoopRecord>>;

    /// Update loop state and sub-state. Also updates `updated_at`.
    async fn update_loop_state(
        &self,
        id: Uuid,
        state: LoopState,
        sub_state: Option<SubState>,
    ) -> Result<()>;

    /// Update the full loop record (for complex state transitions).
    async fn update_loop(&self, record: &LoopRecord) -> Result<()>;

    /// Set a command flag on a loop (cancel_requested, approve_requested, resume_requested).
    async fn set_loop_flag(&self, id: Uuid, flag: LoopFlag, value: bool) -> Result<()>;

    /// Set current_sha on a loop (narrow update, no full record overwrite).
    async fn set_current_sha(&self, id: Uuid, sha: &str) -> Result<()>;

    /// Check if there is an active loop for the given branch.
    async fn has_active_loop_for_branch(&self, branch: &str) -> Result<bool>;

    /// Create a round record.
    async fn create_round(&self, record: &RoundRecord) -> Result<()>;

    /// Update a round record (set output, completed_at, duration).
    async fn update_round(&self, record: &RoundRecord) -> Result<()>;

    /// Get all rounds for a loop.
    async fn get_rounds(&self, loop_id: Uuid) -> Result<Vec<RoundRecord>>;

    /// Get rounds for multiple loops in a single query (avoids N+1).
    /// Returns a map from loop_id to its rounds.
    async fn get_rounds_for_loops(
        &self,
        loop_ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, Vec<RoundRecord>>>;

    /// Append a log event.
    async fn append_log(&self, event: &LogEvent) -> Result<()>;

    /// Get log events for a loop, optionally filtered by round and stage.
    async fn get_logs(
        &self,
        loop_id: Uuid,
        round: Option<i32>,
        stage: Option<&str>,
    ) -> Result<Vec<LogEvent>>;

    /// Get log events at or after a given timestamp for SSE tailing.
    /// Uses inclusive `>=` query; caller deduplicates by seen IDs.
    async fn get_logs_after(
        &self,
        loop_id: Uuid,
        after: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<LogEvent>>;

    /// Get or create engineer credentials.
    async fn get_credentials(&self, engineer: &str) -> Result<Vec<EngineerCredential>>;

    /// Upsert engineer credentials.
    async fn upsert_credential(&self, cred: &EngineerCredential) -> Result<()>;

    /// Check if credentials are valid for an engineer and provider.
    async fn are_credentials_valid(&self, engineer: &str, provider: &str) -> Result<bool>;

    /// Create a merge event record (NFR-8).
    async fn create_merge_event(&self, event: &MergeEvent) -> Result<()>;

    /// Try to acquire a per-loop advisory lock (for reconciler coordination).
    /// Returns `Some(guard_id)` if acquired, `None` if held by another session.
    /// The lock is session-scoped on a dedicated connection held internally.
    /// Call `advisory_unlock` with the same loop_id when done.
    async fn try_advisory_lock(&self, loop_id: Uuid) -> Result<bool>;

    /// Release a per-loop advisory lock and return its dedicated connection to the pool.
    async fn advisory_unlock(&self, loop_id: Uuid) -> Result<()>;

    /// Delete pod_snapshots older than `max_age_hours` hours (FR-6b).
    /// Returns the number of deleted rows.
    async fn cleanup_pod_snapshots(&self, max_age_hours: u32) -> Result<u64>;

    /// Create a judge decision record.
    async fn create_judge_decision(&self, record: &JudgeDecisionRecord) -> Result<()>;

    /// Get all judge decisions for a loop (ordered by round).
    async fn get_judge_decisions(&self, loop_id: Uuid) -> Result<Vec<JudgeDecisionRecord>>;

    /// Count judge decisions for a loop (for cost-ceiling enforcement).
    async fn count_judge_decisions(&self, loop_id: Uuid) -> Result<u32>;

    /// Count `exit_clean` decisions for a loop (FR-7a one-shot guard).
    async fn count_exit_clean_decisions(&self, loop_id: Uuid) -> Result<u32>;

    /// Back-fill loop_final_state and loop_terminated_at on all judge_decisions
    /// rows for a loop when it reaches a terminal state (FR-5b).
    async fn backfill_judge_decisions(
        &self,
        loop_id: Uuid,
        final_state: &str,
        terminated_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()>;

    /// Count loops grouped by state. Returns a map from LoopState to count.
    /// Uses `SELECT state, COUNT(*) FROM loops GROUP BY state` in Postgres —
    /// O(1) in result size regardless of total loops — giving exact counts
    /// without fetching full row data (dashboard filter chip badges).
    async fn get_loop_state_counts(&self) -> Result<std::collections::HashMap<LoopState, usize>>;

    /// Get distinct engineer names from terminal loops.
    /// Much lighter than fetching full loop records when only names are needed
    /// (e.g., for dashboard feed filter chips).
    async fn get_distinct_engineers(&self) -> Result<Vec<String>>;

    /// Health check: verify the store is reachable (e.g., SELECT 1).
    async fn health_check(&self) -> Result<()>;
}

/// Flags that can be set on a loop by the API server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopFlag {
    Cancel,
    Approve,
    Resume,
}

/// In-memory state store for testing.
pub mod memory {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    /// Simulates a Postgres unique constraint violation for MemoryStateStore.
    #[derive(Debug)]
    struct MemoryUniqueViolation;

    impl std::fmt::Display for MemoryUniqueViolation {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "unique constraint violation")
        }
    }

    impl std::error::Error for MemoryUniqueViolation {}

    impl sqlx::error::DatabaseError for MemoryUniqueViolation {
        fn message(&self) -> &str {
            "unique constraint violation"
        }
        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }
        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::UniqueViolation
        }
    }

    #[derive(Debug, Clone, Default)]
    pub struct MemoryStateStore {
        loops: Arc<RwLock<HashMap<Uuid, LoopRecord>>>,
        rounds: Arc<RwLock<Vec<RoundRecord>>>,
        logs: Arc<RwLock<Vec<LogEvent>>>,
        credentials: Arc<RwLock<Vec<EngineerCredential>>>,
        merge_events: Arc<RwLock<Vec<MergeEvent>>>,
        judge_decisions: Arc<RwLock<Vec<JudgeDecisionRecord>>>,
    }

    impl MemoryStateStore {
        pub fn new() -> Self {
            Self::default()
        }
    }

    #[async_trait]
    impl StateStore for MemoryStateStore {
        async fn create_loop(&self, record: &LoopRecord) -> Result<LoopRecord> {
            let mut loops = self.loops.write().await;
            // Enforce unique active branch constraint (mirrors Postgres
            // partial unique index, including the #96 FAILED+resume
            // exception: a failed loop with a pending resume still owns
            // the branch and must block new loops on it).
            let has_active = loops.values().any(|l| {
                l.branch == record.branch
                    && (!l.state.is_terminal()
                        || (l.state == LoopState::Failed && l.resume_requested))
            });
            if has_active {
                return Err(crate::error::NautiloopError::Database(
                    sqlx::Error::Database(Box::new(MemoryUniqueViolation)),
                ));
            }
            loops.insert(record.id, record.clone());
            Ok(record.clone())
        }

        async fn get_loop(&self, id: Uuid) -> Result<Option<LoopRecord>> {
            let loops = self.loops.read().await;
            Ok(loops.get(&id).cloned())
        }

        async fn get_loop_by_branch(&self, branch: &str) -> Result<Option<LoopRecord>> {
            let loops = self.loops.read().await;
            Ok(loops
                .values()
                .find(|l| {
                    l.branch == branch
                        && (!l.state.is_terminal()
                            || (l.state == LoopState::Failed && l.resume_requested))
                })
                .cloned())
        }

        async fn get_loop_by_branch_any(&self, branch: &str) -> Result<Option<LoopRecord>> {
            let loops = self.loops.read().await;
            // Return the most recently updated loop for this branch
            Ok(loops
                .values()
                .filter(|l| l.branch == branch)
                .max_by_key(|l| l.updated_at)
                .cloned())
        }

        async fn get_active_loops(&self) -> Result<Vec<LoopRecord>> {
            let loops = self.loops.read().await;
            Ok(loops
                .values()
                .filter(|l| {
                    // #96: include FAILED loops with a pending resume so
                    // handle_failed gets a chance to redispatch them.
                    !l.state.is_terminal() || (l.state == LoopState::Failed && l.resume_requested)
                })
                .cloned()
                .collect())
        }

        async fn get_loops_for_engineer(
            &self,
            engineer: Option<&str>,
            team: bool,
            include_terminal: bool,
        ) -> Result<Vec<LoopRecord>> {
            let loops = self.loops.read().await;
            Ok(loops
                .values()
                .filter(|l| {
                    let eng_match = if team {
                        true
                    } else if let Some(eng) = engineer {
                        l.engineer == eng
                    } else {
                        true
                    };
                    eng_match && (include_terminal || !l.state.is_terminal())
                })
                .cloned()
                .collect())
        }

        async fn get_active_loops_for_spec(&self, spec_path: &str) -> Result<Vec<LoopRecord>> {
            let loops = self.loops.read().await;
            Ok(loops
                .values()
                .filter(|l| !l.state.is_terminal() && l.spec_path == spec_path)
                .cloned()
                .collect())
        }

        async fn get_loops_for_aggregation(
            &self,
            since: chrono::DateTime<chrono::Utc>,
        ) -> Result<Vec<LoopRecord>> {
            let loops = self.loops.read().await;
            Ok(loops
                .values()
                .filter(|l| l.created_at >= since)
                .cloned()
                .collect())
        }

        async fn get_terminal_loops(
            &self,
            engineer: Option<&str>,
            spec_path: Option<&str>,
            since: Option<chrono::DateTime<chrono::Utc>>,
            cursor: Option<chrono::DateTime<chrono::Utc>>,
            limit: usize,
            states: Option<&[LoopState]>,
        ) -> Result<Vec<LoopRecord>> {
            let loops = self.loops.read().await;
            let mut result: Vec<_> = loops
                .values()
                .filter(|l| {
                    if let Some(s) = states {
                        if !s.contains(&l.state) {
                            return false;
                        }
                    } else if !l.state.is_terminal() {
                        return false;
                    }
                    if let Some(eng) = engineer
                        && l.engineer != eng
                    {
                        return false;
                    }
                    if let Some(sp) = spec_path
                        && l.spec_path != sp
                    {
                        return false;
                    }
                    if let Some(s) = since
                        && l.updated_at < s
                    {
                        return false;
                    }
                    if let Some(c) = cursor
                        && l.updated_at >= c
                    {
                        return false;
                    }
                    true
                })
                .cloned()
                .collect();
            result.sort_by_key(|l| std::cmp::Reverse(l.updated_at));
            result.truncate(limit);
            Ok(result)
        }

        async fn update_loop_state(
            &self,
            id: Uuid,
            state: LoopState,
            sub_state: Option<SubState>,
        ) -> Result<()> {
            let mut loops = self.loops.write().await;
            if let Some(record) = loops.get_mut(&id) {
                record.state = state;
                record.sub_state = sub_state;
                record.updated_at = chrono::Utc::now();
            }
            Ok(())
        }

        /// Update the loop record. Preserves flag columns (cancel_requested,
        /// approve_requested, resume_requested) to match Postgres behavior and
        /// avoid read-modify-write race with set_loop_flag.
        async fn update_loop(&self, record: &LoopRecord) -> Result<()> {
            let mut loops = self.loops.write().await;
            if let Some(existing) = loops.get(&record.id) {
                let mut merged = record.clone();
                // Preserve flags from existing record (only set_loop_flag should modify these)
                merged.cancel_requested = existing.cancel_requested;
                merged.approve_requested = existing.approve_requested;
                merged.resume_requested = existing.resume_requested;
                loops.insert(record.id, merged);
            } else {
                loops.insert(record.id, record.clone());
            }
            Ok(())
        }

        async fn set_loop_flag(&self, id: Uuid, flag: LoopFlag, value: bool) -> Result<()> {
            let mut loops = self.loops.write().await;
            if let Some(record) = loops.get_mut(&id) {
                match flag {
                    LoopFlag::Cancel => record.cancel_requested = value,
                    LoopFlag::Approve => record.approve_requested = value,
                    LoopFlag::Resume => record.resume_requested = value,
                }
                record.updated_at = chrono::Utc::now();
            }
            Ok(())
        }

        async fn set_current_sha(&self, id: Uuid, sha: &str) -> Result<()> {
            let mut loops = self.loops.write().await;
            if let Some(record) = loops.get_mut(&id) {
                record.current_sha = Some(sha.to_string());
                record.updated_at = chrono::Utc::now();
            }
            Ok(())
        }

        async fn has_active_loop_for_branch(&self, branch: &str) -> Result<bool> {
            let loops = self.loops.read().await;
            Ok(loops.values().any(|l| {
                l.branch == branch
                    && (!l.state.is_terminal()
                        || (l.state == LoopState::Failed && l.resume_requested))
            }))
        }

        async fn create_round(&self, record: &RoundRecord) -> Result<()> {
            let mut rounds = self.rounds.write().await;
            rounds.push(record.clone());
            Ok(())
        }

        async fn update_round(&self, record: &RoundRecord) -> Result<()> {
            let mut rounds = self.rounds.write().await;
            if let Some(existing) = rounds.iter_mut().find(|r| r.id == record.id) {
                *existing = record.clone();
            }
            Ok(())
        }

        async fn get_rounds(&self, loop_id: Uuid) -> Result<Vec<RoundRecord>> {
            let rounds = self.rounds.read().await;
            Ok(rounds
                .iter()
                .filter(|r| r.loop_id == loop_id)
                .cloned()
                .collect())
        }

        async fn get_rounds_for_loops(
            &self,
            loop_ids: &[Uuid],
        ) -> Result<std::collections::HashMap<Uuid, Vec<RoundRecord>>> {
            let id_set: std::collections::HashSet<Uuid> = loop_ids.iter().copied().collect();
            let rounds = self.rounds.read().await;
            let mut map: std::collections::HashMap<Uuid, Vec<RoundRecord>> =
                std::collections::HashMap::new();
            for r in rounds.iter() {
                if id_set.contains(&r.loop_id) {
                    map.entry(r.loop_id).or_default().push(r.clone());
                }
            }
            Ok(map)
        }

        async fn append_log(&self, event: &LogEvent) -> Result<()> {
            let mut logs = self.logs.write().await;
            logs.push(event.clone());
            Ok(())
        }

        async fn get_logs(
            &self,
            loop_id: Uuid,
            round: Option<i32>,
            stage: Option<&str>,
        ) -> Result<Vec<LogEvent>> {
            let logs = self.logs.read().await;
            Ok(logs
                .iter()
                .filter(|l| {
                    l.loop_id == loop_id
                        && round.is_none_or(|r| l.round == r)
                        && stage.is_none_or(|s| l.stage == s)
                })
                .cloned()
                .collect())
        }

        async fn get_logs_after(
            &self,
            loop_id: Uuid,
            after: chrono::DateTime<chrono::Utc>,
        ) -> Result<Vec<LogEvent>> {
            let logs = self.logs.read().await;
            Ok(logs
                .iter()
                .filter(|l| l.loop_id == loop_id && l.timestamp >= after)
                .cloned()
                .collect())
        }

        async fn get_credentials(&self, engineer: &str) -> Result<Vec<EngineerCredential>> {
            let creds = self.credentials.read().await;
            Ok(creds
                .iter()
                .filter(|c| c.engineer == engineer)
                .cloned()
                .collect())
        }

        async fn upsert_credential(&self, cred: &EngineerCredential) -> Result<()> {
            let mut creds = self.credentials.write().await;
            if let Some(existing) = creds
                .iter_mut()
                .find(|c| c.engineer == cred.engineer && c.provider == cred.provider)
            {
                *existing = cred.clone();
            } else {
                creds.push(cred.clone());
            }
            Ok(())
        }

        async fn are_credentials_valid(&self, engineer: &str, provider: &str) -> Result<bool> {
            let creds = self.credentials.read().await;
            Ok(creds
                .iter()
                .any(|c| c.engineer == engineer && c.provider == provider && c.valid))
        }

        async fn create_merge_event(&self, event: &MergeEvent) -> Result<()> {
            let mut events = self.merge_events.write().await;
            events.push(event.clone());
            Ok(())
        }

        async fn create_judge_decision(&self, record: &JudgeDecisionRecord) -> Result<()> {
            let mut decisions = self.judge_decisions.write().await;
            decisions.push(record.clone());
            Ok(())
        }

        async fn get_judge_decisions(&self, loop_id: Uuid) -> Result<Vec<JudgeDecisionRecord>> {
            let decisions = self.judge_decisions.read().await;
            let mut result: Vec<_> = decisions
                .iter()
                .filter(|d| d.loop_id == loop_id)
                .cloned()
                .collect();
            result.sort_by_key(|d| d.round);
            Ok(result)
        }

        async fn count_judge_decisions(&self, loop_id: Uuid) -> Result<u32> {
            let decisions = self.judge_decisions.read().await;
            Ok(decisions.iter().filter(|d| d.loop_id == loop_id).count() as u32)
        }

        async fn count_exit_clean_decisions(&self, loop_id: Uuid) -> Result<u32> {
            let decisions = self.judge_decisions.read().await;
            Ok(decisions
                .iter()
                .filter(|d| d.loop_id == loop_id && d.decision == "exit_clean")
                .count() as u32)
        }

        async fn backfill_judge_decisions(
            &self,
            loop_id: Uuid,
            final_state: &str,
            terminated_at: chrono::DateTime<chrono::Utc>,
        ) -> Result<()> {
            let mut decisions = self.judge_decisions.write().await;
            for d in decisions.iter_mut().filter(|d| d.loop_id == loop_id) {
                d.loop_final_state = Some(final_state.to_string());
                d.loop_terminated_at = Some(terminated_at);
            }
            Ok(())
        }

        async fn cleanup_pod_snapshots(&self, _max_age_hours: u32) -> Result<u64> {
            // No-op for in-memory store (no pod_snapshots table)
            Ok(0)
        }

        async fn try_advisory_lock(&self, _loop_id: Uuid) -> Result<bool> {
            // In-memory store: always succeeds (single instance)
            Ok(true)
        }

        async fn advisory_unlock(&self, _loop_id: Uuid) -> Result<()> {
            Ok(())
        }

        async fn get_loop_state_counts(
            &self,
        ) -> Result<std::collections::HashMap<LoopState, usize>> {
            let loops = self.loops.read().await;
            let mut counts = std::collections::HashMap::new();
            for l in loops.values() {
                *counts.entry(l.state).or_insert(0) += 1;
            }
            Ok(counts)
        }

        async fn get_distinct_engineers(&self) -> Result<Vec<String>> {
            let loops = self.loops.read().await;
            let mut engineers: Vec<String> = loops
                .values()
                .filter(|l| l.state.is_terminal())
                .map(|l| l.engineer.clone())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            engineers.sort();
            Ok(engineers)
        }

        async fn health_check(&self) -> Result<()> {
            Ok(())
        }
    }
}
