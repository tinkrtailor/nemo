//! Egress logger and HTTP proxy (FR-17 through FR-19, FR-28 timeouts).
//!
//! Accepts HTTP/1.1 on `127.0.0.1:9092` and handles three request
//! shapes:
//!
//! 1. **`CONNECT`**: hijacks the TCP stream and tunnels bytes bidirectionally
//!    to the upstream host:port. 10s connect timeout (FR-28). Manual
//!    CloseWrite half-close sequence on client→upstream EOF to match Go.
//! 2. **Plain HTTP (origin-form or absolute `http://`)**: repairs
//!    missing scheme/host, strips `Proxy-Connection` and
//!    `Proxy-Authorization` headers, dials upstream as plain TCP, and
//!    forwards the request byte-for-byte.
//! 3. **Absolute `https://`**: same as plain HTTP except the upstream
//!    dial is wrapped in a TLS client using the shared rustls
//!    `ClientConfig` and SNI set to the upstream hostname. This
//!    matches Go's `http.Client` which silently upgrades `https://`
//!    absolute-form targets to TLS. **Does not follow redirects.**
//!
//! SSRF protection (FR-18) runs before any dial in all paths. Every
//! accepted connection is registered with the [`ConnectionTracker`] so
//! FR-27 step 3 drains them on shutdown.
//!
//! Every completed request emits one FR-19 egress log line to stdout.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::time::timeout as tokio_timeout;
use tokio_rustls::TlsConnector;

use crate::logging;
use crate::shutdown::ConnectionTracker;
use crate::ssrf;

/// FR-28: CONNECT dial timeout (10s).
const CONNECT_DIAL_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum size of an HTTP request head (method + URL + headers). Keeps
/// us bounded while we parse. Matches typical proxy limits.
const MAX_REQ_HEAD_BYTES: usize = 32 * 1024;

/// Errors produced by the server loop.
#[derive(Debug, Error)]
pub enum EgressError {
    #[error("accept error: {0}")]
    Accept(std::io::Error),
}

/// Parsed HTTP request head. We parse manually to keep full control over
/// header order and to avoid going through hyper's request types (which
/// don't expose the raw request line shape we need for non-proxy
/// forwarding).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestHead {
    pub method: String,
    pub target: String,
    pub version: String,
    /// Headers in order of arrival, with canonical lowercase names.
    pub headers: Vec<(String, String)>,
    /// Length of the parsed head in bytes (so the caller knows where the
    /// body starts in the original buffer).
    pub head_len: usize,
}

impl RequestHead {
    /// Lookup a header case-insensitively.
    pub fn get_header(&self, name: &str) -> Option<&str> {
        let lower = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| k == &lower)
            .map(|(_, v)| v.as_str())
    }

    /// Remove a header case-insensitively. Returns the number of removed
    /// entries.
    pub fn remove_header(&mut self, name: &str) -> usize {
        let lower = name.to_ascii_lowercase();
        let before = self.headers.len();
        self.headers.retain(|(k, _)| k != &lower);
        before - self.headers.len()
    }
}

/// Parse an HTTP/1.x request head. Returns `None` if the head is
/// incomplete (caller should read more bytes).
///
/// This is deliberately minimal: no support for HTTP/0.9, no chunked
/// decoding, no multi-line header continuation (which is deprecated and
/// was already absent from Go's stdlib net/http server).
pub fn parse_request_head(buf: &[u8]) -> Result<Option<RequestHead>, EgressParseError> {
    // Find the end-of-head marker `\r\n\r\n`.
    let Some(end) = find_crlf_crlf(buf) else {
        if buf.len() > MAX_REQ_HEAD_BYTES {
            return Err(EgressParseError::HeadTooLarge);
        }
        return Ok(None);
    };
    let head_bytes = &buf[..end];
    let head_str = std::str::from_utf8(head_bytes).map_err(|_| EgressParseError::InvalidUtf8)?;

    let mut lines = head_str.split("\r\n");
    let request_line = lines.next().ok_or(EgressParseError::MalformedRequestLine)?;
    let mut parts = request_line.splitn(3, ' ');
    let method = parts.next().ok_or(EgressParseError::MalformedRequestLine)?;
    let target = parts.next().ok_or(EgressParseError::MalformedRequestLine)?;
    let version = parts.next().ok_or(EgressParseError::MalformedRequestLine)?;

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or(EgressParseError::MalformedHeader)?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }

    Ok(Some(RequestHead {
        method: method.to_string(),
        target: target.to_string(),
        version: version.to_string(),
        headers,
        head_len: end + 4, // include the trailing CRLFCRLF
    }))
}

fn find_crlf_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Errors from [`parse_request_head`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum EgressParseError {
    #[error("request head exceeds {0} bytes", MAX_REQ_HEAD_BYTES)]
    HeadTooLarge,
    #[error("request head is not valid UTF-8")]
    InvalidUtf8,
    #[error("malformed request line")]
    MalformedRequestLine,
    #[error("malformed header")]
    MalformedHeader,
}

/// Compute the destination string logged for a CONNECT request per
/// FR-19. If the target has no port, synthesize `:443`.
pub fn destination_for_connect(target: &str) -> String {
    if target.contains(':') {
        target.to_string()
    } else {
        format!("{target}:443")
    }
}

/// Compute the destination string logged for a plain HTTP request per
/// FR-19. Repairs missing scheme/host from the `Host` header and returns
/// the raw `URL.Host` string WITHOUT synthesizing a port.
pub fn destination_for_http(target: &str, host_header: Option<&str>) -> String {
    // If the target is an absolute-form URL, extract the authority.
    if let Some(authority) = absolute_form_authority(target) {
        return authority.to_string();
    }
    // Otherwise origin form `/path` — use the Host header verbatim.
    host_header.unwrap_or("").to_string()
}

fn absolute_form_authority(target: &str) -> Option<&str> {
    // http://host[:port]/path
    for scheme in ["http://", "https://"] {
        if let Some(rest) = target.strip_prefix(scheme) {
            let end = rest.find('/').unwrap_or(rest.len());
            return Some(&rest[..end]);
        }
    }
    None
}

/// Extract `host[:port]` from a CONNECT target, then split into `(host,
/// port)` with default port 443 if absent.
pub fn split_host_port_with_default(target: &str, default_port: u16) -> (String, u16) {
    if let Some((host, port)) = target.rsplit_once(':')
        && let Ok(p) = port.parse::<u16>()
    {
        return (host.to_string(), p);
    }
    (target.to_string(), default_port)
}

/// Request headers stripped from plain-HTTP forwarding per FR-17.
pub const STRIPPED_REQUEST_HEADERS: &[&str] = &["proxy-connection", "proxy-authorization"];

/// Serialize a `RequestHead` + body to wire format for forwarding.
/// Exposed for unit testing — the wire format is tiny and easy to
/// assert.
pub fn serialize_request(head: &RequestHead, target_override: Option<&str>) -> Vec<u8> {
    let mut out = Vec::with_capacity(head.head_len + 32);
    let target = target_override.unwrap_or(&head.target);
    out.extend_from_slice(head.method.as_bytes());
    out.push(b' ');
    out.extend_from_slice(target.as_bytes());
    out.push(b' ');
    out.extend_from_slice(head.version.as_bytes());
    out.extend_from_slice(b"\r\n");
    for (k, v) in &head.headers {
        out.extend_from_slice(k.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(v.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out
}

/// Serve the egress logger until `shutdown_rx` receives `true`.
///
/// `tls_config` is the shared rustls client config used to wrap
/// upstream connections for absolute-form `https://` forwards (Fix #5).
pub async fn serve(
    listener: TcpListener,
    mut shutdown_rx: watch::Receiver<bool>,
    drain_tracker: ConnectionTracker,
    tls_config: Arc<ClientConfig>,
) -> Result<(), EgressError> {
    loop {
        tokio::select! {
            biased;
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            accept = listener.accept() => {
                let (stream, _) = accept.map_err(EgressError::Accept)?;
                let drain = drain_tracker.clone();
                let tls_config = Arc::clone(&tls_config);
                tokio::spawn(async move {
                    // FR-27: register every egress connection with the
                    // drain tracker so shutdown waits for in-flight
                    // HTTP forwards as well as CONNECT tunnels.
                    let _guard = drain.track();
                    if let Err(e) = handle_connection(stream, drain.clone(), tls_config).await {
                        logging::warn(&format!("egress connection error: {e}"));
                    }
                });
            }
        }
    }
    Ok(())
}

async fn handle_connection(
    mut client: TcpStream,
    drain_tracker: ConnectionTracker,
    tls_config: Arc<ClientConfig>,
) -> io::Result<()> {
    // Buffer incoming bytes until we have a full request head.
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];
    let head = loop {
        match parse_request_head(&buf) {
            Ok(Some(head)) => break head,
            Ok(None) => {
                let n = client.read(&mut tmp).await?;
                if n == 0 {
                    return Ok(()); // client disconnected
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            Err(e) => {
                let body = format!("{{\"error\":\"{e}\"}}");
                write_simple_response(&mut client, 400, "Bad Request", &body).await?;
                return Ok(());
            }
        }
    };

    if head.method.eq_ignore_ascii_case("CONNECT") {
        handle_connect(client, head, drain_tracker).await
    } else {
        handle_plain_http(client, head, buf, tls_config).await
    }
}

async fn handle_connect(
    mut client: TcpStream,
    head: RequestHead,
    _drain_tracker: ConnectionTracker,
) -> io::Result<()> {
    let dest = destination_for_connect(&head.target);
    let (host, port) = split_host_port_with_default(&head.target, 443);

    // SSRF check — fail closed.
    let socket_addr = match ssrf::resolve_safe(&host, port).await {
        Ok(a) => a,
        Err(e) => {
            let body = format!(r#"{{"error":"ssrf: {e}"}}"#);
            let status = match e {
                crate::ssrf::SsrfError::PrivateIp(_) => (403, "Forbidden"),
                _ => (502, "Bad Gateway"),
            };
            logging::warn(&format!("blocked CONNECT to {dest}: {e}"));
            write_simple_response(&mut client, status.0, status.1, &body).await?;
            return Ok(());
        }
    };

    // FR-28: 10s connect timeout.
    let upstream_res = tokio_timeout(CONNECT_DIAL_TIMEOUT, TcpStream::connect(socket_addr)).await;
    let mut upstream = match upstream_res {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            let body = format!(r#"{{"error":"connect failed: {e}"}}"#);
            write_simple_response(&mut client, 502, "Bad Gateway", &body).await?;
            return Ok(());
        }
        Err(_) => {
            let body = r#"{"error":"connect timeout"}"#;
            write_simple_response(&mut client, 504, "Gateway Timeout", body).await?;
            return Ok(());
        }
    };

    // Drain tracking is done at the connection-accept level in
    // `serve`, so this tunnel is already counted toward the
    // FR-27 drain budget.

    // Send the tunnel-established line to the client.
    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;
    client.flush().await?;

    // Half-close-aware bidirectional copy. FR-28 requires that on
    // client→upstream EOF we call `shutdown()` on the upstream write
    // half, then wait for the upstream→client copy to complete, then
    // close the upstream. `tokio::io::copy_bidirectional` already does
    // this half-close sequence by default.
    let (mut bytes_sent, mut bytes_recv) = (0i64, 0i64);
    match tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
        Ok((c2u, u2c)) => {
            bytes_sent = c2u as i64;
            bytes_recv = u2c as i64;
        }
        Err(e) => {
            // Normal during abrupt drops; log at warn level.
            logging::warn(&format!("CONNECT tunnel copy error: {e}"));
        }
    }

    // Ensure both halves are fully closed.
    let _ = upstream.shutdown().await;
    let _ = client.shutdown().await;

    logging::egress(dest, "CONNECT", bytes_sent, bytes_recv);
    Ok(())
}

/// Upstream scheme of a plain-HTTP forward, chosen from the parsed
/// request target. `Http` is the default (origin-form `/path` or
/// `http://` absolute-form). `Https` is set for absolute-form
/// `https://` targets and triggers a TLS-wrapped upstream dial per
/// Fix #5. Exposed for unit testing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamScheme {
    Http,
    Https,
}

/// Pure helper that decides the upstream scheme for a given request
/// target. Exposed so tests can pin down the absolute-form `https://`
/// parity bug regression.
pub fn upstream_scheme_for_target(target: &str) -> UpstreamScheme {
    if target.starts_with("https://") {
        UpstreamScheme::Https
    } else {
        UpstreamScheme::Http
    }
}

async fn handle_plain_http(
    mut client: TcpStream,
    mut head: RequestHead,
    buf: Vec<u8>,
    tls_config: Arc<ClientConfig>,
) -> io::Result<()> {
    // Repair the request target: if it's origin-form `/path`, combine
    // with the Host header. If it's absolute-form `http[s]://host/path`,
    // extract the authority as the upstream host and convert to
    // origin-form for the forwarded request.
    let host_header = head.get_header("host").map(str::to_string);
    let dest = destination_for_http(&head.target, host_header.as_deref());
    let scheme = upstream_scheme_for_target(&head.target);
    let default_port = match scheme {
        UpstreamScheme::Https => 443,
        UpstreamScheme::Http => 80,
    };

    // Determine upstream host/port + rewritten target (origin-form).
    let (upstream_host, upstream_port, forward_target) =
        if let Some(authority) = absolute_form_authority(&head.target) {
            // absolute form — strip scheme+host to origin-form.
            let origin = head
                .target
                .strip_prefix("http://")
                .or_else(|| head.target.strip_prefix("https://"))
                .and_then(|rest| rest.find('/').map(|i| &rest[i..]))
                .unwrap_or("/")
                .to_string();
            let (h, p) = split_host_port_with_default(authority, default_port);
            (h, p, origin)
        } else if let Some(host) = host_header.clone() {
            // Origin-form: scheme is implicitly http per Go parity.
            let (h, p) = split_host_port_with_default(&host, default_port);
            (h, p, head.target.clone())
        } else {
            write_simple_response(
                &mut client,
                400,
                "Bad Request",
                r#"{"error":"missing Host header"}"#,
            )
            .await?;
            return Ok(());
        };

    // Strip proxy-only headers per FR-17.
    for name in STRIPPED_REQUEST_HEADERS {
        head.remove_header(name);
    }

    // SSRF check.
    let socket_addr = match ssrf::resolve_safe(&upstream_host, upstream_port).await {
        Ok(a) => a,
        Err(e) => {
            let body = format!(r#"{{"error":"ssrf: {e}"}}"#);
            let status = match e {
                crate::ssrf::SsrfError::PrivateIp(_) => (403, "Forbidden"),
                _ => (502, "Bad Gateway"),
            };
            logging::warn(&format!("blocked HTTP to {dest}: {e}"));
            write_simple_response(&mut client, status.0, status.1, &body).await?;
            return Ok(());
        }
    };

    // Dial upstream. No explicit timeout (parity with Go's http.Client
    // default). The underlying TCP connect will still fail eventually
    // on a bad host.
    let tcp = match TcpStream::connect(socket_addr).await {
        Ok(s) => s,
        Err(e) => {
            let body = format!(r#"{{"error":"connect failed: {e}"}}"#);
            write_simple_response(&mut client, 502, "Bad Gateway", &body).await?;
            return Ok(());
        }
    };

    // Serialize the rewritten head now — same bytes for both plain and
    // TLS paths.
    let head_bytes = serialize_request(&head, Some(&forward_target));
    let buffered_body = if buf.len() > head.head_len {
        buf[head.head_len..].to_vec()
    } else {
        Vec::new()
    };

    let (bytes_sent, bytes_recv) = match scheme {
        UpstreamScheme::Http => {
            forward_plain(client, tcp, &head_bytes, &buffered_body, &dest).await
        }
        UpstreamScheme::Https => {
            forward_tls(
                client,
                tcp,
                &upstream_host,
                &head_bytes,
                &buffered_body,
                &dest,
                tls_config,
            )
            .await
        }
    };

    logging::egress(dest, head.method, bytes_sent, bytes_recv);
    Ok(())
}

/// Forward a plain-HTTP request/response over an un-TLS-wrapped TCP
/// stream. Returns `(bytes_sent, bytes_recv)`. Errors are logged and
/// turned into `(0, 0)` so the caller can still emit an egress log
/// line.
async fn forward_plain(
    mut client: TcpStream,
    mut upstream: TcpStream,
    head_bytes: &[u8],
    buffered_body: &[u8],
    dest: &str,
) -> (i64, i64) {
    if let Err(e) = upstream.write_all(head_bytes).await {
        logging::warn(&format!("HTTP forward write error to {dest}: {e}"));
        return (0, 0);
    }
    if !buffered_body.is_empty()
        && let Err(e) = upstream.write_all(buffered_body).await
    {
        logging::warn(&format!("HTTP forward body write error to {dest}: {e}"));
        return (0, 0);
    }

    let (mut c2u, mut u2c) = (0i64, 0i64);
    match tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
        Ok((s, r)) => {
            c2u = s as i64;
            u2c = r as i64;
        }
        Err(e) => logging::warn(&format!("HTTP forward copy error to {dest}: {e}")),
    }
    let _ = upstream.shutdown().await;
    let _ = client.shutdown().await;
    (c2u, u2c)
}

/// Forward an absolute-form `https://` request: wrap the upstream TCP
/// stream in TLS using the shared rustls `ClientConfig` and SNI set to
/// the upstream hostname, then pipe bytes bidirectionally. Go's
/// `http.Client` performs this automatically for `https://` URLs, so
/// without this path a previously-working Go-sidecar client that sent
/// an absolute-form `https://` target would get a silent HTTP-on-TLS
/// downgrade — see Fix #5 in the codex review.
async fn forward_tls(
    mut client: TcpStream,
    tcp: TcpStream,
    upstream_host: &str,
    head_bytes: &[u8],
    buffered_body: &[u8],
    dest: &str,
    tls_config: Arc<ClientConfig>,
) -> (i64, i64) {
    let server_name = match ServerName::try_from(upstream_host.to_string()) {
        Ok(n) => n,
        Err(e) => {
            logging::warn(&format!("invalid DNS name for TLS upstream {dest}: {e}"));
            let _ = write_simple_response(
                &mut client,
                502,
                "Bad Gateway",
                r#"{"error":"invalid upstream DNS name"}"#,
            )
            .await;
            return (0, 0);
        }
    };
    let connector = TlsConnector::from(tls_config);
    let mut upstream = match connector.connect(server_name, tcp).await {
        Ok(s) => s,
        Err(e) => {
            logging::warn(&format!("TLS handshake to {dest} failed: {e}"));
            let _ = write_simple_response(
                &mut client,
                502,
                "Bad Gateway",
                r#"{"error":"upstream TLS handshake failed"}"#,
            )
            .await;
            return (0, 0);
        }
    };

    if let Err(e) = upstream.write_all(head_bytes).await {
        logging::warn(&format!("HTTPS forward write error to {dest}: {e}"));
        return (0, 0);
    }
    if !buffered_body.is_empty()
        && let Err(e) = upstream.write_all(buffered_body).await
    {
        logging::warn(&format!("HTTPS forward body write error to {dest}: {e}"));
        return (0, 0);
    }

    let (mut c2u, mut u2c) = (0i64, 0i64);
    match tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
        Ok((s, r)) => {
            c2u = s as i64;
            u2c = r as i64;
        }
        Err(e) => logging::warn(&format!("HTTPS forward copy error to {dest}: {e}")),
    }
    let _ = upstream.shutdown().await;
    let _ = client.shutdown().await;
    (c2u, u2c)
}

/// Write a minimal HTTP/1.1 response with a JSON body. Used for error
/// paths only.
async fn write_simple_response(
    client: &mut TcpStream,
    status: u16,
    reason: &str,
    body: &str,
) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    client.write_all(head.as_bytes()).await?;
    client.write_all(body.as_bytes()).await?;
    client.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- destination normalization ---

    #[test]
    fn test_destination_for_connect_with_port() {
        assert_eq!(destination_for_connect("github.com:443"), "github.com:443");
        assert_eq!(
            destination_for_connect("api.openai.com:443"),
            "api.openai.com:443"
        );
    }

    #[test]
    fn test_destination_for_connect_without_port_synthesizes_443() {
        assert_eq!(destination_for_connect("github.com"), "github.com:443");
    }

    #[test]
    fn test_destination_for_http_origin_form_uses_host_header() {
        assert_eq!(
            destination_for_http("/foo", Some("mock-example.docker")),
            "mock-example.docker"
        );
    }

    #[test]
    fn test_destination_for_http_origin_form_with_port_in_host_header() {
        assert_eq!(
            destination_for_http("/foo", Some("mock-example.docker:8080")),
            "mock-example.docker:8080"
        );
    }

    #[test]
    fn test_destination_for_http_absolute_form_extracts_authority() {
        assert_eq!(
            destination_for_http("http://mock-example.docker/foo", None),
            "mock-example.docker"
        );
        assert_eq!(
            destination_for_http("http://mock-example.docker:8080/foo", None),
            "mock-example.docker:8080"
        );
    }

    #[test]
    fn test_destination_for_http_http_does_not_synthesize_port() {
        // FR-19 explicitly forbids synthesizing a port for plain HTTP.
        assert_eq!(
            destination_for_http("/foo", Some("mock-example.docker")),
            "mock-example.docker"
        );
    }

    // --- parse_request_head ---

    #[test]
    fn test_parse_request_head_basic_get() {
        let req = b"GET /foo HTTP/1.1\r\nHost: example.com\r\nUser-Agent: test\r\n\r\n";
        let head = parse_request_head(req).expect("ok").expect("complete");
        assert_eq!(head.method, "GET");
        assert_eq!(head.target, "/foo");
        assert_eq!(head.version, "HTTP/1.1");
        assert_eq!(head.get_header("host"), Some("example.com"));
        assert_eq!(head.get_header("Host"), Some("example.com"));
        assert_eq!(head.get_header("user-agent"), Some("test"));
    }

    #[test]
    fn test_parse_request_head_connect() {
        let req = b"CONNECT github.com:443 HTTP/1.1\r\nHost: github.com:443\r\n\r\n";
        let head = parse_request_head(req).expect("ok").expect("complete");
        assert_eq!(head.method, "CONNECT");
        assert_eq!(head.target, "github.com:443");
    }

    #[test]
    fn test_parse_request_head_incomplete_returns_none() {
        let req = b"GET /foo HTTP/1.1\r\nHost: example.com\r\n"; // no blank line yet
        assert!(parse_request_head(req).expect("ok").is_none());
    }

    #[test]
    fn test_parse_request_head_too_large() {
        let mut big = vec![b'X'; MAX_REQ_HEAD_BYTES + 1];
        big.extend_from_slice(b"\r\n\r\n"); // won't be reached
        // Don't include the CRLF CRLF — the fn checks when buf exceeds
        // MAX_REQ_HEAD_BYTES and the marker hasn't been found.
        let just_big = vec![b'X'; MAX_REQ_HEAD_BYTES + 1];
        match parse_request_head(&just_big) {
            Err(EgressParseError::HeadTooLarge) => {}
            other => panic!("expected HeadTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn test_strip_proxy_connection_header() {
        let req = b"GET /foo HTTP/1.1\r\nHost: example.com\r\nProxy-Connection: keep-alive\r\n\r\n";
        let mut head = parse_request_head(req).expect("ok").expect("complete");
        assert_eq!(head.get_header("proxy-connection"), Some("keep-alive"));
        head.remove_header("proxy-connection");
        assert_eq!(head.get_header("proxy-connection"), None);
    }

    #[test]
    fn test_strip_proxy_authorization_header() {
        let req =
            b"GET /foo HTTP/1.1\r\nHost: example.com\r\nProxy-Authorization: Basic xyz\r\n\r\n";
        let mut head = parse_request_head(req).expect("ok").expect("complete");
        head.remove_header("proxy-authorization");
        assert_eq!(head.get_header("proxy-authorization"), None);
    }

    #[test]
    fn test_stripped_request_headers_list() {
        // Guard against drift: the exact list must match the spec.
        assert_eq!(
            STRIPPED_REQUEST_HEADERS,
            &["proxy-connection", "proxy-authorization"]
        );
    }

    // --- serialize_request ---

    #[test]
    fn test_serialize_request_preserves_method_and_target() {
        let head = RequestHead {
            method: "POST".to_string(),
            target: "/v1/chat".to_string(),
            version: "HTTP/1.1".to_string(),
            headers: vec![
                ("host".to_string(), "example.com".to_string()),
                ("content-length".to_string(), "4".to_string()),
            ],
            head_len: 0,
        };
        let wire = serialize_request(&head, None);
        let s = std::str::from_utf8(&wire).expect("utf8");
        assert!(s.starts_with("POST /v1/chat HTTP/1.1\r\n"));
        assert!(s.contains("host: example.com\r\n"));
        assert!(s.contains("content-length: 4\r\n"));
        assert!(s.ends_with("\r\n\r\n"));
    }

    #[test]
    fn test_serialize_request_with_target_override() {
        let head = RequestHead {
            method: "GET".to_string(),
            target: "http://example.com/foo".to_string(),
            version: "HTTP/1.1".to_string(),
            headers: vec![],
            head_len: 0,
        };
        let wire = serialize_request(&head, Some("/foo"));
        let s = std::str::from_utf8(&wire).expect("utf8");
        assert!(s.starts_with("GET /foo HTTP/1.1\r\n"));
    }

    // --- split_host_port_with_default ---

    #[test]
    fn test_split_host_port_with_port() {
        assert_eq!(
            split_host_port_with_default("example.com:8080", 80),
            ("example.com".to_string(), 8080)
        );
    }

    #[test]
    fn test_split_host_port_without_port_uses_default() {
        assert_eq!(
            split_host_port_with_default("example.com", 443),
            ("example.com".to_string(), 443)
        );
    }
}
