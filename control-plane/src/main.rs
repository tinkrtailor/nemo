use std::sync::Arc;
use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use tokio::net::TcpListener;
use tokio::sync::{Notify, watch};
use tracing_subscriber::EnvFilter;

use nautiloop_control_plane::api::{self, AppState};
use nautiloop_control_plane::config::NautiloopConfig;
use nautiloop_control_plane::git::GitOperations;
use nautiloop_control_plane::k8s::JobDispatcher;
use nautiloop_control_plane::k8s::client::KubeJobDispatcher;
use nautiloop_control_plane::loop_engine::{ConvergentLoopDriver, Reconciler, watcher::JobWatcher};
use nautiloop_control_plane::state::StateStore;
use nautiloop_control_plane::state::postgres::PgStateStore;

/// Run mode selected by the first CLI argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Run API server only (serves HTTP requests, health endpoint).
    ApiServer,
    /// Run loop engine only (reconciler + K8s job watcher).
    LoopEngine,
}

fn parse_mode() -> anyhow::Result<Mode> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("api-server") => Ok(Mode::ApiServer),
        Some("loop-engine") => Ok(Mode::LoopEngine),
        Some(other) => anyhow::bail!(
            "Unknown mode '{}'. Usage: nautiloop-server <api-server|loop-engine>",
            other
        ),
        None => anyhow::bail!("Usage: nautiloop-server <api-server|loop-engine>"),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let mode = parse_mode()?;
    tracing::info!(?mode, "Starting Nautiloop control plane");

    // TODO(V1.5): Replace flat NautiloopConfig with three-layer config merge
    // (cluster -> repo nemo.toml -> engineer ~/.nemo/config.toml) using
    // config::merged::MergedConfig. V1 uses flat NautiloopConfig; engineer-level
    // model/limit overrides are deferred to V1.5. The merge modules exist
    // in config/cluster.rs, config/engineer.rs, config/merged.rs, config/repo.rs.
    let config = NautiloopConfig::load().map_err(|e| anyhow::anyhow!(e))?;
    let config_arc = Arc::new(config.clone());

    // API server needs NAUTILOOP_API_KEY for auth middleware
    if mode == Mode::ApiServer && std::env::var("NAUTILOOP_API_KEY").is_err() {
        anyhow::bail!("NAUTILOOP_API_KEY environment variable is required for api-server mode");
    }

    // Connect to Postgres and run migrations
    let database_url =
        std::env::var("DATABASE_URL").unwrap_or_else(|_| config.cluster.database_url.clone());

    let pool = PgPoolOptions::new()
        .max_connections(config.cluster.max_connections)
        .connect(&database_url)
        .await?;

    let pg_store = PgStateStore::new(pool);
    pg_store.run_migrations().await?;
    tracing::info!("Database migrations complete");

    let store: Arc<dyn StateStore> = Arc::new(pg_store);

    // Setup shutdown signal
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    match mode {
        Mode::ApiServer => {
            let kube_client = kube::Client::try_default().await?;
            tracing::info!("Connected to Kubernetes cluster");

            let bare_repo_path = std::env::var("BARE_REPO_PATH")
                .or_else(|_| std::env::var("NAUTILOOP_BARE_REPO_PATH"))
                .unwrap_or_else(|_| "/bare-repo".to_string());
            let git: Arc<dyn GitOperations> = Arc::new(
                nautiloop_control_plane::git::bare::BareRepoGitOperations::new(&bare_repo_path),
            );

            let app_state = AppState {
                store: store.clone(),
                git,
                config: config_arc,
                kube_client: Some(kube_client),
            };
            let router = api::build_router(app_state);

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

            wait_for_shutdown().await?;
            tracing::info!("Received shutdown signal");
            shutdown_tx.send(true)?;
            let _ = server_handle.await;
        }

        Mode::LoopEngine => {
            let kube_client = kube::Client::try_default().await?;
            tracing::info!("Connected to Kubernetes cluster");

            let dispatcher: Arc<dyn JobDispatcher> = Arc::new(KubeJobDispatcher::new(
                kube_client.clone(),
                config.cluster.jobs_namespace.clone(),
            ));

            let bare_repo_path = std::env::var("BARE_REPO_PATH")
                .or_else(|_| std::env::var("NAUTILOOP_BARE_REPO_PATH"))
                .unwrap_or_else(|_| "/bare-repo".to_string());
            let git: Arc<dyn GitOperations> = Arc::new(
                nautiloop_control_plane::git::bare::BareRepoGitOperations::new(&bare_repo_path),
            );

            let mut driver = ConvergentLoopDriver::new(
                store.clone(),
                dispatcher,
                git,
                config.clone(),
            );

            // Wire up the orchestrator judge if enabled
            if config.orchestrator.judge_enabled {
                let judge_client = Arc::new(
                    nautiloop_control_plane::loop_engine::judge::SidecarJudgeClient::new(),
                );
                let judge = Arc::new(
                    nautiloop_control_plane::loop_engine::judge::OrchestratorJudge::new(
                        config.orchestrator.clone(),
                        store.clone(),
                        judge_client,
                    )
                    .await,
                );
                driver = driver.with_judge(judge);
            }

            let driver = Arc::new(driver);

            let wake = Arc::new(Notify::new());

            let reconciler = Reconciler::new(
                driver,
                store.clone(),
                Duration::from_secs(config.cluster.reconcile_interval_secs),
                wake.clone(),
            );

            let reconciler_rx = shutdown_rx.clone();
            let mut reconciler_handle = tokio::spawn(async move {
                reconciler.run(reconciler_rx).await;
            });

            let watcher_client = kube::Client::try_default().await?;
            let job_watcher = JobWatcher::new(wake);
            let watcher_namespace = config.cluster.jobs_namespace.clone();
            let watcher_rx = shutdown_rx.clone();
            let mut watcher_handle = tokio::spawn(async move {
                job_watcher
                    .run(watcher_client, &watcher_namespace, watcher_rx)
                    .await;
            });

            // Supervise: exit if shutdown signal OR any background task dies.
            // A dead reconciler/watcher means the pod is inert — must restart.
            tokio::select! {
                _ = wait_for_shutdown() => {
                    tracing::info!("Received shutdown signal");
                }
                result = &mut reconciler_handle => {
                    tracing::error!(?result, "Reconciler task exited unexpectedly");
                }
                result = &mut watcher_handle => {
                    tracing::error!(?result, "Job watcher task exited unexpectedly");
                }
            }
            // Signal all tasks to stop, then drain in-flight work
            shutdown_tx.send(true)?;
            let _ = tokio::join!(reconciler_handle, watcher_handle);
        }
    }

    tracing::info!("Nautiloop control plane shut down");
    Ok(())
}

/// Wait for SIGTERM (K8s pod shutdown) or SIGINT (ctrl-c).
async fn wait_for_shutdown() -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = signal(SignalKind::terminate())?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = sigterm.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
    }
    Ok(())
}
