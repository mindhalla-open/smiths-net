//! UDPTL relay session — the T.38 analogue of `smiths-media::Bridge`.
//!
//! `UdptlSession` holds one UDP socket per leg and spawns two
//! forwarder tasks (A → B and B → A). Each task calls `recv_from`,
//! logs the UDPTL sequence number for tracing, and `send_to`s the
//! received datagram to the peer leg's remote address. No rewrite,
//! no re-framing — UDPTL datagrams travel byte-identity. That's
//! intentional: T.38 terminals care about the sequence and the
//! redundancy ordering; a relay that rewrites either corrupts the
//! loss-recovery math.
//!
//! The session implements [`MediaSession`], so the engine's future
//! bridge integration (see the crate-level doc) can swap it into
//! place for an existing RTP bridge when a re-INVITE upgrades a
//! call to T.38.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use smiths_core::media::{BridgeId, MediaSession};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::metrics::FaxMetrics;
use crate::udptl::UdptlPacket;

/// Runtime knobs for [`UdptlSession`].
#[derive(Clone, Debug)]
pub struct UdptlSessionConfig {
    /// Opaque session id the engine files the session under.
    pub id: BridgeId,
    /// When `true`, the forwarder parses the UDPTL header on every
    /// received datagram and logs sequence jumps at `debug`. Adds a
    /// ~200 ns parse cost per datagram; off by default.
    pub trace_sequence: bool,
    /// Prometheus metrics handle — when `Some`, the forwarders
    /// record datagram counts + parse errors through it. `None` on
    /// tests that don't need Prometheus output. Slice 5.12.
    pub metrics: Option<Arc<FaxMetrics>>,
}

impl Default for UdptlSessionConfig {
    fn default() -> Self {
        Self {
            id: BridgeId(0),
            trace_sequence: false,
            metrics: None,
        }
    }
}

/// Two-leg UDPTL relay.
///
/// Use [`Self::spawn`] to construct and start forwarders; the
/// returned handle exposes [`stop`](Self::stop) + the `MediaSession`
/// trait surface.
pub struct UdptlSession {
    id: BridgeId,
    cancel: CancellationToken,
    tasks: tokio::sync::Mutex<Vec<JoinHandle<()>>>,
    metrics: Option<Arc<FaxMetrics>>,
}

impl UdptlSession {
    /// Spawn the relay. `leg_a` and `leg_b` each own the local UDP
    /// socket and the peer's remote address.
    #[must_use]
    // By-value `cfg` + leg tuples read naturally at construction
    // time; refs would clutter every call site with `&`.
    #[allow(clippy::needless_pass_by_value)]
    pub fn spawn(
        cfg: UdptlSessionConfig,
        leg_a: (Arc<UdpSocket>, SocketAddr),
        leg_b: (Arc<UdpSocket>, SocketAddr),
    ) -> Arc<Self> {
        let cancel = CancellationToken::new();
        if let Some(m) = &cfg.metrics {
            m.sessions_active.inc();
        }
        let a_to_b = spawn_forwarder(
            "a_to_b",
            Arc::clone(&leg_a.0),
            Arc::clone(&leg_b.0),
            leg_b.1,
            cfg.trace_sequence,
            cfg.metrics.clone(),
            cancel.clone(),
        );
        let b_to_a = spawn_forwarder(
            "b_to_a",
            Arc::clone(&leg_b.0),
            Arc::clone(&leg_a.0),
            leg_a.1,
            cfg.trace_sequence,
            cfg.metrics.clone(),
            cancel.clone(),
        );
        Arc::new(Self {
            id: cfg.id,
            cancel,
            tasks: tokio::sync::Mutex::new(vec![a_to_b, b_to_a]),
            metrics: cfg.metrics,
        })
    }
}

#[async_trait]
impl MediaSession for UdptlSession {
    fn id(&self) -> BridgeId {
        self.id
    }

    async fn stop(&self) {
        self.cancel.cancel();
        let mut handles = self.tasks.lock().await;
        let was_live = !handles.is_empty();
        for h in handles.drain(..) {
            // Abort on join errors; we've already signaled cancel, so
            // a panicking forwarder is already "stopped" for our
            // purposes.
            let _ = h.await;
        }
        if was_live && let Some(m) = &self.metrics {
            m.sessions_active.dec();
        }
    }
}

fn spawn_forwarder(
    direction: &'static str,
    recv_sock: Arc<UdpSocket>,
    send_sock: Arc<UdpSocket>,
    peer: SocketAddr,
    trace_sequence: bool,
    metrics: Option<Arc<FaxMetrics>>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf = vec![0_u8; 1500];
        let mut last_seq: Option<u16> = None;
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    debug!(direction, "udptl forwarder cancelled");
                    return;
                }
                recv = recv_sock.recv_from(&mut buf) => {
                    match recv {
                        Ok((n, _from)) => {
                            if trace_sequence {
                                trace_seq(direction, &buf[..n], &mut last_seq, metrics.as_deref());
                            }
                            if let Err(e) = send_sock.send_to(&buf[..n], peer).await {
                                warn!(direction, error = %e, "udptl send_to failed");
                            } else if let Some(m) = &metrics {
                                m.record_forward(direction);
                            }
                        }
                        Err(e) => {
                            warn!(direction, error = %e, "udptl recv_from failed");
                        }
                    }
                }
            }
        }
    })
}

fn trace_seq(
    direction: &'static str,
    bytes: &[u8],
    last_seq: &mut Option<u16>,
    metrics: Option<&FaxMetrics>,
) {
    match UdptlPacket::parse(bytes) {
        Ok(pkt) => {
            if let Some(prev) = *last_seq {
                let expected = prev.wrapping_add(1);
                if pkt.sequence != expected {
                    debug!(
                        direction,
                        got = pkt.sequence,
                        expected,
                        "udptl sequence gap",
                    );
                }
            }
            *last_seq = Some(pkt.sequence);
        }
        Err(e) => {
            debug!(direction, error = %e, "udptl parse failed (not fatal, forwarding raw)");
            if let Some(m) = metrics {
                m.record_parse_error(&e);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::similar_names)] // leg_a / leg_b / peer_a / peer_b are load-bearing
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    async fn bind() -> Arc<UdpSocket> {
        Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relay_forwards_datagram_byte_identity() {
        // Topology:
        //
        //   peer_a  <----> leg_a <===[session]===> leg_b <----> peer_b
        //
        // We send a canned UDPTL datagram from peer_a to leg_a, and
        // assert it arrives at peer_b unchanged.
        let leg_a = bind().await;
        let leg_b = bind().await;
        let peer_a = bind().await;
        let peer_b = bind().await;

        let leg_a_addr = leg_a.local_addr().unwrap();
        let leg_b_addr = leg_b.local_addr().unwrap();
        let peer_a_addr = peer_a.local_addr().unwrap();
        let peer_b_addr = peer_b.local_addr().unwrap();

        let session = UdptlSession::spawn(
            UdptlSessionConfig::default(),
            (Arc::clone(&leg_a), peer_a_addr),
            (Arc::clone(&leg_b), peer_b_addr),
        );

        let pkt = UdptlPacket {
            sequence: 0xBEEF,
            primary: b"IFP primary".to_vec(),
            secondary: vec![b"IFP secondary 1".to_vec()],
        };
        let wire = pkt.encode().unwrap();
        peer_a.send_to(&wire, leg_a_addr).await.unwrap();

        let mut buf = [0_u8; 1500];
        let (n, from) = timeout(Duration::from_secs(1), peer_b.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(from, leg_b_addr);
        assert_eq!(&buf[..n], &wire[..], "udptl relay mangled bytes");

        session.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relay_forwards_both_directions() {
        let leg_a = bind().await;
        let leg_b = bind().await;
        let peer_a = bind().await;
        let peer_b = bind().await;

        let leg_a_addr = leg_a.local_addr().unwrap();
        let leg_b_addr = leg_b.local_addr().unwrap();
        let peer_a_addr = peer_a.local_addr().unwrap();
        let peer_b_addr = peer_b.local_addr().unwrap();

        let session = UdptlSession::spawn(
            UdptlSessionConfig {
                trace_sequence: true,
                ..UdptlSessionConfig::default()
            },
            (Arc::clone(&leg_a), peer_a_addr),
            (Arc::clone(&leg_b), peer_b_addr),
        );

        // A → B
        let a_pkt = UdptlPacket {
            sequence: 1,
            primary: b"from A".to_vec(),
            secondary: vec![],
        };
        peer_a
            .send_to(&a_pkt.encode().unwrap(), leg_a_addr)
            .await
            .unwrap();
        let mut buf = [0_u8; 1500];
        let (n, _) = timeout(Duration::from_secs(1), peer_b.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(UdptlPacket::parse(&buf[..n]).unwrap(), a_pkt);

        // B → A
        let b_pkt = UdptlPacket {
            sequence: 99,
            primary: b"from B".to_vec(),
            secondary: vec![],
        };
        peer_b
            .send_to(&b_pkt.encode().unwrap(), leg_b_addr)
            .await
            .unwrap();
        let (n, _) = timeout(Duration::from_secs(1), peer_a.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(UdptlPacket::parse(&buf[..n]).unwrap(), b_pkt);

        session.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_cancels_forwarders() {
        let leg_a = bind().await;
        let leg_b = bind().await;
        let session = UdptlSession::spawn(
            UdptlSessionConfig::default(),
            (Arc::clone(&leg_a), "127.0.0.1:1".parse().unwrap()),
            (Arc::clone(&leg_b), "127.0.0.1:2".parse().unwrap()),
        );
        // Immediately stop — should not hang.
        timeout(Duration::from_secs(2), session.stop())
            .await
            .expect("stop hung");
    }
}
