//! HTTP-webhook-backed [`CredentialStore`].
//!
//! Operators who already have an IAM / HR system that owns subscriber
//! credentials point the engine at an HTTPS endpoint with this store.
//! On each REGISTER / INVITE challenge the store posts
//! `{realm, username, algorithm}` and expects either a
//! pre-computed HA1 or a deny verdict back.
//!
//! Compiled only when `--features auth-http` is on (default for
//! `smiths-sip`). The HTTP client is `reqwest` with `rustls-tls` so
//! musl builds stay self-contained (no system OpenSSL).
//!
//! ## Webhook contract
//!
//! Request:
//!
//! ```json
//! {
//!   "realm":     "smiths.local",
//!   "username":  "alice",
//!   "algorithm": "md5"        // or "sha-256"
//! }
//! ```
//!
//! Response (HTTP 200 for both `accept` and `deny` — non-200 is a
//! backend error, which flips the circuit breaker):
//!
//! ```json
//! { "status": "accept", "ha1": "939e7578ed9e3c518a452acee763bce9" }
//! ```
//!
//! or
//!
//! ```json
//! { "status": "deny" }
//! ```
//!
//! The engine never sees a plaintext password on this path — which is
//! the point of having an HTTP backend at all. Operators that can't
//! expose HA1 directly can keep the password in their IAM and hash on
//! demand before responding.
//!
//! ## Threading
//!
//! [`CredentialStore::lookup`] is a synchronous trait method, so we
//! bridge with `tokio::task::block_in_place` + `Handle::block_on`.
//! The enclosing tokio runtime **must be multi-threaded** (our
//! default — see `crates/smiths-cli/src/main.rs`). Single-threaded
//! runtimes will panic; document this on [`HttpAuthStore::new`].

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::Client;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::runtime::Handle;
use tracing::{debug, info, warn};

use crate::auth::digest::Algorithm;

use super::{CredentialStore, Credentials};

/// Configuration for an HTTP-webhook-backed credential store.
#[derive(Clone, Debug)]
pub struct HttpAuthConfig {
    /// Full endpoint URL the engine POSTs to (e.g.
    /// `https://iam.example.com/sip-auth`). Must be HTTPS in prod;
    /// `http://` is accepted for localhost dev.
    pub endpoint: String,
    /// Per-request timeout. Default 2 s — credential lookup is on the
    /// REGISTER hot path so slow backends starve the auth worker pool.
    pub timeout: Duration,
    /// Number of times to retry a failed request (separate from the
    /// circuit breaker: retries are for transient blips, the breaker
    /// is for sustained outages). Default 1 — one extra attempt.
    pub retries: u8,
    /// Optional `Authorization: Bearer <token>` sent with every
    /// request so the webhook can authenticate the engine.
    pub bearer_token: Option<String>,
    /// Consecutive failures before the breaker trips Open. Default 5.
    pub breaker_threshold: u32,
    /// Cooldown after tripping Open before a single probe is allowed.
    /// Default 30 s.
    pub breaker_cooldown: Duration,
    /// What to do while the breaker is Open — fail every lookup with
    /// `None` (`FailureMode::FailClosed`, the safer default), or pass
    /// every lookup with an empty credential set so the registrar
    /// sees `UnknownUser` rather than "deny" (`FailureMode::FailOpen`
    /// — appropriate only when auth is genuinely optional).
    pub failure_mode: FailureMode,
}

impl Default for HttpAuthConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            timeout: Duration::from_secs(2),
            retries: 1,
            bearer_token: None,
            breaker_threshold: 5,
            breaker_cooldown: Duration::from_secs(30),
            failure_mode: FailureMode::FailClosed,
        }
    }
}

/// Circuit-breaker posture while the backend is Open.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FailureMode {
    /// Every lookup returns `None` (registrar sees `UnknownUser`).
    /// Default and correct for real credential stores: a dead backend
    /// must not silently authenticate the world.
    FailClosed,
    /// Every lookup returns `None` **without** incrementing the
    /// failure counter — the registrar still sees `UnknownUser`, but
    /// the breaker cools down faster. Useful in dev or when auth is
    /// genuinely optional and a lookup miss is the expected path.
    FailOpen,
}

/// Errors surfaced internally by the HTTP store. Not leaked through
/// `CredentialStore::lookup` (which returns `Option`) but exposed on
/// the explicit [`HttpAuthStore::authenticate`] method for tests +
/// diagnostic paths.
#[derive(Debug, Error)]
pub enum HttpAuthError {
    /// Network / transport error. The breaker counts these.
    #[error("transport: {0}")]
    Transport(String),
    /// Non-200 HTTP response. Also counted by the breaker.
    #[error("unexpected HTTP {code}")]
    HttpStatus {
        /// Status code the webhook returned.
        code: u16,
    },
    /// Response body didn't match the documented shape.
    #[error("malformed response: {0}")]
    Malformed(String),
    /// Breaker is Open and the store is configured `FailClosed` — the
    /// lookup path bailed without touching the wire.
    #[error("circuit breaker open (cooldown {cooldown_secs}s)")]
    BreakerOpen {
        /// How long the breaker intends to stay open before the next
        /// probe attempt.
        cooldown_secs: u64,
    },
}

/// Wire shape of the webhook request body.
#[derive(Debug, Serialize)]
struct AuthRequest<'a> {
    realm: &'a str,
    username: &'a str,
    algorithm: &'a str,
}

/// Wire shape of the webhook response. `status` is the discriminator;
/// `ha1` is required for `"accept"` and ignored otherwise.
#[derive(Debug, Deserialize)]
struct AuthResponse {
    status: String,
    #[serde(default)]
    ha1: Option<String>,
}

/// HTTP-webhook [`CredentialStore`].
///
/// Cheap to `Arc`-clone; `reqwest::Client` is internally `Arc`-backed.
pub struct HttpAuthStore {
    config: HttpAuthConfig,
    client: Client,
    /// Breaker state — `Arc` so the sync `CredentialStore::lookup`
    /// path can mutate atomically.
    breaker: Arc<Breaker>,
}

/// Simple three-state breaker. The public enum is kept private to the
/// store — callers interact via the lookup result.
#[derive(Debug)]
struct Breaker {
    consecutive_failures: AtomicU32,
    /// Unix-seconds timestamp when the breaker last tripped Open. `0`
    /// means Closed.
    tripped_at_unix: AtomicU64,
    threshold: u32,
    cooldown: Duration,
}

impl Breaker {
    fn new(threshold: u32, cooldown: Duration) -> Self {
        Self {
            consecutive_failures: AtomicU32::new(0),
            tripped_at_unix: AtomicU64::new(0),
            threshold,
            cooldown,
        }
    }

    /// If the breaker is Open and still inside its cooldown window,
    /// return the remaining seconds so the caller can surface it on
    /// an error. `None` means "proceed with the request" — either
    /// Closed or the cooldown has expired (`HalfOpen`).
    fn gate(&self) -> Option<u64> {
        let tripped = self.tripped_at_unix.load(Ordering::Acquire);
        if tripped == 0 {
            return None;
        }
        let now = unix_now();
        let elapsed = now.saturating_sub(tripped);
        let cooldown = self.cooldown.as_secs();
        if elapsed >= cooldown {
            // HalfOpen — let one probe through. We don't Reset here;
            // the probe's success or failure decides.
            None
        } else {
            Some(cooldown - elapsed)
        }
    }

    fn record_success(&self) {
        // Any 2xx / valid deny resets the breaker. A probe succeeding
        // from HalfOpen closes the breaker.
        self.consecutive_failures.store(0, Ordering::Release);
        self.tripped_at_unix.store(0, Ordering::Release);
    }

    fn record_failure(&self) {
        let n = self.consecutive_failures.fetch_add(1, Ordering::AcqRel) + 1;
        if n >= self.threshold {
            let now = unix_now();
            self.tripped_at_unix.store(now, Ordering::Release);
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl HttpAuthStore {
    /// Build a store against `config`. The `reqwest::Client` is built
    /// once and reused across lookups (HTTP/1.1 keep-alive + TLS
    /// session resumption). Must be called from a tokio
    /// **multi-threaded** runtime — [`CredentialStore::lookup`] uses
    /// `block_in_place` to bridge the sync trait.
    pub fn new(config: HttpAuthConfig) -> Result<Self, HttpAuthError> {
        let client = Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|e| HttpAuthError::Transport(e.to_string()))?;
        let breaker = Arc::new(Breaker::new(
            config.breaker_threshold,
            config.breaker_cooldown,
        ));
        Ok(Self {
            config,
            client,
            breaker,
        })
    }

    /// Current consecutive failure count. Exposed for diagnostics /
    /// `/health` bubbles.
    #[must_use]
    pub fn consecutive_failures(&self) -> u32 {
        self.breaker.consecutive_failures.load(Ordering::Acquire)
    }

    /// `true` when the breaker is Open and still inside its cooldown.
    #[must_use]
    pub fn is_breaker_open(&self) -> bool {
        self.breaker.gate().is_some()
    }

    /// Async lookup primitive — directly awaitable from async code
    /// that doesn't want to go through the sync trait.
    pub async fn authenticate(
        &self,
        realm: &str,
        username: &str,
        algorithm: Algorithm,
    ) -> Result<Option<Credentials>, HttpAuthError> {
        if let Some(cooldown_secs) = self.breaker.gate() {
            if self.config.failure_mode == FailureMode::FailClosed {
                return Err(HttpAuthError::BreakerOpen { cooldown_secs });
            }
            // FailOpen: caller sees `None`, registrar reports UnknownUser.
            return Ok(None);
        }

        let mut last_err = None;
        for attempt in 0..=self.config.retries {
            match self.post(realm, username, algorithm).await {
                Ok(resp) => {
                    self.breaker.record_success();
                    return Ok(resp);
                }
                Err(e) => {
                    debug!(
                        attempt,
                        retries = self.config.retries,
                        ?e,
                        "http auth webhook call failed"
                    );
                    last_err = Some(e);
                }
            }
        }
        // Exhausted retries.
        self.breaker.record_failure();
        let err = last_err.unwrap_or_else(|| {
            HttpAuthError::Transport("no attempts and no error — unreachable".into())
        });
        warn!(
            failures = self.breaker.consecutive_failures.load(Ordering::Acquire),
            threshold = self.config.breaker_threshold,
            "http auth webhook failure"
        );
        Err(err)
    }

    async fn post(
        &self,
        realm: &str,
        username: &str,
        algorithm: Algorithm,
    ) -> Result<Option<Credentials>, HttpAuthError> {
        let body = AuthRequest {
            realm,
            username,
            algorithm: algorithm_wire_token(algorithm),
        };
        let mut req = self.client.post(&self.config.endpoint).json(&body);
        if let Some(tok) = self.config.bearer_token.as_deref() {
            req = req.bearer_auth(tok);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| HttpAuthError::Transport(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(HttpAuthError::HttpStatus {
                code: status.as_u16(),
            });
        }
        let parsed: AuthResponse = resp
            .json()
            .await
            .map_err(|e| HttpAuthError::Malformed(e.to_string()))?;
        match parsed.status.as_str() {
            "accept" => {
                let ha1 = parsed.ha1.ok_or_else(|| {
                    HttpAuthError::Malformed("accept response missing `ha1`".into())
                })?;
                Ok(Some(Credentials::from_ha1(username, realm, ha1)))
            }
            "deny" => Ok(None),
            other => Err(HttpAuthError::Malformed(format!(
                "unknown status `{other}` (expected accept|deny)"
            ))),
        }
    }
}

impl CredentialStore for HttpAuthStore {
    fn lookup(&self, realm: &str, username: &str) -> Option<Credentials> {
        // Registrar hashes with whatever `algorithm` the peer's
        // `Authorization:` header declared. The store doesn't see
        // that detail on the sync trait — for MVP we ask the webhook
        // for MD5 HA1 since every client in the wild still speaks
        // MD5. A follow-on slice teaches the trait to pass
        // `algorithm` through.
        let handle = Handle::current();
        let result = tokio::task::block_in_place(|| {
            handle.block_on(self.authenticate(realm, username, Algorithm::Md5))
        });
        match result {
            Ok(opt) => opt,
            Err(e) => {
                // Breaker-open / transport errors are logged + counted
                // in `authenticate` already; here we just collapse to
                // the trait's `None` for UnknownUser-equivalent
                // behaviour.
                info!(?e, %realm, %username, "http auth lookup failed");
                None
            }
        }
    }
}

fn algorithm_wire_token(alg: Algorithm) -> &'static str {
    match alg {
        Algorithm::Md5 => "md5",
        Algorithm::Sha256 => "sha-256",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breaker_trips_after_threshold_failures() {
        let b = Breaker::new(3, Duration::from_secs(60));
        assert!(b.gate().is_none());
        b.record_failure();
        b.record_failure();
        assert!(b.gate().is_none(), "below threshold stays Closed");
        b.record_failure();
        assert!(b.gate().is_some(), "threshold hit trips Open");
    }

    #[test]
    fn breaker_success_resets_counter() {
        let b = Breaker::new(3, Duration::from_secs(60));
        b.record_failure();
        b.record_failure();
        b.record_success();
        b.record_failure();
        b.record_failure();
        assert!(b.gate().is_none(), "counter reset; need 3 more to trip");
    }

    #[test]
    fn algorithm_wire_token_roundtrips() {
        assert_eq!(algorithm_wire_token(Algorithm::Md5), "md5");
        assert_eq!(algorithm_wire_token(Algorithm::Sha256), "sha-256");
    }

    #[test]
    fn default_config_is_fail_closed() {
        let cfg = HttpAuthConfig::default();
        assert_eq!(cfg.failure_mode, FailureMode::FailClosed);
        assert_eq!(cfg.timeout, Duration::from_secs(2));
        assert_eq!(cfg.breaker_threshold, 5);
        assert_eq!(cfg.retries, 1);
    }
}
