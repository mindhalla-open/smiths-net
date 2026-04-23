//! Integration: REGISTER flow backed by `HttpAuthStore` + an
//! in-process `axum` mock webhook.
//!
//! Slice 2.2 (v0.34.0) acceptance:
//!
//! 1. Axum mock responds `{"status":"accept","ha1":"<hex>"}` for a
//!    seeded user. REGISTER → 200 OK, webhook saw the challenge.
//! 2. Mock responds `{"status":"deny"}` for an unknown user —
//!    REGISTER → 401.
//! 3. Mock goes 500 five times in a row — the circuit breaker trips
//!    Open; the sixth lookup short-circuits without touching the
//!    wire (`is_breaker_open()` == `true`).
//! 4. Bearer auth — the webhook asserts the `Authorization: Bearer
//!    <token>` header is present.

#![cfg(feature = "auth-http")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use serde::{Deserialize, Serialize};
use smiths_sip::auth::digest::Algorithm;
use smiths_sip::auth::http_store::{HttpAuthConfig, HttpAuthStore};
use smiths_sip::auth::{CredentialStore, Credentials};
use tokio::net::TcpListener;

#[derive(Clone, Default)]
struct MockState {
    /// Ordered verdicts to return — each request consumes one. When
    /// empty, default to `{"status":"accept","ha1":"<fixed>"}`.
    script: Arc<std::sync::Mutex<Vec<Verdict>>>,
    /// Per-request body capture for assertions.
    last_request: Arc<std::sync::Mutex<Option<IncomingRequest>>>,
    /// Expected bearer token. When set, missing / wrong bearer →
    /// HTTP 401.
    expected_bearer: Arc<std::sync::Mutex<Option<String>>>,
    /// Counts every hit regardless of verdict — the circuit-breaker
    /// test asserts against this to confirm the breaker short-
    /// circuits subsequent lookups.
    hits: Arc<AtomicUsize>,
}

#[derive(Clone, Debug)]
enum Verdict {
    Accept { ha1: String },
    Deny,
    ServerError,
}

#[derive(Clone, Debug)]
struct IncomingRequest {
    realm: String,
    username: String,
    algorithm: String,
    bearer: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AuthRequestBody {
    realm: String,
    username: String,
    algorithm: String,
}

#[derive(Debug, Serialize)]
struct AuthResponseBody {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    ha1: Option<String>,
}

async fn authenticate_handler(
    State(state): State<MockState>,
    headers: HeaderMap,
    Json(body): Json<AuthRequestBody>,
) -> (StatusCode, Json<AuthResponseBody>) {
    state.hits.fetch_add(1, Ordering::Relaxed);
    // Capture the bearer for assertions.
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_owned);

    if let Some(expected) = state.expected_bearer.lock().unwrap().clone()
        && bearer.as_deref() != Some(expected.as_str())
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(AuthResponseBody {
                status: "deny",
                ha1: None,
            }),
        );
    }

    *state.last_request.lock().unwrap() = Some(IncomingRequest {
        realm: body.realm,
        username: body.username,
        algorithm: body.algorithm,
        bearer,
    });

    let verdict = state
        .script
        .lock()
        .unwrap()
        .pop()
        .unwrap_or(Verdict::Accept {
            ha1: "939e7578ed9e3c518a452acee763bce9".into(),
        });
    match verdict {
        Verdict::Accept { ha1 } => (
            StatusCode::OK,
            Json(AuthResponseBody {
                status: "accept",
                ha1: Some(ha1),
            }),
        ),
        Verdict::Deny => (
            StatusCode::OK,
            Json(AuthResponseBody {
                status: "deny",
                ha1: None,
            }),
        ),
        Verdict::ServerError => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(AuthResponseBody {
                status: "error",
                ha1: None,
            }),
        ),
    }
}

async fn spawn_mock_webhook(state: MockState) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/authenticate", post(authenticate_handler))
        .with_state(state);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

#[tokio::test(flavor = "multi_thread")]
async fn http_store_accepts_and_denies() {
    let state = MockState::default();
    // LIFO script: first pop = first request. Reverse intent: the
    // first call accepts, the second denies.
    *state.script.lock().unwrap() = vec![
        Verdict::Deny,
        Verdict::Accept {
            ha1: "939e7578ed9e3c518a452acee763bce9".into(),
        },
    ];
    let addr = spawn_mock_webhook(state.clone()).await;

    let cfg = HttpAuthConfig {
        endpoint: format!("http://{addr}/authenticate"),
        timeout: Duration::from_secs(2),
        ..HttpAuthConfig::default()
    };
    let store = HttpAuthStore::new(cfg).unwrap();

    // Call 1 → accept → `Some(Credentials { ha1: Some(...) })`.
    let got = store.lookup("smiths.test", "alice");
    assert!(got.is_some(), "first lookup should accept");
    let creds = got.unwrap();
    assert_eq!(creds.username, "alice");
    assert_eq!(
        creds.ha1.as_deref(),
        Some("939e7578ed9e3c518a452acee763bce9")
    );
    assert!(creds.password.is_empty(), "HA1 mode: password stays empty");

    let captured = state.last_request.lock().unwrap().clone().unwrap();
    assert_eq!(captured.realm, "smiths.test");
    assert_eq!(captured.username, "alice");
    assert_eq!(captured.algorithm, "md5");

    // Call 2 → deny → `None`.
    assert!(
        store.lookup("smiths.test", "ghost").is_none(),
        "deny verdict must surface as None"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn http_store_trips_breaker_after_threshold() {
    let state = MockState::default();
    // 5 consecutive server errors → breaker trips. Pre-seed exactly
    // that many so the test is deterministic; anything after should
    // short-circuit without hitting the wire.
    *state.script.lock().unwrap() = vec![
        Verdict::ServerError,
        Verdict::ServerError,
        Verdict::ServerError,
        Verdict::ServerError,
        Verdict::ServerError,
    ];
    let addr = spawn_mock_webhook(state.clone()).await;

    let cfg = HttpAuthConfig {
        endpoint: format!("http://{addr}/authenticate"),
        timeout: Duration::from_millis(500),
        retries: 0, // one attempt per lookup so the count is predictable
        breaker_threshold: 5,
        breaker_cooldown: Duration::from_secs(30),
        ..HttpAuthConfig::default()
    };
    let store = HttpAuthStore::new(cfg).unwrap();

    for _ in 0..5 {
        let _ = store.lookup("smiths.test", "alice");
    }

    assert!(
        store.is_breaker_open(),
        "5 consecutive failures must trip the breaker"
    );
    let hits_before = state.hits.load(Ordering::Relaxed);

    // 6th lookup — short-circuits on FailClosed without touching wire.
    assert!(
        store.lookup("smiths.test", "alice").is_none(),
        "open breaker must surface None"
    );
    let hits_after = state.hits.load(Ordering::Relaxed);
    assert_eq!(
        hits_before, hits_after,
        "open breaker must short-circuit before sending HTTP"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn http_store_sends_bearer_token() {
    let state = MockState::default();
    *state.expected_bearer.lock().unwrap() = Some("s3cret-token".into());
    let addr = spawn_mock_webhook(state.clone()).await;

    let cfg = HttpAuthConfig {
        endpoint: format!("http://{addr}/authenticate"),
        timeout: Duration::from_secs(2),
        bearer_token: Some("s3cret-token".into()),
        ..HttpAuthConfig::default()
    };
    let store = HttpAuthStore::new(cfg).unwrap();
    let got = store.lookup("smiths.test", "alice");
    assert!(
        got.is_some(),
        "matching bearer must accept; mismatched yields 401"
    );
    let captured = state.last_request.lock().unwrap().clone().unwrap();
    assert_eq!(captured.bearer.as_deref(), Some("s3cret-token"));
}

#[tokio::test(flavor = "multi_thread")]
async fn http_store_rejects_missing_bearer() {
    let state = MockState::default();
    *state.expected_bearer.lock().unwrap() = Some("s3cret-token".into());
    let addr = spawn_mock_webhook(state.clone()).await;

    let cfg = HttpAuthConfig {
        endpoint: format!("http://{addr}/authenticate"),
        timeout: Duration::from_secs(2),
        bearer_token: None, // client doesn't send one; webhook rejects
        retries: 0,
        ..HttpAuthConfig::default()
    };
    let store = HttpAuthStore::new(cfg).unwrap();
    assert!(
        store.lookup("smiths.test", "alice").is_none(),
        "webhook returning 401 must surface as None"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn http_store_authenticate_api_returns_error_on_breaker_open() {
    // The async API (as opposed to the sync CredentialStore trait)
    // surfaces typed errors — operators using it directly from
    // async code get the cooldown seconds back.
    let state = MockState::default();
    *state.script.lock().unwrap() = vec![
        Verdict::ServerError,
        Verdict::ServerError,
        Verdict::ServerError,
    ];
    let addr = spawn_mock_webhook(state.clone()).await;

    let cfg = HttpAuthConfig {
        endpoint: format!("http://{addr}/authenticate"),
        timeout: Duration::from_millis(500),
        retries: 0,
        breaker_threshold: 3,
        breaker_cooldown: Duration::from_secs(30),
        ..HttpAuthConfig::default()
    };
    let store = HttpAuthStore::new(cfg).unwrap();

    for _ in 0..3 {
        let _ = store.authenticate("r", "u", Algorithm::Md5).await;
    }
    let err = store
        .authenticate("r", "u", Algorithm::Md5)
        .await
        .unwrap_err();
    match err {
        smiths_sip::auth::http_store::HttpAuthError::BreakerOpen { cooldown_secs } => {
            assert!(cooldown_secs > 0 && cooldown_secs <= 30);
        }
        other => panic!("expected BreakerOpen, got {other:?}"),
    }
    // Ensure Credentials would have carried only HA1 (no plaintext)
    // on the happy path — sanity-check the DTO shape.
    let synthetic = Credentials::from_ha1("u", "r", "DEADBEEF");
    assert!(synthetic.password.is_empty());
    assert_eq!(synthetic.ha1.as_deref(), Some("DEADBEEF"));
}
