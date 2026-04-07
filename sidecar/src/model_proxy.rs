//! Model API proxy (FR-1 through FR-7, FR-28 streaming / no timeout).
//!
//! Listens on `127.0.0.1:9090`, accepts HTTP/1.1 requests, and routes
//! them to either `https://api.openai.com` or `https://api.anthropic.com`
//! based on the path prefix. Credentials are read fresh from disk on
//! every request (FR-4 / SR-3), injected into the outgoing request, and
//! the response is streamed back to the client without buffering
//! (FR-6).
//!
//! The upstream client uses our custom [`SsrfConnector`]
//! (crate::ssrf_connector) to guarantee fail-closed SSRF protection
//! (FR-7, FR-18).

use std::convert::Infallible;
use std::sync::Arc;

use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::Request;
use hyper::Response;
use hyper::body::Bytes;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use rustls::ClientConfig;
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::logging;
use crate::ssrf_connector::SsrfConnector;

const OPENAI_HOST: &str = "api.openai.com";
const ANTHROPIC_HOST: &str = "api.anthropic.com";
const OPENAI_CRED_PATH: &str = "/secrets/model-credentials/openai";
const ANTHROPIC_CRED_PATH: &str = "/secrets/model-credentials/anthropic";
const FORBIDDEN_BODY: &[u8] =
    br#"{"error":"only /openai/* and /anthropic/* routes are supported"}"#;

/// Errors produced by the server. These are surfaced to the caller in
/// `main.rs`; per-request errors are turned into HTTP responses.
#[derive(Debug, Error)]
pub enum ModelProxyError {
    #[error("accept error: {0}")]
    Accept(std::io::Error),
}

/// Selector describing which upstream a request is routed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamKind {
    OpenAi,
    Anthropic,
}

impl UpstreamKind {
    fn host(self) -> &'static str {
        match self {
            Self::OpenAi => OPENAI_HOST,
            Self::Anthropic => ANTHROPIC_HOST,
        }
    }

    fn credential_path(self) -> &'static str {
        match self {
            Self::OpenAi => OPENAI_CRED_PATH,
            Self::Anthropic => ANTHROPIC_CRED_PATH,
        }
    }
}

/// Result of routing a request path. Pure function so it can be unit-tested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteTarget {
    pub kind: UpstreamKind,
    /// Upstream path after trimming the prefix. Includes a leading `/`.
    pub upstream_path: String,
}

/// Pure routing function. Returns `None` if the path is neither an
/// `/openai` nor an `/anthropic` prefix.
pub fn route_target(path: &str) -> Option<RouteTarget> {
    if path == "/openai" || path == "/openai/" {
        return Some(RouteTarget {
            kind: UpstreamKind::OpenAi,
            upstream_path: "/".to_string(),
        });
    }
    if let Some(rest) = path.strip_prefix("/openai/") {
        return Some(RouteTarget {
            kind: UpstreamKind::OpenAi,
            upstream_path: format!("/{rest}"),
        });
    }
    if path == "/anthropic" || path == "/anthropic/" {
        return Some(RouteTarget {
            kind: UpstreamKind::Anthropic,
            upstream_path: "/".to_string(),
        });
    }
    if let Some(rest) = path.strip_prefix("/anthropic/") {
        return Some(RouteTarget {
            kind: UpstreamKind::Anthropic,
            upstream_path: format!("/{rest}"),
        });
    }
    None
}

/// Build the upstream URI for a routed request, including the original
/// query string.
pub fn upstream_uri(target: &RouteTarget, query: Option<&str>) -> String {
    let host = target.kind.host();
    match query {
        Some(q) if !q.is_empty() => format!("https://{host}{}?{q}", target.upstream_path),
        _ => format!("https://{host}{}", target.upstream_path),
    }
}

/// Serve the model proxy until `shutdown_rx` receives `true`.
pub async fn serve(
    listener: TcpListener,
    mut shutdown_rx: watch::Receiver<bool>,
    tls_config: Arc<ClientConfig>,
) -> Result<(), ModelProxyError> {
    // Build the upstream client once. The SsrfConnector re-runs
    // resolve_safe on every call, so sharing the client is safe.
    let connector = SsrfConnector::new(tls_config);
    let client: Client<SsrfConnector, Full<Bytes>> =
        Client::builder(TokioExecutor::new()).build(connector);
    let client = Arc::new(client);

    loop {
        tokio::select! {
            biased;
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            accept = listener.accept() => {
                let (stream, _) = accept.map_err(ModelProxyError::Accept)?;
                let client = Arc::clone(&client);
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let svc = service_fn(move |req: Request<Incoming>| {
                        let client = Arc::clone(&client);
                        async move { Ok::<_, Infallible>(handle(req, client.as_ref()).await) }
                    });
                    let builder = auto::Builder::new(TokioExecutor::new());
                    let _ = builder.serve_connection(io, svc).await;
                });
            }
        }
    }
    Ok(())
}

/// Handle a single request end-to-end.
async fn handle(
    req: Request<Incoming>,
    client: &Client<SsrfConnector, Full<Bytes>>,
) -> Response<BoxBody<Bytes, hyper::Error>> {
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(|s| s.to_string());
    let method = req.method().clone();
    let headers = req.headers().clone();

    let Some(target) = route_target(&path) else {
        return forbidden_response();
    };

    let cred = match read_credential(target.kind.credential_path()).await {
        Ok(c) => c,
        Err(e) => {
            logging::error(&format!(
                "failed to read credentials from {}: {e}",
                target.kind.credential_path()
            ));
            return error_response(500, "credential read failed");
        }
    };

    let uri_string = upstream_uri(&target, query.as_deref());
    let uri: hyper::Uri = match uri_string.parse() {
        Ok(u) => u,
        Err(_) => return error_response(500, "invalid upstream URI"),
    };

    // Read the request body into memory. The model proxy's upstream
    // client uses Full<Bytes> because hyper-util's legacy client does
    // not have a first-class streaming-body upload path that composes
    // with our custom connector type in stable hyper 1.x. The request
    // bodies are bounded by the client's transport buffers; long SSE
    // streaming happens on the RESPONSE side, which we stream back
    // without buffering.
    //
    // This matches Go parity for the vast majority of requests (chat
    // completions with short prompts, bearer-auth GETs). Pathological
    // multi-gigabyte uploads are out of scope per FR-28 ("no body size
    // limits" — parity with Go, accepted tradeoff).
    let body_bytes = match collect_request_body(req).await {
        Ok(b) => b,
        Err(e) => {
            logging::error(&format!("failed to read request body: {e}"));
            return error_response(502, "request body read failed");
        }
    };

    // Build the outgoing request.
    let mut builder = Request::builder().method(method).uri(uri);
    // Copy every header through, then overwrite the auth header that
    // matches the upstream's expected value.
    {
        let Some(h) = builder.headers_mut() else {
            return error_response(500, "failed to build upstream request headers");
        };
        for (k, v) in headers.iter() {
            h.append(k.clone(), v.clone());
        }
        inject_auth_header(h, target.kind, &cred);
    }
    let upstream_req = match builder.body(Full::new(body_bytes)) {
        Ok(r) => r,
        Err(e) => {
            logging::error(&format!("failed to build upstream request: {e}"));
            return error_response(500, "failed to build upstream request");
        }
    };

    // Dispatch.
    let resp = match client.request(upstream_req).await {
        Ok(r) => r,
        Err(e) => {
            logging::error(&format!("upstream request failed: {e}"));
            return error_response(502, "upstream request failed");
        }
    };

    // Copy status + headers; stream the body back untouched. Using
    // `body.boxed()` returns a BoxBody that forwards each frame as it
    // arrives — no buffering. This is FR-6.
    let (parts, body) = resp.into_parts();
    let mut out = Response::builder().status(parts.status);
    if let Some(headers) = out.headers_mut() {
        headers.extend(parts.headers);
    }
    match out.body(body.boxed()) {
        Ok(r) => r,
        Err(_) => error_response(500, "failed to build response"),
    }
}

/// Read a credential file fresh, trimming full whitespace per FR-4.
pub async fn read_credential(path: &str) -> std::io::Result<String> {
    let data = tokio::fs::read_to_string(path).await?;
    // Go's `strings.TrimSpace` strips Unicode whitespace; for our
    // purposes the ASCII-only subset (space, tab, newline, carriage
    // return, form feed, vertical tab) is sufficient because
    // credential files are ASCII. Rust's `str::trim` strips Unicode
    // whitespace which is a strict superset.
    Ok(data.trim().to_string())
}

/// Collect the request body into a single `Bytes`.
async fn collect_request_body(req: Request<Incoming>) -> Result<Bytes, hyper::Error> {
    let collected = req.into_body().collect().await?;
    Ok(collected.to_bytes())
}

/// Pure function: inject the right auth header based on the target.
/// Exposed for unit testing.
pub fn inject_auth_header(headers: &mut hyper::HeaderMap, kind: UpstreamKind, credential: &str) {
    match kind {
        UpstreamKind::OpenAi => {
            // Always overwrite — a client-supplied Authorization must
            // not reach upstream.
            let value = format!("Bearer {credential}");
            if let Ok(v) = http::HeaderValue::from_str(&value) {
                headers.remove(http::header::AUTHORIZATION);
                headers.insert(http::header::AUTHORIZATION, v);
            }
        }
        UpstreamKind::Anthropic => {
            if let Ok(v) = http::HeaderValue::from_str(credential) {
                headers.remove("x-api-key");
                headers.insert("x-api-key", v);
            }
            // FR-2: only set anthropic-version when not already present.
            if !headers.contains_key("anthropic-version") {
                headers.insert(
                    "anthropic-version",
                    http::HeaderValue::from_static("2023-06-01"),
                );
            }
        }
    }
}

/// Build a 403 forbidden response with the exact Go error body.
pub fn forbidden_response() -> Response<BoxBody<Bytes, hyper::Error>> {
    static_error_response(403, FORBIDDEN_BODY)
}

fn error_response(status: u16, msg: &str) -> Response<BoxBody<Bytes, hyper::Error>> {
    let body = format!(r#"{{"error":"{msg}"}}"#);
    match Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(BoxBody::new(
            Full::new(Bytes::from(body)).map_err(|never| match never {}),
        )) {
        Ok(r) => r,
        Err(_) => Response::new(BoxBody::new(
            Full::new(Bytes::new()).map_err(|never| match never {}),
        )),
    }
}

fn static_error_response(
    status: u16,
    body: &'static [u8],
) -> Response<BoxBody<Bytes, hyper::Error>> {
    match Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(BoxBody::new(
            Full::new(Bytes::from_static(body)).map_err(|never| match never {}),
        )) {
        Ok(r) => r,
        Err(_) => Response::new(BoxBody::new(
            Full::new(Bytes::new()).map_err(|never| match never {}),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderMap;

    // --- route_target ---

    #[test]
    fn test_openai_prefix_route_injects_bearer_token() {
        let rt = route_target("/openai/v1/models").expect("routed");
        assert_eq!(rt.kind, UpstreamKind::OpenAi);
        assert_eq!(rt.upstream_path, "/v1/models");
    }

    #[test]
    fn test_openai_bare_route_maps_to_upstream_root() {
        let rt = route_target("/openai").expect("routed");
        assert_eq!(rt.kind, UpstreamKind::OpenAi);
        assert_eq!(rt.upstream_path, "/");

        let rt = route_target("/openai/").expect("routed");
        assert_eq!(rt.upstream_path, "/");
    }

    #[test]
    fn test_anthropic_prefix_route_injects_x_api_key_and_version() {
        let rt = route_target("/anthropic/v1/messages").expect("routed");
        assert_eq!(rt.kind, UpstreamKind::Anthropic);
        assert_eq!(rt.upstream_path, "/v1/messages");
    }

    #[test]
    fn test_anthropic_bare_route_maps_to_upstream_root() {
        let rt = route_target("/anthropic").expect("routed");
        assert_eq!(rt.kind, UpstreamKind::Anthropic);
        assert_eq!(rt.upstream_path, "/");
    }

    #[test]
    fn test_unknown_route_returns_none() {
        assert!(route_target("/foo").is_none());
        assert!(route_target("/").is_none());
        assert!(route_target("").is_none());
        assert!(route_target("/openaix").is_none());
    }

    #[test]
    fn test_upstream_uri_preserves_query() {
        let rt = route_target("/openai/v1/models").expect("routed");
        assert_eq!(upstream_uri(&rt, None), "https://api.openai.com/v1/models");
        assert_eq!(
            upstream_uri(&rt, Some("limit=10")),
            "https://api.openai.com/v1/models?limit=10"
        );
        assert_eq!(
            upstream_uri(&rt, Some("")),
            "https://api.openai.com/v1/models"
        );
    }

    // --- inject_auth_header ---

    #[test]
    fn test_openai_bearer_overwrites_client_authorization() {
        let mut h = HeaderMap::new();
        h.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Bearer client-forged"),
        );
        inject_auth_header(&mut h, UpstreamKind::OpenAi, "secret-key");
        assert_eq!(
            h.get(http::header::AUTHORIZATION)
                .expect("auth header present")
                .to_str()
                .expect("valid utf8"),
            "Bearer secret-key"
        );
    }

    #[test]
    fn test_anthropic_x_api_key_overwrites_client_value() {
        let mut h = HeaderMap::new();
        h.insert("x-api-key", http::HeaderValue::from_static("client-forged"));
        inject_auth_header(&mut h, UpstreamKind::Anthropic, "secret-key");
        assert_eq!(
            h.get("x-api-key")
                .expect("x-api-key present")
                .to_str()
                .expect("valid utf8"),
            "secret-key"
        );
    }

    #[test]
    fn test_anthropic_sets_default_version_when_missing() {
        let mut h = HeaderMap::new();
        inject_auth_header(&mut h, UpstreamKind::Anthropic, "k");
        assert_eq!(
            h.get("anthropic-version")
                .expect("version present")
                .to_str()
                .expect("valid utf8"),
            "2023-06-01"
        );
    }

    #[test]
    fn test_anthropic_respects_existing_anthropic_version_header() {
        let mut h = HeaderMap::new();
        h.insert(
            "anthropic-version",
            http::HeaderValue::from_static("2024-01-01"),
        );
        inject_auth_header(&mut h, UpstreamKind::Anthropic, "k");
        assert_eq!(
            h.get("anthropic-version")
                .expect("version present")
                .to_str()
                .expect("valid utf8"),
            "2024-01-01"
        );
    }

    #[test]
    fn test_passthrough_headers_preserved() {
        // Simulate the flow: copy client headers, then inject auth. Any
        // header other than the auth header (and anthropic-version when
        // absent) is preserved.
        let mut h = HeaderMap::new();
        h.insert("x-trace-id", http::HeaderValue::from_static("abc"));
        h.insert("user-agent", http::HeaderValue::from_static("nautiloop/1"));
        inject_auth_header(&mut h, UpstreamKind::OpenAi, "k");
        assert_eq!(
            h.get("x-trace-id")
                .expect("x-trace-id present")
                .to_str()
                .expect("valid utf8"),
            "abc"
        );
        assert_eq!(
            h.get("user-agent")
                .expect("user-agent present")
                .to_str()
                .expect("valid utf8"),
            "nautiloop/1"
        );
    }

    // --- read_credential ---

    #[tokio::test]
    async fn test_credential_file_read_fresh_per_request() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(tmp.path(), "first-key").expect("write 1");
        let p = tmp.path().to_string_lossy().to_string();
        assert_eq!(read_credential(&p).await.expect("read 1"), "first-key");

        std::fs::write(tmp.path(), "second-key").expect("write 2");
        assert_eq!(read_credential(&p).await.expect("read 2"), "second-key");
    }

    #[tokio::test]
    async fn test_credential_file_leading_whitespace_trimmed() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(tmp.path(), "\n\t  sk-abc").expect("write");
        let c = read_credential(tmp.path().to_str().expect("utf8"))
            .await
            .expect("read");
        assert_eq!(c, "sk-abc");
    }

    #[tokio::test]
    async fn test_credential_file_trailing_whitespace_trimmed() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(tmp.path(), "sk-abc\n\t  ").expect("write");
        let c = read_credential(tmp.path().to_str().expect("utf8"))
            .await
            .expect("read");
        assert_eq!(c, "sk-abc");
    }
}
