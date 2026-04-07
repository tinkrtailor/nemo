//! Entry point for the nautiloop auth sidecar binary.
//!
//! Startup order, readiness verification, and graceful shutdown all live
//! here. The individual servers (model proxy, git SSH proxy, egress
//! logger, health endpoint) live in their own modules and are wired up
//! below.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use nautiloop_sidecar::{
    egress, git_ssh_proxy, git_url, health, logging, model_proxy, shutdown, tls,
};
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;

/// The four ports the sidecar owns.
const MODEL_PROXY_PORT: u16 = 9090;
const GIT_SSH_PROXY_PORT: u16 = 9091;
const EGRESS_PORT: u16 = 9092;
const HEALTH_PORT: u16 = 9093;

/// Graceful shutdown budget. Matches Go `main.go:826`.
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

fn main() -> ExitCode {
    // Build the tokio runtime explicitly so we can map setup failures to a
    // plain-stderr exit per FR-25 before any tokio-backed logging exists.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("failed to start tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };

    match runtime.block_on(run()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // Per FR-25, fatal startup errors are plain stderr, not JSON.
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}

async fn run() -> Result<(), String> {
    logging::info("starting auth sidecar");

    // FR-24: parse GIT_REPO_URL. Missing or unparseable → plain stderr fatal.
    let git_repo_url = std::env::var("GIT_REPO_URL")
        .map_err(|_| "GIT_REPO_URL environment variable is required".to_string())?;
    let git_remote =
        git_url::parse(&git_repo_url).map_err(|e| format!("failed to parse GIT_REPO_URL: {e}"))?;
    logging::info(&format!(
        "git remote host: {}:{}, allowed repo: {}",
        git_remote.host, git_remote.port, git_remote.repo_path
    ));

    // Build TLS client config. Failures here are fatal startup errors per
    // FR-25 — the sidecar cannot safely serve the model proxy without it.
    let tls_config = tls::build_client_config()
        .map_err(|e| format!("failed to build TLS client config: {e}"))?;

    // Bind all four listeners BEFORE we start accepting. Any bind failure
    // is a fatal startup error.
    let loopback = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
    let all_interfaces = IpAddr::V4(Ipv4Addr::UNSPECIFIED);

    let model_listener = TcpListener::bind(SocketAddr::new(loopback, MODEL_PROXY_PORT))
        .await
        .map_err(|e| format!("failed to bind model proxy listener: {e}"))?;
    let egress_listener = TcpListener::bind(SocketAddr::new(loopback, EGRESS_PORT))
        .await
        .map_err(|e| format!("failed to bind egress logger listener: {e}"))?;
    // FR-20: health endpoint binds all interfaces, NOT loopback. See spec.
    let health_listener = TcpListener::bind(SocketAddr::new(all_interfaces, HEALTH_PORT))
        .await
        .map_err(|e| format!("failed to bind health endpoint listener: {e}"))?;
    let git_ssh_listener = TcpListener::bind(SocketAddr::new(loopback, GIT_SSH_PROXY_PORT))
        .await
        .map_err(|e| format!("failed to bind git SSH proxy listener: {e}"))?;

    // Shutdown signal distributed to each server. `false` = keep running.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Connection tracker counts in-flight SSH sessions AND CONNECT tunnels.
    // Both long-lived connection types are tracked here so FR-27 step 3
    // can drain them together.
    let drain_tracker = shutdown::ConnectionTracker::new();

    // Readiness flag. Starts false; flipped by verify_readiness below.
    // FR-27 explicitly says /healthz does NOT drop to 503 on shutdown,
    // so nothing else ever mutates this atomic.
    let ready = Arc::new(AtomicBool::new(false));

    // Spawn servers.
    let model_handle = {
        let shutdown_rx = shutdown_rx.clone();
        let tls_config = tls_config.clone();
        tokio::spawn(async move {
            if let Err(e) = model_proxy::serve(model_listener, shutdown_rx, tls_config).await {
                logging::error(&format!("model proxy exited with error: {e}"));
            }
        })
    };

    let egress_handle = {
        let shutdown_rx = shutdown_rx.clone();
        let drain_tracker = drain_tracker.clone();
        tokio::spawn(async move {
            if let Err(e) = egress::serve(egress_listener, shutdown_rx, drain_tracker).await {
                logging::error(&format!("egress logger exited with error: {e}"));
            }
        })
    };

    let health_handle = {
        let shutdown_rx = shutdown_rx.clone();
        let ready = Arc::clone(&ready);
        tokio::spawn(async move {
            if let Err(e) = health::serve(health_listener, ready, shutdown_rx).await {
                logging::error(&format!("health endpoint exited with error: {e}"));
            }
        })
    };

    let git_ssh_handle = {
        let shutdown_rx = shutdown_rx.clone();
        let drain_tracker = drain_tracker.clone();
        let git_remote = git_remote.clone();
        tokio::spawn(async move {
            if let Err(e) =
                git_ssh_proxy::serve(git_ssh_listener, shutdown_rx, drain_tracker, git_remote).await
            {
                logging::error(&format!("git SSH proxy exited with error: {e}"));
            }
        })
    };

    // FR-22: verify readiness by dialing each loopback port.
    let probe_targets = [
        SocketAddr::new(loopback, MODEL_PROXY_PORT),
        SocketAddr::new(loopback, GIT_SSH_PROXY_PORT),
        SocketAddr::new(loopback, EGRESS_PORT),
        SocketAddr::new(loopback, HEALTH_PORT),
    ];
    health::verify_readiness(&probe_targets)
        .await
        .map_err(|e| format!("readiness verification failed: {e}"))?;

    // Flip the gate and write the readiness file.
    ready.store(true, Ordering::SeqCst);
    if let Err(e) = health::write_ready_file(health::DEFAULT_READY_PATH) {
        return Err(format!("failed to write readiness file: {e}"));
    }
    logging::info("all ports ready, readiness file written");

    // Wait for SIGTERM or SIGINT.
    let mut sigterm = signal(SignalKind::terminate())
        .map_err(|e| format!("failed to register SIGTERM handler: {e}"))?;
    let mut sigint = signal(SignalKind::interrupt())
        .map_err(|e| format!("failed to register SIGINT handler: {e}"))?;

    tokio::select! {
        _ = sigterm.recv() => {
            logging::info("received SIGTERM, draining connections");
        }
        _ = sigint.recv() => {
            logging::info("received SIGINT, draining connections");
        }
    }

    // FR-27 shutdown sequence:
    //
    // 1. Signal all four servers that shutdown is starting. Each server's
    //    accept loop will stop accepting new connections. The health
    //    endpoint keeps serving 200 for in-flight probe requests until its
    //    listener is closed at the end of step 4 (we only tell it to stop
    //    AFTER the drain wait, not at the same time as the others).
    let _ = shutdown_tx.send(true);

    // 2. Wait up to 5s for SSH sessions + CONNECT tunnels to drain.
    let drained = drain_tracker.wait_for_drain(SHUTDOWN_DRAIN_TIMEOUT).await;
    if !drained {
        logging::warn("SSH/CONNECT drain timed out, proceeding with shutdown");
    }

    // 3. Await all server tasks. They should exit once the shutdown signal
    //    propagates and their in-flight work completes.
    let _ = tokio::join!(model_handle, egress_handle, git_ssh_handle, health_handle);

    logging::info("shutdown complete");
    Ok(())
}
