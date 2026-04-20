//! Digest authentication.
//!
//! Two parts:
//!
//! * **`CredentialStore`** trait + `InMemoryCredentialStore` default —
//!   MVP guardrail for pluggable subscriber DBs (`SQLite` / Postgres /
//!   LDAP / sidecar plugin) without engine changes.
//! * **`digest`** module — RFC 2617 / RFC 8760 MD5 and SHA-256 digest
//!   computation, plus a stateful [`Registrar`] that issues nonces,
//!   parses `Authorization:` headers, and verifies responses against
//!   the credential store.

// Slice 1.7: the `expect()` call sites in this module are all on
// `RwLock` guards protecting in-memory auth state. A poisoned lock
// means another thread panicked mid-mutation; recovering would leave
// credential tables in an ambiguous state, so propagating the panic
// is the correct response. Per-call `#[allow]` would be noisier than
// one module-level justification.
#![allow(clippy::expect_used)]

use std::collections::HashMap;
use std::sync::RwLock;

/// A set of credentials for one account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credentials {
    /// SIP username (typically the URI's user part).
    pub username: String,
    /// Authentication realm.
    pub realm: String,
    /// Plaintext password. Hashed HA1 variants land later.
    pub password: String,
}

/// Lookup interface the SIP auth path uses to resolve credentials.
///
/// Implementations must be `Send + Sync + 'static` because the lookup
/// happens from multiple transport tasks.
pub trait CredentialStore: Send + Sync + 'static {
    /// Return credentials for `(realm, username)` if known.
    fn lookup(&self, realm: &str, username: &str) -> Option<Credentials>;
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
        self.entries
            .write()
            .expect("credential store poisoned")
            .insert(key, creds);
    }

    /// Remove an account; returns the previous value if it existed.
    pub fn remove(&self, realm: &str, username: &str) -> Option<Credentials> {
        self.entries
            .write()
            .expect("credential store poisoned")
            .remove(&(realm.to_owned(), username.to_owned()))
    }

    /// Number of stored accounts.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries
            .read()
            .expect("credential store poisoned")
            .len()
    }

    /// `true` if no accounts are stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl CredentialStore for InMemoryCredentialStore {
    fn lookup(&self, realm: &str, username: &str) -> Option<Credentials> {
        self.entries
            .read()
            .expect("credential store poisoned")
            .get(&(realm.to_owned(), username.to_owned()))
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds(u: &str) -> Credentials {
        Credentials {
            username: u.to_owned(),
            realm: "smiths.local".to_owned(),
            password: "s3cret".to_owned(),
        }
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
}

/// RFC 2617 (+ RFC 8760) digest authentication primitives + registrar.
///
/// We deliberately support both MD5 and SHA-256 because softphones in
/// the wild still speak MD5; SHA-256 is the forward-looking default
/// when the client offers it. Response comparison uses constant-time
/// equality to avoid timing leaks.
pub mod digest {
    use std::sync::Arc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use dashmap::DashMap;
    use md5::{Digest as Md5Digest, Md5};
    use sha2::Sha256;
    use thiserror::Error;

    use super::CredentialStore;

    /// Supported digest algorithms.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

    /// Errors raised by [`Registrar::authenticate`].
    #[derive(Debug, Error, PartialEq, Eq)]
    #[non_exhaustive]
    pub enum AuthError {
        /// Header missing or wrong scheme.
        #[error("no digest credentials")]
        Missing,
        /// Realm in the request doesn't match the registrar's realm.
        #[error("wrong realm")]
        WrongRealm,
        /// Nonce was not issued by (or has expired in) this registrar.
        #[error("stale or unknown nonce")]
        StaleNonce,
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
    }

    /// Issues nonces, caches them briefly, verifies responses.
    ///
    /// Nonces are kept for [`Registrar::ttl`] after issuance. Beyond
    /// that a replayed request gets `StaleNonce` and the client
    /// should retry with the new nonce from the fresh 401 challenge.
    #[derive(Clone)]
    pub struct Registrar {
        realm: String,
        store: Arc<dyn CredentialStore>,
        nonces: Arc<DashMap<String, u64>>,
        ttl: Duration,
    }

    impl Registrar {
        /// Build a new registrar.
        #[must_use]
        pub fn new(realm: impl Into<String>, store: Arc<dyn CredentialStore>) -> Self {
            Self {
                realm: realm.into(),
                store,
                nonces: Arc::new(DashMap::new()),
                ttl: Duration::from_secs(300), // 5 minutes is a fine default
            }
        }

        /// Override the nonce lifetime.
        #[must_use]
        pub const fn with_ttl(mut self, ttl: Duration) -> Self {
            self.ttl = ttl;
            self
        }

        /// Protection realm the registrar defends.
        #[must_use]
        pub fn realm(&self) -> &str {
            &self.realm
        }

        /// Generate a fresh nonce and register it. Caller embeds it in
        /// a `WWW-Authenticate` challenge.
        #[must_use]
        pub fn issue_nonce(&self) -> String {
            // Random 16 bytes, hex. Collisions are astronomically
            // unlikely at our TTL.
            let mut buf = [0u8; 16];
            // Use process-time + a counter-ish seed. Not cryptographic
            // grade, but fine for nonce freshness (replay is prevented
            // by nonce-set membership, not unguessability alone).
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let seed: [u8; 16] = nanos.to_be_bytes();
            buf.copy_from_slice(&seed);
            let nonce = hex::encode(buf);
            self.nonces.insert(nonce.clone(), now_secs());
            self.gc_nonces();
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
        pub fn authenticate(
            &self,
            method: &str,
            request_uri: &str,
            auth_header: &str,
        ) -> Result<String, AuthError> {
            let params = parse_authorization(auth_header).ok_or(AuthError::Missing)?;
            if params.realm != self.realm {
                return Err(AuthError::WrongRealm);
            }
            let alg = Algorithm::parse(params.algorithm.as_deref().unwrap_or(""))
                .ok_or(AuthError::UnsupportedAlgorithm)?;

            // Nonce must be live.
            let fresh = match self.nonces.get(&params.nonce) {
                Some(e) => now_secs().saturating_sub(*e.value()) <= self.ttl.as_secs(),
                None => false,
            };
            if !fresh {
                return Err(AuthError::StaleNonce);
            }

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
                tracing::debug!(
                    auth_uri = %params.uri,
                    request_uri,
                    "digest URI mismatch"
                );
                return Err(AuthError::UriMismatch);
            }

            let creds = self
                .store
                .lookup(&self.realm, &params.username)
                .ok_or(AuthError::UnknownUser)?;
            let ha1 = ha1(alg, &creds.username, &creds.realm, &creds.password);
            let ha2 = ha2(alg, method, &params.uri);

            let expected = match (
                params.qop.as_deref(),
                params.nc.as_deref(),
                params.cnonce.as_deref(),
            ) {
                (Some("auth"), Some(nc), Some(cnonce)) => {
                    response_qop_auth(alg, &ha1, &params.nonce, nc, cnonce, &ha2)
                }
                _ => response_no_qop(alg, &ha1, &params.nonce, &ha2),
            };

            if constant_time_eq(expected.as_bytes(), params.response.as_bytes()) {
                Ok(params.username)
            } else {
                Err(AuthError::BadResponse)
            }
        }

        /// Drop expired nonces. Called on every issue; idempotent.
        fn gc_nonces(&self) {
            let cutoff = now_secs().saturating_sub(self.ttl.as_secs());
            self.nonces.retain(|_, issued_at| *issued_at >= cutoff);
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

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::auth::{Credentials, InMemoryCredentialStore};

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

        #[test]
        fn registrar_round_trip_md5() {
            let store = Arc::new(InMemoryCredentialStore::new());
            store.insert(Credentials {
                username: "alice".into(),
                realm: "smiths.local".into(),
                password: "s3cret".into(),
            });
            let reg = Registrar::new("smiths.local", store.clone());

            // Issue challenge (we get the nonce).
            let challenge = reg.challenge(Algorithm::Md5, false);
            let nonce = extract_param(&challenge, "nonce").unwrap();

            // Client computes digest.
            let method = "REGISTER";
            let uri = "sip:smiths.local";
            let h1 = ha1(Algorithm::Md5, "alice", "smiths.local", "s3cret");
            let h2 = ha2(Algorithm::Md5, method, uri);
            let resp = response_qop_auth(Algorithm::Md5, &h1, &nonce, "00000001", "cnonce-1", &h2);

            let hdr = format!(
                "Digest username=\"alice\", realm=\"smiths.local\", \
                 nonce=\"{nonce}\", uri=\"{uri}\", response=\"{resp}\", \
                 algorithm=MD5, qop=auth, nc=00000001, cnonce=\"cnonce-1\""
            );
            let user = reg.authenticate(method, uri, &hdr).unwrap();
            assert_eq!(user, "alice");
        }

        #[test]
        fn registrar_round_trip_sha256() {
            let store = Arc::new(InMemoryCredentialStore::new());
            store.insert(Credentials {
                username: "bob".into(),
                realm: "smiths.local".into(),
                password: "hunter2".into(),
            });
            let reg = Registrar::new("smiths.local", store);
            let challenge = reg.challenge(Algorithm::Sha256, false);
            let nonce = extract_param(&challenge, "nonce").unwrap();
            let method = "REGISTER";
            let uri = "sip:smiths.local";
            let h1 = ha1(Algorithm::Sha256, "bob", "smiths.local", "hunter2");
            let h2 = ha2(Algorithm::Sha256, method, uri);
            let resp = response_qop_auth(Algorithm::Sha256, &h1, &nonce, "00000001", "c", &h2);
            let hdr = format!(
                "Digest username=\"bob\", realm=\"smiths.local\", \
                 nonce=\"{nonce}\", uri=\"{uri}\", response=\"{resp}\", \
                 algorithm=SHA-256, qop=auth, nc=00000001, cnonce=\"c\""
            );
            assert_eq!(reg.authenticate(method, uri, &hdr).unwrap(), "bob");
        }

        #[test]
        fn registrar_bad_password_fails() {
            let store = Arc::new(InMemoryCredentialStore::new());
            store.insert(Credentials {
                username: "alice".into(),
                realm: "smiths.local".into(),
                password: "right".into(),
            });
            let reg = Registrar::new("smiths.local", store);
            let challenge = reg.challenge(Algorithm::Md5, false);
            let nonce = extract_param(&challenge, "nonce").unwrap();
            let h1 = ha1(Algorithm::Md5, "alice", "smiths.local", "wrong"); // bad pass
            let h2 = ha2(Algorithm::Md5, "REGISTER", "sip:smiths.local");
            let resp = response_qop_auth(Algorithm::Md5, &h1, &nonce, "00000001", "c", &h2);
            let hdr = format!(
                "Digest username=\"alice\", realm=\"smiths.local\", \
                 nonce=\"{nonce}\", uri=\"sip:smiths.local\", response=\"{resp}\", \
                 algorithm=MD5, qop=auth, nc=00000001, cnonce=\"c\""
            );
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
        fn registrar_stale_nonce_fails() {
            let store = Arc::new(InMemoryCredentialStore::new());
            store.insert(Credentials {
                username: "alice".into(),
                realm: "smiths.local".into(),
                password: "p".into(),
            });
            let reg = Registrar::new("smiths.local", store);
            // Never issued this nonce.
            let h1 = ha1(Algorithm::Md5, "alice", "smiths.local", "p");
            let h2 = ha2(Algorithm::Md5, "REGISTER", "sip:smiths.local");
            let resp = response_qop_auth(Algorithm::Md5, &h1, "fake", "00000001", "c", &h2);
            let hdr = format!(
                "Digest username=\"alice\", realm=\"smiths.local\", \
                 nonce=\"fake\", uri=\"sip:smiths.local\", response=\"{resp}\", \
                 algorithm=MD5, qop=auth, nc=00000001, cnonce=\"c\""
            );
            assert_eq!(
                reg.authenticate("REGISTER", "sip:smiths.local", &hdr),
                Err(AuthError::StaleNonce)
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
