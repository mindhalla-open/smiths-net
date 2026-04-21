//! Pluggable storage traits — the persistence surface every
//! subsequent feature (HA, recording, RAG, presence) targets.
//!
//! Five traits live here:
//!
//! - [`CdrStore`] — Call Detail Records. Written by the UAS on
//!   dialog terminate; read by MCP's `list_cdr` tool.
//! - [`KvStore`] — opaque key/value. Used by session-level caches,
//!   hot-reload snapshots, and anything else that wants persistence
//!   without a schema.
//! - [`VectorStore`] (slice 3.4) — embedding-indexed semantic
//!   search. Backs `search_calls_semantic` and RAG flows.
//! - [`RecordingStore`] (slice 3.4) — per-call audio retention.
//!   Filesystem default ships in-tree; object-store implementations
//!   slot in behind the same trait.
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

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::Value;
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

// ---------------------------------------------------------------------
// VectorStore — slice 3.4 semantic-search surface
// ---------------------------------------------------------------------

/// One record in a [`VectorStore`]. The `id` is the caller's choice
/// of primary key (in practice a SIP `Call-ID` or a stable
/// transcript chunk key). `vector` is the dense embedding; backends
/// are free to reject differently-sized vectors against what they
/// were created with. `metadata` rides through opaque — enough for
/// the common case ("store `call_id`, transcript, `started_at` here,
/// filter on them in the LLM prompt").
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct VectorRecord {
    /// Stable key for upsert / dedup. Empty string is invalid.
    pub id: String,
    /// Dense float vector. Dimensionality must match the backend's
    /// configured one.
    pub vector: Vec<f32>,
    /// Opaque tags the caller wants to retrieve on search hits.
    /// Default is an empty object — callers don't have to set this
    /// just to index a vector.
    #[serde(default)]
    pub metadata: Value,
}

/// One hit from [`VectorStore::search`]. `score` is cosine similarity
/// in `[-1, 1]` for the in-tree impl — sidecar backends (Qdrant,
/// pgvector) may normalize differently; callers should read the
/// backend's docs before attaching semantic meaning to the number
/// beyond "higher is better".
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct VectorHit {
    /// Record id from the matched [`VectorRecord`].
    pub id: String,
    /// Similarity score — higher is better.
    pub score: f32,
    /// Metadata copied from the indexed record.
    #[serde(default)]
    pub metadata: Value,
}

/// Embedding-indexed search surface. Every method is synchronous to
/// match the rest of this module; async-only backends (remote
/// Qdrant, Pinecone) adapt by blocking on a runtime handle in their
/// own impl.
pub trait VectorStore: Send + Sync + 'static {
    /// Insert or overwrite a record keyed by `record.id`.
    fn upsert(&self, record: &VectorRecord) -> Result<(), StorageError>;

    /// Drop a record by id. Returns `Ok(false)` when the id was
    /// absent — idempotent cleanup paths don't have to catch
    /// `NotFound`.
    fn delete(&self, id: &str) -> Result<bool, StorageError>;

    /// Top-k nearest vectors to `query`. `k == 0` is rejected as
    /// `Invalid`; backends cap their own return size independently
    /// when the index is smaller than `k`.
    fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>, StorageError>;

    /// Total record count. Cheap on in-memory impls; sidecar
    /// impls may implement by HTTP GET on the backend's stats
    /// endpoint.
    fn len(&self) -> Result<usize, StorageError>;

    /// `true` iff `len()? == 0`. Convenience so callers don't have to
    /// compare against 0 themselves.
    fn is_empty(&self) -> Result<bool, StorageError> {
        Ok(self.len()? == 0)
    }
}

/// Simple in-memory [`VectorStore`] with cosine-similarity search.
/// Good enough for tests, dev loops, and deployments that don't
/// need durability; sidecar-backed impls (Qdrant, pgvector) slot in
/// behind the same trait.
#[derive(Debug, Default)]
pub struct MemoryVectorStore {
    rows: Mutex<BTreeMap<String, VectorRecord>>,
}

impl MemoryVectorStore {
    /// Empty in-memory store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl VectorStore for MemoryVectorStore {
    fn upsert(&self, record: &VectorRecord) -> Result<(), StorageError> {
        if record.id.is_empty() {
            return Err(StorageError::Invalid("record.id must not be empty".into()));
        }
        if record.vector.is_empty() {
            return Err(StorageError::Invalid(
                "record.vector must not be empty".into(),
            ));
        }
        self.rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(record.id.clone(), record.clone());
        Ok(())
    }

    fn delete(&self, id: &str) -> Result<bool, StorageError> {
        let removed = self
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id)
            .is_some();
        Ok(removed)
    }

    fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>, StorageError> {
        if k == 0 {
            return Err(StorageError::Invalid("k must be >= 1".into()));
        }
        if query.is_empty() {
            return Err(StorageError::Invalid(
                "query vector must not be empty".into(),
            ));
        }
        let guard = self
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut scored: Vec<VectorHit> = guard
            .values()
            .filter(|r| r.vector.len() == query.len())
            .map(|r| VectorHit {
                id: r.id.clone(),
                score: cosine_similarity(query, &r.vector),
                metadata: r.metadata.clone(),
            })
            .collect();
        // NaN-safe sort — hits with NaN scores sink to the bottom.
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(k);
        Ok(scored)
    }

    fn len(&self) -> Result<usize, StorageError> {
        Ok(self
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len())
    }
}

/// Cosine similarity. Returns 0 when either vector is zero-magnitude
/// — distinguishes "completely orthogonal" and "degenerate input"
/// only by magnitude of the result, which is what callers expect.
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

// ---------------------------------------------------------------------
// RecordingStore — slice 3.4 per-call audio retention
// ---------------------------------------------------------------------

/// Audio retention surface. One blob per `call_id`; the engine writes
/// after the call terminates, MCP tools (`transcribe_call`,
/// `summarize_call`) read during post-processing or on demand.
///
/// Blobs are bytes — format is implementation-defined. The filesystem
/// default writes exactly what was handed to [`Self::put`] (the
/// engine hands it PCM16 LE today; a future WAV-wrapping helper lives
/// outside the trait). Object-store backends should preserve bytes
/// verbatim; they are not decoding / re-encoding middle-men.
pub trait RecordingStore: Send + Sync + 'static {
    /// Store `audio` under `call_id`, overwriting any previous blob.
    fn put(&self, call_id: &str, audio: &[u8]) -> Result<(), StorageError>;

    /// Retrieve the blob for `call_id`. `NotFound` when the call
    /// never had a recording stored.
    fn get(&self, call_id: &str) -> Result<Vec<u8>, StorageError>;

    /// Drop the blob. Returns `Ok(false)` when the key was absent
    /// so cleanup paths can be idempotent.
    fn delete(&self, call_id: &str) -> Result<bool, StorageError>;

    /// Return every `call_id` the store has a recording for. Ordering
    /// is backend-defined; the in-tree filesystem impl returns them
    /// sorted by `call_id` for deterministic test assertions.
    fn list(&self) -> Result<Vec<String>, StorageError>;

    /// Delete every recording whose on-disk (or backend-reported)
    /// mtime is older than `max_age`. Returns the number of blobs
    /// removed. Default impl is a no-op — backends that can't
    /// express retention cheaply opt out by leaving this unchanged,
    /// and the retention sweeper logs a warning once on startup.
    fn prune_older_than(&self, max_age: Duration) -> Result<usize, StorageError> {
        let _ = max_age;
        Ok(0)
    }
}

/// Filesystem-backed [`RecordingStore`]. One file per call, keyed by
/// a filename-safe mangling of `call_id` so any printable ASCII
/// call-id round-trips through the filesystem.
///
/// The mangling is reversible for the `list()` path: we store a
/// `.cid` sidecar text file per blob containing the original
/// `call_id`. That avoids the "filesystem characters escape the
/// call id" landmine and keeps `list()` honest.
#[derive(Debug)]
pub struct FsRecordingStore {
    root: PathBuf,
}

impl FsRecordingStore {
    /// Create the store rooted at `root`. The directory is created
    /// on first call; missing-parent errors surface as `Backend`.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, StorageError> {
        let root = root.into();
        std::fs::create_dir_all(&root).map_err(|e| StorageError::Backend(e.to_string()))?;
        Ok(Self { root })
    }

    fn blob_path(&self, call_id: &str) -> PathBuf {
        self.root.join(format!("{}.wav", hex_key(call_id)))
    }

    fn meta_path(&self, call_id: &str) -> PathBuf {
        self.root.join(format!("{}.cid", hex_key(call_id)))
    }
}

impl RecordingStore for FsRecordingStore {
    fn put(&self, call_id: &str, audio: &[u8]) -> Result<(), StorageError> {
        if call_id.is_empty() {
            return Err(StorageError::Invalid("call_id must not be empty".into()));
        }
        std::fs::write(self.blob_path(call_id), audio)
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        std::fs::write(self.meta_path(call_id), call_id.as_bytes())
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        Ok(())
    }

    fn get(&self, call_id: &str) -> Result<Vec<u8>, StorageError> {
        std::fs::read(self.blob_path(call_id)).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => StorageError::NotFound(call_id.to_owned()),
            _ => StorageError::Backend(e.to_string()),
        })
    }

    fn delete(&self, call_id: &str) -> Result<bool, StorageError> {
        let blob = self.blob_path(call_id);
        let had_blob = blob.exists();
        if had_blob {
            std::fs::remove_file(&blob).map_err(|e| StorageError::Backend(e.to_string()))?;
        }
        let meta = self.meta_path(call_id);
        if meta.exists() {
            let _ = std::fs::remove_file(meta);
        }
        Ok(had_blob)
    }

    fn list(&self) -> Result<Vec<String>, StorageError> {
        let mut out = Vec::new();
        let iter =
            std::fs::read_dir(&self.root).map_err(|e| StorageError::Backend(e.to_string()))?;
        for entry in iter.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("cid") {
                continue;
            }
            if let Ok(id_bytes) = std::fs::read(&path)
                && let Ok(id) = String::from_utf8(id_bytes)
            {
                out.push(id);
            }
        }
        out.sort();
        Ok(out)
    }

    fn prune_older_than(&self, max_age: Duration) -> Result<usize, StorageError> {
        let cutoff = SystemTime::now()
            .checked_sub(max_age)
            .ok_or_else(|| StorageError::Invalid("max_age overflows the clock".into()))?;
        let mut removed = 0usize;
        let iter =
            std::fs::read_dir(&self.root).map_err(|e| StorageError::Backend(e.to_string()))?;
        for entry in iter.flatten() {
            let path = entry.path();
            // Only sweep pairs whose primary blob predates the cutoff.
            if path.extension().and_then(|s| s.to_str()) != Some("wav") {
                continue;
            }
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            let Ok(mtime) = meta.modified() else {
                continue;
            };
            if mtime < cutoff {
                let _ = std::fs::remove_file(&path);
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    let _ = std::fs::remove_file(self.root.join(format!("{stem}.cid")));
                }
                removed += 1;
            }
        }
        Ok(removed)
    }
}

/// Hex-encode a `call_id` so arbitrary printable strings become
/// filesystem-safe filenames. SHA-256-shaped would also work, but
/// hex is reversible in principle (we don't need the reverse; the
/// `.cid` sidecar carries the original) and keeps names short
/// enough for humans to `ls | grep`.
fn hex_key(call_id: &str) -> String {
    hex::encode(call_id.as_bytes())
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

    // -----------------------------------------------------------------
    // Slice 3.4 — VectorStore + RecordingStore
    // -----------------------------------------------------------------

    fn vec_rec(id: &str, v: &[f32]) -> VectorRecord {
        VectorRecord {
            id: id.into(),
            vector: v.to_vec(),
            metadata: serde_json::json!({}),
        }
    }

    #[test]
    fn memory_vector_store_round_trips_upsert_search() {
        let store = MemoryVectorStore::new();
        store.upsert(&vec_rec("a", &[1.0, 0.0, 0.0])).unwrap();
        store.upsert(&vec_rec("b", &[0.0, 1.0, 0.0])).unwrap();
        store.upsert(&vec_rec("c", &[0.9, 0.1, 0.0])).unwrap();
        assert_eq!(store.len().unwrap(), 3);

        // Query near `a` — expect `a` first, then `c`, then `b`.
        let hits = store.search(&[1.0, 0.0, 0.0], 3).unwrap();
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].id, "a");
        assert_eq!(hits[1].id, "c");
        assert_eq!(hits[2].id, "b");
        assert!(hits[0].score > hits[1].score);
    }

    #[test]
    fn memory_vector_store_rejects_invalid_inputs() {
        let store = MemoryVectorStore::new();
        assert!(matches!(
            store.upsert(&vec_rec("", &[1.0])),
            Err(StorageError::Invalid(_))
        ));
        assert!(matches!(
            store.upsert(&vec_rec("x", &[])),
            Err(StorageError::Invalid(_))
        ));
        assert!(matches!(
            store.search(&[], 1),
            Err(StorageError::Invalid(_))
        ));
        assert!(matches!(
            store.search(&[1.0], 0),
            Err(StorageError::Invalid(_))
        ));
    }

    #[test]
    fn memory_vector_store_skips_dimension_mismatch_on_search() {
        let store = MemoryVectorStore::new();
        store.upsert(&vec_rec("a", &[1.0, 0.0, 0.0])).unwrap();
        store.upsert(&vec_rec("b", &[1.0, 0.0])).unwrap(); // 2-dim
        let hits = store.search(&[1.0, 0.0, 0.0], 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "a");
    }

    #[test]
    fn memory_vector_store_delete_is_idempotent() {
        let store = MemoryVectorStore::new();
        store.upsert(&vec_rec("a", &[1.0])).unwrap();
        assert!(store.delete("a").unwrap());
        assert!(!store.delete("a").unwrap()); // already gone
    }

    #[test]
    fn fs_recording_store_put_get_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FsRecordingStore::new(tmp.path()).unwrap();
        let audio = vec![1u8, 2, 3, 4, 5];
        store.put("call/42:with weird chars", &audio).unwrap();
        let out = store.get("call/42:with weird chars").unwrap();
        assert_eq!(out, audio);
    }

    #[test]
    fn fs_recording_store_list_returns_original_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FsRecordingStore::new(tmp.path()).unwrap();
        store.put("alpha", b"a").unwrap();
        store.put("bravo", b"b").unwrap();
        let ids = store.list().unwrap();
        assert_eq!(ids, vec!["alpha".to_string(), "bravo".to_string()]);
    }

    #[test]
    fn fs_recording_store_get_missing_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FsRecordingStore::new(tmp.path()).unwrap();
        assert!(matches!(store.get("ghost"), Err(StorageError::NotFound(_))));
    }

    #[test]
    fn fs_recording_store_delete_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FsRecordingStore::new(tmp.path()).unwrap();
        store.put("a", b"x").unwrap();
        assert!(store.delete("a").unwrap());
        assert!(!store.delete("a").unwrap());
    }

    #[test]
    fn fs_recording_store_prune_removes_only_old_files() {
        use std::thread::sleep;
        let tmp = tempfile::tempdir().unwrap();
        let store = FsRecordingStore::new(tmp.path()).unwrap();
        store.put("old", b"x").unwrap();
        sleep(Duration::from_millis(50));
        store.put("new", b"y").unwrap();
        // Prune anything older than 30 ms — `old` should vanish,
        // `new` (just written) should survive.
        let removed = store.prune_older_than(Duration::from_millis(30)).unwrap();
        assert_eq!(removed, 1);
        let remaining = store.list().unwrap();
        assert_eq!(remaining, vec!["new".to_string()]);
    }
}
