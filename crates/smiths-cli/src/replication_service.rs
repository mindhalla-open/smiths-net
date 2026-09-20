//! HA dialog replication: primary → secondary delta stream with
//! snapshot resync.
//!
//! The primary listens on `cluster.peer_addr`; the secondary dials
//! it. Frames are newline-delimited JSON ([`ReplFrame`]):
//!
//! - `snapshot` — the whole dialog table. Sent on every secondary
//!   (re)connect and whenever the primary has flagged a resync
//!   (delta queue overflow or a failed write). The secondary
//!   replaces its table wholesale, so a snapshot always converges
//!   the two sides regardless of what was lost in between.
//! - `delta` — one [`DialogDelta`] (upsert / delete) in order.
//! - `heartbeat` — liveness ping every
//!   `cluster.heartbeat_interval_secs`; the secondary drops and
//!   redials a connection that stays silent for three intervals.
//!
//! The UAS hands deltas to [`PrimaryReplicator::replicate`] from
//! its request path, so that call never blocks: it `try_send`s into
//! a bounded queue and, when the queue is full or the service is
//! gone, counts the drop and raises the resync flag instead of
//! stalling signaling. [`ReplicationState`] exposes every counter
//! for the `cluster://status` MCP resource.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use smiths_core::{ClusterMode, DialogDelta, DialogKey, DialogRecord, Replicator};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Depth of the primary's delta queue. Deltas are a few hundred
/// bytes each; a burst deeper than this means the secondary link is
/// stalled, at which point a snapshot resync is cheaper than a
/// backlog.
pub(crate) const REPLICATION_QUEUE_DEPTH: usize = 1024;

/// Delay between a secondary's reconnect attempts.
pub(crate) const DEFAULT_RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// Shared dialog table type replicated between the two roles.
pub(crate) type DialogTable = dashmap::DashMap<DialogKey, DialogRecord>;

/// One line on the replication wire.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub(crate) enum ReplFrame {
    /// Full dialog table; the receiver replaces its own.
    Snapshot {
        /// Every live record on the primary.
        records: Vec<DialogRecord>,
    },
    /// One incremental change.
    Delta {
        /// The change.
        delta: DialogDelta,
    },
    /// Liveness ping.
    Heartbeat,
}

/// Live replication counters shared by the service tasks, the
/// [`PrimaryReplicator`] and the `cluster://status` resource.
#[derive(Debug)]
pub(crate) struct ReplicationState {
    role: ClusterMode,
    peer: Option<SocketAddr>,
    connected: AtomicBool,
    /// Unix seconds of the last delta sent (primary) or applied
    /// (secondary). `0` = never.
    last_delta_unix: AtomicU64,
    /// Unix seconds of the last heartbeat received (secondary).
    last_heartbeat_unix: AtomicU64,
    /// Snapshots sent because a delta was dropped or a write failed
    /// — i.e. convergence had to be repaired.
    resyncs: AtomicU64,
    /// Snapshots sent on (re)connect.
    snapshots_sent: AtomicU64,
    /// Deltas `replicate` could not queue.
    dropped_deltas: AtomicU64,
    /// Secondary connections after the first one.
    reconnects: AtomicU64,
    /// A drop or write failure happened since the last snapshot.
    resync_needed: AtomicBool,
    /// Heartbeat interval in seconds; hot-reloadable through
    /// [`Self::set_heartbeat_secs`].
    heartbeat_secs: AtomicU64,
}

/// Point-in-time copy of [`ReplicationState`] for reporting.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct ReplicationStatus {
    /// Configured role.
    pub role: ClusterMode,
    /// Peer address (bind for primary, dial target for secondary).
    pub peer: Option<SocketAddr>,
    /// A replication connection is up right now.
    pub connected: bool,
    /// Seconds since the last delta crossed the wire, if any did.
    pub last_delta_age_secs: Option<u64>,
    /// Seconds since the last heartbeat arrived (secondary only).
    pub last_heartbeat_age_secs: Option<u64>,
    /// Snapshots sent to repair convergence.
    pub resyncs: u64,
    /// Snapshots sent on (re)connect.
    pub snapshots_sent: u64,
    /// Deltas dropped at the queue.
    pub dropped_deltas: u64,
    /// Reconnects observed.
    pub reconnects: u64,
    /// A resync is pending the next connected write.
    pub resync_pending: bool,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn age_of(unix: u64) -> Option<u64> {
    (unix != 0).then(|| now_unix().saturating_sub(unix))
}

impl ReplicationState {
    pub(crate) fn new(
        role: ClusterMode,
        peer: Option<SocketAddr>,
        heartbeat_secs: u64,
    ) -> Arc<Self> {
        Arc::new(Self {
            role,
            peer,
            connected: AtomicBool::new(false),
            last_delta_unix: AtomicU64::new(0),
            last_heartbeat_unix: AtomicU64::new(0),
            resyncs: AtomicU64::new(0),
            snapshots_sent: AtomicU64::new(0),
            dropped_deltas: AtomicU64::new(0),
            reconnects: AtomicU64::new(0),
            resync_needed: AtomicBool::new(false),
            heartbeat_secs: AtomicU64::new(heartbeat_secs.max(1)),
        })
    }

    /// Update the heartbeat interval (config read-through).
    pub(crate) fn set_heartbeat_secs(&self, secs: u64) {
        self.heartbeat_secs.store(secs.max(1), Ordering::Release);
    }

    fn heartbeat(&self) -> Duration {
        Duration::from_secs(self.heartbeat_secs.load(Ordering::Acquire))
    }

    pub(crate) fn status(&self) -> ReplicationStatus {
        ReplicationStatus {
            role: self.role,
            peer: self.peer,
            connected: self.connected.load(Ordering::Acquire),
            last_delta_age_secs: age_of(self.last_delta_unix.load(Ordering::Acquire)),
            last_heartbeat_age_secs: age_of(self.last_heartbeat_unix.load(Ordering::Acquire)),
            resyncs: self.resyncs.load(Ordering::Acquire),
            snapshots_sent: self.snapshots_sent.load(Ordering::Acquire),
            dropped_deltas: self.dropped_deltas.load(Ordering::Acquire),
            reconnects: self.reconnects.load(Ordering::Acquire),
            resync_pending: self.resync_needed.load(Ordering::Acquire),
        }
    }

    /// JSON shape for the MCP `cluster://status` resource. `status`
    /// follows the leader/follower vocabulary the resource expects.
    pub(crate) fn status_json(&self) -> serde_json::Value {
        let s = self.status();
        let status = match (s.role, s.connected) {
            (ClusterMode::Standalone, _) => "standalone",
            (ClusterMode::Primary, true) => "leader",
            (ClusterMode::Secondary, true) => "follower",
            (ClusterMode::Primary | ClusterMode::Secondary, false) => "degraded",
        };
        let mut v = serde_json::to_value(&s).unwrap_or_else(|_| serde_json::json!({}));
        if let serde_json::Value::Object(map) = &mut v {
            map.insert("status".into(), serde_json::Value::String(status.into()));
            map.insert(
                "peers".into(),
                serde_json::json!(
                    s.peer
                        .map(|p| vec![serde_json::json!({"addr": p, "connected": s.connected})])
                        .unwrap_or_default()
                ),
            );
        }
        v
    }

    fn note_delta(&self) {
        self.last_delta_unix.store(now_unix(), Ordering::Release);
    }
}

/// Primary-side [`Replicator`]: non-blocking hand-off into the
/// service's bounded queue.
pub(crate) struct PrimaryReplicator {
    tx: mpsc::Sender<DialogDelta>,
    state: Arc<ReplicationState>,
}

impl PrimaryReplicator {
    pub(crate) fn new(tx: mpsc::Sender<DialogDelta>, state: Arc<ReplicationState>) -> Self {
        Self { tx, state }
    }
}

impl Replicator for PrimaryReplicator {
    fn replicate(&self, delta: DialogDelta) {
        match self.tx.try_send(delta) {
            Ok(()) => {}
            Err(TrySendError::Full(_) | TrySendError::Closed(_)) => {
                let dropped = self.state.dropped_deltas.fetch_add(1, Ordering::AcqRel) + 1;
                self.state.resync_needed.store(true, Ordering::Release);
                debug!(
                    dropped,
                    "replication queue full/closed; delta dropped, resync flagged"
                );
            }
        }
    }
}

async fn write_frame(socket: &mut TcpStream, frame: &ReplFrame) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(frame).map_err(std::io::Error::other)?;
    line.push(b'\n');
    socket.write_all(&line).await
}

async fn send_snapshot(
    socket: &mut TcpStream,
    dialogs: &DialogTable,
    state: &ReplicationState,
) -> std::io::Result<()> {
    // Clear the flag *before* reading the table, not after. A delta
    // dropped while this snapshot is being built raises the flag
    // again, and that must survive: clearing afterwards would discard
    // the signal for a record this snapshot never saw, and no later
    // delta would carry it either (it was the dropped one).
    state.resync_needed.store(false, Ordering::Release);
    let records: Vec<DialogRecord> = dialogs.iter().map(|e| e.value().clone()).collect();
    let count = records.len();
    if let Err(e) = write_frame(socket, &ReplFrame::Snapshot { records }).await {
        // The snapshot never landed, so the secondary is still stale.
        state.resync_needed.store(true, Ordering::Release);
        return Err(e);
    }
    state.snapshots_sent.fetch_add(1, Ordering::AcqRel);
    state.note_delta();
    debug!(count, "replication snapshot sent");
    Ok(())
}

/// Run the primary replication server on an already-bound
/// listener. Serves one secondary at a time; while none is
/// connected, queued deltas are discarded because the snapshot sent
/// on connect supersedes them. Returns when `cancel` fires or the
/// delta queue closes.
pub(crate) async fn run_primary_service(
    listener: TcpListener,
    mut rx: mpsc::Receiver<DialogDelta>,
    dialogs: Arc<DialogTable>,
    state: Arc<ReplicationState>,
    cancel: CancellationToken,
) -> std::io::Result<()> {
    let mut first = true;
    loop {
        // Wait for a secondary. Deltas arriving meanwhile are
        // consumed and dropped: the connect-time snapshot carries
        // the table they produced.
        let (mut socket, peer) = loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Ok(()),
                accepted = listener.accept() => break accepted?,
                delta = rx.recv() => {
                    if delta.is_none() {
                        return Ok(());
                    }
                }
            }
        };
        if first {
            first = false;
        } else {
            state.reconnects.fetch_add(1, Ordering::AcqRel);
        }
        info!(%peer, "HA secondary connected");
        state.connected.store(true, Ordering::Release);

        if let Err(e) = send_snapshot(&mut socket, &dialogs, &state).await {
            warn!(%peer, ?e, "HA snapshot write failed; waiting for reconnect");
            state.connected.store(false, Ordering::Release);
            continue;
        }

        let disconnect_reason = loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Ok(()),
                delta = rx.recv() => {
                    let Some(delta) = delta else { return Ok(()) };
                    if state.resync_needed.load(Ordering::Acquire) {
                        if let Err(e) = send_snapshot(&mut socket, &dialogs, &state).await {
                            break e;
                        }
                        state.resyncs.fetch_add(1, Ordering::AcqRel);
                    }
                    if let Err(e) = write_frame(&mut socket, &ReplFrame::Delta { delta }).await {
                        break e;
                    }
                    state.note_delta();
                }
                () = tokio::time::sleep(state.heartbeat()) => {
                    let result = if state.resync_needed.load(Ordering::Acquire) {
                        let r = send_snapshot(&mut socket, &dialogs, &state).await;
                        if r.is_ok() {
                            state.resyncs.fetch_add(1, Ordering::AcqRel);
                        }
                        r
                    } else {
                        write_frame(&mut socket, &ReplFrame::Heartbeat).await
                    };
                    if let Err(e) = result {
                        break e;
                    }
                }
            }
        };
        state.connected.store(false, Ordering::Release);
        // Whatever was in flight may not have landed; the next
        // connection starts with a snapshot regardless.
        state.resync_needed.store(true, Ordering::Release);
        warn!(%peer, ?disconnect_reason, "HA secondary disconnected; waiting for reconnect");
    }
}

/// Run the secondary replication client: dial the primary, apply
/// frames into `dialogs`, redial after any failure. Returns when
/// `cancel` fires.
pub(crate) async fn run_secondary_service(
    addr: SocketAddr,
    dialogs: Arc<DialogTable>,
    state: Arc<ReplicationState>,
    cancel: CancellationToken,
    reconnect_delay: Duration,
) {
    let mut first = true;
    loop {
        let socket = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            res = TcpStream::connect(addr) => match res {
                Ok(s) => s,
                Err(e) => {
                    warn!(%addr, ?e, "HA primary unreachable; retrying");
                    tokio::select! {
                        () = cancel.cancelled() => return,
                        () = tokio::time::sleep(reconnect_delay) => continue,
                    }
                }
            },
        };
        if first {
            first = false;
        } else {
            state.reconnects.fetch_add(1, Ordering::AcqRel);
        }
        info!(%addr, "HA secondary connected to primary");
        state.connected.store(true, Ordering::Release);

        let mut lines = BufReader::new(socket).lines();
        loop {
            // Three silent heartbeat intervals = dead primary.
            let idle = state.heartbeat() * 3;
            let next = tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                line = tokio::time::timeout(idle, lines.next_line()) => line,
            };
            match next {
                Ok(Ok(Some(line))) => match serde_json::from_str::<ReplFrame>(&line) {
                    Ok(frame) => apply_frame(&dialogs, &state, frame),
                    Err(e) => warn!(?e, "HA frame from primary unparseable; skipped"),
                },
                Ok(Ok(None)) => {
                    warn!(%addr, "HA primary closed the connection");
                    break;
                }
                Ok(Err(e)) => {
                    warn!(%addr, ?e, "HA replication read failed");
                    break;
                }
                Err(_) => {
                    warn!(%addr, idle_secs = idle.as_secs(), "HA primary silent; reconnecting");
                    break;
                }
            }
        }
        state.connected.store(false, Ordering::Release);
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(reconnect_delay) => {}
        }
    }
}

fn apply_frame(dialogs: &DialogTable, state: &ReplicationState, frame: ReplFrame) {
    match frame {
        ReplFrame::Snapshot { records } => {
            let count = records.len();
            dialogs.clear();
            for record in records {
                dialogs.insert(record.key(), record);
            }
            state.resyncs.fetch_add(1, Ordering::AcqRel);
            state.note_delta();
            info!(count, "HA snapshot applied from primary");
        }
        ReplFrame::Delta { delta } => {
            debug!(?delta, "applying delta from primary");
            match delta {
                DialogDelta::Upsert(record) => {
                    let record = *record;
                    dialogs.insert(record.key(), record);
                }
                DialogDelta::Delete(key) => {
                    dialogs.remove(&key);
                }
            }
            state.note_delta();
        }
        ReplFrame::Heartbeat => {
            state
                .last_heartbeat_unix
                .store(now_unix(), Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use tokio::task::JoinHandle;

    /// Build a record through serde so optional fields added to
    /// `DialogRecord` later take their defaults automatically.
    fn record(n: u32) -> DialogRecord {
        serde_json::from_value(serde_json::json!({
            "call_id": format!("call-{n}"),
            "local_tag": format!("lt-{n}"),
            "remote_tag": format!("rt-{n}"),
            "state": "confirmed",
            "peer_signal": "127.0.0.1:5060",
            "rendezvous": null,
            "media": null,
            "remote_media": null,
        }))
        .unwrap()
    }

    fn call_ids(table: &DialogTable) -> BTreeSet<String> {
        table.iter().map(|e| e.value().call_id.clone()).collect()
    }

    async fn wait_until(mut cond: impl FnMut() -> bool, what: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !cond() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    struct Primary {
        addr: SocketAddr,
        table: Arc<DialogTable>,
        state: Arc<ReplicationState>,
        replicator: PrimaryReplicator,
        _task: JoinHandle<std::io::Result<()>>,
    }

    async fn spawn_primary(queue: usize, cancel: &CancellationToken) -> Primary {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let table: Arc<DialogTable> = Arc::new(DialogTable::new());
        let state = ReplicationState::new(ClusterMode::Primary, Some(addr), 1);
        let (tx, rx) = mpsc::channel(queue);
        let replicator = PrimaryReplicator::new(tx, Arc::clone(&state));
        let task = tokio::spawn(run_primary_service(
            listener,
            rx,
            Arc::clone(&table),
            Arc::clone(&state),
            cancel.clone(),
        ));
        Primary {
            addr,
            table,
            state,
            replicator,
            _task: task,
        }
    }

    fn spawn_secondary(
        addr: SocketAddr,
        cancel: &CancellationToken,
    ) -> (Arc<DialogTable>, Arc<ReplicationState>, JoinHandle<()>) {
        let table: Arc<DialogTable> = Arc::new(DialogTable::new());
        let state = ReplicationState::new(ClusterMode::Secondary, Some(addr), 1);
        let task = tokio::spawn(run_secondary_service(
            addr,
            Arc::clone(&table),
            Arc::clone(&state),
            cancel.clone(),
            Duration::from_millis(50),
        ));
        (table, state, task)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn secondary_receives_snapshot_then_deltas() {
        let cancel = CancellationToken::new();
        let primary = spawn_primary(REPLICATION_QUEUE_DEPTH, &cancel).await;
        // Pre-existing dialogs must reach the secondary via the
        // connect-time snapshot, not via deltas.
        primary.table.insert(record(1).key(), record(1));
        primary.table.insert(record(2).key(), record(2));

        let (sec_table, sec_state, _sec) = spawn_secondary(primary.addr, &cancel);
        wait_until(|| call_ids(&sec_table).len() == 2, "snapshot").await;
        assert_eq!(call_ids(&sec_table), call_ids(&primary.table));

        // Live deltas: upsert a third, delete the first.
        primary.table.insert(record(3).key(), record(3));
        primary
            .replicator
            .replicate(DialogDelta::Upsert(Box::new(record(3))));
        primary.table.remove(&record(1).key());
        primary
            .replicator
            .replicate(DialogDelta::Delete(record(1).key()));
        wait_until(
            || call_ids(&sec_table) == call_ids(&primary.table),
            "delta convergence",
        )
        .await;
        assert_eq!(
            call_ids(&sec_table),
            ["call-2", "call-3"].into_iter().map(String::from).collect()
        );
        // Heartbeats flow at the 1 s test interval.
        wait_until(
            || sec_state.status().last_heartbeat_age_secs.is_some(),
            "heartbeat",
        )
        .await;
        assert!(primary.state.status().connected);
        assert!(sec_state.status().connected);
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dropped_connection_reconnects_and_resyncs_to_convergence() {
        let cancel = CancellationToken::new();
        let primary = spawn_primary(REPLICATION_QUEUE_DEPTH, &cancel).await;
        primary.table.insert(record(1).key(), record(1));

        let sec_cancel = cancel.child_token();
        let (sec_table, _sec_state, sec_task) = spawn_secondary(primary.addr, &sec_cancel);
        wait_until(|| call_ids(&sec_table).len() == 1, "initial snapshot").await;

        // Kill the secondary mid-stream (drops its socket).
        sec_cancel.cancel();
        let _ = sec_task.await;
        // Mutate the primary while nobody is listening: these
        // deltas are consumed and discarded by the primary loop.
        primary.table.insert(record(2).key(), record(2));
        primary
            .replicator
            .replicate(DialogDelta::Upsert(Box::new(record(2))));
        primary.table.remove(&record(1).key());
        primary
            .replicator
            .replicate(DialogDelta::Delete(record(1).key()));
        // Let the primary notice the dead peer (heartbeat write
        // fails within ~1 s) and fall back to accept.
        wait_until(
            || !primary.state.status().connected,
            "primary sees disconnect",
        )
        .await;

        // A fresh secondary must converge from the snapshot alone.
        let (sec_table2, _sec_state2, _sec2) = spawn_secondary(primary.addr, &cancel);
        wait_until(
            || call_ids(&sec_table2) == call_ids(&primary.table),
            "post-reconnect convergence",
        )
        .await;
        assert_eq!(
            call_ids(&sec_table2),
            ["call-2"].into_iter().map(String::from).collect()
        );
        let st = primary.state.status();
        assert_eq!(st.reconnects, 1);
        assert!(st.snapshots_sent >= 2, "{st:?}");
        assert!(!st.resync_pending, "{st:?}");
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn queue_overflow_counts_drops_and_triggers_snapshot_resync() {
        let cancel = CancellationToken::new();
        // Queue depth 1 with no secondary yet: the primary loop
        // drains the queue while waiting for accept, so fill it
        // faster than it drains by pausing the loop — simplest is
        // to build the replicator against a channel nobody reads.
        let (tx, rx) = mpsc::channel(1);
        let state = ReplicationState::new(ClusterMode::Primary, None, 1);
        let replicator = PrimaryReplicator::new(tx, Arc::clone(&state));
        for n in 1..=3 {
            replicator.replicate(DialogDelta::Upsert(Box::new(record(n))));
        }
        let st = state.status();
        assert_eq!(st.dropped_deltas, 2, "{st:?}");
        assert!(st.resync_pending, "{st:?}");

        // Now start the service on that queue: the connect-time
        // snapshot clears the flag, and a later overflow while
        // connected produces a counted resync.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let table: Arc<DialogTable> = Arc::new(DialogTable::new());
        for n in 1..=3 {
            table.insert(record(n).key(), record(n));
        }
        let _primary = tokio::spawn(run_primary_service(
            listener,
            rx,
            Arc::clone(&table),
            Arc::clone(&state),
            cancel.clone(),
        ));
        let (sec_table, _sec_state, _sec) = spawn_secondary(addr, &cancel);
        wait_until(|| call_ids(&sec_table).len() == 3, "snapshot").await;
        assert!(!state.status().resync_pending);

        // Overflow again while connected.
        for n in 4..=40 {
            table.insert(record(n).key(), record(n));
            replicator.replicate(DialogDelta::Upsert(Box::new(record(n))));
        }
        wait_until(
            || call_ids(&sec_table) == call_ids(&table),
            "resync convergence",
        )
        .await;
        let st = state.status();
        assert!(st.dropped_deltas > 2, "{st:?}");
        assert!(st.resyncs >= 1, "{st:?}");
        cancel.cancel();
    }

    #[test]
    fn status_json_reports_role_and_counters() {
        let state = ReplicationState::new(
            ClusterMode::Primary,
            Some("127.0.0.1:9000".parse().unwrap()),
            5,
        );
        let v = state.status_json();
        assert_eq!(v["status"], "degraded");
        assert_eq!(v["role"], "primary");
        assert_eq!(v["connected"], false);
        assert_eq!(v["peers"][0]["addr"], "127.0.0.1:9000");
        state.connected.store(true, Ordering::Release);
        assert_eq!(state.status_json()["status"], "leader");
        let standalone = ReplicationState::new(ClusterMode::Standalone, None, 5);
        assert_eq!(standalone.status_json()["status"], "standalone");
    }
}
