use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

use super::ConvergentLoopDriver;
use crate::state::StateStore;

/// The reconciliation loop that drives all active loops.
///
/// Runs on a configurable interval (default 5s), ticking each active loop.
/// Can be woken up early via a `Notify` (e.g., from K8s Job watcher).
pub struct Reconciler {
    driver: Arc<ConvergentLoopDriver>,
    store: Arc<dyn StateStore>,
    interval: Duration,
    wake: Arc<Notify>,
}

impl Reconciler {
    pub fn new(
        driver: Arc<ConvergentLoopDriver>,
        store: Arc<dyn StateStore>,
        interval: Duration,
        wake: Arc<Notify>,
    ) -> Self {
        Self {
            driver,
            store,
            interval,
            wake,
        }
    }

    /// Run the reconciliation loop until the cancellation token is triggered.
    pub async fn run(&self, cancel: tokio::sync::watch::Receiver<bool>) {
        tracing::info!(
            interval_ms = self.interval.as_millis(),
            "Starting reconciliation loop"
        );

        loop {
            // Wait for interval or wake signal or cancellation
            tokio::select! {
                _ = tokio::time::sleep(self.interval) => {},
                _ = self.wake.notified() => {
                    tracing::debug!("Reconciler woken up by watcher");
                },
                _ = wait_for_cancel(&cancel) => {
                    tracing::info!("Reconciler shutting down");
                    break;
                }
            }

            // Check cancellation
            if *cancel.borrow() {
                break;
            }

            self.reconcile_all().await;
        }
    }

    /// Tick all active loops.
    async fn reconcile_all(&self) {
        let active_loops = match self.store.get_active_loops().await {
            Ok(loops) => loops,
            Err(e) => {
                tracing::error!(error = %e, "Failed to get active loops");
                return;
            }
        };

        if active_loops.is_empty() {
            return;
        }

        tracing::debug!(count = active_loops.len(), "Reconciling active loops");

        for loop_record in &active_loops {
            match self.driver.tick(loop_record.id).await {
                Ok(new_state) => {
                    tracing::trace!(
                        loop_id = %loop_record.id,
                        old_state = %loop_record.state,
                        new_state = %new_state,
                        "Tick completed"
                    );
                }
                Err(e) => {
                    tracing::error!(
                        loop_id = %loop_record.id,
                        error = %e,
                        "Tick failed for loop"
                    );
                }
            }
        }
    }
}

async fn wait_for_cancel(cancel: &tokio::sync::watch::Receiver<bool>) {
    let mut rx = cancel.clone();
    while !*rx.borrow() {
        if rx.changed().await.is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NemoConfig;
    use crate::git::mock::MockGitOperations;
    use crate::k8s::mock::MockJobDispatcher;
    use crate::loop_engine::ConvergentLoopDriver;
    use crate::state::memory::MemoryStateStore;
    use crate::types::{LoopKind, LoopRecord, LoopState};
    use uuid::Uuid;

    #[tokio::test]
    async fn test_reconciler_processes_active_loops() {
        let store = Arc::new(MemoryStateStore::new());
        let dispatcher = Arc::new(MockJobDispatcher::new());
        let git = Arc::new(MockGitOperations::new());
        let driver = Arc::new(ConvergentLoopDriver::new(
            store.clone(),
            dispatcher,
            git,
            NemoConfig::default(),
        ));
        let wake = Arc::new(Notify::new());

        let reconciler = Reconciler::new(
            driver,
            store.clone(),
            Duration::from_millis(50),
            wake.clone(),
        );

        // Create a pending auto-approve loop
        let record = LoopRecord {
            id: Uuid::new_v4(),
            engineer: "alice".to_string(),
            spec_path: "specs/test.md".to_string(),
            spec_content_hash: "abc12345".to_string(),
            branch: "agent/alice/test-abc12345".to_string(),
            kind: LoopKind::Implement,
            state: LoopState::Pending,
            sub_state: None,
            round: 0,
            max_rounds: 15,
            harden: false,
            harden_only: false,
            auto_approve: true,
            ship_mode: false,
            cancel_requested: false,
            approve_requested: false,
            resume_requested: false,
            paused_from_state: None,
            reauth_from_state: None,
            failure_reason: None,
            current_sha: None,
            session_id: None,
            active_job_name: None,
            retry_count: 0,
            model_implementor: None,
            model_reviewer: None,
            merge_sha: None,
            merged_at: None,
            hardened_spec_path: None,
            spec_pr_url: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        store.create_loop(&record).await.unwrap();

        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

        // Run reconciler in background
        let reconciler_handle = tokio::spawn(async move {
            reconciler.run(cancel_rx).await;
        });

        // Wait a bit for reconciliation
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Cancel
        cancel_tx.send(true).unwrap();
        reconciler_handle.await.unwrap();

        // Check that the loop was processed
        let updated = store.get_loop(record.id).await.unwrap().unwrap();
        assert_ne!(updated.state, LoopState::Pending);
    }
}
