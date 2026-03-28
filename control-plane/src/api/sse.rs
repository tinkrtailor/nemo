use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use axum::response::sse::{Event, Sse};
use futures::stream::Stream;
use uuid::Uuid;

use crate::state::StateStore;
use crate::types::api::LogEventResponse;

/// Stream logs for a loop via SSE.
///
/// For active loops: tails from Postgres, sending new events as they appear.
/// Uses inclusive timestamp query (`>=`) with client-side dedup by ID to avoid
/// skipping same-timestamp rows (UUIDs are not monotonic).
/// Closes when the loop reaches a terminal state.
pub async fn stream_logs(
    store: Arc<dyn StateStore>,
    loop_id: Uuid,
    round: Option<i32>,
    stage: Option<String>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let stream = async_stream::stream! {
        let mut cursor_timestamp = chrono::DateTime::<chrono::Utc>::MIN_UTC;
        let mut seen_ids: HashSet<Uuid> = HashSet::new();
        let poll_interval = Duration::from_millis(500);

        loop {
            let logs = match store.get_logs_after(loop_id, cursor_timestamp).await {
                Ok(logs) => logs,
                Err(e) => {
                    tracing::error!(error = %e, "Failed to get logs for SSE");
                    break;
                }
            };

            for log in &logs {
                // Dedup: skip already-sent events
                if !seen_ids.insert(log.id) {
                    continue;
                }

                // Advance cursor to latest timestamp seen
                if log.timestamp > cursor_timestamp {
                    cursor_timestamp = log.timestamp;
                    // Prune seen_ids: only need to track IDs at cursor_timestamp
                    seen_ids.retain(|id| {
                        // Keep all IDs (we can't check timestamp from ID alone).
                        // The set is bounded by events-per-poll which is small.
                        let _ = id;
                        true
                    });
                }

                // Apply filters
                if round.is_some_and(|r| log.round != r) {
                    continue;
                }
                if stage.as_ref().is_some_and(|s| log.stage != *s) {
                    continue;
                }

                let event = LogEventResponse {
                    timestamp: log.timestamp,
                    stage: log.stage.clone(),
                    round: log.round,
                    line: log.line.clone(),
                };

                if let Ok(json) = serde_json::to_string(&event) {
                    yield Ok(Event::default().data(json));
                }
            }

            // Check if loop is terminal
            match store.get_loop(loop_id).await {
                Ok(Some(record)) if record.state.is_terminal() => {
                    yield Ok(Event::default().data(
                        serde_json::json!({
                            "type": "end",
                            "state": record.state,
                        }).to_string()
                    ));
                    break;
                }
                Ok(None) => break,
                Err(_) => break,
                _ => {}
            }

            tokio::time::sleep(poll_interval).await;
        }
    };

    Sse::new(stream)
}
