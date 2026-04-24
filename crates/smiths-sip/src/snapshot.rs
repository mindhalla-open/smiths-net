//! Dialog-table snapshot + replay (slice 6.1 / P15 HA MVP).
//!
//! On graceful shutdown, the CLI writes every live
//! [`DialogRecord`] in `UasServer::dialogs_handle()` to a JSON
//! file. On the next boot, if that file exists, the CLI reads it
//! and calls [`UasServer::restore_dialogs`] before `run()`. The
//! restored UAS picks up where the crashed one left off: ACKs to
//! its Early dialogs still confirm, BYEs tear down both the
//! restored record and the engine-local bookkeeping cleanly.
//!
//! ## What this slice ships
//!
//! - JSON format (not CBOR / `MessagePack`) — one small file the
//!   operator can `cat`, `grep`, and diff during an incident. The
//!   scale ceiling is "thousands of active dialogs"; JSON's
//!   overhead is irrelevant there, and the `serde` derives on
//!   `DialogRecord` + its transitive fields already support it.
//! - Atomic write — the writer creates `<path>.tmp`, fsyncs, then
//!   renames onto `<path>`. No partial-write half-snapshot can
//!   fool the replay path.
//! - No schema version today — the file carries a magic tag
//!   (`"smiths-net/dialog-snapshot"`) + a version integer so a
//!   future evolution has a place to hook migration.
//!
//! ## What's NOT in this slice
//!
//! - **Live replication.** Writing deltas to a secondary as they
//!   happen is slice 6.2; this slice only writes on shutdown.
//!   A primary that crashes mid-call loses the in-progress
//!   dialogs that hadn't been Confirmed yet.
//! - **Media-plane resumption.** Restored dialogs have no bridge.
//!   An RTP flow belonging to a pre-crash dialog goes silent; a
//!   BYE from either side cleans up the restored record.
//! - **Snapshot interval.** The shutdown-only write is deliberate:
//!   periodic snapshots at a hot path would interact badly with
//!   concurrent dialog-table mutation without a heavy lock. 6.2's
//!   delta-replication supersedes this anyway.

use std::path::Path;

use smiths_core::{DialogKey, DialogRecord};

use crate::error::Error;

/// Snapshot magic tag. Changed when the JSON shape evolves
/// incompatibly — today's version is `1` and pre-6.1 `Config` /
/// runtimes have no snapshot at all, so there's nothing to
/// migrate from.
const SNAPSHOT_MAGIC: &str = "smiths-net/dialog-snapshot";
const SNAPSHOT_VERSION: u8 = 1;

/// On-disk shape. `records` is a flat `Vec<DialogRecord>` — the
/// `DialogKey` is derivable from each record, so we don't store
/// it twice. The wrapper's `magic` + `version` guard against
/// accidentally reading an unrelated file.
#[derive(serde::Serialize, serde::Deserialize)]
struct SnapshotFile {
    magic: String,
    version: u8,
    records: Vec<DialogRecord>,
}

/// Errors the snapshot path can surface.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// Underlying filesystem I/O failed.
    #[error("snapshot I/O: {0}")]
    Io(#[from] std::io::Error),
    /// JSON serialization / deserialization failed.
    #[error("snapshot serde: {0}")]
    Serde(#[from] serde_json::Error),
    /// File exists but the magic tag doesn't match — probably a
    /// different snapshot format or an unrelated file pointed at
    /// by an operator typo.
    #[error("snapshot magic mismatch: expected {expected:?}, got {got:?}")]
    MagicMismatch {
        /// What we expected.
        expected: String,
        /// What we read.
        got: String,
    },
    /// File's version is newer than this build supports. A
    /// future slice lands a migration matrix; today we refuse
    /// loudly so an operator downgrading from a future build
    /// doesn't silently lose dialogs.
    #[error("snapshot version {0} exceeds supported {supported}", supported = SNAPSHOT_VERSION)]
    UnsupportedVersion(u8),
}

impl From<SnapshotError> for Error {
    fn from(e: SnapshotError) -> Self {
        Self::other(e.to_string())
    }
}

/// Write every live dialog to `path`.
///
/// `dialogs` is typically [`crate::UasServer::dialogs_handle`]
/// cloned out before `run()` was spawned. Writes atomically via a
/// `<path>.tmp` scratch + rename so a partial write can never
/// fool the replay path.
///
/// # Errors
/// [`SnapshotError`] on any I/O or serialization failure.
pub fn write_snapshot<P: AsRef<Path>>(
    path: P,
    dialogs: &dashmap::DashMap<DialogKey, DialogRecord>,
) -> Result<usize, SnapshotError> {
    let records: Vec<DialogRecord> = dialogs.iter().map(|e| e.value().clone()).collect();
    let n = records.len();
    let snapshot = SnapshotFile {
        magic: SNAPSHOT_MAGIC.to_owned(),
        version: SNAPSHOT_VERSION,
        records,
    };
    let body = serde_json::to_vec_pretty(&snapshot)?;
    let path = path.as_ref();
    let tmp = path.with_extension("snapshot.tmp");
    std::fs::write(&tmp, &body)?;
    // Best-effort fsync via `File::sync_all` — Rust's std only
    // offers this on a `File`, and `std::fs::write` opens+writes
    // in one call. Re-open briefly to sync so the rename doesn't
    // beat the data to disk on crash-heavy filesystems.
    if let Ok(f) = std::fs::File::open(&tmp) {
        let _ = f.sync_all();
    }
    std::fs::rename(&tmp, path)?;
    Ok(n)
}

/// Read a snapshot from `path`. `Ok(None)` when the file doesn't
/// exist — the common cold-boot case. `Err` on any corruption,
/// magic mismatch, or version mismatch (loud rather than silent
/// so operators notice a stale file).
///
/// # Errors
/// [`SnapshotError`] on I/O, serde, magic, or version failure.
pub fn read_snapshot<P: AsRef<Path>>(path: P) -> Result<Option<Vec<DialogRecord>>, SnapshotError> {
    let path = path.as_ref();
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let snapshot: SnapshotFile = serde_json::from_slice(&bytes)?;
    if snapshot.magic != SNAPSHOT_MAGIC {
        return Err(SnapshotError::MagicMismatch {
            expected: SNAPSHOT_MAGIC.to_owned(),
            got: snapshot.magic,
        });
    }
    if snapshot.version > SNAPSHOT_VERSION {
        return Err(SnapshotError::UnsupportedVersion(snapshot.version));
    }
    Ok(Some(snapshot.records))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dashmap::DashMap;
    use smiths_core::DialogState;

    fn record(call_id: &str, state: DialogState) -> DialogRecord {
        DialogRecord {
            call_id: call_id.into(),
            local_tag: "lt".into(),
            remote_tag: "rt".into(),
            state,
            peer_signal: "127.0.0.1:5060".parse().unwrap(),
            rendezvous: None,
            media: None,
            remote_media: None,
            pending_2xx: None,
            per_leg_codec: std::collections::BTreeMap::new(),
            ice: None,
        }
    }

    #[test]
    fn write_then_read_round_trips_every_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snap.json");
        let dialogs: DashMap<DialogKey, DialogRecord> = DashMap::new();
        let r1 = record("c1@x", DialogState::Early);
        let r2 = record("c2@x", DialogState::Confirmed);
        dialogs.insert(r1.key(), r1.clone());
        dialogs.insert(r2.key(), r2.clone());

        let n = write_snapshot(&path, &dialogs).unwrap();
        assert_eq!(n, 2);
        let restored = read_snapshot(&path).unwrap().unwrap();
        assert_eq!(restored.len(), 2);
        let mut call_ids: Vec<_> = restored.iter().map(|r| r.call_id.clone()).collect();
        call_ids.sort();
        assert_eq!(call_ids, vec!["c1@x".to_string(), "c2@x".to_string()]);
    }

    #[test]
    fn read_missing_file_returns_ok_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no-such.json");
        assert!(read_snapshot(&path).unwrap().is_none());
    }

    #[test]
    fn read_wrong_magic_returns_magic_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snap.json");
        std::fs::write(&path, r#"{"magic":"other-tool","version":1,"records":[]}"#).unwrap();
        let err = read_snapshot(&path).unwrap_err();
        assert!(matches!(err, SnapshotError::MagicMismatch { .. }));
    }

    #[test]
    fn read_future_version_refuses_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snap.json");
        std::fs::write(
            &path,
            r#"{"magic":"smiths-net/dialog-snapshot","version":99,"records":[]}"#,
        )
        .unwrap();
        let err = read_snapshot(&path).unwrap_err();
        assert!(matches!(err, SnapshotError::UnsupportedVersion(99)));
    }

    #[test]
    fn atomic_write_uses_tmp_path_then_rename() {
        // Sanity: after write_snapshot the tmp file must not
        // exist — rename is the last step.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snap.json");
        let dialogs: DashMap<DialogKey, DialogRecord> = DashMap::new();
        dialogs.insert(
            record("c@x", DialogState::Early).key(),
            record("c@x", DialogState::Early),
        );
        write_snapshot(&path, &dialogs).unwrap();
        let tmp = path.with_extension("snapshot.tmp");
        assert!(!tmp.exists(), "tmp file should be gone after rename");
        assert!(path.exists(), "final snapshot path should exist");
    }
}
