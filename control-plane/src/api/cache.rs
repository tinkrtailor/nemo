use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;

use super::AppState;
use crate::types::api::{CacheDiskUsage, CacheResponse};

/// Query parameters for GET /cache.
#[derive(Debug, serde::Deserialize)]
pub struct CacheQuery {
    /// Unused in the API (CLI uses --json client-side), but accepted for forward compat.
    #[serde(default)]
    pub json: Option<bool>,
}

/// GET /cache — return resolved cache configuration and optional disk usage.
///
/// The env vars returned are the resolved config as loaded by the control plane
/// process (sccache defaults if `[cache]` is absent, or the explicit `[cache.env]`
/// values). Disk usage requires exec into a running agent pod and is omitted
/// when no suitable pod is available.
pub async fn cache_show(
    State(state): State<AppState>,
    Query(_query): Query<CacheQuery>,
) -> Result<impl IntoResponse, StatusCode> {
    let resolved = state.config.resolved_cache_config();
    let namespace = &state.config.cluster.jobs_namespace;

    // FR-3d: when cache is disabled, no /cache mount exists — skip kube API calls.
    let (disk_usage, volume_capacity_gi) = if resolved.disabled {
        (None, None)
    } else if let Some(ref client) = state.kube_client {
        let disk = get_cache_disk_usage(client, namespace).await;
        let cap = get_pvc_capacity(client, namespace).await;
        (disk, cap)
    } else {
        (None, None)
    };

    let response = CacheResponse {
        disabled: resolved.disabled,
        env: resolved.env,
        volume_name: "nautiloop-cache".to_string(),
        volume_capacity_gi,
        disk_usage,
    };

    Ok((StatusCode::OK, Json(response)))
}

/// Read the PVC's `status.capacity["storage"]` to get the provisioned size in GiB.
async fn get_pvc_capacity(client: &kube::Client, namespace: &str) -> Option<u64> {
    use k8s_openapi::api::core::v1::PersistentVolumeClaim;
    use kube::api::Api;

    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), namespace);

    let pvc = match pvcs.get("nautiloop-cache").await {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!("Failed to get nautiloop-cache PVC: {e}");
            return None;
        }
    };

    // Read from status.capacity.storage (the actual provisioned size).
    let storage = pvc.status?.capacity?.get("storage")?.0.clone();

    // Parse quantity string like "50Gi" or "20Gi" → u64 GiB.
    parse_gi_quantity(&storage)
}

/// Parse a Kubernetes quantity string (e.g. "50Gi", "20Gi") to GiB as u64.
fn parse_gi_quantity(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(num) = s.strip_suffix("Gi") {
        num.parse().ok()
    } else if let Some(num) = s.strip_suffix("G") {
        // G = 10^9 bytes, Gi = 2^30 bytes. Approximate.
        num.parse::<u64>().ok()
    } else {
        // Try parsing as raw bytes and convert to GiB.
        s.parse::<u64>().ok().map(|b| b / (1024 * 1024 * 1024))
    }
}

/// Exec `du` on a running implement/revise pod to get cache disk usage.
/// Returns None if no running pod is found or exec fails.
///
/// Since agent pods typically complete quickly, the window for a running pod is
/// small — disk usage will frequently be `unavailable`. No ephemeral pod or
/// debug container is spawned to inspect the PVC.
async fn get_cache_disk_usage(client: &kube::Client, namespace: &str) -> Option<CacheDiskUsage> {
    use k8s_openapi::api::core::v1::Pod;
    use kube::api::{Api, ListParams};

    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);

    // Find running implement/revise pods, sorted by creation time desc.
    let lp = ListParams::default().labels("nautiloop.dev/stage in (implement, revise)");

    let pod_list = match pods.list(&lp).await {
        Ok(list) => list,
        Err(e) => {
            tracing::debug!("Failed to list pods for cache disk usage: {e}");
            return None;
        }
    };

    // Filter to Running pods, sort by creation time descending.
    let mut running_pods: Vec<_> = pod_list
        .items
        .into_iter()
        .filter(|p| p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running"))
        .collect();

    running_pods.sort_by(|a, b| {
        let ts_a = a.metadata.creation_timestamp.as_ref();
        let ts_b = b.metadata.creation_timestamp.as_ref();
        ts_b.cmp(&ts_a) // Most recent first
    });

    let pod = running_pods.first()?;
    let pod_name = pod.metadata.name.as_deref()?;

    // Get total size first: `du -sh /cache`
    let total = exec_in_pod(&pods, pod_name, &["sh", "-c", "du -sh /cache 2>/dev/null"])
        .await
        .and_then(|output| {
            output.lines().next().and_then(|line| {
                let parts: Vec<&str> = line.split('\t').collect();
                if parts.len() == 2 {
                    Some(parts[0].to_string())
                } else {
                    None
                }
            })
        })
        .unwrap_or_else(|| "0".to_string());

    // Get per-subdirectory sizes: `du -sh /cache/*` (needs shell for glob expansion)
    let subdirectories = exec_in_pod(
        &pods,
        pod_name,
        &["sh", "-c", "du -sh /cache/* 2>/dev/null"],
    )
    .await
    .map(|output| {
        let mut dirs = std::collections::HashMap::new();
        for line in output.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() == 2 {
                dirs.insert(parts[1].to_string(), parts[0].to_string());
            }
        }
        dirs
    })
    .unwrap_or_default();

    Some(CacheDiskUsage {
        subdirectories,
        total,
    })
}

/// Execute a command in the agent container of a pod and return stdout as a string.
async fn exec_in_pod(
    pods: &kube::api::Api<k8s_openapi::api::core::v1::Pod>,
    pod_name: &str,
    command: &[&str],
) -> Option<String> {
    use kube::api::AttachParams;
    use tokio::io::AsyncReadExt;

    let ap = AttachParams::default()
        .container("agent")
        .stdout(true)
        .stderr(false);

    let mut exec = match pods.exec(pod_name, command.to_vec(), &ap).await {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!("Failed to exec into pod {pod_name}: {e}");
            return None;
        }
    };

    // Read all stdout using the kube-rs async reader.
    let stdout = exec.stdout()?;
    let mut output = String::new();
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut reader = stdout;
        reader.read_to_string(&mut output).await
    })
    .await;

    match result {
        Ok(Ok(_)) => Some(output),
        _ => {
            tracing::debug!("Timeout or error reading exec output from pod {pod_name}");
            // Return whatever we got so far, if anything.
            if output.is_empty() {
                None
            } else {
                Some(output)
            }
        }
    }
}
