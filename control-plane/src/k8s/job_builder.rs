use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::batch::v1::JobSpec;
use k8s_openapi::api::core::v1::{Container, EnvVar, PodSpec, PodTemplateSpec, VolumeMount, Volume, PersistentVolumeClaimVolumeSource};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use std::collections::BTreeMap;
use uuid::Uuid;

use crate::types::{LoopContext, StageConfig};

/// Build a K8s Job spec for a given stage.
///
/// Job naming: `nemo-{short-loop-id}-{stage}-r{round}`
/// Labels: `app=nemo`, `nemo.dev/loop-id`, `nemo.dev/stage`, `nemo.dev/round`, `nemo.dev/engineer`
pub fn build_job(
    ctx: &LoopContext,
    stage: &StageConfig,
    namespace: &str,
    agent_image: &str,
    bare_repo_pvc: &str,
) -> Job {
    let short_id = &ctx.loop_id.to_string()[..8];
    // Include retry count in name to avoid AlreadyExists on redispatch
    let job_name = if ctx.retry_count > 0 {
        format!("nemo-{short_id}-{}-r{}-t{}", stage.name, ctx.round, ctx.retry_count)
    } else {
        format!("nemo-{short_id}-{}-r{}", stage.name, ctx.round)
    };

    let mut labels = BTreeMap::new();
    labels.insert("app".to_string(), "nemo".to_string());
    labels.insert("nemo.dev/loop-id".to_string(), ctx.loop_id.to_string());
    labels.insert("nemo.dev/stage".to_string(), stage.name.clone());
    labels.insert("nemo.dev/round".to_string(), ctx.round.to_string());
    labels.insert("nemo.dev/engineer".to_string(), ctx.engineer.clone());

    let mut env_vars = vec![
        EnvVar {
            name: "NEMO_LOOP_ID".to_string(),
            value: Some(ctx.loop_id.to_string()),
            ..Default::default()
        },
        EnvVar {
            name: "NEMO_STAGE".to_string(),
            value: Some(stage.name.clone()),
            ..Default::default()
        },
        EnvVar {
            name: "NEMO_ROUND".to_string(),
            value: Some(ctx.round.to_string()),
            ..Default::default()
        },
        EnvVar {
            name: "NEMO_SPEC_PATH".to_string(),
            value: Some(ctx.spec_path.clone()),
            ..Default::default()
        },
        EnvVar {
            name: "NEMO_BRANCH".to_string(),
            value: Some(ctx.branch.clone()),
            ..Default::default()
        },
        EnvVar {
            name: "NEMO_ENGINEER".to_string(),
            value: Some(ctx.engineer.clone()),
            ..Default::default()
        },
    ];

    if let Some(sha) = &ctx.current_sha.is_empty().then_some(()).or(None) {
        let _ = sha; // current_sha handled below
    }
    if !ctx.current_sha.is_empty() {
        env_vars.push(EnvVar {
            name: "NEMO_CURRENT_SHA".to_string(),
            value: Some(ctx.current_sha.clone()),
            ..Default::default()
        });
    }

    if let Some(ref feedback) = ctx.feedback_path {
        env_vars.push(EnvVar {
            name: "NEMO_FEEDBACK_PATH".to_string(),
            value: Some(feedback.clone()),
            ..Default::default()
        });
    }

    if let Some(ref session_id) = ctx.session_id {
        env_vars.push(EnvVar {
            name: "NEMO_SESSION_ID".to_string(),
            value: Some(session_id.clone()),
            ..Default::default()
        });
    }

    if let Some(ref model) = stage.model {
        env_vars.push(EnvVar {
            name: "NEMO_MODEL".to_string(),
            value: Some(model.clone()),
            ..Default::default()
        });
    }

    // Inject credentials into the pod so agents can authenticate with model APIs
    for (provider, cred_ref) in &ctx.credentials {
        // Sanitize provider to valid env var chars (uppercase alphanumeric + underscore)
        let safe_provider: String = provider
            .to_uppercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let env_name = format!("NEMO_CRED_{safe_provider}");
        env_vars.push(EnvVar {
            name: env_name,
            value: Some(cred_ref.clone()),
            ..Default::default()
        });
    }

    if let Some(ref prompt) = stage.prompt_template {
        env_vars.push(EnvVar {
            name: "NEMO_PROMPT_TEMPLATE".to_string(),
            value: Some(prompt.clone()),
            ..Default::default()
        });
    }

    let timeout_secs = stage.timeout.as_secs();
    let active_deadline = if timeout_secs > 0 {
        Some(timeout_secs as i64)
    } else {
        None
    };

    Job {
        metadata: ObjectMeta {
            name: Some(job_name),
            namespace: Some(namespace.to_string()),
            labels: Some(labels.clone()),
            ..Default::default()
        },
        spec: Some(JobSpec {
            active_deadline_seconds: active_deadline,
            backoff_limit: Some(0), // No K8s-level retries; we handle retries in the loop engine
            ttl_seconds_after_finished: Some(300), // K8s auto-cleans completed Jobs after 5 min
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    restart_policy: Some("Never".to_string()),
                    containers: vec![Container {
                        name: format!("nemo-{}", stage.name),
                        image: Some(agent_image.to_string()),
                        env: Some(env_vars),
                        volume_mounts: Some(vec![VolumeMount {
                            name: "bare-repo".to_string(),
                            mount_path: "/repo".to_string(),
                            read_only: Some(false),
                            ..Default::default()
                        }]),
                        ..Default::default()
                    }],
                    volumes: Some(vec![Volume {
                        name: "bare-repo".to_string(),
                        persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                            claim_name: bare_repo_pvc.to_string(),
                            read_only: Some(false),
                        }),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Generate a unique job name for a loop stage.
pub fn job_name(loop_id: Uuid, stage: &str, round: u32) -> String {
    let short_id = &loop_id.to_string()[..8];
    format!("nemo-{short_id}-{stage}-r{round}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn test_ctx() -> LoopContext {
        LoopContext {
            loop_id: Uuid::parse_str("a1b2c3d4-e5f6-7890-abcd-ef1234567890").unwrap(),
            engineer: "alice".to_string(),
            spec_path: "specs/feature/invoice-cancel.md".to_string(),
            branch: "agent/alice/invoice-cancel-a1b2c3d4".to_string(),
            current_sha: "abc123".to_string(),
            round: 2,
            max_rounds: 15,
            retry_count: 0,
            session_id: Some("session-123".to_string()),
            feedback_path: Some(".agent/review-feedback-round-1.json".to_string()),
            credentials: vec![],
        }
    }

    fn test_stage() -> StageConfig {
        StageConfig {
            name: "implement".to_string(),
            model: Some("claude-opus-4".to_string()),
            prompt_template: Some(".nemo/prompts/implement.md".to_string()),
            timeout: Duration::from_secs(1800),
            max_retries: 2,
        }
    }

    #[test]
    fn test_build_job_name() {
        let ctx = test_ctx();
        let stage = test_stage();
        let job = build_job(&ctx, &stage, "nemo-jobs", "nemo-agent:latest", "bare-repo-pvc");
        let name = job.metadata.name.unwrap();
        assert!(name.starts_with("nemo-a1b2c3d4-implement-r2"));
    }

    #[test]
    fn test_build_job_labels() {
        let ctx = test_ctx();
        let stage = test_stage();
        let job = build_job(&ctx, &stage, "nemo-jobs", "nemo-agent:latest", "bare-repo-pvc");
        let labels = job.metadata.labels.unwrap();
        assert_eq!(labels["app"], "nemo");
        assert_eq!(labels["nemo.dev/stage"], "implement");
        assert_eq!(labels["nemo.dev/round"], "2");
        assert_eq!(labels["nemo.dev/engineer"], "alice");
    }

    #[test]
    fn test_build_job_env_vars() {
        let ctx = test_ctx();
        let stage = test_stage();
        let job = build_job(&ctx, &stage, "nemo-jobs", "nemo-agent:latest", "bare-repo-pvc");
        let containers = &job.spec.unwrap().template.spec.unwrap().containers;
        let env = containers[0].env.as_ref().unwrap();

        let find_env = |name: &str| -> Option<String> {
            env.iter()
                .find(|e| e.name == name)
                .and_then(|e| e.value.clone())
        };

        assert_eq!(find_env("NEMO_LOOP_ID").unwrap(), ctx.loop_id.to_string());
        assert_eq!(find_env("NEMO_STAGE").unwrap(), "implement");
        assert_eq!(find_env("NEMO_ROUND").unwrap(), "2");
        assert_eq!(find_env("NEMO_FEEDBACK_PATH").unwrap(), ".agent/review-feedback-round-1.json");
        assert_eq!(find_env("NEMO_SESSION_ID").unwrap(), "session-123");
        assert_eq!(find_env("NEMO_MODEL").unwrap(), "claude-opus-4");
    }

    #[test]
    fn test_build_job_timeout() {
        let ctx = test_ctx();
        let stage = test_stage();
        let job = build_job(&ctx, &stage, "nemo-jobs", "nemo-agent:latest", "bare-repo-pvc");
        let spec = job.spec.unwrap();
        assert_eq!(spec.active_deadline_seconds, Some(1800));
    }

    #[test]
    fn test_job_name_generation() {
        let id = Uuid::parse_str("a1b2c3d4-e5f6-7890-abcd-ef1234567890").unwrap();
        assert_eq!(job_name(id, "implement", 2), "nemo-a1b2c3d4-implement-r2");
        assert_eq!(job_name(id, "review", 1), "nemo-a1b2c3d4-review-r1");
    }
}
