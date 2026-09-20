//! HA plumbing: dialog snapshot I/O and the primary / secondary
//! replication wiring.
//!
//! The snapshot file (`--snapshot-path`) is read once at boot and
//! written once at shutdown; replication runs continuously between
//! a primary and a secondary. Both roles share one dialog table
//! across every SIP bind so the snapshot and the replication stream
//! cover the whole engine.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use smiths_core::{ClusterConfig, ClusterMode, DialogRecord, NoopReplicator, Replicator};
use smiths_mcp::cluster::ClusterStatusSource;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::replication_service::{
    DEFAULT_RECONNECT_DELAY, DialogTable, PrimaryReplicator, REPLICATION_QUEUE_DEPTH,
    ReplicationState, run_primary_service, run_secondary_service,
};

/// Everything the SIP listeners and the control plane need from the
/// HA layer.
pub(crate) struct HaRuntime {
    /// Replicator every UAS publishes deltas through.
    pub replicator: Arc<dyn Replicator>,
    /// Dialog table shared by every SIP bind (`Some` outside
    /// standalone mode).
    pub dialogs: Option<Arc<DialogTable>>,
    /// Live replication counters, when replication is on.
    pub state: Option<Arc<ReplicationState>>,
    /// Background service tasks to join on shutdown.
    pub tasks: Vec<JoinHandle<()>>,
}

/// Read the HA snapshot if a path was given. Returns the restored
/// records plus the path to write back to on shutdown.
pub(crate) fn load_snapshot(path: Option<&Path>) -> (Vec<DialogRecord>, Option<PathBuf>) {
    let Some(path) = path else {
        return (Vec::new(), None);
    };
    let records = match smiths_sip::read_snapshot(path) {
        Ok(Some(records)) => {
            info!(
                path = %path.display(),
                count = records.len(),
                "HA snapshot loaded; dialogs will be replayed on the first SIP bind"
            );
            records
        }
        Ok(None) => {
            info!(path = %path.display(), "HA snapshot file absent; cold boot");
            Vec::new()
        }
        Err(e) => {
            warn!(path = %path.display(), ?e, "HA snapshot unreadable; cold boot");
            Vec::new()
        }
    };
    (records, Some(path.to_path_buf()))
}

/// Serialize the live dialog table to `sink`. Called after every SIP
/// task has joined so the table is quiescent.
pub(crate) fn write_snapshot(sink: Option<&Path>, dialogs: Option<&Arc<DialogTable>>) {
    let (Some(path), Some(dialogs)) = (sink, dialogs) else {
        return;
    };
    match smiths_sip::write_snapshot(path, dialogs) {
        Ok(n) => info!(path = %path.display(), count = n, "HA snapshot written"),
        Err(e) => warn!(path = %path.display(), ?e, "HA snapshot write failed"),
    }
}

/// Bring up replication for the configured role. Binding the
/// primary's listener happens here so a bad `peer_addr` fails boot
/// instead of a background task.
pub(crate) async fn start(
    cluster: &ClusterConfig,
    cancel: CancellationToken,
) -> anyhow::Result<HaRuntime> {
    let heartbeat = u64::from(cluster.heartbeat_interval_secs);
    match (cluster.mode, cluster.peer_addr) {
        (ClusterMode::Standalone, _) => Ok(HaRuntime {
            replicator: Arc::new(NoopReplicator),
            dialogs: None,
            state: None,
            tasks: Vec::new(),
        }),
        (mode, None) => {
            // `Config::validate` refuses this; keep the guard so a
            // caller that skipped validation still fails loudly.
            anyhow::bail!("cluster.mode = {mode:?} requires cluster.peer_addr")
        }
        (ClusterMode::Primary, Some(bind)) => {
            let listener = TcpListener::bind(bind)
                .await
                .with_context(|| format!("binding HA replication listener on {bind}"))?;
            let bound = listener.local_addr().unwrap_or(bind);
            info!(%bound, "HA primary replication service listening");
            let dialogs: Arc<DialogTable> = Arc::new(DialogTable::new());
            let state = ReplicationState::new(ClusterMode::Primary, Some(bound), heartbeat);
            let (tx, rx) = mpsc::channel(REPLICATION_QUEUE_DEPTH);
            let replicator: Arc<dyn Replicator> =
                Arc::new(PrimaryReplicator::new(tx, Arc::clone(&state)));
            let task = {
                let dialogs = Arc::clone(&dialogs);
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    if let Err(e) = run_primary_service(listener, rx, dialogs, state, cancel).await
                    {
                        tracing::error!(?e, "HA primary replication service failed");
                    }
                })
            };
            Ok(HaRuntime {
                replicator,
                dialogs: Some(dialogs),
                state: Some(state),
                tasks: vec![task],
            })
        }
        (ClusterMode::Secondary, Some(peer)) => {
            let dialogs: Arc<DialogTable> = Arc::new(DialogTable::new());
            let state = ReplicationState::new(ClusterMode::Secondary, Some(peer), heartbeat);
            let task = {
                let dialogs = Arc::clone(&dialogs);
                let state = Arc::clone(&state);
                tokio::spawn(run_secondary_service(
                    peer,
                    dialogs,
                    state,
                    cancel,
                    DEFAULT_RECONNECT_DELAY,
                ))
            };
            Ok(HaRuntime {
                replicator: Arc::new(NoopReplicator),
                dialogs: Some(dialogs),
                state: Some(state),
                tasks: vec![task],
            })
        }
    }
}

/// `cluster://status` source: the live replication counters, or a
/// static standalone report when replication is off.
pub(crate) fn cluster_status_source(
    state: Option<Arc<ReplicationState>>,
) -> Arc<dyn ClusterStatusSource> {
    match state {
        Some(state) => Arc::new(move || state.status_json()),
        None => Arc::new(|| {
            serde_json::json!({
                "status": "standalone",
                "role": "standalone",
                "connected": false,
                "peers": [],
            })
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_snapshot_without_path_is_cold_boot() {
        let (records, sink) = load_snapshot(None);
        assert!(records.is_empty());
        assert!(sink.is_none());
    }

    #[test]
    fn load_snapshot_missing_file_keeps_sink_for_writeback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dialogs.json");
        let (records, sink) = load_snapshot(Some(&path));
        assert!(records.is_empty());
        assert_eq!(sink.as_deref(), Some(path.as_path()));
    }

    #[tokio::test]
    async fn start_refuses_role_without_peer_addr() {
        let cluster = ClusterConfig {
            mode: ClusterMode::Primary,
            ..ClusterConfig::default()
        };
        let Err(err) = start(&cluster, CancellationToken::new()).await else {
            panic!("a role without peer_addr must be refused");
        };
        assert!(err.to_string().contains("peer_addr"), "{err}");
    }

    #[tokio::test]
    async fn primary_binds_listener_at_start_and_reports_status() {
        let cluster = ClusterConfig {
            mode: ClusterMode::Primary,
            peer_addr: Some("127.0.0.1:0".parse().unwrap()),
            ..ClusterConfig::default()
        };
        let cancel = CancellationToken::new();
        let rt = start(&cluster, cancel.clone()).await.unwrap();
        assert!(rt.dialogs.is_some());
        let status = cluster_status_source(rt.state.clone()).status();
        assert_eq!(status["status"], "degraded");
        assert_eq!(status["role"], "primary");
        cancel.cancel();
        for t in rt.tasks {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), t).await;
        }
    }

    #[test]
    fn standalone_status_source_is_static() {
        let v = cluster_status_source(None).status();
        assert_eq!(v["status"], "standalone");
        assert_eq!(v["peers"], serde_json::json!([]));
    }
}
