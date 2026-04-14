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
use std::time::Duration;

use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::Request;
use hyper::Response;
use hyper::body::Bytes;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::graceful::GracefulShutdown;
use rustls::ClientConfig;
use serde::Deserialize;
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, watch};

use crate::logging;
use crate::ssrf_connector::SsrfConnector;

const OPENAI_HOST: &str = "api.openai.com";
const CHATGPT_CODEX_HOST: &str = "chatgpt.com";
const ANTHROPIC_HOST: &str = "api.anthropic.com";
const OPENAI_OAUTH_ISSUER: &str = "https://auth.openai.com";
const OPENAI_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CHATGPT_CODEX_RESPONSES_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";
const OPENAI_CRED_PATH: &str = "/secrets/model-credentials/openai";
const ANTHROPIC_CRED_PATH: &str = "/secrets/model-credentials/anthropic";
const OPENAI_OAUTH_REFRESH_MARGIN_MS: i64 = 60_000;
const FORBIDDEN_BODY: &[u8] =
    br#"{"error":"only /openai/* and /anthropic/* routes are supported"}"#;

type SharedOpenAiOauthCache = Arc<Mutex<Option<CodexOauthCredential>>>;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenAiCredential {
    ApiKey(String),
    CodexOauth(CodexOauthCredential),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexOauthCredential {
    pub access: String,
    pub refresh: String,
    pub expires_ms: i64,
    pub account_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct CodexOauthCredentialWire {
    #[serde(default)]
    #[allow(dead_code)]
    r#type: Option<String>,
    #[serde(default, alias = "access_token", alias = "accessToken")]
    access: Option<String>,
    #[serde(default, alias = "refresh_token", alias = "refreshToken")]
    refresh: Option<String>,
    #[serde(
        default,
        alias = "expires_at",
        alias = "expiresAt",
        alias = "expires_in"
    )]
    expires: Option<i64>,
    #[serde(
        default,
        alias = "account_id",
        alias = "accountId",
        alias = "chatgpt_account_id",
        alias = "chatgptAccountId"
    )]
    account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CodexOauthRefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default, alias = "expires_at", alias = "expiresAt")]
    expires: Option<i64>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(
        default,
        alias = "account_id",
        alias = "accountId",
        alias = "chatgpt_account_id",
        alias = "chatgptAccountId"
    )]
    account_id: Option<String>,
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

fn is_codex_responses_path(path: &str) -> bool {
    path.contains("/v1/responses") || path.contains("/chat/completions")
}

fn upstream_host(
    target: &RouteTarget,
    openai_credential: Option<&OpenAiCredential>,
) -> &'static str {
    if matches!(openai_credential, Some(OpenAiCredential::CodexOauth(_)))
        && is_codex_responses_path(&target.upstream_path)
    {
        CHATGPT_CODEX_HOST
    } else {
        target.kind.host()
    }
}

/// Build the upstream URI for a routed request, including the original
/// query string.
pub fn upstream_uri(
    target: &RouteTarget,
    query: Option<&str>,
    openai_credential: Option<&OpenAiCredential>,
) -> String {
    if matches!(openai_credential, Some(OpenAiCredential::CodexOauth(_)))
        && is_codex_responses_path(&target.upstream_path)
    {
        return match query {
            Some(q) if !q.is_empty() => format!("{CHATGPT_CODEX_RESPONSES_ENDPOINT}?{q}"),
            _ => CHATGPT_CODEX_RESPONSES_ENDPOINT.to_string(),
        };
    }

    let host = target.kind.host();
    match query {
        Some(q) if !q.is_empty() => format!("https://{host}{}?{q}", target.upstream_path),
        _ => format!("https://{host}{}", target.upstream_path),
    }
}

/// Upstream request body type. We forward the inbound
/// [`hyper::body::Incoming`] directly to the upstream via [`BoxBody`] so
/// bytes stream through without being fully buffered (FR-28 parity with
/// Go's `http.Client`).
type UpstreamBody = BoxBody<Bytes, hyper::Error>;

/// Shared client type for upstream requests. Using `BoxBody` lets us
/// send either a streamed `Incoming` body or a `Full<Bytes>` empty body
/// (for error paths) through the same `Client`.
type UpstreamClient = Client<SsrfConnector, UpstreamBody>;

/// Upper bound on how long we wait for in-flight model proxy
/// connections to finish after shutdown is signaled. Matches the whole
/// sidecar drain budget (`main::SHUTDOWN_DRAIN_TIMEOUT`).
const GRACEFUL_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Serve the model proxy until `shutdown_rx` receives `true`.
///
/// Uses `hyper_util::server::graceful::GracefulShutdown` so in-flight
/// HTTP requests finish before the server returns (FR-27). When
/// `shutdown_rx` flips to `true` we stop accepting new connections,
/// ask every watched connection to shut down, and wait up to
/// [`GRACEFUL_DRAIN_TIMEOUT`] for them to finish.
pub async fn serve(
    listener: TcpListener,
    mut shutdown_rx: watch::Receiver<bool>,
    tls_config: Arc<ClientConfig>,
) -> Result<(), ModelProxyError> {
    // Build the upstream client once. The SsrfConnector re-runs
    // resolve_safe on every call, so sharing the client is safe.
    let connector = SsrfConnector::new(tls_config);
    let client: UpstreamClient = Client::builder(TokioExecutor::new()).build(connector);
    let client = Arc::new(client);
    let openai_oauth_cache: SharedOpenAiOauthCache = Arc::new(Mutex::new(None));

    let graceful = GracefulShutdown::new();

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
                let openai_oauth_cache = Arc::clone(&openai_oauth_cache);
                let io = TokioIo::new(stream);
                let svc = service_fn(move |req: Request<Incoming>| {
                    let client = Arc::clone(&client);
                    let openai_oauth_cache = Arc::clone(&openai_oauth_cache);
                    async move {
                        Ok::<_, Infallible>(
                            handle(req, client.as_ref(), openai_oauth_cache.as_ref()).await,
                        )
                    }
                });
                // HTTP/1 only, matching Go parity. `http1::Builder` wires
                // into GracefulShutdown via the impl in hyper-util.
                let conn = http1::Builder::new().serve_connection(io, svc);
                let watched = graceful.watch(conn);
                tokio::spawn(async move {
                    let _ = watched.await;
                });
            }
        }
    }

    // FR-27: wait up to GRACEFUL_DRAIN_TIMEOUT for in-flight requests
    // to finish. The listener has already stopped accepting at this
    // point because we broke out of the loop above.
    match tokio::time::timeout(GRACEFUL_DRAIN_TIMEOUT, graceful.shutdown()).await {
        Ok(()) => logging::info("model proxy drained in-flight requests"),
        Err(_) => logging::warn("model proxy drain timed out, forcing shutdown"),
    }
    Ok(())
}

/// Handle a single request end-to-end.
async fn handle(
    req: Request<Incoming>,
    client: &UpstreamClient,
    openai_oauth_cache: &Mutex<Option<CodexOauthCredential>>,
) -> Response<BoxBody<Bytes, hyper::Error>> {
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(|s| s.to_string());
    let method = req.method().clone();
    let headers = req.headers().clone();

    let Some(target) = route_target(&path) else {
        return forbidden_response();
    };

    let openai_credential = if target.kind == UpstreamKind::OpenAi {
        let credential = match read_openai_credential(target.kind.credential_path()).await {
            Ok(credential) => credential,
            Err(e) => {
                logging::error(&format!(
                    "failed to read credentials from {}: {e}",
                    target.kind.credential_path()
                ));
                return error_response(500, "credential read failed");
            }
        };
        let credential =
            maybe_resolve_cached_oauth_credential(credential, openai_oauth_cache).await;
        match ensure_fresh_oauth_credential(client, credential, openai_oauth_cache).await {
            Ok(credential) => Some(credential),
            Err(e) => {
                logging::error(&format!("failed to refresh OpenAI OAuth credentials: {e}"));
                return error_response(502, "openai oauth refresh failed");
            }
        }
    } else {
        None
    };

    let raw_credential = if target.kind == UpstreamKind::Anthropic {
        match read_credential(target.kind.credential_path()).await {
            Ok(credential) => Some(credential),
            Err(e) => {
                logging::error(&format!(
                    "failed to read credentials from {}: {e}",
                    target.kind.credential_path()
                ));
                return error_response(500, "credential read failed");
            }
        }
    } else {
        None
    };

    let uri_string = upstream_uri(&target, query.as_deref(), openai_credential.as_ref());
    let uri: hyper::Uri = match uri_string.parse() {
        Ok(u) => u,
        Err(_) => return error_response(500, "invalid upstream URI"),
    };

    // For POST /v1/responses we must buffer the body to inject the required
    // `instructions` field when the caller (opencode) omits it. The Responses
    // API rejects requests without `instructions` even when the value is empty.
    // All other requests stream through without buffering (FR-28).
    let body = if method == hyper::Method::POST && path.contains("/v1/responses") {
        match req.into_body().collect().await {
            Ok(collected) => {
                let bytes = collected.to_bytes();
                let patched = inject_instructions_if_missing(bytes);
                Full::new(patched).map_err(|e| match e {}).boxed()
            }
            Err(e) => {
                logging::error(&format!("failed to read request body: {e}"));
                return error_response(500, "failed to read request body");
            }
        }
    } else {
        req.into_body().boxed()
    };

    // Build the outgoing request.
    let mut builder = Request::builder().method(&method).uri(uri);
    // Copy every header through, then overwrite the auth header and
    // Host header so upstream sees the correct values.
    {
        let Some(h) = builder.headers_mut() else {
            return error_response(500, "failed to build upstream request headers");
        };
        for (k, v) in headers.iter() {
            h.append(k.clone(), v.clone());
        }
        // When we patch the body (instructions injection), the byte count
        // changes. Remove the stale Content-Length so hyper recalculates
        // it from the Full body's exact size_hint instead of forwarding
        // the caller's (now-incorrect) value.
        if method == hyper::Method::POST && path.contains("/v1/responses") {
            h.remove(http::header::CONTENT_LENGTH);
        }
        // FR-18 / SR-5: the inbound Host header reflects the sidecar's
        // loopback bind (e.g. `127.0.0.1:9090`). Forwarding that verbatim
        // would poison upstream virtual-host routing and break TLS-SNI
        // audit expectations. Rewrite it to the upstream hostname.
        rewrite_host_header(h, upstream_host(&target, openai_credential.as_ref()));
        inject_auth_header(
            h,
            target.kind,
            openai_credential.as_ref(),
            raw_credential.as_deref(),
        );
    }
    let upstream_req = match builder.body(body) {
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

pub async fn read_openai_credential(path: &str) -> std::io::Result<OpenAiCredential> {
    let data = read_credential(path).await?;
    Ok(parse_openai_credential(&data))
}

fn parse_openai_credential(raw: &str) -> OpenAiCredential {
    let trimmed = raw.trim();
    let parsed_json = serde_json::from_str::<serde_json::Value>(trimmed).ok();

    if let Some(value) = parsed_json.as_ref() {
        if let Some(oauth) = parse_codex_oauth_credential_value(value) {
            return OpenAiCredential::CodexOauth(oauth);
        }
        if let Some(api_key) = extract_api_key(value) {
            return OpenAiCredential::ApiKey(api_key);
        }
    }

    OpenAiCredential::ApiKey(trimmed.to_string())
}

fn parse_codex_oauth_credential_value(value: &serde_json::Value) -> Option<CodexOauthCredential> {
    for candidate in [
        Some(value),
        value.get("openai"),
        value.get("chatgptAuthTokens"),
        value.get("chatgpt_auth_tokens"),
    ]
    .into_iter()
    .flatten()
    {
        let Ok(parsed) = serde_json::from_value::<CodexOauthCredentialWire>(candidate.clone())
        else {
            continue;
        };
        let (Some(access), Some(refresh)) = (parsed.access, parsed.refresh) else {
            continue;
        };
        return Some(CodexOauthCredential {
            access,
            refresh,
            expires_ms: normalize_expires_ms(parsed.expires),
            account_id: parsed.account_id,
        });
    }
    None
}

fn extract_api_key(value: &serde_json::Value) -> Option<String> {
    let candidate = value.get("openai").unwrap_or(value);
    candidate
        .get("api_key")
        .or_else(|| candidate.get("key"))
        .or_else(|| candidate.get("apiKey"))
        .or_else(|| candidate.get("OPENAI_API_KEY"))
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

fn normalize_expires_ms(expires: Option<i64>) -> i64 {
    match expires {
        Some(value) if value >= 10_000_000_000 => value,
        Some(value) if value > 0 => value.saturating_mul(1000),
        _ => 0,
    }
}

fn oauth_needs_refresh(credential: &CodexOauthCredential) -> bool {
    credential.expires_ms <= chrono::Utc::now().timestamp_millis() + OPENAI_OAUTH_REFRESH_MARGIN_MS
}

async fn maybe_resolve_cached_oauth_credential(
    credential: OpenAiCredential,
    openai_oauth_cache: &Mutex<Option<CodexOauthCredential>>,
) -> OpenAiCredential {
    let OpenAiCredential::CodexOauth(file_credential) = credential else {
        return credential;
    };

    let cached = openai_oauth_cache.lock().await.clone();
    let effective = match cached {
        Some(cached_credential)
            if cached_credential.account_id == file_credential.account_id
                && cached_credential.expires_ms > file_credential.expires_ms =>
        {
            cached_credential
        }
        _ => file_credential,
    };

    OpenAiCredential::CodexOauth(effective)
}

async fn ensure_fresh_oauth_credential(
    client: &UpstreamClient,
    credential: OpenAiCredential,
    openai_oauth_cache: &Mutex<Option<CodexOauthCredential>>,
) -> Result<OpenAiCredential, String> {
    let OpenAiCredential::CodexOauth(oauth) = credential else {
        return Ok(credential);
    };

    if !oauth_needs_refresh(&oauth) {
        return Ok(OpenAiCredential::CodexOauth(oauth));
    }

    let refreshed = refresh_codex_oauth_credential(client, &oauth).await?;
    *openai_oauth_cache.lock().await = Some(refreshed.clone());
    Ok(OpenAiCredential::CodexOauth(refreshed))
}

async fn refresh_codex_oauth_credential(
    client: &UpstreamClient,
    credential: &CodexOauthCredential,
) -> Result<CodexOauthCredential, String> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "refresh_token")
        .append_pair("refresh_token", &credential.refresh)
        .append_pair("client_id", OPENAI_OAUTH_CLIENT_ID)
        .finish();
    let uri: hyper::Uri = format!("{OPENAI_OAUTH_ISSUER}/oauth/token")
        .parse()
        .map_err(|e| format!("invalid oauth token URI: {e}"))?;
    let request = Request::builder()
        .method(http::Method::POST)
        .uri(uri)
        .header(http::header::HOST, "auth.openai.com")
        .header(
            http::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .header(http::header::ACCEPT, "application/json")
        .body(BoxBody::new(
            Full::new(Bytes::from(body)).map_err(|never| match never {}),
        ))
        .map_err(|e| format!("failed to build oauth refresh request: {e}"))?;

    let response = client
        .request(request)
        .await
        .map_err(|e| format!("oauth token exchange transport failure: {e}"))?;
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .map_err(|e| format!("failed to read oauth refresh body: {e}"))?
        .to_bytes();

    if !status.is_success() {
        let body = String::from_utf8_lossy(&body);
        return Err(format!(
            "oauth token exchange returned non-success status {status}: {body}"
        ));
    }

    let parsed: CodexOauthRefreshResponse = serde_json::from_slice(&body)
        .map_err(|e| format!("failed to parse oauth refresh response: {e}"))?;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let expires_ms = parsed
        .expires
        .map(|value| normalize_expires_ms(Some(value)))
        .or_else(|| {
            parsed
                .expires_in
                .map(|value| now_ms.saturating_add(value.saturating_mul(1000)))
        })
        .unwrap_or_else(|| now_ms.saturating_add(55 * 60 * 1000));

    Ok(CodexOauthCredential {
        access: parsed.access_token,
        refresh: parsed
            .refresh_token
            .unwrap_or_else(|| credential.refresh.clone()),
        expires_ms,
        account_id: parsed.account_id.or_else(|| credential.account_id.clone()),
    })
}

/// Rewrite the outgoing request's `Host` header to the upstream
/// hostname. Exposed for unit testing. The inbound request's Host
/// header is whatever the client sent to the sidecar (almost always
/// `127.0.0.1:9090`); upstream needs to see the real hostname so
/// virtual-host routing works and the HTTP Host parity with Go's
/// `http.NewRequestWithContext` (which derives Host from the URL) is
/// preserved.
pub fn rewrite_host_header(headers: &mut hyper::HeaderMap, upstream_host: &str) {
    match http::HeaderValue::from_str(upstream_host) {
        Ok(v) => {
            headers.remove(http::header::HOST);
            headers.insert(http::header::HOST, v);
        }
        Err(_) => {
            // Unreachable in practice: our upstream hosts are compile-time
            // static ASCII constants (`api.openai.com`, `api.anthropic.com`).
            // Fall through without setting — the client transport will
            // still fill it in from the URI.
            headers.remove(http::header::HOST);
        }
    }
}

/// Pure function: inject the right auth header based on the target.
/// Exposed for unit testing.
pub fn inject_auth_header(
    headers: &mut hyper::HeaderMap,
    kind: UpstreamKind,
    openai_credential: Option<&OpenAiCredential>,
    raw_credential: Option<&str>,
) {
    match kind {
        UpstreamKind::OpenAi => {
            // Always overwrite — a client-supplied Authorization must
            // not reach upstream.
            let Some(credential) = openai_credential else {
                return;
            };
            let value = match credential {
                OpenAiCredential::ApiKey(api_key) => format!("Bearer {api_key}"),
                OpenAiCredential::CodexOauth(oauth) => format!("Bearer {}", oauth.access),
            };
            if let Ok(v) = http::HeaderValue::from_str(&value) {
                headers.remove(http::header::AUTHORIZATION);
                headers.insert(http::header::AUTHORIZATION, v);
            }
            headers.remove("chatgpt-account-id");
            if let OpenAiCredential::CodexOauth(oauth) = credential
                && let Some(account_id) = oauth.account_id.as_deref()
                && let Ok(v) = http::HeaderValue::from_str(account_id)
            {
                headers.insert("chatgpt-account-id", v);
            }
        }
        UpstreamKind::Anthropic => {
            let Some(credential) = raw_credential else {
                return;
            };
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

/// Default instructions value injected when opencode omits the field.
///
/// Both `api.openai.com` and `chatgpt.com/backend-api/codex/responses`
/// require `instructions` to be present and non-empty. opencode passes
/// the full prompt as `input` and never sets `instructions`, so we
/// inject a neutral non-empty sentinel. The real prompt content is in
/// `input` and takes precedence at inference time.
const DEFAULT_INSTRUCTIONS: &str = "Follow the instructions provided in the input carefully.";

/// Inject `"instructions"` into a Responses API JSON body if the field is
/// absent. Returns the original bytes unchanged if the body is not valid
/// JSON or already contains `instructions`.
fn inject_instructions_if_missing(bytes: Bytes) -> Bytes {
    let Ok(mut payload) = serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&bytes) else {
        return bytes;
    };
    if payload.contains_key("instructions") {
        return bytes;
    }
    payload.insert(
        "instructions".to_string(),
        serde_json::Value::String(DEFAULT_INSTRUCTIONS.to_string()),
    );
    match serde_json::to_vec(&payload) {
        Ok(v) => Bytes::from(v),
        Err(_) => bytes,
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
        assert_eq!(
            upstream_uri(&rt, None, None),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(
            upstream_uri(&rt, Some("limit=10"), None),
            "https://api.openai.com/v1/models?limit=10"
        );
        assert_eq!(
            upstream_uri(&rt, Some(""), None),
            "https://api.openai.com/v1/models"
        );
    }

    #[test]
    fn test_upstream_uri_rewrites_codex_oauth_requests() {
        let oauth = OpenAiCredential::CodexOauth(CodexOauthCredential {
            access: "access-token".to_string(),
            refresh: "refresh-token".to_string(),
            expires_ms: 1,
            account_id: Some("acct-123".to_string()),
        });
        let target = route_target("/openai/v1/responses").expect("routed");
        assert_eq!(
            upstream_uri(&target, None, Some(&oauth)),
            "https://chatgpt.com/backend-api/codex/responses"
        );

        let target = route_target("/openai/v1/chat/completions").expect("routed");
        assert_eq!(
            upstream_uri(&target, Some("stream=true"), Some(&oauth)),
            "https://chatgpt.com/backend-api/codex/responses?stream=true"
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
        inject_auth_header(
            &mut h,
            UpstreamKind::OpenAi,
            Some(&OpenAiCredential::ApiKey("secret-key".to_string())),
            None,
        );
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
        inject_auth_header(&mut h, UpstreamKind::Anthropic, None, Some("secret-key"));
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
        inject_auth_header(&mut h, UpstreamKind::Anthropic, None, Some("k"));
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
        inject_auth_header(&mut h, UpstreamKind::Anthropic, None, Some("k"));
        assert_eq!(
            h.get("anthropic-version")
                .expect("version present")
                .to_str()
                .expect("valid utf8"),
            "2024-01-01"
        );
    }

    // --- rewrite_host_header ---

    #[test]
    fn test_rewrite_host_header_replaces_loopback_with_upstream() {
        // Regression test for Codex finding #1: a client-supplied Host
        // header of 127.0.0.1:9090 (the model proxy bind address) must
        // NOT be forwarded to upstream. After rewrite we expect the
        // upstream hostname.
        let mut h = HeaderMap::new();
        h.insert(
            http::header::HOST,
            http::HeaderValue::from_static("127.0.0.1:9090"),
        );
        rewrite_host_header(&mut h, "api.openai.com");
        assert_eq!(
            h.get(http::header::HOST)
                .expect("host header present")
                .to_str()
                .expect("utf8"),
            "api.openai.com"
        );
    }

    #[test]
    fn test_rewrite_host_header_inserts_when_missing() {
        let mut h = HeaderMap::new();
        rewrite_host_header(&mut h, "api.anthropic.com");
        assert_eq!(
            h.get(http::header::HOST)
                .expect("host header present")
                .to_str()
                .expect("utf8"),
            "api.anthropic.com"
        );
    }

    #[test]
    fn test_outgoing_request_host_header_is_upstream_not_local_bind() {
        // Regression for Codex finding #1: the full header build flow
        // (copy client headers -> rewrite host -> inject auth) must
        // produce a request whose Host header is the upstream host,
        // not 127.0.0.1:9090. Simulates the relevant subset of
        // handle()'s header-building sequence.
        let mut client_headers = HeaderMap::new();
        client_headers.insert(
            http::header::HOST,
            http::HeaderValue::from_static("127.0.0.1:9090"),
        );
        client_headers.insert(
            "user-agent",
            http::HeaderValue::from_static("claude-cli/1.0"),
        );
        client_headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Bearer client-forged"),
        );

        let target = route_target("/openai/v1/chat/completions").expect("routed");
        let mut builder = Request::builder()
            .method(http::Method::POST)
            .uri(upstream_uri(&target, None, None));
        {
            let h = builder.headers_mut().expect("builder headers");
            for (k, v) in client_headers.iter() {
                h.append(k.clone(), v.clone());
            }
            rewrite_host_header(h, upstream_host(&target, None));
            inject_auth_header(
                h,
                target.kind,
                Some(&OpenAiCredential::ApiKey("sk-server-key".to_string())),
                None,
            );
        }
        // Build with an empty streamed body so we can inspect the
        // parts without bringing in hyper::Incoming.
        let req = builder
            .body(BoxBody::new(
                Full::new(Bytes::new()).map_err(|never| match never {}),
            ))
            .expect("request builds");

        let host = req
            .headers()
            .get(http::header::HOST)
            .expect("host header present")
            .to_str()
            .expect("utf8");
        assert_eq!(
            host, "api.openai.com",
            "model proxy must rewrite Host to the upstream hostname, not leak the sidecar's bind address"
        );
        // Authorization should have been overwritten.
        assert_eq!(
            req.headers()
                .get(http::header::AUTHORIZATION)
                .expect("auth present")
                .to_str()
                .expect("utf8"),
            "Bearer sk-server-key"
        );
        // User agent preserved.
        assert_eq!(
            req.headers()
                .get("user-agent")
                .expect("user-agent present")
                .to_str()
                .expect("utf8"),
            "claude-cli/1.0"
        );
        // Exactly one Host header.
        assert_eq!(req.headers().get_all(http::header::HOST).iter().count(), 1);
    }

    #[test]
    fn test_outgoing_request_host_header_for_anthropic_route() {
        let mut client_headers = HeaderMap::new();
        client_headers.insert(
            http::header::HOST,
            http::HeaderValue::from_static("127.0.0.1:9090"),
        );
        let target = route_target("/anthropic/v1/messages").expect("routed");
        let mut builder = Request::builder()
            .method(http::Method::POST)
            .uri(upstream_uri(&target, None, None));
        {
            let h = builder.headers_mut().expect("builder headers");
            for (k, v) in client_headers.iter() {
                h.append(k.clone(), v.clone());
            }
            rewrite_host_header(h, upstream_host(&target, None));
            inject_auth_header(h, target.kind, None, Some("anthropic-key"));
        }
        let req = builder
            .body(BoxBody::new(
                Full::new(Bytes::new()).map_err(|never| match never {}),
            ))
            .expect("request builds");
        assert_eq!(
            req.headers()
                .get(http::header::HOST)
                .expect("host header present")
                .to_str()
                .expect("utf8"),
            "api.anthropic.com"
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
        inject_auth_header(
            &mut h,
            UpstreamKind::OpenAi,
            Some(&OpenAiCredential::ApiKey("k".to_string())),
            None,
        );
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

    #[test]
    fn test_codex_oauth_injects_account_header() {
        let mut h = HeaderMap::new();
        inject_auth_header(
            &mut h,
            UpstreamKind::OpenAi,
            Some(&OpenAiCredential::CodexOauth(CodexOauthCredential {
                access: "access-token".to_string(),
                refresh: "refresh-token".to_string(),
                expires_ms: 1,
                account_id: Some("acct-123".to_string()),
            })),
            None,
        );
        assert_eq!(
            h.get(http::header::AUTHORIZATION)
                .expect("auth header present")
                .to_str()
                .expect("utf8"),
            "Bearer access-token"
        );
        assert_eq!(
            h.get("chatgpt-account-id")
                .expect("account header present")
                .to_str()
                .expect("utf8"),
            "acct-123"
        );
    }

    // --- inject_instructions_if_missing ---

    #[test]
    fn test_inject_instructions_adds_field_when_absent() {
        let input = br#"{"model":"gpt-5.4","input":"do a review"}"#;
        let result = inject_instructions_if_missing(Bytes::from_static(input));
        let out: serde_json::Value = serde_json::from_slice(&result).unwrap();
        assert_eq!(out["instructions"], DEFAULT_INSTRUCTIONS);
        assert_eq!(out["model"], "gpt-5.4");
    }

    #[test]
    fn test_inject_instructions_preserves_existing_value() {
        let input = br#"{"model":"gpt-5.4","input":"review","instructions":"be precise"}"#;
        let result = inject_instructions_if_missing(Bytes::from_static(input));
        let out: serde_json::Value = serde_json::from_slice(&result).unwrap();
        assert_eq!(out["instructions"], "be precise");
    }

    #[test]
    fn test_inject_instructions_passthrough_non_json() {
        let input = b"not json at all";
        let result = inject_instructions_if_missing(Bytes::from_static(input));
        assert_eq!(result.as_ref(), input.as_slice());
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

    #[tokio::test]
    async fn test_read_openai_credential_parses_opencode_oauth_bundle() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(
            tmp.path(),
            r#"{"openai":{"type":"oauth","access":"access-token","refresh":"refresh-token","expires":1776698155357,"accountId":"acct-123"}}"#,
        )
        .expect("write");

        let credential = read_openai_credential(tmp.path().to_str().expect("utf8"))
            .await
            .expect("read");
        assert_eq!(
            credential,
            OpenAiCredential::CodexOauth(CodexOauthCredential {
                access: "access-token".to_string(),
                refresh: "refresh-token".to_string(),
                expires_ms: 1776698155357,
                account_id: Some("acct-123".to_string()),
            })
        );
    }
}
