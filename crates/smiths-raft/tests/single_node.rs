//! A real Raft node: elect, commit a dialog delta through the
//! `Replicator` seam the SIP layer uses, and survive a restart.

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use smiths_core::call::DialogRecord;
use smiths_core::{DialogDelta, Replicator};
use smiths_raft::{DialogTable, RaftNodeConfig, RaftReplicator, start_node};
use tokio_util::sync::CancellationToken;

/// Build a record through serde so fields added later take defaults.
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
    .expect("record")
}

/// An unused loopback port, released before the caller binds it.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

async fn wait_until(mut cond: impl FnMut() -> bool, what: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !cond() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn single_node_elects_itself_and_commits_a_delta() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cancel = CancellationToken::new();
    let port = free_port();
    let cfg = RaftNodeConfig {
        node_id: 1,
        raft_addr: format!("127.0.0.1:{port}").parse().expect("addr"),
        raft_dir: dir.path().to_path_buf(),
        initial_peers: Vec::new(),
    };
    let dialogs: Arc<DialogTable> = Arc::new(DashMap::new());
    let node = start_node(cfg, Arc::clone(&dialogs), cancel.clone())
        .await
        .expect("node starts");

    // A single-node cluster is its own quorum, so it must become
    // leader without anyone else voting.
    let metrics = node.raft.metrics();
    wait_until(
        || metrics.borrow().current_leader == Some(1),
        "self-election",
    )
    .await;

    let (replicator, pump) = RaftReplicator::new(node.raft.clone(), cancel.clone());
    replicator.replicate(DialogDelta::Upsert(Box::new(record(1))));
    replicator.replicate(DialogDelta::Upsert(Box::new(record(2))));

    wait_until(|| dialogs.len() == 2, "both deltas applied").await;
    assert_eq!(replicator.applied(), 2, "both writes committed");
    assert_eq!(replicator.dropped(), 0);
    assert_eq!(replicator.not_leader(), 0, "the leader accepts its writes");

    // A delete replicates the same way.
    let key = record(1).key();
    replicator.replicate(DialogDelta::Delete(key.clone()));
    wait_until(|| !dialogs.contains_key(&key), "delete applied").await;

    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), pump).await;
    for t in node.tasks {
        t.abort();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_rejoins_the_existing_log_and_keeps_applied_state() {
    let dir = tempfile::tempdir().expect("tempdir");
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let mk = || RaftNodeConfig {
        node_id: 1,
        raft_addr: addr.parse().expect("addr"),
        raft_dir: dir.path().to_path_buf(),
        initial_peers: vec![format!("1@{addr}")],
    };

    // First boot: initialize and commit one record.
    let cancel = CancellationToken::new();
    let dialogs: Arc<DialogTable> = Arc::new(DashMap::new());
    let node = start_node(mk(), Arc::clone(&dialogs), cancel.clone())
        .await
        .expect("first start");
    let metrics = node.raft.metrics();
    wait_until(|| metrics.borrow().current_leader == Some(1), "election").await;
    let (replicator, pump) = RaftReplicator::new(node.raft.clone(), cancel.clone());
    replicator.replicate(DialogDelta::Upsert(Box::new(record(7))));
    wait_until(|| dialogs.len() == 1, "delta applied").await;

    // Shut the node down completely before reopening the log: SQLite
    // will not hand the same file to a second writer.
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), pump).await;
    node.raft.shutdown().await.expect("clean shutdown");
    for t in node.tasks {
        let _ = tokio::time::timeout(Duration::from_secs(5), t).await;
    }
    drop(node.raft);
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Second boot against the same directory: `initialize` is refused
    // because membership is already in the log, and the committed
    // entry replays into a fresh table.
    let cancel2 = CancellationToken::new();
    let dialogs2: Arc<DialogTable> = Arc::new(DashMap::new());
    let node2 = start_node(mk(), Arc::clone(&dialogs2), cancel2.clone())
        .await
        .expect("restart must not error on an existing log");
    let metrics2 = node2.raft.metrics();
    wait_until(
        || metrics2.borrow().current_leader == Some(1),
        "re-election after restart",
    )
    .await;
    wait_until(|| dialogs2.len() == 1, "committed entry replayed").await;
    assert!(
        dialogs2.contains_key(&record(7).key()),
        "the pre-restart record survived"
    );

    cancel2.cancel();
    node2.raft.shutdown().await.expect("clean shutdown");
    for t in node2.tasks {
        t.abort();
    }
}

#[tokio::test]
async fn malformed_initial_peer_is_a_clear_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = RaftNodeConfig {
        node_id: 1,
        raft_addr: "127.0.0.1:0".parse().expect("addr"),
        raft_dir: dir.path().to_path_buf(),
        initial_peers: vec!["not-a-peer".into()],
    };
    let Err(err) = start_node(cfg, Arc::new(DashMap::new()), CancellationToken::new()).await else {
        panic!("a malformed peer entry must be rejected");
    };
    let msg = err.to_string();
    assert!(msg.contains("not-a-peer"), "{msg}");
    assert!(msg.contains("node_id@host:port"), "{msg}");
}
