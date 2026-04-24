use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

use super::ConvergentLoopDriver;
use crate::config::NautiloopConfig;
use crate::state::StateStore;

/// The reconciliation loop that drives all active loops.
///
/// Runs on a configurable interval (default 5s), ticking each active loop.
/// Can be woken up early via a `Notify` (e.g., from K8s Job watcher).
pub struct Reconciler {
    driver: Arc<ConvergentLoopDriver>,
    store: Arc<dyn StateStore>,
    config: Arc<NautiloopConfig>,
    interval: Duration,
    wake: Arc<Notify>,
}

impl Reconciler {
    pub fn new(
        driver: Arc<ConvergentLoopDriver>,
        store: Arc<dyn StateStore>,
        config: Arc<NautiloopConfig>,
        interval: Duration,
        wake: Arc<Notify>,
    ) -> Self {
        Self {
            driver,
            store,
            config,
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

        // FR-6b: daily sweep of old pod_snapshots (7-day TTL = 168 hours)
        let mut cleanup_interval = tokio::time::interval(Duration::from_secs(86400));
        // Delay missed ticks so accumulated misses don't burst-fire multiple sweeps.
        cleanup_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // First tick fires immediately; skip it so the first sweep happens after 24h
        cleanup_interval.tick().await;

        loop {
            // Wait for interval or wake signal or cleanup or cancellation
            tokio::select! {
                _ = tokio::time::sleep(self.interval) => {},
                _ = self.wake.notified() => {
                    tracing::debug!("Reconciler woken up by watcher");
                },
                _ = cleanup_interval.tick() => {
                    self.sweep_old_pod_snapshots().await;
                    continue;
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

    /// FR-6b: delete pod_snapshots older than 7 days.
    /// Only runs when record_introspection is enabled to avoid wasting queries.
    async fn sweep_old_pod_snapshots(&self) {
        if !self.config.observability.record_introspection {
            return;
        }
        const TTL_HOURS: u32 = 168; // 7 days
        match self.store.cleanup_pod_snapshots(TTL_HOURS).await {
            Ok(0) => {}
            Ok(deleted) => {
                tracing::info!(deleted, "Swept old pod_snapshots (>7 days)");
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to sweep pod_snapshots");
            }
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
            // Try to acquire a per-loop advisory lock so multiple control-plane
            // instances don't tick the same loop concurrently.
            if !self
                .store
                .try_advisory_lock(loop_record.id)
                .await
                .unwrap_or(false)
            {
                tracing::debug!(loop_id = %loop_record.id, "Skipping loop (advisory lock held by another instance)");
                continue;
            }

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
                    if e.is_fatal() {
                        // Fatal error: transition to FAILED so we don't retry forever.
                        // Re-read the current record to avoid overwriting fields that
                        // tick() may have updated before failing.
                        tracing::error!(
                            loop_id = %loop_record.id,
                            error = %e,
                            "Fatal tick error, transitioning to FAILED"
                        );
                        match self.store.get_loop(loop_record.id).await {
                            Ok(Some(mut current)) => {
                                current.state = crate::types::LoopState::Failed;
                                current.sub_state = None;
                                current.failure_reason = Some(format!("Fatal error: {e}"));
                                current.active_job_name = None;
                                if let Err(update_err) = self.store.update_loop(&current).await {
                                    tracing::error!(
                                        loop_id = %loop_record.id,
                                        error = %update_err,
                                        "Failed to mark loop as FAILED"
                                    );
                                }
                            }
                            _ => {
                                tracing::error!(
                                    loop_id = %loop_record.id,
                                    "Could not re-read loop to mark as FAILED"
                                );
                            }
                        }
                    } else {
                        tracing::warn!(
                            loop_id = %loop_record.id,
                            error = %e,
                            "Transient tick error, will retry"
                        );
                    }
                }
            }

            // Release advisory lock
            let _ = self.store.advisory_unlock(loop_record.id).await;
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
    use crate::config::NautiloopConfig;
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
            NautiloopConfig::default(),
        ));
        let wake = Arc::new(Notify::new());

        let reconciler = Reconciler::new(
            driver,
            store.clone(),
            Arc::new(NautiloopConfig::default()),
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
            failed_from_state: None,
            failure_reason: None,
            current_sha: None,
            opencode_session_id: None,
            claude_session_id: None,
            active_job_name: None,
            retry_count: 0,
            model_implementor: None,
            model_reviewer: None,
            merge_sha: None,
            merged_at: None,
            hardened_spec_path: None,
            spec_pr_url: None,
            resolved_default_branch: Some("main".to_string()),
            stage_timeout_secs: None,
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
