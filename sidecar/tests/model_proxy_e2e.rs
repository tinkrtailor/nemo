//! End-to-end integration tests for the model API proxy.
//!
//! Topology:
//!
//! ```text
//!  [test HTTP client]
//!        |  TCP, port P1 on 127.0.0.1
//!        v
//!  [model_proxy::serve_for_test (plain HTTP connector)]
//!        |  TCP, port P2 on 127.0.0.1 (mock upstream)
//!        v
//!  [mock HTTP server (hyper service_fn)]
//! ```
//!
//! The mock server captures every inbound request (method, path, headers,
//! body) into a shared `Vec<CapturedRequest>` so each test can assert on
//! what the sidecar actually sent upstream.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use nautiloop_sidecar::model_proxy::{TestProxyConfig, serve_for_test};
use tokio::net::TcpListener;
use tokio::sync::watch;

// ─── captured request ───────────────────────────────────────────────────────

#[allow(dead_code)]
struct CapturedRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

// ─── mock upstream server ────────────────────────────────────────────────────

/// Spawn a plain HTTP/1.1 server on a random port.
///
/// Returns the bound address and a shared buffer of captured requests.
/// Every request is answered with `200 OK` + `{}` body.
async fn start_mock_upstream() -> (SocketAddr, Arc<Mutex<Vec<CapturedRequest>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("mock bind");
    let addr = listener.local_addr().expect("mock addr");
    let captured: Arc<Mutex<Vec<CapturedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_clone = Arc::clone(&captured);

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let captured = Arc::clone(&captured_clone);
            let io = TokioIo::new(stream);
            tokio::spawn(async move {
                let svc = service_fn(move |req: Request<Incoming>| {
                    let captured = Arc::clone(&captured);
                    async move {
                        let method = req.method().to_string();
                        let path = req.uri().path().to_string();
                        let headers: HashMap<String, String> = req
                            .headers()
                            .iter()
                            .map(|(k, v)| {
                                (
                                    k.as_str().to_lowercase(),
                                    v.to_str().unwrap_or("").to_string(),
                                )
                            })
                            .collect();
                        let body = req
                            .into_body()
                            .collect()
                            .await
                            .unwrap_or_else(|_| http_body_util::Collected::default())
                            .to_bytes()
                            .to_vec();
                        {
                            let mut guard = captured.lock().expect("lock");
                            guard.push(CapturedRequest {
                                method,
                                path,
                                headers,
                                body,
                            });
                        }
                        let resp = Response::builder()
                            .status(200)
                            .header("content-type", "application/json")
                            .body(Full::new(Bytes::from_static(b"{}")))
                            .expect("response");
                        Ok::<_, Infallible>(resp)
                    }
                });
                let _ = http1::Builder::new().serve_connection(io, svc).await;
            });
        }
    });

    (addr, captured)
}

// ─── proxy starter ───────────────────────────────────────────────────────────

/// Start the model proxy with a `TestProxyConfig` pointing at mock upstreams.
///
/// Returns the sidecar's bound address and a shutdown sender.
async fn start_proxy(config: TestProxyConfig) -> (SocketAddr, watch::Sender<bool>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("proxy bind");
    let addr = listener.local_addr().expect("proxy addr");
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let config = Arc::new(config);

    tokio::spawn(async move {
        let _ = serve_for_test(listener, shutdown_rx, config).await;
    });

    // Give the server a moment to start accepting.
    tokio::time::sleep(Duration::from_millis(20)).await;

    (addr, shutdown_tx)
}

// ─── helpers ─────────────────────────────────────────────────────────────────

/// Write a plain-string credential file (OpenAI API key or Anthropic key).
fn write_string_cred(dir: &tempfile::TempDir, name: &str, content: &str) -> String {
    let path = dir.path().join(name);
    std::fs::write(&path, content).expect("write cred");
    path.to_string_lossy().to_string()
}

/// Write a CodexOauth JSON credential file with an access token that will
/// not expire during the test (expires_at far in the future).
fn write_codex_oauth_cred(dir: &tempfile::TempDir, name: &str) -> String {
    let json = r#"{
        "access_token": "fake-access-token",
        "refresh_token": "fake-refresh-token",
        "expires_at": 9999999999000,
        "chatgpt_account_id": "acct-test"
    }"#;
    let path = dir.path().join(name);
    std::fs::write(&path, json).expect("write codex cred");
    path.to_string_lossy().to_string()
}

/// Send a POST request to the sidecar proxy and return status + response body.
async fn post(proxy_addr: SocketAddr, path: &str, body: &str) -> (u16, String) {
    use hyper_util::client::legacy::Client;
    use hyper_util::client::legacy::connect::HttpConnector;

    let client: Client<HttpConnector, _> =
        Client::builder(TokioExecutor::new()).build(HttpConnector::new());
    let uri: hyper::Uri = format!("http://{proxy_addr}{path}").parse().expect("uri");
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .expect("req");

    let resp = client.request(req).await.expect("send");
    let status = resp.status().as_u16();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect")
        .to_bytes();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

// ─── tests ───────────────────────────────────────────────────────────────────

/// POST /openai/v1/responses with a plain API key credential.
///
/// Assert: upstream receives `instructions` injected, `max_output_tokens`
/// unchanged (not renamed), Content-Length absent, Authorization = Bearer.
#[tokio::test]
async fn test_api_key_injects_instructions() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (mock_addr, captured) = start_mock_upstream().await;
    let base = format!("http://127.0.0.1:{}", mock_addr.port());

    let openai_cred = write_string_cred(&tmp, "openai", "fake-api-key");
    let anthropic_cred = write_string_cred(&tmp, "anthropic", "fake-anthropic-key");

    let (proxy_addr, _shutdown) = start_proxy(TestProxyConfig {
        openai_cred_path: openai_cred,
        anthropic_cred_path: anthropic_cred,
        openai_base_url: base.clone(),
        anthropic_base_url: base.clone(),
        codex_base_url: base.clone(),
    })
    .await;

    let (status, _body) = post(
        proxy_addr,
        "/openai/v1/responses",
        r#"{"model":"gpt-4o","input":"review"}"#,
    )
    .await;
    assert_eq!(status, 200);

    let guard = captured.lock().expect("lock");
    assert_eq!(
        guard.len(),
        1,
        "mock should have received exactly one request"
    );
    let req = &guard[0];

    // Auth header
    assert_eq!(
        req.headers.get("authorization").map(|s| s.as_str()),
        Some("Bearer fake-api-key"),
        "Authorization header must be Bearer <api-key>"
    );

    // Body must contain injected `instructions`
    let body: serde_json::Value =
        serde_json::from_slice(&req.body).expect("upstream body must be JSON");
    assert!(
        body.get("instructions").is_some(),
        "instructions must be injected when absent: {body}"
    );

    // `max_output_tokens` must not be renamed (no CodexOauth)
    assert!(
        body.get("max_tokens").is_none(),
        "max_tokens must not be present for api-key credential: {body}"
    );

    // Content-Length, if present, must match the actual patched body size
    // (hyper recalculates it from the Full body after the sidecar removes
    // the stale value — so the value should be correct, not the original).
    let actual_body_len = req.body.len();
    if let Some(cl) = req.headers.get("content-length") {
        let reported: usize = cl.parse().expect("content-length must be numeric");
        assert_eq!(
            reported, actual_body_len,
            "Content-Length must match actual patched body length"
        );
    }
}

/// POST /openai/v1/responses with a CodexOauth credential.
///
/// Assert: `instructions` injected, body's token field is
/// `max_output_tokens` (regardless of whether the client sent
/// `max_tokens` or `max_output_tokens`), Authorization = Bearer
/// <access>, chatgpt-account-id header present.
///
/// v0.7.18 unified the rename: chatgpt.com/backend-api/codex/responses
/// now requires `max_output_tokens` like api.openai.com does. The old
/// inverted rename (max_output_tokens → max_tokens for CodexOauth) was
/// the regression that surfaced as `Bad Request: {"detail":"Unsupported
/// parameter: max_tokens"}`.
#[tokio::test]
async fn test_codex_oauth_patches_body() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (mock_addr, captured) = start_mock_upstream().await;
    let base = format!("http://127.0.0.1:{}", mock_addr.port());

    let openai_cred = write_codex_oauth_cred(&tmp, "openai-oauth");
    let anthropic_cred = write_string_cred(&tmp, "anthropic", "fake-anthropic-key");

    let (proxy_addr, _shutdown) = start_proxy(TestProxyConfig {
        openai_cred_path: openai_cred,
        anthropic_cred_path: anthropic_cred,
        openai_base_url: base.clone(),
        anthropic_base_url: base.clone(),
        codex_base_url: base.clone(),
    })
    .await;

    // Send `max_tokens` — the legacy name. The sidecar must rewrite to
    // `max_output_tokens` even on the codex-oauth route.
    let (status, _body) = post(
        proxy_addr,
        "/openai/v1/responses",
        r#"{"model":"gpt-5.4","input":"review","max_tokens":4096}"#,
    )
    .await;
    assert_eq!(status, 200);

    let guard = captured.lock().expect("lock");
    assert_eq!(guard.len(), 1);
    let req = &guard[0];

    // Authorization
    assert_eq!(
        req.headers.get("authorization").map(|s| s.as_str()),
        Some("Bearer fake-access-token"),
    );

    // chatgpt-account-id must be present
    assert!(
        req.headers.contains_key("chatgpt-account-id"),
        "chatgpt-account-id header must be present for CodexOauth: {:#?}",
        req.headers
    );

    // Content-Length, if present, must match the actual patched body size.
    let actual_body_len = req.body.len();
    if let Some(cl) = req.headers.get("content-length") {
        let reported: usize = cl.parse().expect("content-length must be numeric");
        assert_eq!(
            reported, actual_body_len,
            "Content-Length must match actual patched body length"
        );
    }

    // Body: instructions injected, max_tokens rewritten to max_output_tokens.
    let body: serde_json::Value =
        serde_json::from_slice(&req.body).expect("upstream body must be JSON");
    assert!(
        body.get("instructions").is_some(),
        "instructions must be injected: {body}"
    );
    assert_eq!(
        body.get("max_output_tokens").and_then(|v| v.as_u64()),
        Some(4096),
        "max_output_tokens must be 4096 after rename: {body}"
    );
    assert!(
        body.get("max_tokens").is_none(),
        "max_tokens must be removed: {body}"
    );
}

/// POST /anthropic/v1/messages with a plain Anthropic API key.
///
/// Assert: x-api-key set, anthropic-version header present, body unchanged.
#[tokio::test]
async fn test_anthropic_route() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (mock_addr, captured) = start_mock_upstream().await;
    let base = format!("http://127.0.0.1:{}", mock_addr.port());

    let openai_cred = write_string_cred(&tmp, "openai", "fake-api-key");
    let anthropic_cred = write_string_cred(&tmp, "anthropic", "fake-anthropic-key");

    let (proxy_addr, _shutdown) = start_proxy(TestProxyConfig {
        openai_cred_path: openai_cred,
        anthropic_cred_path: anthropic_cred,
        openai_base_url: base.clone(),
        anthropic_base_url: base.clone(),
        codex_base_url: base.clone(),
    })
    .await;

    let original_body = r#"{"model":"claude-opus-4-6","messages":[]}"#;
    let (status, _body) = post(proxy_addr, "/anthropic/v1/messages", original_body).await;
    assert_eq!(status, 200);

    let guard = captured.lock().expect("lock");
    assert_eq!(guard.len(), 1);
    let req = &guard[0];

    // x-api-key must be set
    assert_eq!(
        req.headers.get("x-api-key").map(|s| s.as_str()),
        Some("fake-anthropic-key"),
    );

    // anthropic-version must be present
    assert!(
        req.headers.contains_key("anthropic-version"),
        "anthropic-version header must be present: {:#?}",
        req.headers
    );

    // Body should be unchanged (no patching for /v1/messages)
    let sent: serde_json::Value = serde_json::from_slice(&req.body).expect("body must be JSON");
    let expected: serde_json::Value =
        serde_json::from_str(original_body).expect("original body must be JSON");
    assert_eq!(sent, expected, "body must be unchanged for Anthropic route");
}

/// POST to an unknown route returns 403.
#[tokio::test]
async fn test_unknown_route_returns_403() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (mock_addr, _captured) = start_mock_upstream().await;
    let base = format!("http://127.0.0.1:{}", mock_addr.port());

    let openai_cred = write_string_cred(&tmp, "openai", "fake-api-key");
    let anthropic_cred = write_string_cred(&tmp, "anthropic", "fake-anthropic-key");

    let (proxy_addr, _shutdown) = start_proxy(TestProxyConfig {
        openai_cred_path: openai_cred,
        anthropic_cred_path: anthropic_cred,
        openai_base_url: base.clone(),
        anthropic_base_url: base.clone(),
        codex_base_url: base,
    })
    .await;

    let (status, _body) = post(proxy_addr, "/unknown/route", r#"{"foo":"bar"}"#).await;
    assert_eq!(status, 403, "unknown routes must return 403");
}

/// Verify that the sidecar does not forward the stale Content-Length after
/// patching the body.
///
/// The sidecar removes the inbound Content-Length header before forwarding.
/// Hyper then recalculates it from the actual patched body size. This test
/// asserts that, if Content-Length is present in what the mock receives, it
/// matches the actual received body length — confirming the stale value was
/// removed and the correct value was computed.
#[tokio::test]
async fn test_content_length_absent_on_patched_body() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (mock_addr, captured) = start_mock_upstream().await;
    let base = format!("http://127.0.0.1:{}", mock_addr.port());

    let openai_cred = write_string_cred(&tmp, "openai", "fake-api-key");
    let anthropic_cred = write_string_cred(&tmp, "anthropic", "fake-anthropic-key");

    let (proxy_addr, _shutdown) = start_proxy(TestProxyConfig {
        openai_cred_path: openai_cred,
        anthropic_cred_path: anthropic_cred,
        openai_base_url: base.clone(),
        anthropic_base_url: base.clone(),
        codex_base_url: base,
    })
    .await;

    let original_body = r#"{"model":"gpt-4o","input":"hello"}"#;
    let original_len = original_body.len();

    // Send a request that will be patched (instructions added → body grows).
    let (status, _body) = post(proxy_addr, "/openai/v1/responses", original_body).await;
    assert_eq!(status, 200);

    let guard = captured.lock().expect("lock");
    assert_eq!(guard.len(), 1);
    let req = &guard[0];

    // The patched body must be larger than the original because `instructions`
    // was injected.
    assert!(
        req.body.len() > original_len,
        "patched body must be larger than original (instructions injected)"
    );

    // Content-Length, if present, must reflect the patched body size — NOT
    // the stale original size. If hyper omits it (chunked transfer), that is
    // also acceptable.
    if let Some(cl) = req.headers.get("content-length") {
        let reported: usize = cl.parse().expect("content-length must be numeric");
        assert_eq!(
            reported,
            req.body.len(),
            "Content-Length must match the PATCHED body length, not the original. \
             Stale Content-Length ({original_len}) was forwarded unchanged."
        );
    }
}
