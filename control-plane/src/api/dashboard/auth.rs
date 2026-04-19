use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};

/// Dashboard auth middleware: checks for `nautiloop_api_key` cookie OR Bearer header.
/// Unauthenticated requests to `/dashboard/*` (except `/dashboard/login` and
/// `/dashboard/static/*`) are redirected to `/dashboard/login`.
pub async fn dashboard_auth_middleware(
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let path = request.uri().path().to_string();

    // Skip auth for login page and static assets
    if path == "/dashboard/login" || path.starts_with("/dashboard/static/") {
        return Ok(next.run(request).await);
    }

    let expected_key = std::env::var("NAUTILOOP_API_KEY").map_err(|_| {
        tracing::error!("NAUTILOOP_API_KEY not set");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // Check Bearer header first (for JS fetch calls)
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
        return Ok(next.run(request).await);
    }

    // Check cookie
    let cookie_valid = extract_cookie_value(request.headers(), "nautiloop_api_key")
        .is_some_and(|key| constant_time_eq(key.as_bytes(), expected_key.as_bytes()));

    if cookie_valid {
        return Ok(next.run(request).await);
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

/// Extract a cookie value by name from the Cookie header.
pub fn extract_cookie_value<'a>(
    headers: &'a axum::http::HeaderMap,
    name: &str,
) -> Option<&'a str> {
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|cookies| {
            for cookie in cookies.split(';') {
                let cookie = cookie.trim();
                if let Some(value) = cookie.strip_prefix(name).and_then(|v| v.strip_prefix('=')) {
                    return Some(value);
                }
            }
            None
        })
}

/// Validate an API key against the expected key from the environment.
pub fn validate_api_key(key: &str) -> bool {
    let Ok(expected) = std::env::var("NAUTILOOP_API_KEY") else {
        return false;
    };
    if key.is_empty() || expected.is_empty() {
        return false;
    }
    constant_time_eq(key.as_bytes(), expected.as_bytes())
}

/// Constant-time byte comparison to prevent timing side-channel attacks.
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
            "foo=bar; nautiloop_api_key=test123; baz=qux".parse().unwrap(),
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
    }
}
