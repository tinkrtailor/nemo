use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Repo-level configuration loaded from `nemo.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NemoConfig {
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
}

impl NemoConfig {
    /// Load config from `NEMO_CONFIG_PATH` env var, or `./nemo.toml` (repo-local),
    /// or `/etc/nemo/nemo.toml` (system), or fall back to defaults.
    pub fn load() -> std::result::Result<Self, String> {
        let candidates: Vec<String> = if let Ok(explicit) = std::env::var("NEMO_CONFIG_PATH") {
            vec![explicit]
        } else {
            vec![
                "./nemo.toml".to_string(),
                "/etc/nemo/nemo.toml".to_string(),
            ]
        };

        let path = candidates.iter()
            .map(std::path::PathBuf::from)
            .find(|p| p.exists());

        let path = match path {
            Some(p) => p,
            None => {
                tracing::warn!("No config file found at {:?}, using defaults", candidates);
                return Ok(Self::default());
            }
        };

        let contents = std::fs::read_to_string(&path)
            .map_err(|e| format!("Failed to read config at {}: {e}", path.display()))?;

        let config: NemoConfig = toml::from_str(&contents)
            .map_err(|e| format!("Failed to parse config at {}: {e}", path.display()))?;

        tracing::info!(path = %path.display(), "Loaded config");
        Ok(config)
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
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            database_url: default_database_url(),
            jobs_namespace: default_jobs_namespace(),
            system_namespace: default_system_namespace(),
            agent_image: default_agent_image(),
            bare_repo_pvc: default_bare_repo_pvc(),
            bind_addr: default_bind_addr(),
            port: default_port(),
            max_connections: default_max_connections(),
            reconcile_interval_secs: default_reconcile_interval(),
        }
    }
}

fn default_database_url() -> String {
    "postgres://nemo:nemo@localhost:5432/nemo".to_string()
}
fn default_jobs_namespace() -> String {
    "nemo-jobs".to_string()
}
fn default_system_namespace() -> String {
    "nemo-system".to_string()
}
fn default_agent_image() -> String {
    "nemo-agent:latest".to_string()
}
fn default_bare_repo_pvc() -> String {
    "bare-repo-pvc".to_string()
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
        let config = NemoConfig::default();
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
        let config: NemoConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.limits.max_rounds_harden, 5);
        assert_eq!(config.limits.max_rounds_implement, 10);
        assert_eq!(config.timeouts.implement_secs, 3600);
        assert_eq!(config.timeouts.review_secs, 900); // default
        assert_eq!(config.models.implementor, "claude-sonnet-4");
    }
}
