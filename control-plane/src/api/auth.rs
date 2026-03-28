use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;

/// Auth middleware: validates API key from `Authorization: Bearer <key>` header.
///
/// Validates against `NEMO_API_KEY` env var. If unset, rejects all requests.
/// In V1, all authenticated users have full access (FR-14).
/// mTLS is handled at the ingress/load-balancer level, not in application code.
pub async fn auth_middleware(request: Request, next: Next) -> Result<Response, StatusCode> {
    let expected_key = std::env::var("NEMO_API_KEY").map_err(|_| {
        tracing::error!("NEMO_API_KEY not set - all requests will be rejected");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok());

    match auth_header {
        Some(header) if header.starts_with("Bearer ") => {
            let api_key = &header[7..];
            if api_key.is_empty() {
                return Err(StatusCode::UNAUTHORIZED);
            }
            // Constant-time comparison to prevent timing attacks
            if constant_time_eq(api_key.as_bytes(), expected_key.as_bytes()) {
                Ok(next.run(request).await)
            } else {
                Err(StatusCode::UNAUTHORIZED)
            }
        }
        _ => Err(StatusCode::UNAUTHORIZED),
    }
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

    #[test]
    fn test_constant_time_eq() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }
}
