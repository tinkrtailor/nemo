pub mod client;
pub mod job_builder;

use async_trait::async_trait;
use k8s_openapi::api::batch::v1::Job;

use crate::error::Result;

/// Status of a K8s Job as observed by the control plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobStatus {
    /// Job exists but no pods are active yet.
    Pending,
    /// At least one pod is running.
    Running,
    /// Job completed successfully (all pods succeeded).
    Succeeded,
    /// Job failed (backoff limit reached or pod failure).
    Failed { reason: String },
    /// Job failed with exit code 42 (auth/credential expiry convention).
    AuthExpired { reason: String },
    /// Job not found.
    NotFound,
}

/// Trait abstracting K8s Job operations for testability.
#[async_trait]
pub trait JobDispatcher: Send + Sync + 'static {
    /// Create a K8s Job and return its name.
    async fn create_job(&self, job: &Job) -> Result<String>;

    /// Delete a K8s Job by name.
    async fn delete_job(&self, name: &str, namespace: &str) -> Result<()>;

    /// Get the status of a K8s Job by name.
    async fn get_job_status(&self, name: &str, namespace: &str) -> Result<JobStatus>;

    /// Get a K8s Job by name.
    async fn get_job(&self, name: &str, namespace: &str) -> Result<Option<Job>>;

    /// Get logs from the agent container of a job's pod.
    /// Used to extract NAUTILOOP_RESULT: lines from completed jobs.
    async fn get_job_logs(&self, name: &str, namespace: &str) -> Result<String>;
}

/// In-memory mock job dispatcher for testing.
pub mod mock {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    #[derive(Debug, Clone)]
    pub struct MockJobDispatcher {
        jobs: Arc<RwLock<HashMap<String, (Job, JobStatus)>>>,
        logs: Arc<RwLock<HashMap<String, String>>>,
    }

    impl MockJobDispatcher {
        pub fn new() -> Self {
            Self {
                jobs: Arc::new(RwLock::new(HashMap::new())),
                logs: Arc::new(RwLock::new(HashMap::new())),
            }
        }

        /// Set mock logs for a job (for testing NAUTILOOP_RESULT extraction).
        pub async fn set_job_logs(&self, name: &str, logs: &str) {
            let mut log_map = self.logs.write().await;
            log_map.insert(name.to_string(), logs.to_string());
        }

        /// Set the status of a job (for test control).
        pub async fn set_job_status(&self, name: &str, status: JobStatus) {
            let mut jobs = self.jobs.write().await;
            if let Some(entry) = jobs.get_mut(name) {
                entry.1 = status;
            }
        }

        /// Get all created job names.
        pub async fn created_jobs(&self) -> Vec<String> {
            let jobs = self.jobs.read().await;
            jobs.keys().cloned().collect()
        }
    }

    impl Default for MockJobDispatcher {
        fn default() -> Self {
            Self::new()
        }
    }

    #[async_trait]
    impl JobDispatcher for MockJobDispatcher {
        async fn create_job(&self, job: &Job) -> Result<String> {
            let name = job
                .metadata
                .name
                .clone()
                .unwrap_or_else(|| "unknown".to_string());
            let mut jobs = self.jobs.write().await;
            jobs.insert(name.clone(), (job.clone(), JobStatus::Pending));
            Ok(name)
        }

        async fn delete_job(&self, name: &str, _namespace: &str) -> Result<()> {
            let mut jobs = self.jobs.write().await;
            jobs.remove(name);
            Ok(())
        }

        async fn get_job_status(&self, name: &str, _namespace: &str) -> Result<JobStatus> {
            let jobs = self.jobs.read().await;
            Ok(jobs
                .get(name)
                .map(|(_, status)| status.clone())
                .unwrap_or(JobStatus::NotFound))
        }

        async fn get_job(&self, name: &str, _namespace: &str) -> Result<Option<Job>> {
            let jobs = self.jobs.read().await;
            Ok(jobs.get(name).map(|(job, _)| job.clone()))
        }

        async fn get_job_logs(&self, name: &str, _namespace: &str) -> Result<String> {
            let logs = self.logs.read().await;
            Ok(logs.get(name).cloned().unwrap_or_default())
        }
    }
}
