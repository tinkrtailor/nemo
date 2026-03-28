use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::{watch, Notify};
use tracing_subscriber::EnvFilter;

use nemo_control_plane::api::{self, AppState};
use nemo_control_plane::config::NemoConfig;
use nemo_control_plane::git::mock::MockGitOperations;
use nemo_control_plane::k8s::mock::MockJobDispatcher;
use nemo_control_plane::loop_engine::{ConvergentLoopDriver, Reconciler};
use nemo_control_plane::state::memory::MemoryStateStore;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    tracing::info!("Starting Nemo control plane");

    let config = NemoConfig::default();
    let config_arc = Arc::new(config.clone());

    // For now, use in-memory/mock implementations.
    // Production: use PgStateStore, KubeJobDispatcher, real GitOperations.
    let store: Arc<MemoryStateStore> = Arc::new(MemoryStateStore::new());
    let dispatcher: Arc<MockJobDispatcher> = Arc::new(MockJobDispatcher::new());
    let git: Arc<MockGitOperations> = Arc::new(MockGitOperations::new());

    // Build the loop driver
    let driver = Arc::new(ConvergentLoopDriver::new(
        store.clone(),
        dispatcher.clone(),
        git.clone(),
        config.clone(),
    ));

    // Build the API server
    let app_state = AppState {
        store: store.clone(),
        git: git.clone(),
        config: config_arc,
    };
    let router = api::build_router(app_state);

    // Setup shutdown signal
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let wake = Arc::new(Notify::new());

    // Start reconciler
    let reconciler = Reconciler::new(
        driver,
        store.clone(),
        Duration::from_secs(config.cluster.reconcile_interval_secs),
        wake,
    );

    let reconciler_rx = shutdown_rx.clone();
    let reconciler_handle = tokio::spawn(async move {
        reconciler.run(reconciler_rx).await;
    });

    // Start API server
    let bind_addr = format!("{}:{}", config.cluster.bind_addr, config.cluster.port);
    tracing::info!(addr = %bind_addr, "Starting API server");

    let listener = TcpListener::bind(&bind_addr).await?;
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                let mut rx = shutdown_rx;
                while !*rx.borrow() {
                    if rx.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await
            .expect("Server failed");
    });

    // Wait for SIGTERM/SIGINT
    tokio::signal::ctrl_c().await?;
    tracing::info!("Received shutdown signal");

    // Signal all tasks to stop
    shutdown_tx.send(true)?;

    // Wait for tasks to finish
    let _ = tokio::join!(reconciler_handle, server_handle);

    tracing::info!("Nemo control plane shut down");
    Ok(())
}
