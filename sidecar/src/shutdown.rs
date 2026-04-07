//! Graceful shutdown coordination.
//!
//! Two types of long-lived connections need to be drained on SIGTERM:
//! active SSH sessions in the git SSH proxy and active CONNECT tunnels
//! in the egress logger. FR-27 step 3 requires both to be tracked in a
//! single wait group. This module owns that wait group.
//!
//! Go's implementation only tracks SSH sessions (see `images/sidecar/main.go:848`).
//! Matching the spec's "improvement" note, we also track CONNECT
//! tunnels.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::timeout;

/// Thread-safe reference-counted tracker of long-lived connections.
#[derive(Clone, Debug, Default)]
pub struct ConnectionTracker {
    inner: Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    active: AtomicUsize,
    notify: Notify,
}

impl ConnectionTracker {
    /// Construct a new tracker with zero active connections.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new long-lived connection and return a guard. When the
    /// guard is dropped the connection count decrements and any task
    /// waiting in [`wait_for_drain`] is notified.
    pub fn track(&self) -> ConnectionGuard {
        self.inner.active.fetch_add(1, Ordering::SeqCst);
        ConnectionGuard {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Current number of active tracked connections. Primarily for
    /// tests.
    pub fn active(&self) -> usize {
        self.inner.active.load(Ordering::SeqCst)
    }

    /// Wait until the active-connection count reaches zero, or until
    /// `dur` elapses. Returns `true` if drained, `false` on timeout.
    pub async fn wait_for_drain(&self, dur: Duration) -> bool {
        if self.inner.active.load(Ordering::SeqCst) == 0 {
            return true;
        }
        let wait_future = async {
            while self.inner.active.load(Ordering::SeqCst) > 0 {
                self.inner.notify.notified().await;
            }
        };
        timeout(dur, wait_future).await.is_ok()
    }
}

/// Guard returned by [`ConnectionTracker::track`]. Decrements on drop.
#[derive(Debug)]
pub struct ConnectionGuard {
    inner: Arc<Inner>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        // fetch_sub returns the previous value — if it was 1 we just
        // reached zero and should wake every waiter.
        if self.inner.active.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.inner.notify.notify_waiters();
        } else {
            // Still wake waiters so they can re-check the count; the
            // notified future requires a post-decrement notify to make
            // progress on the while-loop above.
            self.inner.notify.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_drain_returns_true_when_zero_immediately() {
        let tracker = ConnectionTracker::new();
        assert!(tracker.wait_for_drain(Duration::from_millis(10)).await);
    }

    #[tokio::test]
    async fn test_drain_returns_true_after_unregister() {
        let tracker = ConnectionTracker::new();
        let guard = tracker.track();
        assert_eq!(tracker.active(), 1);

        let tracker_bg = tracker.clone();
        let h =
            tokio::spawn(async move { tracker_bg.wait_for_drain(Duration::from_secs(2)).await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(guard);

        let drained = h.await.expect("background task must not panic");
        assert!(drained);
    }

    #[tokio::test]
    async fn test_drain_returns_false_on_timeout() {
        let tracker = ConnectionTracker::new();
        let _guard = tracker.track();
        assert!(!tracker.wait_for_drain(Duration::from_millis(50)).await);
    }
}
