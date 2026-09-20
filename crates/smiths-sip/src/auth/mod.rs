//! Digest authentication.
//!
//! * **`CredentialStore`** trait + `InMemoryCredentialStore` default —
//!   pluggable subscriber DBs (`SQLite` / HTTP webhook / sidecar
//!   plugin) without engine changes. Stores answer both the
//!   synchronous [`CredentialStore::lookup_for`] and, when they have a
//!   native async client, [`CredentialStore::lookup_async`].
//! * **`digest`** module — RFC 2617 / RFC 7616 / RFC 8760 MD5 and
//!   SHA-256 digest computation, plus a stateful [`digest::Registrar`]
//!   that issues CSPRNG nonces, tracks nonce-count per nonce, parses
//!   `Authorization:` headers, and verifies responses against the
//!   credential store (sync or async).
//! * **`RegistrationStore`** trait — persists contact bindings learned
//!   from successful REGISTER requests. Separate from `CredentialStore`
//!   because the two lifetimes differ (credentials are long-lived
//!   subscriber state; bindings are short-lived contact → expiry
//!   entries). A single backend can impl both.
//! * **[`sqlite_store`] module** (behind the `auth-sqlite` feature) —
//!   `SqliteAuthStore` impl of both traits backed by an embedded
//!   `SQLite` database, with an idempotent migration runner.
//! * **[`http_store`] module** (behind the `auth-http` feature) —
//!   `HttpAuthStore`, a webhook-backed `CredentialStore`.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use self::digest::Algorithm;

/// A set of credentials for one account.
///
/// Two shapes are supported:
///
/// - **Plaintext**: `password` is populated, `ha1` is `None`. The
///   registrar hashes on every call. This is what the in-memory store
///   returns.
/// - **Pre-computed HA1**: `ha1` is `Some`, `password` is typically
///   empty. The HTTP webhook and `SQLite` backends use this shape so
///   plaintext passwords never sit in a database or cross the wire
///   between an IAM service and the engine. The hash must be for the
///   algorithm the caller asked for via
///   [`CredentialStore::lookup_for`] (MD5 for the plain
///   [`CredentialStore::lookup`]).
///
/// The registrar uses `ha1` when present and falls back to hashing
/// `password` otherwise. Backends that can populate both (e.g. a
/// cache in front of a plaintext DB) are free to do so; `ha1` wins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credentials {
    /// SIP username (typically the URI's user part).
    pub username: String,
    /// Authentication realm.
    pub realm: String,
    /// Plaintext password. Ignored when `ha1` is `Some`.
    pub password: String,
    /// Pre-computed HA1 hash (RFC 2617 / RFC 8760). Hex-encoded, for
    /// the algorithm the lookup was made with.
    #[doc(alias = "H(A1)")]
    pub ha1: Option<String>,
}

impl Credentials {
    /// Plaintext constructor — the common case for static seeding.
    #[must_use]
    pub fn new(
        username: impl Into<String>,
        realm: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        Self {
            username: username.into(),
            realm: realm.into(),
            password: password.into(),
            ha1: None,
        }
    }

    /// HA1-only constructor for backends that don't surface plaintext
    /// passwords (IAM webhooks, LDAP bindings, HSM-backed stores).
    /// `ha1` must be the hex-encoded digest computed with the same
    /// algorithm the registrar will use for the request.
    #[must_use]
    pub fn from_ha1(
        username: impl Into<String>,
        realm: impl Into<String>,
        ha1: impl Into<String>,
    ) -> Self {
        Self {
            username: username.into(),
            realm: realm.into(),
            password: String::new(),
            ha1: Some(ha1.into()),
        }
    }
}

/// Boxed future returned by [`CredentialStore::lookup_async`].
pub type CredentialFuture<'a> = Pin<Box<dyn Future<Output = Option<Credentials>> + Send + 'a>>;

/// Lookup interface the SIP auth path uses to resolve credentials.
///
/// Implementations must be `Send + Sync + 'static` because the lookup
/// happens from multiple transport tasks.
///
/// Only [`Self::lookup`] is required. Backends that keep per-algorithm
/// HA1 hashes override [`Self::lookup_for`] so a SHA-256 challenge
/// gets a SHA-256 hash back; backends with a native async client
/// (HTTP) override [`Self::lookup_async`] so
/// [`digest::Registrar::authenticate_async`] never blocks a runtime
/// worker.
pub trait CredentialStore: Send + Sync + 'static {
    /// Return credentials for `(realm, username)` if known. A store
    /// that holds pre-computed hashes returns the MD5 HA1 here.
    fn lookup(&self, realm: &str, username: &str) -> Option<Credentials>;

    /// Algorithm-aware lookup. Plaintext stores ignore `algorithm`
    /// (the registrar hashes on demand); HA1 stores return the hash
    /// for exactly this algorithm. Defaults to [`Self::lookup`].
    fn lookup_for(&self, realm: &str, username: &str, algorithm: Algorithm) -> Option<Credentials> {
        let _ = algorithm;
        self.lookup(realm, username)
    }

    /// Non-blocking lookup for backends with a native async client.
    /// Returns `None` when the backend has no async path — the
    /// registrar then runs [`Self::lookup_for`] on tokio's blocking
    /// pool so a slow disk query never stalls a worker thread.
    fn lookup_async<'a>(
        &'a self,
        realm: &'a str,
        username: &'a str,
        algorithm: Algorithm,
    ) -> Option<CredentialFuture<'a>> {
        let _ = (realm, username, algorithm);
        None
    }
}

/// Acquire a read guard, recovering from poisoning. The in-memory
/// tables below hold plain `Clone` data with no invariants spanning
/// multiple writes, so a guard left behind by a panicking writer is
/// still a consistent map.
fn read_lock<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

/// Write-side counterpart of [`read_lock`].
fn write_lock<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

/// Simple in-memory store for development and tests.
#[derive(Default)]
pub struct InMemoryCredentialStore {
    // Keyed by (realm, username).
    entries: RwLock<HashMap<(String, String), Credentials>>,
}

impl InMemoryCredentialStore {
    /// Build an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace an account.
    pub fn insert(&self, creds: Credentials) {
        let key = (creds.realm.clone(), creds.username.clone());
        write_lock(&self.entries).insert(key, creds);
    }

    /// Remove an account; returns the previous value if it existed.
    pub fn remove(&self, realm: &str, username: &str) -> Option<Credentials> {
        write_lock(&self.entries).remove(&(realm.to_owned(), username.to_owned()))
    }

    /// Number of stored accounts.
    #[must_use]
    pub fn len(&self) -> usize {
        read_lock(&self.entries).len()
    }

    /// `true` if no accounts are stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl CredentialStore for InMemoryCredentialStore {
    fn lookup(&self, realm: &str, username: &str) -> Option<Credentials> {
        read_lock(&self.entries)
            .get(&(realm.to_owned(), username.to_owned()))
            .cloned()
    }
}

/// A registered contact binding: the address-of-record (AOR), the
/// contact URI the UA wants inbound calls routed to, and the wall-
/// clock timestamp at which the binding expires.
///
/// Stored by any implementor of [`RegistrationStore`]; returned to
/// MCP callers via the `sip://registrations` resource.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    /// Full address-of-record. Canonical form: `sip:user@realm` — the
    /// registrar normalizes before calling into the store.
    pub aor: String,
    /// Contact URI the UA registered. Opaque — the engine doesn't
    /// parse it beyond what it needs to route.
    pub contact: String,
    /// Unix-seconds deadline after which the binding is stale and
    /// eligible for GC. `0` is treated as "explicit unregister" per
    /// RFC 3261 §10.3.
    pub expires_at_unix: i64,
}

/// Errors surfaced by [`RegistrationStore`] implementations.
///
/// Generic over backend: the `SQLite` store wraps `rusqlite::Error`, a
/// future HTTP store would wrap an HTTP error, etc.
#[derive(Debug, thiserror::Error)]
pub enum RegistrationError {
    /// Underlying backend rejected the operation.
    #[error("registration store backend: {0}")]
    Backend(String),
    /// The AOR was not known to the store. Non-fatal on `lookup` —
    /// callers treat it as "no bindings" — and surfaced verbatim on
    /// `unbind` so callers can decide whether a missing AOR is an
    /// error or an idempotent no-op.
    #[error("unknown AOR: {0}")]
    UnknownAor(String),
}

/// Persistence surface for registered contact bindings.
///
/// Separate from [`CredentialStore`] because the two have different
/// lifetimes: credentials change when an operator provisions a new
/// subscriber; bindings change whenever a UA re-registers. A single
/// backend (the `SQLite` store, a sidecar plugin, etc.) can impl both.
///
/// Implementors must be `Send + Sync + 'static` because the
/// registrar runs under tokio's multi-thread scheduler.
pub trait RegistrationStore: Send + Sync + 'static {
    /// Upsert a binding. An existing `(aor, contact)` pair has its
    /// expiry refreshed; a new pair lands as a fresh row. Returns the
    /// canonical stored form so callers don't need a separate lookup
    /// to echo it back in the REGISTER 200 OK.
    fn bind(&self, binding: &Binding) -> Result<Binding, RegistrationError>;

    /// Remove a specific `(aor, contact)` binding. Idempotent —
    /// absent bindings surface as `Err(UnknownAor)` so the caller can
    /// decide whether that's a problem for its flow.
    fn unbind(&self, aor: &str, contact: &str) -> Result<(), RegistrationError>;

    /// Return every live binding for `aor`. An AOR with no bindings
    /// (or an unknown AOR) returns `Ok(Vec::new)` — lookups don't
    /// raise for missing entries.
    fn lookup_bindings(&self, aor: &str) -> Result<Vec<Binding>, RegistrationError>;

    /// Full snapshot: every live binding in the store. Used by the
    /// `sip://registrations` MCP resource. Implementors are expected
    /// to filter expired rows before returning (or schedule a GC so
    /// the caller sees only live entries).
    fn snapshot(&self) -> Result<Vec<Binding>, RegistrationError>;
}

/// In-memory [`RegistrationStore`] mirror of [`InMemoryCredentialStore`].
/// Kept tiny — production deployments use the `SQLite` backend; this is
/// for tests and dev setups that don't want to touch the filesystem.
#[derive(Default)]
pub struct InMemoryRegistrationStore {
    // Keyed by (aor, contact) so same-AOR re-registrations upsert cleanly.
    entries: RwLock<HashMap<(String, String), Binding>>,
}

impl InMemoryRegistrationStore {
    /// Build an empty in-memory registration store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl RegistrationStore for InMemoryRegistrationStore {
    fn bind(&self, binding: &Binding) -> Result<Binding, RegistrationError> {
        let key = (binding.aor.clone(), binding.contact.clone());
        write_lock(&self.entries).insert(key, binding.clone());
        Ok(binding.clone())
    }

    fn unbind(&self, aor: &str, contact: &str) -> Result<(), RegistrationError> {
        let key = (aor.to_owned(), contact.to_owned());
        if write_lock(&self.entries).remove(&key).is_none() {
            return Err(RegistrationError::UnknownAor(aor.to_owned()));
        }
        Ok(())
    }

    fn lookup_bindings(&self, aor: &str) -> Result<Vec<Binding>, RegistrationError> {
        let now = unix_now_secs();
        Ok(read_lock(&self.entries)
            .iter()
            .filter(|(k, v)| k.0 == aor && v.expires_at_unix > now)
            .map(|(_, v)| v.clone())
            .collect())
    }

    fn snapshot(&self) -> Result<Vec<Binding>, RegistrationError> {
        let now = unix_now_secs();
        Ok(read_lock(&self.entries)
            .values()
            .filter(|b| b.expires_at_unix > now)
            .cloned()
            .collect())
    }
}

fn unix_now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(feature = "auth-sqlite")]
pub mod sqlite_store;

#[cfg(feature = "auth-http")]
pub mod http_store;

#[cfg(test)]
mod tests {
    use super::*;

    fn creds(u: &str) -> Credentials {
        Credentials::new(u, "smiths.local", "s3cret")
    }

    #[test]
    fn insert_then_lookup() {
        let store = InMemoryCredentialStore::new();
        assert!(store.is_empty());
        store.insert(creds("alice"));
        assert_eq!(store.len(), 1);
        let got = store.lookup("smiths.local", "alice").unwrap();
        assert_eq!(got.password, "s3cret");
    }

    #[test]
    fn miss_returns_none() {
        let store = InMemoryCredentialStore::new();
        assert!(store.lookup("smiths.local", "ghost").is_none());
    }

    #[test]
    fn remove_evicts() {
        let store = InMemoryCredentialStore::new();
        store.insert(creds("bob"));
        assert!(store.remove("smiths.local", "bob").is_some());
        assert!(store.lookup("smiths.local", "bob").is_none());
    }

    #[test]
    fn default_lookup_for_ignores_algorithm() {
        let store = InMemoryCredentialStore::new();
        store.insert(creds("carol"));
        let md5 = store
            .lookup_for("smiths.local", "carol", Algorithm::Md5)
            .unwrap();
        let sha = store
            .lookup_for("smiths.local", "carol", Algorithm::Sha256)
            .unwrap();
        assert_eq!(md5, sha);
        assert!(
            store
                .lookup_async("smiths.local", "carol", Algorithm::Md5)
                .is_none(),
            "plain stores have no native async path"
        );
    }
}

/// RFC 2617 / RFC 7616 (+ RFC 8760) digest authentication primitives
/// + registrar.
///
/// We deliberately support both MD5 and SHA-256 because softphones in
/// the wild still speak MD5; SHA-256 is the forward-looking default
/// when the client offers it. Response comparison uses constant-time
/// equality to avoid timing leaks.
pub mod digest {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use dashmap::DashMap;
    use md5::{Digest as Md5Digest, Md5};
    use rand::Rng as _;
    use sha2::Sha256;
    use thiserror::Error;
    use tracing::{debug, warn};

    use super::{CredentialStore, Credentials};

    /// Supported digest algorithms.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub enum Algorithm {
        /// RFC 2617 MD5.
        Md5,
        /// RFC 8760 SHA-256.
        Sha256,
    }

    impl Algorithm {
        /// `MD5` / `SHA-256` token for wire use.
        #[must_use]
        pub const fn as_str(self) -> &'static str {
            match self {
                Self::Md5 => "MD5",
                Self::Sha256 => "SHA-256",
            }
        }

        /// Parse the wire token (case-insensitive).
        #[must_use]
        pub fn parse(token: &str) -> Option<Self> {
            match token.to_ascii_uppercase().as_str() {
                "MD5" | "" => Some(Self::Md5), // absent = MD5 per RFC 2617
                "SHA-256" | "SHA256" => Some(Self::Sha256),
                _ => None,
            }
        }

        /// Hex-encoded digest of `input`.
        #[must_use]
        pub fn hash_hex(self, input: &[u8]) -> String {
            match self {
                Self::Md5 => {
                    let mut h = Md5::new();
                    h.update(input);
                    hex::encode(h.finalize())
                }
                Self::Sha256 => {
                    let mut h = Sha256::new();
                    h.update(input);
                    hex::encode(h.finalize())
                }
            }
        }
    }

    /// `HA1 = H(username:realm:password)` — the cachable credential digest.
    #[must_use]
    pub fn ha1(alg: Algorithm, username: &str, realm: &str, password: &str) -> String {
        alg.hash_hex(format!("{username}:{realm}:{password}").as_bytes())
    }

    /// `HA2 = H(method:uri)` — request-side digest.
    #[must_use]
    pub fn ha2(alg: Algorithm, method: &str, uri: &str) -> String {
        alg.hash_hex(format!("{method}:{uri}").as_bytes())
    }

    /// Response for `qop=auth`: `H(HA1:nonce:nc:cnonce:qop:HA2)`.
    #[must_use]
    pub fn response_qop_auth(
        alg: Algorithm,
        ha1: &str,
        nonce: &str,
        nc: &str,
        cnonce: &str,
        ha2: &str,
    ) -> String {
        alg.hash_hex(format!("{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}").as_bytes())
    }

    /// Response without `qop`: `H(HA1:nonce:HA2)` (legacy).
    #[must_use]
    pub fn response_no_qop(alg: Algorithm, ha1: &str, nonce: &str, ha2: &str) -> String {
        alg.hash_hex(format!("{ha1}:{nonce}:{ha2}").as_bytes())
    }

    /// Parsed `Authorization:` / `Proxy-Authorization:` header params.
    #[derive(Clone, Debug, Default)]
    pub struct AuthParams {
        pub username: String,
        pub realm: String,
        pub nonce: String,
        pub uri: String,
        pub response: String,
        pub algorithm: Option<String>,
        pub qop: Option<String>,
        pub nc: Option<String>,
        pub cnonce: Option<String>,
    }

    /// Parse `Digest foo="bar", baz=qux, ...` into a flat map. Returns
    /// `None` if the scheme isn't `Digest`.
    #[must_use]
    pub fn parse_authorization(header_value: &str) -> Option<AuthParams> {
        let trimmed = header_value.trim();
        let rest = trimmed
            .strip_prefix("Digest ")
            .or_else(|| trimmed.strip_prefix("digest "))?;
        let mut params = AuthParams::default();
        for raw in split_params(rest) {
            let (k, v) = raw.split_once('=')?;
            let key = k.trim().to_ascii_lowercase();
            let val = unquote(v.trim());
            match key.as_str() {
                "username" => params.username = val,
                "realm" => params.realm = val,
                "nonce" => params.nonce = val,
                "uri" => params.uri = val,
                "response" => params.response = val,
                "algorithm" => params.algorithm = Some(val),
                "qop" => params.qop = Some(val),
                "nc" => params.nc = Some(val),
                "cnonce" => params.cnonce = Some(val),
                _ => {} // ignore unknown params for forward-compat
            }
        }
        if params.username.is_empty()
            || params.realm.is_empty()
            || params.nonce.is_empty()
            || params.uri.is_empty()
            || params.response.is_empty()
        {
            return None;
        }
        Some(params)
    }

    /// Comma-split that respects `"..."` quoting.
    fn split_params(input: &str) -> Vec<&str> {
        let mut out = Vec::new();
        let mut start = 0;
        let mut in_quote = false;
        let bytes = input.as_bytes();
        for (i, b) in bytes.iter().enumerate() {
            match b {
                b'"' => in_quote = !in_quote,
                b',' if !in_quote => {
                    out.push(input[start..i].trim());
                    start = i + 1;
                }
                _ => {}
            }
        }
        let tail = input[start..].trim();
        if !tail.is_empty() {
            out.push(tail);
        }
        out
    }

    fn unquote(s: &str) -> String {
        if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
            s[1..s.len() - 1].to_owned()
        } else {
            s.to_owned()
        }
    }

    /// Errors raised by [`Registrar::authenticate`] and
    /// [`Registrar::authenticate_async`].
    #[derive(Debug, Error, PartialEq, Eq)]
    #[non_exhaustive]
    pub enum AuthError {
        /// Header missing or wrong scheme.
        #[error("no digest credentials")]
        Missing,
        /// Realm in the request doesn't match the registrar's realm.
        #[error("wrong realm")]
        WrongRealm,
        /// The digest was valid but the nonce was not issued by (or
        /// has expired in) this registrar. The client knows the
        /// password, so the re-challenge should carry `stale=true`
        /// (RFC 7616 §3.3) and the client retries without prompting.
        #[error("stale or unknown nonce")]
        StaleNonce,
        /// The nonce was live but the request re-used a nonce-count
        /// already accepted (`qop=auth`), or re-used a nonce that had
        /// already authenticated one request (no `qop`). Either way the
        /// header is a replay (RFC 7616 §5.1.2). Callers should
        /// re-challenge with `stale=true` so an honest client whose
        /// counter fell out of sync recovers transparently.
        #[error("replayed nonce")]
        NonceReplayed,
        /// Username not found in the credential store.
        #[error("unknown user")]
        UnknownUser,
        /// Computed response didn't match the submitted one.
        #[error("bad response")]
        BadResponse,
        /// `algorithm=` not in our supported set.
        #[error("unsupported algorithm")]
        UnsupportedAlgorithm,
        /// URI in the Authorization header can't be verified against
        /// the request-line (mismatched target).
        #[error("uri mismatch")]
        UriMismatch,
        /// The credential backend failed (blocking-pool task
        /// panicked or was cancelled). Distinct from `UnknownUser` so
        /// operators can tell an outage from a typo.
        #[error("credential backend: {0}")]
        Backend(String),
    }

    impl AuthError {
        /// `true` when the failure is a nonce-freshness problem rather
        /// than bad credentials — the re-challenge should carry
        /// `stale=true` so the client retries silently.
        #[must_use]
        pub const fn wants_stale_challenge(&self) -> bool {
            matches!(self, Self::StaleNonce | Self::NonceReplayed)
        }
    }

    /// Default cap on live nonces. Each entry is ~100 bytes, so the
    /// default bounds the table at roughly 10 MiB under a challenge
    /// flood.
    pub const DEFAULT_MAX_NONCES: usize = 100_000;

    /// Per-nonce replay state.
    struct NonceState {
        issued_at: Instant,
        /// Highest `nc` accepted so far for `qop=auth`. `0` means no
        /// request has authenticated with this nonce yet (clients
        /// start at `00000001`).
        last_nc: u32,
        /// `true` once a `qop`-less request has authenticated with
        /// this nonce. Without a nonce-count the only replay defence
        /// is single use.
        used_without_qop: bool,
    }

    /// Nonce table shared by every clone of a [`Registrar`].
    struct NonceTable {
        nonces: DashMap<String, NonceState>,
        /// Monotonic base for `last_gc_ms`.
        epoch: Instant,
        /// Milliseconds since `epoch` at the last sweep. Sweeps are
        /// amortized: at most one per `gc_interval` unless the table
        /// hits its cap.
        last_gc_ms: AtomicU64,
    }

    /// Issues nonces, tracks their use, verifies responses.
    ///
    /// Nonces are 16 CSPRNG bytes (hex) and live for
    /// [`Registrar::with_ttl`] after issuance. Every accepted
    /// `qop=auth` request must carry a strictly increasing `nc` for its
    /// nonce; `qop`-less nonces are single use. Expired nonces with a
    /// valid digest yield [`AuthError::StaleNonce`] so the caller can
    /// re-challenge with `stale=true`; the table is bounded by
    /// [`Registrar::with_max_nonces`] and swept lazily.
    ///
    /// Cheap to clone — clones share the nonce table.
    #[derive(Clone)]
    pub struct Registrar {
        realm: String,
        store: Arc<dyn CredentialStore>,
        table: Arc<NonceTable>,
        ttl: Duration,
        max_nonces: usize,
    }

    impl Registrar {
        /// Build a new registrar.
        #[must_use]
        pub fn new(realm: impl Into<String>, store: Arc<dyn CredentialStore>) -> Self {
            Self {
                realm: realm.into(),
                store,
                table: Arc::new(NonceTable {
                    nonces: DashMap::new(),
                    epoch: Instant::now(),
                    last_gc_ms: AtomicU64::new(0),
                }),
                ttl: Duration::from_mins(5),
                max_nonces: DEFAULT_MAX_NONCES,
            }
        }

        /// Override the nonce lifetime.
        #[must_use]
        pub const fn with_ttl(mut self, ttl: Duration) -> Self {
            self.ttl = ttl;
            self
        }

        /// Override the live-nonce cap. When the table is full the
        /// oldest tenth of the entries is evicted to make room;
        /// clients holding an evicted nonce see a `stale=true`
        /// re-challenge. Values below 1 are clamped to 1.
        #[must_use]
        pub const fn with_max_nonces(mut self, max_nonces: usize) -> Self {
            self.max_nonces = if max_nonces == 0 { 1 } else { max_nonces };
            self
        }

        /// Protection realm the registrar defends.
        #[must_use]
        pub fn realm(&self) -> &str {
            &self.realm
        }

        /// Number of nonces currently tracked. Diagnostics / tests.
        #[must_use]
        pub fn live_nonces(&self) -> usize {
            self.table.nonces.len()
        }

        /// Generate a fresh nonce and register it. Caller embeds it in
        /// a `WWW-Authenticate` challenge.
        #[must_use]
        pub fn issue_nonce(&self) -> String {
            let mut buf = [0u8; 16];
            rand::rng().fill_bytes(&mut buf);
            let nonce = hex::encode(buf);
            self.maybe_gc();
            self.table.nonces.insert(
                nonce.clone(),
                NonceState {
                    issued_at: Instant::now(),
                    last_nc: 0,
                    used_without_qop: false,
                },
            );
            nonce
        }

        /// Build a `WWW-Authenticate` header value, offering the
        /// caller's preferred algorithm (SHA-256 if asked, MD5 otherwise).
        #[must_use]
        pub fn challenge(&self, algorithm: Algorithm, stale: bool) -> String {
            let nonce = self.issue_nonce();
            let mut header = format!(
                "Digest realm=\"{realm}\", nonce=\"{nonce}\", qop=\"auth\", algorithm={alg}",
                realm = self.realm,
                alg = algorithm.as_str()
            );
            if stale {
                header.push_str(", stale=true");
            }
            header
        }

        /// Verify `auth_header` against the request's `method` and
        /// `request_uri`. On success returns the authenticated username.
        ///
        /// Synchronous: the credential lookup runs inline via
        /// [`CredentialStore::lookup_for`]. Prefer
        /// [`Self::authenticate_async`] from async code so network or
        /// disk-backed stores don't block the runtime.
        pub fn authenticate(
            &self,
            method: &str,
            request_uri: &str,
            auth_header: &str,
        ) -> Result<String, AuthError> {
            let (params, alg) = self.precheck(request_uri, auth_header)?;
            let creds = self
                .store
                .lookup_for(&self.realm, &params.username, alg)
                .ok_or(AuthError::UnknownUser)?;
            self.finish(params, alg, method, &creds)
        }

        /// Async twin of [`Self::authenticate`] with identical
        /// semantics. Stores with a native async path
        /// ([`CredentialStore::lookup_async`]) are awaited directly;
        /// every other store runs on tokio's blocking pool.
        pub async fn authenticate_async(
            &self,
            method: &str,
            request_uri: &str,
            auth_header: &str,
        ) -> Result<String, AuthError> {
            let (params, alg) = self.precheck(request_uri, auth_header)?;
            let looked_up =
                if let Some(fut) = self.store.lookup_async(&self.realm, &params.username, alg) {
                    fut.await
                } else {
                    let store = Arc::clone(&self.store);
                    let realm = self.realm.clone();
                    let username = params.username.clone();
                    tokio::task::spawn_blocking(move || store.lookup_for(&realm, &username, alg))
                        .await
                        .map_err(|e| AuthError::Backend(e.to_string()))?
                };
            let creds = looked_up.ok_or(AuthError::UnknownUser)?;
            self.finish(params, alg, method, &creds)
        }

        /// Everything that can be decided before touching the
        /// credential store: header shape, realm, algorithm, URI, and
        /// the cheap replay rejections for a known nonce.
        fn precheck(
            &self,
            request_uri: &str,
            auth_header: &str,
        ) -> Result<(AuthParams, Algorithm), AuthError> {
            let params = parse_authorization(auth_header).ok_or(AuthError::Missing)?;
            if params.realm != self.realm {
                return Err(AuthError::WrongRealm);
            }
            let alg = Algorithm::parse(params.algorithm.as_deref().unwrap_or(""))
                .ok_or(AuthError::UnsupportedAlgorithm)?;

            // The `uri` in Authorization should be what the client
            // signed. RFC 2617 is silent on canonicalization, so in
            // practice clients vary:
            //   - sipp drops the user-part (`sip:127.0.0.1:5062`
            //     instead of `sip:smiths.test@127.0.0.1:5062`).
            //   - some UAs normalize ports or scheme.
            // Accept an exact match, a prefix/substring containment
            // (legacy behaviour), or an authority-only match where
            // `host[:port]` agrees and only the user-part differs.
            let uri_ok = params.uri == request_uri
                || request_uri.contains(&params.uri)
                || sip_uri_authority(&params.uri) == sip_uri_authority(request_uri);
            if !uri_ok {
                debug!(
                    auth_uri = %params.uri,
                    request_uri,
                    "digest URI mismatch"
                );
                return Err(AuthError::UriMismatch);
            }

            // A replayed header for a live nonce is rejected before
            // the store round-trip: the digest in a replay is valid by
            // construction, so refusing early leaks nothing.
            if let Some(state) = self.table.nonces.get(&params.nonce)
                && state.issued_at.elapsed() <= self.ttl
            {
                match nonce_count(&params) {
                    Some(nc) if nc <= state.last_nc => return Err(AuthError::NonceReplayed),
                    None if state.used_without_qop => return Err(AuthError::NonceReplayed),
                    _ => {}
                }
            }
            Ok((params, alg))
        }

        /// Verify the digest, then claim the nonce (nonce-count or
        /// single use) atomically so two racing copies of the same
        /// header can never both succeed.
        fn finish(
            &self,
            params: AuthParams,
            alg: Algorithm,
            method: &str,
            creds: &Credentials,
        ) -> Result<String, AuthError> {
            // HA1: pre-computed when the backend supplies it (HTTP
            // webhook, SQLite) so plaintext passwords never cross the
            // backend boundary; otherwise hashed on demand from the
            // plaintext the in-memory store holds.
            let ha1 = creds
                .ha1
                .clone()
                .unwrap_or_else(|| ha1(alg, &creds.username, &creds.realm, &creds.password));
            let ha2 = ha2(alg, method, &params.uri);

            let nc = nonce_count(&params);
            let expected = match (
                params.qop.as_deref(),
                params.nc.as_deref(),
                params.cnonce.as_deref(),
            ) {
                (Some("auth"), Some(nc_raw), Some(cnonce)) => {
                    response_qop_auth(alg, &ha1, &params.nonce, nc_raw, cnonce, &ha2)
                }
                _ => response_no_qop(alg, &ha1, &params.nonce, &ha2),
            };
            if !constant_time_eq(expected.as_bytes(), params.response.as_bytes()) {
                return Err(AuthError::BadResponse);
            }

            self.claim_nonce(&params.nonce, nc)?;
            Ok(params.username)
        }

        /// Record the use of `nonce`. `nc` is `Some` for `qop=auth`
        /// (must strictly exceed the last accepted value) and `None`
        /// for the legacy path (nonce becomes spent).
        fn claim_nonce(&self, nonce: &str, nc: Option<u32>) -> Result<(), AuthError> {
            let Some(mut state) = self.table.nonces.get_mut(nonce) else {
                return Err(AuthError::StaleNonce);
            };
            if state.issued_at.elapsed() > self.ttl {
                drop(state);
                self.table.nonces.remove(nonce);
                return Err(AuthError::StaleNonce);
            }
            if let Some(nc) = nc {
                if nc <= state.last_nc {
                    return Err(AuthError::NonceReplayed);
                }
                state.last_nc = nc;
            } else {
                if state.used_without_qop {
                    return Err(AuthError::NonceReplayed);
                }
                state.used_without_qop = true;
            }
            Ok(())
        }

        /// Sweep expired nonces, at most once per `ttl` (capped at
        /// one second) unless the table is at its cap, in which case
        /// the sweep runs unconditionally and — if still full — the
        /// oldest tenth of the table is evicted.
        fn maybe_gc(&self) {
            let table = &self.table;
            let now = Instant::now();
            let now_ms =
                u64::try_from(now.duration_since(table.epoch).as_millis()).unwrap_or(u64::MAX);
            let interval_ms =
                u64::try_from(self.ttl.min(Duration::from_secs(1)).as_millis()).unwrap_or(1_000);
            let full = table.nonces.len() >= self.max_nonces;
            let due =
                now_ms.saturating_sub(table.last_gc_ms.load(Ordering::Relaxed)) >= interval_ms;
            if !full && !due {
                return;
            }
            table.last_gc_ms.store(now_ms, Ordering::Relaxed);
            let ttl = self.ttl;
            table
                .nonces
                .retain(|_, state| now.duration_since(state.issued_at) <= ttl);
            if table.nonces.len() < self.max_nonces {
                return;
            }
            // Still full: every live nonce is younger than `ttl`, so
            // we are under a challenge flood. Drop the oldest tenth.
            let mut ages: Vec<(Duration, String)> = table
                .nonces
                .iter()
                .map(|e| (now.duration_since(e.value().issued_at), e.key().clone()))
                .collect();
            ages.sort_unstable_by_key(|(age, _)| std::cmp::Reverse(*age));
            let evict = (ages.len() / 10).max(1);
            for (_, key) in ages.into_iter().take(evict) {
                table.nonces.remove(&key);
            }
            warn!(
                evicted = evict,
                cap = self.max_nonces,
                "digest nonce table full; evicted oldest nonces"
            );
        }
    }

    /// Parse the `nc` parameter for a `qop=auth` request. `None`
    /// when the request carries no `qop` (legacy path). A `qop=auth`
    /// request with an unparseable count parses as `Some(u32::MAX)`
    /// so it can authenticate once and then never again — the digest
    /// still covers the raw string, so correctness is unaffected.
    fn nonce_count(params: &AuthParams) -> Option<u32> {
        match (
            params.qop.as_deref(),
            params.nc.as_deref(),
            params.cnonce.as_deref(),
        ) {
            (Some("auth"), Some(nc), Some(_)) => {
                Some(u32::from_str_radix(nc.trim(), 16).unwrap_or(u32::MAX))
            }
            _ => None,
        }
    }

    /// Extract the authority (`host[:port]`) portion of a SIP URI.
    /// Strips the `sip:` / `sips:` scheme, any `user@` user-info, and
    /// trailing `;params` / `?headers`. Used for the permissive URI
    /// match in digest auth — clients often sign just the authority
    /// (e.g. sipp's default) even when the request-line carries a
    /// user-part.
    fn sip_uri_authority(uri: &str) -> &str {
        let body = uri
            .strip_prefix("sips:")
            .or_else(|| uri.strip_prefix("sip:"))
            .unwrap_or(uri);
        let after_user = body.rfind('@').map_or(body, |i| &body[i + 1..]);
        after_user.split([';', '?']).next().unwrap_or(after_user)
    }

    /// Constant-time byte slice equality — guards against timing-based
    /// response guessing.
    fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
        if a.len() != b.len() {
            return false;
        }
        let mut diff: u8 = 0;
        for (x, y) in a.iter().zip(b.iter()) {
            diff |= x ^ y;
        }
        diff == 0
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::auth::{CredentialFuture, Credentials, InMemoryCredentialStore};

        /// RFC 2617 test vector — without qop.
        #[test]
        fn rfc2617_no_qop_vector() {
            let user = "Mufasa";
            let pass = "Circle Of Life";
            let realm = "testrealm@host.com";
            let nonce = "dcd98b7102dd2f0e8b11d0f600bfb0c093";
            let method = "GET";
            let uri = "/dir/index.html";

            let h1 = ha1(Algorithm::Md5, user, realm, pass);
            assert_eq!(h1, "939e7578ed9e3c518a452acee763bce9");
            let h2 = ha2(Algorithm::Md5, method, uri);
            assert_eq!(h2, "39aff3a2bab6126f332b942af96d3366");
            let resp = response_no_qop(Algorithm::Md5, &h1, nonce, &h2);
            assert_eq!(resp, "670fd8c2df070c60b045671b8b24ff02");
        }

        /// RFC 2617 example — with qop=auth.
        #[test]
        fn rfc2617_qop_auth_vector() {
            let user = "Mufasa";
            let pass = "Circle Of Life";
            let realm = "testrealm@host.com";
            let nonce = "dcd98b7102dd2f0e8b11d0f600bfb0c093";
            let cnonce = "0a4f113b";
            let nc = "00000001";
            let method = "GET";
            let uri = "/dir/index.html";

            let h1 = ha1(Algorithm::Md5, user, realm, pass);
            let h2 = ha2(Algorithm::Md5, method, uri);
            let resp = response_qop_auth(Algorithm::Md5, &h1, nonce, nc, cnonce, &h2);
            assert_eq!(resp, "6629fae49393a05397450978507c4ef1");
        }

        #[test]
        fn parse_authorization_quoted_and_unquoted() {
            let hdr = r#"Digest username="alice", realm="smiths.local", nonce="abc", uri="sip:smiths.local", response="deadbeef", algorithm=MD5, qop=auth, nc=00000001, cnonce="xyz""#;
            let p = parse_authorization(hdr).unwrap();
            assert_eq!(p.username, "alice");
            assert_eq!(p.realm, "smiths.local");
            assert_eq!(p.nonce, "abc");
            assert_eq!(p.uri, "sip:smiths.local");
            assert_eq!(p.response, "deadbeef");
            assert_eq!(p.algorithm.as_deref(), Some("MD5"));
            assert_eq!(p.qop.as_deref(), Some("auth"));
            assert_eq!(p.nc.as_deref(), Some("00000001"));
            assert_eq!(p.cnonce.as_deref(), Some("xyz"));
        }

        #[test]
        fn parse_authorization_rejects_non_digest() {
            assert!(parse_authorization("Basic YWxpY2U6c2VjcmV0").is_none());
        }

        /// Build a `qop=auth` Authorization header for `user` /
        /// `pass` on `nonce` with the given nonce-count.
        fn qop_header(alg: Algorithm, user: &str, pass: &str, nonce: &str, nc: &str) -> String {
            let uri = "sip:smiths.local";
            let h1 = ha1(alg, user, "smiths.local", pass);
            let h2 = ha2(alg, "REGISTER", uri);
            let resp = response_qop_auth(alg, &h1, nonce, nc, "cnonce-1", &h2);
            format!(
                "Digest username=\"{user}\", realm=\"smiths.local\", \
                 nonce=\"{nonce}\", uri=\"{uri}\", response=\"{resp}\", \
                 algorithm={alg}, qop=auth, nc={nc}, cnonce=\"cnonce-1\"",
                alg = alg.as_str()
            )
        }

        /// Legacy (no `qop`) Authorization header.
        fn no_qop_header(user: &str, pass: &str, nonce: &str) -> String {
            let uri = "sip:smiths.local";
            let h1 = ha1(Algorithm::Md5, user, "smiths.local", pass);
            let h2 = ha2(Algorithm::Md5, "REGISTER", uri);
            let resp = response_no_qop(Algorithm::Md5, &h1, nonce, &h2);
            format!(
                "Digest username=\"{user}\", realm=\"smiths.local\", \
                 nonce=\"{nonce}\", uri=\"{uri}\", response=\"{resp}\", algorithm=MD5"
            )
        }

        fn registrar_with(user: &str, pass: &str) -> Registrar {
            let store = Arc::new(InMemoryCredentialStore::new());
            store.insert(Credentials::new(user, "smiths.local", pass));
            Registrar::new("smiths.local", store)
        }

        #[test]
        fn registrar_round_trip_md5() {
            let reg = registrar_with("alice", "s3cret");
            let challenge = reg.challenge(Algorithm::Md5, false);
            let nonce = extract_param(&challenge, "nonce").unwrap();
            let hdr = qop_header(Algorithm::Md5, "alice", "s3cret", &nonce, "00000001");
            let user = reg
                .authenticate("REGISTER", "sip:smiths.local", &hdr)
                .unwrap();
            assert_eq!(user, "alice");
        }

        #[test]
        fn registrar_round_trip_sha256() {
            let reg = registrar_with("bob", "hunter2");
            let challenge = reg.challenge(Algorithm::Sha256, false);
            let nonce = extract_param(&challenge, "nonce").unwrap();
            let hdr = qop_header(Algorithm::Sha256, "bob", "hunter2", &nonce, "00000001");
            assert_eq!(
                reg.authenticate("REGISTER", "sip:smiths.local", &hdr)
                    .unwrap(),
                "bob"
            );
        }

        #[test]
        fn registrar_bad_password_fails() {
            let reg = registrar_with("alice", "right");
            let challenge = reg.challenge(Algorithm::Md5, false);
            let nonce = extract_param(&challenge, "nonce").unwrap();
            let hdr = qop_header(Algorithm::Md5, "alice", "wrong", &nonce, "00000001");
            assert_eq!(
                reg.authenticate("REGISTER", "sip:smiths.local", &hdr),
                Err(AuthError::BadResponse)
            );
        }

        #[test]
        fn registrar_unknown_user_fails() {
            let store = Arc::new(InMemoryCredentialStore::new());
            let reg = Registrar::new("smiths.local", store);
            let challenge = reg.challenge(Algorithm::Md5, false);
            let nonce = extract_param(&challenge, "nonce").unwrap();
            let hdr = format!(
                "Digest username=\"ghost\", realm=\"smiths.local\", \
                 nonce=\"{nonce}\", uri=\"sip:x\", response=\"0\", \
                 algorithm=MD5, qop=auth, nc=00000001, cnonce=\"c\""
            );
            assert_eq!(
                reg.authenticate("REGISTER", "sip:x", &hdr),
                Err(AuthError::UnknownUser)
            );
        }

        #[test]
        fn registrar_unknown_nonce_with_valid_digest_is_stale() {
            let reg = registrar_with("alice", "p");
            // Never issued this nonce, but the digest is correct →
            // the client knows the password → stale re-challenge.
            let hdr = qop_header(Algorithm::Md5, "alice", "p", "fake", "00000001");
            let err = reg
                .authenticate("REGISTER", "sip:smiths.local", &hdr)
                .unwrap_err();
            assert_eq!(err, AuthError::StaleNonce);
            assert!(err.wants_stale_challenge());
        }

        #[test]
        fn registrar_unknown_nonce_with_bad_digest_is_bad_response() {
            let reg = registrar_with("alice", "p");
            let hdr = qop_header(Algorithm::Md5, "alice", "WRONG", "fake", "00000001");
            let err = reg
                .authenticate("REGISTER", "sip:smiths.local", &hdr)
                .unwrap_err();
            assert_eq!(err, AuthError::BadResponse);
            assert!(!err.wants_stale_challenge());
        }

        #[test]
        fn expired_nonce_yields_stale_challenge() {
            let reg = registrar_with("alice", "p").with_ttl(Duration::from_millis(1));
            let challenge = reg.challenge(Algorithm::Md5, false);
            let nonce = extract_param(&challenge, "nonce").unwrap();
            std::thread::sleep(Duration::from_millis(10));
            let hdr = qop_header(Algorithm::Md5, "alice", "p", &nonce, "00000001");
            assert_eq!(
                reg.authenticate("REGISTER", "sip:smiths.local", &hdr),
                Err(AuthError::StaleNonce)
            );
        }

        #[test]
        fn replayed_qop_header_is_rejected() {
            let reg = registrar_with("alice", "p");
            let challenge = reg.challenge(Algorithm::Md5, false);
            let nonce = extract_param(&challenge, "nonce").unwrap();
            let hdr = qop_header(Algorithm::Md5, "alice", "p", &nonce, "00000001");
            assert!(
                reg.authenticate("REGISTER", "sip:smiths.local", &hdr)
                    .is_ok()
            );
            // Byte-for-byte replay of a header that just authenticated.
            let err = reg
                .authenticate("REGISTER", "sip:smiths.local", &hdr)
                .unwrap_err();
            assert_eq!(err, AuthError::NonceReplayed);
            assert!(err.wants_stale_challenge());
        }

        #[test]
        fn increasing_nonce_count_is_accepted_and_regressions_rejected() {
            let reg = registrar_with("alice", "p");
            let challenge = reg.challenge(Algorithm::Md5, false);
            let nonce = extract_param(&challenge, "nonce").unwrap();
            for nc in ["00000001", "00000002", "00000005"] {
                let hdr = qop_header(Algorithm::Md5, "alice", "p", &nonce, nc);
                assert_eq!(
                    reg.authenticate("REGISTER", "sip:smiths.local", &hdr),
                    Ok("alice".to_owned()),
                    "nc={nc} must be accepted"
                );
            }
            // Lower than the last accepted count → replay.
            let hdr = qop_header(Algorithm::Md5, "alice", "p", &nonce, "00000003");
            assert_eq!(
                reg.authenticate("REGISTER", "sip:smiths.local", &hdr),
                Err(AuthError::NonceReplayed)
            );
            // Equal to the last accepted count → replay.
            let hdr = qop_header(Algorithm::Md5, "alice", "p", &nonce, "00000005");
            assert_eq!(
                reg.authenticate("REGISTER", "sip:smiths.local", &hdr),
                Err(AuthError::NonceReplayed)
            );
            // Strictly greater → fine again.
            let hdr = qop_header(Algorithm::Md5, "alice", "p", &nonce, "00000006");
            assert!(
                reg.authenticate("REGISTER", "sip:smiths.local", &hdr)
                    .is_ok()
            );
        }

        #[test]
        fn bad_digest_does_not_consume_nonce_count() {
            let reg = registrar_with("alice", "p");
            let challenge = reg.challenge(Algorithm::Md5, false);
            let nonce = extract_param(&challenge, "nonce").unwrap();
            let bad = qop_header(Algorithm::Md5, "alice", "wrong", &nonce, "00000001");
            assert_eq!(
                reg.authenticate("REGISTER", "sip:smiths.local", &bad),
                Err(AuthError::BadResponse)
            );
            // The honest client retries with the same count and wins.
            let good = qop_header(Algorithm::Md5, "alice", "p", &nonce, "00000001");
            assert!(
                reg.authenticate("REGISTER", "sip:smiths.local", &good)
                    .is_ok()
            );
        }

        #[test]
        fn no_qop_nonce_is_single_use() {
            let reg = registrar_with("alice", "p");
            let challenge = reg.challenge(Algorithm::Md5, false);
            let nonce = extract_param(&challenge, "nonce").unwrap();
            let hdr = no_qop_header("alice", "p", &nonce);
            assert_eq!(
                reg.authenticate("REGISTER", "sip:smiths.local", &hdr),
                Ok("alice".to_owned())
            );
            assert_eq!(
                reg.authenticate("REGISTER", "sip:smiths.local", &hdr),
                Err(AuthError::NonceReplayed)
            );
        }

        #[test]
        fn nonces_are_random_and_distinct() {
            let reg = registrar_with("alice", "p");
            let a = reg.issue_nonce();
            let b = reg.issue_nonce();
            assert_eq!(a.len(), 32, "16 bytes hex");
            assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
            assert_ne!(a, b);
        }

        #[test]
        fn nonce_table_is_bounded() {
            let reg = registrar_with("alice", "p").with_max_nonces(20);
            for _ in 0..200 {
                let _ = reg.issue_nonce();
            }
            assert!(
                reg.live_nonces() <= 20,
                "table must stay at or under its cap, got {}",
                reg.live_nonces()
            );
            // The most recent nonce survives the eviction and still works.
            let challenge = reg.challenge(Algorithm::Md5, false);
            let nonce = extract_param(&challenge, "nonce").unwrap();
            let hdr = qop_header(Algorithm::Md5, "alice", "p", &nonce, "00000001");
            assert!(
                reg.authenticate("REGISTER", "sip:smiths.local", &hdr)
                    .is_ok()
            );
        }

        #[test]
        fn expired_nonces_are_swept_on_issue() {
            let reg = registrar_with("alice", "p").with_ttl(Duration::from_millis(1));
            for _ in 0..10 {
                let _ = reg.issue_nonce();
            }
            std::thread::sleep(Duration::from_millis(10));
            let _ = reg.issue_nonce();
            assert_eq!(reg.live_nonces(), 1, "only the fresh nonce survives");
        }

        #[tokio::test(flavor = "current_thread")]
        async fn authenticate_async_round_trip_via_blocking_pool() {
            let reg = registrar_with("alice", "s3cret");
            let challenge = reg.challenge(Algorithm::Md5, false);
            let nonce = extract_param(&challenge, "nonce").unwrap();
            let hdr = qop_header(Algorithm::Md5, "alice", "s3cret", &nonce, "00000001");
            let user = reg
                .authenticate_async("REGISTER", "sip:smiths.local", &hdr)
                .await
                .unwrap();
            assert_eq!(user, "alice");
            // Replay protection is shared with the sync path.
            assert_eq!(
                reg.authenticate_async("REGISTER", "sip:smiths.local", &hdr)
                    .await,
                Err(AuthError::NonceReplayed)
            );
        }

        /// Store whose async path is the only one that works —
        /// proves `authenticate_async` prefers `lookup_async`.
        struct AsyncOnlyStore;

        impl CredentialStore for AsyncOnlyStore {
            fn lookup(&self, _realm: &str, _username: &str) -> Option<Credentials> {
                None
            }
            fn lookup_async<'a>(
                &'a self,
                realm: &'a str,
                username: &'a str,
                _algorithm: Algorithm,
            ) -> Option<CredentialFuture<'a>> {
                Some(Box::pin(async move {
                    tokio::task::yield_now().await;
                    Some(Credentials::new(username, realm, "async-pw"))
                }))
            }
        }

        #[tokio::test(flavor = "current_thread")]
        async fn authenticate_async_uses_native_async_lookup() {
            let reg = Registrar::new("smiths.local", Arc::new(AsyncOnlyStore));
            let challenge = reg.challenge(Algorithm::Sha256, false);
            let nonce = extract_param(&challenge, "nonce").unwrap();
            let hdr = qop_header(Algorithm::Sha256, "dave", "async-pw", &nonce, "00000001");
            assert_eq!(
                reg.authenticate_async("REGISTER", "sip:smiths.local", &hdr)
                    .await,
                Ok("dave".to_owned())
            );
            // The sync path only sees the (empty) sync lookup.
            let hdr2 = qop_header(Algorithm::Sha256, "dave", "async-pw", &nonce, "00000002");
            assert_eq!(
                reg.authenticate("REGISTER", "sip:smiths.local", &hdr2),
                Err(AuthError::UnknownUser)
            );
        }

        /// Pull `name="value"` out of a free-form `Digest ...` header.
        fn extract_param(header: &str, name: &str) -> Option<String> {
            let needle = format!("{name}=\"");
            let start = header.find(&needle)? + needle.len();
            let end = header[start..].find('"')? + start;
            Some(header[start..end].to_owned())
        }
    }
}
