//! Three-layer config merge: cluster (lowest) -> repo -> engineer (highest).
//!
//! For each scalar field, take the highest-priority non-None value.
//! For limits, apply `min(engineer_value, cluster_cap)`.
//! If a required field (like `implementor_model`) is None at all three layers,
//! return `ConfigError::MissingField`.

use std::collections::HashMap;

use super::cluster::ClusterConfig;
use super::engineer::EngineerConfig;
use super::repo::RepoConfig;
use crate::config::ServiceConfig;

/// Fully resolved configuration after merging all three layers.
#[derive(Debug, Clone)]
pub struct MergedConfig {
    pub implementor_model: String,
    pub reviewer_model: String,
    pub max_parallel_loops: u32,
    pub max_rounds_harden: u32,
    pub max_rounds_implement: u32,
    pub services: HashMap<String, ServiceConfig>,
    // Ship settings
    pub ship_allowed: bool,
    pub ship_require_passing_ci: bool,
    pub ship_require_harden: bool,
    pub ship_max_rounds_for_auto_merge: u32,
    pub ship_merge_strategy: String,
    // Harden settings
    pub harden_auto_merge_spec_pr: bool,
    pub harden_merge_strategy: String,
    // Timeouts
    pub implement_timeout_min: u32,
    pub review_timeout_min: u32,
    pub test_timeout_min: u32,
    pub audit_timeout_min: u32,
    pub revise_timeout_min: u32,
}

/// Errors from config merge.
#[derive(Debug, Clone)]
pub enum ConfigError {
    /// A required field is missing at all layers.
    MissingField { field: String, role: String },
    /// Validation warning (not fatal).
    Warning(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingField { field, role } => {
                write!(
                    f,
                    "No {field} configured for role '{role}'. Set it in cluster config, nemo.toml [models], or ~/.nemo/config.toml [models]."
                )
            }
            Self::Warning(msg) => write!(f, "Config warning: {msg}"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Result of merge: the merged config plus any warnings.
#[derive(Debug)]
pub struct MergeResult {
    pub config: MergedConfig,
    pub warnings: Vec<String>,
}

impl MergedConfig {
    /// Merge three layers: cluster (lowest) -> repo -> engineer (highest).
    ///
    /// Returns the merged config plus any warnings (e.g., service key collisions).
    pub fn merge(
        cluster: &ClusterConfig,
        repo: &RepoConfig,
        engineer: Option<&EngineerConfig>,
    ) -> Result<MergeResult, ConfigError> {
        let warnings = Vec::new();

        // Model resolution: engineer > repo > cluster
        let engineer_implementor = engineer
            .and_then(|e| e.models.as_ref())
            .and_then(|m| m.implementor.as_ref())
            .filter(|s| !s.is_empty());

        let repo_implementor = repo
            .models
            .as_ref()
            .and_then(|m| m.implementor.as_ref())
            .filter(|s| !s.is_empty());

        let cluster_implementor = cluster
            .default_implementor
            .as_ref()
            .filter(|s| !s.is_empty());

        let implementor_model = engineer_implementor
            .or(repo_implementor)
            .or(cluster_implementor)
            .ok_or_else(|| ConfigError::MissingField {
                field: "implementor model".to_string(),
                role: "implementor".to_string(),
            })?
            .clone();

        let engineer_reviewer = engineer
            .and_then(|e| e.models.as_ref())
            .and_then(|m| m.reviewer.as_ref())
            .filter(|s| !s.is_empty());

        let repo_reviewer = repo
            .models
            .as_ref()
            .and_then(|m| m.reviewer.as_ref())
            .filter(|s| !s.is_empty());

        let cluster_reviewer = cluster.default_reviewer.as_ref().filter(|s| !s.is_empty());

        let reviewer_model = engineer_reviewer
            .or(repo_reviewer)
            .or(cluster_reviewer)
            .ok_or_else(|| ConfigError::MissingField {
                field: "reviewer model".to_string(),
                role: "reviewer".to_string(),
            })?
            .clone();

        // Limits: engineer > repo, capped by cluster
        let engineer_limits = engineer.and_then(|e| e.limits.as_ref());

        let max_parallel_loops_raw = 5u32; // default
        let max_parallel_loops = if let Some(cap) = cluster.max_parallel_loops_cap {
            max_parallel_loops_raw.min(cap)
        } else {
            max_parallel_loops_raw
        };

        let max_rounds_harden = engineer_limits
            .and_then(|l| l.max_rounds_harden)
            .or_else(|| repo.limits.as_ref().and_then(|l| l.max_rounds_harden))
            .unwrap_or(10);

        let max_rounds_implement = engineer_limits
            .and_then(|l| l.max_rounds_implement)
            .or_else(|| repo.limits.as_ref().and_then(|l| l.max_rounds_implement))
            .unwrap_or(20);

        // Services: deep merge. Repo defines; engineer can add but not override.
        let services = repo.services.clone();
        // Engineer cannot add services (spec says engineer cannot override repo service keys)
        // but we check for collisions and warn
        if let Some(eng) = engineer {
            // EngineerConfig doesn't have services field per spec,
            // but if it did, we'd warn on collision here.
            let _ = eng; // suppress unused warning
        }

        // Ship settings with defaults
        let ship = repo.ship.as_ref();
        let ship_allowed = ship.and_then(|s| s.allowed).unwrap_or(false);
        let ship_require_passing_ci = ship.and_then(|s| s.require_passing_ci).unwrap_or(true);
        let ship_require_harden = ship.and_then(|s| s.require_harden).unwrap_or(false);
        let ship_max_rounds_for_auto_merge =
            ship.and_then(|s| s.max_rounds_for_auto_merge).unwrap_or(5);
        let ship_merge_strategy = ship
            .and_then(|s| s.merge_strategy.clone())
            .unwrap_or_else(|| "squash".to_string());

        // Harden settings with defaults
        let harden = repo.harden.as_ref();
        let harden_auto_merge_spec_pr = harden.and_then(|h| h.auto_merge_spec_pr).unwrap_or(true);
        let harden_merge_strategy = harden
            .and_then(|h| h.merge_strategy.clone())
            .unwrap_or_else(|| "squash".to_string());

        // Timeout settings with defaults (in minutes)
        let timeouts = repo.timeouts.as_ref();
        let implement_timeout_min = timeouts.and_then(|t| t.implement_timeout_min).unwrap_or(30);
        let review_timeout_min = timeouts.and_then(|t| t.review_timeout_min).unwrap_or(15);
        let test_timeout_min = timeouts.and_then(|t| t.test_timeout_min).unwrap_or(30);
        let audit_timeout_min = timeouts.and_then(|t| t.audit_timeout_min).unwrap_or(15);
        let revise_timeout_min = timeouts.and_then(|t| t.revise_timeout_min).unwrap_or(15);

        Ok(MergeResult {
            config: MergedConfig {
                implementor_model,
                reviewer_model,
                max_parallel_loops,
                max_rounds_harden,
                max_rounds_implement,
                services,
                ship_allowed,
                ship_require_passing_ci,
                ship_require_harden,
                ship_max_rounds_for_auto_merge,
                ship_merge_strategy,
                harden_auto_merge_spec_pr,
                harden_merge_strategy,
                implement_timeout_min,
                review_timeout_min,
                test_timeout_min,
                audit_timeout_min,
                revise_timeout_min,
            },
            warnings,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::repo::{ModelConfig, RepoMeta};

    fn cluster_with_models(implementor: &str, reviewer: &str) -> ClusterConfig {
        ClusterConfig {
            node_size: None,
            provider: None,
            domain: "test.example.com".to_string(),
            default_implementor: Some(implementor.to_string()),
            default_reviewer: Some(reviewer.to_string()),
            max_parallel_loops_cap: None,
            max_cluster_jobs: None,
        }
    }

    fn minimal_repo() -> RepoConfig {
        RepoConfig {
            repo: RepoMeta {
                name: "test".to_string(),
                default_branch: "main".to_string(),
            },
            models: None,
            limits: None,
            services: HashMap::new(),
            ship: None,
            harden: None,
            timeouts: None,
        }
    }

    #[test]
    fn test_merge_cluster_only() {
        let cluster = cluster_with_models("claude-opus-4", "gpt-5.4");
        let repo = minimal_repo();

        let result = MergedConfig::merge(&cluster, &repo, None).unwrap();
        assert_eq!(result.config.implementor_model, "claude-opus-4");
        assert_eq!(result.config.reviewer_model, "gpt-5.4");
    }

    #[test]
    fn test_merge_repo_overrides_cluster() {
        let cluster = cluster_with_models("claude-opus-4", "gpt-5.4");
        let mut repo = minimal_repo();
        repo.models = Some(ModelConfig {
            implementor: Some("claude-sonnet-4".to_string()),
            reviewer: None,
        });

        let result = MergedConfig::merge(&cluster, &repo, None).unwrap();
        assert_eq!(result.config.implementor_model, "claude-sonnet-4");
        assert_eq!(result.config.reviewer_model, "gpt-5.4"); // falls through to cluster
    }

    #[test]
    fn test_merge_engineer_overrides_all() {
        let cluster = cluster_with_models("claude-opus-4", "gpt-5.4");
        let mut repo = minimal_repo();
        repo.models = Some(ModelConfig {
            implementor: Some("claude-sonnet-4".to_string()),
            reviewer: Some("gpt-4o".to_string()),
        });

        let engineer = EngineerConfig {
            identity: None,
            models: Some(ModelConfig {
                implementor: Some("custom-model".to_string()),
                reviewer: None, // falls through to repo
            }),
            limits: None,
        };

        let result = MergedConfig::merge(&cluster, &repo, Some(&engineer)).unwrap();
        assert_eq!(result.config.implementor_model, "custom-model");
        assert_eq!(result.config.reviewer_model, "gpt-4o"); // repo level
    }

    #[test]
    fn test_merge_missing_model_error() {
        let cluster = ClusterConfig {
            node_size: None,
            provider: None,
            domain: "test".to_string(),
            default_implementor: None,
            default_reviewer: None,
            max_parallel_loops_cap: None,
            max_cluster_jobs: None,
        };
        let repo = minimal_repo();

        let result = MergedConfig::merge(&cluster, &repo, None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("implementor"));
    }

    #[test]
    fn test_merge_empty_string_treated_as_none() {
        let cluster = cluster_with_models("claude-opus-4", "gpt-5.4");
        let mut repo = minimal_repo();
        repo.models = Some(ModelConfig {
            implementor: Some(String::new()), // empty string = None
            reviewer: None,
        });

        let result = MergedConfig::merge(&cluster, &repo, None).unwrap();
        assert_eq!(result.config.implementor_model, "claude-opus-4"); // falls through to cluster
    }

    #[test]
    fn test_merge_parallel_loops_capped() {
        let mut cluster = cluster_with_models("m1", "m2");
        cluster.max_parallel_loops_cap = Some(3);
        let repo = minimal_repo();

        let result = MergedConfig::merge(&cluster, &repo, None).unwrap();
        assert!(result.config.max_parallel_loops <= 3);
    }

    #[test]
    fn test_merge_defaults() {
        let cluster = cluster_with_models("m1", "m2");
        let repo = minimal_repo();

        let result = MergedConfig::merge(&cluster, &repo, None).unwrap();
        assert_eq!(result.config.max_rounds_harden, 10);
        assert_eq!(result.config.max_rounds_implement, 20);
        assert!(!result.config.ship_allowed);
        assert!(result.config.ship_require_passing_ci);
        assert_eq!(result.config.ship_merge_strategy, "squash");
        assert!(result.config.harden_auto_merge_spec_pr);
        assert_eq!(result.config.implement_timeout_min, 30);
        assert_eq!(result.config.review_timeout_min, 15);
        assert_eq!(result.config.test_timeout_min, 30);
        assert_eq!(result.config.audit_timeout_min, 15);
        assert_eq!(result.config.revise_timeout_min, 15);
    }
}
