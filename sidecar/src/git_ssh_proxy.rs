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

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use russh::ChannelId;
use russh::client;
use russh::keys::PrivateKey;
use russh::keys::PrivateKeyWithHashAlg;
use russh::server::{self, Auth, Msg, Server as _, Session};
use russh::{Channel, MethodKind, MethodSet};
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::sync::watch;

use crate::git_url::GitRemote;
use crate::logging;
use crate::shutdown::ConnectionTracker;
use crate::ssrf;

/// Messages pushed from the russh server `Handler::data`,
/// `channel_eof`, and `channel_close` callbacks into the spawned
/// upstream proxy task. The proxy task pumps these messages to the
/// upstream SSH channel's writer. Using a bounded mpsc channel lets
/// the handler apply backpressure to the agent SSH connection via
/// russh's flow-control window instead of buffering unboundedly
/// inside the sidecar.
#[derive(Debug)]
enum AgentToUpstream {
    /// Raw bytes from the agent channel that must be written to the
    /// upstream channel's stdin.
    Data(Vec<u8>),
    /// The agent sent EOF. The upstream channel's stdin should be
    /// closed; any already-buffered data must flush first.
    Eof,
    /// The agent closed its channel. The upstream channel should be
    /// closed immediately and the pump task should exit so the
    /// upstream session does not linger when the agent disappears.
    Close,
}

/// Shared per-connection state that maps `ChannelId` to a sender the
/// russh `Handler::data` callback can use to pump bytes into the
/// upstream proxy task. The map is held behind a sync `Mutex` because
/// every access is a fast insert / lookup / remove — no `.await` is
/// held while the lock is taken.
///
/// The sender is bounded ([`AGENT_PUMP_CHANNEL_CAPACITY`]) so that a
/// slow upstream backpressures the agent SSH write path through
/// russh's flow-control window instead of silently queuing an
/// unbounded amount of bytes inside the sidecar.
type UpstreamPumpMap = Arc<Mutex<HashMap<ChannelId, mpsc::Sender<AgentToUpstream>>>>;

/// Bounded capacity for each per-channel agent→upstream pump queue.
/// Picked small on purpose: each slot holds one SSH packet-sized
/// `Vec<u8>` (≤ 32 KiB per russh defaults), so 64 slots cap the
/// worst-case per-channel buffering around 2 MiB. The bound exists
/// so `Handler::data` applies backpressure to the agent connection
/// via russh's flow-control window instead of the sidecar silently
/// accepting an unbounded push and OOMing.
const AGENT_PUMP_CHANNEL_CAPACITY: usize = 64;

/// Path to the agent-facing SSH private key used to authenticate to the
/// upstream git server. Fixed mount per spec.
pub const SSH_KEY_PATH: &str = "/secrets/ssh-key/id_ed25519";
/// Path to the known_hosts file. Mandatory, no bypass.
pub const KNOWN_HOSTS_PATH: &str = "/secrets/ssh-known-hosts/known_hosts";

/// Paths the upstream proxy reads for auth. Production uses the
/// [`SSH_KEY_PATH`] / [`KNOWN_HOSTS_PATH`] defaults; tests pass
/// temporary files so an end-to-end piping test can be wired up
/// without touching `/secrets`.
///
/// The test-only `test_override_addr` escape hatch is gated behind
/// the `test-utils` cargo feature. Release builds of
/// `nautiloop-sidecar` (and any downstream crate that depends on the
/// library without opting into `test-utils`) literally do not see
/// the field, cannot construct a value with it populated, and
/// therefore cannot bypass the FR-18 SSRF protection in
/// [`ssrf::resolve_safe`]. See `Cargo.toml`'s `[features]` section.
#[derive(Debug, Clone)]
pub struct SshAuthPaths {
    pub key_path: String,
    pub known_hosts_path: String,
    /// Test-only escape hatch: if set, the upstream proxy dials this
    /// `SocketAddr` directly instead of running
    /// [`ssrf::resolve_safe`]. Production builds MUST NOT enable the
    /// `test-utils` feature, so this field is absent and there is
    /// no way for a library consumer to set it.
    #[cfg(feature = "test-utils")]
    pub(crate) test_override_addr: Option<std::net::SocketAddr>,
}

impl SshAuthPaths {
    /// Build paths that point at the production `/secrets` mounts.
    pub fn new(key_path: impl Into<String>, known_hosts_path: impl Into<String>) -> Self {
        Self {
            key_path: key_path.into(),
            known_hosts_path: known_hosts_path.into(),
            #[cfg(feature = "test-utils")]
            test_override_addr: None,
        }
    }

    /// Test-only constructor that populates the SSRF override.
    /// Available only when the `test-utils` cargo feature is enabled,
    /// so release builds cannot call it at all.
    #[cfg(feature = "test-utils")]
    pub fn with_test_override_addr(
        key_path: impl Into<String>,
        known_hosts_path: impl Into<String>,
        override_addr: std::net::SocketAddr,
    ) -> Self {
        Self {
            key_path: key_path.into(),
            known_hosts_path: known_hosts_path.into(),
            test_override_addr: Some(override_addr),
        }
    }
}

impl Default for SshAuthPaths {
    fn default() -> Self {
        Self::new(SSH_KEY_PATH, KNOWN_HOSTS_PATH)
    }
}
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
    shutdown_rx: watch::Receiver<bool>,
    drain_tracker: ConnectionTracker,
    remote: GitRemote,
) -> Result<(), GitSshError> {
    serve_with_auth(
        listener,
        shutdown_rx,
        drain_tracker,
        remote,
        SshAuthPaths::default(),
    )
    .await
}

/// Variant of [`serve`] that accepts an [`SshAuthPaths`] override so
/// integration tests can point the upstream-proxy at a tempfile key
/// and known_hosts file.
pub async fn serve_with_auth(
    listener: TcpListener,
    mut shutdown_rx: watch::Receiver<bool>,
    drain_tracker: ConnectionTracker,
    remote: GitRemote,
    auth_paths: SshAuthPaths,
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
        auth_paths: Arc::new(auth_paths),
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
    auth_paths: Arc<SshAuthPaths>,
}

impl server::Server for GitSshServer {
    type Handler = GitSshHandler;

    fn new_client(&mut self, _peer: Option<std::net::SocketAddr>) -> Self::Handler {
        GitSshHandler {
            remote: Arc::clone(&self.remote),
            drain_tracker: self.drain_tracker.clone(),
            upstream_pumps: Arc::new(Mutex::new(HashMap::new())),
            auth_paths: Arc::clone(&self.auth_paths),
        }
    }
}

struct GitSshHandler {
    remote: Arc<GitRemote>,
    drain_tracker: ConnectionTracker,
    /// Per-channel senders that pump agent-side bytes into the upstream
    /// proxy task. Populated in `exec_request` and cleared in
    /// `channel_close` / on upstream completion.
    upstream_pumps: UpstreamPumpMap,
    auth_paths: Arc<SshAuthPaths>,
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

    // FR-13: forward agent-side channel data into the upstream SSH
    // session. Without this override russh's default `data` is a no-op
    // and the upstream never receives the pack-protocol bytes the git
    // client is streaming (git-receive-pack push phase, smart-HTTP
    // refs advertisement reply, etc.). Bugfix for Codex finding #2.
    //
    // Backpressure (Codex v2 finding #1): the pump is a bounded
    // channel, so `send().await` blocks while the pump queue is full.
    // Because russh awaits `Handler::data` before issuing the next
    // `WINDOW_ADJUST`, blocking here throttles the agent SSH write
    // path via the SSH flow-control window instead of letting the
    // sidecar buffer an unbounded `Vec<u8>` per slow upstream.
    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let sender = {
            let guard = match self.upstream_pumps.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.get(&channel).cloned()
        };
        if let Some(tx) = sender {
            // A send failure here means the upstream pump task has
            // exited and dropped the receiver. The channel is about
            // to be torn down; the git client will see the exit
            // status once `exec_request`'s spawned task runs its
            // cleanup branch. We intentionally do NOT propagate the
            // error as a russh error — doing so would tear down the
            // whole SSH connection instead of just this channel.
            if tx.send(AgentToUpstream::Data(data.to_vec())).await.is_err() {
                logging::warn("agent->upstream pump closed; dropping data frame");
            }
        }
        Ok(())
    }

    // Forward EOF. Git clients send EOF after the final pack delimiter
    // on push; without this the upstream blocks waiting for more data.
    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let sender = {
            let guard = match self.upstream_pumps.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.get(&channel).cloned()
        };
        if let Some(tx) = sender {
            let _ = tx.send(AgentToUpstream::Eof).await;
        }
        Ok(())
    }

    // Propagate an agent-side channel close to the upstream pump task
    // (Codex v2 finding #3). Previously this handler only removed the
    // map entry, which left the pump draining upstream→agent forever
    // if the agent closed abruptly while the remote still had data to
    // send. Now we explicitly enqueue `Close`, so the pump loop exits
    // and tears down the upstream session. Removing the map entry
    // after the send ensures the pump always sees the `Close`
    // message and we do not race with `Handler::data` enqueueing more
    // bytes onto a sender that is about to be dropped.
    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let sender = {
            let mut guard = match self.upstream_pumps.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.remove(&channel)
        };
        if let Some(tx) = sender {
            let _ = tx.send(AgentToUpstream::Close).await;
        }
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

        // Create the agent→upstream pump channel. Bounded capacity
        // (Codex v2 finding #1) so a slow upstream backpressures the
        // agent SSH write path via russh's flow-control window
        // instead of letting the sidecar silently buffer an
        // unbounded queue of `Vec<u8>`s.
        let (pump_tx, pump_rx) = mpsc::channel::<AgentToUpstream>(AGENT_PUMP_CHANNEL_CAPACITY);
        {
            let mut guard = match self.upstream_pumps.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.insert(channel, pump_tx);
        }

        // Spawn the upstream proxy. The ChannelId lives in the session's
        // channel map; we use the session handle to send data back.
        let handle = session.handle();
        let remote = Arc::clone(&self.remote);
        let drain = self.drain_tracker.clone();
        let pumps = Arc::clone(&self.upstream_pumps);
        let auth_paths = Arc::clone(&self.auth_paths);
        tokio::spawn(async move {
            let _guard = drain.track();
            let result = proxy_upstream(
                handle.clone(),
                channel,
                remote.as_ref().clone(),
                parsed,
                pump_rx,
                auth_paths.as_ref().clone(),
            )
            .await;
            // Remove the pump entry regardless of outcome so the
            // handler stops forwarding bytes into a dead channel.
            if let Ok(mut guard) = pumps.lock() {
                guard.remove(&channel);
            }
            if let Err(e) = result {
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
///
/// `pump_rx` carries the agent→upstream byte stream pushed by the
/// russh `Handler::data` / `channel_eof` / `channel_close` callbacks
/// (see Codex finding #2 and Codex v2 finding #3). We pump those
/// messages into the upstream channel writer on one concurrent branch
/// while the other branch drains upstream messages and forwards them
/// back to the agent via the server `Handle`.
async fn proxy_upstream(
    handle: server::Handle,
    channel_id: ChannelId,
    remote: GitRemote,
    parsed: ExecParsed,
    mut pump_rx: mpsc::Receiver<AgentToUpstream>,
    auth_paths: SshAuthPaths,
) -> Result<(), UpstreamError> {
    // Read and parse the private key.
    let key_bytes = tokio::fs::read_to_string(&auth_paths.key_path)
        .await
        .map_err(|source| UpstreamError::KeyRead {
            path: auth_paths.key_path.clone(),
            source,
        })?;
    let private_key =
        russh::keys::decode_secret_key(&key_bytes, None).map_err(UpstreamError::KeyParse)?;

    // Load the known_hosts file. FR-15: missing or empty file is a hard
    // refusal.
    let known_hosts_path = PathBuf::from(&auth_paths.known_hosts_path);
    verify_known_hosts_file_nonempty(&known_hosts_path).map_err(|reason| {
        UpstreamError::KnownHosts {
            path: auth_paths.known_hosts_path.clone(),
            reason,
        }
    })?;

    // SSRF-safe upstream resolution. When the `test-utils` feature
    // is enabled (integration tests only), a populated
    // `test_override_addr` bypasses `ssrf::resolve_safe` so tests
    // can point at a loopback mock upstream. Release builds do NOT
    // compile the field, so this always goes through the SSRF
    // resolver and the FR-18 loopback block fires as intended.
    #[cfg(feature = "test-utils")]
    let socket_addr = match auth_paths.test_override_addr {
        Some(addr) => addr,
        None => ssrf::resolve_safe(&remote.host, remote.port).await?,
    };
    #[cfg(not(feature = "test-utils"))]
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
    let mut upstream_channel = upstream.channel_open_session().await?;
    let full_command = format!("{} '{}'", parsed.command, remote.repo_path);
    upstream_channel.exec(true, full_command.as_bytes()).await?;

    // Pipe bytes bidirectionally. Two concurrent branches on each
    // `tokio::select!` iteration:
    //
    //   * upstream → agent: `upstream_channel.wait()` returns the
    //     next `ChannelMsg` from upstream. Data/ExtendedData frames
    //     are forwarded to the agent via the server `Handle`;
    //     ExitStatus/Close trigger a clean return.
    //
    //   * agent → upstream: `pump_rx.recv()` returns the next
    //     `AgentToUpstream` pushed by our `Handler::data` /
    //     `channel_eof` / `channel_close` callbacks. Data frames
    //     are written to the upstream channel's writer; Eof closes
    //     the write half; Close tears the upstream channel down
    //     and exits the pump loop immediately (Codex v2 finding
    //     #3). If the receiver is closed without an explicit
    //     control message — e.g. the handler panicked and dropped
    //     its sender — we treat that as an implicit `Close` too.
    //
    // The `tokio::select!` is deliberately NOT `biased` (Codex v2
    // finding #2): a biased select would always poll the upstream
    // branch first, and a continuously readable upstream would
    // then starve the agent→upstream direction indefinitely. The
    // default pseudo-random selection guarantees both directions
    // make progress under sustained load.
    //
    // This is the fix for Codex finding #2 — previously this loop
    // only polled upstream output and never forwarded agent bytes,
    // so git-receive-pack pushes hung indefinitely.
    let mut agent_eof_sent = false;
    let mut agent_closed = false;
    loop {
        tokio::select! {
            maybe_msg = upstream_channel.wait() => {
                let Some(msg) = maybe_msg else { break };
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
            pump = pump_rx.recv(), if !agent_closed => {
                match pump {
                    Some(AgentToUpstream::Data(bytes)) => {
                        // Write agent bytes into the upstream channel.
                        // Failure here generally means the upstream
                        // channel is closed; log once and stop
                        // accepting further agent bytes.
                        if let Err(e) = upstream_channel.data(bytes.as_slice()).await {
                            logging::warn(&format!(
                                "failed to forward agent data to upstream: {e}"
                            ));
                            agent_closed = true;
                        }
                    }
                    Some(AgentToUpstream::Eof) => {
                        if !agent_eof_sent {
                            agent_eof_sent = true;
                            if let Err(e) = upstream_channel.eof().await {
                                logging::warn(&format!(
                                    "failed to forward agent EOF to upstream: {e}"
                                ));
                            }
                        }
                    }
                    Some(AgentToUpstream::Close) => {
                        // Agent explicitly closed its channel. Tear
                        // the upstream channel down immediately and
                        // exit the pump loop so the upstream session
                        // does not linger (Codex v2 finding #3).
                        if let Err(e) = upstream_channel.close().await {
                            logging::warn(&format!(
                                "failed to forward agent close to upstream: {e}"
                            ));
                        }
                        let _ = handle.close(channel_id).await;
                        return Ok(());
                    }
                    None => {
                        // Receiver closed without an explicit control
                        // message — the handler dropped its sender
                        // (e.g. task panic). Treat this as a Close
                        // so we do not linger forever draining
                        // upstream for an agent that is no longer
                        // listening.
                        let _ = upstream_channel.close().await;
                        let _ = handle.close(channel_id).await;
                        return Ok(());
                    }
                }
            }
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
