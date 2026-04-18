//! Typed event bus built on `tokio::sync::broadcast`.
//!
//! Subscribers receive every event published after they subscribe.
//! Slow subscribers that fall behind receive
//! [`tokio::sync::broadcast::error::RecvError::Lagged`] and must decide
//! how to resync.

use tokio::sync::broadcast;

use crate::{Error, Event};

/// Shared handle to the engine's event bus.
///
/// Cheap to clone — internally an [`Arc`] around the broadcast channel.
///
/// [`Arc`]: std::sync::Arc
#[derive(Clone, Debug)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
}

impl EventBus {
    /// Create a bus with a per-subscriber buffer of `capacity` events.
    ///
    /// A capacity of 1024 is a sensible default during MVP; tune later
    /// based on subscriber count and publish rate.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Publish an event to every live subscriber.
    ///
    /// Returns the number of subscribers that received the event, or
    /// [`Error::BusClosed`] if the bus has no receivers.
    pub fn publish(&self, event: Event) -> Result<usize, Error> {
        self.tx.send(event).map_err(|_| Error::BusClosed)
    }

    /// Subscribe to the bus. The returned receiver must be polled
    /// frequently enough to avoid `Lagged` errors.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    /// Current live subscriber count.
    #[must_use]
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SystemEvent;

    #[tokio::test]
    async fn publish_reaches_subscribers() {
        let bus = EventBus::new(16);
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();

        let delivered = bus.publish(Event::System(SystemEvent::Ready)).unwrap();
        assert_eq!(delivered, 2);

        for rx in [&mut rx1, &mut rx2] {
            let got = rx.recv().await.unwrap();
            assert!(matches!(got, Event::System(SystemEvent::Ready)));
        }
    }

    #[test]
    fn publish_without_subscribers_errors() {
        let bus = EventBus::new(4);
        let err = bus.publish(Event::System(SystemEvent::Ready));
        assert!(matches!(err, Err(Error::BusClosed)));
    }
}
