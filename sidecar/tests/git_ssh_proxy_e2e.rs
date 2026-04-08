//! End-to-end integration test for the git SSH proxy.
//!
//! This test is the regression for Codex finding #2: the sidecar's
//! russh `Handler::data` callback was a no-op, so agent-to-upstream
//! pack-protocol bytes were silently dropped and `git push` hung
//! forever.
//!
//! Topology:
//!
//! ```text
//!  [agent client (russh::client)]
//!        |  TCP, port P1 on 127.0.0.1
//!        v
//!  [sidecar git_ssh_proxy::serve_with_auth]
//!        |  TCP, port P2 on 127.0.0.1 (via test_override_addr
//!        |  bypass so the SSRF loopback block does not fire)
//!        v
//!  [mock upstream russh server]
//! ```
//!
//! What we assert:
//!
//! 1. Agent -> upstream data flows: bytes we `channel.data(...)` from
//!    the agent reach the mock upstream's `Handler::data` callback.
//! 2. Upstream -> agent data flows: bytes the mock upstream writes
//!    via `session.data(...)` reach the agent's `ChannelMsg::Data`
//!    receiver.
//! 3. Upstream exit status propagates back to the agent end-to-end:
//!    the mock upstream explicitly emits exit status 0, and the
//!    agent must observe `ChannelMsg::ExitStatus { exit_status: 0 }`
//!    before the channel closes (Codex v2 finding #5 — previously
//!    this test only asserted that reply bytes arrived).
//!
//! The test uses ephemeral ports, tempfile-mounted key + known_hosts,
//! and the `SshAuthPaths::test_override_addr` escape hatch to bypass
//! FR-18's "loopback is private" SSRF rule for the agent→sidecar→mock
//! dial path. Production config must leave `test_override_addr` None.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use nautiloop_sidecar::git_ssh_proxy::{self, SshAuthPaths};
use nautiloop_sidecar::git_url::GitRemote;
use nautiloop_sidecar::shutdown::ConnectionTracker;
use russh::client;
use russh::keys::PrivateKey;
use russh::server::{self as rserver, Auth, Msg, Server as _, Session};
use russh::{Channel, ChannelId, MethodKind, MethodSet};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::sync::watch;

const ALLOWED_REPO: &str = "reitun/virdismat-mono.git";

// ---------------- mock upstream ----------------

#[derive(Clone)]
struct MockUpstream {
    // Channel for pushing bytes received on the mock's `Handler::data`
    // callback out to the test body. Cloned into each MockUpstreamHandler.
    received_tx: mpsc::UnboundedSender<Vec<u8>>,
    // Shared buffer: bytes the mock upstream writes back to the agent
    // after the exec lands. Set before the handler runs; read under a
    // sync mutex lock inside `exec_request`.
    reply_bytes: Arc<StdMutex<Vec<u8>>>,
}

#[derive(Clone)]
struct MockUpstreamHandler {
    received_tx: mpsc::UnboundedSender<Vec<u8>>,
    reply_bytes: Arc<StdMutex<Vec<u8>>>,
}

impl rserver::Server for MockUpstream {
    type Handler = MockUpstreamHandler;

    fn new_client(&mut self, _peer: Option<SocketAddr>) -> Self::Handler {
        MockUpstreamHandler {
            received_tx: self.received_tx.clone(),
            reply_bytes: Arc::clone(&self.reply_bytes),
        }
    }
}

impl rserver::Handler for MockUpstreamHandler {
    type Error = russh::Error;

    async fn auth_publickey(
        &mut self,
        _user: &str,
        _key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        _data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        // Accept the exec. Immediately push any queued reply bytes back
        // to the agent so the test has something to read on the
        // upstream -> agent direction.
        session.channel_success(channel)?;
        let reply = {
            let guard = match self.reply_bytes.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            guard.clone()
        };
        if !reply.is_empty() {
            session.data(channel, bytes::Bytes::from(reply))?;
        }
        Ok(())
    }

    async fn data(
        &mut self,
        _channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        // Forward to the test body.
        let _ = self.received_tx.send(data.to_vec());
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        // Agent closed its side: send exit status 0 and close so the
        // sidecar's `proxy_upstream` loop returns cleanly.
        let _ = session.exit_status_request(channel, 0);
        let _ = session.eof(channel);
        let _ = session.close(channel);
        Ok(())
    }
}

async fn spawn_mock_upstream(
    reply_bytes: Vec<u8>,
) -> (SocketAddr, PrivateKey, mpsc::UnboundedReceiver<Vec<u8>>) {
    let host_key =
        PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519).expect("host key");
    let host_key_public = host_key.clone();

    let methods_pk = {
        let mut m = MethodSet::empty();
        m.push(MethodKind::PublicKey);
        m
    };
    let config = Arc::new(rserver::Config {
        server_id: russh::SshId::Standard(std::borrow::Cow::Borrowed(
            "SSH-2.0-nautiloop-mock-upstream",
        )),
        methods: methods_pk,
        auth_rejection_time: Duration::from_secs(1),
        auth_rejection_time_initial: Some(Duration::from_millis(10)),
        keys: vec![host_key],
        inactivity_timeout: Some(Duration::from_secs(10)),
        nodelay: true,
        ..Default::default()
    });

    let (rx_tx, rx_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let mut server = MockUpstream {
        received_tx: rx_tx,
        reply_bytes: Arc::new(StdMutex::new(reply_bytes)),
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let addr = listener.local_addr().expect("mock local addr");
    tokio::spawn(async move {
        let _ = server.run_on_socket(config, &listener).await;
    });

    (addr, host_key_public, rx_rx)
}

// ---------------- agent client ----------------

struct AgentClient;

impl client::Handler for AgentClient {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        // The sidecar presents an ephemeral host key; the agent doesn't
        // verify it in this test (it matches the production agent
        // behaviour which also uses auth_none + no host-key check
        // because the sidecar is loopback).
        Ok(true)
    }
}

// ---------------- helpers ----------------

fn write_private_key(path: &std::path::Path, key: &PrivateKey) {
    use std::os::unix::fs::PermissionsExt;
    let pem = key
        .to_openssh(russh::keys::ssh_key::LineEnding::LF)
        .expect("openssh pem");
    std::fs::write(path, pem.as_bytes()).expect("write key");
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

fn write_known_hosts(path: &std::path::Path, host: &str, port: u16, host_key: &PrivateKey) {
    // Format: `[host]:port ssh-ed25519 BASE64`
    let pubkey = host_key.public_key();
    let openssh = pubkey.to_openssh().expect("pubkey to openssh");
    let line = if port == 22 {
        format!("{host} {openssh}\n")
    } else {
        format!("[{host}]:{port} {openssh}\n")
    };
    std::fs::write(path, line).expect("write known_hosts");
}

// ---------------- the test ----------------

/// End-to-end regression for Codex finding #2. This test deliberately
/// does NOT use any git tooling or pack-protocol bytes — it sends
/// plain ASCII payloads so a failure points straight at the byte pump
/// rather than some git-upload-pack quirk.
#[tokio::test]
async fn test_git_ssh_proxy_pipes_bidirectional_bytes() {
    // ----- 1. Mock upstream -----
    let reply_payload = b"MOCK_UPSTREAM_REPLY_PAYLOAD_XYZ".to_vec();
    let (mock_addr, mock_host_key, mut mock_rx) = spawn_mock_upstream(reply_payload.clone()).await;

    // ----- 2. Private key + known_hosts for the sidecar to auth to mock -----
    let tmp = tempfile::tempdir().expect("tempdir");
    let key_path = tmp.path().join("id_ed25519");
    let agent_key =
        PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519).expect("agent key");
    write_private_key(&key_path, &agent_key);

    let known_hosts_path = tmp.path().join("known_hosts");
    write_known_hosts(
        &known_hosts_path,
        &mock_addr.ip().to_string(),
        mock_addr.port(),
        &mock_host_key,
    );

    // ----- 3. Start the sidecar git SSH proxy -----
    let sidecar_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("sidecar bind");
    let sidecar_addr = sidecar_listener.local_addr().expect("sidecar addr");
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let drain_tracker = ConnectionTracker::new();
    let remote = GitRemote {
        host: mock_addr.ip().to_string(),
        port: mock_addr.port(),
        repo_path: ALLOWED_REPO.to_string(),
    };
    let auth_paths = SshAuthPaths::with_test_override_addr(
        key_path.to_string_lossy().to_string(),
        known_hosts_path.to_string_lossy().to_string(),
        mock_addr,
    );
    let sidecar_handle = tokio::spawn({
        let drain_tracker = drain_tracker.clone();
        async move {
            git_ssh_proxy::serve_with_auth(
                sidecar_listener,
                shutdown_rx,
                drain_tracker,
                remote,
                auth_paths,
            )
            .await
        }
    });

    // Give the server a moment to become ready.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // ----- 4. Connect as the agent -----
    let client_config = Arc::new(client::Config::default());
    let mut session = client::connect(client_config, sidecar_addr, AgentClient)
        .await
        .expect("agent connect");

    let auth = session
        .authenticate_none("git")
        .await
        .expect("agent auth none");
    assert!(
        auth.success(),
        "sidecar must accept auth_none from loopback agent"
    );

    let mut channel = session
        .channel_open_session()
        .await
        .expect("agent open session");

    // exec `git-upload-pack 'reitun/virdismat-mono.git'` — this is what
    // `git fetch` sends when cloning over SSH.
    let exec_bytes = format!("git-upload-pack '{ALLOWED_REPO}'");
    channel
        .exec(true, exec_bytes.as_bytes())
        .await
        .expect("agent exec");

    // ----- 5. Agent -> upstream: send payload bytes -----
    let upstream_payload = b"AGENT_TO_UPSTREAM_PAYLOAD_01234567";
    channel
        .data(&upstream_payload[..])
        .await
        .expect("agent send data");

    // Send EOF so the mock's `channel_eof` handler fires and the
    // upstream returns exit status 0, which causes the sidecar's proxy
    // loop to exit cleanly.
    channel.eof().await.expect("agent eof");

    // ----- 6. Assert the mock received the agent bytes -----
    let recv_timeout = Duration::from_secs(5);
    let mut received = Vec::new();
    loop {
        let frame = tokio::time::timeout(recv_timeout, mock_rx.recv())
            .await
            .expect("mock receive timed out — Handler::data forwarding is broken");
        match frame {
            Some(bytes) => received.extend_from_slice(&bytes),
            None => break,
        }
        if received.len() >= upstream_payload.len() {
            break;
        }
    }
    assert!(
        received
            .windows(upstream_payload.len())
            .any(|w| w == upstream_payload),
        "mock upstream did not receive the agent's payload (received {} bytes, expected {:?})",
        received.len(),
        std::str::from_utf8(upstream_payload).unwrap_or("<non-utf8>"),
    );

    // ----- 7. Assert the agent got the reply bytes and exit status -----
    //
    // Codex v2 finding #5: we must NOT stop reading the moment the
    // reply payload is observed — the mock upstream explicitly emits
    // exit status 0, and asserting end-to-end propagation of that
    // status is the whole point of the test. Drive the loop until
    // EITHER we see `ExitStatus` OR we see `Close`/`Eof`, so that a
    // regression that drops the exit status is caught instead of
    // silently ignored.
    let mut agent_received: Vec<u8> = Vec::new();
    let mut observed_exit_status: Option<u32> = None;
    let mut saw_reply_payload = false;
    let agent_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = agent_deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!(
                "timed out waiting for upstream reply + exit status on agent channel \
                 (got {} bytes, saw_reply_payload={}, exit_status={:?})",
                agent_received.len(),
                saw_reply_payload,
                observed_exit_status,
            );
        }
        let msg = tokio::time::timeout(remaining, channel.wait()).await;
        let msg = match msg {
            Ok(Some(m)) => m,
            Ok(None) => break,
            Err(_) => panic!(
                "timed out waiting on channel.wait() (got {} bytes, exit_status={:?})",
                agent_received.len(),
                observed_exit_status,
            ),
        };
        match msg {
            russh::ChannelMsg::Data { data } => {
                agent_received.extend_from_slice(&data);
                if agent_received
                    .windows(reply_payload.len())
                    .any(|w| w == reply_payload)
                {
                    saw_reply_payload = true;
                }
            }
            russh::ChannelMsg::ExitStatus { exit_status } => {
                observed_exit_status = Some(exit_status);
                // Keep reading briefly to collect any trailing close/eof
                // and any data that arrives ordered after the exit
                // status frame on the wire.
            }
            russh::ChannelMsg::Close | russh::ChannelMsg::Eof => {
                // Channel is tearing down — stop polling. We fall
                // through to the assertions below which will fail
                // loudly if the exit status never arrived.
                break;
            }
            _ => {}
        }
        // Exit the loop once BOTH requirements are satisfied so the
        // test doesn't hang if the sidecar closes cleanly.
        if saw_reply_payload && observed_exit_status.is_some() {
            break;
        }
    }
    assert!(
        saw_reply_payload,
        "agent did not receive the mock upstream's reply payload (got {} bytes)",
        agent_received.len()
    );
    assert_eq!(
        observed_exit_status,
        Some(0),
        "expected upstream exit status 0 to propagate end-to-end \
         through the sidecar; instead observed {observed_exit_status:?}"
    );

    // ----- 8. Clean up -----
    let _ = shutdown_tx.send(true);
    // The sidecar may take a moment to observe the shutdown.
    let _ = tokio::time::timeout(Duration::from_secs(5), sidecar_handle).await;
}

// ------------------------------------------------------------------
// Regression test for Codex v2 finding #2.
// ------------------------------------------------------------------
//
// A biased `tokio::select!` with the upstream→agent branch listed
// first will, in the presence of a continuously readable upstream,
// starve the agent→upstream direction indefinitely: the select
// keeps yielding the upstream branch because it has data available
// and never gets around to polling `pump_rx`.
//
// This test reproduces that pathology by running a mock upstream
// that emits a steady stream of small frames from a background
// task the moment `exec_request` lands. If the sidecar's proxy
// loop is `biased`, agent bytes never reach the mock and the
// assertion below times out. Without `biased`, both directions
// make progress and the assertion passes within the deadline.

#[derive(Clone)]
struct StarvationUpstream {
    received_tx: mpsc::UnboundedSender<Vec<u8>>,
}

#[derive(Clone)]
struct StarvationUpstreamHandler {
    received_tx: mpsc::UnboundedSender<Vec<u8>>,
}

impl rserver::Server for StarvationUpstream {
    type Handler = StarvationUpstreamHandler;

    fn new_client(&mut self, _peer: Option<SocketAddr>) -> Self::Handler {
        StarvationUpstreamHandler {
            received_tx: self.received_tx.clone(),
        }
    }
}

impl rserver::Handler for StarvationUpstreamHandler {
    type Error = russh::Error;

    async fn auth_publickey(
        &mut self,
        _user: &str,
        _key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        _data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        // Spawn a background task that keeps the upstream branch
        // of the sidecar's `tokio::select!` continuously ready. A
        // biased select would keep yielding this branch forever
        // and starve `pump_rx`, so the assertion below would
        // timeout. The mock pushes frames as fast as russh lets
        // us — the intent is "always something queued in the
        // upstream direction".
        let handle = session.handle();
        tokio::spawn(async move {
            for i in 0..2000u32 {
                let frame = format!("UP_FRAME_{i:04}\n");
                if handle
                    .data(channel, bytes::Bytes::from(frame.into_bytes()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        Ok(())
    }

    async fn data(
        &mut self,
        _channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let _ = self.received_tx.send(data.to_vec());
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let _ = session.exit_status_request(channel, 0);
        let _ = session.eof(channel);
        let _ = session.close(channel);
        Ok(())
    }
}

async fn spawn_starvation_upstream() -> (SocketAddr, PrivateKey, mpsc::UnboundedReceiver<Vec<u8>>) {
    let host_key =
        PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519).expect("host key");
    let host_key_public = host_key.clone();

    let methods_pk = {
        let mut m = MethodSet::empty();
        m.push(MethodKind::PublicKey);
        m
    };
    let config = Arc::new(rserver::Config {
        server_id: russh::SshId::Standard(std::borrow::Cow::Borrowed(
            "SSH-2.0-nautiloop-starvation-upstream",
        )),
        methods: methods_pk,
        auth_rejection_time: Duration::from_secs(1),
        auth_rejection_time_initial: Some(Duration::from_millis(10)),
        keys: vec![host_key],
        inactivity_timeout: Some(Duration::from_secs(10)),
        nodelay: true,
        ..Default::default()
    });

    let (rx_tx, rx_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let mut server = StarvationUpstream { received_tx: rx_tx };

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let addr = listener.local_addr().expect("mock local addr");
    tokio::spawn(async move {
        let _ = server.run_on_socket(config, &listener).await;
    });

    (addr, host_key_public, rx_rx)
}

/// Interleaved bidirectional flow with tight deadlines.
///
/// Codex v2 finding #2: the proxy_upstream loop used a biased
/// `tokio::select!` with the upstream→agent branch listed first,
/// which under load can starve the agent→upstream direction.
///
/// This test runs a mock upstream that emits a sustained stream of
/// small frames the moment the exec lands, while the agent also
/// writes a short sequence of small frames with an interleave
/// sleep. A healthy (unbiased) proxy delivers both directions
/// within a 2s deadline; a regression that (a) drops the pump
/// entirely or (b) wires agent→upstream behind an unfair scheduler
/// will fail here.
#[tokio::test]
async fn test_git_ssh_proxy_bidirectional_no_starvation() {
    // ----- 1. Mock upstream that emits a continuous stream -----
    let (mock_addr, mock_host_key, mut mock_rx) = spawn_starvation_upstream().await;

    // ----- 2. Key + known_hosts for sidecar -> mock auth -----
    let tmp = tempfile::tempdir().expect("tempdir");
    let key_path = tmp.path().join("id_ed25519");
    let agent_key =
        PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519).expect("agent key");
    write_private_key(&key_path, &agent_key);
    let known_hosts_path = tmp.path().join("known_hosts");
    write_known_hosts(
        &known_hosts_path,
        &mock_addr.ip().to_string(),
        mock_addr.port(),
        &mock_host_key,
    );

    // ----- 3. Sidecar git SSH proxy -----
    let sidecar_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("sidecar bind");
    let sidecar_addr = sidecar_listener.local_addr().expect("sidecar addr");
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let drain_tracker = ConnectionTracker::new();
    let remote = GitRemote {
        host: mock_addr.ip().to_string(),
        port: mock_addr.port(),
        repo_path: ALLOWED_REPO.to_string(),
    };
    let auth_paths = SshAuthPaths::with_test_override_addr(
        key_path.to_string_lossy().to_string(),
        known_hosts_path.to_string_lossy().to_string(),
        mock_addr,
    );
    let sidecar_handle = tokio::spawn({
        let drain_tracker = drain_tracker.clone();
        async move {
            git_ssh_proxy::serve_with_auth(
                sidecar_listener,
                shutdown_rx,
                drain_tracker,
                remote,
                auth_paths,
            )
            .await
        }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    // ----- 4. Agent connect + exec -----
    let client_config = Arc::new(client::Config::default());
    let mut session = client::connect(client_config, sidecar_addr, AgentClient)
        .await
        .expect("agent connect");
    let auth = session
        .authenticate_none("git")
        .await
        .expect("agent auth none");
    assert!(auth.success());
    let mut channel = session
        .channel_open_session()
        .await
        .expect("agent open session");
    let exec_bytes = format!("git-upload-pack '{ALLOWED_REPO}'");
    channel
        .exec(true, exec_bytes.as_bytes())
        .await
        .expect("agent exec");

    // ----- 5. Interleaved traffic -----
    //
    // Push a few small agent frames. Each `channel.data(...).await`
    // runs concurrently with the mock's background stream; on a
    // healthy proxy both directions make progress and the mock's
    // `Handler::data` observes the agent bytes within the deadline.
    // On a biased proxy the agent frames pile up in the pump
    // channel and never reach the mock — the assertion below times
    // out.
    let agent_frames: &[&[u8]] = &[
        b"DOWN_FRAME_0001_AAAAAAAAAAAAA",
        b"DOWN_FRAME_0002_BBBBBBBBBBBBB",
        b"DOWN_FRAME_0003_CCCCCCCCCCCCC",
        b"DOWN_FRAME_0004_DDDDDDDDDDDDD",
        b"DOWN_FRAME_0005_EEEEEEEEEEEEE",
    ];
    for frame in agent_frames {
        channel.data(*frame).await.expect("agent send data frame");
        // Small sleep so the upstream branch definitely has a bunch
        // of frames queued between each agent write. A biased select
        // would spin on upstream during this gap.
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // ----- 6. Assert all agent frames arrive at the mock -----
    //
    // Short deadline (2s) — a healthy proxy delivers them in tens of
    // milliseconds; a biased proxy never delivers them.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut mock_received: Vec<u8> = Vec::new();
    let mut matched_frames = 0usize;
    while matched_frames < agent_frames.len() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!(
                "agent->upstream starvation: only {}/{} agent frames reached mock upstream \
                 within 2s deadline (biased select regression? got {} bytes total)",
                matched_frames,
                agent_frames.len(),
                mock_received.len(),
            );
        }
        let frame = tokio::time::timeout(remaining, mock_rx.recv())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "agent->upstream starvation: mock upstream did not receive any agent frame \
                 within remaining deadline (matched {}/{}, got {} bytes)",
                    matched_frames,
                    agent_frames.len(),
                    mock_received.len(),
                )
            });
        let Some(bytes) = frame else { break };
        mock_received.extend_from_slice(&bytes);
        matched_frames = agent_frames
            .iter()
            .filter(|needle| mock_received.windows(needle.len()).any(|w| w == **needle))
            .count();
    }
    assert_eq!(
        matched_frames,
        agent_frames.len(),
        "mock upstream saw only {matched_frames}/{} agent frames; rest were starved by biased select",
        agent_frames.len(),
    );

    // Also verify the other direction landed something — the mock
    // emits 200 frames; we only need to see a couple so the check
    // is quick and non-flaky.
    let agent_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut agent_down_bytes: Vec<u8> = Vec::new();
    while tokio::time::Instant::now() < agent_deadline {
        let remaining = agent_deadline.saturating_duration_since(tokio::time::Instant::now());
        let msg = match tokio::time::timeout(remaining, channel.wait()).await {
            Ok(Some(m)) => m,
            Ok(None) => break,
            Err(_) => break,
        };
        if let russh::ChannelMsg::Data { data } = msg {
            agent_down_bytes.extend_from_slice(&data);
            if agent_down_bytes.windows(9).any(|w| w == b"UP_FRAME_") {
                break;
            }
        }
    }
    assert!(
        agent_down_bytes.windows(9).any(|w| w == b"UP_FRAME_"),
        "upstream->agent direction produced no frames in 2s; expected continuous stream"
    );

    // ----- 7. Clean up -----
    let _ = channel.eof().await;
    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), sidecar_handle).await;
}

// ==========================================================================
// Codex v3 finding #3: e2e reject-path coverage.
// ==========================================================================
//
// The reject paths (non-git exec, bare git-upload-pack, bare
// git-receive-pack, mismatched repo path) were previously only covered
// by pure parsing / path-matching unit tests. A regression that silently
// broke wire-level `exit_status_request(channel, 1)` propagation would
// not fail any existing test.
//
// The tests below spin the sidecar up against the same mock upstream
// used by the happy-path test, send a single exec request whose command
// triggers a specific reject branch, and assert that the agent observes
// `ChannelMsg::ExitStatus { exit_status: 1 }` on the wire.
//
// Per FR-17, these share a helper (`drive_ssh_proxy_with_command`) with
// the happy-path test. The starvation regression test above uses a
// different mock upstream (continuous-stream emitter) and is left alone.

/// Outcome of driving the SSH proxy with a single exec command.
struct ProxyResult {
    /// Exit status observed on the agent channel, if any.
    exit_status: Option<u32>,
    /// Bytes the agent received on `ChannelMsg::Data` before the
    /// channel closed. Used by the happy-path test; the reject tests
    /// ignore it.
    agent_received: Vec<u8>,
    /// Did we see `saw_reply_payload` during the drive loop? Only
    /// meaningful when a non-empty `reply_bytes` was supplied.
    saw_reply_payload: bool,
}

/// Drive the SSH proxy end-to-end with a single exec command.
///
/// Spins up the mock upstream used by the happy-path test, writes a
/// tempfile key + known_hosts, starts the sidecar against an ephemeral
/// loopback port, connects as the agent, opens a session channel, and
/// sends `exec(command)`. Drives `channel.wait()` until either the exit
/// status arrives or the channel closes, with a hard 5s deadline.
///
/// `reply_bytes` is what the mock upstream will push back to the agent
/// after the exec lands. For reject-path tests, pass `Vec::new()` — the
/// sidecar rejects the exec before dialing upstream, so the mock is
/// never reached.
async fn drive_ssh_proxy_with_command(command: &str, reply_bytes: Vec<u8>) -> ProxyResult {
    // ----- Mock upstream -----
    let (mock_addr, mock_host_key, _mock_rx) = spawn_mock_upstream(reply_bytes.clone()).await;

    // ----- Key + known_hosts -----
    let tmp = tempfile::tempdir().expect("tempdir");
    let key_path = tmp.path().join("id_ed25519");
    let agent_key =
        PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519).expect("agent key");
    write_private_key(&key_path, &agent_key);
    let known_hosts_path = tmp.path().join("known_hosts");
    write_known_hosts(
        &known_hosts_path,
        &mock_addr.ip().to_string(),
        mock_addr.port(),
        &mock_host_key,
    );

    // ----- Sidecar -----
    let sidecar_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("sidecar bind");
    let sidecar_addr = sidecar_listener.local_addr().expect("sidecar addr");
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let drain_tracker = ConnectionTracker::new();
    let remote = GitRemote {
        host: mock_addr.ip().to_string(),
        port: mock_addr.port(),
        repo_path: ALLOWED_REPO.to_string(),
    };
    let auth_paths = SshAuthPaths::with_test_override_addr(
        key_path.to_string_lossy().to_string(),
        known_hosts_path.to_string_lossy().to_string(),
        mock_addr,
    );
    let sidecar_handle = tokio::spawn({
        let drain_tracker = drain_tracker.clone();
        async move {
            git_ssh_proxy::serve_with_auth(
                sidecar_listener,
                shutdown_rx,
                drain_tracker,
                remote,
                auth_paths,
            )
            .await
        }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    // ----- Agent -----
    let client_config = Arc::new(client::Config::default());
    let mut session = client::connect(client_config, sidecar_addr, AgentClient)
        .await
        .expect("agent connect");
    let auth = session
        .authenticate_none("git")
        .await
        .expect("agent auth none");
    assert!(auth.success(), "sidecar must accept auth_none");
    let mut channel = session
        .channel_open_session()
        .await
        .expect("agent open session");
    // NOTE: exec returns Err when the server responds with
    // channel_failure — which is exactly what the sidecar does on a
    // reject path (before it also sends exit_status 1 and closes).
    // We deliberately ignore the `exec` result and rely on
    // `channel.wait()` below to pick up the exit status off the wire.
    let _ = channel.exec(true, command.as_bytes()).await;
    // For the happy-path test, send EOF so the mock's `channel_eof`
    // handler fires and emits exit_status 0. For reject paths this
    // has no effect (the sidecar already closed the channel).
    let _ = channel.eof().await;

    // ----- Drive channel.wait() until exit status or close -----
    let mut agent_received: Vec<u8> = Vec::new();
    let mut observed_exit_status: Option<u32> = None;
    let mut saw_reply_payload = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let msg = match tokio::time::timeout(remaining, channel.wait()).await {
            Ok(Some(m)) => m,
            Ok(None) => break,
            Err(_) => break,
        };
        match msg {
            russh::ChannelMsg::Data { data } => {
                agent_received.extend_from_slice(&data);
                if !reply_bytes.is_empty()
                    && agent_received
                        .windows(reply_bytes.len())
                        .any(|w| w == reply_bytes)
                {
                    saw_reply_payload = true;
                }
            }
            russh::ChannelMsg::ExitStatus { exit_status } => {
                observed_exit_status = Some(exit_status);
            }
            russh::ChannelMsg::Close | russh::ChannelMsg::Eof => {
                break;
            }
            _ => {}
        }
        if observed_exit_status.is_some() && (reply_bytes.is_empty() || saw_reply_payload) {
            break;
        }
    }

    // ----- Clean up -----
    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), sidecar_handle).await;

    ProxyResult {
        exit_status: observed_exit_status,
        agent_received,
        saw_reply_payload,
    }
}

/// FR-14: non-git exec command must be rejected with exit status 1
/// at the SSH wire level, not just at the `parse_exec` unit-test
/// level.
#[tokio::test]
async fn test_e2e_rejects_non_git_exec_with_exit_status_1() {
    let result = drive_ssh_proxy_with_command("ls /etc", Vec::new()).await;
    assert_eq!(
        result.exit_status,
        Some(1),
        "expected exit status 1 for non-git exec `ls /etc`, got {:?}",
        result.exit_status,
    );
}

/// FR-14: bare `git-upload-pack` (no repo path) must be rejected
/// with exit status 1. This is the wire-level regression for the
/// Go bare-exec bypass bug.
#[tokio::test]
async fn test_e2e_rejects_bare_git_upload_pack_with_exit_status_1() {
    let result = drive_ssh_proxy_with_command("git-upload-pack", Vec::new()).await;
    assert_eq!(
        result.exit_status,
        Some(1),
        "expected exit status 1 for bare `git-upload-pack` (Go bypass bug fix), got {:?}",
        result.exit_status,
    );
}

/// FR-14: bare `git-receive-pack` (no repo path) must also be
/// rejected with exit status 1.
#[tokio::test]
async fn test_e2e_rejects_bare_git_receive_pack_with_exit_status_1() {
    let result = drive_ssh_proxy_with_command("git-receive-pack", Vec::new()).await;
    assert_eq!(
        result.exit_status,
        Some(1),
        "expected exit status 1 for bare `git-receive-pack`, got {:?}",
        result.exit_status,
    );
}

/// FR-14: `git-upload-pack 'wrong/repo.git'` (mismatched repo path)
/// must be rejected with exit status 1.
#[tokio::test]
async fn test_e2e_rejects_mismatched_repo_path_with_exit_status_1() {
    let result =
        drive_ssh_proxy_with_command("git-upload-pack 'someone-else/repo.git'", Vec::new()).await;
    assert_eq!(
        result.exit_status,
        Some(1),
        "expected exit status 1 for mismatched repo path, got {:?}",
        result.exit_status,
    );
}

/// FR-17: happy-path regression reusing the shared helper. Equivalent
/// to `test_git_ssh_proxy_pipes_bidirectional_bytes` but drives the
/// proxy via `drive_ssh_proxy_with_command` so the helper keeps real
/// exercise of the success branch alongside the reject paths.
#[tokio::test]
async fn test_e2e_accepts_git_upload_pack_with_exit_status_0() {
    let reply = b"MOCK_UPSTREAM_REPLY_PAYLOAD_HELPER".to_vec();
    let result =
        drive_ssh_proxy_with_command(&format!("git-upload-pack '{ALLOWED_REPO}'"), reply.clone())
            .await;
    assert!(
        result.saw_reply_payload,
        "agent did not receive the mock upstream's reply payload via helper (got {} bytes)",
        result.agent_received.len(),
    );
    assert_eq!(
        result.exit_status,
        Some(0),
        "expected upstream exit status 0 to propagate end-to-end via helper, got {:?}",
        result.exit_status,
    );
}
