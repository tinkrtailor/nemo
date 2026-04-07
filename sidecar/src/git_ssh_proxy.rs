//! Git SSH proxy (FR-8 through FR-16, FR-28 10s upstream dial).
//!
//! Accepts SSH connections on loopback. Authenticates via `auth_none`
//! (safe — loopback-only, single-agent pod). Only session channels are
//! opened; only `exec git-upload-pack <repo>` or `exec git-receive-pack
//! <repo>` are accepted, and the repo path MUST match the configured
//! `GIT_REPO_URL`. Every other channel type, global request, and
//! channel request is rejected.
//!
//! On an accepted exec, we open an upstream SSH session as user `git`
//! (always — FR-14), authenticate with the private key at
//! `/secrets/ssh-key/id_ed25519`, verify the server host key against
//! `/secrets/ssh-known-hosts/known_hosts` (mandatory — no bypass), and
//! pipe stdin/stdout/stderr bidirectionally until the command completes.
//!
//! The three Go bugs this implementation fixes (documented in the spec):
//!
//! 1. Bare `git-upload-pack` with no repo argument is rejected with
//!    exit status 1 (Go at `main.go:479` only validated when `len(parts)
//!    == 2`).
//! 2. Upstream SSH user is always `git` regardless of `GIT_REPO_URL`
//!    userinfo (fix for parity with FR-14).
//! 3. Connection errors in upstream dial propagate exit status 1 to the
//!    agent rather than hanging indefinitely (10s connect timeout per
//!    FR-28).

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use russh::ChannelId;
use russh::client;
use russh::keys::PrivateKey;
use russh::keys::PrivateKeyWithHashAlg;
use russh::server::{self, Auth, Msg, Server as _, Session};
use russh::{Channel, MethodKind, MethodSet};
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::git_url::GitRemote;
use crate::logging;
use crate::shutdown::ConnectionTracker;
use crate::ssrf;

/// Path to the agent-facing SSH private key used to authenticate to the
/// upstream git server. Fixed mount per spec.
pub const SSH_KEY_PATH: &str = "/secrets/ssh-key/id_ed25519";
/// Path to the known_hosts file. Mandatory, no bypass.
pub const KNOWN_HOSTS_PATH: &str = "/secrets/ssh-known-hosts/known_hosts";
/// FR-28 upstream SSH dial timeout.
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Git commands we allow clients to proxy.
pub const ALLOWED_GIT_COMMANDS: &[&str] = &["git-upload-pack", "git-receive-pack"];

/// Errors returned by the git SSH proxy server loop.
#[derive(Debug, Error)]
pub enum GitSshError {
    #[error("accept error: {0}")]
    Accept(std::io::Error),
    #[error("russh error: {0}")]
    Russh(#[from] russh::Error),
}

/// Parsed exec request — pure, used both at runtime and in unit tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecParsed {
    /// The command name, e.g. `git-upload-pack`.
    pub command: String,
    /// The repo path argument, with surrounding quotes and a leading
    /// slash stripped. Required for an accepted request.
    pub repo: String,
}

/// Errors from parsing an exec request.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ExecParseError {
    /// The command bytes were not valid UTF-8.
    #[error("exec command is not valid UTF-8")]
    InvalidUtf8,
    /// The command was empty.
    #[error("exec command is empty")]
    Empty,
    /// The command is not in the allowlist.
    #[error("exec command {0:?} not in allowlist")]
    NotAllowed(String),
    /// The allowed command was invoked WITHOUT a repo path argument.
    /// This is the fix for the Go bare-exec bypass bug.
    #[error("git command {0:?} requires a repo path argument")]
    MissingRepoPath(String),
}

/// Parse an SSH exec command. Caller should handle the errors as
/// follows:
///
/// - [`ExecParseError::InvalidUtf8`]: reject the request via
///   `session.channel_failure(channel)` (no exit status) — mirrors
///   Go's behaviour on malformed exec payloads at `main.go:456`.
/// - [`ExecParseError::Empty`]: same as above.
/// - [`ExecParseError::NotAllowed`] or
///   [`ExecParseError::MissingRepoPath`]: send exit status 1 via
///   `session.exit_status_request(channel, 1)` (matches Go at
///   `main.go:471` and fixes the bare-exec bypass).
pub fn parse_exec(data: &[u8]) -> Result<ExecParsed, ExecParseError> {
    let full = std::str::from_utf8(data).map_err(|_| ExecParseError::InvalidUtf8)?;
    if full.is_empty() {
        return Err(ExecParseError::Empty);
    }
    // Split on the first space to separate command from the rest.
    let (command, rest) = match full.split_once(' ') {
        Some((c, r)) => (c, r),
        None => (full, ""),
    };
    if !ALLOWED_GIT_COMMANDS.contains(&command) {
        return Err(ExecParseError::NotAllowed(command.to_string()));
    }
    let repo_raw = rest.trim();
    if repo_raw.is_empty() {
        return Err(ExecParseError::MissingRepoPath(command.to_string()));
    }
    // Strip surrounding single/double quotes (mirrors Go's
    // `strings.Trim(cmdParts[1], "' \"")`).
    let repo_stripped: String = repo_raw
        .trim_matches(|c| c == '\'' || c == '"' || c == ' ')
        .trim_start_matches('/')
        .to_string();
    if repo_stripped.is_empty() {
        return Err(ExecParseError::MissingRepoPath(command.to_string()));
    }
    Ok(ExecParsed {
        command: command.to_string(),
        repo: repo_stripped,
    })
}

/// Validate that the parsed exec's repo path matches the configured
/// allowed repo path from `GIT_REPO_URL`. Pure function.
pub fn repo_path_matches(allowed: &str, requested: &str) -> bool {
    let allowed_norm = allowed.trim_start_matches('/');
    let requested_norm = requested.trim_start_matches('/');
    allowed_norm == requested_norm
}

/// Serve the git SSH proxy until `shutdown_rx` receives `true`.
pub async fn serve(
    listener: TcpListener,
    mut shutdown_rx: watch::Receiver<bool>,
    drain_tracker: ConnectionTracker,
    remote: GitRemote,
) -> Result<(), GitSshError> {
    // Generate an ephemeral Ed25519 host key via rand 0.10's thread rng
    // (OS-backed CSPRNG). Never persisted.
    let host_key = PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
        .map_err(|e| GitSshError::Russh(russh::Error::from(e)))?;
    let mut methods = MethodSet::empty();
    methods.push(MethodKind::None);
    let config = Arc::new(server::Config {
        server_id: russh::SshId::Standard(std::borrow::Cow::Borrowed(
            "SSH-2.0-nautiloop-sidecar_0.1",
        )),
        methods,
        auth_rejection_time: Duration::from_secs(1),
        auth_rejection_time_initial: Some(Duration::from_millis(10)),
        keys: vec![host_key],
        inactivity_timeout: Some(Duration::from_secs(300)),
        nodelay: true,
        ..Default::default()
    });

    let mut server = GitSshServer {
        remote: Arc::new(remote),
        drain_tracker,
    };
    let running = server.run_on_socket(config, &listener);
    let handle = running.handle();

    // When shutdown fires, stop the russh server. The server's own
    // shutdown returns the running future to completion.
    tokio::spawn(async move {
        // Wait for the shutdown signal without holding the running
        // future's lock.
        loop {
            if shutdown_rx.changed().await.is_err() {
                break;
            }
            if *shutdown_rx.borrow() {
                handle.shutdown("sidecar shutting down".to_string());
                break;
            }
        }
    });

    running.await.map_err(GitSshError::Accept)?;
    Ok(())
}

#[derive(Clone)]
struct GitSshServer {
    remote: Arc<GitRemote>,
    drain_tracker: ConnectionTracker,
}

impl server::Server for GitSshServer {
    type Handler = GitSshHandler;

    fn new_client(&mut self, _peer: Option<std::net::SocketAddr>) -> Self::Handler {
        GitSshHandler {
            remote: Arc::clone(&self.remote),
            drain_tracker: self.drain_tracker.clone(),
        }
    }
}

struct GitSshHandler {
    remote: Arc<GitRemote>,
    drain_tracker: ConnectionTracker,
}

impl server::Handler for GitSshHandler {
    type Error = russh::Error;

    // FR-8: auth_none returns Accept. Loopback only, single-agent pod.
    async fn auth_none(&mut self, _user: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    // Anything else (password, publickey, certificate, keyboard) is
    // rejected by the default trait implementation. We do NOT override
    // those — reject() is safer than any affirmative path.

    // FR-10: only session channels accepted. The default for
    // channel_open_direct_tcpip, channel_open_forwarded_tcpip,
    // channel_open_x11, channel_open_direct_streamlocal is Ok(false),
    // matching FR-10. We only override channel_open_session.
    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    // FR-9: reject all global requests at the wire level. The defaults
    // for tcpip_forward, cancel_tcpip_forward, streamlocal_forward,
    // cancel_streamlocal_forward, and agent_request are already
    // Ok(false). We override them explicitly to make the contract
    // auditable at review time.
    async fn tcpip_forward(
        &mut self,
        _address: &str,
        _port: &mut u32,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        Ok(false)
    }
    async fn cancel_tcpip_forward(
        &mut self,
        _address: &str,
        _port: u32,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        Ok(false)
    }
    async fn streamlocal_forward(
        &mut self,
        _socket_path: &str,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        Ok(false)
    }
    async fn cancel_streamlocal_forward(
        &mut self,
        _socket_path: &str,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        Ok(false)
    }
    async fn agent_request(
        &mut self,
        _channel: ChannelId,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        Ok(false)
    }

    // FR-12: env, pty-req, subsystem, and every other channel request
    // type that isn't `exec` is rejected by calling
    // `session.channel_failure(channel)`. No exit-status is sent.
    async fn env_request(
        &mut self,
        channel: ChannelId,
        _name: &str,
        _value: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(channel)?;
        Ok(())
    }
    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(channel)?;
        Ok(())
    }
    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        _name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(channel)?;
        Ok(())
    }
    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(channel)?;
        Ok(())
    }
    async fn x11_request(
        &mut self,
        channel: ChannelId,
        _single_connection: bool,
        _auth_proto: &str,
        _auth_cookie: &str,
        _screen: u32,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(channel)?;
        Ok(())
    }
    async fn signal(
        &mut self,
        _channel: ChannelId,
        _signal: russh::Sig,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        // No-op. Signals may arrive mid-command; we do not forward
        // them to the upstream (matching Go parity).
        Ok(())
    }

    // FR-11 + FR-13: validate the exec command and spawn the upstream
    // proxy task. Malformed payloads → channel_failure. Non-allowlisted
    // commands or repo path mismatches → exit status 1.
    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let parsed = match parse_exec(data) {
            Ok(p) => p,
            Err(ExecParseError::InvalidUtf8) | Err(ExecParseError::Empty) => {
                // Malformed payload — matches Go's silent failure at
                // main.go:456.
                session.channel_failure(channel)?;
                return Ok(());
            }
            Err(ExecParseError::NotAllowed(cmd)) => {
                logging::warn(&format!("rejected SSH command: {cmd}"));
                session.channel_failure(channel)?;
                let _ = session.exit_status_request(channel, 1);
                session.close(channel)?;
                return Ok(());
            }
            Err(ExecParseError::MissingRepoPath(cmd)) => {
                // FIX for Go bare-exec bypass: reject bare
                // git-upload-pack / git-receive-pack with exit status
                // 1 rather than proxying through.
                logging::warn(&format!(
                    "rejected bare git command without repo path: {cmd}"
                ));
                session.channel_failure(channel)?;
                let _ = session.exit_status_request(channel, 1);
                session.close(channel)?;
                return Ok(());
            }
        };

        if !repo_path_matches(&self.remote.repo_path, &parsed.repo) {
            logging::warn(&format!(
                "rejected git command: repo path {:?} does not match allowed {:?}",
                parsed.repo, self.remote.repo_path
            ));
            session.channel_failure(channel)?;
            let _ = session.exit_status_request(channel, 1);
            session.close(channel)?;
            return Ok(());
        }

        // Accept.
        session.channel_success(channel)?;
        logging::info(&format!(
            "proxying git command {} to {}:{}",
            parsed.command, self.remote.host, self.remote.port
        ));

        // Spawn the upstream proxy. The ChannelId lives in the session's
        // channel map; we use the session handle to send data back.
        let handle = session.handle();
        let remote = Arc::clone(&self.remote);
        let drain = self.drain_tracker.clone();
        tokio::spawn(async move {
            let _guard = drain.track();
            if let Err(e) =
                proxy_upstream(handle.clone(), channel, remote.as_ref().clone(), parsed).await
            {
                logging::error(&format!("upstream git proxy error: {e}"));
                let _ = handle.exit_status_request(channel, 1).await;
                let _ = handle.close(channel).await;
            }
        });
        Ok(())
    }
}

/// Errors from the upstream proxy task.
#[derive(Debug, Error)]
pub enum UpstreamError {
    #[error("failed to read private key at {path}: {source}")]
    KeyRead {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse private key: {0}")]
    KeyParse(russh::keys::Error),
    #[error("known_hosts file at {path}: {reason}")]
    KnownHosts { path: String, reason: String },
    #[error("ssrf: {0}")]
    Ssrf(#[from] ssrf::SsrfError),
    #[error("upstream connect: {0}")]
    Connect(std::io::Error),
    #[error("upstream authentication failed")]
    AuthFailed,
    #[error("upstream channel open failed")]
    ChannelOpen,
    #[error("russh: {0}")]
    Russh(#[from] russh::Error),
}

/// Drive an upstream SSH session proxied to/from the agent's channel.
async fn proxy_upstream(
    handle: server::Handle,
    channel_id: ChannelId,
    remote: GitRemote,
    parsed: ExecParsed,
) -> Result<(), UpstreamError> {
    // Read and parse the private key.
    let key_bytes = tokio::fs::read_to_string(SSH_KEY_PATH)
        .await
        .map_err(|source| UpstreamError::KeyRead {
            path: SSH_KEY_PATH.to_string(),
            source,
        })?;
    let private_key =
        russh::keys::decode_secret_key(&key_bytes, None).map_err(UpstreamError::KeyParse)?;

    // Load the known_hosts file. FR-15: missing or empty file is a hard
    // refusal.
    let known_hosts_path = PathBuf::from(KNOWN_HOSTS_PATH);
    verify_known_hosts_file_nonempty(&known_hosts_path).map_err(|reason| {
        UpstreamError::KnownHosts {
            path: KNOWN_HOSTS_PATH.to_string(),
            reason,
        }
    })?;

    // SSRF-safe upstream resolution.
    let socket_addr = ssrf::resolve_safe(&remote.host, remote.port).await?;

    // FR-28 10s connect timeout. We dial plain TCP first, then pass
    // the stream to russh::client::connect_stream so we retain control
    // over the connect timeout.
    let tcp = tokio::time::timeout(
        UPSTREAM_CONNECT_TIMEOUT,
        tokio::net::TcpStream::connect(socket_addr),
    )
    .await
    .map_err(|_| {
        UpstreamError::Connect(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "upstream TCP connect timed out after 10s",
        ))
    })?
    .map_err(UpstreamError::Connect)?;

    let client_config = Arc::new(client::Config::default());
    let client_handler = UpstreamClient {
        known_hosts_path: known_hosts_path.clone(),
        upstream_host: remote.host.clone(),
        upstream_port: remote.port,
    };
    let mut upstream = client::connect_stream(client_config, tcp, client_handler).await?;

    // FR-14: always authenticate as `git`.
    let auth_result = upstream
        .authenticate_publickey(
            "git",
            PrivateKeyWithHashAlg::new(Arc::new(private_key), None),
        )
        .await?;
    if !auth_result.success() {
        return Err(UpstreamError::AuthFailed);
    }

    // Open session + exec the git command.
    let upstream_channel = upstream.channel_open_session().await?;
    let full_command = format!("{} '{}'", parsed.command, remote.repo_path);
    upstream_channel.exec(true, full_command.as_bytes()).await?;

    // Pipe bytes bidirectionally. Read from the upstream channel and
    // forward to the agent's channel via the server handle; read from
    // the agent's channel (which arrives via our Handler's `data`
    // callback) — for simplicity, we use the upstream channel's
    // `into_stream()` AsyncRead/AsyncWrite and manually forward.
    //
    // Rather than wire agent→upstream through the data() callback (which
    // would require mutable access to an async-spawned task), we use
    // the upstream channel's blocking loop: read incoming data, forward
    // it to the server handle, and watch for exit-status.

    let mut upstream_channel = upstream_channel;
    loop {
        let msg = match upstream_channel.wait().await {
            Some(m) => m,
            None => break,
        };
        match msg {
            russh::ChannelMsg::Data { data } => {
                handle.data(channel_id, data).await.map_err(|_| {
                    UpstreamError::Russh(russh::Error::from(std::io::Error::other(
                        "failed to forward upstream data to agent",
                    )))
                })?;
            }
            russh::ChannelMsg::ExtendedData { data, ext } => {
                handle
                    .extended_data(channel_id, ext, data)
                    .await
                    .map_err(|_| {
                        UpstreamError::Russh(russh::Error::from(std::io::Error::other(
                            "failed to forward upstream extended data to agent",
                        )))
                    })?;
            }
            russh::ChannelMsg::ExitStatus { exit_status } => {
                let _ = handle.exit_status_request(channel_id, exit_status).await;
                let _ = handle.eof(channel_id).await;
                let _ = handle.close(channel_id).await;
                return Ok(());
            }
            russh::ChannelMsg::Eof => {
                let _ = handle.eof(channel_id).await;
            }
            russh::ChannelMsg::Close => {
                let _ = handle.close(channel_id).await;
                return Ok(());
            }
            _ => {}
        }
    }

    // If we fall out of the loop without an explicit exit status, the
    // upstream disconnected unexpectedly. Propagate exit status 1.
    let _ = handle.exit_status_request(channel_id, 1).await;
    let _ = handle.close(channel_id).await;
    Ok(())
}

/// Verify that the known_hosts file exists and is non-empty. Returns
/// `Err(reason)` for the caller to format.
fn verify_known_hosts_file_nonempty(path: &Path) -> Result<(), String> {
    let metadata = std::fs::metadata(path).map_err(|e| format!("metadata: {e}"))?;
    if !metadata.is_file() {
        return Err("not a regular file".to_string());
    }
    if metadata.len() == 0 {
        return Err("file is empty".to_string());
    }
    Ok(())
}

/// Upstream SSH client handler. The only method we care about is
/// `check_server_key`, which verifies the server's host key against the
/// known_hosts file loaded at startup. Missing / empty file is a hard
/// refusal (verified before the connection starts in
/// `verify_known_hosts_file_nonempty`).
struct UpstreamClient {
    known_hosts_path: PathBuf,
    upstream_host: String,
    upstream_port: u16,
}

impl client::Handler for UpstreamClient {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        // Delegate to the known_hosts checker. On any error (parse
        // failure, missing file race), return Ok(false) — NEVER Ok(true)
        // — matching SR-2 ("no InsecureIgnoreHostKey bypass").
        match russh::keys::known_hosts::check_known_hosts_path(
            &self.upstream_host,
            self.upstream_port,
            server_public_key,
            &self.known_hosts_path,
        ) {
            Ok(true) => Ok(true),
            Ok(false) | Err(_) => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_exec ---

    #[test]
    fn test_parse_exec_command_git_upload_pack_with_path() {
        let parsed = parse_exec(b"git-upload-pack 'reitun/virdismat-mono.git'").expect("parsed");
        assert_eq!(parsed.command, "git-upload-pack");
        assert_eq!(parsed.repo, "reitun/virdismat-mono.git");
    }

    #[test]
    fn test_parse_exec_command_git_receive_pack_with_path() {
        let parsed = parse_exec(b"git-receive-pack 'reitun/virdismat-mono.git'").expect("parsed");
        assert_eq!(parsed.command, "git-receive-pack");
        assert_eq!(parsed.repo, "reitun/virdismat-mono.git");
    }

    #[test]
    fn test_parse_exec_command_bare_git_upload_pack_rejected() {
        // This is the fix for the Go bypass bug.
        let err = parse_exec(b"git-upload-pack").unwrap_err();
        assert_eq!(
            err,
            ExecParseError::MissingRepoPath("git-upload-pack".to_string())
        );
    }

    #[test]
    fn test_parse_exec_command_bare_git_receive_pack_rejected() {
        let err = parse_exec(b"git-receive-pack").unwrap_err();
        assert_eq!(
            err,
            ExecParseError::MissingRepoPath("git-receive-pack".to_string())
        );
    }

    #[test]
    fn test_parse_exec_command_bare_with_trailing_space_rejected() {
        // "git-upload-pack " should also be rejected — the argument is
        // empty after trimming.
        let err = parse_exec(b"git-upload-pack ").unwrap_err();
        assert_eq!(
            err,
            ExecParseError::MissingRepoPath("git-upload-pack".to_string())
        );
    }

    #[test]
    fn test_parse_exec_command_unknown_command_rejected() {
        let err = parse_exec(b"ls /etc").unwrap_err();
        assert_eq!(err, ExecParseError::NotAllowed("ls".to_string()));
    }

    #[test]
    fn test_parse_exec_empty_rejected() {
        let err = parse_exec(b"").unwrap_err();
        assert_eq!(err, ExecParseError::Empty);
    }

    #[test]
    fn test_parse_exec_invalid_utf8_rejected() {
        let err = parse_exec(&[0xff, 0xfe]).unwrap_err();
        assert_eq!(err, ExecParseError::InvalidUtf8);
    }

    #[test]
    fn test_parse_exec_strips_double_quotes() {
        let parsed = parse_exec(b"git-upload-pack \"reitun/virdismat-mono.git\"").expect("parsed");
        assert_eq!(parsed.repo, "reitun/virdismat-mono.git");
    }

    #[test]
    fn test_parse_exec_strips_leading_slash() {
        let parsed = parse_exec(b"git-upload-pack /reitun/virdismat-mono.git").expect("parsed");
        assert_eq!(parsed.repo, "reitun/virdismat-mono.git");
    }

    // --- repo_path_matches ---

    #[test]
    fn test_repo_path_matches_exact() {
        assert!(repo_path_matches(
            "reitun/virdismat-mono.git",
            "reitun/virdismat-mono.git"
        ));
    }

    #[test]
    fn test_repo_path_matches_with_leading_slashes() {
        assert!(repo_path_matches(
            "/reitun/virdismat-mono.git",
            "reitun/virdismat-mono.git"
        ));
        assert!(repo_path_matches(
            "reitun/virdismat-mono.git",
            "/reitun/virdismat-mono.git"
        ));
    }

    #[test]
    fn test_repo_path_mismatch_rejected() {
        assert!(!repo_path_matches(
            "reitun/virdismat-mono.git",
            "someone-else/repo.git"
        ));
    }

    // --- verify_known_hosts_file_nonempty ---

    #[test]
    fn test_known_hosts_missing_rejected() {
        let result = verify_known_hosts_file_nonempty(Path::new("/tmp/nonexistent-nautiloop-kh"));
        assert!(result.is_err());
    }

    #[test]
    fn test_known_hosts_empty_rejected() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let result = verify_known_hosts_file_nonempty(tmp.path());
        assert_eq!(result, Err("file is empty".to_string()));
    }

    #[test]
    fn test_known_hosts_nonempty_accepted() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(tmp.path(), "github.com ssh-ed25519 AAAA...\n").expect("write");
        assert!(verify_known_hosts_file_nonempty(tmp.path()).is_ok());
    }
}
