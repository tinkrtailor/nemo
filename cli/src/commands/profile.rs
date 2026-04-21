use anyhow::Result;

use crate::config::{
    NemoConfig, ProfileConfig, redact_api_key, save_config, validate_profile_name,
};

/// `nemo profile ls` / `nemo profile list`
pub fn run_list(config: &NemoConfig) -> Result<()> {
    if config.profiles.is_empty() {
        println!("No profiles configured. Run 'nemo profile add <name> --server <url> --api-key <key> --engineer <id>' to get started.");
        return Ok(());
    }

    let active = config.current_profile.as_deref().unwrap_or("");
    let names = config.profile_names_sorted();

    // Calculate column widths
    let max_name = names.iter().map(|n| n.len()).max().unwrap_or(0);
    let max_url = config
        .profiles
        .values()
        .map(|p| p.server_url.len())
        .max()
        .unwrap_or(0);

    for name in &names {
        let profile = &config.profiles[name];
        let marker = if name == active { "*" } else { " " };
        println!(
            "{marker} {:<width_name$}  {:<width_url$}  {}",
            name,
            profile.server_url,
            profile.engineer,
            width_name = max_name,
            width_url = max_url,
        );
    }

    Ok(())
}

/// `nemo profile show [<name>]`
pub fn run_show(
    config: &NemoConfig,
    name: Option<&str>,
    profile_flag: Option<&str>,
    unmask: bool,
) -> Result<()> {
    let profile_name = match name {
        Some(n) => {
            if !config.profiles.contains_key(n) {
                let available = config.profile_names_sorted().join(", ");
                anyhow::bail!("Profile '{n}' not found. Available: {available}.");
            }
            n.to_string()
        }
        None => config.resolve_profile_name(profile_flag)?,
    };

    let profile = &config.profiles[&profile_name];
    let is_active = config.current_profile.as_deref() == Some(&*profile_name);

    println!(
        "Profile: {}{}",
        profile_name,
        if is_active { " (active)" } else { "" }
    );
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

    Ok(())
}

/// `nemo profile add <name>`
#[allow(clippy::too_many_arguments)]
pub fn run_add(
    config: &mut NemoConfig,
    name: &str,
    server: &str,
    api_key: &str,
    engineer: &str,
    profile_name_field: Option<String>,
    email: Option<String>,
    switch: bool,
) -> Result<()> {
    validate_profile_name(name)?;

    if config.profiles.contains_key(name) {
        anyhow::bail!("Profile '{name}' already exists. Use 'nemo profile rename' or 'nemo profile rm' first.");
    }

    if api_key.is_empty() {
        anyhow::bail!("--api-key must not be empty");
    }
    if engineer.is_empty() {
        anyhow::bail!("--engineer must not be empty");
    }

    // Default name/email from current profile if available
    let (default_name, default_email) = match &config.current_profile {
        Some(cp) if config.profiles.contains_key(cp) => {
            let current = &config.profiles[cp];
            (current.name.clone(), current.email.clone())
        }
        _ => (None, None),
    };

    let profile = ProfileConfig {
        server_url: server.to_string(),
        api_key: Some(api_key.to_string()),
        engineer: engineer.to_string(),
        name: profile_name_field.or(default_name),
        email: email.or(default_email),
    };

    config.profiles.insert(name.to_string(), profile);

    // First-profile auto-activate (FR-3a)
    let auto_activated = if config.current_profile.is_none() {
        config.current_profile = Some(name.to_string());
        true
    } else {
        false
    };

    // --switch flag
    if switch && !auto_activated {
        config.current_profile = Some(name.to_string());
    }

    save_config(config)?;

    eprintln!("Added profile '{name}'.");
    if auto_activated || switch {
        eprintln!("Active profile: {name}");
    }

    Ok(())
}

/// `nemo profile rm <name>`
pub fn run_remove(config: &mut NemoConfig, name: &str) -> Result<()> {
    if !config.profiles.contains_key(name) {
        let available = config.profile_names_sorted().join(", ");
        anyhow::bail!("Profile '{name}' not found. Available: {available}.");
    }

    if config.current_profile.as_deref() == Some(name) {
        anyhow::bail!(
            "Cannot remove the active profile '{name}'. Switch to another profile first with 'nemo use-profile <other>'."
        );
    }

    if config.profiles.len() == 1 {
        anyhow::bail!(
            "Cannot remove the last profile '{name}'. At least one profile must exist."
        );
    }

    config.profiles.remove(name);
    save_config(config)?;

    eprintln!("Removed profile '{name}'.");

    Ok(())
}

/// `nemo profile rename <old> <new>`
pub fn run_rename(config: &mut NemoConfig, old: &str, new: &str) -> Result<()> {
    if old == new {
        return Ok(()); // no-op
    }

    validate_profile_name(new)?;

    if !config.profiles.contains_key(old) {
        let available = config.profile_names_sorted().join(", ");
        anyhow::bail!("Profile '{old}' not found. Available: {available}.");
    }

    if config.profiles.contains_key(new) {
        anyhow::bail!("Profile '{new}' already exists.");
    }

    let profile = config.profiles.remove(old).unwrap();
    config.profiles.insert(new.to_string(), profile);

    // Update current_profile if it was the renamed one
    if config.current_profile.as_deref() == Some(old) {
        config.current_profile = Some(new.to_string());
    }

    save_config(config)?;

    eprintln!("Renamed profile '{old}' to '{new}'.");

    Ok(())
}

/// `nemo use-profile <name>` / `nemo profile use <name>`
pub fn run_use_profile(config: &mut NemoConfig, name: &str) -> Result<()> {
    if !config.profiles.contains_key(name) {
        let available = config.profile_names_sorted().join(", ");
        anyhow::bail!("Profile '{name}' not found. Available: {available}.");
    }

    config.current_profile = Some(name.to_string());
    save_config(config)?;

    let profile = &config.profiles[name];
    eprintln!("Active profile: {name} ({}).", profile.server_url);

    Ok(())
}
