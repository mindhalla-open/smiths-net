//! Replication abstraction for HA (slice 6.2).
//!
//! Modules that mutate state (UAS) call the [`Replicator`] to broadcast
//! deltas to the secondary. Standalone deployments use a no-op
//! implementation.

use crate::DialogDelta;

/// Seam for broadcasting state mutations to an HA peer.
///
/// Typical implementations:
/// - `Standalone`: No-op.
/// - `Primary`: Streams deltas over TCP to the secondary.
pub trait Replicator: Send + Sync {
    /// Send a delta to the replication stream.
    fn replicate(&self, delta: DialogDelta);
}

/// No-op replicator for standalone deployments.
pub struct NoopReplicator;

impl Replicator for NoopReplicator {
    fn replicate(&self, _delta: DialogDelta) {}
}
