use anyhow::Result;

use crate::client::NemoClient;

/// Push local model credentials to the cluster.
///
/// Reads local credential files, validates they exist, and registers them
/// with the control plane so AWAITING_REAUTH loops can recover via `nemo resume`.
pub async fn run(
    client: &NemoClient,
    engineer: &str,
    claude: bool,
    openai: bool,
    ssh: bool,
) -> Result<()> {
    if engineer.is_empty() {
        anyhow::bail!("Engineer name not configured. Run: nemo config --set engineer=<your-name>");
    }

    let mut providers: Vec<&str> = Vec::new();
    if claude {
        providers.push("claude");
    }
    if openai {
        providers.push("openai");
    }
    if ssh {
        providers.push("ssh");
    }
    // Default: all three if none specified
    if providers.is_empty() {
        providers = vec!["claude", "openai", "ssh"];
    }

    let mut any_registered = false;
    let mut any_error = false;

    for provider in &providers {
        let cred_path = match *provider {
            "claude" => {
                // Claude Code credential paths (checked in priority order)
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
                let config_dir =
                    std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| format!("{home}/.config"));
                let candidates = [
                    format!("{home}/.claude/.credentials.json"), // claude-worktree convention
                    format!("{config_dir}/claude-code/credentials.json"), // XDG standard
                    format!("{home}/.claude/credentials.json"),  // legacy
                ];
                candidates
                    .iter()
                    .find(|p| std::path::Path::new(p).exists())
                    .cloned()
                    .unwrap_or_else(|| candidates[0].clone())
            }
            "openai" => {
                // OpenCode / OpenAI credential paths (checked in priority order)
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
                let config_dir =
                    std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| format!("{home}/.config"));
                let candidates = [
                    format!("{config_dir}/opencode/credentials.json"), // opencode reviewer auth
                    format!("{config_dir}/openai/credentials.json"),   // direct OpenAI
                ];
                candidates
                    .iter()
                    .find(|p| std::path::Path::new(p).exists())
                    .cloned()
                    .unwrap_or_else(|| candidates[0].clone())
            }
            "ssh" => {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
                format!("{home}/.ssh/id_ed25519")
            }
            _ => continue,
        };

        if !std::path::Path::new(&cred_path).exists() {
            eprintln!("No {provider} credentials found at {cred_path}");
            match *provider {
                "claude" => eprintln!("  Run: claude login"),
                "openai" => {
                    eprintln!("  Create {cred_path} with your OpenAI API key as content")
                }
                "ssh" => eprintln!("  Run: ssh-keygen -t ed25519"),
                _ => {}
            }
            continue;
        }

        // Read the credential file
        let content = match std::fs::read_to_string(&cred_path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Warning: could not read {provider} credentials at {cred_path}: {e}");
                any_error = true;
                continue;
            }
        };

        // For claude/openai, validate JSON. For SSH, it's a PEM key.
        if *provider != "ssh"
            && serde_json::from_str::<serde_json::Value>(&content).is_err()
            && content.trim().is_empty()
        {
            eprintln!("Error: {provider} credentials at {cred_path} are empty");
            any_error = true;
            continue;
        }

        // Register credentials with the control plane
        match client
            .register_credentials(engineer, provider, &content)
            .await
        {
            Ok(()) => {
                println!("Registered {provider} credentials with control plane");
                any_registered = true;
            }
            Err(e) => {
                eprintln!("Failed to register {provider} credentials: {e}");
                eprintln!("  Credentials found locally at {cred_path} but could not be pushed.");
                eprintln!("  Ensure the control plane is reachable and your API key is valid.");
                any_error = true;
            }
        }
    }

    if any_registered {
        println!();
        println!("Credentials registered. If you have loops in AWAITING_REAUTH state,");
        println!("resume them with: nemo resume <loop-id>");
    }

    if any_error {
        if any_registered {
            anyhow::bail!("Some credential uploads failed (see errors above)");
        } else {
            anyhow::bail!("All credential uploads failed");
        }
    }

    if !any_registered {
        anyhow::bail!("No credential files found. Run the provider CLI to authenticate first.");
    }

    Ok(())
}
