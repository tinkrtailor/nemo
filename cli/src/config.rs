use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::project_config::ModelsSection;

// ---------------------------------------------------------------------------
// Profile name validation (FR-1b)
// ---------------------------------------------------------------------------

/// Validate profile name: `^[a-zA-Z0-9][a-zA-Z0-9-]*$`
pub fn validate_profile_name(name: &str) -> Result<()> {
    if name.is_empty() {
        anyhow::bail!("Profile name must not be empty");
    }
    let first = name.as_bytes()[0];
    if !first.is_ascii_alphanumeric() {
        anyhow::bail!(
            "Profile name must start with a letter or digit, got '{name}'"
        );
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        anyhow::bail!(
            "Profile name may only contain letters, digits, and hyphens, got '{name}'"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// API key redaction (NFR-3)
// ---------------------------------------------------------------------------

/// Redact an API key for display. Keys >12 chars show first4...last4.
/// Keys ≤12 chars show `****`.
pub fn redact_api_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() > 12 {
        let prefix: String = chars[..4].iter().collect();
        let suffix: String = chars[chars.len() - 4..].iter().collect();
        format!("{prefix}...{suffix}")
    } else {
        "****".to_string()
    }
}

// ---------------------------------------------------------------------------
// Structs
// ---------------------------------------------------------------------------

/// Helm TUI settings from `[helm]` section.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HelmConfig {
    /// Theme: "dark", "light", or "high-contrast". Default: "dark".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Enable desktop notifications for convergence events. Default: false.
    #[serde(default)]
    pub desktop_notifications: bool,
}

/// Per-profile connection configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileConfig {
    pub server_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    pub engineer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

/// Top-level nemo config (profile-aware).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NemoConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_profile: Option<String>,
    #[serde(default)]
    pub profiles: BTreeMap<String, ProfileConfig>,
    #[serde(default)]
    pub helm: HelmConfig,
    #[serde(default)]
    pub models: ModelsSection,
}

impl NemoConfig {
    /// Resolve the active profile name from the precedence chain:
    /// `--profile` flag > `NAUTILOOP_PROFILE` env > `current_profile`.
    /// Returns the resolved name or an error.
    pub fn resolve_profile_name(&self, flag: Option<&str>) -> Result<String> {
        // 1. --profile flag
        if let Some(name) = flag {
            if !self.profiles.contains_key(name) {
                let available = self.profile_names_sorted().join(", ");
                anyhow::bail!(
                    "Profile '{name}' not found. Available: {available}."
                );
            }
            return Ok(name.to_string());
        }

        // 2. NAUTILOOP_PROFILE env var (empty string = unset)
        if let Ok(env_name) = std::env::var("NAUTILOOP_PROFILE") {
            let env_name = env_name.trim().to_string();
            if !env_name.is_empty() {
                if !self.profiles.contains_key(&env_name) {
                    let available = self.profile_names_sorted().join(", ");
                    anyhow::bail!(
                        "Profile '{env_name}' not found. Available: {available}."
                    );
                }
                return Ok(env_name);
            }
        }

        // 3. current_profile from config
        match &self.current_profile {
            Some(name) if !name.is_empty() => {
                if !self.profiles.contains_key(name) {
                    let available = self.profile_names_sorted().join(", ");
                    anyhow::bail!(
                        "Active profile '{name}' not found. Available: {available}. Run 'nemo use-profile <name>' to fix."
                    );
                }
                Ok(name.clone())
            }
            _ => {
                if self.profiles.is_empty() {
                    anyhow::bail!(
                        "No profiles configured. Run 'nemo profile add <name> --server <url> --api-key <key> --engineer <id>' to get started."
                    );
                }
                anyhow::bail!(
                    "No active profile set. Run 'nemo use-profile <name>' to select one."
                );
            }
        }
    }

    /// Get the active profile (after precedence resolution).
    pub fn active_profile(&self, profile_flag: Option<&str>) -> Result<(&str, &ProfileConfig)> {
        let name = self.resolve_profile_name(profile_flag)?;
        let profile = self.profiles.get(&name).unwrap(); // safe: resolve_profile_name checks existence
        Ok((
            // Return a reference to the key in the map (stable lifetime)
            self.profiles
                .get_key_value(&name)
                .map(|(k, _)| k.as_str())
                .unwrap(),
            profile,
        ))
    }

    /// Sorted profile names for display.
    pub fn profile_names_sorted(&self) -> Vec<String> {
        let mut names: Vec<String> = self.profiles.keys().cloned().collect();
        names.sort();
        names
    }
}

// ---------------------------------------------------------------------------
// Legacy flat config for migration (FR-2a)
// ---------------------------------------------------------------------------

/// The old flat config shape used before profiles.
#[derive(Debug, Deserialize)]
struct LegacyConfig {
    #[serde(default)]
    server_url: Option<String>,
    #[serde(default)]
    engineer: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    models: Option<ModelsSection>,
    #[serde(default)]
    helm: Option<HelmConfig>,
    // Preserved for backward compat detection — read via raw TOML parsing instead.
    #[serde(default)]
    #[allow(dead_code)]
    profiles: Option<toml::Value>,
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Get the config file path.
pub fn config_path() -> PathBuf {
    dirs_path().join("config.toml")
}

fn dirs_path() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE")) // Windows fallback
        .unwrap_or_else(|_| "/tmp".to_string()); // Safe fallback, never cwd
    PathBuf::from(home).join(".nemo")
}

// ---------------------------------------------------------------------------
// Load / save / migrate
// ---------------------------------------------------------------------------

/// Load the config, performing migration if needed. Returns the config and
/// whether migration was performed (so the caller can print a message).
pub fn load_config_with_migration() -> Result<(NemoConfig, bool)> {
    let path = config_path();
    if !path.exists() {
        return Ok((NemoConfig::default(), false));
    }

    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;

    // Parse as raw TOML to detect shape
    let raw: toml::Value = toml::from_str(&contents)
        .with_context(|| "config file is malformed")?;
    let raw_table = raw.as_table();

    let has_profiles = raw_table
        .map(|t| t.contains_key("profiles"))
        .unwrap_or(false);
    let has_server_url = raw_table
        .map(|t| t.contains_key("server_url"))
        .unwrap_or(false);

    // Already in profile shape: has [profiles] table
    if has_profiles {
        let mut config: NemoConfig = toml::from_str(&contents)
            .with_context(|| "config has [profiles] table but could not be parsed")?;
        if config.current_profile.as_deref() == Some("") {
            config.current_profile = None;
        }
        return Ok((config, false));
    }

    // No profiles and no server_url — empty/new file, just helm/models
    if !has_server_url {
        let legacy: LegacyConfig = toml::from_str(&contents)?;
        let config = NemoConfig {
            current_profile: None,
            profiles: BTreeMap::new(),
            helm: legacy.helm.unwrap_or_default(),
            models: legacy.models.unwrap_or_default(),
        };
        return Ok((config, false));
    }

    // Migration: server_url at root, no profiles table
    let legacy: LegacyConfig = toml::from_str(&contents)
        .with_context(|| "config file is malformed")?;

    // Migrate: flat fields → [profiles.default]
    let server_url = legacy.server_url.unwrap_or_default();
    let engineer = legacy.engineer.unwrap_or_default();
    // Empty strings for name/email → None (FR-2a empty string handling)
    let name = legacy.name.filter(|s| !s.is_empty());
    let email = legacy.email.filter(|s| !s.is_empty());

    let profile = ProfileConfig {
        server_url,
        api_key: legacy.api_key,
        engineer,
        name,
        email,
    };

    let mut profiles = BTreeMap::new();
    profiles.insert("default".to_string(), profile);

    let config = NemoConfig {
        current_profile: Some("default".to_string()),
        profiles,
        helm: legacy.helm.unwrap_or_default(),
        models: legacy.models.unwrap_or_default(),
    };

    // Write the migrated config back
    save_config(&config)?;

    Ok((config, true))
}

/// Load the engineer config, performing migration if needed.
/// Prints migration message to stderr.
pub fn load_config() -> Result<NemoConfig> {
    let (config, migrated) = load_config_with_migration()?;
    if migrated {
        eprintln!(
            "Migrated config to profile 'default'. Create additional profiles with 'nemo profile add <name>'."
        );
    }
    Ok(config)
}

/// Save the config file atomically with 0600 permissions.
pub fn save_config(config: &NemoConfig) -> Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let contents = toml::to_string_pretty(config)?;

    // Write to a temp file with restricted permissions first, then rename.
    let tmp_path = path.with_extension("toml.tmp");

    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp_path)?;
        file.write_all(contents.as_bytes())?;
    }

    #[cfg(not(unix))]
    {
        std::fs::write(&tmp_path, &contents)?;
    }

    std::fs::rename(&tmp_path, &path)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::fs;
    use std::path::Path;

    /// Helper: set up a temp HOME with a config file.
    fn setup_temp_config(content: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let nemo_dir = tmp.path().join(".nemo");
        fs::create_dir_all(&nemo_dir).unwrap();
        fs::write(nemo_dir.join("config.toml"), content).unwrap();
        tmp
    }

    /// Helper: load config from a string (same logic as load_config_with_migration, sans file I/O).
    fn load_from_str(content: &str) -> Result<(NemoConfig, bool)> {
        let raw: toml::Value = toml::from_str(content)?;
        let raw_table = raw.as_table();
        let has_profiles = raw_table.map(|t| t.contains_key("profiles")).unwrap_or(false);
        let has_server_url = raw_table.map(|t| t.contains_key("server_url")).unwrap_or(false);

        if has_profiles {
            let mut config: NemoConfig = toml::from_str(content)?;
            if config.current_profile.as_deref() == Some("") {
                config.current_profile = None;
            }
            return Ok((config, false));
        }

        if !has_server_url {
            let legacy: LegacyConfig = toml::from_str(content)?;
            return Ok((NemoConfig {
                profiles: BTreeMap::new(),
                helm: legacy.helm.unwrap_or_default(),
                models: legacy.models.unwrap_or_default(),
                ..Default::default()
            }, false));
        }

        // Migrate
        let legacy: LegacyConfig = toml::from_str(content)?;
        let profile = ProfileConfig {
            server_url: legacy.server_url.unwrap_or_default(),
            api_key: legacy.api_key,
            engineer: legacy.engineer.unwrap_or_default(),
            name: legacy.name.filter(|s| !s.is_empty()),
            email: legacy.email.filter(|s| !s.is_empty()),
        };
        let mut profiles = BTreeMap::new();
        profiles.insert("default".to_string(), profile);
        Ok((NemoConfig {
            current_profile: Some("default".to_string()),
            profiles,
            helm: legacy.helm.unwrap_or_default(),
            models: legacy.models.unwrap_or_default(),
        }, true))
    }

    #[test]
    fn migrate_flat_to_profile() {
        let flat = r#"
server_url = "http://localhost:18080"
engineer = "dev"
name = "Dev User"
email = "dev@example.com"
api_key = "dev-api-key-12345"

[helm]
desktop_notifications = false
"#;
        let (config, migrated) = load_from_str(flat).unwrap();
        assert!(migrated);
        assert_eq!(config.current_profile.as_deref(), Some("default"));
        let p = config.profiles.get("default").unwrap();
        assert_eq!(p.server_url, "http://localhost:18080");
        assert_eq!(p.engineer, "dev");
        assert_eq!(p.name.as_deref(), Some("Dev User"));
        assert_eq!(p.email.as_deref(), Some("dev@example.com"));
        assert_eq!(p.api_key.as_deref(), Some("dev-api-key-12345"));
        assert!(!config.helm.desktop_notifications);
    }

    #[test]
    fn migrate_empty_strings_to_none() {
        let flat = r#"
server_url = "http://localhost:18080"
engineer = "dev"
name = ""
email = ""
"#;
        let (config, migrated) = load_from_str(flat).unwrap();
        assert!(migrated);
        let p = config.profiles.get("default").unwrap();
        assert!(p.name.is_none());
        assert!(p.email.is_none());
    }

    #[test]
    fn migration_idempotent() {
        let profile_shape = r#"
current_profile = "default"

[profiles.default]
server_url = "http://localhost:18080"
engineer = "dev"
api_key = "abc"

[helm]
desktop_notifications = false
"#;
        let (config, migrated) = load_from_str(profile_shape).unwrap();
        assert!(!migrated);
        assert_eq!(config.current_profile.as_deref(), Some("default"));
        assert!(config.profiles.contains_key("default"));
    }

    #[test]
    fn no_migration_needed_empty() {
        let content = r#"
[helm]
desktop_notifications = true
"#;
        let (config, migrated) = load_from_str(content).unwrap();
        assert!(!migrated);
        assert!(config.profiles.is_empty());
        assert!(config.helm.desktop_notifications);
    }

    #[test]
    fn profile_name_validation() {
        assert!(validate_profile_name("default").is_ok());
        assert!(validate_profile_name("work-prod").is_ok());
        assert!(validate_profile_name("d").is_ok());
        assert!(validate_profile_name("123").is_ok());
        assert!(validate_profile_name("A1").is_ok());
        assert!(validate_profile_name("-bad").is_err());
        assert!(validate_profile_name("").is_err());
        assert!(validate_profile_name("has space").is_err());
        assert!(validate_profile_name("has_underscore").is_err());
    }

    #[test]
    fn redact_api_key_long() {
        assert_eq!(redact_api_key("abcdefghijklmnop"), "abcd...mnop");
    }

    #[test]
    fn redact_api_key_short() {
        assert_eq!(redact_api_key("short"), "****");
        assert_eq!(redact_api_key("exactly12ch"), "****");
    }

    #[test]
    fn resolve_profile_flag_wins() {
        let mut config = NemoConfig::default();
        config.current_profile = Some("default".to_string());
        config.profiles.insert(
            "default".to_string(),
            ProfileConfig {
                server_url: "http://default".to_string(),
                api_key: None,
                engineer: "a".to_string(),
                name: None,
                email: None,
            },
        );
        config.profiles.insert(
            "work".to_string(),
            ProfileConfig {
                server_url: "http://work".to_string(),
                api_key: None,
                engineer: "b".to_string(),
                name: None,
                email: None,
            },
        );

        let name = config.resolve_profile_name(Some("work")).unwrap();
        assert_eq!(name, "work");
    }

    #[test]
    fn resolve_profile_missing_errors() {
        let config = NemoConfig::default();
        let err = config.resolve_profile_name(Some("nope")).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn resolve_profile_cold_start() {
        let config = NemoConfig::default();
        let err = config.resolve_profile_name(None).unwrap_err();
        assert!(err.to_string().contains("No profiles configured"));
    }

    #[test]
    fn resolve_profile_dangling_reference() {
        let mut config = NemoConfig::default();
        config.current_profile = Some("gone".to_string());
        config.profiles.insert(
            "remaining".to_string(),
            ProfileConfig {
                server_url: "http://x".to_string(),
                api_key: None,
                engineer: "a".to_string(),
                name: None,
                email: None,
            },
        );
        let err = config.resolve_profile_name(None).unwrap_err();
        assert!(err.to_string().contains("Active profile 'gone' not found"));
    }
}
