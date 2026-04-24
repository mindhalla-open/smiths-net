//! SQLite-backed Raft log storage.

use std::ops::RangeBounds;
use std::path::Path;
use std::sync::{Arc, Mutex};

use openraft::storage::{LogFlushed, LogState, RaftLogStorage};
use openraft::{LogId, OptionalSend, Vote};
use rusqlite::{Connection, OptionalExtension, params};

use crate::types::{NodeId, SmithsTypeConfig};

type Entry = openraft::Entry<SmithsTypeConfig>;

/// `SQLite` `Raft` log store.
///
/// Stores Raft log entries and voted-for state in an embedded `SQLite` database.
/// Schema:
///   - `raft_state(key TEXT PK, value BLOB)` — stores the current vote.
///   - `raft_logs(log_index INT PK, term INT, payload BLOB)` — stores log entries.
#[derive(Clone)]
pub struct SqliteLogStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteLogStore {
    /// Open (or create) the `SQLite` log store at `path`.
    pub fn new<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;

        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS raft_state (
                key   TEXT PRIMARY KEY,
                value BLOB NOT NULL
            );

            CREATE TABLE IF NOT EXISTS raft_logs (
                log_index INTEGER PRIMARY KEY,
                term      INTEGER NOT NULL,
                payload   BLOB    NOT NULL
            );
            ",
        )?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Query the last persisted log id (term + index) from `SQLite`.
    fn last_log_id_from_db(&self) -> Option<LogId<NodeId>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT log_index, term FROM raft_logs ORDER BY log_index DESC LIMIT 1")
            .unwrap();
        stmt.query_row([], |row| {
            let index: i64 = row.get(0)?;
            let term: i64 = row.get(1)?;
            Ok(LogId {
                leader_id: openraft::CommittedLeaderId::new(term.cast_unsigned(), 0),
                index: index.cast_unsigned(),
            })
        })
        .optional()
        .unwrap()
    }
}

impl openraft::RaftLogReader<SmithsTypeConfig> for SqliteLogStore {
    async fn try_get_log_entries<RB>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry>, openraft::StorageError<NodeId>>
    where
        RB: RangeBounds<u64> + Clone + std::fmt::Debug + OptionalSend,
    {
        use std::ops::Bound;
        let conn = self.conn.lock().unwrap();

        let start: i64 = match range.start_bound() {
            Bound::Included(&v) => v.cast_signed(),
            Bound::Excluded(&v) => v.cast_signed() + 1,
            Bound::Unbounded => 0,
        };
        let end_clause = match range.end_bound() {
            Bound::Included(&v) => format!("AND log_index <= {}", v.cast_signed()),
            Bound::Excluded(&v) => format!("AND log_index < {}", v.cast_signed()),
            Bound::Unbounded => String::new(),
        };

        let sql = format!(
            "SELECT payload FROM raft_logs WHERE log_index >= {start} {end_clause} ORDER BY log_index"
        );
        let mut stmt = conn.prepare(&sql).unwrap();
        let entries: Vec<Entry> = stmt
            .query_map([], |row| {
                let payload: Vec<u8> = row.get(0)?;
                Ok(payload)
            })
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|bytes| serde_json::from_slice(&bytes).unwrap())
            .collect();

        Ok(entries)
    }
}

impl RaftLogStorage<SmithsTypeConfig> for SqliteLogStore {
    type LogReader = Self;

    async fn get_log_state(
        &mut self,
    ) -> Result<LogState<SmithsTypeConfig>, openraft::StorageError<NodeId>> {
        let last_log_id = self.last_log_id_from_db();
        Ok(LogState {
            last_purged_log_id: None,
            last_log_id,
        })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn save_vote(
        &mut self,
        vote: &Vote<NodeId>,
    ) -> Result<(), openraft::StorageError<NodeId>> {
        let conn = self.conn.lock().unwrap();
        let val = serde_json::to_vec(vote).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO raft_state (key, value) VALUES ('vote', ?)",
            params![val],
        )
        .unwrap();
        Ok(())
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<NodeId>>, openraft::StorageError<NodeId>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT value FROM raft_state WHERE key = 'vote'")
            .unwrap();
        let vote: Option<Vec<u8>> = stmt.query_row([], |row| row.get(0)).optional().unwrap();
        Ok(vote.map(|v| serde_json::from_slice(&v).unwrap()))
    }

    async fn append<I>(
        &mut self,
        entries: I,
        callback: LogFlushed<SmithsTypeConfig>,
    ) -> Result<(), openraft::StorageError<NodeId>>
    where
        I: IntoIterator<Item = Entry> + OptionalSend,
        I::IntoIter: OptionalSend,
    {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction().unwrap();

        for entry in entries {
            let payload = serde_json::to_vec(&entry).unwrap();
            tx.execute(
                "INSERT OR REPLACE INTO raft_logs (log_index, term, payload) VALUES (?, ?, ?)",
                params![
                    entry.log_id.index.cast_signed(),
                    entry.log_id.leader_id.term.cast_signed(),
                    payload
                ],
            )
            .unwrap();
        }

        tx.commit().unwrap();
        callback.log_io_completed(Ok(()));
        Ok(())
    }

    async fn truncate(
        &mut self,
        log_id: LogId<NodeId>,
    ) -> Result<(), openraft::StorageError<NodeId>> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM raft_logs WHERE log_index >= ?",
            params![log_id.index.cast_signed()],
        )
        .unwrap();
        Ok(())
    }

    async fn purge(&mut self, log_id: LogId<NodeId>) -> Result<(), openraft::StorageError<NodeId>> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM raft_logs WHERE log_index <= ?",
            params![log_id.index.cast_signed()],
        )
        .unwrap();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_store() -> SqliteLogStore {
        SqliteLogStore::new(":memory:").unwrap()
    }

    #[tokio::test]
    async fn empty_store_has_no_log() {
        let mut store = mem_store();
        let state = store.get_log_state().await.unwrap();
        assert!(state.last_log_id.is_none());
        assert!(state.last_purged_log_id.is_none());
    }

    #[tokio::test]
    async fn vote_round_trips() {
        let mut store = mem_store();
        assert!(store.read_vote().await.unwrap().is_none());

        let vote = Vote::new(42, 1);
        store.save_vote(&vote).await.unwrap();
        let read_back = store.read_vote().await.unwrap().unwrap();
        assert_eq!(vote, read_back);
    }
}
