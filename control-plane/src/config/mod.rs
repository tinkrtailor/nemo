// Three-layer config merge modules (cluster -> repo -> engineer).
// V1: Runtime uses NautiloopConfig (flat file). The merge modules are used by
// nemo init (repo.rs service detection, via CLI) and prepared for V1.5 where
// MergedConfig will be computed per-loop at dispatch time from three layers.
pub mod cluster;
pub mod engineer;
pub mod merged;
pub mod repo;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

pub use repo::CacheConfig;

/// Repo-level configuration loaded from `nemo.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NautiloopConfig {
    #[serde(default)]
    pub limits: LimitsConfig,
    #[serde(default)]
    pub timeouts: TimeoutConfig,
    #[serde(default)]
    pub models: ModelConfig,
    #[serde(default)]
    pub cluster: ClusterConfig,
    #[serde(default)]
    pub ship: ShipConfig,
    #[serde(default)]
    pub harden: HardenMergeConfig,
    /// Service definitions: `[services.<name>]` with `path` and `test` fields.
    /// Used by the control plane to map changed file paths to services and
    /// look up test commands for the TEST stage (FR-42a, FR-42b).
    #[serde(default)]
    pub services: HashMap<String, ServiceConfig>,
    /// Observability configuration from `[observability]` in nemo.toml.
    #[serde(default)]
    pub observability: ObservabilityConfig,
    /// Cache configuration from `[cache]` section in nemo.toml (repo-level only).
    /// `None` = absent from nemo.toml → sccache defaults injected at resolution time.
    /// `Some` with empty env → no cache env vars. Uses `Option` (not `#[serde(default)]`)
    /// so absent vs present-but-empty are distinguishable (FR-3b).
    pub cache: Option<CacheConfig>,
}

/// Configuration for a single service in the monorepo.
/// Defined under `[services.<name>]` in nemo.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceConfig {
    /// Path prefix in the repo that belongs to this service.
    /// Used to map git diff paths to affected services.
    pub path: String,
    /// Shell command to run tests for this service.
    pub test: String,
    /// Optional JVM tag for elevated resource limits (FR-28).
    #[serde(default)]
    pub tags: Vec<String>,
}

impl NautiloopConfig {
    /// Load config from `NAUTILOOP_CONFIG_PATH` env var, or `./nemo.toml` (repo-local),
    /// or `/etc/nautiloop/nemo.toml` (system), or fall back to defaults.
    pub fn load() -> std::result::Result<Self, String> {
        let explicit = std::env::var("NAUTILOOP_CONFIG_PATH").ok();

        let candidates: Vec<String> = if let Some(ref explicit_path) = explicit {
            vec![explicit_path.clone()]
        } else {
            vec![
                "./nemo.toml".to_string(),
                "/etc/nautiloop/nemo.toml".to_string(),
            ]
        };

        let path = candidates
            .iter()
            .map(std::path::PathBuf::from)
            .find(|p| p.exists());

        let path = match path {
            Some(p) => p,
            None => {
                // If NAUTILOOP_CONFIG_PATH was explicitly set but doesn't exist, fail hard
                if let Some(ref explicit_path) = explicit {
                    return Err(format!(
                        "NAUTILOOP_CONFIG_PATH={explicit_path} does not exist"
                    ));
                }
                tracing::warn!("No config file found at {:?}, using defaults", candidates);
                return Ok(Self::default());
            }
        };

        let contents = std::fs::read_to_string(&path)
            .map_err(|e| format!("Failed to read config at {}: {e}", path.display()))?;

        let mut config: NautiloopConfig = toml::from_str(&contents)
            .map_err(|e| format!("Failed to parse config at {}: {e}", path.display()))?;

        // If [repo].default_branch is set in nemo.toml, use it as the runtime default.
        // This closes the loop: nemo init writes it, the runtime reads it.
        if let Ok(raw) = toml::from_str::<toml::Value>(&contents)
            && let Some(branch) = raw
                .get("repo")
                .and_then(|r| r.get("default_branch"))
                .and_then(|v| v.as_str())
        {
            config.cluster.default_branch = branch.to_string();
        }

        tracing::info!(path = %path.display(), "Loaded config");
        Ok(config)
    }

    /// Returns the remote ref for the default branch (e.g., "origin/main").
    pub fn default_remote_ref(&self) -> String {
        format!("origin/{}", self.cluster.default_branch)
    }

    /// Resolve the cache configuration for the job builder.
    ///
    /// Three cases (FR-3b):
    /// - `None` (absent `[cache]` section) → inject sccache defaults
    /// - `Some(CacheConfig { disabled: true, .. })` → no mount, no env vars
    /// - `Some(CacheConfig { disabled: false, env })` → use env as-is (may be empty)
    pub fn resolved_cache_config(&self) -> CacheConfig {
        match &self.cache {
            None => CacheConfig::sccache_defaults(),
            Some(c) => c.clone(),
        }
    }
}

/// Ship configuration from `[ship]` in nemo.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShipConfig {
    /// Enable nemo ship (default: false).
    #[serde(default)]
    pub allowed: bool,
    /// Wait for CI before merge (default: true).
    #[serde(default = "default_true")]
    pub require_passing_ci: bool,
    /// Force --harden on nemo ship (default: false).
    #[serde(default)]
    pub require_harden: bool,
    /// Max rounds for auto-merge threshold (default: 5).
    #[serde(default = "default_max_rounds_auto_merge")]
    pub max_rounds_for_auto_merge: u32,
    /// Merge strategy: squash | merge | rebase (default: squash).
    #[serde(default = "default_merge_strategy")]
    pub merge_strategy: String,
}

impl Default for ShipConfig {
    fn default() -> Self {
        Self {
            allowed: false,
            require_passing_ci: true,
            require_harden: false,
            max_rounds_for_auto_merge: default_max_rounds_auto_merge(),
            merge_strategy: default_merge_strategy(),
        }
    }
}

/// Harden merge configuration from `[harden]` in nemo.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardenMergeConfig {
    /// Merge strategy for spec PRs (default: squash).
    #[serde(default = "default_merge_strategy")]
    pub merge_strategy: String,
    /// Auto-merge the hardened spec PR (default: true).
    #[serde(default = "default_true")]
    pub auto_merge_spec_pr: bool,
}

impl Default for HardenMergeConfig {
    fn default() -> Self {
        Self {
            merge_strategy: default_merge_strategy(),
            auto_merge_spec_pr: true,
        }
    }
}

/// Observability configuration from `[observability]` in nemo.toml.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ObservabilityConfig {
    /// When true, every /pod-introspect response for an active loop is
    /// persisted to the pod_snapshots table (FR-6a). Default: false.
    #[serde(default)]
    pub record_introspection: bool,
}

fn default_true() -> bool {
    true
}
fn default_max_rounds_auto_merge() -> u32 {
    5
}
fn default_merge_strategy() -> String {
    "squash".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitsConfig {
    /// Max rounds for the harden loop.
    #[serde(default = "default_max_rounds_harden")]
    pub max_rounds_harden: u32,
    /// Max rounds for the implement loop.
    #[serde(default = "default_max_rounds_implement")]
    pub max_rounds_implement: u32,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_rounds_harden: default_max_rounds_harden(),
            max_rounds_implement: default_max_rounds_implement(),
        }
    }
}

fn default_max_rounds_harden() -> u32 {
    10
}
fn default_max_rounds_implement() -> u32 {
    15
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeoutConfig {
    /// Implement stage timeout in seconds.
    #[serde(default = "default_implement_timeout")]
    pub implement_secs: u64,
    /// Review stage timeout in seconds.
    #[serde(default = "default_review_timeout")]
    pub review_secs: u64,
    /// Test stage timeout in seconds.
    #[serde(default = "default_test_timeout")]
    pub test_secs: u64,
    /// Spec audit stage timeout in seconds.
    #[serde(default = "default_audit_timeout")]
    pub audit_secs: u64,
    /// Spec revise stage timeout in seconds.
    #[serde(default = "default_revise_timeout")]
    pub revise_secs: u64,
    /// No-output watchdog timeout in seconds.
    #[serde(default = "default_watchdog_timeout")]
    pub watchdog_secs: u64,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            implement_secs: default_implement_timeout(),
            review_secs: default_review_timeout(),
            test_secs: default_test_timeout(),
            audit_secs: default_audit_timeout(),
            revise_secs: default_revise_timeout(),
            watchdog_secs: default_watchdog_timeout(),
        }
    }
}

impl TimeoutConfig {
    pub fn implement_duration(&self) -> Duration {
        Duration::from_secs(self.implement_secs)
    }
    pub fn review_duration(&self) -> Duration {
        Duration::from_secs(self.review_secs)
    }
    pub fn test_duration(&self) -> Duration {
        Duration::from_secs(self.test_secs)
    }
    pub fn audit_duration(&self) -> Duration {
        Duration::from_secs(self.audit_secs)
    }
    pub fn revise_duration(&self) -> Duration {
        Duration::from_secs(self.revise_secs)
    }
}

fn default_implement_timeout() -> u64 {
    1800
}
fn default_review_timeout() -> u64 {
    900
}
fn default_test_timeout() -> u64 {
    1800
}
fn default_audit_timeout() -> u64 {
    900
}
fn default_revise_timeout() -> u64 {
    900
}
fn default_watchdog_timeout() -> u64 {
    900
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    /// Default implementor model.
    #[serde(default = "default_implementor")]
    pub implementor: String,
    /// Default reviewer model.
    #[serde(default = "default_reviewer")]
    pub reviewer: String,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            implementor: default_implementor(),
            reviewer: default_reviewer(),
        }
    }
}

fn default_implementor() -> String {
    "claude-opus-4".to_string()
}
fn default_reviewer() -> String {
    "gpt-5.4".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterConfig {
    /// Postgres connection URL.
    #[serde(default = "default_database_url")]
    pub database_url: String,
    /// K8s namespace for jobs.
    #[serde(default = "default_jobs_namespace")]
    pub jobs_namespace: String,
    /// K8s namespace for the control plane.
    #[serde(default = "default_system_namespace")]
    pub system_namespace: String,
    /// Agent container image.
    #[serde(default = "default_agent_image")]
    pub agent_image: String,
    /// Bare repo PVC claim name.
    #[serde(default = "default_bare_repo_pvc")]
    pub bare_repo_pvc: String,
    /// Auth sidecar container image.
    #[serde(default = "default_sidecar_image")]
    pub sidecar_image: String,
    /// Session state PVC claim name (FR-47b).
    #[serde(default = "default_sessions_pvc")]
    pub sessions_pvc: String,
    /// Image pull secret name (optional, for private registries).
    #[serde(default)]
    pub image_pull_secret: Option<String>,
    /// Git repository URL (SSH format, used for sidecar git proxy host restriction).
    #[serde(default)]
    pub git_repo_url: String,
    /// ConfigMap name containing SSH known_hosts for sidecar host key verification.
    #[serde(default = "default_ssh_known_hosts_configmap")]
    pub ssh_known_hosts_configmap: String,
    /// API server bind address.
    #[serde(default = "default_bind_addr")]
    pub bind_addr: String,
    /// API server port.
    #[serde(default = "default_port")]
    pub port: u16,
    /// Max Postgres connections.
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
    /// Reconciliation interval in seconds.
    #[serde(default = "default_reconcile_interval")]
    pub reconcile_interval_secs: u64,
    /// Default branch name for the target repo (e.g., "main", "master", "trunk").
    #[serde(default = "default_branch_name")]
    pub default_branch: String,
    /// Skip the init-iptables container. Set to true for local dev (k3d)
    /// where NET_ADMIN privileged init containers may not behave identically
    /// to production. Egress enforcement is best-effort in dev.
    #[serde(default)]
    pub skip_iptables: bool,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            database_url: default_database_url(),
            jobs_namespace: default_jobs_namespace(),
            system_namespace: default_system_namespace(),
            agent_image: default_agent_image(),
            bare_repo_pvc: default_bare_repo_pvc(),
            sidecar_image: default_sidecar_image(),
            sessions_pvc: default_sessions_pvc(),
            image_pull_secret: None,
            git_repo_url: String::new(),
            ssh_known_hosts_configmap: default_ssh_known_hosts_configmap(),
            bind_addr: default_bind_addr(),
            port: default_port(),
            max_connections: default_max_connections(),
            reconcile_interval_secs: default_reconcile_interval(),
            default_branch: default_branch_name(),
            skip_iptables: false,
        }
    }
}

fn default_database_url() -> String {
    "postgres://nautiloop:nautiloop@localhost:5432/nautiloop".to_string()
}
fn default_jobs_namespace() -> String {
    "nautiloop-jobs".to_string()
}
fn default_system_namespace() -> String {
    "nautiloop-system".to_string()
}
fn default_agent_image() -> String {
    "nautiloop-agent:latest".to_string()
}
fn default_bare_repo_pvc() -> String {
    "nautiloop-bare-repo".to_string()
}
fn default_sidecar_image() -> String {
    "nautiloop-sidecar:latest".to_string()
}
fn default_ssh_known_hosts_configmap() -> String {
    "nautiloop-ssh-known-hosts".to_string()
}
fn default_sessions_pvc() -> String {
    "nautiloop-sessions".to_string()
}
fn default_bind_addr() -> String {
    "0.0.0.0".to_string()
}
fn default_port() -> u16 {
    8080
}
fn default_max_connections() -> u32 {
    20
}
fn default_reconcile_interval() -> u64 {
    5
}
fn default_branch_name() -> String {
    "main".to_string()
}

/// Engineer-level configuration loaded from `~/.nemo/config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineerConfig {
    /// API server URL.
    pub server_url: String,
    /// Engineer name (used for submissions).
    pub engineer: String,
    /// API key for authentication.
    pub api_key: Option<String>,
}

impl Default for EngineerConfig {
    fn default() -> Self {
        Self {
            server_url: "https://localhost:8080".to_string(),
            engineer: String::new(),
            api_key: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = NautiloopConfig::default();
        assert_eq!(config.limits.max_rounds_harden, 10);
        assert_eq!(config.limits.max_rounds_implement, 15);
        assert_eq!(config.timeouts.implement_secs, 1800);
        assert_eq!(config.timeouts.review_secs, 900);
        assert_eq!(config.cluster.max_connections, 20);
        assert_eq!(config.cluster.reconcile_interval_secs, 5);
    }

    #[test]
    fn test_timeout_durations() {
        let config = TimeoutConfig::default();
        assert_eq!(config.implement_duration(), Duration::from_secs(1800));
        assert_eq!(config.review_duration(), Duration::from_secs(900));
        assert_eq!(config.test_duration(), Duration::from_secs(1800));
    }

    #[test]
    fn test_config_deserialize() {
        let toml = r#"
            [limits]
            max_rounds_harden = 5
            max_rounds_implement = 10

            [timeouts]
            implement_secs = 3600

            [models]
            implementor = "claude-sonnet-4"
            reviewer = "gpt-4o"
        "#;
        let config: NautiloopConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.limits.max_rounds_harden, 5);
        assert_eq!(config.limits.max_rounds_implement, 10);
        assert_eq!(config.timeouts.implement_secs, 3600);
        assert_eq!(config.timeouts.review_secs, 900); // default
        assert_eq!(config.models.implementor, "claude-sonnet-4");
    }

    #[test]
    fn test_services_config_deserialize() {
        let toml_str = r#"
            [services.api]
            path = "packages/api"
            test = "cargo test -p api"

            [services.web]
            path = "packages/web"
            test = "npm test"
            tags = ["jvm"]
        "#;
        let config: NautiloopConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.services.len(), 2);

        let api = &config.services["api"];
        assert_eq!(api.path, "packages/api");
        assert_eq!(api.test, "cargo test -p api");
        assert!(api.tags.is_empty());

        let web = &config.services["web"];
        assert_eq!(web.path, "packages/web");
        assert_eq!(web.test, "npm test");
        assert_eq!(web.tags, vec!["jvm"]);
    }

    #[test]
    fn test_default_config_has_no_services() {
        let config = NautiloopConfig::default();
        assert!(config.services.is_empty());
    }

    #[test]
    fn test_cluster_config_new_fields() {
        let config = ClusterConfig::default();
        assert_eq!(config.sidecar_image, "nautiloop-sidecar:latest");
        assert_eq!(config.sessions_pvc, "nautiloop-sessions");
        assert!(config.image_pull_secret.is_none());
    }

    // =========================================================================
    // Cache config resolution tests (NFR-3)
    // =========================================================================

    #[test]
    fn test_resolved_cache_absent_injects_sccache_defaults() {
        // NFR-3: absent [cache] section (None) → sccache defaults injected.
        let config = NautiloopConfig {
            cache: None,
            ..Default::default()
        };
        let resolved = config.resolved_cache_config();
        assert!(!resolved.disabled);
        assert_eq!(resolved.env.len(), 4);
        assert_eq!(resolved.env["RUSTC_WRAPPER"], "sccache");
        assert_eq!(resolved.env["SCCACHE_DIR"], "/cache/sccache");
        assert_eq!(resolved.env["SCCACHE_CACHE_SIZE"], "15G");
        assert_eq!(resolved.env["SCCACHE_IDLE_TIMEOUT"], "0");
    }

    #[test]
    fn test_resolved_cache_present_empty_no_defaults() {
        // NFR-3: [cache] present with empty env → zero cache env vars.
        // Sccache defaults do NOT apply.
        let config = NautiloopConfig {
            cache: Some(CacheConfig {
                disabled: false,
                env: HashMap::new(),
            }),
            ..Default::default()
        };
        let resolved = config.resolved_cache_config();
        assert!(!resolved.disabled);
        assert!(
            resolved.env.is_empty(),
            "explicit [cache] with empty env must not inject sccache defaults"
        );
    }

    #[test]
    fn test_resolved_cache_disabled() {
        // NFR-3: [cache] disabled = true → disabled flag set.
        let config = NautiloopConfig {
            cache: Some(CacheConfig {
                disabled: true,
                env: HashMap::new(),
            }),
            ..Default::default()
        };
        let resolved = config.resolved_cache_config();
        assert!(resolved.disabled);
    }

    #[test]
    fn test_resolved_cache_custom_env() {
        // NFR-3: [cache.env] with custom entries passes them through.
        let mut env = HashMap::new();
        env.insert("FOO".to_string(), "/cache/foo".to_string());
        let config = NautiloopConfig {
            cache: Some(CacheConfig {
                disabled: false,
                env,
            }),
            ..Default::default()
        };
        let resolved = config.resolved_cache_config();
        assert_eq!(resolved.env.len(), 1);
        assert_eq!(resolved.env["FOO"], "/cache/foo");
    }

    #[test]
    fn test_default_nautiloop_config_cache_is_none() {
        // Default NautiloopConfig has cache = None (triggers sccache defaults).
        let config = NautiloopConfig::default();
        assert!(config.cache.is_none());
    }
}
