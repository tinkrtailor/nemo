use anyhow::Result;
use serde_json::Value;

use crate::client::NemoClient;

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct CanonicalCodexOauthBundle {
    #[serde(rename = "type")]
    bundle_type: String,
    access: String,
    refresh: String,
    expires: i64,
    #[serde(rename = "accountId", skip_serializing_if = "Option::is_none")]
    account_id: Option<String>,
}

/// JSON output for a single provider auth result.
#[derive(serde::Serialize)]
struct AuthProviderResult {
    provider: String,
    status: String,
    messages: Vec<String>,
}

/// Push local model credentials to the cluster.
///
/// Reads local credential files, validates they exist, and registers them
/// with the control plane so AWAITING_REAUTH loops can recover via `nemo resume`.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    client: &NemoClient,
    engineer: &str,
    name: &str,
    email: &str,
    claude: bool,
    openai: bool,
    ssh: bool,
    json: bool,
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
    let mut json_results: Vec<AuthProviderResult> = Vec::new();

    for provider in &providers {
        let mut messages: Vec<String> = Vec::new();
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
                // OpenCode / Codex / OpenAI credential paths (checked in priority order)
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
                let config_dir =
                    std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| format!("{home}/.config"));
                let candidates = [
                    format!("{home}/.local/share/opencode/auth.json"), // opencode ChatGPT OAuth
                    format!("{home}/.codex/auth.json"),                // codex CLI file cache
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

        // For claude on macOS: if the disk file is missing but the keychain has a
        // fresh entry (Claude Code 2.x writes to keychain and only periodically
        // flushes to disk), use the keychain as a fallback rather than erroring.
        let claude_keychain_fallback: Option<String> =
            if *provider == "claude" && !std::path::Path::new(&cred_path).exists() {
                let now = crate::claude_creds::now_ms();
                match crate::claude_creds::extract_from_keychain() {
                    Some(kc) if !crate::claude_creds::is_bundle_stale(&kc, now) => {
                        eprintln!(
                            "Note: no disk credentials at {cred_path}; using fresh keychain entry."
                        );
                        Some(kc)
                    }
                    _ => None,
                }
            } else {
                None
            };

        if !std::path::Path::new(&cred_path).exists() && claude_keychain_fallback.is_none() {
            if !json {
                eprintln!("No {provider} credentials found at {cred_path}");
                match *provider {
                    "claude" => eprintln!("  Run: claude login"),
                    "openai" => {
                        eprintln!("  Create {cred_path} with your OpenAI API key as content")
                    }
                    "ssh" => eprintln!("  Run: ssh-keygen -t ed25519"),
                    _ => {}
                }
            }
            messages.push("No local token found.".to_string());
            json_results.push(AuthProviderResult {
                provider: provider.to_string(),
                status: "skipped".to_string(),
                messages,
            });
            // If the provider was explicitly requested (not default "all"), treat as error
            if claude || openai || ssh {
                any_error = true;
            }
            continue;
        }

        // Read the credential file. For Claude on macOS, prefer a fresh
        // keychain entry over a stale disk file.
        let content = {
            let file_content = std::fs::read_to_string(&cred_path);
            if *provider == "claude" {
                let now = crate::claude_creds::now_ms();
                let file_stale = match &file_content {
                    Ok(c) => crate::claude_creds::is_bundle_stale(c, now),
                    Err(_) => true,
                };
                if file_stale {
                    match crate::claude_creds::extract_from_keychain() {
                        Some(kc) if !crate::claude_creds::is_bundle_stale(&kc, now) => {
                            let msg = format!(
                                "disk credentials stale at {cred_path}; using fresh keychain entry."
                            );
                            if !json {
                                eprintln!("Note: {msg}");
                            }
                            messages.push(msg);
                            kc
                        }
                        _ => match file_content {
                            Ok(c) => c,
                            Err(e) => {
                                let msg = format!(
                                    "could not read claude credentials at {cred_path} and keychain has no fresh entry: {e}"
                                );
                                if !json {
                                    eprintln!("Warning: {msg}");
                                }
                                messages.push(msg);
                                json_results.push(AuthProviderResult {
                                    provider: provider.to_string(),
                                    status: "error".to_string(),
                                    messages,
                                });
                                any_error = true;
                                continue;
                            }
                        },
                    }
                } else {
                    file_content.unwrap()
                }
            } else {
                match file_content {
                    Ok(c) => c,
                    Err(e) => {
                        let msg =
                            format!("could not read {provider} credentials at {cred_path}: {e}");
                        if !json {
                            eprintln!("Warning: {msg}");
                        }
                        messages.push(msg);
                        json_results.push(AuthProviderResult {
                            provider: provider.to_string(),
                            status: "error".to_string(),
                            messages,
                        });
                        any_error = true;
                        continue;
                    }
                }
            }
        };

        if content.trim().is_empty() {
            let msg = format!("{provider} credentials at {cred_path} are empty");
            if !json {
                eprintln!("Error: {msg}");
            }
            messages.push(msg);
            json_results.push(AuthProviderResult {
                provider: provider.to_string(),
                status: "error".to_string(),
                messages,
            });
            any_error = true;
            continue;
        }

        // For claude/openai, validate content is either valid JSON or a raw API key string.
        if *provider != "ssh" {
            let trimmed = content.trim();
            if trimmed.starts_with('{')
                && serde_json::from_str::<serde_json::Value>(trimmed).is_err()
            {
                let msg = format!("{provider} credentials at {cred_path} contain malformed JSON");
                if !json {
                    eprintln!("Error: {msg}");
                }
                messages.push(msg);
                json_results.push(AuthProviderResult {
                    provider: provider.to_string(),
                    status: "error".to_string(),
                    messages,
                });
                any_error = true;
                continue;
            }
        }

        let normalized_content = if *provider == "openai" {
            normalize_openai_credential(&content)
        } else {
            Ok(content.trim().to_string())
        }?;

        // Register credentials with the control plane
        match client
            .register_credentials(
                engineer,
                provider,
                &normalized_content,
                if name.is_empty() { None } else { Some(name) },
                if email.is_empty() { None } else { Some(email) },
            )
            .await
        {
            Ok(()) => {
                if !json {
                    println!("Registered {provider} credentials with control plane");
                }
                messages.push("Token pushed to cluster.".to_string());
                json_results.push(AuthProviderResult {
                    provider: provider.to_string(),
                    status: "ok".to_string(),
                    messages,
                });
                any_registered = true;
            }
            Err(e) => {
                let msg = format!("Failed to register {provider} credentials: {e}");
                if !json {
                    eprintln!("{msg}");
                    eprintln!(
                        "  Credentials found locally at {cred_path} but could not be pushed."
                    );
                    eprintln!("  Ensure the control plane is reachable and your API key is valid.");
                }
                messages.push(msg);
                json_results.push(AuthProviderResult {
                    provider: provider.to_string(),
                    status: "error".to_string(),
                    messages,
                });
                any_error = true;
            }
        }
    }

    if json {
        let output = serde_json::json!({ "results": json_results });
        println!("{}", serde_json::to_string_pretty(&output)?);
        // Still return error if all failed so exit code is non-zero
        if any_error && !any_registered {
            anyhow::bail!("All credential uploads failed");
        }
        return Ok(());
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

fn normalize_openai_credential(content: &str) -> Result<String> {
    let trimmed = content.trim();
    let parsed = match serde_json::from_str::<Value>(trimmed) {
        Ok(parsed) => parsed,
        Err(_) => return Ok(trimmed.to_string()),
    };

    if let Some(bundle) = extract_codex_oauth_bundle(&parsed) {
        return Ok(serde_json::to_string(&bundle)?);
    }

    if let Some(api_key) = extract_api_key(&parsed) {
        return Ok(api_key);
    }

    Ok(trimmed.to_string())
}

fn extract_codex_oauth_bundle(value: &Value) -> Option<CanonicalCodexOauthBundle> {
    for candidate in [
        Some(value),
        value.get("openai"),
        value.get("chatgptAuthTokens"),
        value.get("chatgpt_auth_tokens"),
    ]
    .into_iter()
    .flatten()
    {
        let access = candidate
            .get("access")
            .or_else(|| candidate.get("access_token"))
            .or_else(|| candidate.get("accessToken"))
            .and_then(Value::as_str);
        let refresh = candidate
            .get("refresh")
            .or_else(|| candidate.get("refresh_token"))
            .or_else(|| candidate.get("refreshToken"))
            .and_then(Value::as_str);
        let (Some(access), Some(refresh)) = (access, refresh) else {
            continue;
        };
        let expires = candidate
            .get("expires")
            .or_else(|| candidate.get("expires_at"))
            .or_else(|| candidate.get("expiresAt"))
            .and_then(Value::as_i64)
            .unwrap_or_default();
        let account_id = candidate
            .get("accountId")
            .or_else(|| candidate.get("account_id"))
            .or_else(|| candidate.get("chatgpt_account_id"))
            .or_else(|| candidate.get("chatgptAccountId"))
            .and_then(Value::as_str)
            .map(str::to_string);

        return Some(CanonicalCodexOauthBundle {
            bundle_type: "oauth".to_string(),
            access: access.to_string(),
            refresh: refresh.to_string(),
            expires,
            account_id,
        });
    }

    None
}

fn extract_api_key(value: &Value) -> Option<String> {
    let candidate = value.get("openai").unwrap_or(value);
    candidate
        .get("api_key")
        .or_else(|| candidate.get("key"))
        .or_else(|| candidate.get("apiKey"))
        .or_else(|| candidate.get("OPENAI_API_KEY"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_openai_credential_extracts_opencode_oauth_bundle() {
        let normalized = normalize_openai_credential(
            r#"{"openai":{"type":"oauth","access":"access-token","refresh":"refresh-token","expires":1776698155357,"accountId":"acct-123"},"moonshotai":{"api_key":"ignore-me"}}"#,
        )
        .expect("normalize");

        assert_eq!(
            normalized,
            r#"{"type":"oauth","access":"access-token","refresh":"refresh-token","expires":1776698155357,"accountId":"acct-123"}"#
        );
    }

    #[test]
    fn normalize_openai_credential_extracts_api_key_from_json() {
        let normalized =
            normalize_openai_credential(r#"{"api_key":"sk-test-key"}"#).expect("normalize");
        assert_eq!(normalized, "sk-test-key");
    }
}
