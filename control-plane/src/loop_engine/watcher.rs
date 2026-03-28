use std::sync::Arc;
use tokio::sync::Notify;

/// K8s Job watcher that monitors Job status changes and wakes the reconciler.
///
/// Uses `kube::runtime::watcher` on `batch/v1/Job` with label selector `app=nemo`.
/// On any Job status change, signals the reconciler to wake up via `Notify`.
/// The watcher does NOT write to Postgres directly -- only the reconciler does.
pub struct JobWatcher {
    wake: Arc<Notify>,
}

impl JobWatcher {
    pub fn new(wake: Arc<Notify>) -> Self {
        Self { wake }
    }

    /// Run the watcher using a real kube-rs client.
    /// This requires an in-cluster or configured kubeconfig.
    pub async fn run(
        &self,
        client: kube::Client,
        namespace: &str,
        cancel: tokio::sync::watch::Receiver<bool>,
    ) {
        use futures::{StreamExt, TryStreamExt};
        use k8s_openapi::api::batch::v1::Job;
        use kube::api::Api;
        use kube::runtime::watcher;
        use kube::runtime::watcher::Config;

        let jobs_api: Api<Job> = Api::namespaced(client, namespace);
        let watcher_config = Config::default().labels("app=nemo");

        let mut stream = watcher(jobs_api, watcher_config).boxed();

        loop {
            tokio::select! {
                event = stream.try_next() => {
                    match event {
                        Ok(Some(_event)) => {
                            // Any job event: wake the reconciler
                            self.wake.notify_one();
                        }
                        Ok(None) => {
                            tracing::warn!("Job watcher stream ended");
                            break;
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Job watcher error");
                            // Brief pause before retrying
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        }
                    }
                }
                _ = wait_for_cancel(&cancel) => {
                    tracing::info!("Job watcher shutting down");
                    break;
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
