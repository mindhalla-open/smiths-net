//! Graceful shutdown coordinator.
//!
//! A single [`Shutdown`] value owns a [`CancellationToken`] that all
//! long-lived tasks clone. Signals (`SIGINT`/`SIGTERM` on unix,
//! `Ctrl-C` elsewhere) or an explicit [`Shutdown::trigger`] cancel the
//! token. Tasks watch the token to stop accepting new work and drain.

use tokio::signal;
use tokio_util::sync::CancellationToken;

/// Shutdown coordinator handle. Cheap to clone.
#[derive(Clone, Debug, Default)]
pub struct Shutdown {
    token: CancellationToken,
}

impl Shutdown {
    /// Create a fresh shutdown coordinator.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Return a clone of the underlying cancellation token. Give one to
    /// every long-lived task.
    #[must_use]
    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    /// Trigger shutdown programmatically (in addition to OS signals).
    pub fn trigger(&self) {
        self.token.cancel();
    }

    /// `true` once shutdown has been triggered.
    #[must_use]
    pub fn is_triggered(&self) -> bool {
        self.token.is_cancelled()
    }

    /// Wait for a shutdown signal, then cancel the token.
    ///
    /// On unix listens for `SIGTERM` and `SIGINT`. Elsewhere listens for
    /// `Ctrl-C`. Also returns if the token is cancelled by
    /// [`Shutdown::trigger`] on another handle.
    pub async fn wait_for_signal(&self) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use signal::unix::{SignalKind, signal};
            let mut sigterm = signal(SignalKind::terminate())?;
            let mut sigint = signal(SignalKind::interrupt())?;
            tokio::select! {
                _ = sigterm.recv() => {},
                _ = sigint.recv() => {},
                () = self.token.cancelled() => {},
            }
        }
        #[cfg(not(unix))]
        {
            tokio::select! {
                _ = signal::ctrl_c() => {},
                () = self.token.cancelled() => {},
            }
        }
        self.token.cancel();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn trigger_cancels_token() {
        let s = Shutdown::new();
        let t = s.token();
        assert!(!s.is_triggered());
        s.trigger();
        assert!(s.is_triggered());
        assert!(t.is_cancelled());
    }

    #[tokio::test]
    async fn wait_returns_when_triggered() {
        let s = Shutdown::new();
        let s2 = s.clone();
        let waiter = tokio::spawn(async move { s2.wait_for_signal().await });
        tokio::task::yield_now().await;
        s.trigger();
        waiter.await.unwrap().unwrap();
    }
}
