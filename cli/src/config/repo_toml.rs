//! CLI-side `nemo.toml` parser for the `[server]` section.
//!
//! The CLI does not import the control-plane parser — that would create a
//! tight dependency for what is effectively one section. Instead, this module
//! parses the file with a tolerant schema that only cares about `[server].url`
//! and ignores everything else.
//!
//! This is the writer for `nemo config --local --set server_url=...` as well:
//! it must preserve all other sections when updating `[server].url`, because
//! `nemo.toml` is shared with the control plane (which reads `[repo]`,
//! `[services]`, `[models]`, etc.).
//!
//! See `specs/per-repo-config.md` FR-1, FR-4, FR-8.
#![allow(dead_code)]

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Relative path of the repo-level `nemo.toml`.
pub const NEMO_TOML_RELATIVE_PATH: &str = "nemo.toml";

/// Compute the `nemo.toml` path for a given repo root.
pub fn nemo_toml_path(repo_root: &Path) -> PathBuf {
    repo_root.join(NEMO_TOML_RELATIVE_PATH)
}

/// CLI-side view of `nemo.toml`: only the `[server]` section is modeled.
/// All other sections are tolerated.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RepoToml {
    #[serde(default)]
    pub server: Option<ServerSection>,
}

/// The `[server]` section.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerSection {
    pub url: Option<String>,
}

/// Read `<repo_root>/nemo.toml` if it exists.
///
/// Returns:
/// * `Ok(None)` if the file does not exist.
/// * `Ok(Some(toml))` if the file parses successfully.
/// * `Ok(None)` with a stderr warning if the file exists but fails to parse.
///   Reason: a malformed `nemo.toml` should not prevent the CLI from running
///   (the user can still use env vars or `nemo config --global`). A hard error
///   would be disruptive.
pub fn read_repo_toml(repo_root: &Path) -> Result<Option<RepoToml>> {
    let path = nemo_toml_path(repo_root);
    if !path.exists() {
        return Ok(None);
    }
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    match toml::from_str::<RepoToml>(&contents) {
        Ok(parsed) => Ok(Some(parsed)),
        Err(e) => {
            eprintln!(
                "warning: failed to parse {}: {}. Ignoring for CLI config resolution.",
                path.display(),
                e
            );
            Ok(None)
        }
    }
}

/// Convenience: return the `[server].url` value from `<repo_root>/nemo.toml`,
/// or None if unset / missing / malformed.
pub fn server_url_from_repo_toml(repo_root: &Path) -> Option<String> {
    let toml = read_repo_toml(repo_root).ok().flatten()?;
    let server = toml.server?;
    let url = server.url?;
    let trimmed = url.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Write the `[server].url` value to `<repo_root>/nemo.toml`, preserving all
/// other sections.
///
/// Behavior:
/// * If `nemo.toml` does not exist, creates a minimal file containing only
///   the `[server]` section.
/// * If `nemo.toml` exists, parses it as a generic `toml::Value`, sets
///   `server.url`, and writes the result back atomically (via `.tmp` + rename).
/// * The top-level `[server]` section is a simple two-key subtable
///   `{ url = "..." }`.
pub fn write_server_url(repo_root: &Path, url: &str) -> Result<()> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        bail!("refusing to write empty server_url to nemo.toml");
    }

    let path = nemo_toml_path(repo_root);

    let mut doc: toml::value::Table = if path.exists() {
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        toml::from_str::<toml::value::Table>(&contents)
            .with_context(|| format!("failed to parse existing {}", path.display()))?
    } else {
        toml::value::Table::new()
    };

    let server_entry = doc
        .entry("server".to_string())
        .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));
    let server_table = match server_entry {
        toml::Value::Table(t) => t,
        _ => {
            bail!(
                "nemo.toml has a non-table value for `server`; refusing to overwrite non-table data"
            );
        }
    };
    server_table.insert("url".to_string(), toml::Value::String(trimmed.to_string()));

    let serialized = toml::to_string_pretty(&toml::Value::Table(doc))
        .context("failed to serialize nemo.toml")?;

    let tmp_path = path.with_extension("toml.tmp");
    std::fs::write(&tmp_path, serialized.as_bytes())
        .with_context(|| format!("failed to write {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, &path).with_context(|| {
        format!(
            "failed to rename {} -> {}",
            tmp_path.display(),
            path.display()
        )
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_read_missing_returns_none() {
        let tmp = tempdir().unwrap();
        let result = read_repo_toml(tmp.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_read_with_server_section_parses() {
        let tmp = tempdir().unwrap();
        std::fs::write(
            tmp.path().join("nemo.toml"),
            r#"
[server]
url = "http://100.110.72.64:8080"
"#,
        )
        .unwrap();
        let parsed = read_repo_toml(tmp.path()).unwrap().unwrap();
        assert_eq!(
            parsed.server.unwrap().url.as_deref(),
            Some("http://100.110.72.64:8080")
        );
    }

    #[test]
    fn test_read_tolerates_unrelated_sections() {
        let tmp = tempdir().unwrap();
        std::fs::write(
            tmp.path().join("nemo.toml"),
            r#"
[repo]
name = "myrepo"
default_branch = "main"

[models]
implementor = "claude-opus-4"

[services.api]
path = "api"
test = "cargo test"

[server]
url = "http://fake:1"
"#,
        )
        .unwrap();
        let parsed = read_repo_toml(tmp.path()).unwrap().unwrap();
        assert_eq!(parsed.server.unwrap().url.as_deref(), Some("http://fake:1"));
    }

    #[test]
    fn test_read_missing_server_url_returns_none() {
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join("nemo.toml"), "[server]\n").unwrap();
        let result = server_url_from_repo_toml(tmp.path());
        assert!(result.is_none());
    }

    #[test]
    fn test_server_url_from_repo_toml_returns_value() {
        let tmp = tempdir().unwrap();
        std::fs::write(
            tmp.path().join("nemo.toml"),
            r#"
[server]
url = "http://x:1"
"#,
        )
        .unwrap();
        assert_eq!(
            server_url_from_repo_toml(tmp.path()).as_deref(),
            Some("http://x:1")
        );
    }

    #[test]
    fn test_server_url_trims_whitespace() {
        let tmp = tempdir().unwrap();
        std::fs::write(
            tmp.path().join("nemo.toml"),
            r#"
[server]
url = "  http://x:1  "
"#,
        )
        .unwrap();
        assert_eq!(
            server_url_from_repo_toml(tmp.path()).as_deref(),
            Some("http://x:1")
        );
    }

    #[test]
    fn test_write_creates_minimal_nemo_toml() {
        let tmp = tempdir().unwrap();
        write_server_url(tmp.path(), "http://new:9").unwrap();
        let contents = std::fs::read_to_string(tmp.path().join("nemo.toml")).unwrap();
        assert!(contents.contains("[server]"));
        assert!(contents.contains(r#"url = "http://new:9""#));
    }

    #[test]
    fn test_write_preserves_existing_sections() {
        let tmp = tempdir().unwrap();
        std::fs::write(
            tmp.path().join("nemo.toml"),
            r#"
[repo]
name = "myrepo"
default_branch = "main"

[services.api]
path = "api"
test = "cargo test"
"#,
        )
        .unwrap();
        write_server_url(tmp.path(), "http://new:9").unwrap();
        let contents = std::fs::read_to_string(tmp.path().join("nemo.toml")).unwrap();
        assert!(contents.contains(r#"name = "myrepo""#));
        assert!(contents.contains(r#"path = "api""#));
        assert!(contents.contains(r#"test = "cargo test""#));
        assert!(contents.contains(r#"url = "http://new:9""#));
    }

    #[test]
    fn test_write_updates_existing_server_url() {
        let tmp = tempdir().unwrap();
        std::fs::write(
            tmp.path().join("nemo.toml"),
            r#"
[server]
url = "http://old:1"
"#,
        )
        .unwrap();
        write_server_url(tmp.path(), "http://new:9").unwrap();
        let value = server_url_from_repo_toml(tmp.path()).unwrap();
        assert_eq!(value, "http://new:9");
        // Only one url line
        let contents = std::fs::read_to_string(tmp.path().join("nemo.toml")).unwrap();
        let matches: Vec<_> = contents.matches("url =").collect();
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn test_write_rejects_empty_url() {
        let tmp = tempdir().unwrap();
        assert!(write_server_url(tmp.path(), "").is_err());
        assert!(write_server_url(tmp.path(), "   ").is_err());
    }

    #[test]
    fn test_read_returns_none_on_parse_error() {
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join("nemo.toml"), "[server\nurl = bad").unwrap();
        // Should not panic, returns None (with stderr warning).
        let result = read_repo_toml(tmp.path()).unwrap();
        assert!(result.is_none());
    }
}
