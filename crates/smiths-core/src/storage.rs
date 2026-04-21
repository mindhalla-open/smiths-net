//! Pluggable storage traits — the persistence surface every
//! subsequent feature (HA, recording, RAG, presence) targets.
//!
//! Three traits live here:
//!
//! - [`CdrStore`] — Call Detail Records. Written by the UAS on
//!   dialog terminate; read by MCP's `list_cdr` tool.
//! - [`KvStore`] — opaque key/value. Used by session-level caches,
//!   hot-reload snapshots, and anything else that wants persistence
//!   without a schema.
//! - The SIP subscriber-credential trait is already pluggable — see
//!   [`smiths-sip::auth::CredentialStore`][sip-cred] and the
//!   `SqliteAuthStore` / `HttpAuthStore` impls. It stays in the
//!   SIP crate because it's RFC-2617-shaped; this module adds the
//!   generic companion surfaces.
//!
//! Implementations must be `Send + Sync + 'static`. Async is not yet
//! on the trait — the backends in slice 2.3 are all synchronous
//! (`SQLite`). When an async-only backend (cloud object store,
//! remote Postgres via tokio-postgres) lands, we grow an
//! `AsyncCdrStore` variant rather than retrofit.
//!
//! [sip-cred]: https://docs.rs/smiths-sip/latest/smiths_sip/auth/trait.CredentialStore.html

use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors a storage backend can raise.
///
/// Deliberately small — every backend funnels its own error type
/// through `Backend` via `to_string()`. Callers who need backend-
/// specific detail get it by downcasting the underlying error.
#[derive(Debug, Error)]
pub enum StorageError {
    /// Underlying backend rejected the operation (SQL error, HTTP
    /// 5xx, disk-full, etc).
    #[error("storage backend: {0}")]
    Backend(String),
    /// Key / CDR record was not found. Distinct from `Backend` so
    /// callers can use it for idempotent cleanup paths.
    #[error("not found: {0}")]
    NotFound(String),
    /// Input failed validation before any backend call (invalid
    /// timestamp range, negative limit, malformed key).
    #[error("invalid input: {0}")]
    Invalid(String),
}

// ---------------------------------------------------------------------
// KvStore — opaque key/value surface
// ---------------------------------------------------------------------

/// Opaque key/value persistence.
///
/// Scope: coarse-grained, infrequent reads + writes — session-level
/// state, hot-reload snapshots, per-operator configuration overrides.
/// Not designed for per-packet hot paths; RTP / metrics bookkeeping
/// stays in-memory.
///
/// Keys are UTF-8 strings so backends (`SQLite`, Postgres, Redis,
/// plugin) can index them uniformly. Values are opaque bytes — the
/// caller owns serialization.
pub trait KvStore: Send + Sync + 'static {
    /// Retrieve the value at `key`, or `None` if the key is absent.
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError>;

    /// Insert or overwrite `key` → `value`.
    fn put(&self, key: &str, value: &[u8]) -> Result<(), StorageError>;

    /// Remove `key`. Returns `Ok(true)` when a row was deleted,
    /// `Ok(false)` when the key was absent — callers that treat
    /// missing keys as a no-op don't have to catch `NotFound`.
    fn delete(&self, key: &str) -> Result<bool, StorageError>;

    /// List every key whose prefix matches `prefix`. Ordering is
    /// backend-defined; the in-tree `SQLite` impl returns them in
    /// lexical order.
    fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, StorageError>;
}

// ---------------------------------------------------------------------
// CdrStore — call detail records
// ---------------------------------------------------------------------

/// One call's detail record — what the UAS writes when a dialog
/// terminates.
///
/// Kept narrow on purpose — only fields the operator billing /
/// audit view genuinely needs. Extending for B2B-style SIP transfer
/// tracking (referral history, codec changes) is a follow-on when
/// we have a concrete consumer asking for it.
///
/// All timestamps are Unix seconds; `duration_secs` is the
/// wall-clock gap between dialog establishment and termination.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CallDetailRecord {
    /// `Call-ID` header value — primary key across the engine.
    pub call_id: String,
    /// From URI (minus tags) — e.g. `sip:bob@example.com`.
    pub from_uri: String,
    /// To URI (minus tags) — e.g. `sip:alice@smiths.local`.
    pub to_uri: String,
    /// Unix seconds at dialog establishment (first 200 OK).
    pub started_at_unix: i64,
    /// Unix seconds at dialog termination (BYE observed). Equal to
    /// `started_at_unix` for the edge case where BYE arrives during
    /// the same second — `duration_secs` is still 0.
    pub ended_at_unix: i64,
    /// Wall-clock duration in seconds. Stored (not derived) so
    /// operators can index on it without a computed column.
    pub duration_secs: i64,
    /// Terminal outcome. Normalized strings (`"answered"`, `"cancelled"`,
    /// `"rejected:488"`, `"remote_bye"`) so dashboards can group.
    pub result: String,
}

impl CallDetailRecord {
    /// Convenience builder for the common answered-then-BYE path.
    /// Computes `duration_secs` from the start + end timestamps.
    #[must_use]
    pub fn answered(
        call_id: impl Into<String>,
        from_uri: impl Into<String>,
        to_uri: impl Into<String>,
        started_at_unix: i64,
        ended_at_unix: i64,
    ) -> Self {
        let duration_secs = ended_at_unix.saturating_sub(started_at_unix).max(0);
        Self {
            call_id: call_id.into(),
            from_uri: from_uri.into(),
            to_uri: to_uri.into(),
            started_at_unix,
            ended_at_unix,
            duration_secs,
            result: "answered".into(),
        }
    }

    /// Unix-seconds snapshot of the current wall clock. Pulled out
    /// so the UAS doesn't have to hand-roll the `SystemTime` dance.
    #[must_use]
    pub fn now_unix() -> i64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
    }
}

/// Bounded filter for [`CdrStore::list`].
///
/// Every field is `Option` — `None` means "don't filter on this".
/// `limit` is mandatory (defaults to [`CdrFilter::DEFAULT_LIMIT`] via
/// [`CdrFilter::new`]) so a forgetful caller can't accidentally ask
/// for a full-table scan.
#[derive(Clone, Debug, Deserialize, Serialize, Default)]
pub struct CdrFilter {
    /// Only records whose `started_at_unix` is ≥ this value.
    pub since_unix: Option<i64>,
    /// Only records whose `started_at_unix` is ≤ this value.
    pub until_unix: Option<i64>,
    /// Case-insensitive substring match on `from_uri`. Backends that
    /// can push this down should; the `SQLite` impl uses `LIKE`.
    pub from_like: Option<String>,
    /// Case-insensitive substring match on `to_uri`.
    pub to_like: Option<String>,
    /// Exact match on `result` (e.g. `"answered"`).
    pub result: Option<String>,
    /// Max rows to return. Required so a naive caller can't sink the
    /// storage layer.
    pub limit: u32,
}

impl CdrFilter {
    /// Default `limit` applied by [`Self::new`]. Tuned for the MCP
    /// pagination story — renders cleanly in one LLM response.
    pub const DEFAULT_LIMIT: u32 = 100;

    /// Fresh, unfiltered filter capped at [`Self::DEFAULT_LIMIT`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            limit: Self::DEFAULT_LIMIT,
            ..Self::default()
        }
    }
}

/// Persistence for [`CallDetailRecord`] rows.
pub trait CdrStore: Send + Sync + 'static {
    /// Insert a new CDR. Backends should treat `call_id` as the
    /// de-dup key; a second `record` for the same `call_id` is
    /// implementation-defined (`SQLite` impl upserts). Callers that
    /// need strict append-only semantics should key by
    /// `(call_id, ended_at_unix)` externally.
    fn record(&self, cdr: &CallDetailRecord) -> Result<(), StorageError>;

    /// Retrieve up to `filter.limit` records matching `filter`.
    /// Ordering: newest-first by `started_at_unix` so the most
    /// recent call is always on the first page.
    fn list(&self, filter: &CdrFilter) -> Result<Vec<CallDetailRecord>, StorageError>;

    /// Drop every record — primarily for test harness teardown.
    /// Production backends can no-op / return `Invalid` if the
    /// operator shouldn't be able to invoke it from MCP; the `SQLite`
    /// impl honours the call because it's the same DB the tests
    /// live against.
    fn truncate(&self) -> Result<(), StorageError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cdr_answered_builder_computes_duration() {
        let cdr = CallDetailRecord::answered(
            "call-1",
            "sip:bob@x",
            "sip:alice@y",
            1_700_000_000,
            1_700_000_042,
        );
        assert_eq!(cdr.duration_secs, 42);
        assert_eq!(cdr.result, "answered");
    }

    #[test]
    fn cdr_answered_clamps_negative_duration_to_zero() {
        // BYE arriving before established (clock skew) shouldn't
        // produce a negative duration column.
        let cdr = CallDetailRecord::answered("call-1", "sip:bob@x", "sip:alice@y", 100, 50);
        assert_eq!(cdr.duration_secs, 0);
    }

    #[test]
    fn cdr_filter_new_applies_default_limit() {
        let f = CdrFilter::new();
        assert_eq!(f.limit, CdrFilter::DEFAULT_LIMIT);
        assert!(f.since_unix.is_none());
        assert!(f.from_like.is_none());
    }

    #[test]
    fn cdr_now_unix_is_monotonic_nonzero() {
        let a = CallDetailRecord::now_unix();
        let b = CallDetailRecord::now_unix();
        assert!(a > 0);
        assert!(b >= a);
    }
}
