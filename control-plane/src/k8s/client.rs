use async_trait::async_trait;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{Pod, Secret};
use kube::Client;
use kube::api::{Api, DeleteParams, ListParams, PostParams};

use super::{JobDispatcher, JobStatus};
use crate::error::Result;

/// Real K8s job dispatcher using kube-rs.
#[derive(Clone)]
pub struct KubeJobDispatcher {
    client: Client,
    namespace: String,
}

impl KubeJobDispatcher {
    pub fn new(client: Client, namespace: String) -> Self {
        Self { client, namespace }
    }
}

#[async_trait]
impl JobDispatcher for KubeJobDispatcher {
    async fn create_job(&self, job: &Job) -> Result<String> {
        let jobs_api: Api<Job> = Api::namespaced(self.client.clone(), &self.namespace);
        let created = jobs_api.create(&PostParams::default(), job).await?;
        let name = created
            .metadata
            .name
            .unwrap_or_else(|| "unknown".to_string());
        tracing::info!(job_name = %name, "Created K8s Job");
        Ok(name)
    }

    async fn delete_job(&self, name: &str, namespace: &str) -> Result<()> {
        let ns = if namespace.is_empty() {
            &self.namespace
        } else {
            namespace
        };
        let jobs_api: Api<Job> = Api::namespaced(self.client.clone(), ns);
        let dp = DeleteParams {
            propagation_policy: Some(kube::api::PropagationPolicy::Background),
            ..Default::default()
        };
        match jobs_api.delete(name, &dp).await {
            Ok(_) => {
                tracing::info!(job_name = %name, "Deleted K8s Job");
                Ok(())
            }
            Err(kube::Error::Api(err)) if err.code == 404 => {
                tracing::warn!(job_name = %name, "Job not found during delete (already cleaned up)");
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn get_job_status(&self, name: &str, namespace: &str) -> Result<JobStatus> {
        let ns = if namespace.is_empty() {
            &self.namespace
        } else {
            namespace
        };
        let jobs_api: Api<Job> = Api::namespaced(self.client.clone(), ns);
        let job = match jobs_api.get(name).await {
            Ok(job) => job,
            Err(kube::Error::Api(err)) if err.code == 404 => return Ok(JobStatus::NotFound),
            Err(e) => return Err(e.into()),
        };

        let mut status = job_to_status(&job);

        // For failed jobs, inspect pod exit codes for auth expiry (exit code 42)
        if matches!(status, JobStatus::Failed { .. }) {
            let pods_api: Api<Pod> = Api::namespaced(self.client.clone(), ns);
            let lp = ListParams::default().labels(&format!("job-name={name}"));
            if let Ok(pod_list) = pods_api.list(&lp).await {
                for pod in &pod_list.items {
                    if let Some(exit_code) = extract_exit_code(pod)
                        && exit_code == 42
                    {
                        let reason = match &status {
                            JobStatus::Failed { reason } => reason.clone(),
                            _ => "Auth expired (exit code 42)".to_string(),
                        };
                        status = JobStatus::AuthExpired { reason };
                        break;
                    }
                }
            }
        }

        Ok(status)
    }

    async fn get_job(&self, name: &str, namespace: &str) -> Result<Option<Job>> {
        let ns = if namespace.is_empty() {
            &self.namespace
        } else {
            namespace
        };
        let jobs_api: Api<Job> = Api::namespaced(self.client.clone(), ns);
        match jobs_api.get(name).await {
            Ok(job) => Ok(Some(job)),
            Err(kube::Error::Api(err)) if err.code == 404 => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn get_job_logs(&self, name: &str, namespace: &str) -> Result<String> {
        let ns = if namespace.is_empty() {
            &self.namespace
        } else {
            namespace
        };
        // Find pods for this job, then get logs from the "agent" container
        let pods_api: Api<Pod> = Api::namespaced(self.client.clone(), ns);
        let lp = ListParams::default().labels(&format!("job-name={name}"));
        let pod_list = pods_api.list(&lp).await?;

        for pod in &pod_list.items {
            if let Some(pod_name) = &pod.metadata.name {
                // Fetch the full log so the driver can reconcile without
                // silently dropping lines if a job emits more than a fixed tail.
                let log_params = kube::api::LogParams {
                    container: Some("agent".to_string()),
                    ..Default::default()
                };
                match pods_api.logs(pod_name, &log_params).await {
                    Ok(logs) => return Ok(logs),
                    Err(e) => {
                        tracing::warn!(pod = %pod_name, error = %e, "Failed to get pod logs");
                    }
                }
            }
        }

        if pod_list.items.is_empty() {
            // No pods found — job may have been cleaned up already
            Ok(String::new())
        } else {
            // Pods exist but all log retrievals failed
            Err(crate::error::NautiloopError::Internal(format!(
                "Failed to retrieve logs from any pod for job {name}"
            )))
        }
    }

    async fn get_secret_key(
        &self,
        name: &str,
        namespace: &str,
        key: &str,
    ) -> Result<Option<Vec<u8>>> {
        let ns = if namespace.is_empty() {
            &self.namespace
        } else {
            namespace
        };
        let secrets_api: Api<Secret> = Api::namespaced(self.client.clone(), ns);
        // Force a fresh read from the API server (not the kubelet-
        // style informer cache). The preflight only works if we see
        // the same bytes `nemo auth` just wrote, even if that write
        // happened a few seconds ago. See issue #98.
        match secrets_api.get(name).await {
            Ok(secret) => {
                let bytes = secret
                    .data
                    .as_ref()
                    .and_then(|d| d.get(key))
                    .map(|bs| bs.0.clone());
                Ok(bytes)
            }
            Err(kube::Error::Api(err)) if err.code == 404 => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

/// Extract the exit code from the pod's `agent` container specifically.
/// In a multi-container pod we must not mis-tag from the sidecar container.
fn extract_exit_code(pod: &Pod) -> Option<i32> {
    let status = pod.status.as_ref()?;
    for cs in status.container_statuses.as_ref()? {
        if cs.name == "agent"
            && let Some(ref terminated) = cs.state.as_ref()?.terminated
        {
            return Some(terminated.exit_code);
        }
    }
    None
}

/// Extract job status from K8s Job resource.
fn job_to_status(job: &Job) -> JobStatus {
    let status = match &job.status {
        Some(s) => s,
        None => return JobStatus::Pending,
    };

    // Check for completion conditions
    if let Some(conditions) = &status.conditions {
        for condition in conditions {
            if condition.type_ == "Complete" && condition.status == "True" {
                return JobStatus::Succeeded;
            }
            if condition.type_ == "Failed" && condition.status == "True" {
                // Include both reason and message for auth error detection
                let reason = match (&condition.reason, &condition.message) {
                    (Some(r), Some(m)) => format!("{r}: {m}"),
                    (Some(r), None) => r.clone(),
                    (None, Some(m)) => m.clone(),
                    (None, None) => "Unknown failure".to_string(),
                };
                return JobStatus::Failed { reason };
            }
        }
    }

    // Check active pods
    if status.active.unwrap_or(0) > 0 {
        return JobStatus::Running;
    }

    // Check succeeded/failed counts
    if status.succeeded.unwrap_or(0) > 0 {
        return JobStatus::Succeeded;
    }
    if status.failed.unwrap_or(0) > 0 {
        // Extract failure details from conditions or use exit-code convention
        let reason = status
            .conditions
            .as_ref()
            .and_then(|conds| {
                conds.iter().find_map(|c| {
                    if c.type_ == "Failed" {
                        c.message.clone().or(c.reason.clone())
                    } else {
                        None
                    }
                })
            })
            .unwrap_or_else(|| "Pod failure (check pod logs for details)".to_string());
        return JobStatus::Failed { reason };
    }

    JobStatus::Pending
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::batch::v1::{JobCondition, JobStatus as K8sJobStatus};

    fn make_job(status: Option<K8sJobStatus>) -> Job {
        Job {
            status,
            ..Default::default()
        }
    }

    #[test]
    fn test_job_to_status_no_status() {
        let job = make_job(None);
        assert_eq!(job_to_status(&job), JobStatus::Pending);
    }

    #[test]
    fn test_job_to_status_running() {
        let job = make_job(Some(K8sJobStatus {
            active: Some(1),
            ..Default::default()
        }));
        assert_eq!(job_to_status(&job), JobStatus::Running);
    }

    #[test]
    fn test_job_to_status_succeeded_via_condition() {
        let job = make_job(Some(K8sJobStatus {
            conditions: Some(vec![JobCondition {
                type_: "Complete".to_string(),
                status: "True".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        }));
        assert_eq!(job_to_status(&job), JobStatus::Succeeded);
    }

    #[test]
    fn test_job_to_status_failed_via_condition() {
        let job = make_job(Some(K8sJobStatus {
            conditions: Some(vec![JobCondition {
                type_: "Failed".to_string(),
                status: "True".to_string(),
                reason: Some("BackoffLimitExceeded".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        }));
        assert_eq!(
            job_to_status(&job),
            JobStatus::Failed {
                reason: "BackoffLimitExceeded".to_string()
            }
        );
    }

    #[test]
    fn test_job_to_status_succeeded_via_count() {
        let job = make_job(Some(K8sJobStatus {
            succeeded: Some(1),
            ..Default::default()
        }));
        assert_eq!(job_to_status(&job), JobStatus::Succeeded);
    }
}
