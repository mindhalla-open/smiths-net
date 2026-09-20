//! Bringing a Raft node up and writing dialog deltas through it.
//!
//! [`start_node`] opens the on-disk log, builds the state machine over
//! the engine's live dialog table, starts the RPC listener and (on a
//! fresh cluster) initializes membership. [`RaftReplicator`] is the
//! `smiths_core::Replicator` the SIP layer calls on every dialog
//! change; it hands each delta to `client_write` so the entry is only
//! applied once a quorum has it.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use openraft::BasicNode;
use smiths_core::call::{DialogKey, DialogRecord};
use smiths_core::{DialogDelta, Replicator};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::log_store::SqliteLogStore;
use crate::network::{SmithsNetworkFactory, run_raft_server};
use crate::state_machine::DialogStateMachine;
use crate::types::{NodeId, Raft};

/// Deltas buffered between the SIP request path and the task that
/// awaits `client_write`. The SIP path must never block, so the queue
/// is bounded and overflow is counted rather than awaited.
const WRITE_QUEUE_DEPTH: usize = 1024;

/// The live dialog table the state machine applies committed entries
/// into. The engine shares this exact map with the SIP layer.
pub type DialogTable = DashMap<DialogKey, DialogRecord>;

/// Everything [`start_node`] needs to bring one node up.
#[derive(Clone, Debug)]
pub struct RaftNodeConfig {
    /// This node's cluster-unique id.
    pub node_id: NodeId,
    /// Bind address for inter-node Raft RPC.
    pub raft_addr: SocketAddr,
    /// Directory holding this node's `SQLite` log. Created if absent.
    pub raft_dir: PathBuf,
    /// Cluster members as `"node_id@host:port"`, including this node.
    /// Empty means a single-node cluster consisting of this node.
    pub initial_peers: Vec<String>,
}

/// Why a node could not be started.
#[derive(Debug, thiserror::Error)]
pub enum RaftStartError {
    /// `initial_peers` held an entry that is not `"node_id@host:port"`.
    #[error("cluster.initial_peers entry `{entry}` is not `node_id@host:port`: {reason}")]
    PeerSyntax {
        /// The offending entry, verbatim.
        entry: String,
        /// What specifically failed to parse.
        reason: String,
    },
    /// The log directory could not be created or opened.
    #[error("raft log at `{path}`: {source}")]
    LogStore {
        /// Directory or file the store tried to use.
        path: String,
        /// Underlying IO / `SQLite` error.
        source: anyhow::Error,
    },
    /// The RPC listener could not bind.
    #[error("binding raft rpc listener on {addr}: {source}")]
    Bind {
        /// Address that failed to bind.
        addr: SocketAddr,
        /// Underlying IO error.
        source: std::io::Error,
    },
    /// `openraft` refused to construct the node.
    #[error("raft init: {0}")]
    Raft(String),
}

/// Parse one `"node_id@host:port"` cluster member.
fn parse_peer(entry: &str) -> Result<(NodeId, BasicNode), RaftStartError> {
    let bad = |reason: &str| RaftStartError::PeerSyntax {
        entry: entry.to_owned(),
        reason: reason.to_owned(),
    };
    let (id, addr) = entry.split_once('@').ok_or_else(|| bad("missing `@`"))?;
    let id: NodeId = id
        .trim()
        .parse()
        .map_err(|_| bad("node id is not a number"))?;
    let addr = addr.trim();
    if addr.is_empty() {
        return Err(bad("empty address"));
    }
    // Resolve-ability is the transport's problem; require only that a
    // port is present, since `host` alone would silently never connect.
    if !addr.rsplit_once(':').is_some_and(|(_, p)| {
        !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) && p.parse::<u16>().is_ok()
    }) {
        return Err(bad("address has no `:port`"));
    }
    Ok((
        id,
        BasicNode {
            addr: addr.to_owned(),
        },
    ))
}

/// Build the initial membership map: every configured peer, plus this
/// node itself (a config that lists only the other nodes still yields
/// a cluster this node belongs to).
fn membership(cfg: &RaftNodeConfig) -> Result<BTreeMap<NodeId, BasicNode>, RaftStartError> {
    let mut members = BTreeMap::new();
    for entry in &cfg.initial_peers {
        let (id, node) = parse_peer(entry)?;
        members.insert(id, node);
    }
    members.entry(cfg.node_id).or_insert_with(|| BasicNode {
        addr: cfg.raft_addr.to_string(),
    });
    Ok(members)
}

/// A running Raft node: the consensus handle, its background tasks,
/// and the dialog table committed entries land in.
pub struct RaftNode {
    /// Consensus handle. Cloneable; safe to share across tasks.
    pub raft: Raft,
    /// The table the state machine applies into, shared with the
    /// engine's SIP layer.
    pub dialogs: Arc<DialogTable>,
    /// This node's id.
    pub node_id: NodeId,
    /// RPC listener + write-pump tasks, for bounded shutdown joins.
    pub tasks: Vec<JoinHandle<()>>,
}

/// Open the log, build the state machine over `dialogs`, start the RPC
/// listener and initialize membership when the log is empty.
///
/// Re-running this against an existing `raft_dir` is the restart path:
/// membership already lives in the log, so initialization is skipped
/// and the node rejoins with its persisted vote and entries.
///
/// # Errors
///
/// See [`RaftStartError`].
pub async fn start_node(
    cfg: RaftNodeConfig,
    dialogs: Arc<DialogTable>,
    cancel: CancellationToken,
) -> Result<RaftNode, RaftStartError> {
    let members = membership(&cfg)?;

    std::fs::create_dir_all(&cfg.raft_dir).map_err(|e| RaftStartError::LogStore {
        path: cfg.raft_dir.display().to_string(),
        source: e.into(),
    })?;
    let log_path = log_path(&cfg.raft_dir, cfg.node_id);
    let log_store = SqliteLogStore::new(&log_path).map_err(|source| RaftStartError::LogStore {
        path: log_path.display().to_string(),
        source,
    })?;

    // Bind before constructing the node so a port clash fails fast
    // rather than after the log is open.
    let listener = tokio::net::TcpListener::bind(cfg.raft_addr)
        .await
        .map_err(|source| RaftStartError::Bind {
            addr: cfg.raft_addr,
            source,
        })?;
    let bound = listener.local_addr().unwrap_or(cfg.raft_addr);
    drop(listener);

    let state_machine = DialogStateMachine::new(Arc::clone(&dialogs));
    let config = Arc::new(
        openraft::Config {
            cluster_name: "smiths-net".to_owned(),
            election_timeout_min: 300,
            election_timeout_max: 600,
            heartbeat_interval: 100,
            ..openraft::Config::default()
        }
        .validate()
        .map_err(|e| RaftStartError::Raft(e.to_string()))?,
    );

    let raft = Raft::new(
        cfg.node_id,
        config,
        SmithsNetworkFactory,
        log_store,
        state_machine,
    )
    .await
    .map_err(|e| RaftStartError::Raft(e.to_string()))?;

    let tasks = vec![spawn_rpc_server(bound, raft.clone(), cancel.clone())];

    // `initialize` is how a brand-new cluster gets its first
    // membership entry. On a restart the log already carries one and
    // openraft refuses — that refusal is the expected path, not a
    // failure, so it is logged and ignored.
    match raft.initialize(members.clone()).await {
        Ok(()) => info!(
            node_id = cfg.node_id,
            members = members.len(),
            "raft cluster initialized"
        ),
        Err(e) => debug!(
            node_id = cfg.node_id,
            ?e,
            "raft already initialized; rejoining from the existing log"
        ),
    }

    info!(node_id = cfg.node_id, %bound, members = members.len(), "raft node started");
    Ok(RaftNode {
        raft,
        dialogs,
        node_id: cfg.node_id,
        tasks,
    })
}

impl RaftNode {
    /// Live cluster status for the `cluster://status` MCP resource.
    ///
    /// Every field is read from `openraft`'s own metrics, so a node
    /// that has lost quorum reports `follower` / `candidate` with no
    /// leader rather than claiming to be healthy.
    #[must_use]
    pub fn status_json(&self) -> serde_json::Value {
        let m = self.raft.metrics().borrow().clone();
        let leader = m.current_leader;
        let is_leader = leader == Some(self.node_id);
        // Without a leader the cluster cannot commit, which is the
        // one thing an operator needs to see at a glance.
        let status = match (leader.is_some(), is_leader) {
            (true, true) => "leader",
            (true, false) => "follower",
            (false, _) => "no-quorum",
        };
        let peers: Vec<serde_json::Value> = m
            .membership_config
            .nodes()
            .map(|(id, node)| serde_json::json!({ "node_id": id, "addr": node.addr }))
            .collect();
        serde_json::json!({
            "status": status,
            "role": "raft",
            "node_id": self.node_id,
            "leader": leader,
            "is_leader": is_leader,
            "term": m.current_term,
            "state": format!("{:?}", m.state),
            "last_applied": m.last_applied.map(|l| l.index),
            "last_log_index": m.last_log_index,
            "connected": leader.is_some(),
            "peers": peers,
        })
    }
}

/// Per-node log file inside the configured directory, so several
/// nodes can share one directory in tests and single-host demos.
fn log_path(dir: &Path, node_id: NodeId) -> PathBuf {
    dir.join(format!("raft-{node_id}.sqlite"))
}

/// Serve Raft RPCs until `cancel` fires.
fn spawn_rpc_server(addr: SocketAddr, raft: Raft, cancel: CancellationToken) -> JoinHandle<()> {
    tokio::spawn(async move {
        tokio::select! {
            () = cancel.cancelled() => {}
            r = run_raft_server(addr, raft) => {
                if let Err(e) = r {
                    warn!(%addr, ?e, "raft rpc server stopped");
                }
            }
        }
    })
}

/// Counters describing what the replicator did with submitted deltas.
#[derive(Debug, Default)]
struct WriteCounters {
    submitted: AtomicU64,
    applied: AtomicU64,
    not_leader: AtomicU64,
    dropped: AtomicU64,
}

/// [`Replicator`] that commits each dialog delta through Raft.
///
/// `Replicator::replicate` is synchronous and must not block the SIP
/// request path, while `client_write` is async and only succeeds on
/// the leader. So deltas go into a bounded queue that a background
/// task drains. A follower has nothing useful to do with a local
/// write — the leader's own replicator already committed it, and the
/// entry arrives here through `AppendEntries` — so a rejected write
/// is counted and dropped rather than retried in a loop.
pub struct RaftReplicator {
    tx: mpsc::Sender<DialogDelta>,
    counters: Arc<WriteCounters>,
}

impl RaftReplicator {
    /// Build the replicator and the task that drains its queue.
    #[must_use]
    pub fn new(raft: Raft, cancel: CancellationToken) -> (Self, JoinHandle<()>) {
        let (tx, mut rx) = mpsc::channel::<DialogDelta>(WRITE_QUEUE_DEPTH);
        let counters = Arc::new(WriteCounters::default());
        let task_counters = Arc::clone(&counters);
        let task = tokio::spawn(async move {
            loop {
                let delta = tokio::select! {
                    biased;
                    () = cancel.cancelled() => break,
                    d = rx.recv() => match d {
                        Some(d) => d,
                        None => break,
                    },
                };
                match raft.client_write(delta).await {
                    Ok(_) => {
                        task_counters.applied.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        task_counters.not_leader.fetch_add(1, Ordering::Relaxed);
                        debug!(?e, "raft client_write rejected; delta dropped on this node");
                    }
                }
            }
        });
        (Self { tx, counters }, task)
    }

    /// Deltas handed to the queue.
    #[must_use]
    pub fn submitted(&self) -> u64 {
        self.counters.submitted.load(Ordering::Relaxed)
    }

    /// Deltas committed through Raft.
    #[must_use]
    pub fn applied(&self) -> u64 {
        self.counters.applied.load(Ordering::Relaxed)
    }

    /// Writes Raft refused, almost always "not the leader".
    #[must_use]
    pub fn not_leader(&self) -> u64 {
        self.counters.not_leader.load(Ordering::Relaxed)
    }

    /// Deltas dropped because the queue was full or closed.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.counters.dropped.load(Ordering::Relaxed)
    }
}

impl Replicator for RaftReplicator {
    fn replicate(&self, delta: DialogDelta) {
        self.counters.submitted.fetch_add(1, Ordering::Relaxed);
        if self.tx.try_send(delta).is_err() {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            warn!("raft write queue full or closed; dialog delta dropped");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_peer_accepts_id_at_host_port() {
        let (id, node) = parse_peer("7@10.0.0.1:9000").expect("valid peer");
        assert_eq!(id, 7);
        assert_eq!(node.addr, "10.0.0.1:9000");
    }

    #[test]
    fn parse_peer_rejects_malformed_entries() {
        for bad in [
            "10.0.0.1:9000",   // no id
            "x@10.0.0.1:9000", // id is not a number
            "7@",              // empty address
            "7@10.0.0.1",      // no port
            "7@10.0.0.1:http", // non-numeric port
        ] {
            let err = parse_peer(bad).expect_err(bad);
            assert!(
                matches!(&err, RaftStartError::PeerSyntax { entry, .. } if entry == bad),
                "{bad}: {err}"
            );
        }
    }

    #[test]
    fn membership_always_includes_this_node() {
        let cfg = RaftNodeConfig {
            node_id: 1,
            raft_addr: "127.0.0.1:9100".parse().unwrap(),
            raft_dir: PathBuf::from("/tmp"),
            initial_peers: vec!["2@127.0.0.1:9101".into()],
        };
        let m = membership(&cfg).expect("valid");
        assert_eq!(m.len(), 2);
        assert_eq!(m[&1].addr, "127.0.0.1:9100");
        assert_eq!(m[&2].addr, "127.0.0.1:9101");
    }

    #[test]
    fn membership_keeps_an_explicit_self_entry() {
        let cfg = RaftNodeConfig {
            node_id: 1,
            raft_addr: "127.0.0.1:9100".parse().unwrap(),
            raft_dir: PathBuf::from("/tmp"),
            initial_peers: vec!["1@10.0.0.9:9000".into()],
        };
        let m = membership(&cfg).expect("valid");
        assert_eq!(m.len(), 1);
        assert_eq!(
            m[&1].addr, "10.0.0.9:9000",
            "an explicit entry wins over the local bind"
        );
    }

    #[test]
    fn log_path_is_per_node() {
        assert_eq!(
            log_path(Path::new("/var/lib/smiths"), 3),
            PathBuf::from("/var/lib/smiths/raft-3.sqlite")
        );
    }
}
