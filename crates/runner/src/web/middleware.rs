use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderValue, Method, Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::response::ApiError;
use super::state::WebState;

/// Reject requests where Host header does not match the configured bind
/// address. This is a primary DNS rebinding defense for loopback services.
pub async fn host_validation_layer(
    State(state): State<Arc<WebState>>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let Some(host_header) = request
        .headers()
        .get(header::HOST)
        .and_then(header_value_to_str)
    else {
        return ApiError::with_status(
            StatusCode::FORBIDDEN,
            "invalid_request",
            "missing Host header",
        )
        .into_response();
    };

    if !state.allows_host_header(host_header) {
        return ApiError::with_status(
            StatusCode::FORBIDDEN,
            "invalid_request",
            format!(
                "Host header `{host_header}` does not match configured bind `{}`",
                state.bind_address
            ),
        )
        .into_response();
    }

    next.run(request).await
}

/// Enforce `application/json` on all API mutation methods.
pub async fn content_type_enforcement(
    _state: State<Arc<WebState>>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    if !request.uri().path().starts_with("/api/v1/") {
        return next.run(request).await;
    }

    if !matches!(
        request.method(),
        &Method::PATCH | &Method::POST | &Method::DELETE
    ) {
        return next.run(request).await;
    }

    let is_json = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(header_value_to_str)
        .map(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
        })
        .unwrap_or(false);

    if !is_json {
        return ApiError::with_status(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "invalid_content_type",
            "mutation endpoints require `Content-Type: application/json`",
        )
        .into_response();
    }

    next.run(request).await
}

/// Enforce bearer token auth for API routes when web auth mode is token.
pub async fn auth_layer(
    State(state): State<Arc<WebState>>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    if !request.uri().path().starts_with("/api/v1/") {
        return next.run(request).await;
    }

    let Some(expected_token) = state.auth_token() else {
        return next.run(request).await;
    };

    let provided_token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(header_value_to_str)
        .and_then(|auth| auth.strip_prefix("Bearer "))
        .unwrap_or_default();

    if !constant_time_eq(expected_token.as_bytes(), provided_token.as_bytes()) {
        return ApiError::with_status(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "missing or invalid bearer token",
        )
        .into_response();
    }

    next.run(request).await
}

fn header_value_to_str(value: &HeaderValue) -> Option<&str> {
    value.to_str().ok()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let max_len = left.len().max(right.len());
    let mut diff = (left.len() ^ right.len()) as u8;

    for idx in 0..max_len {
        let left_byte = left.get(idx).copied().unwrap_or(0);
        let right_byte = right.get(idx).copied().unwrap_or(0);
        diff |= left_byte ^ right_byte;
    }

    diff == 0
}

#[cfg(test)]
mod tests {
    use super::constant_time_eq;

    #[test]
    fn constant_time_eq_detects_equal_and_unequal_values() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"different"));
        assert!(!constant_time_eq(b"secret", b"secret-long"));
    }

    #[test]
    fn constant_time_eq_empty_strings_are_equal() {
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn constant_time_eq_rejects_different_lengths() {
        assert!(!constant_time_eq(b"a", b"ab"));
        assert!(!constant_time_eq(b"ab", b"a"));
    }

    #[test]
    fn constant_time_eq_rejects_prefix_match() {
        assert!(!constant_time_eq(b"token-full", b"token"));
    }
}
