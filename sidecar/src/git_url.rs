//! Parse `GIT_REPO_URL` per FR-24.
//!
//! Three formats:
//!
//! 1. `ssh://[user@]host[:port]/path`
//! 2. scp-style `user@host:path`
//! 3. `https://host/path`
//!
//! Edge cases:
//!
//! - Control characters (`\t`, `\n`, `\r`) → parse error.
//! - Percent-encoded bytes → parse error (treated as unparseable to avoid
//!   ambiguity in host interpretation).
//! - Missing host → parse error.
//! - `repo_path` strips a leading `/` only.

use thiserror::Error;

/// Parsed representation of `GIT_REPO_URL` suitable for the upstream SSH
/// proxy. The upstream always authenticates as user `git` (FR-14), so we
/// deliberately do not store the userinfo portion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRemote {
    /// Hostname of the upstream git server (e.g. `github.com`).
    pub host: String,
    /// Port of the upstream git server. Defaults to 22 when not specified.
    pub port: u16,
    /// Repository path on the upstream server with any leading `/` stripped
    /// (e.g. `reitun/virdismat-mono.git`).
    pub repo_path: String,
}

/// Errors returned by [`parse`].
#[derive(Debug, Error)]
pub enum GitUrlError {
    /// The URL contained disallowed characters (control chars, percent-
    /// encoding, etc.) or was otherwise structurally invalid.
    #[error("invalid GIT_REPO_URL: {0}")]
    Invalid(String),
}

/// Parse `GIT_REPO_URL`.
pub fn parse(url: &str) -> Result<GitRemote, GitUrlError> {
    // Reject control characters unconditionally — they could smuggle
    // newlines into downstream SSH commands.
    if url
        .chars()
        .any(|c| c == '\t' || c == '\n' || c == '\r' || c == '\0')
    {
        return Err(GitUrlError::Invalid(
            "control characters are not allowed".to_string(),
        ));
    }
    // Reject percent-encoded bytes. A percent-encoded host allows
    // ambiguous parsing (e.g. `github.com%2Fevil.com`) that could trick
    // downstream consumers.
    if url.contains('%') {
        return Err(GitUrlError::Invalid(
            "percent-encoded bytes are not allowed in GIT_REPO_URL".to_string(),
        ));
    }

    // Try ssh://…
    if let Some(rest) = url.strip_prefix("ssh://") {
        return parse_scheme_url(rest, 22);
    }
    // Try https://…
    if let Some(rest) = url.strip_prefix("https://") {
        return parse_scheme_url(rest, 22);
    }
    // Try scp-style user@host:path
    if let Some(at) = url.find('@')
        && !url.contains("://")
    {
        let after_at = &url[at + 1..];
        return parse_scp_style(after_at);
    }
    Err(GitUrlError::Invalid(format!(
        "unrecognized GIT_REPO_URL format: {url}"
    )))
}

/// Parse the portion after `ssh://` or `https://`. `default_port` is
/// used when the authority has no explicit port.
fn parse_scheme_url(rest: &str, default_port: u16) -> Result<GitRemote, GitUrlError> {
    // Split into authority + path.
    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, ""),
    };

    // Strip any userinfo — FR-14 says we always authenticate as `git`,
    // so any user@ here is intentionally discarded.
    let host_port = match authority.find('@') {
        Some(idx) => &authority[idx + 1..],
        None => authority,
    };

    if host_port.is_empty() {
        return Err(GitUrlError::Invalid(
            "missing host in GIT_REPO_URL".to_string(),
        ));
    }

    // host:port split. Ignore IPv6-literal `[::1]:22` because the spec
    // only targets named upstream hosts (github.com etc.); we reject
    // anything that looks like a bracketed IPv6 literal defensively.
    if host_port.starts_with('[') {
        return Err(GitUrlError::Invalid(
            "bracketed IPv6 literals are not supported in GIT_REPO_URL".to_string(),
        ));
    }

    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => {
            let parsed_port: u16 = p
                .parse()
                .map_err(|_| GitUrlError::Invalid(format!("invalid port in GIT_REPO_URL: {p}")))?;
            (h, parsed_port)
        }
        None => (host_port, default_port),
    };

    if host.is_empty() {
        return Err(GitUrlError::Invalid(
            "missing host in GIT_REPO_URL".to_string(),
        ));
    }

    let repo_path = path.trim_start_matches('/').to_string();
    if repo_path.is_empty() {
        return Err(GitUrlError::Invalid(
            "missing repository path in GIT_REPO_URL".to_string(),
        ));
    }

    Ok(GitRemote {
        host: host.to_string(),
        port,
        repo_path,
    })
}

/// Parse the portion after `@` in `user@host:path` form.
fn parse_scp_style(after_at: &str) -> Result<GitRemote, GitUrlError> {
    // scp-style uses a literal `:` (no port number).
    let (host, path) = after_at
        .split_once(':')
        .ok_or_else(|| GitUrlError::Invalid("scp-style URL missing `:`".to_string()))?;

    if host.is_empty() {
        return Err(GitUrlError::Invalid(
            "missing host in GIT_REPO_URL".to_string(),
        ));
    }
    if path.is_empty() {
        return Err(GitUrlError::Invalid(
            "missing repository path in GIT_REPO_URL".to_string(),
        ));
    }

    Ok(GitRemote {
        host: host.to_string(),
        port: 22,
        repo_path: path.trim_start_matches('/').to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_scp_style() {
        let remote =
            parse("git@github.com:reitun/virdismat-mono.git").expect("scp-style URL must parse");
        assert_eq!(
            remote,
            GitRemote {
                host: "github.com".to_string(),
                port: 22,
                repo_path: "reitun/virdismat-mono.git".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_scp_style_without_user_rejected() {
        // scp-style requires `@` per spec. A bare `host:path` is
        // ambiguous with `ssh://host:port/path` so we reject it.
        let err = parse("github.com:reitun/virdismat-mono.git").unwrap_err();
        assert!(matches!(err, GitUrlError::Invalid(_)));
    }

    #[test]
    fn test_parse_ssh_url() {
        let remote =
            parse("ssh://git@github.com/reitun/virdismat-mono.git").expect("ssh URL must parse");
        assert_eq!(
            remote,
            GitRemote {
                host: "github.com".to_string(),
                port: 22,
                repo_path: "reitun/virdismat-mono.git".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_ssh_url_with_port() {
        let remote = parse("ssh://git@git.example.com:2222/reitun/repo.git")
            .expect("ssh URL with port must parse");
        assert_eq!(
            remote,
            GitRemote {
                host: "git.example.com".to_string(),
                port: 2222,
                repo_path: "reitun/repo.git".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_https_url() {
        let remote =
            parse("https://github.com/reitun/virdismat-mono.git").expect("https URL must parse");
        assert_eq!(
            remote,
            GitRemote {
                host: "github.com".to_string(),
                port: 22,
                repo_path: "reitun/virdismat-mono.git".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_rejects_control_characters() {
        assert!(matches!(
            parse("git@github.com:repo.git\n"),
            Err(GitUrlError::Invalid(_))
        ));
        assert!(matches!(
            parse("ssh://git@git\tgithub.com/repo.git"),
            Err(GitUrlError::Invalid(_))
        ));
        assert!(matches!(
            parse("git@github.com:repo\rgit"),
            Err(GitUrlError::Invalid(_))
        ));
    }

    #[test]
    fn test_parse_rejects_percent_encoded_host() {
        assert!(matches!(
            parse("https://github.com%2Fevil.com/repo.git"),
            Err(GitUrlError::Invalid(_))
        ));
    }

    #[test]
    fn test_parse_invalid_returns_error() {
        assert!(matches!(parse(""), Err(GitUrlError::Invalid(_))));
        assert!(matches!(parse("not-a-url"), Err(GitUrlError::Invalid(_))));
        assert!(matches!(parse("ssh://"), Err(GitUrlError::Invalid(_))));
        assert!(matches!(
            parse("ssh:///repo.git"),
            Err(GitUrlError::Invalid(_))
        ));
        assert!(matches!(
            parse("ssh://github.com"),
            Err(GitUrlError::Invalid(_))
        ));
    }

    #[test]
    fn test_upstream_user_always_git_regardless_of_userinfo() {
        // Userinfo is parsed and discarded — we never store it, so the
        // upstream connection code literally cannot choose a different
        // user. This test proves the parser strips it.
        let with_user =
            parse("ssh://notgit@github.com/repo.git").expect("URL with user must parse");
        let without_user = parse("ssh://github.com/repo.git").expect("URL without user must parse");
        assert_eq!(with_user.host, without_user.host);
        assert_eq!(with_user.port, without_user.port);
        assert_eq!(with_user.repo_path, without_user.repo_path);
    }
}
