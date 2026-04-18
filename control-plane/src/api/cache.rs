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

    // Attempt disk usage from a running pod if we have a kube client.
    let disk_usage = if let Some(ref client) = state.kube_client {
        get_cache_disk_usage(client, &state.config.cluster.jobs_namespace).await
    } else {
        None
    };

    let response = CacheResponse {
        disabled: resolved.disabled,
        env: resolved.env,
        disk_usage,
    };

    Ok((StatusCode::OK, Json(response)))
}

/// Exec `du -sh /cache/*` on a running implement/revise pod.
/// Returns None if no running pod is found or exec fails.
///
/// Since agent pods typically complete quickly, the window for a running pod is
/// small — disk usage will frequently be `unavailable`. No ephemeral pod or
/// debug container is spawned to inspect the PVC.
async fn get_cache_disk_usage(
    client: &kube::Client,
    namespace: &str,
) -> Option<CacheDiskUsage> {
    use k8s_openapi::api::core::v1::Pod;
    use kube::api::{Api, AttachParams, ListParams};

    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);

    // Find running implement/revise pods, sorted by creation time desc.
    let lp = ListParams::default()
        .labels("nautiloop.dev/stage in (implement, revise)");

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
        .filter(|p| {
            p.status
                .as_ref()
                .and_then(|s| s.phase.as_deref())
                == Some("Running")
        })
        .collect();

    running_pods.sort_by(|a, b| {
        let ts_a = a.metadata.creation_timestamp.as_ref();
        let ts_b = b.metadata.creation_timestamp.as_ref();
        ts_b.cmp(&ts_a) // Most recent first
    });

    let pod = running_pods.first()?;
    let pod_name = pod.metadata.name.as_deref()?;

    // Exec `du -sh /cache/*` in the agent container.
    let ap = AttachParams::default()
        .container("agent")
        .stdout(true)
        .stderr(true);

    let mut exec = match pods
        .exec(pod_name, vec!["du", "-sh", "/cache/*"], &ap)
        .await
    {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!("Failed to exec into pod {pod_name} for cache disk usage: {e}");
            return None;
        }
    };

    // Read stdout using the kube-rs async reader.
    use tokio::io::AsyncReadExt;
    let stdout = exec.stdout()?;
    let mut buf = vec![0u8; 65536];
    let output = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        async {
            let mut reader = stdout;
            reader.read(&mut buf).await
        },
    )
    .await
    {
        Ok(Ok(n)) => String::from_utf8_lossy(&buf[..n]).to_string(),
        _ => {
            tracing::debug!("Timeout or error reading du output from pod {pod_name}");
            return None;
        }
    };

    // Parse du output: "1.8G\t/cache/sccache\n340M\t/cache/npm\n"
    let mut subdirectories = std::collections::HashMap::new();
    for line in output.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() == 2 {
            subdirectories.insert(parts[1].to_string(), parts[0].to_string());
        }
    }

    let total = format!("{} subdirectories", subdirectories.len());

    Some(CacheDiskUsage {
        subdirectories,
        total,
    })
}
