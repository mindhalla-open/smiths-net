//! Embedded-SQLite backend for [`CredentialStore`] + [`RegistrationStore`].
//!
//! Compiled only when `--features auth-sqlite` is on (the default for
//! `smiths-sip`). The `bundled` feature on `rusqlite` links sqlite3.c
//! directly — no system libsqlite3 required, which keeps the
//! single-binary distribution story intact.
//!
//! ## Three-table schema (slice 2.1 spec)
//!
//! ```sql
//! CREATE TABLE realms (
//!     id   INTEGER PRIMARY KEY,
//!     name TEXT    NOT NULL UNIQUE
//! );
//! CREATE TABLE users (
//!     id       INTEGER PRIMARY KEY,
//!     realm_id INTEGER NOT NULL REFERENCES realms(id) ON DELETE CASCADE,
//!     username TEXT    NOT NULL,
//!     password TEXT    NOT NULL,
//!     UNIQUE(realm_id, username)
//! );
//! CREATE TABLE contacts (
//!     id              INTEGER PRIMARY KEY,
//!     aor             TEXT    NOT NULL,
//!     contact         TEXT    NOT NULL,
//!     expires_at_unix INTEGER NOT NULL,
//!     UNIQUE(aor, contact)
//! );
//! ```
//!
//! A single internal `_schema_version` table tracks applied
//! migrations. [`MigrationRunner`] is idempotent: opening an already-
//! migrated DB is a no-op; opening an older DB runs the missing
//! steps in order and logs each as it applies.
//!
//! ## Threading model
//!
//! `rusqlite::Connection` is `Send + !Sync`. We wrap it in a
//! `std::sync::Mutex` and serialize every call. Registration traffic
//! is nowhere near the hot path (one DB hit per REGISTER, not per
//! packet) so lock contention is a non-issue; a connection-pool
//! optimization would be premature.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension as _, params};
use smiths_core::{RegistrationSnapshot, RegistrationView};
use thiserror::Error;
use tracing::{debug, info};

use super::{
    Binding, CredentialStore, Credentials, RegistrationError, RegistrationStore,
};

/// Errors raised while opening / migrating / reading the `SQLite` store.
#[derive(Debug, Error)]
pub enum SqliteStoreError {
    /// Underlying `rusqlite` error — carried as a string so the enum
    /// doesn't leak the dependency through its public surface.
    #[error("sqlite: {0}")]
    Sqlite(String),
    /// The schema version on disk is higher than this binary knows
    /// how to read. Fatal — operator needs to upgrade the engine.
    #[error("schema version {found} is newer than supported {max_supported}")]
    SchemaTooNew {
        /// What we read off disk.
        found: i32,
        /// Highest version this binary understands.
        max_supported: i32,
    },
}

impl From<rusqlite::Error> for SqliteStoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e.to_string())
    }
}

impl From<SqliteStoreError> for RegistrationError {
    fn from(e: SqliteStoreError) -> Self {
        Self::Backend(e.to_string())
    }
}

/// Highest schema version this binary knows how to create / read.
/// Bump when adding a migration to [`MIGRATIONS`].
pub const CURRENT_SCHEMA_VERSION: i32 = 1;

/// Ordered list of migration steps. Index 0 is applied first. Each
/// step is run inside a single transaction; a failure rolls back
/// cleanly and leaves `_schema_version` at its previous value.
const MIGRATIONS: &[(&str, i32)] = &[(
    "v1 — users / realms / contacts",
    // All four tables live in one migration because the FK graph is
    // mutual: users→realms, contacts are dialog-scoped (AOR+contact)
    // so they don't need a FK, just a unique key. Keeps the v1 bundle
    // atomic and the schema-version bookkeeping trivial.
    1,
)];

/// Idempotent migration runner. Holds no state beyond the current
/// version read off disk; every call to [`Self::run`] is safe to
/// repeat.
pub struct MigrationRunner;

impl MigrationRunner {
    /// Apply every pending migration to `conn`, in order. Logs each
    /// step as it lands. Returns the post-migration schema version.
    pub fn run(conn: &mut Connection) -> Result<i32, SqliteStoreError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS _schema_version (
                 version INTEGER PRIMARY KEY
             );",
        )?;
        let current: i32 = conn
            .query_row(
                "SELECT version FROM _schema_version LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        if current > CURRENT_SCHEMA_VERSION {
            return Err(SqliteStoreError::SchemaTooNew {
                found: current,
                max_supported: CURRENT_SCHEMA_VERSION,
            });
        }

        for (label, target) in MIGRATIONS {
            if current >= *target {
                continue;
            }
            let tx = conn.transaction()?;
            apply_migration(*target, &tx)?;
            tx.execute("DELETE FROM _schema_version", [])?;
            tx.execute(
                "INSERT INTO _schema_version(version) VALUES(?1)",
                params![target],
            )?;
            tx.commit()?;
            info!(version = *target, name = *label, "sqlite auth schema migrated");
        }

        Ok(CURRENT_SCHEMA_VERSION)
    }
}

fn apply_migration(target: i32, tx: &rusqlite::Transaction<'_>) -> Result<(), SqliteStoreError> {
    match target {
        1 => {
            tx.execute_batch(
                "CREATE TABLE realms (
                     id   INTEGER PRIMARY KEY,
                     name TEXT    NOT NULL UNIQUE
                 );
                 CREATE TABLE users (
                     id       INTEGER PRIMARY KEY,
                     realm_id INTEGER NOT NULL REFERENCES realms(id) ON DELETE CASCADE,
                     username TEXT    NOT NULL,
                     password TEXT    NOT NULL,
                     UNIQUE(realm_id, username)
                 );
                 CREATE TABLE contacts (
                     id              INTEGER PRIMARY KEY,
                     aor             TEXT    NOT NULL,
                     contact         TEXT    NOT NULL,
                     expires_at_unix INTEGER NOT NULL,
                     UNIQUE(aor, contact)
                 );
                 CREATE INDEX contacts_by_aor ON contacts(aor);
                 CREATE INDEX contacts_by_expiry ON contacts(expires_at_unix);",
            )?;
        }
        other => {
            return Err(SqliteStoreError::Sqlite(format!(
                "unknown migration target v{other}"
            )));
        }
    }
    Ok(())
}

/// Embedded-SQLite backing store.
///
/// Cheap to `Arc`-clone; every call serializes through the inner
/// `Mutex<Connection>`. Not `Clone` itself — share via `Arc`.
pub struct SqliteAuthStore {
    conn: Mutex<Connection>,
    /// Path the store was opened at. Kept for diagnostics + the
    /// `sip://registrations` MCP resource, which surfaces it so
    /// operators know where bindings live.
    path: PathBuf,
}

impl SqliteAuthStore {
    /// Open (or create) a store at `path`, then run any pending
    /// migrations. An empty file is treated the same as a missing
    /// file — the runner creates the schema from scratch.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, SqliteStoreError> {
        let path = path.as_ref().to_path_buf();
        let mut conn = Connection::open(&path)?;
        // Foreign keys are off by default in SQLite; turn them on so
        // `REFERENCES realms(id) ON DELETE CASCADE` actually fires.
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        MigrationRunner::run(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            path,
        })
    }

    /// Open an in-memory store. Primarily for tests; operators should
    /// always use [`Self::open`] with a persistent path.
    pub fn open_in_memory() -> Result<Self, SqliteStoreError> {
        let mut conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        MigrationRunner::run(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            path: PathBuf::from(":memory:"),
        })
    }

    /// Path the store was opened at. `:memory:` for in-memory stores.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Provision a user. Creates the realm row if it doesn't exist.
    /// Operators will typically seed the DB via this API (or a later
    /// CLI subcommand); there's no "register through SIP" flow for
    /// credentials — REGISTER authenticates, it doesn't provision.
    pub fn upsert_user(&self, creds: &Credentials) -> Result<(), SqliteStoreError> {
        let mut conn = self.conn.lock().expect("sqlite auth store poisoned");
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO realms(name) VALUES(?1)",
            params![creds.realm],
        )?;
        tx.execute(
            "INSERT INTO users(realm_id, username, password)
             VALUES((SELECT id FROM realms WHERE name = ?1), ?2, ?3)
             ON CONFLICT(realm_id, username) DO UPDATE SET password = excluded.password",
            params![creds.realm, creds.username, creds.password],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Delete a user. Idempotent — missing rows don't error.
    pub fn delete_user(&self, realm: &str, username: &str) -> Result<(), SqliteStoreError> {
        let conn = self.conn.lock().expect("sqlite auth store poisoned");
        conn.execute(
            "DELETE FROM users
             WHERE username = ?1
               AND realm_id = (SELECT id FROM realms WHERE name = ?2)",
            params![username, realm],
        )?;
        Ok(())
    }

    /// Garbage-collect every binding whose expiry is already past.
    /// Called before serving `snapshot()` so MCP readers never see
    /// stale rows; operators can also invoke it directly from a
    /// scheduled job.
    pub fn gc_expired(&self, now_unix: i64) -> Result<usize, SqliteStoreError> {
        let conn = self.conn.lock().expect("sqlite auth store poisoned");
        let removed = conn.execute(
            "DELETE FROM contacts WHERE expires_at_unix <= ?1",
            params![now_unix],
        )?;
        if removed > 0 {
            debug!(removed, "gc'd expired SIP registrations");
        }
        Ok(removed)
    }
}

impl CredentialStore for SqliteAuthStore {
    fn lookup(&self, realm: &str, username: &str) -> Option<Credentials> {
        let conn = self.conn.lock().expect("sqlite auth store poisoned");
        conn.query_row(
            "SELECT u.username, r.name, u.password
             FROM users u
             JOIN realms r ON r.id = u.realm_id
             WHERE r.name = ?1 AND u.username = ?2",
            params![realm, username],
            |row| {
                Ok(Credentials {
                    username: row.get(0)?,
                    realm: row.get(1)?,
                    password: row.get(2)?,
                    ha1: None,
                })
            },
        )
        .optional()
        .ok()
        .flatten()
    }
}

impl RegistrationStore for SqliteAuthStore {
    fn bind(&self, binding: &Binding) -> Result<Binding, RegistrationError> {
        let conn = self.conn.lock().expect("sqlite auth store poisoned");
        conn.execute(
            "INSERT INTO contacts(aor, contact, expires_at_unix)
             VALUES(?1, ?2, ?3)
             ON CONFLICT(aor, contact) DO UPDATE
                 SET expires_at_unix = excluded.expires_at_unix",
            params![binding.aor, binding.contact, binding.expires_at_unix],
        )
        .map_err(|e| RegistrationError::Backend(e.to_string()))?;
        Ok(binding.clone())
    }

    fn unbind(&self, aor: &str, contact: &str) -> Result<(), RegistrationError> {
        let conn = self.conn.lock().expect("sqlite auth store poisoned");
        let removed = conn
            .execute(
                "DELETE FROM contacts WHERE aor = ?1 AND contact = ?2",
                params![aor, contact],
            )
            .map_err(|e| RegistrationError::Backend(e.to_string()))?;
        if removed == 0 {
            return Err(RegistrationError::UnknownAor(aor.to_owned()));
        }
        Ok(())
    }

    fn lookup_bindings(&self, aor: &str) -> Result<Vec<Binding>, RegistrationError> {
        let now = crate::auth::unix_now_secs();
        let conn = self.conn.lock().expect("sqlite auth store poisoned");
        let mut stmt = conn
            .prepare(
                "SELECT aor, contact, expires_at_unix
                 FROM contacts
                 WHERE aor = ?1 AND expires_at_unix > ?2",
            )
            .map_err(|e| RegistrationError::Backend(e.to_string()))?;
        let rows = stmt
            .query_map(params![aor, now], |row| {
                Ok(Binding {
                    aor: row.get(0)?,
                    contact: row.get(1)?,
                    expires_at_unix: row.get(2)?,
                })
            })
            .map_err(|e| RegistrationError::Backend(e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| RegistrationError::Backend(e.to_string()))?);
        }
        Ok(out)
    }

    fn snapshot(&self) -> Result<Vec<Binding>, RegistrationError> {
        let now = crate::auth::unix_now_secs();
        let conn = self.conn.lock().expect("sqlite auth store poisoned");
        let mut stmt = conn
            .prepare(
                "SELECT aor, contact, expires_at_unix
                 FROM contacts
                 WHERE expires_at_unix > ?1
                 ORDER BY aor, contact",
            )
            .map_err(|e| RegistrationError::Backend(e.to_string()))?;
        let rows = stmt
            .query_map(params![now], |row| {
                Ok(Binding {
                    aor: row.get(0)?,
                    contact: row.get(1)?,
                    expires_at_unix: row.get(2)?,
                })
            })
            .map_err(|e| RegistrationError::Backend(e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| RegistrationError::Backend(e.to_string()))?);
        }
        Ok(out)
    }
}

impl RegistrationView for SqliteAuthStore {
    fn snapshot(&self) -> Vec<RegistrationSnapshot> {
        // `RegistrationStore::snapshot` already filters expired rows;
        // errors surface as an empty snapshot for the read-only
        // observability view (operators still have the `Err` path
        // through the store trait for writes).
        RegistrationStore::snapshot(self)
            .unwrap_or_default()
            .into_iter()
            .map(|b| RegistrationSnapshot {
                aor: b.aor,
                contact: b.contact,
                expires_at_unix: b.expires_at_unix,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_creds(user: &str) -> Credentials {
        Credentials::new(user, "smiths.local", "hunter2")
    }

    fn binding(aor: &str, contact: &str, ttl_from_now: i64) -> Binding {
        Binding {
            aor: aor.to_owned(),
            contact: contact.to_owned(),
            expires_at_unix: crate::auth::unix_now_secs() + ttl_from_now,
        }
    }

    #[test]
    fn fresh_store_runs_all_migrations_once() {
        let store = SqliteAuthStore::open_in_memory().unwrap();
        // Second migrate should be a no-op; opening again on the
        // same in-memory DB isn't possible, so rerun on the
        // internal connection.
        let mut guard = store.conn.lock().unwrap();
        let v = MigrationRunner::run(&mut guard).unwrap();
        assert_eq!(v, CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn upsert_and_lookup_round_trip() {
        let store = SqliteAuthStore::open_in_memory().unwrap();
        store.upsert_user(&test_creds("alice")).unwrap();
        let got = store.lookup("smiths.local", "alice").unwrap();
        assert_eq!(got.username, "alice");
        assert_eq!(got.password, "hunter2");
    }

    #[test]
    fn upsert_replaces_password_on_conflict() {
        let store = SqliteAuthStore::open_in_memory().unwrap();
        store.upsert_user(&test_creds("bob")).unwrap();
        let mut new_creds = test_creds("bob");
        new_creds.password = "NEW-secret".into();
        store.upsert_user(&new_creds).unwrap();
        let got = store.lookup("smiths.local", "bob").unwrap();
        assert_eq!(got.password, "NEW-secret");
    }

    #[test]
    fn lookup_unknown_user_returns_none() {
        let store = SqliteAuthStore::open_in_memory().unwrap();
        assert!(store.lookup("smiths.local", "ghost").is_none());
    }

    #[test]
    fn delete_user_is_idempotent() {
        let store = SqliteAuthStore::open_in_memory().unwrap();
        store.upsert_user(&test_creds("temp")).unwrap();
        store.delete_user("smiths.local", "temp").unwrap();
        // Second delete is a no-op; no panic.
        store.delete_user("smiths.local", "temp").unwrap();
        assert!(store.lookup("smiths.local", "temp").is_none());
    }

    #[test]
    fn bind_and_snapshot_round_trip() {
        let store = SqliteAuthStore::open_in_memory().unwrap();
        let b = binding(
            "sip:alice@smiths.local",
            "sip:alice@192.0.2.10:5060",
            3600,
        );
        let stored = store.bind(&b).unwrap();
        assert_eq!(stored, b);

        let snap = RegistrationStore::snapshot(&store).unwrap();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].aor, "sip:alice@smiths.local");
    }

    #[test]
    fn bind_refreshes_expiry_on_same_contact() {
        let store = SqliteAuthStore::open_in_memory().unwrap();
        let original = binding("sip:bob@smiths.local", "sip:bob@1.2.3.4", 60);
        store.bind(&original).unwrap();
        let refreshed = Binding {
            expires_at_unix: original.expires_at_unix + 3600,
            ..original.clone()
        };
        store.bind(&refreshed).unwrap();
        let snap = RegistrationStore::snapshot(&store).unwrap();
        assert_eq!(snap.len(), 1, "rebind must upsert, not duplicate");
        assert_eq!(snap[0].expires_at_unix, refreshed.expires_at_unix);
    }

    #[test]
    fn lookup_bindings_filters_expired() {
        let store = SqliteAuthStore::open_in_memory().unwrap();
        let live = binding("sip:c@smiths.local", "sip:c@live", 3600);
        let stale = Binding {
            expires_at_unix: crate::auth::unix_now_secs() - 10,
            ..binding("sip:c@smiths.local", "sip:c@stale", 0)
        };
        store.bind(&live).unwrap();
        store.bind(&stale).unwrap();
        let hits = store.lookup_bindings("sip:c@smiths.local").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].contact, "sip:c@live");
    }

    #[test]
    fn unbind_unknown_aor_errors() {
        let store = SqliteAuthStore::open_in_memory().unwrap();
        let err = store.unbind("sip:nobody@smiths.local", "sip:x@y").unwrap_err();
        match err {
            RegistrationError::UnknownAor(aor) => assert_eq!(aor, "sip:nobody@smiths.local"),
            RegistrationError::Backend(_) => panic!("expected UnknownAor, got Backend"),
        }
    }

    #[test]
    fn gc_expired_removes_stale_rows_only() {
        let store = SqliteAuthStore::open_in_memory().unwrap();
        let live = binding("sip:a@r", "sip:a@live", 3600);
        let stale = Binding {
            expires_at_unix: crate::auth::unix_now_secs() - 10,
            ..binding("sip:a@r", "sip:a@stale", 0)
        };
        store.bind(&live).unwrap();
        store.bind(&stale).unwrap();
        let removed = store.gc_expired(crate::auth::unix_now_secs()).unwrap();
        assert_eq!(removed, 1);
        let snap = RegistrationStore::snapshot(&store).unwrap();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].contact, "sip:a@live");
    }

    #[test]
    fn persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.db");
        {
            let store = SqliteAuthStore::open(&path).unwrap();
            store.upsert_user(&test_creds("persist")).unwrap();
            store
                .bind(&binding("sip:persist@r", "sip:persist@1.1.1.1", 3600))
                .unwrap();
        }
        let reopened = SqliteAuthStore::open(&path).unwrap();
        assert!(reopened.lookup("smiths.local", "persist").is_some());
        assert_eq!(RegistrationStore::snapshot(&reopened).unwrap().len(), 1);
    }
}
