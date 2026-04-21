use anyhow::Result;

use crate::api_types::{CredentialsResponse, ProviderInfo};
use crate::client::NemoClient;

/// Model catalog - known models per provider.
const CLAUDE_MODELS: &[&str] = &["claude-opus-4", "claude-sonnet-4", "claude-haiku-4"];

const OPENAI_MODELS: &[&str] = &["gpt-5.4", "gpt-4o", "o1-preview", "o1-mini"];

/// Local credential file paths (checked in priority order).
fn local_cred_paths(provider: &str) -> Vec<String> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let config_dir = std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| format!("{home}/.config"));

    match provider {
        "claude" => vec![
            format!("{home}/.claude/.credentials.json"),
            format!("{config_dir}/claude-code/credentials.json"),
            format!("{home}/.claude/credentials.json"),
        ],
        "openai" => vec![
            format!("{home}/.local/share/opencode/auth.json"),
            format!("{home}/.codex/auth.json"),
            format!("{config_dir}/opencode/credentials.json"),
            format!("{config_dir}/openai/credentials.json"),
        ],
        "ssh" => vec![format!("{home}/.ssh/id_ed25519")],
        _ => vec![],
    }
}

/// Check if any credential file exists for a provider.
fn has_local_credentials(provider: &str) -> (bool, Option<String>) {
    for path in local_cred_paths(provider) {
        if std::path::Path::new(&path).exists() {
            return (true, Some(path));
        }
    }
    (false, None)
}

/// JSON output type for `nemo models --json`.
#[derive(serde::Serialize)]
struct ModelsJsonOutput {
    providers: Vec<ModelsJsonProvider>,
}

#[derive(serde::Serialize)]
struct ModelsJsonProvider {
    provider: String,
    models: Vec<String>,
    valid: bool,
    updated_at: String,
}

fn build_models_json(cp_providers: &[ProviderInfo]) -> ModelsJsonOutput {
    let provider_configs: &[(&str, &[&str])] = &[
        ("claude", CLAUDE_MODELS),
        ("openai", OPENAI_MODELS),
    ];

    let mut providers = Vec::new();
    for (name, models) in provider_configs {
        let cp_info = cp_providers.iter().find(|p| p.provider == *name);
        providers.push(ModelsJsonProvider {
            provider: name.to_string(),
            models: models.iter().map(|m| m.to_string()).collect(),
            valid: cp_info.map(|p| p.valid).unwrap_or(false),
            updated_at: cp_info.map(|p| p.updated_at.clone()).unwrap_or_default(),
        });
    }

    ModelsJsonOutput { providers }
}

/// Run the models command (profile-aware entry point).
pub async fn run_with_models(client: &NemoClient, engineer: &str, json: bool) -> Result<()> {
    if engineer.is_empty() {
        anyhow::bail!("Engineer name not configured. Run: nemo config --set engineer=<your-name>");
    }

    // Fetch control plane credentials
    let cp_providers: Vec<ProviderInfo> = match client
        .get::<CredentialsResponse>(&format!(
            "/credentials?engineer={}",
            urlencoding::encode(engineer)
        ))
        .await
    {
        Ok(resp) => resp.providers,
        Err(e) => {
            if !json {
                eprintln!("Warning: Could not fetch credentials from control plane: {e}");
            }
            vec![]
        }
    };

    if json {
        let output = build_models_json(&cp_providers);
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    println!("Authenticated Providers");
    println!("========================\n");

    let providers = ["claude", "openai", "ssh"];
    let mut any_registered = false;

    for provider in &providers {
        let (local_exists, local_path) = has_local_credentials(provider);
        let cp_info = cp_providers.iter().find(|p| p.provider == *provider);

        let status_icon = if cp_info.map(|p| p.valid).unwrap_or(false) {
            any_registered = true;
            "✓"
        } else if local_exists {
            "○" // Local only, not pushed
        } else {
            "✗"
        };

        let status_text = if cp_info.map(|p| p.valid).unwrap_or(false) {
            if let Some(path) = local_path {
                format!("{} [control plane: valid]", path)
            } else {
                "[control plane: valid, local file missing]".to_string()
            }
        } else if local_exists {
            if let Some(path) = local_path {
                format!("{} [control plane: not registered — run 'nemo auth']", path)
            } else {
                "[local file missing]".to_string()
            }
        } else {
            match *provider {
                "claude" => "not found — Run: claude login".to_string(),
                "openai" => {
                    "not found — Create ~/.config/openai/credentials.json with your API key"
                        .to_string()
                }
                "ssh" => "not found — Run: ssh-keygen -t ed25519".to_string(),
                _ => "not found".to_string(),
            }
        };

        println!("{:>3} {:<8} {}", status_icon, provider, status_text);
    }

    println!();
    println!("Available Models by Provider");
    println!("=============================\n");

    for provider in &providers {
        let cp_valid = cp_providers
            .iter()
            .any(|p| p.provider == *provider && p.valid);
        let (local_exists, _) = has_local_credentials(provider);
        let available = cp_valid || local_exists;

        let icon = if available { "✓" } else { "✗" };
        println!("{} {}:", icon, provider);

        match *provider {
            "claude" => {
                for model in CLAUDE_MODELS {
                    let prefix = if available { "  -" } else { "  ·" };
                    println!("{} {}", prefix, model);
                }
            }
            "openai" => {
                for model in OPENAI_MODELS {
                    let prefix = if available { "  -" } else { "  ·" };
                    println!("{} {}", prefix, model);
                }
            }
            "ssh" => {
                if available {
                    println!("  - Git SSH key (for push operations)");
                } else {
                    println!("  · Git SSH key (for push operations)");
                }
            }
            _ => {}
        }
        println!();
    }

    println!("Usage Examples");
    println!("==============\n");

    let has_claude = cp_providers
        .iter()
        .any(|p| p.provider == "claude" && p.valid);
    let has_openai = cp_providers
        .iter()
        .any(|p| p.provider == "openai" && p.valid);

    if has_claude && has_openai {
        println!("Mix providers:");
        println!("  nemo start spec.md --model-impl claude-opus-4 --model-review gpt-5.4");
        println!();
        println!("Use same model for both:");
        println!("  nemo start spec.md --model-impl claude-opus-4 --model-review claude-opus-4");
    } else if has_claude {
        println!("Using Claude (OpenAI not configured):");
        println!("  nemo start spec.md --model-impl claude-opus-4 --model-review claude-sonnet-4");
    } else if has_openai {
        println!("Using OpenAI (Claude not configured):");
        println!("  nemo start spec.md --model-impl gpt-5.4 --model-review gpt-4o");
    } else {
        println!("No providers configured. Authenticate first:");
        println!("  claude login          # For Claude models");
        println!("  # Or create ~/.config/openai/credentials.json with your OpenAI API key");
        println!("  nemo auth             # Push credentials to control plane");
    }

    println!();

    if !any_registered {
        anyhow::bail!("No credentials registered with control plane. Run: nemo auth");
    }

    Ok(())
}
