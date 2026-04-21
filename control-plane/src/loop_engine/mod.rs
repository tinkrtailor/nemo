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
    /// Judge is active with the given model name, via auth-sidecar.
    Enabled { model: String },
    /// Judge is disabled via config.
    Disabled,
}

/// Default sidecar base URL used by the judge in control-plane pods.
/// The auth-sidecar runs as a native sidecar initContainer on localhost:9090,
/// identical to how agent pods access model APIs.
const SIDECAR_BASE_URL: &str = "http://localhost:9090";

/// Build the loop driver, wiring the judge via the auth-sidecar (FR-2b).
///
/// When `judge_enabled = true`, unconditionally constructs a `SidecarJudgeClient`
/// pointing at `http://localhost:9090`. The control-plane pod's auth-sidecar
/// handles credential injection, egress logging, and proxying to Anthropic.
///
/// When `judge_enabled = false`, the judge is omitted entirely.
pub fn build_loop_driver(
    config: &NautiloopConfig,
    store: Arc<dyn StateStore>,
    dispatcher: Arc<dyn JobDispatcher>,
    git: Arc<dyn GitOperations>,
) -> (Arc<ConvergentLoopDriver>, JudgeResolution) {
    build_loop_driver_with(config, store, dispatcher, git, SIDECAR_BASE_URL)
}

/// Inner builder that accepts a sidecar base URL for testability.
pub fn build_loop_driver_with(
    config: &NautiloopConfig,
    store: Arc<dyn StateStore>,
    dispatcher: Arc<dyn JobDispatcher>,
    git: Arc<dyn GitOperations>,
    sidecar_base_url: &str,
) -> (Arc<ConvergentLoopDriver>, JudgeResolution) {
    if !config.orchestrator.judge_enabled {
        return (
            Arc::new(ConvergentLoopDriver::new(
                store,
                dispatcher,
                git,
                config.clone(),
            )),
            JudgeResolution::Disabled,
        );
    }

    let model_client = Arc::new(judge::SidecarJudgeClient::new(sidecar_base_url));
    let driver =
        ConvergentLoopDriver::with_judge(store, dispatcher, git, config.clone(), model_client);
    (
        Arc::new(driver),
        JudgeResolution::Enabled {
            model: config.orchestrator.judge_model.clone(),
        },
    )
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

        let (_, resolution) =
            build_loop_driver_with(&config, store, dispatcher, git, "http://localhost:9090");
        assert_eq!(resolution, JudgeResolution::Disabled);
    }

    #[test]
    fn test_build_driver_judge_enabled_uses_sidecar() {
        let config = test_config(true);
        let store: Arc<dyn StateStore> = Arc::new(MemoryStateStore::new());
        let dispatcher: Arc<dyn JobDispatcher> = Arc::new(MockJobDispatcher::new());
        let git: Arc<dyn GitOperations> = Arc::new(MockGitOperations::new());

        let (_, resolution) =
            build_loop_driver_with(&config, store, dispatcher, git, "http://localhost:9090");
        assert_eq!(
            resolution,
            JudgeResolution::Enabled {
                model: "claude-haiku-4-5".to_string(),
            },
        );
    }
}
