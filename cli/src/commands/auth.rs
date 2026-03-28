use anyhow::Result;

use crate::client::NemoClient;

/// Push local model credentials to the cluster.
///
/// Reads local credential files, validates they exist, and registers them
/// with the control plane so AWAITING_REAUTH loops can recover via `nemo resume`.
pub async fn run(client: &NemoClient, engineer: &str, claude: bool, openai: bool) -> Result<()> {
    if engineer.is_empty() {
        anyhow::bail!(
            "Engineer name not configured. Run: nemo config --set engineer=<your-name>"
        );
    }

    let providers: Vec<&str> = match (claude, openai) {
        (true, false) => vec!["claude"],
        (false, true) => vec!["openai"],
        _ => vec!["claude", "openai"],
    };

    let mut any_registered = false;
    let mut any_error = false;

    for provider in &providers {
        let cred_path = match *provider {
            "claude" => {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
                format!("{home}/.claude/credentials.json")
            }
            "openai" => {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
                format!("{home}/.config/openai/auth.json")
            }
            _ => continue,
        };

        if !std::path::Path::new(&cred_path).exists() {
            eprintln!("No {provider} credentials found at {cred_path}");
            eprintln!("  Run the {provider} CLI to authenticate first.");
            continue;
        }

        // Validate the credential file is readable JSON
        let content = std::fs::read_to_string(&cred_path)?;
        if serde_json::from_str::<serde_json::Value>(&content).is_err() {
            eprintln!("Warning: {provider} credentials at {cred_path} are not valid JSON");
            continue;
        }

        // Register credentials with the control plane (send content, not path)
        match client.register_credentials(engineer, provider, &content).await {
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
        // Some or all providers failed
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
