//! Claude credential freshness preflight (#97).
//!
//! Claude Code on macOS stores its OAuth credentials in the system
//! Keychain, not on disk, and the access token has a ~1 hour expiry.
//! `nemo auth --claude` extracts the keychain entry and pushes it to
//! the control-plane K8s secret, but nothing re-reads the keychain
//! automatically. A loop dispatched with stale creds dies on its very
//! first claude call with `401 Invalid authentication credentials`.
//!
//! This module adds a small preflight that runs before every
//! `nemo harden` / `nemo start` / `nemo ship`. When the cached file
//! is missing, expired, or within a 5-minute buffer of expiry, it
//! re-extracts from the keychain on macOS and pushes the refreshed
//! bundle to the control plane. Linux users whose Claude Code writes
//! the file directly need nothing beyond the freshness check.
//!
//! See issue #97.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::client::NemoClient;

/// Treat a token as stale when it's within this many seconds of expiry.
const EXPIRY_BUFFER_SECS: u64 = 5 * 60;

#[derive(Debug, Deserialize)]
struct ClaudeCredentialsShape {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: ClaudeOauth,
}

#[derive(Debug, Deserialize)]
struct ClaudeOauth {
    /// Epoch milliseconds. Claude Code's bundle uses `expiresAt` at the
    /// millisecond granularity; we treat anything non-numeric as "no
    /// expiry info", which triggers a refresh.
    #[serde(rename = "expiresAt")]
    expires_at: Option<u64>,
}

/// Return the Claude credential file paths `nemo auth --claude` knows
/// about, in priority order. Kept in sync with `cli/src/commands/auth.rs`.
pub fn credential_candidate_paths() -> Vec<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let config_dir = std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| format!("{home}/.config"));
    vec![
        PathBuf::from(&home)
            .join(".claude")
            .join(".credentials.json"),
        PathBuf::from(&config_dir)
            .join("claude-code")
            .join("credentials.json"),
        PathBuf::from(&home)
            .join(".claude")
            .join("credentials.json"),
    ]
}

/// The canonical path we write refreshed credentials to. Matches the
/// `claude-worktree` convention used as the first candidate in
/// `commands/auth.rs`.
pub fn credentials_path() -> PathBuf {
    credential_candidate_paths().into_iter().next().unwrap()
}

/// Return the first credential file that exists on disk, or None if
/// none of the known locations are populated.
fn find_existing_credentials() -> Option<PathBuf> {
    credential_candidate_paths()
        .into_iter()
        .find(|p| p.is_file())
}

/// Decide whether the on-disk credential bundle is stale. Returns
/// true if the file is missing, unparseable, has no expiry, has
/// already expired, or is within EXPIRY_BUFFER_SECS of expiring.
pub fn is_stale(path: &Path, now_ms: u64) -> bool {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return true;
    };
    is_bundle_stale(&contents, now_ms)
}

/// Decide whether a raw Claude credential JSON string represents a
/// stale bundle. Shared between disk and keychain checks so both
/// sources apply the same freshness rule.
fn is_bundle_stale(contents: &str, now_ms: u64) -> bool {
    let Ok(parsed) = serde_json::from_str::<ClaudeCredentialsShape>(contents) else {
        return true;
    };
    let Some(expires_at) = parsed.claude_ai_oauth.expires_at else {
        return true;
    };
    let buffer_ms = EXPIRY_BUFFER_SECS.saturating_mul(1000);
    expires_at.saturating_sub(buffer_ms) <= now_ms
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Extract the Claude Code credential bundle from the macOS keychain.
/// Returns None on non-macOS platforms or when the keychain entry
/// isn't present.
#[cfg(target_os = "macos")]
fn extract_from_keychain() -> Option<String> {
    let output = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8(output.stdout).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(not(target_os = "macos"))]
fn extract_from_keychain() -> Option<String> {
    None
}

/// Atomically write the bundle to `~/.claude/.credentials.json` with
/// mode 0600. Mirrors the pattern used by cli/src/config.rs.
fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");

    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(contents.as_bytes())?;
    }

    #[cfg(not(unix))]
    {
        std::fs::write(&tmp, contents)?;
    }

    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Run the preflight. If the cached bundle is stale and we can
/// extract a fresh one from the keychain, write it and push it to
/// the control plane. Never fails the command — stale-but-present
/// credentials might still work for this one dispatch, and a real
/// auth error will surface in the loop itself with a much clearer
/// error message than we could produce here.
pub async fn ensure_fresh(
    client: &NemoClient,
    engineer: &str,
    name: &str,
    email: &str,
) -> Result<()> {
    // Missing engineer is caught earlier in main.rs, but a paranoid
    // guard here keeps the helper self-contained and testable.
    if engineer.is_empty() {
        return Ok(());
    }

    // Check every known credential path (same list as `nemo auth --claude`),
    // not just the macOS default. A Linux/XDG user whose Claude Code
    // writes to ~/.config/claude-code/credentials.json would otherwise
    // look permanently stale here and the preflight would never run.
    let now = now_ms();
    let existing_path = find_existing_credentials();
    let is_fresh = existing_path
        .as_ref()
        .map(|p| !is_stale(p, now))
        .unwrap_or(false);
    if is_fresh {
        return Ok(());
    }

    let Some(fresh) = extract_from_keychain() else {
        // Not macOS, or keychain entry missing, or extraction failed.
        // On Linux the file on disk IS the source of truth so we trust
        // whatever Claude Code wrote there. On macOS without a
        // keychain entry, the user needs to run `claude login` first.
        tracing::debug!("Claude creds stale but no keychain refresh available; continuing");
        return Ok(());
    };

    // Reject the keychain bundle if IT is also stale — no point
    // overwriting the last known-working server copy with a bundle
    // that will 401 on the first dispatch. Happens when the user
    // hasn't reopened Claude Code since their token expired.
    if is_bundle_stale(&fresh, now) {
        tracing::warn!(
            "Keychain Claude credentials are also expired; not pushing stale bundle. \
             Open Claude Code to refresh, then re-run."
        );
        return Ok(());
    }

    // Only write if the extracted bundle differs from what's on disk.
    // Avoids bumping mtime on every start when the keychain itself
    // happens to match the disk copy exactly.
    let write_path = existing_path.unwrap_or_else(credentials_path);
    let existing = std::fs::read_to_string(&write_path).unwrap_or_default();
    if existing.trim() != fresh.trim() {
        write_atomic(&write_path, &fresh).with_context(|| {
            format!(
                "failed to write refreshed credentials to {}",
                write_path.display()
            )
        })?;
    }

    // Push to the control plane so the next job mount picks it up.
    // Name/email are optional — if blank they'll be ignored by the
    // handler and not overwritten server-side.
    let name_opt = if name.is_empty() { None } else { Some(name) };
    let email_opt = if email.is_empty() { None } else { Some(email) };
    if let Err(e) = client
        .register_credentials(engineer, "claude", &fresh, name_opt, email_opt)
        .await
    {
        // Non-fatal: the loop will either work with the existing
        // server-side credentials or fail with a clearer message
        // from the agent side. Log and move on.
        tracing::warn!(
            error = %e,
            "Could not push refreshed Claude credentials to control plane; continuing with server-side copy"
        );
    } else {
        tracing::info!("Refreshed Claude credentials before dispatch");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_bundle(dir: &Path, expires_at: Option<u64>) -> PathBuf {
        let path = dir.join("creds.json");
        let body = match expires_at {
            Some(e) => format!(r#"{{"claudeAiOauth":{{"expiresAt":{e}}}}}"#),
            None => r#"{"claudeAiOauth":{}}"#.to_string(),
        };
        std::fs::write(&path, body).unwrap();
        path
    }

    static TMPDIR_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn tmpdir() -> PathBuf {
        // Monotonic counter + nanos + pid so parallel tests never
        // collide. A pure timestamp isn't enough on fast machines —
        // two tests can land in the same nanosecond bucket.
        let seq = TMPDIR_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p =
            std::env::temp_dir().join(format!("nemo-creds-{}-{}-{seq}", nanos, std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn missing_file_is_stale() {
        let dir = tmpdir();
        let path = dir.join("does-not-exist.json");
        assert!(is_stale(&path, 0));
    }

    #[test]
    fn unparseable_file_is_stale() {
        let dir = tmpdir();
        let path = dir.join("bad.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(is_stale(&path, 0));
    }

    #[test]
    fn no_expires_at_is_stale() {
        let dir = tmpdir();
        let path = write_bundle(&dir, None);
        assert!(is_stale(&path, 1_000_000));
    }

    #[test]
    fn fresh_token_is_not_stale() {
        let dir = tmpdir();
        // expires one hour from now_ms
        let now = 1_000_000_000u64;
        let path = write_bundle(&dir, Some(now + 60 * 60 * 1000));
        assert!(!is_stale(&path, now));
    }

    #[test]
    fn expired_token_is_stale() {
        let dir = tmpdir();
        let now = 1_000_000_000u64;
        let path = write_bundle(&dir, Some(now - 1000));
        assert!(is_stale(&path, now));
    }

    #[test]
    fn within_buffer_is_stale() {
        let dir = tmpdir();
        let now = 1_000_000_000u64;
        // expires in 4 minutes — inside the 5-minute buffer
        let path = write_bundle(&dir, Some(now + 4 * 60 * 1000));
        assert!(is_stale(&path, now));
    }

    #[test]
    fn just_outside_buffer_is_fresh() {
        let dir = tmpdir();
        let now = 1_000_000_000u64;
        // expires in 6 minutes — outside the 5-minute buffer
        let path = write_bundle(&dir, Some(now + 6 * 60 * 1000));
        assert!(!is_stale(&path, now));
    }

    #[test]
    fn is_bundle_stale_treats_expired_string_as_stale() {
        let now = 1_000_000_000u64;
        let expired = format!(r#"{{"claudeAiOauth":{{"expiresAt":{}}}}}"#, now - 1000);
        assert!(is_bundle_stale(&expired, now));
        let fresh = format!(
            r#"{{"claudeAiOauth":{{"expiresAt":{}}}}}"#,
            now + 60 * 60 * 1000
        );
        assert!(!is_bundle_stale(&fresh, now));
    }

    #[test]
    fn candidate_paths_match_auth_command() {
        // Regression guard: if cli/src/commands/auth.rs ever gains a
        // new Claude path, add it here too or the preflight will
        // silently skip it. Kept as a structural assertion rather
        // than a string match so path separators don't make it fragile.
        let paths = credential_candidate_paths();
        assert_eq!(paths.len(), 3, "three known Claude credential locations");
        assert!(paths[0].ends_with(".claude/.credentials.json"));
        assert!(paths[1].ends_with("claude-code/credentials.json"));
        assert!(paths[2].ends_with(".claude/credentials.json"));
    }
}
