use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};

use crate::util::constant_time_eq;

/// Dashboard auth middleware: accepts cookie OR Bearer header.
/// On failure, redirects to `/dashboard/login` for browser requests,
/// returns 401 for API requests (Accept: application/json).
pub async fn dashboard_auth_middleware(
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let expected_key = std::env::var("NAUTILOOP_API_KEY").map_err(|_| {
        tracing::error!("NAUTILOOP_API_KEY not set");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // Extract API key from cookie first, then Bearer header
    let api_key = extract_cookie_value(request.headers(), "nautiloop_api_key")
        .or_else(|| extract_bearer(request.headers()));

    match api_key {
        Some(key) if constant_time_eq(key.as_bytes(), expected_key.as_bytes()) => {
            Ok(next.run(request).await)
        }
        _ => {
            // Check if this is a JSON API request
            let accepts_json = request
                .headers()
                .get("accept")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.contains("application/json"));

            if accepts_json {
                Err(StatusCode::UNAUTHORIZED)
            } else {
                Ok(Redirect::to("/dashboard/login").into_response())
            }
        }
    }
}

/// Extract a cookie value by name from the Cookie header.
pub fn extract_cookie_value(
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
    use axum::http::HeaderMap;

    #[test]
    fn test_extract_cookie_value() {
        let mut headers = HeaderMap::new();
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
        assert_eq!(
            extract_cookie_value(&headers, "nautiloop_engineer"),
            Some("alice".to_string())
        );
        assert_eq!(extract_cookie_value(&headers, "missing"), None);
    }

    #[test]
    fn test_extract_cookie_single() {
        let mut headers = HeaderMap::new();
        headers.insert("cookie", "nautiloop_api_key=abc".parse().unwrap());
        assert_eq!(
            extract_cookie_value(&headers, "nautiloop_api_key"),
            Some("abc".to_string())
        );
    }

    #[test]
    fn test_extract_bearer() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer mykey".parse().unwrap());
        assert_eq!(extract_bearer(&headers), Some("mykey".to_string()));
    }

    #[test]
    fn test_extract_bearer_empty() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer ".parse().unwrap());
        assert_eq!(extract_bearer(&headers), None);
    }
}
