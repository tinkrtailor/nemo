use crate::types::{LoopContext, Stage, StageConfig};
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

use crate::config::CacheConfig;

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
    /// Skip the init-iptables container (for local dev with k3d).
    pub skip_iptables: bool,
    /// Resolved cache configuration. Controls the /cache PVC mount and
    /// cache-related env vars on implement/revise pods.
    pub cache: CacheConfig,
}

/// Build a K8s Job spec for a given stage.
///
/// The Job contains:
/// - init container: `init-iptables` for network egress enforcement (FR-41a)
/// - **native sidecar** initContainer: `auth-sidecar` for model API proxy, git
///   SSH proxy, egress logging. Uses the K8s 1.29+ native sidecar pattern
///   (`restartPolicy: Always` on an initContainer) so the sidecar:
///     1. Starts BEFORE the agent (its `startupProbe` gates agent start),
///     2. Auto-terminates when the agent exits (k8s native sidecar lifecycle),
///        which lets the Pod reach `Succeeded` and the Job reach `Complete`
///        on the success path. Without this, the long-running sidecar would
///        outlive the agent forever and every successful run would only
///        terminate via `activeDeadlineSeconds` as a `DeadlineExceeded`
///        failure (issue #53).
/// - agent container: runs the agent entrypoint
///
/// Job naming: `nautiloop-{loop_id_short}-{stage}-r{round}-t{attempt}` (FR-31)
/// Labels: `nautiloop.dev/loop-id`, `nautiloop.dev/stage`, `nautiloop.dev/engineer`, `nautiloop.dev/round` (FR-32)
pub fn build_job(ctx: &LoopContext, stage: &StageConfig, cfg: &JobBuildConfig) -> Job {
    let short_id = &ctx.loop_id.to_string()[..8];
    // FR-31: Include attempt number in name to avoid AlreadyExists on redispatch.
    // Attempt = retry_count + 1 (first dispatch is attempt 1, first retry is attempt 2).
    let attempt = ctx.retry_count + 1;
    let job_name = format!(
        "nautiloop-{short_id}-{}-r{}-t{attempt}",
        stage.name, ctx.round
    );

    // FR-32: Labels for control plane queries
    let mut labels = BTreeMap::new();
    labels.insert("app".to_string(), "nautiloop".to_string());
    labels.insert("nautiloop.dev/loop-id".to_string(), ctx.loop_id.to_string());
    labels.insert("nautiloop.dev/stage".to_string(), stage.name.clone());
    labels.insert("nautiloop.dev/round".to_string(), ctx.round.to_string());
    labels.insert("nautiloop.dev/engineer".to_string(), ctx.engineer.clone());

    let parsed_stage = Stage::from_short_name(&stage.name);
    let is_review_or_audit = matches!(parsed_stage, Some(Stage::Review) | Some(Stage::Audit));
    let is_implement_or_revise =
        matches!(parsed_stage, Some(Stage::Implement) | Some(Stage::Revise));
    let is_test = matches!(parsed_stage, Some(Stage::Test));

    // FR-27: Environment variables on the agent container
    let agent_env = build_agent_env_vars(ctx, stage, is_test, is_implement_or_revise, &cfg.cache);

    // Build volumes (FR-25, FR-26, FR-30)
    let mut volumes = build_volumes(
        &cfg.bare_repo_pvc,
        &cfg.sessions_pvc,
        &ctx.engineer,
        &cfg.ssh_known_hosts_configmap,
        &cfg.cache,
    );

    // FR-25b: Claude session volume for IMPLEMENT/REVISE/REVIEW/AUDIT stages
    // (review/audit may also use claude CLI when model-review is a claude-* model)
    if is_implement_or_revise || is_review_or_audit {
        let safe_engineer: String = ctx.engineer.to_lowercase().replace('_', "-");
        volumes.push(Volume {
            name: "claude-session".to_string(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(format!("nautiloop-creds-{safe_engineer}")),
                items: Some(vec![KeyToPath {
                    key: "claude".to_string(),
                    path: ".credentials.json".to_string(),
                    ..Default::default()
                }]),
                optional: Some(true), // May not exist if engineer hasn't run nemo auth --claude
                ..Default::default()
            }),
            ..Default::default()
        });
    }

    // Build agent container volume mounts (with subPath for worktree isolation)
    let agent_mounts = build_agent_mounts(
        is_review_or_audit,
        is_implement_or_revise,
        &ctx.worktree_path,
        &cfg.cache,
    );

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

    // FR-22: Sidecar readiness probe.
    // Pod-level "ready" gate. Cheap and frequent.
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

    // FR-22: Sidecar startup probe.
    // For a native sidecar (initContainer with restartPolicy: Always), the
    // startupProbe is the gate that decides when subsequent containers
    // (the agent) are allowed to start. Without it, k8s would consider the
    // sidecar "started" the instant the container process exists and would
    // race the agent against the sidecar's port-binding code. Generous
    // failure_threshold * period covers slow image pulls and cold starts
    // without ever flapping under steady state.
    //
    // The sidecar's /healthz handler returns 503 until ALL four proxy ports
    // (:9090 model, :9091 git SSH, :9092 egress, :9093 health) are listening,
    // then flips to 200 (see `ready` atomic flag in images/sidecar/main.go).
    // So this probe genuinely gates the agent on full sidecar readiness, not
    // just on the health server having bound its own port.
    let startup_probe = Probe {
        http_get: Some(HTTPGetAction {
            path: Some("/healthz".to_string()),
            port: IntOrString::Int(9093),
            ..Default::default()
        }),
        period_seconds: Some(2),
        failure_threshold: Some(30), // up to 60s for sidecar to bind all 4 ports
        timeout_seconds: Some(3),
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

    // FR-24: Auth sidecar as a K8s native sidecar (initContainer with
    // restartPolicy: Always). This is what makes the sidecar auto-terminate
    // when the agent exits, so the Job actually reaches Complete on the
    // success path (issue #53). It also gives us free ordering: the agent
    // container does not start until the sidecar's startupProbe passes,
    // replacing the historical /tmp/shared/ready polling hack.
    let sidecar_container = Container {
        name: "auth-sidecar".to_string(),
        image: Some(cfg.sidecar_image.clone()),
        env: Some(sidecar_env),
        volume_mounts: Some(sidecar_mounts),
        resources: Some(sidecar_resources),
        security_context: Some(sidecar_security_ctx),
        startup_probe: Some(startup_probe),
        readiness_probe: Some(readiness_probe),
        // K8s native sidecar marker. Requires k8s >= 1.29 (GA) and is
        // available on the k3s versions nautiloop pins (>= v1.32).
        restart_policy: Some("Always".to_string()),
        ..Default::default()
    };

    // FR-41a: Init container for iptables network enforcement (runs and exits).
    // Skipped in local dev (k3d) where NET_ADMIN privileged init containers
    // may not behave identically to production.
    let maybe_init_container = if cfg.skip_iptables {
        None
    } else {
        Some(build_init_iptables_container())
    };

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
                    // Order matters: iptables runs and exits before the
                    // sidecar starts, so the sidecar's outbound traffic
                    // hits the agent-vs-sidecar UID rules. The sidecar
                    // (native sidecar via restartPolicy: Always) then
                    // stays up for the lifetime of the agent and is
                    // auto-terminated when the agent exits.
                    init_containers: Some(
                        maybe_init_container
                            .into_iter()
                            .chain(std::iter::once(sidecar_container))
                            .collect(),
                    ),
                    containers: vec![agent_container],
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

/// Build all environment variables for the agent container (FR-27, FR-8, FR-9, FR-10, FR-11).
fn build_agent_env_vars(
    ctx: &LoopContext,
    stage: &StageConfig,
    is_test: bool,
    is_implement_or_revise: bool,
    cache: &CacheConfig,
) -> Vec<EnvVar> {
    let mut env = vec![
        // FR-27: Core env vars
        env_var("STAGE", &stage.name),
        env_var("SPEC_PATH", &ctx.spec_path),
        env_var("BRANCH", &ctx.branch),
        env_var("SHA", &ctx.current_sha),
        env_var("ROUND", &ctx.round.to_string()),
        env_var("MAX_ROUNDS", &ctx.max_rounds.to_string()),
        env_var("LOOP_ID", &ctx.loop_id.to_string()),
        // FR-27: Writable path env vars.
        // HOME lives at /home/agent (NOT inside /work) so the home EmptyDir
        // mount doesn't need to be created inside the worktree mount. For
        // REVIEW/AUDIT stages /work is mounted read-only (FR-6), and a
        // sub-mount inside a read-only mount fails at pod start with:
        //   mkdirat .../rootfs/work/home: read-only file system
        // because containerd cannot create the mountpoint inside the
        // read-only worktree.
        env_var("HOME", "/home/agent"),
        env_var("XDG_CONFIG_HOME", "/home/agent/.config"),
        env_var("XDG_CACHE_HOME", "/home/agent/.cache"),
        env_var("TMPDIR", "/tmp"),
        // FR-8: Proxy env vars for outbound traffic through sidecar egress logger
        env_var("HTTP_PROXY", "http://localhost:9092"),
        env_var("HTTPS_PROXY", "http://localhost:9092"),
        env_var("http_proxy", "http://localhost:9092"),
        env_var("https_proxy", "http://localhost:9092"),
        env_var("NO_PROXY", "localhost,127.0.0.1,::1"),
        env_var("no_proxy", "localhost,127.0.0.1,::1"),
        // FR-9: OpenAI API through sidecar model proxy.
        //
        // OPENAI_BASE_URL points at the sidecar's :9090 model proxy. The
        // sidecar reads /secrets/model-credentials/openai and OVERWRITES
        // the Authorization header on every forwarded request (see
        // modelProxyHandler in images/sidecar/main.go). So the real
        // OpenAI Platform key never enters the agent container — the
        // agent sends a placeholder, the sidecar swaps in the real key.
        //
        // OPENAI_API_KEY is a PLACEHOLDER that exists ONLY to make
        // opencode enable its OpenAI provider. opencode v1.3.x detects
        // the OpenAI provider via the presence of OPENAI_API_KEY in
        // the environment (`opencode providers list` shows it in the
        // "Environment" section). Without it, opencode falls back to
        // its built-in `opencode/*` models routed through opencode.ai,
        // which (a) bypasses the sidecar credential-injection path
        // entirely, (b) sends prompts to a third-party service with
        // unknown billing/retention, and (c) makes `nemo auth --openai`
        // dead code. See issue #62.
        //
        // The literal value `sk-replaced-by-sidecar` is sent as the
        // Bearer token in every opencode request to :9090, and the
        // sidecar replaces it with the real key before forwarding to
        // api.openai.com. The agent never sees the real key.
        //
        // The base URL MUST include `/v1` because opencode's SDK
        // (AI SDK / @ai-sdk/openai) appends the endpoint path directly
        // to OPENAI_BASE_URL without inserting a version prefix. With
        // the old `http://localhost:9090/openai`, opencode sends requests
        // to `http://localhost:9090/openai/responses` and
        // `http://localhost:9090/openai/chat/completions`, which the
        // sidecar forwards to `https://api.openai.com/responses` and
        // `https://api.openai.com/chat/completions` — both 404 because
        // the real endpoints are under `/v1/`. Symptom: opencode hangs
        // in `ep_poll` for the full stage deadline while its internal
        // log silently records `statusCode: 404` responses for every
        // model call, the stage produces zero bytes of output, and the
        // loop fails as BackoffLimitExceeded.
        env_var("OPENAI_BASE_URL", "http://localhost:9090/openai/v1"),
        env_var("OPENAI_API_KEY", "sk-replaced-by-sidecar"),
        // Note: ANTHROPIC_BASE_URL is NOT set. Claude Code authenticates via the
        // mounted ~/.claude/ session directory (FR-25b), not via the sidecar proxy.
        // Base branch for diff context in review/audit stages
        env_var("NAUTILOOP_BASE_BRANCH", &ctx.base_branch),
    ];

    // FR-10, FR-27: Git identity from engineer identity (populated by nemo auth)
    env.push(env_var("GIT_AUTHOR_NAME", &ctx.engineer_name));
    env.push(env_var("GIT_AUTHOR_EMAIL", &ctx.engineer_email));
    env.push(env_var("GIT_COMMITTER_NAME", &ctx.engineer_name));
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

    // Cache env vars for implement/revise stages. Driven by [cache.env] in
    // nemo.toml (FR-3a). When [cache] is absent, sccache defaults are injected
    // by NautiloopConfig::resolved_cache_config(). When disabled=true, no env
    // vars are set. Sorted by key for deterministic pod specs.
    if is_implement_or_revise && !cache.disabled {
        let mut keys: Vec<&String> = cache.env.keys().collect();
        keys.sort();
        for key in keys {
            env.push(env_var(key, &cache.env[key]));
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
    cache: &CacheConfig,
) -> Vec<Volume> {
    // Normalize engineer name for K8s Secret references (lowercase, _ -> -)
    let safe_engineer: String = engineer.to_lowercase().replace('_', "-");
    let engineer = &safe_engineer;
    let mut vols = vec![
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
    ];

    // FR-1a: Shared cache PVC (nautiloop-cache). Only included when cache is
    // not disabled. Mounted at /cache on implement/revise stages via build_agent_mounts.
    if !cache.disabled {
        vols.push(Volume {
            name: "cache".to_string(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: "nautiloop-cache".to_string(),
                read_only: Some(false),
            }),
            ..Default::default()
        });
    }

    vols.extend([
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
                secret_name: Some(format!("nautiloop-creds-{engineer}")),
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
                secret_name: Some(format!("nautiloop-creds-{engineer}")),
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
    ]);

    vols
}

/// Build agent container volume mounts (FR-25, FR-25b).
/// The worktree is mounted via subPath so the agent only sees its own worktree.
/// For IMPLEMENT/REVISE, the Claude session dir is mounted read-only.
fn build_agent_mounts(
    is_review_or_audit: bool,
    is_implement_or_revise: bool,
    worktree_path: &str,
    cache: &CacheConfig,
) -> Vec<VolumeMount> {
    let mut mounts = vec![
        VolumeMount {
            name: "worktree".to_string(),
            mount_path: "/work".to_string(),
            sub_path: Some(worktree_path.to_string()),
            read_only: Some(is_review_or_audit), // FR-6: Read-only for REVIEW/AUDIT
            ..Default::default()
        },
        // Mount the whole bare-repo PVC (no subPath) at /bare-repo so the
        // worktree's .git pointer file can resolve `gitdir: /bare-repo/worktrees/<name>`.
        // Without this, /work/.git points at a path outside the subPath mount, git
        // fails, and the agent runs `git init` creating a disjoint repo whose commits
        // never reach the bare repo's branch ref.
        VolumeMount {
            name: "worktree".to_string(),
            mount_path: "/bare-repo".to_string(),
            read_only: Some(is_review_or_audit),
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
            mount_path: "/home/agent".to_string(),
            ..Default::default()
        },
    ];

    // FR-2a: Mount shared cache PVC at /cache for IMPLEMENT/REVISE only.
    // Review/audit are read-only stages. Test is per-service.
    // FR-3d: Skip mount when cache is disabled.
    if is_implement_or_revise && !cache.disabled {
        mounts.push(VolumeMount {
            name: "cache".to_string(),
            mount_path: "/cache".to_string(),
            ..Default::default()
        });
    }

    // FR-25b: Mount Claude credentials for IMPLEMENT/REVISE/REVIEW/AUDIT stages.
    // Mounted at /secrets/claude-creds/ (read-only), NOT at ~/.claude directly.
    // The entrypoint copies .credentials.json to the writable ~/.claude/ so that
    // Claude Code can create session-env/ and other runtime directories there.
    if is_implement_or_revise || is_review_or_audit {
        mounts.push(VolumeMount {
            name: "claude-session".to_string(),
            mount_path: "/secrets/claude-creds".to_string(),
            read_only: Some(true),
            ..Default::default()
        });
    }

    mounts
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
# Install iptables + ip6tables (not shipped with Alpine by default).
# The Alpine `iptables` package provides both binaries.
apk add --no-cache iptables ip6tables

# IPv6: block egress entirely in V1.
# We deliberately do NOT use `sysctl -w net.ipv6.conf.*.disable_ipv6=1` here:
# /proc/sys is mounted read-only inside the container without
# `privileged: true`, and the NET_ADMIN capability does not grant write
# access to it. Using ip6tables to DROP all IPv6 OUTPUT achieves the same
# goal (no IPv6 egress from the agent or sidecar) without requiring an
# elevated security context.
ip6tables -P OUTPUT DROP
ip6tables -A OUTPUT -o lo -j ACCEPT

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
    use uuid::Uuid;

    fn test_ctx() -> LoopContext {
        LoopContext {
            loop_id: Uuid::parse_str("a1b2c3d4-e5f6-7890-abcd-ef1234567890").unwrap(),
            engineer: "alice".to_string(),
            engineer_name: "Alice Smith".to_string(),
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
            base_branch: "main".to_string(),
        }
    }

    fn test_stage() -> StageConfig {
        StageConfig {
            name: "implement".to_string(),
            model: Some("claude-opus-4".to_string()),
            prompt_template: Some(".nautiloop/prompts/implement.md".to_string()),
            timeout: Duration::from_secs(1800),
            max_retries: 2,
        }
    }

    fn test_cfg() -> JobBuildConfig {
        JobBuildConfig {
            namespace: "nautiloop-jobs".to_string(),
            agent_image: "nautiloop-agent:latest".to_string(),
            sidecar_image: "nautiloop-sidecar:latest".to_string(),
            bare_repo_pvc: "nautiloop-bare-repo".to_string(),
            sessions_pvc: "nautiloop-sessions".to_string(),
            image_pull_secret: None,
            git_repo_url: "git@github.com:test-org/test-repo.git".to_string(),
            ssh_known_hosts_configmap: "nautiloop-ssh-known-hosts".to_string(),
            skip_iptables: false,
            cache: CacheConfig::sccache_defaults(),
        }
    }

    #[test]
    fn test_build_job_name_includes_attempt() {
        let ctx = test_ctx();
        let stage = test_stage();
        let cfg = test_cfg();
        let job = build_job(&ctx, &stage, &cfg);
        let name = job.metadata.name.unwrap();
        // FR-31: nautiloop-{loop_id_short}-{stage}-r{round}-t{attempt}
        assert_eq!(name, "nautiloop-a1b2c3d4-implement-r2-t1");
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
        assert_eq!(name, "nautiloop-a1b2c3d4-implement-r2-t4");
    }

    #[test]
    fn test_build_job_labels() {
        let ctx = test_ctx();
        let stage = test_stage();
        let cfg = test_cfg();
        let job = build_job(&ctx, &stage, &cfg);
        let labels = job.metadata.labels.unwrap();
        assert_eq!(labels["app"], "nautiloop");
        assert_eq!(labels["nautiloop.dev/stage"], "implement");
        assert_eq!(labels["nautiloop.dev/round"], "2");
        assert_eq!(labels["nautiloop.dev/engineer"], "alice");
    }

    #[test]
    fn test_build_job_pod_layout() {
        // FR-24, issue #53: agent runs as the only regular container.
        // The auth-sidecar is a NATIVE SIDECAR (initContainer with
        // restartPolicy: Always) so it auto-terminates on agent exit.
        let ctx = test_ctx();
        let stage = test_stage();
        let cfg = test_cfg();
        let job = build_job(&ctx, &stage, &cfg);
        let pod_spec = job.spec.unwrap().template.spec.unwrap();

        // Exactly one regular container — the agent. The sidecar lives
        // in init_containers as a native sidecar.
        assert_eq!(pod_spec.containers.len(), 1);
        assert_eq!(pod_spec.containers[0].name, "agent");

        let init = pod_spec.init_containers.as_ref().unwrap();
        assert_eq!(init.len(), 2);
        assert_eq!(init[0].name, "init-iptables");
        assert_eq!(init[1].name, "auth-sidecar");

        // The native sidecar marker: restartPolicy: Always on the
        // sidecar initContainer. Without this k8s would treat the
        // sidecar as a normal init container that has to exit before
        // the agent starts (which would never happen — the sidecar
        // is a long-running proxy).
        assert_eq!(init[1].restart_policy.as_deref(), Some("Always"));

        // The iptables init container is NOT a native sidecar — it
        // runs and exits before anything else starts.
        assert!(init[0].restart_policy.is_none());
    }

    #[test]
    fn test_build_job_init_iptables_caps() {
        let ctx = test_ctx();
        let stage = test_stage();
        let cfg = test_cfg();
        let job = build_job(&ctx, &stage, &cfg);
        let pod_spec = job.spec.unwrap().template.spec.unwrap();
        let init = pod_spec.init_containers.unwrap();
        // FR-41a: init-iptables needs NET_ADMIN
        let iptables = init.iter().find(|c| c.name == "init-iptables").unwrap();
        let caps = iptables
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
        assert_eq!(find_env("HOME").unwrap(), "/home/agent");
        assert_eq!(find_env("TMPDIR").unwrap(), "/tmp");

        // FR-8: Proxy env vars
        assert_eq!(find_env("HTTP_PROXY").unwrap(), "http://localhost:9092");
        assert_eq!(find_env("HTTPS_PROXY").unwrap(), "http://localhost:9092");
        assert_eq!(find_env("NO_PROXY").unwrap(), "localhost,127.0.0.1,::1");

        // FR-9: OpenAI base URL points at the sidecar model proxy.
        assert_eq!(
            find_env("OPENAI_BASE_URL").unwrap(),
            "http://localhost:9090/openai/v1"
        );
        // FR-9/issue #62: OPENAI_API_KEY placeholder is required so opencode
        // enables its OpenAI provider. The sidecar overwrites the real auth
        // header on forwarded requests, so the agent never sees a real key.
        assert_eq!(
            find_env("OPENAI_API_KEY").unwrap(),
            "sk-replaced-by-sidecar"
        );

        // FR-10: Git identity (display name, not slug)
        assert_eq!(find_env("GIT_AUTHOR_NAME").unwrap(), "Alice Smith");
        assert_eq!(find_env("GIT_COMMITTER_NAME").unwrap(), "Alice Smith");

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

    /// Sidecar lives in init_containers as a native sidecar (issue #53).
    /// Test helper to find it cleanly.
    fn find_sidecar(job: &Job) -> Container {
        let pod_spec = job.spec.as_ref().unwrap().template.spec.as_ref().unwrap();
        pod_spec
            .init_containers
            .as_ref()
            .unwrap()
            .iter()
            .find(|c| c.name == "auth-sidecar")
            .expect("auth-sidecar must be present in init_containers as a native sidecar")
            .clone()
    }

    #[test]
    fn test_build_job_sidecar_security_context() {
        let ctx = test_ctx();
        let stage = test_stage();
        let cfg = test_cfg();
        let job = build_job(&ctx, &stage, &cfg);
        let sidecar = find_sidecar(&job);
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
        let containers = &job
            .spec
            .as_ref()
            .unwrap()
            .template
            .spec
            .as_ref()
            .unwrap()
            .containers;

        // Agent resources for implement stage (FR-28)
        let agent_res = containers[0].resources.as_ref().unwrap();
        let limits = agent_res.limits.as_ref().unwrap();
        assert_eq!(limits["cpu"], Quantity("500m".to_string()));
        assert_eq!(limits["memory"], Quantity("2Gi".to_string()));

        // Sidecar resources (FR-28) — sidecar is now a native sidecar in init_containers
        let sidecar = find_sidecar(&job);
        let sidecar_res = sidecar.resources.as_ref().unwrap();
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
        let sidecar = find_sidecar(&job);
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
    fn test_build_job_implement_has_claude_session() {
        // FR-25b: Claude credentials mounted at /secrets/claude-creds for implement stage
        let ctx = test_ctx();
        let stage = test_stage(); // implement
        let cfg = test_cfg();
        let job = build_job(&ctx, &stage, &cfg);
        let agent = &job.spec.unwrap().template.spec.unwrap().containers[0];
        let mounts = agent.volume_mounts.as_ref().unwrap();
        let claude_mount = mounts
            .iter()
            .find(|m| m.mount_path == "/secrets/claude-creds");
        assert!(
            claude_mount.is_some(),
            "Claude credentials should be mounted at /secrets/claude-creds for implement"
        );
        assert_eq!(claude_mount.unwrap().read_only, Some(true));
        // ~/.claude is NOT mounted directly (entrypoint copies from /secrets/claude-creds)
        assert!(
            !mounts.iter().any(|m| m.mount_path == "/home/agent/.claude"),
            "~/.claude should not be directly mounted; credentials are copied by entrypoint"
        );
    }

    #[test]
    fn test_build_job_review_has_claude_session() {
        // Claude credentials are mounted at /secrets/claude-creds for review stage
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
        assert!(
            mounts
                .iter()
                .any(|m| m.mount_path == "/secrets/claude-creds")
        );
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

        // Sidecar has secret mounts (lives in init_containers as native sidecar)
        let sidecar = find_sidecar(&job);
        let sidecar_mounts = sidecar.volume_mounts.as_ref().unwrap();
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

        // Agent does NOT have model-credentials or ssh-key mounts (FR-26).
        // /secrets/claude-creds is allowed (read-only Claude credentials for the agent).
        let pod_spec = job.spec.unwrap().template.spec.unwrap();
        let agent_mounts = pod_spec.containers[0].volume_mounts.as_ref().unwrap();
        assert!(
            !agent_mounts
                .iter()
                .any(|m| m.mount_path == "/secrets/model-credentials"
                    || m.mount_path == "/secrets/ssh-key")
        );
    }

    #[test]
    fn test_build_job_sidecar_probes() {
        let ctx = test_ctx();
        let stage = test_stage();
        let cfg = test_cfg();
        let job = build_job(&ctx, &stage, &cfg);
        let sidecar = find_sidecar(&job);

        // FR-22: Readiness probe on :9093/healthz
        let readiness = sidecar.readiness_probe.as_ref().unwrap();
        let http = readiness.http_get.as_ref().unwrap();
        assert_eq!(http.port, IntOrString::Int(9093));
        assert_eq!(http.path.as_deref(), Some("/healthz"));

        // FR-22: Startup probe on :9093/healthz — gates the agent container start
        // when the sidecar is a native sidecar (initContainer with restartPolicy: Always).
        let startup = sidecar
            .startup_probe
            .as_ref()
            .expect("native sidecar must have a startupProbe");
        let http = startup.http_get.as_ref().unwrap();
        assert_eq!(http.port, IntOrString::Int(9093));
        assert_eq!(http.path.as_deref(), Some("/healthz"));
        // Generous failure_threshold * period covers slow image pulls.
        assert!(startup.failure_threshold.unwrap_or(0) >= 30);

        // No liveness probe — kubelet must not restart the sidecar mid-job (issue #53).
        assert!(sidecar.liveness_probe.is_none());

        // Native sidecar marker
        assert_eq!(sidecar.restart_policy.as_deref(), Some("Always"));
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
        cfg.image_pull_secret = Some("nautiloop-registry-creds".to_string());
        let job = build_job(&ctx, &stage, &cfg);
        let pod_spec = job.spec.unwrap().template.spec.unwrap();
        let secrets = pod_spec.image_pull_secrets.unwrap();
        assert_eq!(secrets.len(), 1);
        assert_eq!(secrets[0].name, "nautiloop-registry-creds");
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
    fn test_build_job_volumes_count() {
        let ctx = test_ctx();
        let stage = test_stage();
        let cfg = test_cfg();
        let job = build_job(&ctx, &stage, &cfg);
        let volumes = job.spec.unwrap().template.spec.unwrap().volumes.unwrap();
        // worktree, sessions, cache, output, shared, tmpdir, home, model-credentials, ssh-key, ssh-known-hosts, claude-session (implement stage)
        assert_eq!(volumes.len(), 11);
    }

    // =========================================================================
    // Cache config tests (NFR-3)
    // =========================================================================

    #[test]
    fn test_cache_env_vars_on_implement() {
        // NFR-3: [cache.env] with N entries produces N EnvVar entries on implement pods.
        let ctx = test_ctx();
        let stage = test_stage(); // implement
        let mut cfg = test_cfg();
        let mut env = std::collections::HashMap::new();
        env.insert("FOO".to_string(), "/cache/foo".to_string());
        env.insert("BAR".to_string(), "/cache/bar".to_string());
        env.insert("BAZ".to_string(), "/cache/baz".to_string());
        cfg.cache = CacheConfig {
            disabled: false,
            env,
        };

        let job = build_job(&ctx, &stage, &cfg);
        let agent = &job.spec.unwrap().template.spec.unwrap().containers[0];
        let env_vars = agent.env.as_ref().unwrap();

        let find_env = |name: &str| -> Option<String> {
            env_vars
                .iter()
                .find(|e| e.name == name)
                .and_then(|e| e.value.clone())
        };

        assert_eq!(find_env("FOO").unwrap(), "/cache/foo");
        assert_eq!(find_env("BAR").unwrap(), "/cache/bar");
        assert_eq!(find_env("BAZ").unwrap(), "/cache/baz");
        // Sccache defaults should NOT be present (explicit config overrides)
        assert!(find_env("RUSTC_WRAPPER").is_none());
    }

    #[test]
    fn test_cache_env_vars_not_on_review() {
        // NFR-3: Zero cache env vars on review/audit/test stages.
        let ctx = test_ctx();
        let stage = StageConfig {
            name: "review".to_string(),
            timeout: Duration::from_secs(900),
            ..Default::default()
        };
        let mut cfg = test_cfg();
        let mut env = std::collections::HashMap::new();
        env.insert("FOO".to_string(), "/cache/foo".to_string());
        cfg.cache = CacheConfig {
            disabled: false,
            env,
        };

        let job = build_job(&ctx, &stage, &cfg);
        let agent = &job.spec.unwrap().template.spec.unwrap().containers[0];
        let env_vars = agent.env.as_ref().unwrap();

        // FOO should not be in env for review stage
        assert!(
            !env_vars.iter().any(|e| e.name == "FOO"),
            "cache env vars must not appear on review stage"
        );
    }

    #[test]
    fn test_cache_env_vars_on_revise() {
        // NFR-3: Cache env vars appear on revise stage (same as implement).
        let ctx = test_ctx();
        let stage = StageConfig {
            name: "revise".to_string(),
            timeout: Duration::from_secs(900),
            ..Default::default()
        };
        let mut cfg = test_cfg();
        let mut env = std::collections::HashMap::new();
        env.insert("FOO".to_string(), "/cache/foo".to_string());
        cfg.cache = CacheConfig {
            disabled: false,
            env,
        };

        let job = build_job(&ctx, &stage, &cfg);
        let agent = &job.spec.unwrap().template.spec.unwrap().containers[0];
        let env_vars = agent.env.as_ref().unwrap();

        let find_env = |name: &str| -> Option<String> {
            env_vars
                .iter()
                .find(|e| e.name == name)
                .and_then(|e| e.value.clone())
        };

        assert_eq!(find_env("FOO").unwrap(), "/cache/foo");
    }

    #[test]
    fn test_cache_disabled_skips_mount_and_env() {
        // NFR-3: [cache] disabled = true skips both mount and env vars.
        let ctx = test_ctx();
        let stage = test_stage(); // implement
        let mut cfg = test_cfg();
        let mut env = std::collections::HashMap::new();
        env.insert("FOO".to_string(), "/cache/foo".to_string());
        cfg.cache = CacheConfig {
            disabled: true,
            env,
        };

        let job = build_job(&ctx, &stage, &cfg);
        let pod_spec = job.spec.unwrap().template.spec.unwrap();
        let agent = &pod_spec.containers[0];
        let env_vars = agent.env.as_ref().unwrap();
        let mounts = agent.volume_mounts.as_ref().unwrap();
        let volumes = pod_spec.volumes.as_ref().unwrap();

        // No FOO env var
        assert!(
            !env_vars.iter().any(|e| e.name == "FOO"),
            "cache env vars must not appear when disabled"
        );

        // No /cache mount
        assert!(
            !mounts.iter().any(|m| m.mount_path == "/cache"),
            "/cache mount must not appear when disabled"
        );

        // No cache volume
        assert!(
            !volumes.iter().any(|v| v.name == "cache"),
            "cache volume must not appear when disabled"
        );
    }

    #[test]
    fn test_cache_empty_env_produces_no_cache_vars() {
        // NFR-3: [cache] present with empty env produces zero cache env vars.
        let ctx = test_ctx();
        let stage = test_stage(); // implement
        let mut cfg = test_cfg();
        cfg.cache = CacheConfig {
            disabled: false,
            env: std::collections::HashMap::new(),
        };

        let job = build_job(&ctx, &stage, &cfg);
        let agent = &job.spec.unwrap().template.spec.unwrap().containers[0];
        let env_vars = agent.env.as_ref().unwrap();

        // No sccache defaults should be present (explicit empty cache config)
        assert!(
            !env_vars.iter().any(|e| e.name == "RUSTC_WRAPPER"),
            "sccache defaults must not appear when [cache] is explicit with empty env"
        );
        assert!(
            !env_vars.iter().any(|e| e.name == "SCCACHE_DIR"),
            "sccache defaults must not appear when [cache] is explicit with empty env"
        );

        // /cache mount should still be present (disabled=false)
        let mounts = agent.volume_mounts.as_ref().unwrap();
        assert!(
            mounts.iter().any(|m| m.mount_path == "/cache"),
            "/cache mount should be present when disabled=false"
        );
    }

    #[test]
    fn test_sccache_defaults_on_implement() {
        // NFR-3: Default sccache env vars appear on implement when using defaults.
        let ctx = test_ctx();
        let stage = test_stage(); // implement
        let cfg = test_cfg(); // Uses CacheConfig::sccache_defaults()

        let job = build_job(&ctx, &stage, &cfg);
        let agent = &job.spec.unwrap().template.spec.unwrap().containers[0];
        let env_vars = agent.env.as_ref().unwrap();

        let find_env = |name: &str| -> Option<String> {
            env_vars
                .iter()
                .find(|e| e.name == name)
                .and_then(|e| e.value.clone())
        };

        // Sccache defaults
        assert_eq!(find_env("RUSTC_WRAPPER").unwrap(), "sccache");
        assert_eq!(find_env("SCCACHE_DIR").unwrap(), "/cache/sccache");
        assert_eq!(find_env("SCCACHE_CACHE_SIZE").unwrap(), "15G");
        assert_eq!(find_env("SCCACHE_IDLE_TIMEOUT").unwrap(), "0");
    }

    #[test]
    fn test_cache_mount_path_is_cache() {
        // FR-2a: Mount at /cache, not /cache/sccache.
        let ctx = test_ctx();
        let stage = test_stage(); // implement
        let cfg = test_cfg();

        let job = build_job(&ctx, &stage, &cfg);
        let agent = &job.spec.unwrap().template.spec.unwrap().containers[0];
        let mounts = agent.volume_mounts.as_ref().unwrap();
        let cache_mount = mounts.iter().find(|m| m.name == "cache").unwrap();
        assert_eq!(cache_mount.mount_path, "/cache");
    }

    #[test]
    fn test_cache_volume_name_is_nautiloop_cache() {
        // FR-1a: PVC named nautiloop-cache.
        let ctx = test_ctx();
        let stage = test_stage();
        let cfg = test_cfg();

        let job = build_job(&ctx, &stage, &cfg);
        let volumes = job.spec.unwrap().template.spec.unwrap().volumes.unwrap();
        let cache_vol = volumes.iter().find(|v| v.name == "cache").unwrap();
        let pvc = cache_vol.persistent_volume_claim.as_ref().unwrap();
        assert_eq!(pvc.claim_name, "nautiloop-cache");
    }

    #[test]
    fn test_cache_not_mounted_on_test_stage() {
        // FR-2a: Test stages do NOT get the /cache mount.
        let ctx = test_ctx();
        let stage = StageConfig {
            name: "test".to_string(),
            timeout: Duration::from_secs(1800),
            ..Default::default()
        };
        let cfg = test_cfg();

        let job = build_job(&ctx, &stage, &cfg);
        let agent = &job.spec.unwrap().template.spec.unwrap().containers[0];
        let mounts = agent.volume_mounts.as_ref().unwrap();
        assert!(
            !mounts.iter().any(|m| m.mount_path == "/cache"),
            "/cache mount must not appear on test stage"
        );
    }
}
