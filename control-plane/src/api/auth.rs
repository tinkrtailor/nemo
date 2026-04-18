use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;

use crate::util::constant_time_eq;

/// Auth middleware: validates API key from `Authorization: Bearer <key>` header
/// OR from the `nautiloop_api_key` cookie (FR-4b).
///
/// Validates against `NAUTILOOP_API_KEY` env var. If unset, rejects all requests.
/// Cookie is checked first, then Bearer header — matching the dashboard auth logic.
/// This allows dashboard action buttons (Approve, Cancel, Resume, Extend) to call
/// existing API endpoints via same-origin fetch() with the HttpOnly cookie.
pub async fn auth_middleware(request: Request, next: Next) -> Result<Response, StatusCode> {
    let expected_key = std::env::var("NAUTILOOP_API_KEY").map_err(|_| {
        tracing::error!("NAUTILOOP_API_KEY not set - all requests will be rejected");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // Try cookie first (dashboard JS sends HttpOnly cookie via same-origin fetch)
    let api_key = extract_cookie_value(request.headers(), "nautiloop_api_key")
        .or_else(|| extract_bearer(request.headers()));

    match api_key {
        Some(key) if constant_time_eq(key.as_bytes(), expected_key.as_bytes()) => {
            Ok(next.run(request).await)
        }
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

/// Extract a cookie value by name from the Cookie header.
fn extract_cookie_value(
    headers: &axum::http::HeaderMap,
    name: &str,
) -> Option<String> {
    headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|pair| {
                let pair = pair.trim();
                let (k, v) = pair.split_once('=')?;
                if k.trim() == name {
                    Some(v.trim().to_string())
                } else {
                    None
                }
            })
        })
}

/// Extract API key from Authorization: Bearer <key> header.
fn extract_bearer(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|header| {
            if header.len() > 7 && header[..7].eq_ignore_ascii_case("bearer ") {
                let key = &header[7..];
                if key.is_empty() {
                    None
                } else {
                    Some(key.to_string())
                }
            } else {
                None
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_cookie_value() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "cookie",
            "nautiloop_api_key=secret123; nautiloop_engineer=alice"
                .parse()
                .unwrap(),
        );
        assert_eq!(
            extract_cookie_value(&headers, "nautiloop_api_key"),
            Some("secret123".to_string())
        );
        assert_eq!(extract_cookie_value(&headers, "missing"), None);
    }

    #[test]
    fn test_extract_bearer() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("authorization", "Bearer mykey".parse().unwrap());
        assert_eq!(extract_bearer(&headers), Some("mykey".to_string()));

        let mut headers = axum::http::HeaderMap::new();
        headers.insert("authorization", "Bearer ".parse().unwrap());
        assert_eq!(extract_bearer(&headers), None);
    }
}
