use async_trait::async_trait;
use k8s_openapi::api::batch::v1::Job;
use kube::api::{Api, DeleteParams, PostParams};
use kube::Client;

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
        match jobs_api.get(name).await {
            Ok(job) => Ok(job_to_status(&job)),
            Err(kube::Error::Api(err)) if err.code == 404 => Ok(JobStatus::NotFound),
            Err(e) => Err(e.into()),
        }
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
                let reason = condition
                    .reason
                    .clone()
                    .unwrap_or_else(|| "Unknown failure".to_string());
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
