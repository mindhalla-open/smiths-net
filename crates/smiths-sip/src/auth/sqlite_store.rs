//! Embedded-SQLite backend for [`CredentialStore`] + [`RegistrationStore`].
//!
//! Compiled only when `--features auth-sqlite` is on (the default for
//! `smiths-sip`). The `bundled` feature on `rusqlite` links sqlite3.c
//! directly — no system libsqlite3 required, which keeps the
//! single-binary distribution story intact.
//!
//! ## Schema
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
//!     ha1_md5    TEXT    NOT NULL,   -- hex MD5(user:realm:password)
//!     ha1_sha256 TEXT    NOT NULL,   -- hex SHA-256(user:realm:password)
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
//! plus the `cdr` / `kv` tables that back `smiths_core::storage`.
//!
//! Passwords are never stored: `users` keeps only the two digest
//! HA1 hashes, one per supported algorithm, so a leaked database
//! file yields nothing reusable outside this realm. Lookups return
//! [`Credentials`] in HA1 form for the algorithm the registrar asked
//! for ([`CredentialStore::lookup_for`]).
//!
//! A single internal `_schema_version` table tracks applied
//! migrations. [`MigrationRunner`] is idempotent: opening an already-
//! migrated DB is a no-op; opening an older DB runs the missing
//! steps in order and logs each as it applies. The v3 step hashes
//! any plaintext rows left by an older schema and drops the
//! `password` column.
//!
//! ## Threading model
//!
//! `rusqlite::Connection` is `Send + !Sync`. We wrap it in a
//! `std::sync::Mutex` and serialize every call. Registration traffic
//! is nowhere near the hot path (one DB hit per REGISTER, not per
//! packet) so lock contention is a non-issue; a connection-pool
//! optimization would be premature.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use rusqlite::{Connection, OptionalExtension as _, params};
use smiths_core::storage::{CallDetailRecord, CdrFilter, CdrStore, KvStore, StorageError};
use smiths_core::{RegistrationSnapshot, RegistrationView};
use thiserror::Error;
use tracing::{debug, info};

use super::digest::{Algorithm, ha1};
use super::{Binding, CredentialStore, Credentials, RegistrationError, RegistrationStore};

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
    /// [`SqliteAuthStore::upsert_user`] was handed HA1-only
    /// credentials. The store keeps one hash per algorithm and cannot
    /// derive the second from a single opaque HA1; use
    /// [`SqliteAuthStore::upsert_user_ha1`] with both hashes instead.
    #[error(
        "sqlite auth store needs a plaintext password (or both HA1 hashes) to provision a user"
    )]
    PlaintextRequired,
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
pub const CURRENT_SCHEMA_VERSION: i32 = 3;

/// Ordered list of migration steps. Index 0 is applied first. Each
/// step is run inside a single transaction; a failure rolls back
/// cleanly and leaves `_schema_version` at its previous value.
const MIGRATIONS: &[(&str, i32)] = &[
    (
        "v1 — users / realms / contacts",
        // All four tables live in one migration because the FK graph is
        // mutual: users→realms, contacts are dialog-scoped (AOR+contact)
        // so they don't need a FK, just a unique key. Keeps the v1 bundle
        // atomic and the schema-version bookkeeping trivial.
        1,
    ),
    (
        "v2 — cdr / kv (slice 2.3: pluggable storage)",
        // Adds the generic CDR + KV tables so the same DB file can
        // back `CdrStore` + `KvStore` from `smiths-core::storage`.
        // Operators typically point `[auth.sqlite]` and
        // `[storage.sqlite]` at the same path; separate paths work
        // too (each file migrates independently — the unused tables
        // just sit empty).
        2,
    ),
    (
        "v3 — users store HA1 hashes instead of plaintext passwords",
        // Rebuilds `users` with `ha1_md5` / `ha1_sha256` columns,
        // hashing every existing plaintext row once, and drops the
        // `password` column.
        3,
    ),
];

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
            .query_row("SELECT version FROM _schema_version LIMIT 1", [], |r| {
                r.get(0)
            })
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
            info!(
                version = *target,
                name = *label,
                "sqlite auth schema migrated"
            );
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
        2 => {
            tx.execute_batch(
                "CREATE TABLE cdr (
                     call_id         TEXT    PRIMARY KEY,
                     from_uri        TEXT    NOT NULL,
                     to_uri          TEXT    NOT NULL,
                     started_at_unix INTEGER NOT NULL,
                     ended_at_unix   INTEGER NOT NULL,
                     duration_secs   INTEGER NOT NULL,
                     result          TEXT    NOT NULL
                 );
                 CREATE INDEX cdr_by_started ON cdr(started_at_unix DESC);
                 CREATE INDEX cdr_by_result  ON cdr(result);
                 CREATE TABLE kv (
                     key   TEXT PRIMARY KEY,
                     value BLOB NOT NULL
                 );",
            )?;
        }
        3 => migrate_users_to_ha1(tx)?,
        other => {
            return Err(SqliteStoreError::Sqlite(format!(
                "unknown migration target v{other}"
            )));
        }
    }
    Ok(())
}

/// v3: replace the plaintext `password` column with per-algorithm
/// HA1 hashes. Existing rows are hashed in Rust (`SQLite` has no
/// digest functions) inside the migration's transaction, then the
/// old table is dropped and the rebuilt one takes its name.
fn migrate_users_to_ha1(tx: &rusqlite::Transaction<'_>) -> Result<(), SqliteStoreError> {
    tx.execute_batch(
        "CREATE TABLE users_v3 (
             id         INTEGER PRIMARY KEY,
             realm_id   INTEGER NOT NULL REFERENCES realms(id) ON DELETE CASCADE,
             username   TEXT    NOT NULL,
             ha1_md5    TEXT    NOT NULL,
             ha1_sha256 TEXT    NOT NULL,
             UNIQUE(realm_id, username)
         );",
    )?;
    let rows: Vec<(i64, i64, String, String, String)> = {
        let mut stmt = tx.prepare(
            "SELECT u.id, u.realm_id, r.name, u.username, u.password
             FROM users u JOIN realms r ON r.id = u.realm_id",
        )?;
        let mapped = stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?;
        mapped.collect::<Result<_, _>>()?
    };
    for (id, realm_id, realm, username, password) in &rows {
        tx.execute(
            "INSERT INTO users_v3(id, realm_id, username, ha1_md5, ha1_sha256)
             VALUES(?1, ?2, ?3, ?4, ?5)",
            params![
                id,
                realm_id,
                username,
                ha1(Algorithm::Md5, username, realm, password),
                ha1(Algorithm::Sha256, username, realm, password),
            ],
        )?;
    }
    tx.execute_batch("DROP TABLE users; ALTER TABLE users_v3 RENAME TO users;")?;
    if !rows.is_empty() {
        info!(
            users = rows.len(),
            "sqlite auth: hashed plaintext passwords into HA1 columns"
        );
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

    /// Serialize access to the connection. A poisoned mutex means a
    /// caller panicked mid-statement; `rusqlite` rolls back any open
    /// transaction when its guard drops, so the connection itself is
    /// still consistent and safe to keep using.
    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Provision a user from plaintext credentials. Creates the realm
    /// row if it doesn't exist. Only the MD5 and SHA-256 HA1 hashes
    /// are written — the password itself never touches disk.
    /// Operators will typically seed the DB via this API (or a CLI
    /// subcommand); there's no "register through SIP" flow for
    /// credentials — REGISTER authenticates, it doesn't provision.
    ///
    /// Fails with [`SqliteStoreError::PlaintextRequired`] for
    /// HA1-only credentials; use [`Self::upsert_user_ha1`] for those.
    pub fn upsert_user(&self, creds: &Credentials) -> Result<(), SqliteStoreError> {
        if creds.ha1.is_some() && creds.password.is_empty() {
            return Err(SqliteStoreError::PlaintextRequired);
        }
        let md5 = ha1(
            Algorithm::Md5,
            &creds.username,
            &creds.realm,
            &creds.password,
        );
        let sha256 = ha1(
            Algorithm::Sha256,
            &creds.username,
            &creds.realm,
            &creds.password,
        );
        self.upsert_user_ha1(&creds.realm, &creds.username, &md5, &sha256)
    }

    /// Provision a user from pre-computed hashes: `ha1_md5` is
    /// `MD5(user:realm:password)` and `ha1_sha256` its SHA-256 twin,
    /// both hex. For operators whose provisioning pipeline never
    /// handles plaintext.
    pub fn upsert_user_ha1(
        &self,
        realm: &str,
        username: &str,
        ha1_md5: &str,
        ha1_sha256: &str,
    ) -> Result<(), SqliteStoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO realms(name) VALUES(?1)",
            params![realm],
        )?;
        tx.execute(
            "INSERT INTO users(realm_id, username, ha1_md5, ha1_sha256)
             VALUES((SELECT id FROM realms WHERE name = ?1), ?2, ?3, ?4)
             ON CONFLICT(realm_id, username) DO UPDATE
                 SET ha1_md5 = excluded.ha1_md5,
                     ha1_sha256 = excluded.ha1_sha256",
            params![realm, username, ha1_md5, ha1_sha256],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Delete a user. Idempotent — missing rows don't error.
    pub fn delete_user(&self, realm: &str, username: &str) -> Result<(), SqliteStoreError> {
        let conn = self.conn();
        conn.execute(
            "DELETE FROM users
             WHERE username = ?1
               AND realm_id = (SELECT id FROM realms WHERE name = ?2)",
            params![username, realm],
        )?;
        Ok(())
    }

    /// Garbage-collect every binding whose expiry is already past.
    /// Called before serving `snapshot` so MCP readers never see
    /// stale rows; operators can also invoke it directly from a
    /// scheduled job.
    pub fn gc_expired(&self, now_unix: i64) -> Result<usize, SqliteStoreError> {
        let conn = self.conn();
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
        self.lookup_for(realm, username, Algorithm::Md5)
    }

    fn lookup_for(&self, realm: &str, username: &str, algorithm: Algorithm) -> Option<Credentials> {
        let column = match algorithm {
            Algorithm::Md5 => "u.ha1_md5",
            Algorithm::Sha256 => "u.ha1_sha256",
        };
        let sql = format!(
            "SELECT u.username, r.name, {column}
             FROM users u
             JOIN realms r ON r.id = u.realm_id
             WHERE r.name = ?1 AND u.username = ?2"
        );
        let conn = self.conn();
        conn.query_row(&sql, params![realm, username], |row| {
            let username: String = row.get(0)?;
            let realm: String = row.get(1)?;
            let ha1: String = row.get(2)?;
            Ok(Credentials::from_ha1(username, realm, ha1))
        })
        .optional()
        .ok()
        .flatten()
    }
}

impl RegistrationStore for SqliteAuthStore {
    fn bind(&self, binding: &Binding) -> Result<Binding, RegistrationError> {
        let conn = self.conn();
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
        let conn = self.conn();
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
        let conn = self.conn();
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
        let conn = self.conn();
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

/// Map any `rusqlite::Error` into the generic `StorageError` so the
/// `CdrStore` + `KvStore` impls don't leak the dep through their
/// signatures.
fn to_storage(err: &rusqlite::Error) -> StorageError {
    StorageError::Backend(err.to_string())
}

impl CdrStore for SqliteAuthStore {
    fn record(&self, cdr: &CallDetailRecord) -> Result<(), StorageError> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO cdr(call_id, from_uri, to_uri, started_at_unix,
                             ended_at_unix, duration_secs, result)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(call_id) DO UPDATE SET
                 from_uri        = excluded.from_uri,
                 to_uri          = excluded.to_uri,
                 started_at_unix = excluded.started_at_unix,
                 ended_at_unix   = excluded.ended_at_unix,
                 duration_secs   = excluded.duration_secs,
                 result          = excluded.result",
            params![
                cdr.call_id,
                cdr.from_uri,
                cdr.to_uri,
                cdr.started_at_unix,
                cdr.ended_at_unix,
                cdr.duration_secs,
                cdr.result,
            ],
        )
        .map(|_| ())
        .map_err(|e| to_storage(&e))
    }

    fn list(&self, filter: &CdrFilter) -> Result<Vec<CallDetailRecord>, StorageError> {
        if filter.limit == 0 {
            return Err(StorageError::Invalid("limit must be > 0".into()));
        }
        // Build the WHERE clause dynamically but keep it prepared —
        // string-interpolate only the column list + fixed predicates;
        // every user-supplied value goes through `params`.
        let mut sql = String::from(
            "SELECT call_id, from_uri, to_uri, started_at_unix,
                    ended_at_unix, duration_secs, result
             FROM cdr WHERE 1=1",
        );
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(since) = filter.since_unix {
            sql.push_str(" AND started_at_unix >= ?");
            args.push(Box::new(since));
        }
        if let Some(until) = filter.until_unix {
            sql.push_str(" AND started_at_unix <= ?");
            args.push(Box::new(until));
        }
        if let Some(ref fl) = filter.from_like {
            sql.push_str(" AND from_uri LIKE ?");
            args.push(Box::new(format!("%{fl}%")));
        }
        if let Some(ref tl) = filter.to_like {
            sql.push_str(" AND to_uri LIKE ?");
            args.push(Box::new(format!("%{tl}%")));
        }
        if let Some(ref res) = filter.result {
            sql.push_str(" AND result = ?");
            args.push(Box::new(res.clone()));
        }
        sql.push_str(" ORDER BY started_at_unix DESC LIMIT ?");
        args.push(Box::new(i64::from(filter.limit)));

        let conn = self.conn();
        let mut stmt = conn.prepare(&sql).map_err(|e| to_storage(&e))?;
        let param_refs: Vec<&dyn rusqlite::ToSql> =
            args.iter().map(std::convert::AsRef::as_ref).collect();
        let rows = stmt
            .query_map(param_refs.as_slice(), |row| {
                Ok(CallDetailRecord {
                    call_id: row.get(0)?,
                    from_uri: row.get(1)?,
                    to_uri: row.get(2)?,
                    started_at_unix: row.get(3)?,
                    ended_at_unix: row.get(4)?,
                    duration_secs: row.get(5)?,
                    result: row.get(6)?,
                })
            })
            .map_err(|e| to_storage(&e))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| to_storage(&e))?);
        }
        Ok(out)
    }

    fn truncate(&self) -> Result<(), StorageError> {
        let conn = self.conn();
        conn.execute("DELETE FROM cdr", [])
            .map_err(|e| to_storage(&e))?;
        Ok(())
    }
}

impl KvStore for SqliteAuthStore {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
        let conn = self.conn();
        conn.query_row("SELECT value FROM kv WHERE key = ?1", params![key], |row| {
            row.get::<_, Vec<u8>>(0)
        })
        .optional()
        .map_err(|e| to_storage(&e))
    }

    fn put(&self, key: &str, value: &[u8]) -> Result<(), StorageError> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO kv(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )
        .map_err(|e| to_storage(&e))?;
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<bool, StorageError> {
        let conn = self.conn();
        let removed = conn
            .execute("DELETE FROM kv WHERE key = ?1", params![key])
            .map_err(|e| to_storage(&e))?;
        Ok(removed > 0)
    }

    fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, StorageError> {
        // `LIKE 'prefix%'` with `%` and `_` in the user-supplied
        // prefix would be over-permissive; escape them so callers
        // can pass arbitrary byte strings safely.
        let escaped = prefix
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("{escaped}%");
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT key FROM kv WHERE key LIKE ?1 ESCAPE '\\' ORDER BY key")
            .map_err(|e| to_storage(&e))?;
        let rows = stmt
            .query_map(params![pattern], |row| row.get::<_, String>(0))
            .map_err(|e| to_storage(&e))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| to_storage(&e))?);
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

    use std::sync::Arc;

    #[test]
    fn fresh_store_runs_all_migrations_once() {
        let store = SqliteAuthStore::open_in_memory().unwrap();
        // Second migrate should be a no-op; opening again on the
        // same in-memory DB isn't possible, so rerun on the
        // internal connection.
        let mut guard = store.conn();
        let v = MigrationRunner::run(&mut guard).unwrap();
        assert_eq!(v, CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn upsert_and_lookup_round_trip_returns_ha1_not_plaintext() {
        let store = SqliteAuthStore::open_in_memory().unwrap();
        store.upsert_user(&test_creds("alice")).unwrap();
        let got = store.lookup("smiths.local", "alice").unwrap();
        assert_eq!(got.username, "alice");
        assert_eq!(got.realm, "smiths.local");
        assert!(got.password.is_empty(), "plaintext must never come back");
        assert_eq!(
            got.ha1.as_deref(),
            Some(ha1(Algorithm::Md5, "alice", "smiths.local", "hunter2").as_str()),
            "plain lookup serves the MD5 hash"
        );
        let sha = store
            .lookup_for("smiths.local", "alice", Algorithm::Sha256)
            .unwrap();
        assert_eq!(
            sha.ha1.as_deref(),
            Some(ha1(Algorithm::Sha256, "alice", "smiths.local", "hunter2").as_str())
        );
    }

    #[test]
    fn upsert_replaces_hashes_on_conflict() {
        let store = SqliteAuthStore::open_in_memory().unwrap();
        store.upsert_user(&test_creds("bob")).unwrap();
        let mut new_creds = test_creds("bob");
        new_creds.password = "NEW-secret".into();
        store.upsert_user(&new_creds).unwrap();
        let got = store.lookup("smiths.local", "bob").unwrap();
        assert_eq!(
            got.ha1.as_deref(),
            Some(ha1(Algorithm::Md5, "bob", "smiths.local", "NEW-secret").as_str())
        );
    }

    #[test]
    fn upsert_rejects_ha1_only_credentials() {
        let store = SqliteAuthStore::open_in_memory().unwrap();
        let err = store
            .upsert_user(&Credentials::from_ha1("eve", "smiths.local", "deadbeef"))
            .unwrap_err();
        assert!(matches!(err, SqliteStoreError::PlaintextRequired));
    }

    #[test]
    fn upsert_user_ha1_stores_both_hashes_verbatim() {
        let store = SqliteAuthStore::open_in_memory().unwrap();
        store
            .upsert_user_ha1("smiths.local", "carol", "md5hash", "sha256hash")
            .unwrap();
        assert_eq!(
            store
                .lookup_for("smiths.local", "carol", Algorithm::Md5)
                .unwrap()
                .ha1
                .as_deref(),
            Some("md5hash")
        );
        assert_eq!(
            store
                .lookup_for("smiths.local", "carol", Algorithm::Sha256)
                .unwrap()
                .ha1
                .as_deref(),
            Some("sha256hash")
        );
    }

    #[test]
    fn password_column_is_gone_after_migration() {
        let store = SqliteAuthStore::open_in_memory().unwrap();
        let columns = user_columns(&store.conn());
        assert!(!columns.iter().any(|c| c == "password"), "{columns:?}");
        assert!(columns.iter().any(|c| c == "ha1_md5"));
        assert!(columns.iter().any(|c| c == "ha1_sha256"));
    }

    /// Column names of the `users` table.
    fn user_columns(conn: &Connection) -> Vec<String> {
        let mut stmt = conn.prepare("PRAGMA table_info(users)").unwrap();
        stmt.query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn v2_plaintext_rows_are_hashed_on_upgrade() {
        // Hand-build a v2 database (plaintext `password` column) with
        // two users, then open it through the store: the v3 step must
        // hash both rows, keep the ids, and drop the plaintext.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE _schema_version (version INTEGER PRIMARY KEY);
                 INSERT INTO _schema_version(version) VALUES(2);
                 CREATE TABLE realms (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
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
                 CREATE TABLE cdr (
                     call_id TEXT PRIMARY KEY, from_uri TEXT NOT NULL, to_uri TEXT NOT NULL,
                     started_at_unix INTEGER NOT NULL, ended_at_unix INTEGER NOT NULL,
                     duration_secs INTEGER NOT NULL, result TEXT NOT NULL
                 );
                 CREATE TABLE kv (key TEXT PRIMARY KEY, value BLOB NOT NULL);
                 INSERT INTO realms(id, name) VALUES(1, 'smiths.local');
                 INSERT INTO realms(id, name) VALUES(2, 'other.example');
                 INSERT INTO users(id, realm_id, username, password) VALUES(7, 1, 'alice', 'pw-a');
                 INSERT INTO users(id, realm_id, username, password) VALUES(9, 2, 'bob', 'pw-b');",
            )
            .unwrap();
        }

        let store = SqliteAuthStore::open(&path).unwrap();
        let columns = user_columns(&store.conn());
        assert!(!columns.iter().any(|c| c == "password"), "{columns:?}");

        let alice = store
            .lookup_for("smiths.local", "alice", Algorithm::Md5)
            .unwrap();
        assert_eq!(
            alice.ha1.as_deref(),
            Some(ha1(Algorithm::Md5, "alice", "smiths.local", "pw-a").as_str())
        );
        let bob = store
            .lookup_for("other.example", "bob", Algorithm::Sha256)
            .unwrap();
        assert_eq!(
            bob.ha1.as_deref(),
            Some(ha1(Algorithm::Sha256, "bob", "other.example", "pw-b").as_str())
        );
        // Row ids survive the rebuild.
        let id: i64 = store
            .conn()
            .query_row("SELECT id FROM users WHERE username = 'bob'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(id, 9);
        // Re-opening is a no-op at v3.
        drop(store);
        let reopened = SqliteAuthStore::open(&path).unwrap();
        assert!(reopened.lookup("smiths.local", "alice").is_some());
    }

    #[test]
    fn registrar_authenticates_sha256_against_sqlite_ha1() {
        use crate::auth::digest::{Registrar, ha2, response_qop_auth};

        let store = Arc::new(SqliteAuthStore::open_in_memory().unwrap());
        store.upsert_user(&test_creds("alice")).unwrap();
        let reg = Registrar::new("smiths.local", store);
        for alg in [Algorithm::Md5, Algorithm::Sha256] {
            let challenge = reg.challenge(alg, false);
            let nonce = challenge
                .split("nonce=\"")
                .nth(1)
                .and_then(|s| s.split('"').next())
                .unwrap()
                .to_owned();
            let h1 = ha1(alg, "alice", "smiths.local", "hunter2");
            let h2 = ha2(alg, "REGISTER", "sip:smiths.local");
            let resp = response_qop_auth(alg, &h1, &nonce, "00000001", "cn", &h2);
            let hdr = format!(
                "Digest username=\"alice\", realm=\"smiths.local\", nonce=\"{nonce}\", \
                 uri=\"sip:smiths.local\", response=\"{resp}\", algorithm={}, \
                 qop=auth, nc=00000001, cnonce=\"cn\"",
                alg.as_str()
            );
            assert_eq!(
                reg.authenticate("REGISTER", "sip:smiths.local", &hdr),
                Ok("alice".to_owned()),
                "{alg:?}"
            );
        }
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
        let b = binding("sip:alice@smiths.local", "sip:alice@192.0.2.10:5060", 3600);
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
        let err = store
            .unbind("sip:nobody@smiths.local", "sip:x@y")
            .unwrap_err();
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

    // ---------- : CDR + KV surface ----------

    fn cdr(call_id: &str, started: i64, ended: i64, result: &str) -> CallDetailRecord {
        CallDetailRecord {
            call_id: call_id.into(),
            from_uri: "sip:bob@x".into(),
            to_uri: "sip:alice@y".into(),
            started_at_unix: started,
            ended_at_unix: ended,
            duration_secs: (ended - started).max(0),
            result: result.into(),
        }
    }

    #[test]
    fn cdr_record_and_list_newest_first() {
        let store = SqliteAuthStore::open_in_memory().unwrap();
        store
            .record(&cdr("c-old", 1_700_000_000, 1_700_000_010, "answered"))
            .unwrap();
        store
            .record(&cdr("c-new", 1_700_000_100, 1_700_000_130, "answered"))
            .unwrap();

        let rows = store.list(&CdrFilter::new()).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].call_id, "c-new", "newest must be first");
        assert_eq!(rows[1].call_id, "c-old");
    }

    #[test]
    fn cdr_filter_by_result_and_since() {
        let store = SqliteAuthStore::open_in_memory().unwrap();
        store.record(&cdr("c1", 100, 110, "answered")).unwrap();
        store.record(&cdr("c2", 200, 205, "cancelled")).unwrap();
        store.record(&cdr("c3", 300, 330, "answered")).unwrap();

        let mut f = CdrFilter::new();
        f.result = Some("answered".into());
        f.since_unix = Some(150);
        let rows = store.list(&f).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].call_id, "c3");
    }

    #[test]
    fn cdr_filter_substring_from_to() {
        let store = SqliteAuthStore::open_in_memory().unwrap();
        let mut row = cdr("c-party", 1, 2, "answered");
        row.from_uri = "sip:alice@party.example".into();
        row.to_uri = "sip:bob@example".into();
        store.record(&row).unwrap();

        let mut f = CdrFilter::new();
        f.from_like = Some("party".into());
        assert_eq!(store.list(&f).unwrap().len(), 1);
        let mut f2 = CdrFilter::new();
        f2.from_like = Some("nobody".into());
        assert_eq!(store.list(&f2).unwrap().len(), 0);
    }

    #[test]
    fn cdr_record_upserts_on_call_id_conflict() {
        let store = SqliteAuthStore::open_in_memory().unwrap();
        store.record(&cdr("dup", 1, 2, "answered")).unwrap();
        let mut updated = cdr("dup", 1, 9, "answered");
        updated.duration_secs = 8;
        store.record(&updated).unwrap();
        let rows = store.list(&CdrFilter::new()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ended_at_unix, 9);
        assert_eq!(rows[0].duration_secs, 8);
    }

    #[test]
    fn cdr_list_rejects_zero_limit() {
        let store = SqliteAuthStore::open_in_memory().unwrap();
        let mut f = CdrFilter::new();
        f.limit = 0;
        match store.list(&f) {
            Err(StorageError::Invalid(_)) => {}
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn cdr_truncate_empties_the_table() {
        let store = SqliteAuthStore::open_in_memory().unwrap();
        store.record(&cdr("c1", 1, 2, "answered")).unwrap();
        store.record(&cdr("c2", 3, 4, "answered")).unwrap();
        store.truncate().unwrap();
        assert!(store.list(&CdrFilter::new()).unwrap().is_empty());
    }

    #[test]
    fn kv_round_trip() {
        let store = SqliteAuthStore::open_in_memory().unwrap();
        assert!(store.get("missing").unwrap().is_none());
        store.put("hello", b"world").unwrap();
        assert_eq!(
            store.get("hello").unwrap().as_deref(),
            Some(b"world" as &[u8])
        );
        // Overwrite.
        store.put("hello", b"earth").unwrap();
        assert_eq!(
            store.get("hello").unwrap().as_deref(),
            Some(b"earth" as &[u8])
        );
    }

    #[test]
    fn kv_delete_reports_whether_row_existed() {
        let store = SqliteAuthStore::open_in_memory().unwrap();
        store.put("doomed", b"x").unwrap();
        assert!(store.delete("doomed").unwrap());
        assert!(!store.delete("doomed").unwrap(), "second delete is false");
    }

    #[test]
    fn kv_list_prefix_returns_only_matching_keys_in_order() {
        let store = SqliteAuthStore::open_in_memory().unwrap();
        for k in ["cfg/a", "cfg/b", "cfg/c", "other/x"] {
            store.put(k, b"").unwrap();
        }
        let cfg_keys = store.list_prefix("cfg/").unwrap();
        assert_eq!(cfg_keys, vec!["cfg/a", "cfg/b", "cfg/c"]);
    }

    #[test]
    fn kv_list_prefix_escapes_wildcards() {
        // Without escaping, a prefix of `%` would match every row —
        // the escape-by-hand keeps user-supplied bytes literal.
        let store = SqliteAuthStore::open_in_memory().unwrap();
        store.put("real-prefix/a", b"").unwrap();
        store.put("other/b", b"").unwrap();
        let hits = store.list_prefix("%").unwrap();
        assert!(hits.is_empty(), "literal `%` must not match anything");
    }
}
