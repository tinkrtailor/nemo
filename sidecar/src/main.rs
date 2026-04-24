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
const PARITY_BIND_ALL_INTERFACES_ENV: &str = "NAUTILOOP_BIND_ALL_INTERFACES";

/// Graceful shutdown budget for tracked long-lived connections.
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
    let private_listener_bind_ip = private_listener_bind_ip();

    let model_listener =
        TcpListener::bind(SocketAddr::new(private_listener_bind_ip, MODEL_PROXY_PORT))
            .await
            .map_err(|e| format!("failed to bind model proxy listener: {e}"))?;
    let egress_listener = TcpListener::bind(SocketAddr::new(private_listener_bind_ip, EGRESS_PORT))
        .await
        .map_err(|e| format!("failed to bind egress logger listener: {e}"))?;
    // FR-20: health endpoint binds all interfaces, NOT loopback. See spec.
    let health_listener = TcpListener::bind(SocketAddr::new(all_interfaces, HEALTH_PORT))
        .await
        .map_err(|e| format!("failed to bind health endpoint listener: {e}"))?;
    let git_ssh_listener = TcpListener::bind(SocketAddr::new(
        private_listener_bind_ip,
        GIT_SSH_PROXY_PORT,
    ))
    .await
    .map_err(|e| format!("failed to bind git SSH proxy listener: {e}"))?;

    // Shutdown signal distributed to the model proxy, egress, and git
    // SSH proxy servers. `false` = keep running.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Separate signal for the /healthz listener so we can delay its
    // teardown until AFTER the other servers have drained. FR-27
    // requires /healthz to keep serving 200 throughout the drain so
    // readiness probes continue to see the pod as Ready.
    let (health_shutdown_tx, health_shutdown_rx) = watch::channel(false);

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
        let tls_config = tls_config.clone();
        tokio::spawn(async move {
            if let Err(e) =
                egress::serve(egress_listener, shutdown_rx, drain_tracker, tls_config).await
            {
                logging::error(&format!("egress logger exited with error: {e}"));
            }
        })
    };

    let health_handle = {
        let ready = Arc::clone(&ready);
        tokio::spawn(async move {
            if let Err(e) = health::serve(health_listener, ready, health_shutdown_rx).await {
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
    // 1. Signal the model proxy, egress, and git SSH proxy servers to
    //    stop accepting new connections. The /healthz endpoint keeps
    //    serving 200 throughout the drain (no shutdown signal sent to
    //    it yet) so k8s readiness probes still see the pod as Ready.
    let _ = shutdown_tx.send(true);

    // 2. Wait up to 5s for long-lived tracked connections (SSH sessions,
    //    CONNECT tunnels, plain HTTP forwards) to drain AND for the
    //    model proxy, egress, and git SSH proxy tasks to exit. The
    //    model proxy does per-connection graceful shutdown internally
    //    via `hyper_util::server::graceful`; egress and git SSH rely on
    //    the drain_tracker.
    let drained = drain_tracker.wait_for_drain(SHUTDOWN_DRAIN_TIMEOUT).await;
    if !drained {
        logging::warn("SSH/CONNECT/HTTP drain timed out, proceeding with shutdown");
    }

    // 3. Await the three server tasks. They should have exited once the
    //    shutdown signal propagated and their in-flight work finished.
    let _ = tokio::join!(model_handle, egress_handle, git_ssh_handle);

    // 4. NOW tell the /healthz listener to stop. Any in-flight probe
    //    still mid-request will get its response because hyper's
    //    connection loop exits after finishing current work. This is
    //    the last step so readiness probes never see 503 during
    //    shutdown.
    let _ = health_shutdown_tx.send(true);
    let _ = health_handle.await;

    logging::info("shutdown complete");
    Ok(())
}

fn private_listener_bind_ip() -> IpAddr {
    match std::env::var(PARITY_BIND_ALL_INTERFACES_ENV) {
        Ok(value) if is_truthy_env(&value) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        _ => IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
    }
}

fn is_truthy_env(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Env vars are process-global. Cargo runs tests in threads by
    // default, so two tests mutating PARITY_BIND_ALL_INTERFACES_ENV
    // concurrently race: one test removes the var mid-read of the
    // other's assertion and the default-loopback branch fires when
    // "true" was expected. Seen intermittently in CI; serialize.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn private_listener_bind_ip_defaults_to_loopback() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::remove_var(PARITY_BIND_ALL_INTERFACES_ENV);
        }
        assert_eq!(
            private_listener_bind_ip(),
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))
        );
    }

    #[test]
    fn private_listener_bind_ip_uses_all_interfaces_when_enabled() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::set_var(PARITY_BIND_ALL_INTERFACES_ENV, "true");
        }
        assert_eq!(
            private_listener_bind_ip(),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        );
        unsafe {
            std::env::remove_var(PARITY_BIND_ALL_INTERFACES_ENV);
        }
    }
}
