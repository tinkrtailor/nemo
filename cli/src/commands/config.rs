use anyhow::Result;

/// Edit ~/.nemo/config.toml.
pub fn run(set: Option<String>, get: Option<String>) -> Result<()> {
    // Try loading existing config. For --set, fall back to defaults with a
    // warning if the file is malformed, so users can repair with --set.
    // For --get and display, propagate the error.
    let config = match crate::config::load_config() {
        Ok(c) => c,
        Err(e) => {
            if set.is_some() {
                eprintln!("Warning: existing config is malformed ({e}), starting from defaults");
                eprintln!("Other settings may be lost. Re-set them after this operation.");
                crate::config::EngineerConfig::default()
            } else {
                return Err(e);
            }
        }
    };

    if let Some(key) = get {
        match key.as_str() {
            "server_url" => println!("{}", config.server_url),
            "engineer" => println!("{}", config.engineer),
            "api_key" => {
                if let Some(key) = &config.api_key {
                    // Mask sensitive value using chars() to handle non-ASCII safely
                    let chars: Vec<char> = key.chars().collect();
                    if chars.len() > 12 {
                        let prefix: String = chars[..4].iter().collect();
                        let suffix: String = chars[chars.len() - 4..].iter().collect();
                        println!("{prefix}...{suffix}");
                    } else {
                        println!("****");
                    }
                } else {
                    println!("(not set)");
                }
            }
            _ => anyhow::bail!("Unknown config key: {key}"),
        }
        return Ok(());
    }

    if let Some(kv) = set {
        let parts: Vec<&str> = kv.splitn(2, '=').collect();
        if parts.len() != 2 {
            anyhow::bail!("Expected format: key=value");
        }

        let (key, value) = (parts[0], parts[1]);
        let mut config = config;

        match key {
            "server_url" => config.server_url = value.to_string(),
            "engineer" => config.engineer = value.to_string(),
            "api_key" => {
                // Reject empty API keys — they break all authenticated requests
                if value.is_empty() {
                    config.api_key = None;
                    println!("Cleared api_key");
                } else {
                    config.api_key = Some(value.to_string());
                    println!("Set {key} = ****");
                }
                crate::config::save_config(&config)?;
                return Ok(());
            }
            _ => anyhow::bail!("Unknown config key: {key}"),
        }

        crate::config::save_config(&config)?;
        println!("Set {key} = {value}");
        return Ok(());
    }

    // No flags: print current config
    println!("Nemo CLI Configuration (~/.nemo/config.toml)");
    println!("  server_url: {}", config.server_url);
    println!("  engineer:   {}", config.engineer);
    println!(
        "  api_key:    {}",
        if config.api_key.is_some() {
            "(set)"
        } else {
            "(not set)"
        }
    );

    Ok(())
}
