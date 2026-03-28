pub mod postgres;

use async_trait::async_trait;
use uuid::Uuid;

use crate::error::Result;
use crate::types::{
    EngineerCredential, LogEvent, LoopRecord, LoopState, MergeEvent, RoundRecord, SubState,
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
    async fn get_loops_for_engineer(
        &self,
        engineer: Option<&str>,
        team: bool,
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

    /// Check if there is an active loop for the given branch.
    async fn has_active_loop_for_branch(&self, branch: &str) -> Result<bool>;

    /// Create a round record.
    async fn create_round(&self, record: &RoundRecord) -> Result<()>;

    /// Update a round record (set output, completed_at, duration).
    async fn update_round(&self, record: &RoundRecord) -> Result<()>;

    /// Get all rounds for a loop.
    async fn get_rounds(&self, loop_id: Uuid) -> Result<Vec<RoundRecord>>;

    /// Append a log event.
    async fn append_log(&self, event: &LogEvent) -> Result<()>;

    /// Get log events for a loop, optionally filtered by round and stage.
    async fn get_logs(
        &self,
        loop_id: Uuid,
        round: Option<i32>,
        stage: Option<&str>,
    ) -> Result<Vec<LogEvent>>;

    /// Get log events after a given timestamp (for SSE tailing).
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

    #[derive(Debug, Clone, Default)]
    pub struct MemoryStateStore {
        loops: Arc<RwLock<HashMap<Uuid, LoopRecord>>>,
        rounds: Arc<RwLock<Vec<RoundRecord>>>,
        logs: Arc<RwLock<Vec<LogEvent>>>,
        credentials: Arc<RwLock<Vec<EngineerCredential>>>,
        merge_events: Arc<RwLock<Vec<MergeEvent>>>,
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
                .find(|l| l.branch == branch && !l.state.is_terminal())
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
                .filter(|l| !l.state.is_terminal())
                .cloned()
                .collect())
        }

        async fn get_loops_for_engineer(
            &self,
            engineer: Option<&str>,
            team: bool,
        ) -> Result<Vec<LoopRecord>> {
            let loops = self.loops.read().await;
            Ok(loops
                .values()
                .filter(|l| {
                    if team {
                        true
                    } else if let Some(eng) = engineer {
                        l.engineer == eng
                    } else {
                        true
                    }
                })
                .cloned()
                .collect())
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

        async fn has_active_loop_for_branch(&self, branch: &str) -> Result<bool> {
            let loops = self.loops.read().await;
            Ok(loops
                .values()
                .any(|l| l.branch == branch && !l.state.is_terminal()))
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
                .filter(|l| l.loop_id == loop_id && l.timestamp > after)
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
    }
}
