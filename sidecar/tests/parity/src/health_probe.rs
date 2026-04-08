//! Health polling for mock services (FR-17 step 3) and sidecars
//! (FR-17 step 4).
//!
//! Uses exponential backoff capped at the provided deadline. On
//! failure, the error message names the specific service that did
//! not come up (FR-17 explicit requirement).

use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use tokio::net::TcpStream;
use tokio::time::sleep;

use crate::compose::ports;

/// Poll the five mock services until each is healthy or the deadline
/// expires.
///
/// - `mock-openai`, `mock-anthropic`, `mock-example`: HTTP GET
///   `/_healthz` on the published port, expect 200.
/// - `mock-github-ssh`, `mock-tcp-echo`: TCP connect to the published
///   health port.
pub async fn wait_mock_health(deadline: Duration) -> Result<()> {
    let start = Instant::now();
    wait_http_200("mock-openai", ports::MOCK_OPENAI_HEALTH, start, deadline).await?;
    wait_http_200(
        "mock-anthropic",
        ports::MOCK_ANTHROPIC_HEALTH,
        start,
        deadline,
    )
    .await?;
    wait_http_200("mock-example", ports::MOCK_EXAMPLE_HEALTH, start, deadline).await?;
    wait_tcp_connect(
        "mock-github-ssh",
        ports::MOCK_GH_SSH_HEALTH,
        start,
        deadline,
    )
    .await?;
    wait_tcp_connect(
        "mock-tcp-echo",
        ports::MOCK_TCP_ECHO_HEALTH,
        start,
        deadline,
    )
    .await?;
    Ok(())
}

/// Poll both sidecars' `/healthz` endpoints until both return 200.
pub async fn wait_sidecar_ready(deadline: Duration) -> Result<()> {
    let start = Instant::now();
    wait_http_200("sidecar-go", ports::GO_HEALTH, start, deadline).await?;
    wait_http_200("sidecar-rust", ports::RUST_HEALTH, start, deadline).await?;
    Ok(())
}

async fn wait_http_200(name: &str, port: u16, start: Instant, deadline: Duration) -> Result<()> {
    let client = match reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(500))
        .timeout(Duration::from_millis(500))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return Err(anyhow!(
                "failed to build health probe client for {name}: {e}"
            ));
        }
    };
    let url = if name.starts_with("sidecar-") {
        format!("http://127.0.0.1:{port}/healthz")
    } else {
        format!("http://127.0.0.1:{port}/_healthz")
    };
    let mut backoff_ms = 100u64;
    loop {
        if start.elapsed() > deadline {
            return Err(anyhow!(
                "{name} did not reach healthy state at {url} within {:?}",
                deadline
            ));
        }
        match client.get(&url).send().await {
            Ok(r) if r.status().as_u16() == 200 => {
                tracing::debug!(%name, %url, "mock/sidecar health OK");
                return Ok(());
            }
            Ok(r) => {
                tracing::trace!(%name, %url, status = %r.status(), "health not ready");
            }
            Err(e) => {
                tracing::trace!(%name, %url, error = %e, "health probe error");
            }
        }
        sleep(Duration::from_millis(backoff_ms)).await;
        backoff_ms = (backoff_ms * 2).min(500);
    }
}

async fn wait_tcp_connect(name: &str, port: u16, start: Instant, deadline: Duration) -> Result<()> {
    let addr = format!("127.0.0.1:{port}");
    let mut backoff_ms = 100u64;
    loop {
        if start.elapsed() > deadline {
            return Err(anyhow!(
                "{name} did not accept TCP at {addr} within {:?}",
                deadline
            ));
        }
        match tokio::time::timeout(Duration::from_millis(500), TcpStream::connect(&addr)).await {
            Ok(Ok(_)) => {
                tracing::debug!(%name, %addr, "mock health TCP connect OK");
                return Ok(());
            }
            Ok(Err(e)) => {
                tracing::trace!(%name, %addr, error = %e, "tcp connect error");
            }
            Err(_) => {
                tracing::trace!(%name, %addr, "tcp connect timeout");
            }
        }
        sleep(Duration::from_millis(backoff_ms)).await;
        backoff_ms = (backoff_ms * 2).min(500);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wait_http_200_reports_named_mock_on_timeout() {
        // Pick a port that is guaranteed not to be listening.
        let err = wait_http_200(
            "synthetic-mock",
            1,
            Instant::now(),
            Duration::from_millis(300),
        )
        .await
        .expect_err("should time out");
        let msg = format!("{err}");
        assert!(
            msg.contains("synthetic-mock"),
            "error message must name the mock, got: {msg}"
        );
        assert!(msg.contains("within"), "message should mention timeout");
    }

    #[tokio::test]
    async fn wait_tcp_connect_reports_named_mock_on_timeout() {
        let err = wait_tcp_connect(
            "synthetic-tcp",
            1,
            Instant::now(),
            Duration::from_millis(300),
        )
        .await
        .expect_err("should time out");
        let msg = format!("{err}");
        assert!(msg.contains("synthetic-tcp"));
    }

    #[tokio::test]
    async fn wait_tcp_connect_succeeds_on_reachable_listener() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Accept in the background so the SYN completes cleanly.
        let _accept = tokio::spawn(async move {
            loop {
                if listener.accept().await.is_err() {
                    return;
                }
            }
        });
        wait_tcp_connect(
            "ephemeral",
            addr.port(),
            Instant::now(),
            Duration::from_secs(2),
        )
        .await
        .expect("tcp connect must succeed against real listener");
    }
}
