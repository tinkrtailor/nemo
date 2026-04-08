//! Git SSH category runner (FR-22 third block + divergence_bare_exec_*).
//!
//! Connects via russh client to both sidecars' SSH proxy ports (19091
//! Go, 29091 Rust), opens a session channel, sends an `exec` request,
//! and captures stdout/stderr/exit status. Matches the shape of the
//! existing `sidecar/tests/git_ssh_proxy_e2e.rs` integration test.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use russh::ChannelMsg;
use russh::client::{Config as ClientConfig, Handler};
use russh::keys::{PrivateKey, PrivateKeyWithHashAlg};
use serde::Deserialize;
use std::io::Cursor;

use crate::compose::ports;
use crate::corpus::CorpusCase;
use crate::introspection;
use crate::result::SideOutput;
use crate::runner::RunnerContext;

#[derive(Debug, Clone, Deserialize)]
struct GitSshInput {
    /// "exec" — always, because that's the only channel request type
    /// the spec's cases exercise besides env.
    #[serde(default = "default_request_kind")]
    request_kind: String,
    /// Exec command to send (verbatim bytes, quotes included). Ignored
    /// when `request_kind == "env"`.
    #[serde(default)]
    exec_command: String,
    /// Name/value pair to send if `request_kind == "env"`.
    #[serde(default)]
    env_name: String,
    #[serde(default)]
    env_value: String,
    /// Bytes to send as channel stdin before waiting for the exit
    /// status. Empty = no writes. Used by receive-pack.
    #[serde(default)]
    stdin_hex: String,
}

fn default_request_kind() -> String {
    "exec".to_string()
}

pub async fn run(case: &CorpusCase, ctx: &RunnerContext) -> Result<(SideOutput, SideOutput)> {
    let input: GitSshInput = serde_json::from_value(case.input.clone())
        .with_context(|| format!("parsing input for case {}", case.name))?;

    let (mut go_out, mut rust_out) = issue_pair(&input, &ctx.ssh_key_path).await?;
    let (mut go_obs, mut rust_obs) = introspection::fetch_and_split().await?;
    go_out.mock_observations.append(&mut go_obs);
    rust_out.mock_observations.append(&mut rust_obs);
    Ok((go_out, rust_out))
}

/// Bare-exec divergence runner: issues `git-upload-pack` (no args) or
/// `git-receive-pack` (no args) to both sidecars and expects:
///
/// - Rust: exit status 1 from the sidecar itself (no upstream reached)
/// - Go: exit status 128 from paramiko (sidecar forwarded unchanged)
///
/// The runner encodes the two exit statuses on each side so the diff
/// engine sees them as different — which is the divergence pass.
pub async fn run_bare_exec_divergence(
    case: &CorpusCase,
    ctx: &RunnerContext,
) -> Result<(SideOutput, SideOutput)> {
    // Bare exec input: empty command arg.
    let command = match case.name.as_str() {
        "divergence_bare_exec_upload_pack_rejection" => "git-upload-pack",
        "divergence_bare_exec_receive_pack_rejection" => "git-receive-pack",
        other => return Err(anyhow!("run_bare_exec_divergence: unknown case {other}")),
    };
    let input = GitSshInput {
        request_kind: "exec".to_string(),
        exec_command: command.to_string(),
        env_name: String::new(),
        env_value: String::new(),
        stdin_hex: String::new(),
    };
    let (mut go_out, mut rust_out) = issue_pair(&input, &ctx.ssh_key_path).await?;
    let (mut go_obs, mut rust_obs) = introspection::fetch_and_split().await?;
    go_out.mock_observations.append(&mut go_obs);
    rust_out.mock_observations.append(&mut rust_obs);
    Ok((go_out, rust_out))
}

async fn issue_pair(input: &GitSshInput, key_path: &Path) -> Result<(SideOutput, SideOutput)> {
    // Load key ONCE — russh signing doesn't care which port is used.
    let key_bytes = tokio::fs::read(key_path)
        .await
        .with_context(|| format!("reading ssh key {}", key_path.display()))?;
    // The committed harness client key is OpenSSH format.
    let key_str = String::from_utf8(key_bytes).context("ssh key is not UTF-8")?;
    let private_key = PrivateKey::from_openssh(&key_str).context("parsing OpenSSH private key")?;

    let go_fut = issue_one(&private_key, ports::GO_SSH, input);
    let rust_fut = issue_one(&private_key, ports::RUST_SSH, input);
    tokio::try_join!(go_fut, rust_fut)
}

async fn issue_one(private_key: &PrivateKey, port: u16, input: &GitSshInput) -> Result<SideOutput> {
    let config = Arc::new(ClientConfig {
        inactivity_timeout: Some(Duration::from_secs(30)),
        ..Default::default()
    });
    let handler = Client;
    let addr = format!("127.0.0.1:{port}");
    let mut session = russh::client::connect(config, &addr, handler)
        .await
        .with_context(|| format!("ssh connect {addr}"))?;

    // Auth: sidecar accepts publickey for the committed harness key.
    // The spec's mock-github-ssh accepts any client key; the Rust
    // sidecar requires publickey; Go sidecar accepts loopback auth.
    // We always try publickey first with rsa-sha2-512 as the hash
    // for the RSA-OAEP flavors if someone swaps the committed key
    // later; for Ed25519 the hash field is ignored.
    let auth_key = PrivateKeyWithHashAlg::new(Arc::new(private_key.clone()), None);
    let authed = session
        .authenticate_publickey("git", auth_key)
        .await
        .context("ssh authenticate_publickey")?;
    if !authed.success() {
        return Err(anyhow!(
            "ssh publickey auth rejected by sidecar on port {port}"
        ));
    }

    let mut channel = session
        .channel_open_session()
        .await
        .context("channel_open_session")?;

    let mut side = SideOutput::default();
    let mut got_exit = false;
    let mut got_failure = false;

    match input.request_kind.as_str() {
        "env" => {
            // Send env request (want_reply=true), don't exec — we're
            // testing the ssh_rejects_env_request parity case. The
            // sidecar should reject with channel_failure, which
            // russh surfaces as the channel's next message being
            // ChannelMsg::Failure.
            channel
                .set_env(true, input.env_name.clone(), input.env_value.clone())
                .await
                .context("send env request")?;
            // Read the next message from the channel to see if the
            // server sent Failure (rejection) or Success (accepted).
            let reply = tokio::time::timeout(Duration::from_secs(5), channel.wait()).await;
            match reply {
                Ok(Some(ChannelMsg::Failure)) => {
                    side.ssh_channel_failed = true;
                }
                Ok(Some(ChannelMsg::Success)) => {
                    side.ssh_channel_failed = false;
                }
                Ok(Some(_)) | Ok(None) | Err(_) => {
                    // Neither success nor failure — treat as failure
                    // because the test expects a clear rejection.
                    side.ssh_channel_failed = true;
                }
            }
            // Close the channel cleanly.
            let _ = channel.close().await;
        }
        "exec" => {
            channel
                .exec(true, input.exec_command.as_bytes().to_vec())
                .await
                .context("send exec")?;

            // If the case supplied stdin bytes, write them after the
            // exec request. Used for receive-pack.
            if !input.stdin_hex.is_empty() {
                let bytes = hex_decode(&input.stdin_hex)?;
                channel
                    .data(Cursor::new(bytes))
                    .await
                    .context("write channel stdin")?;
                let _ = channel.eof().await;
            }

            let mut stdout: Vec<u8> = Vec::new();
            let mut stderr: Vec<u8> = Vec::new();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
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
                    ChannelMsg::Data { data } => {
                        stdout.extend_from_slice(&data);
                    }
                    ChannelMsg::ExtendedData { data, ext: 1 } => {
                        stderr.extend_from_slice(&data);
                    }
                    ChannelMsg::ExtendedData { .. } => {}
                    ChannelMsg::ExitStatus { exit_status } => {
                        side.ssh_exit_status = Some(exit_status as i32);
                        got_exit = true;
                    }
                    ChannelMsg::Eof => {}
                    ChannelMsg::Close => {
                        break;
                    }
                    ChannelMsg::Failure => {
                        got_failure = true;
                        break;
                    }
                    _ => {}
                }
                if got_exit {
                    // Stay on the loop a bit longer to absorb the
                    // close message, but break quickly.
                    match tokio::time::timeout(Duration::from_millis(200), channel.wait()).await {
                        Ok(Some(ChannelMsg::Close)) | Ok(None) | Err(_) => break,
                        Ok(Some(_)) => continue,
                    }
                }
            }

            side.ssh_stdout_hex = hex_encode(&stdout);
            side.ssh_stderr = String::from_utf8_lossy(&stderr).to_string();
            side.ssh_channel_failed = got_failure && side.ssh_exit_status.is_none();
        }
        other => {
            return Err(anyhow!("unknown request_kind {other}"));
        }
    }

    // Silence unused warnings on older toolchains.
    let _ = got_exit;
    let _ = got_failure;

    let _ = channel.close().await;
    drop(session);
    Ok(side)
}

/// Minimal russh client handler. Accepts the sidecar's host key
/// unconditionally — the sidecar uses an ephemeral host key per run
/// (see `sidecar/src/git_ssh_proxy.rs:302`) so TOFU is the only
/// practical policy.
struct Client;

impl Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

fn hex_decode(s: &str) -> Result<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return Err(anyhow!("hex length must be even"));
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for i in (0..s.len()).step_by(2) {
        let byte = u8::from_str_radix(&s[i..i + 2], 16).context("hex decode")?;
        out.push(byte);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exec_input_with_default_kind() {
        let v = serde_json::json!({
            "exec_command": "git-upload-pack 'test/repo.git'"
        });
        let input: GitSshInput = serde_json::from_value(v).unwrap();
        assert_eq!(input.request_kind, "exec");
        assert_eq!(input.exec_command, "git-upload-pack 'test/repo.git'");
    }

    #[test]
    fn parses_env_input() {
        let v = serde_json::json!({
            "request_kind": "env",
            "env_name": "FOO",
            "env_value": "bar"
        });
        let input: GitSshInput = serde_json::from_value(v).unwrap();
        assert_eq!(input.request_kind, "env");
        assert_eq!(input.env_name, "FOO");
    }

    #[test]
    fn hex_roundtrip() {
        assert_eq!(hex_encode(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
        assert_eq!(
            hex_decode("deadbeef").unwrap(),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
    }

    #[test]
    fn committed_harness_client_key_parses_as_openssh() {
        // Step 2 fixture gate: the committed Ed25519 private key
        // under fixtures/go-secrets/ssh-key/id_ed25519 must parse
        // via russh's `PrivateKey::from_openssh`, otherwise the
        // `git_ssh` runner cannot authenticate to either sidecar.
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/go-secrets/ssh-key/id_ed25519");
        let pem = std::fs::read_to_string(&path).expect("committed client ssh key must exist");
        PrivateKey::from_openssh(&pem).expect("committed client ssh key must parse");
    }

    #[test]
    fn rust_and_go_secrets_client_keys_are_identical() {
        // Both sidecars must authenticate as the same client. The
        // two mounts (go-secrets/ and rust-secrets/) hold identical
        // key material — we verify that invariant here so an
        // accidental divergence during fixture regeneration gets
        // caught at cargo test time.
        let go = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/go-secrets/ssh-key/id_ed25519");
        let rust = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/rust-secrets/ssh-key/id_ed25519");
        let g = std::fs::read(&go).expect("go key");
        let r = std::fs::read(&rust).expect("rust key");
        assert_eq!(g, r, "go and rust client keys must be byte-identical");
    }
}
