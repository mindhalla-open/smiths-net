//! `OpenRaft` `TypeConfig` definition.

use openraft::RaftTypeConfig;
use smiths_core::DialogDelta;

/// Raft node ID type. We use `u64`.
pub type NodeId = u64;

/// Raft type configuration for `smiths-raft`.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Ord, PartialOrd)]
pub struct SmithsTypeConfig;

impl RaftTypeConfig for SmithsTypeConfig {
    type D = DialogDelta;
    type R = ();
    type NodeId = NodeId;
    type Node = openraft::BasicNode;
    type Entry = openraft::Entry<SmithsTypeConfig>;
    type SnapshotData = std::io::Cursor<Vec<u8>>;
    type AsyncRuntime = openraft::impls::TokioRuntime;
    type Responder = openraft::impls::OneshotResponder<SmithsTypeConfig>;
}

pub type Raft = openraft::Raft<SmithsTypeConfig>;
