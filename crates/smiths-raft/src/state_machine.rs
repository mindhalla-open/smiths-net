//! Raft state machine backed by the shared dialog `DashMap` (slice 6.3a/b).
//!
//! The state machine's `apply()` replays `DialogDelta` log entries onto the
//! dialog table. This is the same table used by `UasServer`, so the Raft
//! consensus layer is the authoritative source of call-state changes.

use std::io::Cursor;
use std::sync::Arc;

use dashmap::DashMap;
use openraft::storage::RaftStateMachine;
use openraft::{LogId, OptionalSend, Snapshot, SnapshotMeta, StorageError, StoredMembership};
use smiths_core::{DialogKey, DialogRecord};
use tracing::debug;

use crate::types::{NodeId, SmithsTypeConfig};

type Entry = openraft::Entry<SmithsTypeConfig>;

// ---------------------------------------------------------------------------
// Snapshot payload — serialized as JSON inside the Cursor<Vec<u8>>
// ---------------------------------------------------------------------------

/// Everything we need to fully reconstruct the state machine from a snapshot.
#[derive(serde::Serialize, serde::Deserialize)]
struct SnapshotPayload {
    /// All dialogs at snapshot time.
    dialogs: Vec<DialogRecord>,
    /// Last applied log id (serialized as (term, `node_id`, index)).
    last_applied_term: Option<u64>,
    last_applied_node: Option<u64>,
    last_applied_index: Option<u64>,
}

// ---------------------------------------------------------------------------
// State machine
// ---------------------------------------------------------------------------

/// Raft state machine for the dialog table.
///
/// Applies committed `DialogDelta` log entries to the in-memory `DashMap`.
#[derive(Clone)]
pub struct DialogStateMachine {
    dialogs: Arc<DashMap<DialogKey, DialogRecord>>,
    last_applied: Option<LogId<NodeId>>,
    last_membership: StoredMembership<NodeId, openraft::BasicNode>,
    /// Cached snapshot (built or installed).
    current_snapshot: Option<StoredSnapshot>,
}

/// In-memory cached snapshot.
#[derive(Clone)]
struct StoredSnapshot {
    meta: SnapshotMeta<NodeId, openraft::BasicNode>,
    data: Vec<u8>,
}

impl DialogStateMachine {
    /// Create a new state machine sharing the given dialog table.
    #[must_use]
    pub fn new(dialogs: Arc<DashMap<DialogKey, DialogRecord>>) -> Self {
        Self {
            dialogs,
            last_applied: None,
            last_membership: StoredMembership::default(),
            current_snapshot: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Snapshot builder
// ---------------------------------------------------------------------------

/// Builds a snapshot by serializing every dialog record to JSON.
pub struct DialogSnapshotBuilder {
    dialogs: Arc<DashMap<DialogKey, DialogRecord>>,
    last_applied: Option<LogId<NodeId>>,
    last_membership: StoredMembership<NodeId, openraft::BasicNode>,
}

impl openraft::RaftSnapshotBuilder<SmithsTypeConfig> for DialogSnapshotBuilder {
    async fn build_snapshot(&mut self) -> Result<Snapshot<SmithsTypeConfig>, StorageError<NodeId>> {
        let records: Vec<DialogRecord> = self
            .dialogs
            .iter()
            .map(|entry| entry.value().clone())
            .collect();

        let payload = SnapshotPayload {
            dialogs: records,
            last_applied_term: self.last_applied.map(|l| l.leader_id.term),
            last_applied_node: self.last_applied.map(|l| l.leader_id.node_id),
            last_applied_index: self.last_applied.map(|l| l.index),
        };

        let data = serde_json::to_vec(&payload)
            .map_err(|e| openraft::StorageIOError::write_snapshot(None, &e))?;

        let snapshot_id = format!(
            "snap-{}-{}",
            self.last_applied.map_or(0, |l| l.leader_id.term),
            self.last_applied.map_or(0, |l| l.index),
        );

        let meta = SnapshotMeta {
            last_log_id: self.last_applied,
            last_membership: self.last_membership.clone(),
            snapshot_id,
        };

        debug!(
            last_log_id = ?meta.last_log_id,
            dialogs = payload.dialogs.len(),
            bytes = data.len(),
            "Raft snapshot built"
        );

        Ok(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(data)),
        })
    }
}

// ---------------------------------------------------------------------------
// RaftStateMachine impl
// ---------------------------------------------------------------------------

impl RaftStateMachine<SmithsTypeConfig> for DialogStateMachine {
    type SnapshotBuilder = DialogSnapshotBuilder;

    async fn applied_state(
        &mut self,
    ) -> Result<
        (
            Option<LogId<NodeId>>,
            StoredMembership<NodeId, openraft::BasicNode>,
        ),
        StorageError<NodeId>,
    > {
        Ok((self.last_applied, self.last_membership.clone()))
    }

    async fn apply<I>(&mut self, entries: I) -> Result<Vec<()>, StorageError<NodeId>>
    where
        I: IntoIterator<Item = Entry> + OptionalSend,
        I::IntoIter: OptionalSend,
    {
        let mut results = Vec::new();

        for entry in entries {
            self.last_applied = Some(entry.log_id);

            match entry.payload {
                openraft::EntryPayload::Normal(delta) => match delta {
                    smiths_core::DialogDelta::Upsert(record_box) => {
                        let record = *record_box;
                        debug!(call_id = %record.call_id, "Raft apply: Upsert dialog");
                        self.dialogs.insert(record.key(), record);
                    }
                    smiths_core::DialogDelta::Delete(key) => {
                        debug!(?key, "Raft apply: Delete dialog");
                        self.dialogs.remove(&key);
                    }
                },
                openraft::EntryPayload::Membership(membership) => {
                    self.last_membership = StoredMembership::new(Some(entry.log_id), membership);
                }
                openraft::EntryPayload::Blank => {}
            }

            results.push(());
        }

        Ok(results)
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        DialogSnapshotBuilder {
            dialogs: Arc::clone(&self.dialogs),
            last_applied: self.last_applied,
            last_membership: self.last_membership.clone(),
        }
    }

    async fn begin_receiving_snapshot(
        &mut self,
    ) -> Result<Box<Cursor<Vec<u8>>>, StorageError<NodeId>> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta<NodeId, openraft::BasicNode>,
        snapshot: Box<Cursor<Vec<u8>>>,
    ) -> Result<(), StorageError<NodeId>> {
        let data = snapshot.into_inner();
        let payload: SnapshotPayload = serde_json::from_slice(&data)
            .map_err(|e| openraft::StorageIOError::read_snapshot(None, &e))?;

        // Replace the dialog table.
        self.dialogs.clear();
        for record in &payload.dialogs {
            self.dialogs.insert(record.key(), record.clone());
        }

        // Restore the applied pointer.
        self.last_applied = meta.last_log_id;
        self.last_membership = meta.last_membership.clone();

        // Cache the snapshot.
        self.current_snapshot = Some(StoredSnapshot {
            meta: meta.clone(),
            data,
        });

        debug!(
            last_log_id = ?meta.last_log_id,
            dialogs = payload.dialogs.len(),
            "Raft snapshot installed"
        );

        Ok(())
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<Snapshot<SmithsTypeConfig>>, StorageError<NodeId>> {
        Ok(self.current_snapshot.as_ref().map(|s| Snapshot {
            meta: s.meta.clone(),
            snapshot: Box::new(Cursor::new(s.data.clone())),
        }))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use openraft::RaftSnapshotBuilder;
    use smiths_core::{DialogDelta, DialogRecord, call::DialogState};

    fn make_record(call_id: &str, local_tag: &str) -> DialogRecord {
        DialogRecord {
            call_id: call_id.into(),
            local_tag: local_tag.into(),
            remote_tag: "remote".into(),
            state: DialogState::Early,
            peer_signal: "127.0.0.1:5060".parse().unwrap(),
            rendezvous: None,
            media: None,
            remote_media: None,
            pending_2xx: None,
            per_leg_codec: BTreeMap::default(),
            ice: None,
        }
    }

    fn make_entry(index: u64, delta: DialogDelta) -> Entry {
        openraft::Entry {
            log_id: openraft::LogId {
                leader_id: openraft::CommittedLeaderId::new(1, 0),
                index,
            },
            payload: openraft::EntryPayload::Normal(delta),
        }
    }

    #[tokio::test]
    async fn apply_upsert_and_delete() {
        let dialogs = Arc::new(DashMap::new());
        let mut sm = DialogStateMachine::new(Arc::clone(&dialogs));

        let record = make_record("call-1", "alice");
        let key = record.key();
        let delta = DialogDelta::Upsert(Box::new(record));
        sm.apply(vec![make_entry(1, delta)]).await.unwrap();

        assert!(
            dialogs.contains_key(&key),
            "dialog should be in map after Upsert"
        );

        let del = DialogDelta::Delete(key.clone());
        sm.apply(vec![make_entry(2, del)]).await.unwrap();
        assert!(
            !dialogs.contains_key(&key),
            "dialog should be removed after Delete"
        );
    }

    #[tokio::test]
    async fn multiple_upserts_update_in_place() {
        let dialogs = Arc::new(DashMap::new());
        let mut sm = DialogStateMachine::new(Arc::clone(&dialogs));

        let r = make_record("call-2", "bob");
        let key = r.key(); // key = (call_id, local_tag, remote_tag)
        sm.apply(vec![make_entry(
            1,
            DialogDelta::Upsert(Box::new(r.clone())),
        )])
        .await
        .unwrap();
        assert_eq!(dialogs.len(), 1);

        // Update a non-key field (pending_2xx) — must still be 1 entry.
        let mut r2 = r.clone();
        r2.pending_2xx = Some(b"INVITE".to_vec());
        sm.apply(vec![make_entry(2, DialogDelta::Upsert(Box::new(r2)))])
            .await
            .unwrap();
        assert_eq!(dialogs.len(), 1, "should still be one entry after update");
        assert!(dialogs.get(&key).unwrap().pending_2xx.is_some());
    }

    #[tokio::test]
    async fn applied_state_tracks_last_log_id() {
        let dialogs = Arc::new(DashMap::new());
        let mut sm = DialogStateMachine::new(Arc::clone(&dialogs));

        let (last, _) = sm.applied_state().await.unwrap();
        assert!(last.is_none());

        let record = make_record("call-3", "charlie");
        sm.apply(vec![make_entry(5, DialogDelta::Upsert(Box::new(record)))])
            .await
            .unwrap();

        let (last, _) = sm.applied_state().await.unwrap();
        assert_eq!(last.unwrap().index, 5);
    }

    #[tokio::test]
    async fn snapshot_round_trip() {
        let dialogs = Arc::new(DashMap::new());
        let mut sm = DialogStateMachine::new(Arc::clone(&dialogs));

        // Apply two records.
        let r1 = make_record("call-snap-1", "alice");
        let r2 = make_record("call-snap-2", "bob");
        sm.apply(vec![
            make_entry(1, DialogDelta::Upsert(Box::new(r1.clone()))),
            make_entry(2, DialogDelta::Upsert(Box::new(r2.clone()))),
        ])
        .await
        .unwrap();
        assert_eq!(dialogs.len(), 2);

        // Build a snapshot.
        let mut builder = sm.get_snapshot_builder().await;
        let snapshot = builder.build_snapshot().await.unwrap();
        assert_eq!(snapshot.meta.last_log_id.unwrap().index, 2);

        // Create a fresh state machine and install the snapshot.
        let dialogs2 = Arc::new(DashMap::new());
        let mut sm2 = DialogStateMachine::new(Arc::clone(&dialogs2));
        assert_eq!(dialogs2.len(), 0);

        sm2.install_snapshot(&snapshot.meta, snapshot.snapshot)
            .await
            .unwrap();
        assert_eq!(dialogs2.len(), 2, "snapshot should restore both dialogs");

        // Verify get_current_snapshot returns it.
        let cached = sm2.get_current_snapshot().await.unwrap();
        assert!(cached.is_some());
        assert_eq!(cached.unwrap().meta.last_log_id.unwrap().index, 2);
    }

    #[tokio::test]
    async fn begin_receiving_snapshot_returns_empty_cursor() {
        let dialogs = Arc::new(DashMap::new());
        let mut sm = DialogStateMachine::new(dialogs);
        let cursor = sm.begin_receiving_snapshot().await.unwrap();
        assert!(cursor.into_inner().is_empty());
    }
}
