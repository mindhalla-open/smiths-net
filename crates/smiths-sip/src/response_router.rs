//! Response correlation for locally-originated SIP requests.
//!
//! The engine owns one socket per SIP transport bind, and the UAS
//! reader loop receives everything — requests *and* responses. When
//! the transaction driver starts a client transaction it registers
//! the Via-branch it used and receives a channel; the UAS, on seeing
//! a response, calls [`ResponseRouter::deliver`] to push the bytes
//! into that channel. Unknown branches are dropped with a debug log.
//!
//! A subscription stays live until [`ResponseRouter::cancel`] (or the
//! receiver is dropped), so every response on a branch — a `100
//! Trying` immediately followed by the `200 OK`, a retransmitted
//! `2xx` arriving after the transaction closed — reaches the
//! subscriber in order without a re-subscribe window in between.

use std::sync::Arc;

use bytes::Bytes;
use dashmap::DashMap;
use tokio::sync::mpsc;
use tracing::debug;

/// Thread-safe response correlator keyed by the Via `branch`.
#[derive(Clone, Default)]
pub struct ResponseRouter {
    pending: Arc<DashMap<String, mpsc::UnboundedSender<Bytes>>>,
}

impl ResponseRouter {
    /// Build an empty router. Cheap to clone.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register interest in every response carrying `branch`. Returns
    /// a receiver that yields each delivered response in arrival
    /// order until [`Self::cancel`] runs for the branch. Replaces any
    /// prior subscription for the same branch (its receiver then
    /// observes end-of-stream).
    ///
    /// Unbounded so [`Self::deliver`] never blocks the transport
    /// reader; a transaction receives a handful of responses over its
    /// lifetime, so the buffer stays tiny in practice.
    pub fn subscribe(&self, branch: impl Into<String>) -> mpsc::UnboundedReceiver<Bytes> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.pending.insert(branch.into(), tx);
        rx
    }

    /// Deliver the response bytes to whoever subscribed on `branch`.
    /// Returns `true` when a subscriber was notified, `false` when the
    /// branch was unknown (stale response / no UAC) or its receiver
    /// has gone away — in which case the dead entry is dropped.
    pub fn deliver(&self, branch: &str, bytes: Bytes) -> bool {
        let Some(entry) = self.pending.get(branch) else {
            debug!(branch, "no subscriber for response branch");
            return false;
        };
        if entry.value().send(bytes).is_ok() {
            return true;
        }
        drop(entry);
        debug!(branch, "response subscriber dropped before delivery");
        self.pending.remove(branch);
        false
    }

    /// Drop an outstanding subscription. Its receiver observes
    /// end-of-stream on the next `recv`. No-op for unknown branches.
    pub fn cancel(&self, branch: &str) {
        self.pending.remove(branch);
    }

    /// Outstanding subscription count. Useful for tests and metrics.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// `true` when no subscriptions are active.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[tokio::test(flavor = "multi_thread")]
    async fn deliver_resolves_subscriber_and_keeps_subscription() {
        let r = ResponseRouter::new();
        let mut rx = r.subscribe("br-1");
        assert!(r.deliver("br-1", Bytes::from_static(b"100 Trying")));
        assert!(r.deliver("br-1", Bytes::from_static(b"200 OK")));
        assert_eq!(&rx.recv().await.unwrap()[..], b"100 Trying");
        assert_eq!(&rx.recv().await.unwrap()[..], b"200 OK");
        assert_eq!(r.len(), 1, "delivery must not consume the subscription");
        r.cancel("br-1");
        assert!(r.is_empty());
        assert!(rx.recv().await.is_none(), "cancel closes the channel");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deliver_on_unknown_branch_returns_false() {
        let r = ResponseRouter::new();
        assert!(!r.deliver("nope", Bytes::from_static(b"x")));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deliver_to_dropped_receiver_evicts_entry() {
        let r = ResponseRouter::new();
        let rx = r.subscribe("br-dead");
        drop(rx);
        assert!(!r.deliver("br-dead", Bytes::from_static(b"x")));
        assert!(r.is_empty(), "dead subscription must be evicted");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_drops_subscription() {
        let r = ResponseRouter::new();
        let mut rx = r.subscribe("br-2");
        r.cancel("br-2");
        assert!(r.is_empty());
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn second_subscribe_replaces_first() {
        let r = ResponseRouter::new();
        let mut rx1 = r.subscribe("br-3");
        let mut rx2 = r.subscribe("br-3");
        assert!(r.deliver("br-3", Bytes::from_static(b"hit")));
        // Second subscription wins; the first channel is closed.
        assert!(rx1.recv().await.is_none());
        assert_eq!(&rx2.recv().await.unwrap()[..], b"hit");
    }
}
