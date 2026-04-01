//! Cluster-level configuration loaded from a K8s ConfigMap or environment variables.
//!
//! Two sources, checked in order:
//! 1. File at path `$NAUTILOOP_CLUSTER_CONFIG` (K8s ConfigMap mounted as a file)
//! 2. Environment variables: `NAUTILOOP_CLUSTER_DOMAIN`, `NAUTILOOP_CLUSTER_DEFAULT_IMPLEMENTOR`, etc.
//!
//! If the file exists, it takes precedence. Environment variables fill in any fields
//! the file doesn't set.

use serde::Deserialize;

/// Wrapper for the cluster config TOML file format.
/// The file must have a `[cluster]` section.
#[derive(Debug, Clone, Deserialize)]
pub struct ClusterFile {
    pub cluster: ClusterConfig,
}

/// Cluster-level configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct ClusterConfig {
    #[serde(default)]
    pub node_size: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    pub domain: String,
    #[serde(default)]
    pub default_implementor: Option<String>,
    #[serde(default)]
    pub default_reviewer: Option<String>,
    #[serde(default)]
    pub max_parallel_loops_cap: Option<u32>,
    #[serde(default)]
    pub max_cluster_jobs: Option<u32>,
}

impl ClusterConfig {
    /// Load cluster config from file or environment variables.
    pub fn load() -> Result<Self, ConfigLoadError> {
        // 1. Try file at $NAUTILOOP_CLUSTER_CONFIG
        if let Ok(path) = std::env::var("NAUTILOOP_CLUSTER_CONFIG") {
            let path = std::path::PathBuf::from(&path);
            if path.exists() {
                let contents =
                    std::fs::read_to_string(&path).map_err(|e| ConfigLoadError::ReadFailed {
                        path: path.display().to_string(),
                        detail: e.to_string(),
                    })?;
                let file: ClusterFile =
                    toml::from_str(&contents).map_err(|e| ConfigLoadError::ParseFailed {
                        layer: "cluster".to_string(),
                        path: path.display().to_string(),
                        detail: e.to_string(),
                    })?;
                // Merge env vars for any fields the file didn't set
                return Ok(Self::merge_with_env(file.cluster));
            }
        }

        // 2. Fall back to environment variables
        Self::from_env()
    }

    fn from_env() -> Result<Self, ConfigLoadError> {
        let domain =
            std::env::var("NAUTILOOP_CLUSTER_DOMAIN").unwrap_or_else(|_| "localhost".to_string());

        Ok(Self {
            node_size: std::env::var("NAUTILOOP_CLUSTER_NODE_SIZE").ok(),
            provider: std::env::var("NAUTILOOP_CLUSTER_PROVIDER").ok(),
            domain,
            default_implementor: std::env::var("NAUTILOOP_CLUSTER_DEFAULT_IMPLEMENTOR").ok(),
            default_reviewer: std::env::var("NAUTILOOP_CLUSTER_DEFAULT_REVIEWER").ok(),
            max_parallel_loops_cap: std::env::var("NAUTILOOP_CLUSTER_MAX_PARALLEL_LOOPS_CAP")
                .ok()
                .and_then(|v| v.parse().ok()),
            max_cluster_jobs: std::env::var("NAUTILOOP_CLUSTER_MAX_CLUSTER_JOBS")
                .ok()
                .and_then(|v| v.parse().ok()),
        })
    }

    fn merge_with_env(mut config: Self) -> Self {
        if config.node_size.is_none() {
            config.node_size = std::env::var("NAUTILOOP_CLUSTER_NODE_SIZE").ok();
        }
        if config.provider.is_none() {
            config.provider = std::env::var("NAUTILOOP_CLUSTER_PROVIDER").ok();
        }
        if config.default_implementor.is_none() {
            config.default_implementor = std::env::var("NAUTILOOP_CLUSTER_DEFAULT_IMPLEMENTOR").ok();
        }
        if config.default_reviewer.is_none() {
            config.default_reviewer = std::env::var("NAUTILOOP_CLUSTER_DEFAULT_REVIEWER").ok();
        }
        if config.max_parallel_loops_cap.is_none() {
            config.max_parallel_loops_cap = std::env::var("NAUTILOOP_CLUSTER_MAX_PARALLEL_LOOPS_CAP")
                .ok()
                .and_then(|v| v.parse().ok());
        }
        if config.max_cluster_jobs.is_none() {
            config.max_cluster_jobs = std::env::var("NAUTILOOP_CLUSTER_MAX_CLUSTER_JOBS")
                .ok()
                .and_then(|v| v.parse().ok());
        }
        config
    }
}

/// Errors during config loading.
#[derive(Debug, Clone)]
pub enum ConfigLoadError {
    ReadFailed {
        path: String,
        detail: String,
    },
    ParseFailed {
        layer: String,
        path: String,
        detail: String,
    },
}

impl std::fmt::Display for ConfigLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReadFailed { path, detail } => {
                write!(f, "Failed to read config at {path}: {detail}")
            }
            Self::ParseFailed {
                layer,
                path,
                detail,
            } => {
                write!(f, "Failed to parse {layer} config at {path}: {detail}")
            }
        }
    }
}

impl std::error::Error for ConfigLoadError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cluster_file_deserialize() {
        let toml = r#"
            [cluster]
            domain = "nautiloop.example.com"
            max_cluster_jobs = 20
            default_implementor = "claude-opus-4"
        "#;
        let file: ClusterFile = toml::from_str(toml).unwrap();
        assert_eq!(file.cluster.domain, "nautiloop.example.com");
        assert_eq!(file.cluster.max_cluster_jobs, Some(20));
        assert_eq!(
            file.cluster.default_implementor,
            Some("claude-opus-4".to_string())
        );
        assert!(file.cluster.default_reviewer.is_none());
    }

    #[test]
    fn test_cluster_config_wrapper_required() {
        // Without [cluster] wrapper, parsing should fail
        let toml = r#"
            domain = "nautiloop.example.com"
        "#;
        let result: Result<ClusterFile, _> = toml::from_str(toml);
        assert!(result.is_err());
    }
}
