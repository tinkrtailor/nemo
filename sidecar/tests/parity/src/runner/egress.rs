//! Egress category runner (FR-22 block 2) plus the SSRF-error
//! `egress_dns_error_both_fail_502` case.
//!
//! Supported case shapes (declared via `input.kind`):
//!
//! - `connect`: raw TCP proxy `CONNECT <target> HTTP/1.1` + small
//!   echo exchange, asserts `200 Connection Established` is returned
//!   and a few bytes echo through.
//! - `http_get`: `reqwest` configured with `proxy(http://127.0.0.1:<port>)`
//!   sending an absolute-form `GET http://mock-example/foo`, capturing
//!   status + body.
//! - `http_origin_form_repair`: raw `tokio::net::TcpStream` sending
//!   `GET /foo HTTP/1.1\r\nHost: mock-example\r\n\r\n` — reqwest can't
//!   do this because it always fills in the scheme.
//! - `http_dns_error`: `reqwest` GET to a deliberately-unresolvable
//!   hostname, expects a 502 from the sidecar (parity).
//! - `http_no_redirect`: GET `/redirect`, expects 302, mock should
//!   observe exactly one request.
//! - `http_strip_proxy_connection`: GET with `Proxy-Connection`
//!   header, asserts mock observed no `Proxy-Connection` header.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::compose::ports;
use crate::corpus::CorpusCase;
use crate::introspection;
use crate::result::SideOutput;
use crate::runner::RunnerContext;

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum EgressInput {
    Connect {
        /// CONNECT target (e.g. "egress-target:443" or "egress-target").
        target: String,
        /// Bytes to send through the tunnel. Optional for smoke.
        #[serde(default)]
        payload_hex: String,
    },
    HttpGet {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
    HttpOriginFormRepair {
        /// Raw bytes to write onto the proxy socket. The harness
        /// inserts no headers on top; this is a verbatim-wire case.
        raw_request: String,
    },
    HttpDnsError {
        url: String,
    },
    HttpNoRedirect {
        url: String,
    },
    HttpStripProxyConnection {
        url: String,
    },
}

pub async fn run(case: &CorpusCase, _ctx: &RunnerContext) -> Result<(SideOutput, SideOutput)> {
    let input: EgressInput = serde_json::from_value(case.input.clone())
        .with_context(|| format!("parsing input for case {}", case.name))?;

    let (mut go_out, mut rust_out) = match input {
        EgressInput::Connect {
            target,
            payload_hex,
        } => {
            let go = run_connect(ports::GO_EGRESS, &target, &payload_hex).await?;
            let rust = run_connect(ports::RUST_EGRESS, &target, &payload_hex).await?;
            (go, rust)
        }
        EgressInput::HttpGet { url, headers } => {
            let go = run_http_get_via_proxy(ports::GO_EGRESS, &url, &headers).await?;
            let rust = run_http_get_via_proxy(ports::RUST_EGRESS, &url, &headers).await?;
            (go, rust)
        }
        EgressInput::HttpOriginFormRepair { raw_request } => {
            let go = run_raw_proxy_request(ports::GO_EGRESS, &raw_request).await?;
            let rust = run_raw_proxy_request(ports::RUST_EGRESS, &raw_request).await?;
            (go, rust)
        }
        EgressInput::HttpDnsError { url } => {
            let go = run_http_get_via_proxy(ports::GO_EGRESS, &url, &BTreeMap::new()).await?;
            let rust = run_http_get_via_proxy(ports::RUST_EGRESS, &url, &BTreeMap::new()).await?;
            (go, rust)
        }
        EgressInput::HttpNoRedirect { url } => {
            let go = run_http_get_via_proxy(ports::GO_EGRESS, &url, &BTreeMap::new()).await?;
            let rust = run_http_get_via_proxy(ports::RUST_EGRESS, &url, &BTreeMap::new()).await?;
            (go, rust)
        }
        EgressInput::HttpStripProxyConnection { url } => {
            let mut headers = BTreeMap::new();
            headers.insert("proxy-connection".to_string(), "keep-alive".to_string());
            let go = run_http_get_via_proxy(ports::GO_EGRESS, &url, &headers).await?;
            let rust = run_http_get_via_proxy(ports::RUST_EGRESS, &url, &headers).await?;
            (go, rust)
        }
    };

    let (mut go_obs, mut rust_obs) = introspection::fetch_and_split().await?;
    go_out.mock_observations.append(&mut go_obs);
    rust_out.mock_observations.append(&mut rust_obs);
    Ok((go_out, rust_out))
}

async fn run_connect(proxy_port: u16, target: &str, payload_hex: &str) -> Result<SideOutput> {
    let addr = format!("127.0.0.1:{proxy_port}");
    let mut stream = TcpStream::connect(&addr)
        .await
        .with_context(|| format!("connect {addr}"))?;
    let connect_line = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n");
    stream
        .write_all(connect_line.as_bytes())
        .await
        .context("write CONNECT request")?;
    // Read status line + headers until CRLFCRLF.
    let mut buf = Vec::with_capacity(512);
    let mut tmp = [0u8; 512];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut tmp))
            .await
            .context("CONNECT response timeout")?
            .context("read CONNECT response")?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if find_crlf_crlf(&buf).is_some() {
            break;
        }
        if buf.len() > 16 * 1024 {
            return Err(anyhow!("CONNECT response too long"));
        }
    }
    let response_head = String::from_utf8_lossy(&buf).to_string();
    let status = parse_status_code(&response_head).unwrap_or(0);

    // If status indicates tunnel open, echo a few bytes.
    let mut body = response_head.clone();
    if status == 200 && !payload_hex.is_empty() {
        let bytes = hex_decode(payload_hex)?;
        stream.write_all(&bytes).await.context("tunnel write")?;
        let mut echo = vec![0u8; bytes.len()];
        let n = tokio::time::timeout(Duration::from_secs(3), stream.read_exact(&mut echo))
            .await
            .context("tunnel echo timeout")?
            .context("tunnel echo read")?;
        body = format!(
            "{response_head}ECHO_BYTES={}",
            hex_encode(&echo[..bytes.len().min(n + bytes.len())])
        );
    }
    let _ = stream.shutdown().await;
    Ok(SideOutput::http(status, BTreeMap::new(), body))
}

async fn run_http_get_via_proxy(
    proxy_port: u16,
    url: &str,
    headers: &BTreeMap<String, String>,
) -> Result<SideOutput> {
    let proxy = reqwest::Proxy::http(format!("http://127.0.0.1:{proxy_port}"))
        .context("build http proxy")?;
    let client = reqwest::Client::builder()
        .proxy(proxy)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()
        .context("build egress reqwest client")?;
    let mut req = client.get(url);
    for (k, v) in headers {
        req = req.header(k, v);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            // Surface the error as a distinctive body so the diff
            // engine can still compare both sides — both sidecars
            // should fail identically on DNS errors (egress_dns_error).
            return Ok(SideOutput::http(
                0,
                BTreeMap::new(),
                format!("reqwest_error: {e}"),
            ));
        }
    };
    let status = resp.status().as_u16();
    let mut h = BTreeMap::new();
    for (k, v) in resp.headers() {
        h.insert(
            k.as_str().to_ascii_lowercase(),
            v.to_str().unwrap_or("").to_string(),
        );
    }
    let body = resp.text().await.unwrap_or_default();
    Ok(SideOutput::http(status, h, body))
}

/// Send raw bytes onto the proxy socket (origin-form repair case).
/// We read back whatever the sidecar writes until EOF or 5s timeout.
async fn run_raw_proxy_request(proxy_port: u16, raw_request: &str) -> Result<SideOutput> {
    let addr = format!("127.0.0.1:{proxy_port}");
    let mut stream = TcpStream::connect(&addr)
        .await
        .with_context(|| format!("connect {addr}"))?;
    stream
        .write_all(raw_request.as_bytes())
        .await
        .context("write raw request")?;
    stream.shutdown().await.ok(); // client is done writing
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, stream.read(&mut tmp)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => buf.extend_from_slice(&tmp[..n]),
            Ok(Err(_)) | Err(_) => break,
        }
    }
    let body = String::from_utf8_lossy(&buf).to_string();
    let status = parse_status_code(&body).unwrap_or(0);
    Ok(SideOutput::http(status, BTreeMap::new(), body))
}

fn find_crlf_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_status_code(response_head: &str) -> Option<u16> {
    // "HTTP/1.1 200 Connection Established\r\n..." → 200
    let first_line = response_head.lines().next()?;
    let mut parts = first_line.splitn(3, ' ');
    let _version = parts.next()?;
    let status = parts.next()?;
    status.parse().ok()
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
        return Err(anyhow!("hex string length must be even"));
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for i in (0..s.len()).step_by(2) {
        let byte = u8::from_str_radix(&s[i..i + 2], 16).context("decode hex")?;
        out.push(byte);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_connect_input() {
        let v = serde_json::json!({
            "kind": "connect",
            "target": "egress-target:443",
            "payload_hex": "deadbeef"
        });
        let input: EgressInput = serde_json::from_value(v).unwrap();
        match input {
            EgressInput::Connect {
                target,
                payload_hex,
            } => {
                assert_eq!(target, "egress-target:443");
                assert_eq!(payload_hex, "deadbeef");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parses_http_get_input() {
        let v = serde_json::json!({
            "kind": "http_get",
            "url": "http://mock-example/foo"
        });
        let input: EgressInput = serde_json::from_value(v).unwrap();
        matches!(input, EgressInput::HttpGet { .. });
    }

    #[test]
    fn parses_origin_form_repair() {
        let v = serde_json::json!({
            "kind": "http_origin_form_repair",
            "raw_request": "GET /foo HTTP/1.1\r\nHost: mock-example\r\n\r\n"
        });
        let input: EgressInput = serde_json::from_value(v).unwrap();
        match input {
            EgressInput::HttpOriginFormRepair { raw_request } => {
                assert!(raw_request.starts_with("GET /foo"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_status_code_handles_connect_line() {
        assert_eq!(
            parse_status_code("HTTP/1.1 200 Connection Established\r\n"),
            Some(200)
        );
        assert_eq!(
            parse_status_code("HTTP/1.1 502 Bad Gateway\r\n\r\n"),
            Some(502)
        );
    }

    #[test]
    fn parse_status_code_returns_none_on_garbage() {
        assert_eq!(parse_status_code(""), None);
        assert_eq!(parse_status_code("not-a-response"), None);
    }

    #[test]
    fn hex_roundtrip() {
        let bytes = [0x01u8, 0x23, 0xab, 0xcd];
        let s = hex_encode(&bytes);
        assert_eq!(s, "0123abcd");
        let back = hex_decode(&s).unwrap();
        assert_eq!(back, bytes);
    }

    #[test]
    fn hex_decode_rejects_odd_length() {
        assert!(hex_decode("abc").is_err());
    }
}
