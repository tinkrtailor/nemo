//! `divergence_connect_drain_on_sigterm` runner.
//!
//! This case has `order_hint: "last"` because it kills both sidecar
//! containers. The flow:
//!
//! 1. Open a CONNECT tunnel through the egress port to
//!    `egress-target:443` (mock-tcp-echo) on Go and Rust in parallel.
//! 2. Start a background task trickling 1 byte per 100ms into each
//!    tunnel. The tunnel echoes those bytes back.
//! 3. After 500ms of steady traffic, SIGTERM each sidecar via
//!    `docker compose kill --signal SIGTERM <service>`.
//! 4. Measure how long each tunnel continues to echo bytes after
//!    the SIGTERM. Expected:
//!    - Go: stops within ~200ms (no drain, listener closes immediately)
//!    - Rust: continues 2-5s (up to the 5s drain deadline in
//!      `sidecar/src/main.rs`).
//!
//! The runner encodes the drain duration on each side so the diff
//! engine sees different bodies (pass for divergence).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::sleep;

use crate::compose::ports;
use crate::compose::{ComposeStack, SIDECAR_GO_SERVICE, SIDECAR_RUST_SERVICE};
use crate::corpus::CorpusCase;
use crate::result::SideOutput;
use crate::runner::RunnerContext;

/// Maximum wall clock we watch the tunnels for a post-SIGTERM drain.
/// A Rust sidecar should finish draining within 5s per main.rs
/// SHUTDOWN_DRAIN_TIMEOUT; we give ourselves 6s headroom.
const POST_SIGTERM_WATCH: Duration = Duration::from_secs(6);

/// How long to run the baseline steady-state traffic before firing
/// SIGTERM. Matches the spec's 500ms.
const BASELINE_MS: u64 = 500;

/// Drain thresholds from FR-22.
const GO_MAX_DRAIN_MS: u128 = 400; // loose upper bound for "fast close"
const RUST_MIN_DRAIN_MS: u128 = 1500; // loose lower bound for "slow drain"

pub async fn run(_case: &CorpusCase, ctx: &RunnerContext) -> Result<(SideOutput, SideOutput)> {
    // Establish both tunnels.
    let go_tunnel = open_connect_tunnel(ports::GO_EGRESS, "egress-target:443").await?;
    let rust_tunnel = open_connect_tunnel(ports::RUST_EGRESS, "egress-target:443").await?;

    let go_counter = Arc::new(Mutex::new(TunnelState::default()));
    let rust_counter = Arc::new(Mutex::new(TunnelState::default()));

    let go_pump = spawn_pump(go_tunnel, Arc::clone(&go_counter));
    let rust_pump = spawn_pump(rust_tunnel, Arc::clone(&rust_counter));

    // Baseline traffic.
    sleep(Duration::from_millis(BASELINE_MS)).await;

    // Fire SIGTERM via docker compose kill.
    let compose = ComposeStack::new(
        ctx.harness_dir.join("docker-compose.yml"),
        ctx.harness_dir.clone(),
    );
    compose.kill_signal(SIDECAR_GO_SERVICE, "SIGTERM").await?;
    let go_killed_at = Instant::now();
    compose.kill_signal(SIDECAR_RUST_SERVICE, "SIGTERM").await?;
    let rust_killed_at = Instant::now();

    // Watch until tunnels die or the watch window expires.
    let watch_end = Instant::now() + POST_SIGTERM_WATCH;
    while Instant::now() < watch_end {
        sleep(Duration::from_millis(50)).await;
        let go_closed = go_counter.lock().await.closed;
        let rust_closed = rust_counter.lock().await.closed;
        if go_closed && rust_closed {
            break;
        }
    }

    // Abort pumps so the tunnels drop.
    go_pump.abort();
    rust_pump.abort();

    let go_state = go_counter.lock().await.clone();
    let rust_state = rust_counter.lock().await.clone();

    let go_drain_ms = go_state
        .last_byte_at
        .map(|t| t.duration_since(go_killed_at).as_millis())
        .unwrap_or(0);
    let rust_drain_ms = rust_state
        .last_byte_at
        .map(|t| t.duration_since(rust_killed_at).as_millis())
        .unwrap_or(0);

    let go_verdict = if go_drain_ms <= GO_MAX_DRAIN_MS {
        format!("go_closes_fast: drain_ms={go_drain_ms} <= {GO_MAX_DRAIN_MS}")
    } else {
        format!("go_SLOW_DRAIN_BUG: drain_ms={go_drain_ms} > {GO_MAX_DRAIN_MS}")
    };
    let rust_verdict = if rust_drain_ms >= RUST_MIN_DRAIN_MS {
        format!("rust_drains_slow: drain_ms={rust_drain_ms} >= {RUST_MIN_DRAIN_MS}")
    } else {
        format!("rust_FAST_CLOSE_BUG: drain_ms={rust_drain_ms} < {RUST_MIN_DRAIN_MS}")
    };

    let go_out = SideOutput {
        drain_stop_ms: Some(go_drain_ms),
        http_body: go_verdict,
        ..SideOutput::default()
    };
    let rust_out = SideOutput {
        drain_stop_ms: Some(rust_drain_ms),
        http_body: rust_verdict,
        ..SideOutput::default()
    };

    Ok((go_out, rust_out))
}

async fn open_connect_tunnel(proxy_port: u16, target: &str) -> Result<TcpStream> {
    let addr = format!("127.0.0.1:{proxy_port}");
    let mut stream = TcpStream::connect(&addr)
        .await
        .with_context(|| format!("connect {addr}"))?;
    let connect_line = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n");
    stream
        .write_all(connect_line.as_bytes())
        .await
        .context("write CONNECT request")?;
    // Consume until CRLFCRLF.
    let mut buf = [0u8; 512];
    let mut head = Vec::with_capacity(512);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, stream.read(&mut buf)).await {
            Ok(Ok(0)) => return Err(anyhow!("sidecar closed before CONNECT response")),
            Ok(Ok(n)) => {
                head.extend_from_slice(&buf[..n]);
                if head.windows(4).any(|w| w == b"\r\n\r\n") {
                    return Ok(stream);
                }
            }
            Ok(Err(e)) => return Err(anyhow!("CONNECT read error: {e}")),
            Err(_) => return Err(anyhow!("CONNECT response timeout")),
        }
    }
    Err(anyhow!("CONNECT response did not complete"))
}

#[derive(Debug, Clone, Default)]
struct TunnelState {
    _bytes_written: u64,
    _bytes_read: u64,
    last_byte_at: Option<Instant>,
    closed: bool,
}

fn spawn_pump(
    mut stream: TcpStream,
    counter: Arc<Mutex<TunnelState>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = 0u64;
        let mut read_buf = [0u8; 256];
        loop {
            tick = tick.wrapping_add(1);
            let one = [(tick & 0xff) as u8];
            if stream.write_all(&one).await.is_err() {
                let mut s = counter.lock().await;
                s.closed = true;
                return;
            }
            // Short read attempt, but don't block forever.
            match tokio::time::timeout(Duration::from_millis(50), stream.read(&mut read_buf)).await
            {
                Ok(Ok(0)) => {
                    let mut s = counter.lock().await;
                    s.closed = true;
                    return;
                }
                Ok(Ok(n)) => {
                    let mut s = counter.lock().await;
                    s._bytes_read += n as u64;
                    s._bytes_written += 1;
                    s.last_byte_at = Some(Instant::now());
                }
                Ok(Err(_)) => {
                    let mut s = counter.lock().await;
                    s.closed = true;
                    return;
                }
                Err(_) => {
                    // read timed out — not closed, just no echo yet.
                    let mut s = counter.lock().await;
                    s._bytes_written += 1;
                }
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
}

/// Unused import suppressor for BTreeMap (kept so diffs over this
/// module don't shrink into clippy noise if we later add headers to
/// the drain verdict).
#[allow(dead_code)]
fn _btm_is_used(_m: &BTreeMap<String, String>) {}

#[cfg(test)]
mod tests {
    use super::*;

    // Guard against drift: FR-22 says Go must stop "within 200ms".
    // We loosen to 400ms in-runner to absorb docker-kill latency but
    // the loose bound must still be strictly less than Rust's lower
    // bound. These are `const` assertions so the compiler evaluates
    // them at build time — clippy rejects runtime asserts whose
    // operands are all constants.
    const _: () = assert!(GO_MAX_DRAIN_MS < RUST_MIN_DRAIN_MS);
    const _: () = assert!(GO_MAX_DRAIN_MS <= 500);
    const _: () = assert!(RUST_MIN_DRAIN_MS >= 1000);

    #[test]
    fn default_tunnel_state_is_empty() {
        let s = TunnelState::default();
        assert!(!s.closed);
        assert!(s.last_byte_at.is_none());
    }
}
