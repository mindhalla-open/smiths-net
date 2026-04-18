//! Digest authentication.
//!
//! Phase 1 scope: the `CredentialStore` trait (MVP guardrail — lets the
//! engine plug any backing store: in-memory, `SQLite`, Postgres, LDAP,
//! sidecar plugin) and an in-memory default. Digest challenge/response
//! computation lands in a follow-up pass once REGISTER is wired.

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
