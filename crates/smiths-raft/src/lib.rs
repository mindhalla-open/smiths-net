//! `smiths-raft` — Raft consensus layer for smiths-net (slice 6.3a/b).
//!
//! Provides:
//! - [`types::SmithsTypeConfig`] — `OpenRaft` type configuration.
//! - [`log_store::SqliteLogStore`] — SQLite-backed log and vote storage.
//! - [`state_machine::DialogStateMachine`] — dialog-table state machine with snapshot support.
//! - [`network`] — TCP-based inter-node Raft RPC transport.

pub mod log_store;
pub mod network;
pub mod state_machine;
pub mod types;

pub use log_store::SqliteLogStore;
pub use network::{SmithsNetworkFactory, run_raft_server};
pub use state_machine::DialogStateMachine;
pub use types::{NodeId, Raft, SmithsTypeConfig};
