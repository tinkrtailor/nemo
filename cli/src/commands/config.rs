//! `nemo config` command.
//!
//! Per `specs/per-repo-config.md`, this command reads and writes configuration
//! across three locations:
//!
//! * `~/.nemo/config.toml` — global identity + legacy fallback for url/api_key
//! * `<repo>/nemo.toml` `[server].url` — per-repo server URL
//! * `<repo>/.nemo/credentials` — per-repo API key (mode 0600)
//!
//! Scope resolution for `--set` (when neither `--local` nor `--global` given):
//! * `server_url`, `api_key` → local if inside a repo, else global
//! * `engineer`, `name`, `email` → always global (per-user identity)
//!
//! `--local --set engineer=...` is an error (FR-10): identity is per-user.
//! `--global --set server_url=...` prints a legacy-fallback warning (FR-11).

use anyhow::{Result, bail};

use crate::config::credentials;
use crate::config::repo_toml;
use crate::config::sources::{self, ResolveInputs, ResolvedConfig};

/// Which scope to write a key to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Local,
    Global,
}

/// Kind of config key, used for scope rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyKind {
    /// Per-repo-scoped key: `server_url`, `api_key`.
    PerRepo,
    /// Identity key: `engineer`, `name`, `email`.
    Identity,
}

fn classify_key(key: &str) -> Result<KeyKind> {
    match key {
        "server_url" | "api_key" => Ok(KeyKind::PerRepo),
        "engineer" | "name" | "email" => Ok(KeyKind::Identity),
        other => bail!("Unknown config key: {other}"),
    }
}

/// Pure helper that decides the scope for a `--set key=value` call.
///
/// * `--local` and `--global` are the explicit overrides.
/// * If neither is set, fall back to auto-scope rules:
///   - per-repo keys inside a repo → Local
///   - per-repo keys outside a repo → Global
///   - identity keys → always Global
///
/// Returns an error for illegal combinations like `--local engineer=...`.
fn resolve_scope(kind: KeyKind, local: bool, global: bool, inside_repo: bool) -> Result<Scope> {
    if local && global {
        bail!("cannot use --local and --global together");
    }
    if local {
        if matches!(kind, KeyKind::Identity) {
            bail!("identity (engineer/name/email) is per-user; use --global or omit the flag");
        }
        if !inside_repo {
            bail!(
                "--local requires running from inside a repo (a dir containing nemo.toml or .git)"
            );
        }
        return Ok(Scope::Local);
    }
    if global {
        return Ok(Scope::Global);
    }
    // Auto-scope
    Ok(match kind {
        KeyKind::PerRepo if inside_repo => Scope::Local,
        _ => Scope::Global,
    })
}

/// Entry point for `nemo config`.
pub fn run(
    cli_server: Option<&str>,
    set: Option<String>,
    get: Option<String>,
    local: bool,
    global: bool,
) -> Result<()> {
    if set.is_some() && get.is_some() {
        bail!("Cannot use --set and --get together");
    }

    // Resolve once for display and to determine the repo root.
    let resolved = resolve_for_display(cli_server);

    if let Some(key) = get {
        return handle_get(&resolved, &key);
    }

    if let Some(kv) = set {
        return handle_set(&resolved, &kv, local, global);
    }

    // No --set, no --get → display mode.
    display(&resolved);
    Ok(())
}

/// Best-effort resolution for display. Never propagates resolver errors
/// (e.g., `current_dir` failure) — we want `nemo config` to still show
/// what it can.
fn resolve_for_display(cli_server: Option<&str>) -> ResolvedConfig {
    // sources::resolve reads the real env/fs. If current_dir() fails or the
    // global file is malformed, fall back to a synthetic "all default" config
    // so display still works.
    match sources::resolve(cli_server) {
        Ok(r) => r,
        Err(_) => {
            let global = crate::config::EngineerConfig::default();
            sources::resolve_from(ResolveInputs {
                cli_server: cli_server.map(|s| s.to_string()),
                env_server: None,
                env_api_key: None,
                repo_root: None,
                global: &global,
            })
        }
    }
}

fn handle_get(resolved: &ResolvedConfig, key: &str) -> Result<()> {
    match key {
        "server_url" => println!("{}", resolved.server_url.value),
        "engineer" => println!("{}", resolved.engineer.value),
        "name" => println!("{}", resolved.name.value),
        "email" => println!("{}", resolved.email.value),
        "api_key" => match resolved.api_key.as_ref() {
            Some(r) => println!("{}", mask_api_key(&r.value)),
            None => println!("(not set)"),
        },
        other => bail!("Unknown config key: {other}"),
    }
    Ok(())
}

fn handle_set(resolved: &ResolvedConfig, kv: &str, local: bool, global: bool) -> Result<()> {
    let (key, value) = parse_key_value(kv)?;
    let kind = classify_key(key)?;
    let inside_repo = resolved.repo_root.is_some();
    let scope = resolve_scope(kind, local, global, inside_repo)?;

    match (scope, key) {
        (Scope::Local, "server_url") => {
            let repo_root = resolved
                .repo_root
                .as_ref()
                .expect("local scope requires repo_root");
            repo_toml::write_server_url(repo_root, value)?;
            println!(
                "Wrote [server].url to {}",
                repo_toml::nemo_toml_path(repo_root).display()
            );
        }
        (Scope::Local, "api_key") => {
            let repo_root = resolved
                .repo_root
                .as_ref()
                .expect("local scope requires repo_root");
            credentials::write_credentials(repo_root, value)?;
            println!(
                "Wrote api_key to {} (mode 0600)",
                credentials::credentials_path(repo_root).display()
            );
        }
        (Scope::Global, _) => {
            if matches!(kind, KeyKind::PerRepo) {
                eprintln!(
                    "note: {key} in the global file (~/.nemo/config.toml) is the legacy \
                     fallback. Prefer `nemo config --local --set {key}=...` in your repo."
                );
            }
            write_global(key, value)?;
            if key == "api_key" {
                println!("Set {key} = ****");
            } else {
                println!("Set {key} = {value}");
            }
        }
        (Scope::Local, other) => {
            // resolve_scope already rejects Identity+Local, and PerRepo keys
            // only include server_url/api_key, so this arm is unreachable.
            unreachable!("unexpected local key: {other}");
        }
    }
    Ok(())
}

fn parse_key_value(kv: &str) -> Result<(&str, &str)> {
    let parts: Vec<&str> = kv.splitn(2, '=').collect();
    if parts.len() != 2 {
        bail!("Expected format: key=value");
    }
    Ok((parts[0], parts[1]))
}

/// Write a key to the global `~/.nemo/config.toml`. Loads existing values
/// first (with malformed-file recovery), mutates, and saves atomically.
fn write_global(key: &str, value: &str) -> Result<()> {
    let mut config = match crate::config::load_config() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Warning: existing config is malformed ({e}), starting from defaults");
            eprintln!("Other settings may be lost. Re-set them after this operation.");
            crate::config::EngineerConfig::default()
        }
    };

    match key {
        "server_url" => config.server_url = value.to_string(),
        "engineer" => config.engineer = value.to_string(),
        "name" => config.name = value.to_string(),
        "email" => config.email = value.to_string(),
        "api_key" => {
            if value.is_empty() {
                config.api_key = None;
                crate::config::save_config(&config)?;
                println!("Cleared api_key");
                return Ok(());
            }
            config.api_key = Some(value.to_string());
        }
        other => bail!("Unknown config key: {other}"),
    }

    crate::config::save_config(&config)?;
    Ok(())
}

/// Display the fully resolved config with provenance per field.
fn display(resolved: &ResolvedConfig) {
    println!("Nemo CLI Configuration");
    if let Some(root) = resolved.repo_root.as_ref() {
        println!("  repo_root:  {}", root.display());
    } else {
        println!("  repo_root:  (not in a repo)");
    }
    println!(
        "  server_url: {} ({})",
        resolved.server_url.value,
        resolved.server_url.source.label()
    );
    match resolved.api_key.as_ref() {
        Some(r) => println!(
            "  api_key:    {} ({})",
            mask_api_key(&r.value),
            r.source.label()
        ),
        None => println!("  api_key:    (not set)"),
    }
    println!(
        "  engineer:   {} ({})",
        display_or_missing(&resolved.engineer.value),
        resolved.engineer.source.label()
    );
    println!(
        "  name:       {} ({})",
        display_or_missing(&resolved.name.value),
        resolved.name.source.label()
    );
    println!(
        "  email:      {} ({})",
        display_or_missing(&resolved.email.value),
        resolved.email.source.label()
    );
}

fn display_or_missing(v: &str) -> &str {
    if v.is_empty() { "(not set)" } else { v }
}

/// Mask sensitive value using chars() to handle non-ASCII safely.
/// Matches the existing masking behavior from the previous implementation.
fn mask_api_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() > 12 {
        let prefix: String = chars[..4].iter().collect();
        let suffix: String = chars[chars.len() - 4..].iter().collect();
        format!("{prefix}...{suffix}")
    } else {
        "****".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::engineer::EngineerConfig;
    use std::path::Path;

    // ---- resolve_scope / classify_key (pure) ----

    #[test]
    fn test_classify_per_repo_keys() {
        assert_eq!(classify_key("server_url").unwrap(), KeyKind::PerRepo);
        assert_eq!(classify_key("api_key").unwrap(), KeyKind::PerRepo);
    }

    #[test]
    fn test_classify_identity_keys() {
        assert_eq!(classify_key("engineer").unwrap(), KeyKind::Identity);
        assert_eq!(classify_key("name").unwrap(), KeyKind::Identity);
        assert_eq!(classify_key("email").unwrap(), KeyKind::Identity);
    }

    #[test]
    fn test_classify_unknown_key_errors() {
        assert!(classify_key("foo").is_err());
    }

    #[test]
    fn test_scope_local_and_global_mutually_exclusive() {
        let err = resolve_scope(KeyKind::PerRepo, true, true, true).unwrap_err();
        assert!(
            err.to_string()
                .contains("cannot use --local and --global together")
        );
    }

    #[test]
    fn test_scope_local_identity_rejected() {
        let err = resolve_scope(KeyKind::Identity, true, false, true).unwrap_err();
        assert!(err.to_string().contains("identity"));
    }

    #[test]
    fn test_scope_local_outside_repo_rejected() {
        let err = resolve_scope(KeyKind::PerRepo, true, false, false).unwrap_err();
        assert!(err.to_string().contains("--local requires"));
    }

    #[test]
    fn test_scope_local_per_repo_inside_repo_succeeds() {
        let scope = resolve_scope(KeyKind::PerRepo, true, false, true).unwrap();
        assert_eq!(scope, Scope::Local);
    }

    #[test]
    fn test_scope_global_per_repo_succeeds() {
        let scope = resolve_scope(KeyKind::PerRepo, false, true, true).unwrap();
        assert_eq!(scope, Scope::Global);
    }

    #[test]
    fn test_scope_auto_inside_repo_per_repo_key_is_local() {
        let scope = resolve_scope(KeyKind::PerRepo, false, false, true).unwrap();
        assert_eq!(scope, Scope::Local);
    }

    #[test]
    fn test_scope_auto_outside_repo_per_repo_key_is_global() {
        let scope = resolve_scope(KeyKind::PerRepo, false, false, false).unwrap();
        assert_eq!(scope, Scope::Global);
    }

    #[test]
    fn test_scope_auto_identity_key_is_always_global() {
        assert_eq!(
            resolve_scope(KeyKind::Identity, false, false, true).unwrap(),
            Scope::Global
        );
        assert_eq!(
            resolve_scope(KeyKind::Identity, false, false, false).unwrap(),
            Scope::Global
        );
    }

    // ---- handle_set against a tempdir repo root (local writes) ----

    fn empty_global() -> EngineerConfig {
        EngineerConfig {
            server_url: sources::DEFAULT_SERVER_URL.to_string(),
            engineer: String::new(),
            name: String::new(),
            email: String::new(),
            api_key: None,
        }
    }

    fn resolved_with_repo(repo_root: &Path) -> ResolvedConfig {
        let global = empty_global();
        sources::resolve_from(ResolveInputs {
            cli_server: None,
            env_server: None,
            env_api_key: None,
            repo_root: Some(repo_root),
            global: &global,
        })
    }

    #[test]
    fn test_set_local_server_url_writes_to_nemo_toml() {
        sources::reset_shadow_warning_gate();
        let tmp = tempfile::tempdir().unwrap();
        let resolved = resolved_with_repo(tmp.path());
        handle_set(&resolved, "server_url=http://fake:1", true, false).unwrap();
        let read = repo_toml::server_url_from_repo_toml(tmp.path()).unwrap();
        assert_eq!(read, "http://fake:1");
    }

    #[test]
    fn test_set_local_api_key_writes_credentials_file() {
        sources::reset_shadow_warning_gate();
        let tmp = tempfile::tempdir().unwrap();
        let resolved = resolved_with_repo(tmp.path());
        handle_set(&resolved, "api_key=abc123", true, false).unwrap();
        let read = credentials::read_credentials(tmp.path()).unwrap();
        assert_eq!(read.as_deref(), Some("abc123"));
    }

    #[cfg(unix)]
    #[test]
    fn test_set_local_api_key_is_mode_0600() {
        use std::os::unix::fs::MetadataExt;
        sources::reset_shadow_warning_gate();
        let tmp = tempfile::tempdir().unwrap();
        let resolved = resolved_with_repo(tmp.path());
        handle_set(&resolved, "api_key=abc123", true, false).unwrap();
        let path = credentials::credentials_path(tmp.path());
        let mode = std::fs::metadata(&path).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn test_set_local_identity_key_fails_with_clear_error() {
        sources::reset_shadow_warning_gate();
        let tmp = tempfile::tempdir().unwrap();
        let resolved = resolved_with_repo(tmp.path());
        let err = handle_set(&resolved, "engineer=alice", true, false).unwrap_err();
        assert!(err.to_string().contains("identity"));
    }

    #[test]
    fn test_parse_key_value_splits_on_first_equals() {
        let (k, v) = parse_key_value("api_key=abc=def=ghi").unwrap();
        assert_eq!(k, "api_key");
        assert_eq!(v, "abc=def=ghi");
    }

    #[test]
    fn test_parse_key_value_rejects_missing_equals() {
        assert!(parse_key_value("key_without_value").is_err());
    }

    #[test]
    fn test_mask_api_key_short() {
        assert_eq!(mask_api_key("short"), "****");
        assert_eq!(mask_api_key(""), "****");
    }

    #[test]
    fn test_mask_api_key_long() {
        assert_eq!(mask_api_key("sk-abcdefghijklmnop"), "sk-a...mnop");
    }

    #[test]
    fn test_mask_api_key_handles_unicode() {
        // 13 chars, should mask with first-4/last-4
        let key = "abcd\u{00e9}fghij\u{00e9}kl";
        let masked = mask_api_key(key);
        assert!(masked.starts_with("abcd"));
        assert!(masked.ends_with("j\u{00e9}kl"));
    }
}
