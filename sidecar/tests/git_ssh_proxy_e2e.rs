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
//! 3. Upstream exit status propagates back to the agent.
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
    let auth_paths = SshAuthPaths {
        key_path: key_path.to_string_lossy().to_string(),
        known_hosts_path: known_hosts_path.to_string_lossy().to_string(),
        test_override_addr: Some(mock_addr),
    };
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

    // ----- 7. Assert the agent got the reply bytes -----
    let mut agent_received: Vec<u8> = Vec::new();
    let agent_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = agent_deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!(
                "timed out waiting for upstream reply bytes on agent channel (got {} bytes)",
                agent_received.len()
            );
        }
        let msg = tokio::time::timeout(remaining, channel.wait()).await;
        let msg = match msg {
            Ok(Some(m)) => m,
            Ok(None) => break,
            Err(_) => panic!(
                "timed out waiting on channel.wait() (got {} bytes)",
                agent_received.len()
            ),
        };
        match msg {
            russh::ChannelMsg::Data { data } => {
                agent_received.extend_from_slice(&data);
                if agent_received
                    .windows(reply_payload.len())
                    .any(|w| w == reply_payload)
                {
                    break;
                }
            }
            russh::ChannelMsg::ExitStatus { exit_status: _ } => {
                // Exit status arrived before we saw the reply. That
                // would mean the payload was dropped; fall through and
                // let the outer assertion fail.
                break;
            }
            russh::ChannelMsg::Close | russh::ChannelMsg::Eof => break,
            _ => {}
        }
    }
    assert!(
        agent_received
            .windows(reply_payload.len())
            .any(|w| w == reply_payload),
        "agent did not receive the mock upstream's reply payload (got {} bytes)",
        agent_received.len()
    );

    // ----- 8. Clean up -----
    let _ = shutdown_tx.send(true);
    // The sidecar may take a moment to observe the shutdown.
    let _ = tokio::time::timeout(Duration::from_secs(5), sidecar_handle).await;
}
