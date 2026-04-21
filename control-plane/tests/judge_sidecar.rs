//! Integration test: loop engine with a mock sidecar responding to
//! `/anthropic/v1/messages` — verifying judge invocations route through
//! the sidecar endpoint (NFR-5).

use std::sync::Arc;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use nautiloop_control_plane::config::{NautiloopConfig, OrchestratorConfig};
use nautiloop_control_plane::git::mock::MockGitOperations;
use nautiloop_control_plane::k8s::mock::MockJobDispatcher;
use nautiloop_control_plane::loop_engine::{self, JudgeResolution};
use nautiloop_control_plane::state::memory::MemoryStateStore;

/// Verify that `build_loop_driver_with` constructs a `SidecarJudgeClient`
/// that routes requests to the sidecar's `/anthropic/v1/messages` endpoint.
#[tokio::test]
async fn test_build_driver_uses_sidecar_endpoint() {
    let config = NautiloopConfig {
        orchestrator: OrchestratorConfig {
            judge_enabled: true,
            judge_model: "claude-haiku-4-5".to_string(),
            max_judge_calls: 10,
        },
        ..NautiloopConfig::default()
    };

    let store = Arc::new(MemoryStateStore::new());
    let dispatcher = Arc::new(MockJobDispatcher::new());
    let git = Arc::new(MockGitOperations::new());

    // Use a mock server as the sidecar
    let mock_sidecar = MockServer::start().await;

    let (_, resolution) =
        loop_engine::build_loop_driver_with(&config, store, dispatcher, git, &mock_sidecar.uri());

    assert_eq!(
        resolution,
        JudgeResolution::Enabled {
            model: "claude-haiku-4-5".to_string(),
        },
    );
}

/// Verify that when judge_enabled = false, no sidecar client is constructed.
#[tokio::test]
async fn test_build_driver_disabled_skips_sidecar() {
    let config = NautiloopConfig {
        orchestrator: OrchestratorConfig {
            judge_enabled: false,
            judge_model: "claude-haiku-4-5".to_string(),
            max_judge_calls: 10,
        },
        ..NautiloopConfig::default()
    };

    let store = Arc::new(MemoryStateStore::new());
    let dispatcher = Arc::new(MockJobDispatcher::new());
    let git = Arc::new(MockGitOperations::new());

    let (_, resolution) = loop_engine::build_loop_driver_with(
        &config,
        store,
        dispatcher,
        git,
        "http://localhost:9090",
    );

    assert_eq!(resolution, JudgeResolution::Disabled);
}

/// End-to-end: SidecarJudgeClient routes through mock sidecar at
/// `/anthropic/v1/messages` and returns a valid judge response.
#[tokio::test]
async fn test_sidecar_judge_client_routes_through_sidecar() {
    use nautiloop_control_plane::loop_engine::judge::{JudgeModelClient, SidecarJudgeClient};

    let mock_sidecar = MockServer::start().await;

    let anthropic_response = serde_json::json!({
        "id": "msg_sidecar_test",
        "type": "message",
        "role": "assistant",
        "content": [
            {
                "type": "text",
                "text": "{\"decision\": \"exit_clean\", \"confidence\": 0.95, \"reasoning\": \"All clear\", \"hint\": null}"
            }
        ],
        "model": "claude-haiku-4-5-20251001",
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 100, "output_tokens": 30}
    });

    Mock::given(method("POST"))
        .and(path("/anthropic/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&anthropic_response))
        .expect(1)
        .mount(&mock_sidecar)
        .await;

    let client = SidecarJudgeClient::new(&mock_sidecar.uri());
    let result = client.invoke("claude-haiku-4-5", "test prompt").await;

    assert!(result.is_ok());
    let text = result.unwrap();
    assert!(text.contains("exit_clean"));
    assert!(text.contains("All clear"));
}
