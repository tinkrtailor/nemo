use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::batch::v1::JobSpec;
use k8s_openapi::api::core::v1::{
    Capabilities, ConfigMapVolumeSource, Container, EmptyDirVolumeSource, EnvVar, HTTPGetAction,
    KeyToPath, LocalObjectReference, PersistentVolumeClaimVolumeSource, PodSpec, PodTemplateSpec,
    Probe, ResourceRequirements, SecretVolumeSource, SecurityContext, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use std::collections::BTreeMap;
use uuid::Uuid;

use crate::types::{LoopContext, Stage, StageConfig};

/// Configuration for building a K8s Job, encapsulating all cluster-level settings.
#[derive(Debug, Clone)]
pub struct JobBuildConfig {
    pub namespace: String,
    pub agent_image: String,
    pub sidecar_image: String,
    pub bare_repo_pvc: String,
    pub sessions_pvc: String,
    pub image_pull_secret: Option<String>,
    /// Git repository URL passed to the sidecar for SSH proxy host restriction (FR-18).
    pub git_repo_url: String,
    /// ConfigMap name containing SSH known_hosts for sidecar host key verification.
    pub ssh_known_hosts_configmap: String,
}

/// Build a K8s Job spec for a given stage.
///
/// The Job contains:
/// - init container: `init-iptables` for network egress enforcement (FR-41a)
/// - agent container: runs the agent entrypoint
/// - sidecar container: `auth-sidecar` for model API proxy, git SSH proxy, egress logging
///
/// Job naming: `nemo-{loop_id_short}-{stage}-r{round}-t{attempt}` (FR-31)
/// Labels: `nemo.dev/loop-id`, `nemo.dev/stage`, `nemo.dev/engineer`, `nemo.dev/round` (FR-32)
pub fn build_job(ctx: &LoopContext, stage: &StageConfig, cfg: &JobBuildConfig) -> Job {
    let short_id = &ctx.loop_id.to_string()[..8];
    // FR-31: Include attempt number in name to avoid AlreadyExists on redispatch.
    // Attempt = retry_count + 1 (first dispatch is attempt 1, first retry is attempt 2).
    let attempt = ctx.retry_count + 1;
    let job_name = format!("nemo-{short_id}-{}-r{}-t{attempt}", stage.name, ctx.round);

    // FR-32: Labels for control plane queries
    let mut labels = BTreeMap::new();
    labels.insert("app".to_string(), "nemo".to_string());
    labels.insert("nemo.dev/loop-id".to_string(), ctx.loop_id.to_string());
    labels.insert("nemo.dev/stage".to_string(), stage.name.clone());
    labels.insert("nemo.dev/round".to_string(), ctx.round.to_string());
    labels.insert("nemo.dev/engineer".to_string(), ctx.engineer.clone());

    let parsed_stage = Stage::from_short_name(&stage.name);
    let is_review_or_audit = matches!(parsed_stage, Some(Stage::Review) | Some(Stage::Audit));
    let is_test = matches!(parsed_stage, Some(Stage::Test));

    // FR-27: Environment variables on the agent container
    let agent_env = build_agent_env_vars(ctx, stage, is_test);

    // Build volumes (FR-25, FR-26, FR-30)
    let volumes = build_volumes(
        &cfg.bare_repo_pvc,
        &cfg.sessions_pvc,
        &ctx.engineer,
        &cfg.ssh_known_hosts_configmap,
    );

    // Build agent container volume mounts (with subPath for worktree isolation)
    let agent_mounts = build_agent_mounts(is_review_or_audit, &ctx.worktree_path);

    // Build sidecar container volume mounts
    let sidecar_mounts = build_sidecar_mounts();

    // FR-28: Resource limits per stage (with JVM tag support for TEST)
    let has_jvm_tag = ctx
        .credentials
        .iter()
        .any(|(k, v)| k == "service_tags" && v.contains("jvm"));
    let (agent_resources, sidecar_resources) = resource_limits(&stage.name, is_test, has_jvm_tag);

    // FR-25: Agent security context
    let agent_security_ctx = SecurityContext {
        run_as_non_root: Some(true),
        run_as_user: Some(1000),
        read_only_root_filesystem: Some(true),
        ..Default::default()
    };

    // Sidecar security context (runs as nobody/65534)
    let sidecar_security_ctx = SecurityContext {
        run_as_non_root: Some(true),
        run_as_user: Some(65534),
        read_only_root_filesystem: Some(true),
        ..Default::default()
    };

    // FR-22: Sidecar readiness/liveness probes
    let readiness_probe = Probe {
        http_get: Some(HTTPGetAction {
            path: Some("/healthz".to_string()),
            port: IntOrString::Int(9093),
            ..Default::default()
        }),
        initial_delay_seconds: Some(1),
        period_seconds: Some(5),
        ..Default::default()
    };

    let liveness_probe = Probe {
        http_get: Some(HTTPGetAction {
            path: Some("/healthz".to_string()),
            port: IntOrString::Int(9093),
            ..Default::default()
        }),
        initial_delay_seconds: Some(5),
        period_seconds: Some(10),
        ..Default::default()
    };

    // Sidecar env vars
    let sidecar_env = vec![
        env_var("GIT_REPO_URL", &cfg.git_repo_url), // Used by sidecar to know allowed git remote (FR-18)
    ];

    // Agent container
    let agent_container = Container {
        name: "agent".to_string(),
        image: Some(cfg.agent_image.clone()),
        env: Some(agent_env),
        volume_mounts: Some(agent_mounts),
        resources: Some(agent_resources),
        security_context: Some(agent_security_ctx),
        ..Default::default()
    };

    // FR-24: Auth sidecar container
    let sidecar_container = Container {
        name: "auth-sidecar".to_string(),
        image: Some(cfg.sidecar_image.clone()),
        env: Some(sidecar_env),
        volume_mounts: Some(sidecar_mounts),
        resources: Some(sidecar_resources),
        security_context: Some(sidecar_security_ctx),
        readiness_probe: Some(readiness_probe),
        liveness_probe: Some(liveness_probe),
        ..Default::default()
    };

    // FR-41a: Init container for iptables network enforcement
    let init_container = build_init_iptables_container();

    let timeout_secs = stage.timeout.as_secs();
    let active_deadline = if timeout_secs > 0 {
        Some(timeout_secs as i64)
    } else {
        // FR-29: Default 15 min watchdog
        Some(900)
    };

    // FR-24: imagePullSecrets if configured
    let image_pull_secrets = cfg
        .image_pull_secret
        .as_ref()
        .map(|name| vec![LocalObjectReference { name: name.clone() }]);

    Job {
        metadata: ObjectMeta {
            name: Some(job_name),
            namespace: Some(cfg.namespace.clone()),
            labels: Some(labels.clone()),
            ..Default::default()
        },
        spec: Some(JobSpec {
            active_deadline_seconds: active_deadline,
            backoff_limit: Some(0), // No K8s-level retries; we handle retries in the loop engine
            ttl_seconds_after_finished: Some(3600), // 1 hour — must outlive reconciler delays
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    restart_policy: Some("Never".to_string()),
                    init_containers: Some(vec![init_container]),
                    containers: vec![agent_container, sidecar_container],
                    volumes: Some(volumes),
                    image_pull_secrets,
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

/// Build all environment variables for the agent container (FR-27, FR-8, FR-9, FR-10, FR-11).
fn build_agent_env_vars(ctx: &LoopContext, stage: &StageConfig, is_test: bool) -> Vec<EnvVar> {
    let mut env = vec![
        // FR-27: Core env vars
        env_var("STAGE", &stage.name),
        env_var("SPEC_PATH", &ctx.spec_path),
        env_var("BRANCH", &ctx.branch),
        env_var("SHA", &ctx.current_sha),
        env_var("ROUND", &ctx.round.to_string()),
        env_var("MAX_ROUNDS", &ctx.max_rounds.to_string()),
        env_var("LOOP_ID", &ctx.loop_id.to_string()),
        // FR-27: Writable path env vars
        env_var("HOME", "/work/home"),
        env_var("XDG_CONFIG_HOME", "/work/home/.config"),
        env_var("XDG_CACHE_HOME", "/work/home/.cache"),
        env_var("TMPDIR", "/tmp"),
        // FR-8: Proxy env vars for outbound traffic through sidecar egress logger
        env_var("HTTP_PROXY", "http://localhost:9092"),
        env_var("HTTPS_PROXY", "http://localhost:9092"),
        env_var("http_proxy", "http://localhost:9092"),
        env_var("https_proxy", "http://localhost:9092"),
        env_var("NO_PROXY", "localhost,127.0.0.1,::1"),
        env_var("no_proxy", "localhost,127.0.0.1,::1"),
        // FR-9: OpenAI API through sidecar model proxy
        env_var("OPENAI_BASE_URL", "http://localhost:9090/openai"),
        // Claude/Anthropic API through sidecar model proxy
        env_var("ANTHROPIC_BASE_URL", "http://localhost:9090/anthropic"),
    ];

    // FR-10, FR-27: Git identity from engineers table (populated by nemo auth)
    env.push(env_var("GIT_AUTHOR_NAME", &ctx.engineer));
    env.push(env_var("GIT_AUTHOR_EMAIL", &ctx.engineer_email));
    env.push(env_var("GIT_COMMITTER_NAME", &ctx.engineer));
    env.push(env_var("GIT_COMMITTER_EMAIL", &ctx.engineer_email));

    // FR-11: GIT_SSH_COMMAND to route through sidecar SSH proxy on localhost.
    // StrictHostKeyChecking=no is safe here: the connection is pod-local (loopback)
    // to the sidecar, which uses an ephemeral host key per pod. The sidecar itself
    // performs strict host key verification against known_hosts for the real remote.
    env.push(env_var(
        "GIT_SSH_COMMAND",
        "ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -p 9091 localhost",
    ));

    // FR-7: Session ID for round > 1
    if let Some(ref session_id) = ctx.session_id {
        env.push(env_var("SESSION_ID", session_id));
    }

    // Feedback path
    if let Some(ref feedback) = ctx.feedback_path {
        env.push(env_var("FEEDBACK_PATH", feedback));
    }

    // Model override
    if let Some(ref model) = stage.model {
        env.push(env_var("MODEL", model));
    }

    // FR-42a: Affected services for TEST stage (JSON array)
    if is_test {
        // The control plane sets this from git diff analysis.
        // It's passed through LoopContext credentials as a special key.
        for (key, value) in &ctx.credentials {
            if key == "affected_services" {
                env.push(env_var("AFFECTED_SERVICES", value));
            }
        }
    }

    // Credentials are NOT injected as env vars — they go through the sidecar only.
    // This prevents untrusted agent code from reading secrets directly.

    env
}

/// Build all volumes for the pod (FR-25, FR-26, FR-30, FR-47b).
fn build_volumes(
    bare_repo_pvc: &str,
    sessions_pvc: &str,
    engineer: &str,
    ssh_known_hosts_configmap: &str,
) -> Vec<Volume> {
    // Normalize engineer name for K8s Secret references (lowercase, _ -> -)
    let safe_engineer: String = engineer.to_lowercase().replace('_', "-");
    let engineer = &safe_engineer;
    vec![
        // Worktree volume from bare repo PVC (mounted via subPath per job)
        Volume {
            name: "worktree".to_string(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: bare_repo_pvc.to_string(),
                read_only: Some(false),
            }),
            ..Default::default()
        },
        // FR-47b: Session state PVC
        Volume {
            name: "sessions".to_string(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: sessions_pvc.to_string(),
                read_only: Some(false),
            }),
            ..Default::default()
        },
        // Output volume (emptyDir)
        Volume {
            name: "output".to_string(),
            empty_dir: Some(EmptyDirVolumeSource::default()),
            ..Default::default()
        },
        // FR-30: Shared readiness volume (emptyDir)
        Volume {
            name: "shared".to_string(),
            empty_dir: Some(EmptyDirVolumeSource::default()),
            ..Default::default()
        },
        // Writable tmpdir (emptyDir)
        Volume {
            name: "tmpdir".to_string(),
            empty_dir: Some(EmptyDirVolumeSource::default()),
            ..Default::default()
        },
        // Writable home directory (emptyDir)
        Volume {
            name: "home".to_string(),
            empty_dir: Some(EmptyDirVolumeSource::default()),
            ..Default::default()
        },
        // FR-26: Model credentials Secret for sidecar only (never mounted in agent).
        // Mount the whole secret — whichever keys exist (openai, anthropic, ssh)
        // will be available as files. Missing keys are simply absent.
        Volume {
            name: "model-credentials".to_string(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(format!("nemo-creds-{engineer}")),
                optional: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        },
        // FR-26: SSH key Secret for sidecar only. Optional so pods start even
        // if only model creds are registered (no SSH key yet).
        Volume {
            name: "ssh-key".to_string(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(format!("nemo-creds-{engineer}")),
                items: Some(vec![KeyToPath {
                    key: "ssh".to_string(),
                    path: "id_ed25519".to_string(),
                    ..Default::default()
                }]),
                default_mode: Some(0o600),
                optional: Some(true),
            }),
            ..Default::default()
        },
        // SSH known_hosts ConfigMap for sidecar host key verification (Finding 7)
        Volume {
            name: "ssh-known-hosts".to_string(),
            config_map: Some(ConfigMapVolumeSource {
                name: ssh_known_hosts_configmap.to_string(),
                ..Default::default()
            }),
            ..Default::default()
        },
    ]
}

/// Build agent container volume mounts (FR-25).
/// The worktree is mounted via subPath so the agent only sees its own worktree,
/// not the shared bare repo. No secrets are ever mounted in the agent container.
fn build_agent_mounts(is_review_or_audit: bool, worktree_path: &str) -> Vec<VolumeMount> {
    vec![
        VolumeMount {
            name: "worktree".to_string(),
            mount_path: "/work".to_string(),
            sub_path: Some(worktree_path.to_string()),
            read_only: Some(is_review_or_audit), // FR-6: Read-only for REVIEW/AUDIT
            ..Default::default()
        },
        VolumeMount {
            name: "sessions".to_string(),
            mount_path: "/sessions".to_string(),
            ..Default::default()
        },
        VolumeMount {
            name: "output".to_string(),
            mount_path: "/output".to_string(),
            ..Default::default()
        },
        VolumeMount {
            name: "shared".to_string(),
            mount_path: "/tmp/shared".to_string(),
            ..Default::default()
        },
        VolumeMount {
            name: "tmpdir".to_string(),
            mount_path: "/tmp".to_string(),
            ..Default::default()
        },
        VolumeMount {
            name: "home".to_string(),
            mount_path: "/work/home".to_string(),
            ..Default::default()
        },
    ]
}

/// Build sidecar container volume mounts (FR-26).
fn build_sidecar_mounts() -> Vec<VolumeMount> {
    vec![
        VolumeMount {
            name: "model-credentials".to_string(),
            mount_path: "/secrets/model-credentials".to_string(),
            read_only: Some(true),
            ..Default::default()
        },
        VolumeMount {
            name: "ssh-key".to_string(),
            mount_path: "/secrets/ssh-key".to_string(),
            read_only: Some(true),
            ..Default::default()
        },
        VolumeMount {
            name: "ssh-known-hosts".to_string(),
            mount_path: "/secrets/ssh-known-hosts".to_string(),
            read_only: Some(true),
            ..Default::default()
        },
        VolumeMount {
            name: "shared".to_string(),
            mount_path: "/tmp/shared".to_string(),
            ..Default::default()
        },
    ]
}

/// FR-41a: Init container for iptables network egress enforcement.
fn build_init_iptables_container() -> Container {
    let script = r#"set -e
# Install iptables (not shipped with Alpine by default)
apk add --no-cache iptables

# IPv6: disable entirely in V1
sysctl -w net.ipv6.conf.all.disable_ipv6=1
sysctl -w net.ipv6.conf.default.disable_ipv6=1
sysctl -w net.ipv6.conf.lo.disable_ipv6=1

# IPv4: strict egress enforcement for agent UID 1000
# Allow loopback (agent -> sidecar on localhost)
iptables -A OUTPUT -o lo -j ACCEPT
# Allow established connections (return traffic for accepted connections)
iptables -A OUTPUT -m state --state ESTABLISHED,RELATED -j ACCEPT
# Allow sidecar (UID 65534) full outbound access to reach upstream APIs
iptables -A OUTPUT -m owner --uid-owner 65534 -j ACCEPT
# Agent UID 1000: ONLY allow connections to sidecar ports on loopback
# (already covered by -o lo above). Drop everything else from UID 1000.
iptables -A OUTPUT -m owner --uid-owner 1000 -j DROP"#;

    Container {
        name: "init-iptables".to_string(),
        image: Some("alpine:3.19".to_string()),
        command: Some(vec!["/bin/sh".to_string(), "-c".to_string()]),
        args: Some(vec![script.to_string()]),
        security_context: Some(SecurityContext {
            capabilities: Some(Capabilities {
                add: Some(vec!["NET_ADMIN".to_string()]),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// FR-28: Resource limits per job type.
fn resource_limits(
    stage_name: &str,
    _is_test: bool,
    has_jvm_tag: bool,
) -> (ResourceRequirements, ResourceRequirements) {
    let (cpu_req, cpu_lim, mem_req, mem_lim) = match (stage_name, has_jvm_tag) {
        ("test", true) => ("1000m", "2000m", "2Gi", "6Gi"), // TEST (jvm tag)
        ("test", false) => ("500m", "1000m", "1Gi", "3Gi"), // TEST (default)
        _ => ("250m", "500m", "1Gi", "2Gi"),                // IMPLEMENT/REVIEW/AUDIT/REVISE
    };

    let agent = ResourceRequirements {
        requests: Some(BTreeMap::from([
            ("cpu".to_string(), Quantity(cpu_req.to_string())),
            ("memory".to_string(), Quantity(mem_req.to_string())),
        ])),
        limits: Some(BTreeMap::from([
            ("cpu".to_string(), Quantity(cpu_lim.to_string())),
            ("memory".to_string(), Quantity(mem_lim.to_string())),
        ])),
        ..Default::default()
    };

    let sidecar = ResourceRequirements {
        requests: Some(BTreeMap::from([
            ("cpu".to_string(), Quantity("50m".to_string())),
            ("memory".to_string(), Quantity("64Mi".to_string())),
        ])),
        limits: Some(BTreeMap::from([
            ("cpu".to_string(), Quantity("100m".to_string())),
            ("memory".to_string(), Quantity("128Mi".to_string())),
        ])),
        ..Default::default()
    };

    (agent, sidecar)
}

/// Helper to create an EnvVar with a string value.
fn env_var(name: &str, value: &str) -> EnvVar {
    EnvVar {
        name: name.to_string(),
        value: Some(value.to_string()),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn test_ctx() -> LoopContext {
        LoopContext {
            loop_id: Uuid::parse_str("a1b2c3d4-e5f6-7890-abcd-ef1234567890").unwrap(),
            engineer: "alice".to_string(),
            engineer_email: "alice@example.com".to_string(),
            spec_path: "specs/feature/invoice-cancel.md".to_string(),
            branch: "agent/alice/invoice-cancel-a1b2c3d4".to_string(),
            current_sha: "abc123".to_string(),
            round: 2,
            max_rounds: 15,
            retry_count: 0,
            session_id: Some("session-123".to_string()),
            feedback_path: Some(".agent/review-feedback-round-1.json".to_string()),
            worktree_path: "wt/agent-alice-invoice-cancel-a1b2c3d4".to_string(),
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

    fn test_cfg() -> JobBuildConfig {
        JobBuildConfig {
            namespace: "nemo-jobs".to_string(),
            agent_image: "nemo-agent:latest".to_string(),
            sidecar_image: "nemo-sidecar:latest".to_string(),
            bare_repo_pvc: "nemo-bare-repo".to_string(),
            sessions_pvc: "nemo-sessions".to_string(),
            image_pull_secret: None,
            git_repo_url: "git@github.com:test-org/test-repo.git".to_string(),
            ssh_known_hosts_configmap: "nemo-ssh-known-hosts".to_string(),
        }
    }

    #[test]
    fn test_build_job_name_includes_attempt() {
        let ctx = test_ctx();
        let stage = test_stage();
        let cfg = test_cfg();
        let job = build_job(&ctx, &stage, &cfg);
        let name = job.metadata.name.unwrap();
        // FR-31: nemo-{loop_id_short}-{stage}-r{round}-t{attempt}
        assert_eq!(name, "nemo-a1b2c3d4-implement-r2-t1");
    }

    #[test]
    fn test_build_job_name_with_retry() {
        let mut ctx = test_ctx();
        ctx.retry_count = 3;
        let stage = test_stage();
        let cfg = test_cfg();
        let job = build_job(&ctx, &stage, &cfg);
        let name = job.metadata.name.unwrap();
        // retry_count=3 -> attempt=4
        assert_eq!(name, "nemo-a1b2c3d4-implement-r2-t4");
    }

    #[test]
    fn test_build_job_labels() {
        let ctx = test_ctx();
        let stage = test_stage();
        let cfg = test_cfg();
        let job = build_job(&ctx, &stage, &cfg);
        let labels = job.metadata.labels.unwrap();
        assert_eq!(labels["app"], "nemo");
        assert_eq!(labels["nemo.dev/stage"], "implement");
        assert_eq!(labels["nemo.dev/round"], "2");
        assert_eq!(labels["nemo.dev/engineer"], "alice");
    }

    #[test]
    fn test_build_job_two_containers() {
        let ctx = test_ctx();
        let stage = test_stage();
        let cfg = test_cfg();
        let job = build_job(&ctx, &stage, &cfg);
        let pod_spec = job.spec.unwrap().template.spec.unwrap();
        // FR-24: Two containers
        assert_eq!(pod_spec.containers.len(), 2);
        assert_eq!(pod_spec.containers[0].name, "agent");
        assert_eq!(pod_spec.containers[1].name, "auth-sidecar");
    }

    #[test]
    fn test_build_job_init_container() {
        let ctx = test_ctx();
        let stage = test_stage();
        let cfg = test_cfg();
        let job = build_job(&ctx, &stage, &cfg);
        let pod_spec = job.spec.unwrap().template.spec.unwrap();
        // FR-41a: Init container for iptables
        let init = pod_spec.init_containers.unwrap();
        assert_eq!(init.len(), 1);
        assert_eq!(init[0].name, "init-iptables");
        // Must have NET_ADMIN capability
        let caps = init[0]
            .security_context
            .as_ref()
            .unwrap()
            .capabilities
            .as_ref()
            .unwrap();
        assert!(
            caps.add
                .as_ref()
                .unwrap()
                .contains(&"NET_ADMIN".to_string())
        );
    }

    #[test]
    fn test_build_job_agent_env_vars() {
        let ctx = test_ctx();
        let stage = test_stage();
        let cfg = test_cfg();
        let job = build_job(&ctx, &stage, &cfg);
        let containers = &job.spec.unwrap().template.spec.unwrap().containers;
        let env = containers[0].env.as_ref().unwrap();

        let find_env = |name: &str| -> Option<String> {
            env.iter()
                .find(|e| e.name == name)
                .and_then(|e| e.value.clone())
        };

        // FR-27: Core env vars
        assert_eq!(find_env("STAGE").unwrap(), "implement");
        assert_eq!(find_env("ROUND").unwrap(), "2");
        assert_eq!(find_env("MAX_ROUNDS").unwrap(), "15");
        assert_eq!(find_env("LOOP_ID").unwrap(), ctx.loop_id.to_string());
        assert_eq!(find_env("HOME").unwrap(), "/work/home");
        assert_eq!(find_env("TMPDIR").unwrap(), "/tmp");

        // FR-8: Proxy env vars
        assert_eq!(find_env("HTTP_PROXY").unwrap(), "http://localhost:9092");
        assert_eq!(find_env("HTTPS_PROXY").unwrap(), "http://localhost:9092");
        assert_eq!(find_env("NO_PROXY").unwrap(), "localhost,127.0.0.1,::1");

        // FR-9: OpenAI base URL
        assert_eq!(
            find_env("OPENAI_BASE_URL").unwrap(),
            "http://localhost:9090/openai"
        );

        // FR-10: Git identity
        assert_eq!(find_env("GIT_AUTHOR_NAME").unwrap(), "alice");
        assert_eq!(find_env("GIT_COMMITTER_NAME").unwrap(), "alice");

        // FR-11: Git SSH command
        assert!(find_env("GIT_SSH_COMMAND").unwrap().contains("9091"));

        // FR-7: Session ID
        assert_eq!(find_env("SESSION_ID").unwrap(), "session-123");

        // Model override
        assert_eq!(find_env("MODEL").unwrap(), "claude-opus-4");
    }

    #[test]
    fn test_build_job_agent_security_context() {
        let ctx = test_ctx();
        let stage = test_stage();
        let cfg = test_cfg();
        let job = build_job(&ctx, &stage, &cfg);
        let agent = &job.spec.unwrap().template.spec.unwrap().containers[0];
        let sec = agent.security_context.as_ref().unwrap();
        // FR-25: Non-root, UID 1000, read-only root fs
        assert_eq!(sec.run_as_non_root, Some(true));
        assert_eq!(sec.run_as_user, Some(1000));
        assert_eq!(sec.read_only_root_filesystem, Some(true));
    }

    #[test]
    fn test_build_job_sidecar_security_context() {
        let ctx = test_ctx();
        let stage = test_stage();
        let cfg = test_cfg();
        let job = build_job(&ctx, &stage, &cfg);
        let sidecar = &job.spec.unwrap().template.spec.unwrap().containers[1];
        let sec = sidecar.security_context.as_ref().unwrap();
        // Sidecar runs as UID 65534 (nobody)
        assert_eq!(sec.run_as_non_root, Some(true));
        assert_eq!(sec.run_as_user, Some(65534));
    }

    #[test]
    fn test_build_job_resource_limits_implement() {
        let ctx = test_ctx();
        let stage = test_stage();
        let cfg = test_cfg();
        let job = build_job(&ctx, &stage, &cfg);
        let containers = &job.spec.unwrap().template.spec.unwrap().containers;

        // Agent resources for implement stage (FR-28)
        let agent_res = containers[0].resources.as_ref().unwrap();
        let limits = agent_res.limits.as_ref().unwrap();
        assert_eq!(limits["cpu"], Quantity("500m".to_string()));
        assert_eq!(limits["memory"], Quantity("2Gi".to_string()));

        // Sidecar resources (FR-28)
        let sidecar_res = containers[1].resources.as_ref().unwrap();
        let sidecar_limits = sidecar_res.limits.as_ref().unwrap();
        assert_eq!(sidecar_limits["cpu"], Quantity("100m".to_string()));
        assert_eq!(sidecar_limits["memory"], Quantity("128Mi".to_string()));
    }

    #[test]
    fn test_build_job_resource_limits_test() {
        let ctx = test_ctx();
        let stage = StageConfig {
            name: "test".to_string(),
            timeout: Duration::from_secs(1800),
            ..Default::default()
        };
        let cfg = test_cfg();
        let job = build_job(&ctx, &stage, &cfg);
        let containers = &job.spec.unwrap().template.spec.unwrap().containers;

        // FR-28: TEST stage gets more resources
        let agent_res = containers[0].resources.as_ref().unwrap();
        let limits = agent_res.limits.as_ref().unwrap();
        assert_eq!(limits["cpu"], Quantity("1000m".to_string()));
        assert_eq!(limits["memory"], Quantity("3Gi".to_string()));
    }

    #[test]
    fn test_build_job_resource_limits_test_jvm() {
        let mut ctx = test_ctx();
        ctx.credentials = vec![("service_tags".to_string(), "[\"jvm\"]".to_string())];
        let stage = StageConfig {
            name: "test".to_string(),
            timeout: Duration::from_secs(1800),
            ..Default::default()
        };
        let cfg = test_cfg();
        let job = build_job(&ctx, &stage, &cfg);
        let containers = &job.spec.unwrap().template.spec.unwrap().containers;

        // FR-28: TEST (jvm tag) gets elevated resources
        let agent_res = containers[0].resources.as_ref().unwrap();
        let limits = agent_res.limits.as_ref().unwrap();
        assert_eq!(limits["cpu"], Quantity("2000m".to_string()));
        assert_eq!(limits["memory"], Quantity("6Gi".to_string()));
    }

    #[test]
    fn test_build_job_sidecar_git_repo_url() {
        let ctx = test_ctx();
        let stage = test_stage();
        let cfg = test_cfg();
        let job = build_job(&ctx, &stage, &cfg);
        let sidecar = &job.spec.unwrap().template.spec.unwrap().containers[1];
        let env = sidecar.env.as_ref().unwrap();
        let git_url = env.iter().find(|e| e.name == "GIT_REPO_URL").unwrap();
        // FR-18: Sidecar gets the actual git repo URL, not branch
        assert_eq!(
            git_url.value.as_deref(),
            Some("git@github.com:test-org/test-repo.git")
        );
    }

    #[test]
    fn test_build_job_engineer_email() {
        let ctx = test_ctx();
        let stage = test_stage();
        let cfg = test_cfg();
        let job = build_job(&ctx, &stage, &cfg);
        let agent = &job.spec.unwrap().template.spec.unwrap().containers[0];
        let env = agent.env.as_ref().unwrap();
        let author_email = env.iter().find(|e| e.name == "GIT_AUTHOR_EMAIL").unwrap();
        let committer_email = env
            .iter()
            .find(|e| e.name == "GIT_COMMITTER_EMAIL")
            .unwrap();
        // FR-27: Email comes from engineers table, not hardcoded
        assert_eq!(author_email.value.as_deref(), Some("alice@example.com"));
        assert_eq!(committer_email.value.as_deref(), Some("alice@example.com"));
    }

    #[test]
    fn test_build_job_review_read_only_worktree() {
        let ctx = test_ctx();
        let stage = StageConfig {
            name: "review".to_string(),
            timeout: Duration::from_secs(900),
            ..Default::default()
        };
        let cfg = test_cfg();
        let job = build_job(&ctx, &stage, &cfg);
        let agent = &job.spec.unwrap().template.spec.unwrap().containers[0];
        let mounts = agent.volume_mounts.as_ref().unwrap();
        let worktree_mount = mounts.iter().find(|m| m.mount_path == "/work").unwrap();
        // FR-6: REVIEW stage mounts worktree read-only
        assert_eq!(worktree_mount.read_only, Some(true));
    }

    #[test]
    fn test_build_job_implement_writable_worktree() {
        let ctx = test_ctx();
        let stage = test_stage(); // implement
        let cfg = test_cfg();
        let job = build_job(&ctx, &stage, &cfg);
        let agent = &job.spec.unwrap().template.spec.unwrap().containers[0];
        let mounts = agent.volume_mounts.as_ref().unwrap();
        let worktree_mount = mounts.iter().find(|m| m.mount_path == "/work").unwrap();
        // IMPLEMENT stage mounts worktree writable
        assert_eq!(worktree_mount.read_only, Some(false));
    }

    #[test]
    fn test_build_job_no_secrets_in_agent() {
        // Finding 3: No credentials mounted in untrusted agent container
        let ctx = test_ctx();
        let stage = test_stage(); // implement
        let cfg = test_cfg();
        let job = build_job(&ctx, &stage, &cfg);
        let agent = &job.spec.unwrap().template.spec.unwrap().containers[0];
        let mounts = agent.volume_mounts.as_ref().unwrap();
        // No /secrets, no claude-session, no model-credentials
        assert!(!mounts.iter().any(|m| m.mount_path.contains("secret")
            || m.mount_path.contains("claude")
            || m.mount_path.contains("credential")));
    }

    #[test]
    fn test_build_job_worktree_subpath() {
        // Finding 5: Worktree mounted via subPath, not bare repo root
        let ctx = test_ctx();
        let stage = test_stage();
        let cfg = test_cfg();
        let job = build_job(&ctx, &stage, &cfg);
        let agent = &job.spec.unwrap().template.spec.unwrap().containers[0];
        let mounts = agent.volume_mounts.as_ref().unwrap();
        let worktree_mount = mounts.iter().find(|m| m.mount_path == "/work").unwrap();
        assert_eq!(
            worktree_mount.sub_path.as_deref(),
            Some("wt/agent-alice-invoice-cancel-a1b2c3d4")
        );
    }

    #[test]
    fn test_build_job_sidecar_mounts_no_agent_secrets() {
        let ctx = test_ctx();
        let stage = test_stage();
        let cfg = test_cfg();
        let job = build_job(&ctx, &stage, &cfg);
        let pod_spec = job.spec.unwrap().template.spec.unwrap();

        // Sidecar has secret mounts
        let sidecar_mounts = pod_spec.containers[1].volume_mounts.as_ref().unwrap();
        assert!(
            sidecar_mounts
                .iter()
                .any(|m| m.mount_path == "/secrets/model-credentials")
        );
        assert!(
            sidecar_mounts
                .iter()
                .any(|m| m.mount_path == "/secrets/ssh-key")
        );

        // Agent does NOT have /secrets/ mounts (FR-26)
        let agent_mounts = pod_spec.containers[0].volume_mounts.as_ref().unwrap();
        assert!(
            !agent_mounts
                .iter()
                .any(|m| m.mount_path.starts_with("/secrets"))
        );
    }

    #[test]
    fn test_build_job_sidecar_probes() {
        let ctx = test_ctx();
        let stage = test_stage();
        let cfg = test_cfg();
        let job = build_job(&ctx, &stage, &cfg);
        let sidecar = &job.spec.unwrap().template.spec.unwrap().containers[1];

        // FR-22: Readiness and liveness probes on :9093/healthz
        let readiness = sidecar.readiness_probe.as_ref().unwrap();
        let http = readiness.http_get.as_ref().unwrap();
        assert_eq!(http.port, IntOrString::Int(9093));
        assert_eq!(http.path.as_deref(), Some("/healthz"));

        let liveness = sidecar.liveness_probe.as_ref().unwrap();
        let http = liveness.http_get.as_ref().unwrap();
        assert_eq!(http.port, IntOrString::Int(9093));
        assert_eq!(liveness.initial_delay_seconds, Some(5));
        assert_eq!(liveness.period_seconds, Some(10));
    }

    #[test]
    fn test_build_job_restart_policy_never() {
        let ctx = test_ctx();
        let stage = test_stage();
        let cfg = test_cfg();
        let job = build_job(&ctx, &stage, &cfg);
        let pod_spec = job.spec.unwrap().template.spec.unwrap();
        assert_eq!(pod_spec.restart_policy.as_deref(), Some("Never"));
    }

    #[test]
    fn test_build_job_timeout() {
        let ctx = test_ctx();
        let stage = test_stage();
        let cfg = test_cfg();
        let job = build_job(&ctx, &stage, &cfg);
        let spec = job.spec.unwrap();
        assert_eq!(spec.active_deadline_seconds, Some(1800));
    }

    #[test]
    fn test_build_job_default_timeout() {
        let ctx = test_ctx();
        let stage = StageConfig {
            name: "review".to_string(),
            timeout: Duration::from_secs(0),
            ..Default::default()
        };
        let cfg = test_cfg();
        let job = build_job(&ctx, &stage, &cfg);
        let spec = job.spec.unwrap();
        // FR-29: Default 15 min watchdog
        assert_eq!(spec.active_deadline_seconds, Some(900));
    }

    #[test]
    fn test_build_job_image_pull_secrets() {
        let ctx = test_ctx();
        let stage = test_stage();
        let mut cfg = test_cfg();
        cfg.image_pull_secret = Some("nemo-registry-creds".to_string());
        let job = build_job(&ctx, &stage, &cfg);
        let pod_spec = job.spec.unwrap().template.spec.unwrap();
        let secrets = pod_spec.image_pull_secrets.unwrap();
        assert_eq!(secrets.len(), 1);
        assert_eq!(secrets[0].name, "nemo-registry-creds");
    }

    #[test]
    fn test_build_job_no_image_pull_secrets() {
        let ctx = test_ctx();
        let stage = test_stage();
        let cfg = test_cfg();
        let job = build_job(&ctx, &stage, &cfg);
        let pod_spec = job.spec.unwrap().template.spec.unwrap();
        assert!(pod_spec.image_pull_secrets.is_none());
    }

    #[test]
    fn test_job_name_generation() {
        let id = Uuid::parse_str("a1b2c3d4-e5f6-7890-abcd-ef1234567890").unwrap();
        assert_eq!(job_name(id, "implement", 2), "nemo-a1b2c3d4-implement-r2");
        assert_eq!(job_name(id, "review", 1), "nemo-a1b2c3d4-review-r1");
    }

    #[test]
    fn test_build_job_volumes_count() {
        let ctx = test_ctx();
        let stage = test_stage();
        let cfg = test_cfg();
        let job = build_job(&ctx, &stage, &cfg);
        let volumes = job.spec.unwrap().template.spec.unwrap().volumes.unwrap();
        // worktree, sessions, output, shared, tmpdir, home, model-credentials, ssh-key, ssh-known-hosts
        assert_eq!(volumes.len(), 9);
    }
}
