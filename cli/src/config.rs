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
    /// Display name for git attribution (GIT_AUTHOR_NAME).
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub email: String,
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
            name: String::new(),
            email: String::new(),
            api_key: None,
        }
    }
}

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
/// Writes atomically via temp file to avoid a window where the file is world-readable.
pub fn save_config(config: &EngineerConfig) -> Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let contents = toml::to_string_pretty(config)?;

    // Write to a temp file with restricted permissions first, then rename.
    // This avoids a window where the file exists with default umask permissions.
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
