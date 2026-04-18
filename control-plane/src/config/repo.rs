//! Repo-level configuration loaded from `nemo.toml`.
//!
//! Parsed from the monorepo root. The CLI validates `nemo.toml` locally before
//! `nemo submit` (fail fast). The API revalidates on receipt. Missing file at
//! the repo level is an error for `nemo submit`.

use serde::Deserialize;
use std::collections::HashMap;

use crate::config::ServiceConfig;

/// Cache configuration from `[cache]` section in nemo.toml.
///
/// When `[cache]` is absent from nemo.toml, `RepoConfig.cache` is `None`,
/// which triggers sccache default injection at the config resolution layer.
/// When `[cache]` is present but `[cache.env]` is empty/absent, no cache
/// env vars are injected (sccache defaults do NOT apply).
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct CacheConfig {
    /// If true, skip the /cache mount entirely and set no cache env vars.
    #[serde(default)]
    pub disabled: bool,
    /// Every key becomes an env var on implement/revise agent pods.
    /// Values are passed through verbatim.
    #[serde(default)]
    pub env: HashMap<String, String>,
}

impl CacheConfig {
    /// Sccache defaults injected when `[cache]` is absent from nemo.toml.
    /// Byte-identical to #130 behavior.
    pub fn sccache_defaults() -> Self {
        let mut env = HashMap::new();
        env.insert("RUSTC_WRAPPER".to_string(), "sccache".to_string());
        env.insert("SCCACHE_DIR".to_string(), "/cache/sccache".to_string());
        env.insert("SCCACHE_CACHE_SIZE".to_string(), "15G".to_string());
        env.insert("SCCACHE_IDLE_TIMEOUT".to_string(), "0".to_string());
        Self {
            disabled: false,
            env,
        }
    }
}

/// Repo metadata from `[repo]` section.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoMeta {
    pub name: String,
    pub default_branch: String,
}

/// Model config from `[models]` section.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelConfig {
    #[serde(default)]
    pub implementor: Option<String>,
    #[serde(default)]
    pub reviewer: Option<String>,
}

/// Limits config from `[limits]` section.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LimitsConfig {
    #[serde(default)]
    pub max_rounds_harden: Option<u32>,
    #[serde(default)]
    pub max_rounds_implement: Option<u32>,
    #[serde(default)]
    pub max_concurrent_test_jvm: Option<u32>,
}

/// Ship configuration from `[ship]` section.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ShipConfig {
    #[serde(default)]
    pub allowed: Option<bool>,
    #[serde(default)]
    pub require_passing_ci: Option<bool>,
    #[serde(default)]
    pub require_harden: Option<bool>,
    #[serde(default)]
    pub max_rounds_for_auto_merge: Option<u32>,
    #[serde(default)]
    pub merge_strategy: Option<String>,
}

/// Harden configuration from `[harden]` section.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HardenConfig {
    #[serde(default)]
    pub auto_merge_spec_pr: Option<bool>,
    #[serde(default)]
    pub merge_strategy: Option<String>,
}

/// Timeouts configuration from `[timeouts]` section.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TimeoutsConfig {
    #[serde(default)]
    pub implement_timeout_min: Option<u32>,
    #[serde(default)]
    pub review_timeout_min: Option<u32>,
    #[serde(default)]
    pub test_timeout_min: Option<u32>,
    #[serde(default)]
    pub audit_timeout_min: Option<u32>,
    #[serde(default)]
    pub revise_timeout_min: Option<u32>,
}

/// Complete repo configuration from `nemo.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    pub repo: RepoMeta,
    #[serde(default)]
    pub models: Option<ModelConfig>,
    #[serde(default)]
    pub limits: Option<LimitsConfig>,
    #[serde(default)]
    pub services: HashMap<String, ServiceConfig>,
    #[serde(default)]
    pub ship: Option<ShipConfig>,
    #[serde(default)]
    pub harden: Option<HardenConfig>,
    #[serde(default)]
    pub timeouts: Option<TimeoutsConfig>,
    /// Cache configuration. `None` = absent from nemo.toml → sccache defaults.
    /// `Some` with empty env → no cache env vars injected.
    /// `#[serde(default)]` is redundant here (`Option` defaults to `None`), but
    /// kept for consistency with other optional fields on `RepoConfig`. The
    /// absent-vs-present distinction required by FR-3b is handled by
    /// `NautiloopConfig.cache` (which is `Option` without `#[serde(default)]`).
    #[serde(default)]
    pub cache: Option<CacheConfig>,
}

impl RepoConfig {
    /// Parse from a TOML string.
    pub fn parse(content: &str) -> Result<Self, String> {
        toml::from_str(content).map_err(|e| format!("Failed to parse nemo.toml: {e}"))
    }

    /// Load from a file path.
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
        Self::parse(&content)
    }
}

// =============================================================================
// Service detection for `nemo init` (FR-19)
// =============================================================================

/// Marker file to service type mapping for `nemo init` detection.
pub struct ServiceMarker {
    pub filename: &'static str,
    pub service_type: &'static str,
    pub default_test: &'static str,
}

/// Known service markers for auto-detection.
pub const SERVICE_MARKERS: &[ServiceMarker] = &[
    ServiceMarker {
        filename: "Cargo.toml",
        service_type: "rust",
        default_test: "cargo test",
    },
    ServiceMarker {
        filename: "package.json",
        service_type: "node",
        default_test: "npm test",
    },
    ServiceMarker {
        filename: "go.mod",
        service_type: "go",
        default_test: "go test ./...",
    },
    ServiceMarker {
        filename: "pyproject.toml",
        service_type: "python",
        default_test: "pytest",
    },
    ServiceMarker {
        filename: "build.sbt",
        service_type: "jvm",
        default_test: "sbt test",
    },
    ServiceMarker {
        filename: "foundry.toml",
        service_type: "solidity",
        default_test: "forge test",
    },
    ServiceMarker {
        filename: "composer.json",
        service_type: "php",
        default_test: "composer test",
    },
    ServiceMarker {
        filename: "Makefile",
        service_type: "generic",
        default_test: "make test",
    },
];

/// Detect services in a directory tree up to the given depth.
///
/// Returns a map of service name -> ServiceConfig.
pub fn detect_services(root: &std::path::Path, max_depth: u32) -> HashMap<String, ServiceConfig> {
    let mut services = HashMap::new();
    detect_services_recursive(root, root, 0, max_depth, &mut services);
    services
}

fn detect_services_recursive(
    root: &std::path::Path,
    dir: &std::path::Path,
    depth: u32,
    max_depth: u32,
    services: &mut HashMap<String, ServiceConfig>,
) {
    if depth > max_depth {
        return;
    }

    for marker in SERVICE_MARKERS {
        if dir.join(marker.filename).exists() {
            // Use relative path as service name to avoid collisions between
            // same-basename services at different paths (e.g., packages/a/lib vs packages/b/lib)
            let name = if dir == root {
                root.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("root")
                    .to_string()
            } else {
                let rel = dir
                    .strip_prefix(root)
                    .unwrap_or(dir)
                    .to_string_lossy()
                    .replace('/', "-");
                if rel.is_empty() {
                    "root".to_string()
                } else {
                    rel
                }
            };

            let path = dir
                .strip_prefix(root)
                .unwrap_or(dir)
                .to_string_lossy()
                .to_string();
            let path = if path.is_empty() {
                ".".to_string()
            } else {
                path
            };

            services.entry(name).or_insert_with(|| ServiceConfig {
                path,
                test: marker.default_test.to_string(),
                tags: vec![marker.service_type.to_string()],
            });
            // Only use the first matching marker per directory
            break;
        }
    }

    // Recurse into subdirectories
    if depth < max_depth
        && let Ok(entries) = std::fs::read_dir(dir)
    {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                // Skip hidden directories and common non-source directories
                if !name_str.starts_with('.')
                    && name_str != "node_modules"
                    && name_str != "target"
                    && name_str != "vendor"
                {
                    detect_services_recursive(root, &entry.path(), depth + 1, max_depth, services);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_repo_config_parse() {
        let toml = r#"
            [repo]
            name = "my-project"
            default_branch = "main"

            [models]
            implementor = "claude-opus-4"
            reviewer = "gpt-5.4"

            [limits]
            max_rounds_harden = 5
            max_rounds_implement = 10

            [services.backend]
            path = "backend"
            test = "cargo test"
            tags = ["rust"]

            [services.frontend]
            path = "frontend"
            test = "npm test"

            [ship]
            allowed = true
            merge_strategy = "squash"

            [timeouts]
            implement_timeout_min = 45
        "#;

        let config = RepoConfig::parse(toml).unwrap();
        assert_eq!(config.repo.name, "my-project");
        assert_eq!(config.repo.default_branch, "main");
        assert_eq!(
            config.models.as_ref().unwrap().implementor,
            Some("claude-opus-4".to_string())
        );
        assert_eq!(config.services.len(), 2);
        assert_eq!(config.services["backend"].test, "cargo test");
        assert_eq!(config.ship.as_ref().unwrap().allowed, Some(true));
        assert_eq!(
            config.timeouts.as_ref().unwrap().implement_timeout_min,
            Some(45)
        );
    }

    #[test]
    fn test_repo_config_unknown_fields() {
        let toml = r#"
            [repo]
            name = "my-project"
            default_branch = "main"
            unknown_field = "should fail"
        "#;
        let result = RepoConfig::parse(toml);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown field"));
    }

    #[test]
    fn test_cache_config_absent_from_nemo_toml() {
        // FR-3b: When [cache] is absent, cache field is None.
        let toml = r#"
            [repo]
            name = "my-project"
            default_branch = "main"
        "#;
        let config = RepoConfig::parse(toml).unwrap();
        assert!(
            config.cache.is_none(),
            "absent [cache] section should produce None"
        );
    }

    #[test]
    fn test_cache_config_present_but_empty() {
        // FR-3b: [cache] present but [cache.env] absent → Some with empty env.
        let toml = r#"
            [repo]
            name = "my-project"
            default_branch = "main"

            [cache]
        "#;
        let config = RepoConfig::parse(toml).unwrap();
        let cache = config.cache.unwrap();
        assert!(!cache.disabled);
        assert!(cache.env.is_empty());
    }

    #[test]
    fn test_cache_config_disabled() {
        // FR-3d: [cache] disabled = true.
        let toml = r#"
            [repo]
            name = "my-project"
            default_branch = "main"

            [cache]
            disabled = true
        "#;
        let config = RepoConfig::parse(toml).unwrap();
        let cache = config.cache.unwrap();
        assert!(cache.disabled);
    }

    #[test]
    fn test_cache_config_with_env_vars() {
        // FR-3a: [cache.env] with key-value pairs.
        let toml = r#"
            [repo]
            name = "my-project"
            default_branch = "main"

            [cache]

            [cache.env]
            RUSTC_WRAPPER = "sccache"
            SCCACHE_DIR = "/cache/sccache"
            NPM_CONFIG_CACHE = "/cache/npm"
        "#;
        let config = RepoConfig::parse(toml).unwrap();
        let cache = config.cache.unwrap();
        assert!(!cache.disabled);
        assert_eq!(cache.env.len(), 3);
        assert_eq!(cache.env["RUSTC_WRAPPER"], "sccache");
        assert_eq!(cache.env["SCCACHE_DIR"], "/cache/sccache");
        assert_eq!(cache.env["NPM_CONFIG_CACHE"], "/cache/npm");
    }

    #[test]
    fn test_sccache_defaults() {
        let defaults = CacheConfig::sccache_defaults();
        assert!(!defaults.disabled);
        assert_eq!(defaults.env.len(), 4);
        assert_eq!(defaults.env["RUSTC_WRAPPER"], "sccache");
        assert_eq!(defaults.env["SCCACHE_DIR"], "/cache/sccache");
        assert_eq!(defaults.env["SCCACHE_CACHE_SIZE"], "15G");
        assert_eq!(defaults.env["SCCACHE_IDLE_TIMEOUT"], "0");
    }

    #[test]
    fn test_detect_services() {
        let temp = std::env::temp_dir().join(format!("nautiloop-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp).unwrap();

        // Create marker files
        std::fs::write(temp.join("Cargo.toml"), "").unwrap();
        let sub = temp.join("frontend");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("package.json"), "{}").unwrap();

        let services = detect_services(&temp, 2);
        assert!(services.len() >= 2);

        // Cleanup
        let _ = std::fs::remove_dir_all(&temp);
    }
}
