use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Engineer-level configuration from ~/.nemo/config.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineerConfig {
    #[serde(default = "default_server_url")]
    pub server_url: String,
    #[serde(default)]
    pub engineer: String,
    pub api_key: Option<String>,
}

fn default_server_url() -> String {
    "https://localhost:8080".to_string()
}

impl Default for EngineerConfig {
    fn default() -> Self {
        Self {
            server_url: default_server_url(),
            engineer: String::new(),
            api_key: None,
        }
    }
}

/// Get the config file path.
pub fn config_path() -> PathBuf {
    dirs_path().join("config.toml")
}

fn dirs_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".nemo")
}

/// Load the engineer config, returning defaults if the file doesn't exist.
pub fn load_config() -> Result<EngineerConfig> {
    let path = config_path();
    if path.exists() {
        let contents = std::fs::read_to_string(&path)?;
        let config: EngineerConfig = toml::from_str(&contents)?;
        Ok(config)
    } else {
        Ok(EngineerConfig::default())
    }
}

/// Save the engineer config.
pub fn save_config(config: &EngineerConfig) -> Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let contents = toml::to_string_pretty(config)?;
    std::fs::write(&path, contents)?;

    // Restrict permissions to owner-only (0600) since file may contain API keys
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }

    Ok(())
}
