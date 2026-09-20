//! Bearer-token gate shared by the HTTP adapters (MCP HTTP, A2A,
//! webhook).
//!
//! Token comparison is constant-time so a remote caller cannot
//! recover the secret byte-by-byte from response latency.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// Constant-time byte-slice equality.
///
/// Runs in time proportional to the longer input regardless of where
/// the first mismatch is; a length mismatch is folded into the
/// accumulator rather than short-circuited. `black_box` keeps the
/// optimizer from turning the loop back into an early-exit compare.
#[must_use]
pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut acc = u8::from(left.len() != right.len());
    let longest = left.len().max(right.len());
    for idx in 0..longest {
        let lhs = left.get(idx).copied().unwrap_or(0);
        let rhs = right.get(idx).copied().unwrap_or(0);
        acc = std::hint::black_box(acc | (lhs ^ rhs));
    }
    std::hint::black_box(acc) == 0
}

/// `true` when `headers` carries `Authorization: Bearer <expected>`.
#[must_use]
pub fn bearer_matches(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer ").map(str::trim))
        .is_some_and(|submitted| constant_time_eq(submitted.as_bytes(), expected.as_bytes()))
}

/// axum middleware: reject with `401` unless the request carries the
/// expected bearer token. A `None` state disables the gate.
pub async fn bearer_gate(
    State(expected): State<Option<Arc<str>>>,
    req: Request,
    next: Next,
) -> Response {
    match expected {
        None => next.run(req).await,
        Some(token) if bearer_matches(req.headers(), &token) => next.run(req).await,
        Some(_) => (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn constant_time_eq_matches_equal_and_rejects_unequal() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"s3cret", b"s3cret"));
        assert!(!constant_time_eq(b"s3cret", b"s3cref"));
        assert!(!constant_time_eq(b"s3cret", b"s3cre"));
        assert!(!constant_time_eq(b"s3cret", b"s3cret!"));
        assert!(!constant_time_eq(b"", b"x"));
    }

    #[test]
    fn bearer_matches_requires_scheme_and_exact_token() {
        let mut h = HeaderMap::new();
        assert!(!bearer_matches(&h, "tok"));
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer tok"),
        );
        assert!(bearer_matches(&h, "tok"));
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer tok2"),
        );
        assert!(!bearer_matches(&h, "tok"));
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic tok"));
        assert!(!bearer_matches(&h, "tok"));
    }
}
