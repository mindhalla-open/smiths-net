//! `smiths-raft` — Raft consensus over the engine's dialog table.
//!
//! Selected with `cluster.mode = "raft"`, which replicates the dialog
//! table as a state machine: a write is acknowledged once a quorum
//! holds it, so the cluster survives losing any single node. The
//! simpler `primary` / `secondary` modes mirror one way and do not.
//!
//! - [`node::start_node`] — open the log, serve RPCs, join the cluster.
//! - [`node::RaftReplicator`] — the `smiths_core::Replicator` the SIP
//!   layer writes dialog deltas through.
//! - [`types::SmithsTypeConfig`] — `OpenRaft` type configuration.
//! - [`log_store::SqliteLogStore`] — SQLite-backed log and vote storage.
//! - [`state_machine::DialogStateMachine`] — dialog-table state machine
//!   with snapshot support.
//! - [`network`] — TCP inter-node Raft RPC transport.

pub mod log_store;
pub mod network;
pub mod node;
pub mod state_machine;
pub mod types;

pub use log_store::SqliteLogStore;
pub use network::{SmithsNetworkFactory, run_raft_server};
pub use node::{DialogTable, RaftNode, RaftNodeConfig, RaftReplicator, RaftStartError, start_node};
pub use state_machine::DialogStateMachine;
pub use types::{NodeId, Raft, SmithsTypeConfig};
