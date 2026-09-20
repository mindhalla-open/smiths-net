//! Per-connection machinery shared by the stream transports (TCP and
//! TLS): the writer task that drains a per-peer mpsc onto the socket,
//! the reader task that frames RFC 3261 §7.5 messages off the socket,
//! and the connection limits both accept loops enforce.
//!
//! A connection is one writer task + one reader task tied together by
//! a child [`CancellationToken`]: whichever side finishes first
//! (peer closed, write error, idle timeout, transport shutdown)
//! cancels the other, and the writer removes the peer's pool entry on
//! its way out so `peers.len` is an exact live-connection count.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use dashmap::DashMap;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::Datagram;
use super::framing::{FrameOutcome, take_one_message};

/// Per-connection writer mpsc depth. Small on purpose — backpressure
/// propagates to the caller of `send` when a peer is slow to read.
pub(super) const WRITE_QUEUE_DEPTH: usize = 32;

/// Default cap on simultaneously open inbound connections.
pub(super) const DEFAULT_MAX_CONNECTIONS: usize = 1024;

/// Default idle timeout: a connection with no bytes in either
/// direction for this long is closed.
pub(super) const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_mins(5);

/// Live per-peer writer handles, shared by the accept loop, `send`,
/// and every connection's tasks.
pub(super) type PeerMap = Arc<DashMap<SocketAddr, mpsc::Sender<Bytes>>>;

/// Resource limits applied to inbound connections.
#[derive(Clone, Copy, Debug)]
pub(super) struct ConnLimits {
    /// Accept-loop cap: `0` = unlimited.
    pub max_connections: usize,
    /// `None` = never close on inactivity.
    pub idle_timeout: Option<Duration>,
}

impl Default for ConnLimits {
    fn default() -> Self {
        Self {
            max_connections: DEFAULT_MAX_CONNECTIONS,
            idle_timeout: Some(DEFAULT_IDLE_TIMEOUT),
        }
    }
}

impl ConnLimits {
    /// `true` when another inbound connection may be registered.
    pub(super) fn admits(&self, live: usize) -> bool {
        self.max_connections == 0 || live < self.max_connections
    }
}

/// Last-activity clock shared by a connection's reader and writer.
struct Activity {
    epoch: Instant,
    last_ms: AtomicU64,
}

impl Activity {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            epoch: Instant::now(),
            last_ms: AtomicU64::new(0),
        })
    }

    fn touch(&self) {
        let ms = u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_ms.store(ms, Ordering::Relaxed);
    }

    fn idle_for(&self) -> Duration {
        let now = u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX);
        Duration::from_millis(now.saturating_sub(self.last_ms.load(Ordering::Relaxed)))
    }
}

/// Everything a new connection shares with its transport: the peer
/// pool, the inbound message channel, the transport-wide cancel token
/// and the idle policy.
#[derive(Clone)]
pub(super) struct StreamContext {
    pub label: &'static str,
    pub peers: PeerMap,
    pub tx: mpsc::Sender<Datagram>,
    pub cancel: CancellationToken,
    pub idle_timeout: Option<Duration>,
}

/// Install writer + reader tasks for one stream and register its
/// writer mpsc in the peer pool. Returns the writer handle so a
/// caller that just opened an outbound connection can immediately
/// push bytes.
pub(super) fn register_stream<R, W>(
    ctx: &StreamContext,
    peer: SocketAddr,
    read_half: R,
    write_half: W,
) -> mpsc::Sender<Bytes>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (write_tx, write_rx) = mpsc::channel::<Bytes>(WRITE_QUEUE_DEPTH);
    // A previous connection from the same peer (reconnect after a
    // half-close) is superseded: its writer sees the channel close
    // when the old sender drops out of the map.
    ctx.peers.insert(peer, write_tx.clone());
    let conn_cancel = ctx.cancel.child_token();
    let activity = Activity::new();
    spawn_writer(
        ctx.label,
        peer,
        write_half,
        write_rx,
        PoolSlot {
            peers: Arc::clone(&ctx.peers),
            self_tx: write_tx.clone(),
        },
        conn_cancel.clone(),
        Arc::clone(&activity),
    );
    spawn_framed_reader(
        ctx.label,
        peer,
        read_half,
        ctx.tx.clone(),
        conn_cancel,
        activity,
        ctx.idle_timeout,
    );
    write_tx
}

/// This connection's claim on the peer pool: the map, plus the
/// sender that identifies the entry as belonging to this connection
/// rather than to a later reconnect.
struct PoolSlot {
    peers: PeerMap,
    self_tx: mpsc::Sender<Bytes>,
}

fn spawn_writer<W>(
    label: &'static str,
    peer: SocketAddr,
    mut write_half: W,
    mut rx: mpsc::Receiver<Bytes>,
    slot: PoolSlot,
    cancel: CancellationToken,
    activity: Arc<Activity>,
) where
    W: AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                msg = rx.recv() => match msg {
                    Some(bytes) => {
                        if let Err(e) = write_half.write_all(&bytes).await {
                            warn!(%peer, ?e, "{label} write error");
                            break;
                        }
                        activity.touch();
                    }
                    None => break,
                }
            }
        }
        let _ = write_half.shutdown().await;
        // Only drop the pool entry if it is still ours — a reconnect
        // from the same peer may already have replaced it.
        // Drop this peer's pool entry, but only while it still points
        // at *this* connection: if the peer reconnected, the map
        // already holds the newer connection's sender and removing it
        // would strand a live socket.
        let PoolSlot { peers, self_tx } = slot;
        peers.remove_if(&peer, |_, sender| sender.same_channel(&self_tx));
        drop(self_tx);
        rx.close();
        cancel.cancel();
    });
}

fn spawn_framed_reader<R>(
    label: &'static str,
    peer: SocketAddr,
    mut read_half: R,
    tx: mpsc::Sender<Datagram>,
    cancel: CancellationToken,
    activity: Arc<Activity>,
    idle_timeout: Option<Duration>,
) where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buf = BytesMut::with_capacity(8192);
        loop {
            // Idle budget left, counting activity in either direction.
            let remaining = match idle_timeout {
                Some(idle) => {
                    let remaining = idle.saturating_sub(activity.idle_for());
                    if remaining.is_zero() {
                        debug!(%peer, ?idle, "{label} connection idle; closing");
                        break;
                    }
                    remaining
                }
                None => Duration::MAX,
            };
            tokio::select! {
                           biased;
                           () = cancel.cancelled() => break,
                           res = tokio::time::timeout(remaining, read_half.read_buf(&mut buf)) => match res {
            // Timer elapsed: loop back to re-check the shared
            // clock — the writer may have refreshed it.
                               Err(_elapsed) => {}
                               Ok(Ok(0)) => {
                                   debug!(%peer, "{label} peer closed");
                                   break;
                               }
                               Ok(Ok(_)) => {
                                   activity.touch();
                                   loop {
                                       match take_one_message(&mut buf) {
                                           FrameOutcome::Complete(bytes) => {
                                               if tx.send(Datagram { bytes, peer }).await.is_err() {
                                                   debug!("{label} reader: receiver dropped");
                                                   cancel.cancel();
                                                   return;
                                               }
                                           }
                                           FrameOutcome::Partial => break,
                                           FrameOutcome::Overflow => {
                                               warn!(%peer, "{label} message exceeded cap; closing");
                                               cancel.cancel();
                                               return;
                                           }
                                           FrameOutcome::BadLength => {
                                               warn!(%peer, "{label} malformed Content-Length; closing");
                                               cancel.cancel();
                                               return;
                                           }
                                       }
                                   }
                               }
                               Ok(Err(e)) => {
                                   warn!(%peer, ?e, "{label} read error");
                                   break;
                               }
                           }
                       }
        }
        cancel.cancel();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_admit_below_cap_and_unlimited_at_zero() {
        let l = ConnLimits {
            max_connections: 2,
            idle_timeout: None,
        };
        assert!(l.admits(0));
        assert!(l.admits(1));
        assert!(!l.admits(2));
        let unlimited = ConnLimits {
            max_connections: 0,
            idle_timeout: None,
        };
        assert!(unlimited.admits(usize::MAX));
    }

    #[test]
    fn defaults_are_sane() {
        let d = ConnLimits::default();
        assert_eq!(d.max_connections, 1024);
        assert_eq!(d.idle_timeout, Some(Duration::from_mins(5)));
    }

    #[test]
    fn activity_clock_resets_on_touch() {
        let a = Activity::new();
        std::thread::sleep(Duration::from_millis(5));
        assert!(a.idle_for() >= Duration::from_millis(5));
        a.touch();
        assert!(a.idle_for() < Duration::from_millis(5));
    }
}
