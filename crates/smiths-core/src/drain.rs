//! Graceful drain primitive.
//!
//! A [`Drain`] is a cheaply-clonable atomic flag that the shutdown
//! driver flips before actually cancelling subsystems. Consumers
//! check the flag on the hot path and, if set, refuse new work with
//! a 503-style response while letting in-flight work finish.
//!
//! Typical flow:
//! 1. Build one `Drain` at engine startup.
//! 2. Clone it into each subsystem that owns the "accept new work"
//!    decision (UAS, MCP, …).
//! 3. On `SIGTERM`, call [`Drain::start`] and sleep the drain window;
//!    subsystems start rejecting new requests immediately.
//! 4. After the window, fire the existing cancellation token to tear
//!    everything down; any dialogs still live get dropped by the
//!    usual shutdown path.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Shared drain flag. Cloning is cheap (one `Arc` bump).
///
/// `Default` yields a drain that is **not** draining — subsystems can
/// safely be wired with one even when no signal handler exists.
#[derive(Clone, Debug, Default)]
pub struct Drain {
    flag: Arc<AtomicBool>,
}

impl Drain {
    /// Build a fresh drain in the "not draining" state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Flip the flag to "draining". Idempotent; calling twice is a
    /// no-op.
    pub fn start(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    /// Read the current draining state. Relaxed load — the flag
    /// transitions only once, drain-monotonic, so ordering relative
    /// to other data doesn't matter.
    #[must_use]
    pub fn is_draining(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_not_draining() {
        let d = Drain::new();
        assert!(!d.is_draining());
    }

    #[test]
    fn start_propagates_across_clones() {
        let a = Drain::new();
        let b = a.clone();
        assert!(!b.is_draining());
        a.start();
        assert!(b.is_draining(), "clones share the same atomic flag");
    }

    #[test]
    fn start_is_idempotent() {
        let d = Drain::new();
        d.start();
        d.start();
        assert!(d.is_draining());
    }
}
