use anyhow::Result;

use crate::config::{self, NemoConfig, redact_api_key, save_config};

/// Profile-scoped keys (written to active profile).
const PROFILE_KEYS: &[&str] = &["server_url", "api_key", "engineer", "name", "email"];

/// Root-scoped keys (written to top-level sections).
const ROOT_KEYS: &[&str] = &[
    "helm.desktop_notifications",
    "helm.theme",
    "models.implementor",
    "models.reviewer",
];

/// Valid theme values.
const VALID_THEMES: &[&str] = &["dark", "light", "high-contrast"];

fn is_profile_key(key: &str) -> bool {
    PROFILE_KEYS.contains(&key)
}

fn is_root_key(key: &str) -> bool {
    ROOT_KEYS.contains(&key)
}

fn unknown_key_error(key: &str) -> String {
    format!(
        "Unknown config key '{key}'. Gettable keys: current_profile. Profile keys: server_url, api_key, engineer, name, email. Root keys: helm.desktop_notifications, helm.theme, models.implementor, models.reviewer."
    )
}

/// Parse a boolean from a config value string.
fn parse_bool(val: &str) -> Option<bool> {
    match val.to_lowercase().as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Edit ~/.nemo/config.toml (profile-aware).
pub fn run(
    set: Option<String>,
    get: Option<String>,
    profile_flag: Option<&str>,
    unmask: bool,
) -> Result<()> {
    if set.is_some() && get.is_some() {
        anyhow::bail!("Cannot use --set and --get together");
    }

    // Load config with migration (FR-2a, FR-6e)
    let (mut config, migrated) = match config::load_config_with_migration() {
        Ok(r) => r,
        Err(e) => {
            if set.is_some() {
                eprintln!("Warning: existing config is malformed ({e}), starting from defaults");
                eprintln!("Other settings may be lost. Re-set them after this operation.");
                (NemoConfig::default(), false)
            } else {
                return Err(e);
            }
        }
    };

    if migrated {
        eprintln!(
            "Migrated config to profile 'default'. Create additional profiles with 'nemo profile add <name>'."
        );
    }

    // --get
    if let Some(key) = get {
        return handle_get(&config, &key, profile_flag, unmask);
    }

    // --set
    if let Some(kv) = set {
        return handle_set(&mut config, &kv, profile_flag);
    }

    // No flags: display current config (FR-5c)
    display_config(&config, profile_flag, unmask)
}

fn handle_get(
    config: &NemoConfig,
    key: &str,
    profile_flag: Option<&str>,
    unmask: bool,
) -> Result<()> {
    // Special: current_profile
    if key == "current_profile" {
        // Report the resolved effective profile (FR-6g).
        // --profile flag is ignored for this key per spec; only NAUTILOOP_PROFILE > current_profile.
        match config.resolve_profile_name(None) {
            Ok(name) => {
                println!("{name}");
                Ok(())
            }
            Err(_) => {
                // No profile active → empty output, exit 1
                std::process::exit(1);
            }
        }
    } else if is_profile_key(key) {
        let profile_name = config.resolve_profile_name(profile_flag)?;
        let profile = &config.profiles[&profile_name];

        let value = match key {
            "server_url" => Some(profile.server_url.clone()),
            "api_key" => profile
                .api_key
                .as_ref()
                .map(|k| if unmask { k.clone() } else { redact_api_key(k) }),
            "engineer" => {
                if profile.engineer.is_empty() {
                    None
                } else {
                    Some(profile.engineer.clone())
                }
            }
            "name" => profile.name.clone(),
            "email" => profile.email.clone(),
            _ => unreachable!(),
        };

        match value {
            Some(v) => {
                println!("{v}");
                Ok(())
            }
            None => std::process::exit(1),
        }
    } else if is_root_key(key) {
        let value = match key {
            "helm.desktop_notifications" => Some(config.helm.desktop_notifications.to_string()),
            "helm.theme" => config.helm.theme.clone(),
            "models.implementor" => config.models.implementor.clone(),
            "models.reviewer" => config.models.reviewer.clone(),
            _ => unreachable!(),
        };
        match value {
            Some(v) => {
                println!("{v}");
                Ok(())
            }
            None => std::process::exit(1),
        }
    } else {
        anyhow::bail!("{}", unknown_key_error(key));
    }
}

fn handle_set(config: &mut NemoConfig, kv: &str, profile_flag: Option<&str>) -> Result<()> {
    let parts: Vec<&str> = kv.splitn(2, '=').collect();
    if parts.len() != 2 {
        anyhow::bail!("Expected format: key=value");
    }

    let (key, value) = (parts[0], parts[1]);

    // Reject current_profile via --set
    if key == "current_profile" {
        anyhow::bail!(
            "'current_profile' cannot be set via --set. Use 'nemo use-profile <name>' instead."
        );
    }

    if is_profile_key(key) {
        let profile_name = config.resolve_profile_name(profile_flag)?;
        let profile = config.profiles.get_mut(&profile_name).unwrap();

        match key {
            "server_url" => profile.server_url = value.to_string(),
            "engineer" => profile.engineer = value.to_string(),
            "name" => {
                profile.name = if value.is_empty() {
                    None
                } else {
                    Some(value.to_string())
                };
            }
            "email" => {
                profile.email = if value.is_empty() {
                    None
                } else {
                    Some(value.to_string())
                };
            }
            "api_key" => {
                if value.is_empty() {
                    profile.api_key = None;
                    save_config(config)?;
                    eprintln!("Cleared api_key");
                    return Ok(());
                } else {
                    profile.api_key = Some(value.to_string());
                    save_config(config)?;
                    eprintln!("Set {key} = ****");
                    return Ok(());
                }
            }
            _ => unreachable!(),
        }

        save_config(config)?;
        eprintln!("Set {key} = {value}");
        Ok(())
    } else if is_root_key(key) {
        match key {
            "helm.desktop_notifications" => {
                let b = parse_bool(value).ok_or_else(|| {
                    anyhow::anyhow!(
                        "Invalid value for helm.desktop_notifications: '{value}'. Must be true or false."
                    )
                })?;
                config.helm.desktop_notifications = b;
            }
            "helm.theme" => {
                if !VALID_THEMES.contains(&value) {
                    anyhow::bail!(
                        "Invalid value for helm.theme: '{value}'. Must be one of: dark, light, high-contrast"
                    );
                }
                config.helm.theme = Some(value.to_string());
            }
            "models.implementor" => {
                config.models.implementor = if value.is_empty() {
                    None
                } else {
                    Some(value.to_string())
                };
            }
            "models.reviewer" => {
                config.models.reviewer = if value.is_empty() {
                    None
                } else {
                    Some(value.to_string())
                };
            }
            _ => unreachable!(),
        }

        save_config(config)?;
        eprintln!("Set {key} = {value}");
        Ok(())
    } else {
        anyhow::bail!("{}", unknown_key_error(key));
    }
}

fn display_config(config: &NemoConfig, profile_flag: Option<&str>, unmask: bool) -> Result<()> {
    // Try to resolve active profile
    let active_name = config.resolve_profile_name(profile_flag).ok();

    if let Some(ref name) = active_name {
        println!("Active profile: {name}");
    } else if config.profiles.is_empty() {
        println!("No profiles configured.");
    } else {
        println!("No active profile set.");
    }

    if !config.profiles.is_empty() {
        let names = config.profile_names_sorted();
        let display: Vec<String> = names
            .iter()
            .map(|n| {
                if active_name.as_deref() == Some(n.as_str()) {
                    format!("{n}*")
                } else {
                    n.clone()
                }
            })
            .collect();
        println!("Profiles: {}", display.join(", "));
    }

    if let Some(ref name) = active_name {
        let profile = &config.profiles[name];
        println!();
        println!("  server_url: {}", profile.server_url);
        println!(
            "  api_key:    {}",
            match &profile.api_key {
                Some(k) if unmask => k.clone(),
                Some(k) => redact_api_key(k),
                None => "(not set)".to_string(),
            }
        );
        println!("  engineer:   {}", profile.engineer);
        println!(
            "  name:       {}",
            profile.name.as_deref().unwrap_or("(not set)")
        );
        println!(
            "  email:      {}",
            profile.email.as_deref().unwrap_or("(not set)")
        );
    }

    println!();
    println!("[helm]");
    println!(
        "  desktop_notifications: {}",
        config.helm.desktop_notifications
    );
    println!(
        "  theme: {}",
        config.helm.theme.as_deref().unwrap_or("(not set)")
    );

    println!();
    println!("[models]");
    println!(
        "  implementor: {}",
        config.models.implementor.as_deref().unwrap_or("(not set)")
    );
    println!(
        "  reviewer: {}",
        config.models.reviewer.as_deref().unwrap_or("(not set)")
    );

    Ok(())
}
