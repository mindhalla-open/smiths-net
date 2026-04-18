//! Two-leg UDP packet bridge.
//!
//! Each leg owns a UDP socket and knows the peer's remote address.
//! The bridge spawns two tasks — one per direction — that `recv_from`
//! on one leg's socket and `send_to` the other peer via the other
//! leg's socket. Bytes are forwarded opaquely; the caller decides
//! whether they are RTP, RTCP, or something else.
//!
//! Graceful shutdown is driven by a single `CancellationToken`.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// One leg of a bridged call.
#[derive(Debug)]
pub struct Leg {
    /// Engine-owned socket for this leg.
    pub socket: Arc<UdpSocket>,
    /// Where the engine should deliver packets for this leg's peer.
    /// Learned from SDP `c=` / `m=` on offer/answer.
    pub peer: SocketAddr,
}

/// Live bridge running two forwarder tasks.
pub struct Bridge {
    cancel: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
}

impl Bridge {
    /// Start forwarding A ↔ B. Both legs' sockets must already be bound.
    #[must_use]
    pub fn spawn(a: &Leg, b: &Leg) -> Self {
        let cancel = CancellationToken::new();
        let t_ab = spawn_forward(
            Arc::clone(&a.socket),
            Arc::clone(&b.socket),
            b.peer,
            cancel.clone(),
            "a->b",
        );
        let t_ba = spawn_forward(
            Arc::clone(&b.socket),
            Arc::clone(&a.socket),
            a.peer,
            cancel.clone(),
            "b->a",
        );
        Self {
            cancel,
            tasks: vec![t_ab, t_ba],
        }
    }

    /// Cancel forwarding and wait for both tasks to exit.
    pub async fn shutdown(self) {
        self.cancel.cancel();
        for t in self.tasks {
            let _ = t.await;
        }
    }
}

fn spawn_forward(
    recv: Arc<UdpSocket>,
    send: Arc<UdpSocket>,
    dest: SocketAddr,
    cancel: CancellationToken,
    dir: &'static str,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // RTP frames top out well under an MTU; 2 KB leaves headroom.
        let mut buf = vec![0u8; 2048];
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                res = recv.recv_from(&mut buf) => match res {
                    Ok((n, _src)) => {
                        if let Err(e) = send.send_to(&buf[..n], dest).await {
                            warn!(dir, ?e, "bridge send failed");
                        }
                    }
                    Err(e) => {
                        warn!(dir, ?e, "bridge recv failed; stopping direction");
                        break;
                    }
                },
            }
        }
        debug!(dir, "bridge forwarder stopped");
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    async fn bind_udp() -> (Arc<UdpSocket>, SocketAddr) {
        let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let a = s.local_addr().unwrap();
        (Arc::new(s), a)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bridge_forwards_both_ways() {
        // Engine's two legs (receives from UA-A / UA-B).
        let (sock_engine_a, addr_engine_a) = bind_udp().await;
        let (sock_engine_b, addr_engine_b) = bind_udp().await;

        // UA-A and UA-B.
        let (ua_a, ua_addr_a) = bind_udp().await;
        let (ua_b, ua_addr_b) = bind_udp().await;

        let bridge = Bridge::spawn(
            &Leg {
                socket: Arc::clone(&sock_engine_a),
                peer: ua_addr_a,
            },
            &Leg {
                socket: Arc::clone(&sock_engine_b),
                peer: ua_addr_b,
            },
        );

        // UA-A sends to engine's A-leg socket; engine forwards to UA-B.
        ua_a.send_to(b"hello-from-a", addr_engine_a).await.unwrap();
        let mut buf = [0u8; 256];
        let (n, from) = timeout(Duration::from_secs(1), ua_b.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf[..n], b"hello-from-a");
        assert_eq!(from, addr_engine_b);

        ua_b.send_to(b"hello-from-b", addr_engine_b).await.unwrap();
        let (n, from) = timeout(Duration::from_secs(1), ua_a.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf[..n], b"hello-from-b");
        assert_eq!(from, addr_engine_a);

        bridge.shutdown().await;
    }
}
