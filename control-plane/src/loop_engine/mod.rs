pub mod driver;
pub mod judge;
pub mod reconciler;
pub mod watcher;

pub use driver::ConvergentLoopDriver;
pub use judge::OrchestratorJudge;
pub use reconciler::Reconciler;

use std::sync::Arc;

use crate::config::NautiloopConfig;
use crate::git::GitOperations;
use crate::k8s::JobDispatcher;
use crate::state::StateStore;

/// Describes how the judge was resolved during driver construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JudgeResolution {
    /// Judge is active with the given model name.
    Enabled { model: String },
    /// Judge is enabled in config but credentials are missing.
    CredentialsMissing,
    /// Judge is disabled via config.
    Disabled,
}

/// Build the loop driver, resolving judge configuration from config and credentials.
///
/// Returns the constructed driver and how the judge was resolved (for logging).
/// This function is extracted from main.rs for testability (NFR-4).
pub fn build_loop_driver(
    config: &NautiloopConfig,
    store: Arc<dyn StateStore>,
    dispatcher: Arc<dyn JobDispatcher>,
    git: Arc<dyn GitOperations>,
) -> (Arc<ConvergentLoopDriver>, JudgeResolution) {
    build_loop_driver_with(config, store, dispatcher, git, |path| {
        judge::resolve_judge_api_key(path)
    })
}

/// Inner builder that accepts a credential resolver for testability.
pub fn build_loop_driver_with<F>(
    config: &NautiloopConfig,
    store: Arc<dyn StateStore>,
    dispatcher: Arc<dyn JobDispatcher>,
    git: Arc<dyn GitOperations>,
    resolve_creds: F,
) -> (Arc<ConvergentLoopDriver>, JudgeResolution)
where
    F: FnOnce(&str) -> Option<String>,
{
    if !config.orchestrator.judge_enabled {
        return (
            Arc::new(ConvergentLoopDriver::new(store, dispatcher, git, config.clone())),
            JudgeResolution::Disabled,
        );
    }

    match resolve_creds(&config.orchestrator.judge_credentials_path) {
        Some(api_key) => {
            let model_client = Arc::new(judge::DirectAnthropicClient::new(api_key));
            let driver = ConvergentLoopDriver::with_judge(
                store,
                dispatcher,
                git,
                config.clone(),
                model_client,
            );
            (
                Arc::new(driver),
                JudgeResolution::Enabled {
                    model: config.orchestrator.judge_model.clone(),
                },
            )
        }
        None => (
            Arc::new(ConvergentLoopDriver::new(store, dispatcher, git, config.clone())),
            JudgeResolution::CredentialsMissing,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OrchestratorConfig;
    use crate::git::mock::MockGitOperations;
    use crate::k8s::mock::MockJobDispatcher;
    use crate::state::memory::MemoryStateStore;

    use std::sync::Arc;

    fn test_config(judge_enabled: bool) -> NautiloopConfig {
        NautiloopConfig {
            orchestrator: OrchestratorConfig {
                judge_enabled,
                judge_model: "claude-haiku-4-5".to_string(),
                max_judge_calls: 10,
                judge_credentials_path: "/secrets/judge/credentials.json".to_string(),
            },
            ..NautiloopConfig::default()
        }
    }

    #[test]
    fn test_build_driver_judge_disabled() {
        let config = test_config(false);
        let store: Arc<dyn StateStore> = Arc::new(MemoryStateStore::new());
        let dispatcher: Arc<dyn JobDispatcher> = Arc::new(MockJobDispatcher::new());
        let git: Arc<dyn GitOperations> = Arc::new(MockGitOperations::new());

        let (_, resolution) = build_loop_driver_with(
            &config, store, dispatcher, git,
            |_| panic!("should not resolve creds when judge disabled"),
        );
        assert_eq!(resolution, JudgeResolution::Disabled);
    }

    #[test]
    fn test_build_driver_judge_enabled_with_creds() {
        let config = test_config(true);
        let store: Arc<dyn StateStore> = Arc::new(MemoryStateStore::new());
        let dispatcher: Arc<dyn JobDispatcher> = Arc::new(MockJobDispatcher::new());
        let git: Arc<dyn GitOperations> = Arc::new(MockGitOperations::new());

        let (_, resolution) = build_loop_driver_with(
            &config, store, dispatcher, git,
            |_| Some("sk-ant-test-key".to_string()),
        );
        assert_eq!(
            resolution,
            JudgeResolution::Enabled {
                model: "claude-haiku-4-5".to_string(),
            },
        );
    }

    #[test]
    fn test_build_driver_judge_enabled_no_creds() {
        let config = test_config(true);
        let store: Arc<dyn StateStore> = Arc::new(MemoryStateStore::new());
        let dispatcher: Arc<dyn JobDispatcher> = Arc::new(MockJobDispatcher::new());
        let git: Arc<dyn GitOperations> = Arc::new(MockGitOperations::new());

        let (_, resolution) = build_loop_driver_with(
            &config, store, dispatcher, git,
            |_| None,
        );
        assert_eq!(resolution, JudgeResolution::CredentialsMissing);
    }
}
