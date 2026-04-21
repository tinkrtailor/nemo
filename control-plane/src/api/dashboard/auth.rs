use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};

/// Extension type carrying the API key for dashboard auth.
/// Injected as a tower Extension layer so the middleware can access it
/// without requiring `State` extraction (which needs `from_fn_with_state`).
#[derive(Clone)]
pub struct DashboardApiKey(pub String);

/// Extension type carrying the CSRF token for the current request.
/// Set by the auth middleware on authenticated HTML requests; handlers
/// extract it and pass it to render functions for form embedding.
#[derive(Clone)]
pub struct CsrfToken(pub String);

/// Extension type carrying the engineer name from the `nautiloop_engineer` cookie.
/// Used by the 'Mine' filter (FR-3e) to scope loops to the logged-in engineer.
#[derive(Clone)]
pub struct EngineerName(pub String);

/// Dashboard auth middleware: checks for `nautiloop_api_key` cookie OR Bearer header.
/// Unauthenticated requests to `/dashboard/*` (except `/dashboard/login` and
/// `/dashboard/static/*`) are redirected to `/dashboard/login`.
///
/// Reads the expected API key from `AppState.api_key` via request extensions.
pub async fn dashboard_auth_middleware(
    mut request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let path = request.uri().path().to_string();

    // Skip auth for login page and static assets
    if path == "/dashboard/login" || path.starts_with("/dashboard/static/") {
        return Ok(next.run(request).await);
    }

    // Extract expected key from DashboardApiKey extension (set by the router layer)
    // or fall back to NAUTILOOP_API_KEY env var for backwards compatibility.
    let expected_key = request
        .extensions()
        .get::<DashboardApiKey>()
        .map(|k| k.0.clone())
        .or_else(|| std::env::var("NAUTILOOP_API_KEY").ok())
        .ok_or_else(|| {
            tracing::error!("NAUTILOOP_API_KEY not configured");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // Check Bearer header first (for API calls)
    let bearer_valid = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|h| {
            if h.len() > 7 && h[..7].eq_ignore_ascii_case("bearer ") {
                Some(&h[7..])
            } else {
                None
            }
        })
        .is_some_and(|key| constant_time_eq(key.as_bytes(), expected_key.as_bytes()));

    if bearer_valid {
        let csrf_token = generate_csrf_token();
        // Extract engineer name before taking mutable borrow on extensions
        let eng_name =
            extract_cookie_value(request.headers(), "nautiloop_engineer").map(|s| s.to_string());
        request
            .extensions_mut()
            .insert(CsrfToken(csrf_token.clone()));
        if let Some(eng) = eng_name {
            request.extensions_mut().insert(EngineerName(eng));
        }
        let mut response = next.run(request).await;
        set_csrf_cookie(&mut response, &csrf_token);
        return Ok(response);
    }

    // Check cookie
    let cookie_valid = extract_cookie_value(request.headers(), "nautiloop_api_key")
        .is_some_and(|key| constant_time_eq(key.as_bytes(), expected_key.as_bytes()));

    if cookie_valid {
        let csrf_token = generate_csrf_token();
        let eng_name =
            extract_cookie_value(request.headers(), "nautiloop_engineer").map(|s| s.to_string());
        request
            .extensions_mut()
            .insert(CsrfToken(csrf_token.clone()));
        if let Some(eng) = eng_name {
            request.extensions_mut().insert(EngineerName(eng));
        }
        let mut response = next.run(request).await;
        set_csrf_cookie(&mut response, &csrf_token);
        return Ok(response);
    }

    // Redirect to login for HTML requests, 401 for API/JSON requests
    let accepts_html = request
        .headers()
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/html"));

    if accepts_html {
        Ok(Redirect::to("/dashboard/login").into_response())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

/// Set the CSRF cookie on a response. Uses a dedicated helper that avoids
/// accumulating multiple `nautiloop_csrf` Set-Cookie headers across the
/// middleware chain. The cookie uses `Path=/dashboard` consistently with
/// the login page's CSRF cookie to prevent path-scoped conflicts.
fn set_csrf_cookie(response: &mut Response, csrf_token: &str) {
    let csrf_cookie = format!(
        "nautiloop_csrf={}; HttpOnly; SameSite=Strict; Path=/dashboard; Max-Age=604800",
        csrf_token
    );
    if let Ok(val) = csrf_cookie.parse() {
        // Remove any existing nautiloop_csrf Set-Cookie headers to prevent
        // accumulation, then append the new one. We can't use `insert` because
        // that would clobber other Set-Cookie headers (e.g., nautiloop_api_key).
        let headers = response.headers_mut();
        let other_cookies: Vec<_> = headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter(|v| {
                v.to_str()
                    .map(|s| !s.starts_with("nautiloop_csrf="))
                    .unwrap_or(true)
            })
            .cloned()
            .collect();
        headers.remove(header::SET_COOKIE);
        for cookie in other_cookies {
            headers.append(header::SET_COOKIE, cookie);
        }
        headers.append(header::SET_COOKIE, val);
    }
}

/// Extract a cookie value by name from the Cookie header.
/// Returns the **last** match when multiple cookies share the same name,
/// which ensures we pick up the most recently set value (important for
/// the CSRF token cookie that may accumulate across requests).
pub fn extract_cookie_value<'a>(headers: &'a axum::http::HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|cookies| {
            let mut last_match = None;
            for cookie in cookies.split(';') {
                let cookie = cookie.trim();
                if let Some(value) = cookie.strip_prefix(name).and_then(|v| v.strip_prefix('=')) {
                    last_match = Some(value);
                }
            }
            last_match
        })
}

/// Generate a CSRF token using a random UUID (hex-encoded).
pub fn generate_csrf_token() -> String {
    uuid::Uuid::new_v4().to_string().replace('-', "")
}

/// Validate a CSRF token from the form matches the one in the cookie.
/// Uses constant-time comparison.
pub fn validate_csrf_token(form_token: &str, cookie_token: &str) -> bool {
    if form_token.is_empty() || cookie_token.is_empty() {
        return false;
    }
    constant_time_eq(form_token.as_bytes(), cookie_token.as_bytes())
}

/// Validate an API key against the expected key. Accepts the expected key directly
/// rather than reading from the environment.
pub fn validate_api_key_against(key: &str, expected: &str) -> bool {
    if key.is_empty() || expected.is_empty() {
        return false;
    }
    constant_time_eq(key.as_bytes(), expected.as_bytes())
}

/// Constant-time byte comparison to prevent timing side-channel attacks.
/// Uses direct byte XOR accumulation, consistent with the main API auth.
/// Length mismatch returns false — the minor timing leak on length is
/// negligible given the deployment model (Tailscale + shared key of known length).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    #[test]
    fn test_extract_cookie_value() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            "foo=bar; nautiloop_api_key=test123; baz=qux"
                .parse()
                .unwrap(),
        );
        assert_eq!(
            extract_cookie_value(&headers, "nautiloop_api_key"),
            Some("test123")
        );
        assert_eq!(extract_cookie_value(&headers, "foo"), Some("bar"));
        assert_eq!(extract_cookie_value(&headers, "missing"), None);
    }

    #[test]
    fn test_extract_cookie_empty() {
        let headers = HeaderMap::new();
        assert_eq!(extract_cookie_value(&headers, "nautiloop_api_key"), None);
    }

    #[test]
    fn test_constant_time_eq() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"short", b"longer-string"));
    }

    #[test]
    fn test_generate_csrf_token() {
        let token1 = generate_csrf_token();
        let token2 = generate_csrf_token();
        assert_eq!(token1.len(), 32); // UUID without dashes
        assert_ne!(token1, token2); // Each token is unique
        assert!(token1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_validate_csrf_token() {
        assert!(validate_csrf_token("abc123", "abc123"));
        assert!(!validate_csrf_token("abc123", "wrong"));
        assert!(!validate_csrf_token("", "abc123"));
        assert!(!validate_csrf_token("abc123", ""));
    }

    #[test]
    fn test_validate_api_key_against() {
        assert!(validate_api_key_against("test-key", "test-key"));
        assert!(!validate_api_key_against("wrong", "test-key"));
        assert!(!validate_api_key_against("", "test-key"));
        assert!(!validate_api_key_against("test-key", ""));
    }
}
