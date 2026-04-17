use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use chrono::Utc;
use uuid::Uuid;

use super::AppState;
use crate::error::NautiloopError;
use crate::types::api::{
    ContainerStats, PodIntrospectResponse, ProcessInfo, WorktreeInfo,
};

/// GET /pod-introspect/:loop_id — runtime snapshot of the agent container (FR-1a).
///
/// Returns a JSON snapshot of processes, CPU/mem, and worktree state.
/// The handler has a 3s overall timeout (FR-1c). If the pod exec takes > 2s,
/// partial output is still parsed. If metrics-server is absent, container_stats
/// is null (FR-1e).
pub async fn pod_introspect(
    State(state): State<AppState>,
    Path(loop_id): Path<Uuid>,
) -> Result<impl IntoResponse, NautiloopError> {
    let record = state
        .store
        .get_loop(loop_id)
        .await?
        .ok_or(NautiloopError::LoopNotFound { id: loop_id })?;

    // FR-1d: terminal loops have no pod to introspect
    if record.state.is_terminal() {
        return Ok((
            StatusCode::GONE,
            Json(serde_json::json!({
                "error": format!(
                    "loop {} is {}. Run `nemo inspect` for round history.",
                    loop_id, record.state
                )
            })),
        )
            .into_response());
    }

    let Some(job_name) = record.active_job_name.clone() else {
        // NFR-4: pod not yet started — HTTP 425
        return Ok((
            StatusCode::from_u16(425).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
            Json(serde_json::json!({"error": "pod not yet running"})),
        )
            .into_response());
    };

    let kube_client = state.kube_client.as_ref().ok_or_else(|| {
        NautiloopError::Internal("K8s client not available".to_string())
    })?;
    let namespace = &state.config.cluster.jobs_namespace;

    // Validate job_name is a safe Kubernetes label value (alphanumeric + hyphens + dots)
    if !job_name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_')
    {
        return Ok((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "invalid job name in database"})),
        )
            .into_response());
    }

    // FR-1c: 3s overall timeout covers pod listing + exec + metrics.
    // The intent is that callers never block longer than 3s for the k8s-dependent
    // portion of the handler (pod list, exec, and metrics fetch).
    let handler_result = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        // Find the running pod for this job
        let pods_api: kube::Api<k8s_openapi::api::core::v1::Pod> =
            kube::Api::namespaced(kube_client.clone(), namespace);
        let lp = kube::api::ListParams::default()
            .labels(&format!("job-name={job_name}"))
            .limit(10);
        let pod_list = pods_api.list(&lp).await.map_err(|e| {
            NautiloopError::Internal(format!("Failed to list pods for {job_name}: {e}"))
        })?;

        if pod_list.items.is_empty() {
            return Ok::<_, NautiloopError>(None);
        }

        // Pick the best pod (Running > Pending > rest)
        let mut sorted_pods = pod_list.items;
        sorted_pods.sort_by(|a, b| {
            let phase_rank = |p: &k8s_openapi::api::core::v1::Pod| -> u8 {
                match p.status.as_ref().and_then(|s| s.phase.as_deref()) {
                    Some("Running") => 0,
                    Some("Pending") => 1,
                    _ => 2,
                }
            };
            phase_rank(a).cmp(&phase_rank(b))
        });
        let pod = &sorted_pods[0];
        let pod_name = pod
            .metadata
            .name
            .as_deref()
            .unwrap_or("unknown")
            .to_string();
        let pod_phase = pod
            .status
            .as_ref()
            .and_then(|s| s.phase.clone())
            .unwrap_or_else(|| "Unknown".to_string());

        // NFR-4: Pending pods have no running containers — exec will always fail.
        // Return 425 instead of attempting exec and producing a misleading empty snapshot.
        if pod_phase == "Pending" {
            return Ok::<_, NautiloopError>(None);
        }

        // FR-2a + FR-2b: run exec and metrics fetch concurrently
        let (exec_result, container_stats) = tokio::join!(
            exec_introspect_script(kube_client, &pod_name, namespace),
            fetch_container_metrics(kube_client, &pod_name, namespace),
        );
        Ok(Some((pod_name, pod_phase, exec_result, container_stats)))
    })
    .await;

    let (pod_name, pod_phase, exec_result, container_stats) = match handler_result {
        Ok(Ok(Some(results))) => results,
        Ok(Ok(None)) => {
            // No pods found → 425
            return Ok((
                StatusCode::from_u16(425).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
                Json(serde_json::json!({"error": "pod not yet running"})),
            )
                .into_response());
        }
        Ok(Err(e)) => {
            return Err(e);
        }
        Err(_) => {
            // FR-1c: overall 3s timeout exceeded → HTTP 503
            return Ok((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "pod introspection timeout"})),
            )
                .into_response());
        }
    };

    let collected_at = Utc::now();

    // Parse exec output — timeout/failure → partial snapshot with metrics (FR-2c, NFR-4)
    let (processes, worktree) = match exec_result {
        Ok(output) => parse_introspect_output(&output),
        Err(ExecError::Timeout(msg)) => {
            tracing::warn!(pod = %pod_name, error = %msg, "introspect exec timed out, returning partial snapshot");
            (Vec::new(), default_worktree())
        }
        Err(ExecError::Other(e)) => {
            tracing::warn!(pod = %pod_name, error = %e, "introspect exec failed, returning partial");
            (Vec::new(), default_worktree())
        }
    };

    let response = PodIntrospectResponse {
        loop_id,
        pod_name: pod_name.clone(),
        pod_phase,
        collected_at,
        container_stats,
        processes,
        worktree,
    };

    // FR-6a: optionally record snapshot to pod_snapshots table
    if state.config.observability.record_introspection
        && let Some(ref pool) = state.pool
        && let Ok(snapshot_json) = serde_json::to_value(&response)
    {
        let pool = pool.clone();
        let snap_loop_id = loop_id;
        let snap_pod = pod_name.clone();
        tokio::spawn(async move {
            let _ = sqlx::query(
                "INSERT INTO pod_snapshots (loop_id, pod_name, snapshot) VALUES ($1, $2, $3)",
            )
            .bind(snap_loop_id)
            .bind(snap_pod)
            .bind(snapshot_json)
            .execute(&pool)
            .await;
        });
    }

    Ok((StatusCode::OK, Json(response)).into_response())
}

/// Error type for exec operations, distinguishing timeouts from other failures.
#[derive(Debug)]
enum ExecError {
    Timeout(String),
    Other(String),
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecError::Timeout(msg) => write!(f, "{msg}"),
            ExecError::Other(msg) => write!(f, "{msg}"),
        }
    }
}

/// Execute the introspection script on the agent container via pod exec.
/// Has a 3s wall-clock budget (FR-1c). The script itself has a 2s timeout (FR-2c).
async fn exec_introspect_script(
    client: &kube::Client,
    pod_name: &str,
    namespace: &str,
) -> Result<String, ExecError> {
    use tokio::io::AsyncReadExt;

    let pods_api: kube::Api<k8s_openapi::api::core::v1::Pod> =
        kube::Api::namespaced(client.clone(), namespace);

    let attach_params = kube::api::AttachParams {
        container: Some("agent".to_string()),
        stdin: false,
        stdout: true,
        stderr: true,
        tty: false,
        ..Default::default()
    };

    // The script has `timeout 2` internally; the exec handshake gets 2s so
    // loaded clusters with slow API server -> kubelet chains won't miss data.
    // The read timeout is 1s since the script output arrives in a single burst
    // once the 2s script completes. Total budget (2s handshake + 1s read) = 3s,
    // bounded by the outer handler timeout.
    let cmd = vec![
        "/bin/sh",
        "-c",
        "timeout 2 /usr/local/bin/nautiloop-introspect 2>/dev/null || echo '{\"processes\":[],\"worktree\":{\"path\":\"/work\",\"target_dir_bytes\":null,\"target_dir_artifacts\":null,\"uncommitted_files\":null,\"head_sha\":null}}'",
    ];

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        pods_api.exec(pod_name, cmd, &attach_params),
    )
    .await;

    match result {
        Ok(Ok(mut attached)) => {
            // 1s read timeout: script output arrives as a single burst after the
            // 2s internal timeout, so 1s is generous for reading buffered output.
            let read_result = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                async {
                    let mut output = String::new();
                    let mut stderr_output = String::new();
                    // Take streams before async blocks to avoid double-borrow of `attached`
                    let mut maybe_stdout = attached.stdout();
                    let mut maybe_stderr = attached.stderr();
                    // Read stdout (main output) and stderr (for debugging) concurrently
                    let stdout_fut = async {
                        if let Some(ref mut stdout) = maybe_stdout {
                            let mut buf = vec![0u8; 65536];
                            loop {
                                match stdout.read(&mut buf).await {
                                    Ok(0) => break,
                                    Ok(n) => output.push_str(&String::from_utf8_lossy(&buf[..n])),
                                    Err(_) => break,
                                }
                            }
                        }
                    };
                    let stderr_fut = async {
                        if let Some(ref mut stderr) = maybe_stderr {
                            let mut buf = vec![0u8; 4096];
                            loop {
                                match stderr.read(&mut buf).await {
                                    Ok(0) => break,
                                    Ok(n) => stderr_output.push_str(&String::from_utf8_lossy(&buf[..n])),
                                    Err(_) => break,
                                }
                            }
                        }
                    };
                    tokio::join!(stdout_fut, stderr_fut);
                    if !stderr_output.is_empty() {
                        tracing::debug!(stderr = %stderr_output, "introspect exec stderr");
                    }
                    output
                },
            )
            .await;
            match read_result {
                Ok(output) => Ok(output),
                Err(_) => Err(ExecError::Timeout("exec read timed out after 1s".to_string())),
            }
        }
        Ok(Err(e)) => Err(ExecError::Other(format!("exec failed: {e}"))),
        Err(_) => Err(ExecError::Timeout("exec handshake timed out after 2s".to_string())),
    }
}

/// Typed representation of the k8s metrics API PodMetrics response.
/// Using a dedicated struct instead of DynamicObject ensures `containers`
/// is deserialized reliably (DynamicObject puts non-standard fields in `.data`
/// which may not capture `containers` correctly).
#[derive(Debug, serde::Deserialize)]
struct PodMetrics {
    containers: Vec<ContainerMetricsEntry>,
}

#[derive(Debug, serde::Deserialize)]
struct ContainerMetricsEntry {
    name: String,
    usage: ContainerUsage,
}

#[derive(Debug, serde::Deserialize)]
struct ContainerUsage {
    cpu: String,
    memory: String,
}

/// Fetch CPU/memory metrics from the k8s metrics API (FR-2b).
/// Returns None if metrics-server is unavailable.
async fn fetch_container_metrics(
    client: &kube::Client,
    pod_name: &str,
    namespace: &str,
) -> Option<ContainerStats> {
    let url = format!(
        "/apis/metrics.k8s.io/v1beta1/namespaces/{namespace}/pods/{pod_name}"
    );

    // kube::Client::request takes http::Request<Vec<u8>>.
    // Import from `http` crate directly rather than through axum's re-export
    // to avoid breakage if axum and kube-rs diverge on http crate versions.
    let request = http::Request::get(&url)
        .body(Vec::new())
        .ok()?;

    let response: Result<PodMetrics, _> = client.request(request).await;

    match response {
        Ok(pod_metrics) => {
            for container in &pod_metrics.containers {
                if container.name == "agent" {
                    return Some(ContainerStats {
                        cpu_millicores: parse_cpu_to_millicores(&container.usage.cpu),
                        memory_bytes: parse_memory_to_bytes(&container.usage.memory),
                    });
                }
            }
            None
        }
        Err(e) => {
            tracing::debug!(error = %e, "metrics API unavailable (metrics-server absent?)");
            None
        }
    }
}

/// Parse Kubernetes CPU quantity to millicores.
pub fn parse_cpu_to_millicores(cpu: &str) -> u64 {
    if let Some(nano) = cpu.strip_suffix('n') {
        // Round instead of truncate so e.g. 999999n → 1m, not 0m
        (nano.parse::<u64>().unwrap_or(0) + 500_000) / 1_000_000
    } else if let Some(micro) = cpu.strip_suffix('u') {
        (micro.parse::<u64>().unwrap_or(0) + 500) / 1_000
    } else if let Some(milli) = cpu.strip_suffix('m') {
        milli.parse::<u64>().unwrap_or(0)
    } else {
        let cores: f64 = cpu.parse().unwrap_or(0.0);
        (cores * 1000.0) as u64
    }
}

/// Parse Kubernetes memory quantity to bytes.
pub fn parse_memory_to_bytes(mem: &str) -> u64 {
    if let Some(ei) = mem.strip_suffix("Ei") {
        ei.parse::<u64>().unwrap_or(0).saturating_mul(1024 * 1024 * 1024 * 1024 * 1024 * 1024)
    } else if let Some(pi) = mem.strip_suffix("Pi") {
        pi.parse::<u64>().unwrap_or(0).saturating_mul(1024 * 1024 * 1024 * 1024 * 1024)
    } else if let Some(ti) = mem.strip_suffix("Ti") {
        ti.parse::<u64>().unwrap_or(0).saturating_mul(1024 * 1024 * 1024 * 1024)
    } else if let Some(gi) = mem.strip_suffix("Gi") {
        gi.parse::<u64>().unwrap_or(0) * 1024 * 1024 * 1024
    } else if let Some(mi) = mem.strip_suffix("Mi") {
        mi.parse::<u64>().unwrap_or(0) * 1024 * 1024
    } else if let Some(ki) = mem.strip_suffix("Ki") {
        ki.parse::<u64>().unwrap_or(0) * 1024
    } else if let Some(g) = mem.strip_suffix('G') {
        g.parse::<u64>().unwrap_or(0) * 1_000_000_000
    } else if let Some(m) = mem.strip_suffix('M') {
        m.parse::<u64>().unwrap_or(0) * 1_000_000
    } else if let Some(k) = mem.strip_suffix('k') {
        k.parse::<u64>().unwrap_or(0) * 1000
    } else {
        mem.parse::<u64>().unwrap_or(0)
    }
}

/// Parse the introspection script JSON output into processes and worktree.
pub fn parse_introspect_output(output: &str) -> (Vec<ProcessInfo>, WorktreeInfo) {
    let parsed: serde_json::Value = match serde_json::from_str(output.trim()) {
        Ok(v) => v,
        Err(_) => return (Vec::new(), default_worktree()),
    };

    let processes = parsed
        .get("processes")
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| {
                    Some(ProcessInfo {
                        pid: v.get("pid")?.as_u64()? as u32,
                        ppid: v.get("ppid")?.as_u64()? as u32,
                        user: v.get("user")?.as_str()?.to_string(),
                        cpu_percent: v.get("cpu_percent")?.as_f64()?,
                        cmd: v.get("cmd")?.as_str()?.to_string(),
                        age_seconds: v.get("age_seconds")?.as_u64()?,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let worktree = parsed
        .get("worktree")
        .map(|w| WorktreeInfo {
            path: w
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("/work")
                .to_string(),
            target_dir_artifacts: w
                .get("target_dir_artifacts")
                .and_then(|v| v.as_u64()),
            target_dir_bytes: w.get("target_dir_bytes").and_then(|v| v.as_u64()),
            uncommitted_files: w
                .get("uncommitted_files")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            head_sha: w
                .get("head_sha")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        })
        .unwrap_or_else(default_worktree);

    (processes, worktree)
}

fn default_worktree() -> WorktreeInfo {
    WorktreeInfo {
        path: "/work".to_string(),
        target_dir_artifacts: None,
        target_dir_bytes: None,
        uncommitted_files: 0,
        head_sha: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::AppState;
    use crate::config::NautiloopConfig;
    use crate::git::mock::MockGitOperations;
    use crate::state::memory::MemoryStateStore;
    use crate::state::StateStore;
    use crate::types::{LoopKind, LoopRecord, LoopState};
    use axum::body::Body;
    use axum::http::{self, Request};
    use std::sync::Arc;
    use tower::ServiceExt;

    fn test_app() -> (axum::Router, Arc<MemoryStateStore>) {
        let store = Arc::new(MemoryStateStore::new());
        let git = Arc::new(MockGitOperations::new());
        let state = AppState {
            store: store.clone(),
            git: git.clone(),
            config: Arc::new(NautiloopConfig::default()),
            kube_client: None,
            pool: None,
        };
        let router = crate::api::build_router_no_auth(state);
        (router, store)
    }

    fn make_loop(state: LoopState) -> LoopRecord {
        LoopRecord {
            id: uuid::Uuid::new_v4(),
            engineer: "alice".to_string(),
            spec_path: "specs/test.md".to_string(),
            spec_content_hash: "abc12345".to_string(),
            branch: format!("agent/alice/test-{}", uuid::Uuid::new_v4()),
            kind: LoopKind::Implement,
            state,
            sub_state: None,
            round: 1,
            max_rounds: 15,
            harden: false,
            harden_only: false,
            auto_approve: true,
            ship_mode: false,
            cancel_requested: false,
            approve_requested: false,
            resume_requested: false,
            paused_from_state: None,
            reauth_from_state: None,
            failed_from_state: None,
            failure_reason: None,
            current_sha: None,
            opencode_session_id: None,
            claude_session_id: None,
            active_job_name: None,
            retry_count: 0,
            model_implementor: None,
            model_reviewer: None,
            merge_sha: None,
            merged_at: None,
            hardened_spec_path: None,
            spec_pr_url: None,
            resolved_default_branch: Some("main".to_string()),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn test_introspect_terminal_loop_returns_410() {
        let (app, store) = test_app();
        let record = make_loop(LoopState::Converged);
        store.create_loop(&record).await.unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/pod-introspect/{}", record.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), http::StatusCode::GONE);
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(response.into_body(), 1024 * 64).await.unwrap()).unwrap();
        assert!(body["error"].as_str().unwrap().contains("nemo inspect"));
    }

    #[tokio::test]
    async fn test_introspect_failed_loop_returns_410() {
        let (app, store) = test_app();
        let record = make_loop(LoopState::Failed);
        store.create_loop(&record).await.unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/pod-introspect/{}", record.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), http::StatusCode::GONE);
    }

    #[tokio::test]
    async fn test_introspect_no_job_returns_425() {
        let (app, store) = test_app();
        // Implementing loop with no active_job_name
        let record = make_loop(LoopState::Implementing);
        store.create_loop(&record).await.unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/pod-introspect/{}", record.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // 425 Too Early — pod not yet running
        assert_eq!(response.status().as_u16(), 425);
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(response.into_body(), 1024 * 64).await.unwrap()).unwrap();
        assert_eq!(body["error"].as_str().unwrap(), "pod not yet running");
    }

    #[tokio::test]
    async fn test_introspect_no_kube_client_returns_500() {
        let (app, store) = test_app();
        let mut record = make_loop(LoopState::Implementing);
        record.active_job_name = Some("nautiloop-job-xyz".to_string());
        store.create_loop(&record).await.unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/pod-introspect/{}", record.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // No kube client configured → Internal Server Error
        assert_eq!(response.status(), http::StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn test_introspect_loop_not_found_returns_404() {
        let (app, _store) = test_app();
        let fake_id = uuid::Uuid::new_v4();

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/pod-introspect/{fake_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), http::StatusCode::NOT_FOUND);
    }

    #[test]
    fn test_parse_cpu_to_millicores() {
        assert_eq!(parse_cpu_to_millicores("500m"), 500);
        assert_eq!(parse_cpu_to_millicores("1"), 1000);
        assert_eq!(parse_cpu_to_millicores("250000000n"), 250);
        assert_eq!(parse_cpu_to_millicores("100000u"), 100);
        assert_eq!(parse_cpu_to_millicores("0"), 0);
        // Rounding: 999999n → 1m (not 0m from truncation)
        assert_eq!(parse_cpu_to_millicores("999999n"), 1);
        // Rounding: 499999n → 0m (below half)
        assert_eq!(parse_cpu_to_millicores("499999n"), 0);
        // Rounding: 500000n → 1m (exactly half rounds up)
        assert_eq!(parse_cpu_to_millicores("500000n"), 1);
        // Microcores use consistent rounding (999u → 1m with rounding)
        assert_eq!(parse_cpu_to_millicores("999u"), 1);
        assert_eq!(parse_cpu_to_millicores("499u"), 0);
        assert_eq!(parse_cpu_to_millicores("500u"), 1);
    }

    #[test]
    fn test_parse_memory_to_bytes() {
        assert_eq!(parse_memory_to_bytes("1024Ki"), 1024 * 1024);
        assert_eq!(parse_memory_to_bytes("512Mi"), 512 * 1024 * 1024);
        assert_eq!(parse_memory_to_bytes("2Gi"), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_memory_to_bytes("1000000"), 1000000);
        // Ti/Pi/Ei binary suffixes
        assert_eq!(parse_memory_to_bytes("1Ti"), 1024 * 1024 * 1024 * 1024);
        assert_eq!(parse_memory_to_bytes("1Pi"), 1024u64 * 1024 * 1024 * 1024 * 1024);
        assert_eq!(parse_memory_to_bytes("1Ei"), 1024u64 * 1024 * 1024 * 1024 * 1024 * 1024);
        // SI suffixes
        assert_eq!(parse_memory_to_bytes("1k"), 1000);
        assert_eq!(parse_memory_to_bytes("1M"), 1_000_000);
        assert_eq!(parse_memory_to_bytes("1G"), 1_000_000_000);
    }

    #[test]
    fn test_pod_metrics_deserialization() {
        // Verify PodMetrics struct correctly deserializes a real metrics API response
        let json = r#"{
            "kind": "PodMetrics",
            "apiVersion": "metrics.k8s.io/v1beta1",
            "metadata": {
                "name": "nautiloop-test-pod",
                "namespace": "nautiloop-jobs",
                "creationTimestamp": "2026-04-17T12:45:00Z"
            },
            "timestamp": "2026-04-17T12:45:00Z",
            "window": "30s",
            "containers": [
                {
                    "name": "agent",
                    "usage": {
                        "cpu": "508234567n",
                        "memory": "937464Ki"
                    }
                },
                {
                    "name": "auth-sidecar",
                    "usage": {
                        "cpu": "1234567n",
                        "memory": "32768Ki"
                    }
                }
            ]
        }"#;

        let metrics: PodMetrics = serde_json::from_str(json).expect("PodMetrics should deserialize");
        assert_eq!(metrics.containers.len(), 2);

        let agent = &metrics.containers[0];
        assert_eq!(agent.name, "agent");
        assert_eq!(agent.usage.cpu, "508234567n");
        assert_eq!(agent.usage.memory, "937464Ki");

        // Verify end-to-end parsing produces expected values
        let cpu = parse_cpu_to_millicores(&agent.usage.cpu);
        let mem = parse_memory_to_bytes(&agent.usage.memory);
        assert_eq!(cpu, 508); // 508234567n → 508m (with rounding)
        assert_eq!(mem, 937464 * 1024); // 937464Ki → bytes
    }

    #[test]
    fn test_parse_introspect_output_valid() {
        let output = r#"{
            "processes": [
                {"pid": 12, "ppid": 1, "user": "agent", "cpu_percent": 3.2, "cmd": "claude", "age_seconds": 1320},
                {"pid": 126, "ppid": 124, "user": "agent", "cpu_percent": 0.0, "cmd": "cargo-clippy clippy --workspace -- -D warnings", "age_seconds": 900}
            ],
            "worktree": {
                "path": "/work",
                "target_dir_bytes": 3221225472,
                "target_dir_artifacts": 1069,
                "uncommitted_files": 2,
                "head_sha": "42bffd9"
            }
        }"#;

        let (processes, worktree) = parse_introspect_output(output);
        assert_eq!(processes.len(), 2);
        assert_eq!(processes[0].pid, 12);
        assert_eq!(processes[0].cmd, "claude");
        assert_eq!(processes[1].pid, 126);
        assert_eq!(worktree.target_dir_artifacts, Some(1069));
        assert_eq!(worktree.target_dir_bytes, Some(3221225472));
        assert_eq!(worktree.uncommitted_files, 2);
        assert_eq!(worktree.head_sha.as_deref(), Some("42bffd9"));
    }

    #[test]
    fn test_parse_introspect_output_empty() {
        let (processes, worktree) = parse_introspect_output("");
        assert!(processes.is_empty());
        assert_eq!(worktree.path, "/work");
    }

    #[test]
    fn test_parse_introspect_output_garbage() {
        let (processes, worktree) = parse_introspect_output("not json at all");
        assert!(processes.is_empty());
        assert_eq!(worktree.path, "/work");
    }

    #[test]
    fn test_parse_introspect_output_partial_worktree() {
        // Matches the fallback JSON emitted when `timeout 2` kills the script:
        // all worktree fields are null so callers render "unavailable" instead of
        // misleading zeros (e.g. "0 artifacts" implying empty target dir).
        let output = r#"{
            "processes": [],
            "worktree": {
                "path": "/work",
                "target_dir_bytes": null,
                "target_dir_artifacts": null,
                "uncommitted_files": null,
                "head_sha": null
            }
        }"#;
        let (processes, worktree) = parse_introspect_output(output);
        assert!(processes.is_empty());
        assert_eq!(worktree.path, "/work");
        assert_eq!(worktree.target_dir_bytes, None);
        assert_eq!(worktree.target_dir_artifacts, None);
        assert_eq!(worktree.uncommitted_files, 0); // null → default 0
        assert_eq!(worktree.head_sha, None);
    }
}
