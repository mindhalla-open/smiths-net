//! SIP over TCP (RFC 3261 §18.2).
//!
//! One persistent connection per peer, in either direction. Inbound
//! connections are accepted by the `spawn_reader` accept loop;
//! outbound connections are opened lazily on `send` when no pool
//! entry exists for the peer. Per-connection writer and reader tasks
//! (shared with TLS, see [`super::stream`]) talk to the pool through
//! mpsc channels so the pool holds no locks across awaits.
//!
//! Message framing follows §7.5: headers terminated by a double CRLF,
//! body length dictated by `Content-Length`. Messages without a
//! `Content-Length` header are treated as empty-bodied, matching the
//! §20.14 requirement that stream transports always carry one.
//!
//! Inbound connections are capped ([`TcpTransport::with_max_connections`],
//! default 1024) and closed after a period of inactivity
//! ([`TcpTransport::with_idle_timeout`], default 5 minutes).

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use dashmap::DashMap;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, instrument, warn};

use super::proxy::{DirectConnector, ProxyConnector};
use super::stream::{ConnLimits, PeerMap, StreamContext, register_stream};
use super::{Datagram, Transport, TransportKind};

/// SIP TCP transport. Cheap to clone via `Arc`.
#[derive(Clone)]
pub struct TcpTransport {
    listener: Arc<TcpListener>,
    peers: PeerMap,
    /// Inbound routing state populated by `spawn_reader`. `send` uses
    /// this to open outbound connections when no pool entry exists.
    /// `OnceLock` so the trait-object-safe `send` stays immutable.
    inbound: Arc<OnceLock<InboundState>>,
    /// Outbound-connect shim. `DirectConnector` by default —
    /// identical to calling `TcpStream::connect` — so the proxy path
    /// is a zero-cost option when `[sip.proxy] mode = "none"`.
    connector: Arc<dyn ProxyConnector>,
    limits: ConnLimits,
}

#[derive(Clone)]
struct InboundState {
    tx: mpsc::Sender<Datagram>,
    cancel: CancellationToken,
}

impl TcpTransport {
    /// Bind a TCP listener. `bind.port == 0` lets the OS assign one.
    pub async fn bind(bind: SocketAddr) -> std::io::Result<Self> {
        let listener = TcpListener::bind(bind).await?;
        Ok(Self {
            listener: Arc::new(listener),
            peers: Arc::new(DashMap::new()),
            inbound: Arc::new(OnceLock::new()),
            connector: Arc::new(DirectConnector),
            limits: ConnLimits::default(),
        })
    }

    /// Install a [`ProxyConnector`] for outbound connects. The
    /// default is [`DirectConnector`]; swap in
    /// [`super::proxy::Socks5Connector`] or
    /// [`super::proxy::HttpConnectConnector`] to tunnel SIP-over-TCP
    /// through an outbound proxy without touching the listener path.
    #[must_use]
    pub fn with_proxy(mut self, connector: Arc<dyn ProxyConnector>) -> Self {
        self.connector = connector;
        self
    }

    /// Cap on simultaneously open connections admitted by the accept
    /// loop; excess connections are closed immediately. `0` removes
    /// the cap. Outbound connections opened by `send` are not
    /// counted against it. Default 1024.
    #[must_use]
    pub const fn with_max_connections(mut self, max_connections: usize) -> Self {
        self.limits.max_connections = max_connections;
        self
    }

    /// Close a connection once no bytes have moved in either
    /// direction for `idle`. `None` disables the timeout. Default
    /// 5 minutes.
    #[must_use]
    pub const fn with_idle_timeout(mut self, idle: Option<Duration>) -> Self {
        self.limits.idle_timeout = idle;
        self
    }

    /// Number of connections currently in the pool (both directions).
    #[must_use]
    pub fn connections(&self) -> usize {
        self.peers.len()
    }

    /// Spawn the accept loop. Per-peer reader tasks forward framed
    /// messages to `tx`; the loop exits when `cancel` fires.
    ///
    /// Must be called once before `send` can open outbound connections.
    #[instrument(skip_all, fields(local = ?self.local_addr().ok()))]
    pub fn spawn_reader(
        &self,
        tx: mpsc::Sender<Datagram>,
        cancel: CancellationToken,
    ) -> JoinHandle<()> {
        // Stash the inbound channel for later outbound connects.
        let _ = self.inbound.set(InboundState {
            tx: tx.clone(),
            cancel: cancel.clone(),
        });

        let listener = Arc::clone(&self.listener);
        let peers = Arc::clone(&self.peers);
        let limits = self.limits;
        let ctx = StreamContext {
            label: "tcp",
            peers: Arc::clone(&peers),
            tx,
            cancel: cancel.clone(),
            idle_timeout: limits.idle_timeout,
        };
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        debug!("tcp accept loop cancelled");
                        break;
                    }
                    res = listener.accept() => match res {
                        Ok((stream, peer)) => {
                            if !limits.admits(peers.len()) {
                                warn!(
                                    %peer,
                                    cap = limits.max_connections,
                                    "tcp connection cap reached; refusing"
                                );
                                drop(stream);
                                continue;
                            }
                            register_connection(&ctx, stream, peer);
                        }
                        Err(e) => {
                            error!(?e, "tcp accept error");
                        }
                    }
                }
            }
        })
    }
}

impl Transport for TcpTransport {
    async fn send(&self, bytes: Bytes, peer: SocketAddr) -> std::io::Result<()> {
        // Reuse an existing connection to this peer if we have one.
        if let Some(sender) = self.peers.get(&peer).map(|e| e.value().clone())
            && sender.send(bytes.clone()).await.is_ok()
        {
            return Ok(());
        }

        // No pool entry (or the previous writer is gone) — open
        // outbound. Requires `spawn_reader` to have run first so we
        // have an inbound channel to thread new connections into.
        let Some(state) = self.inbound.get() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "tcp transport: spawn_reader not called; outbound disabled",
            ));
        };
        // Evict the stale entry, if any, before reconnecting.
        self.peers.remove(&peer);
        let stream = self.connector.connect(peer).await.map_err(|e| {
            std::io::Error::new(
                e.kind(),
                format!("tcp outbound via {}: {e}", self.connector.label()),
            )
        })?;
        let ctx = StreamContext {
            label: "tcp",
            peers: Arc::clone(&self.peers),
            tx: state.tx.clone(),
            cancel: state.cancel.clone(),
            idle_timeout: self.limits.idle_timeout,
        };
        let sender = register_connection(&ctx, stream, peer);
        sender.send(bytes).await.map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "tcp writer channel closed")
        })
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    fn kind(&self) -> TransportKind {
        TransportKind::Tcp
    }
}

/// Split one TCP stream and hand both halves to the shared
/// per-connection tasks. Returns the writer handle so a caller that
/// just opened an outbound connection can immediately push bytes.
fn register_connection(
    ctx: &StreamContext,
    stream: TcpStream,
    peer: SocketAddr,
) -> mpsc::Sender<Bytes> {
    let (read_half, write_half) = stream.into_split();
    register_stream(ctx, peer, read_half, write_half)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test(flavor = "multi_thread")]
    async fn bind_and_roundtrip_one_message() {
        let cancel = CancellationToken::new();
        let server = TcpTransport::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let server_addr = server.local_addr().unwrap();
        let (tx, mut rx) = mpsc::channel::<Datagram>(16);
        let _h = server.spawn_reader(tx, cancel.clone());

        // Open a client connection and write a framed OPTIONS.
        let mut client = TcpStream::connect(server_addr).await.unwrap();
        let frame = b"OPTIONS sip:engine@localhost SIP/2.0\r\nContent-Length: 0\r\n\r\n";
        client.write_all(frame).await.unwrap();

        let dg = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&dg.bytes[..], frame);

        // Reply on the same connection through the transport.
        let reply = b"SIP/2.0 200 OK\r\nContent-Length: 0\r\n\r\n";
        server
            .send(Bytes::from_static(reply), dg.peer)
            .await
            .unwrap();
        let mut buf = vec![0u8; 256];
        let n = tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf[..n], reply);
        assert_eq!(server.connections(), 1);

        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn peer_close_releases_pool_entry() {
        let cancel = CancellationToken::new();
        let server = TcpTransport::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let server_addr = server.local_addr().unwrap();
        let (tx, mut rx) = mpsc::channel::<Datagram>(16);
        let _h = server.spawn_reader(tx, cancel.clone());

        let mut client = TcpStream::connect(server_addr).await.unwrap();
        client
            .write_all(b"OPTIONS sip:x SIP/2.0\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();
        let _ = rx.recv().await.unwrap();
        assert_eq!(server.connections(), 1);
        drop(client);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while server.connections() != 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(server.connections(), 0, "closed peer must leave the pool");
        cancel.cancel();
    }
}
