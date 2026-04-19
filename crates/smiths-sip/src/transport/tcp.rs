//! SIP over TCP (RFC 3261 §18.2).
//!
//! One persistent connection per peer, in either direction. Inbound
//! connections are accepted by the `spawn_reader` accept loop;
//! outbound connections are opened lazily on `send` when no pool
//! entry exists for the peer. Per-connection writer and reader tasks
//! share the connection through mpsc channels so the pool holds no
//! locks across awaits.
//!
//! Message framing follows §7.5: headers terminated by a double CRLF,
//! body length dictated by `Content-Length`. Messages without a
//! `Content-Length` header are treated as empty-bodied, matching the
//! §20.14 requirement that stream transports always carry one.

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};

use bytes::{Bytes, BytesMut};
use dashmap::DashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, instrument, warn};

use super::framing::{FrameOutcome, take_one_message};
use super::{Datagram, Transport};

/// Per-connection writer mpsc depth. Small on purpose — backpressure
/// propagates to the caller of `send` when a peer is slow to read.
const WRITE_QUEUE_DEPTH: usize = 32;

/// SIP TCP transport. Cheap to clone via `Arc`.
#[derive(Clone)]
pub struct TcpTransport {
    listener: Arc<TcpListener>,
    peers: Arc<DashMap<SocketAddr, mpsc::Sender<Bytes>>>,
    /// Inbound routing state populated by `spawn_reader`. `send` uses
    /// this to open outbound connections when no pool entry exists.
    /// `OnceLock` so the trait-object-safe `send` stays immutable.
    inbound: Arc<OnceLock<InboundState>>,
}

#[derive(Clone)]
struct InboundState {
    tx: mpsc::Sender<Datagram>,
    cancel: CancellationToken,
}

impl TcpTransport {
    /// Bind a TCP listener. `bind.port() == 0` lets the OS assign one.
    pub async fn bind(bind: SocketAddr) -> std::io::Result<Self> {
        let listener = TcpListener::bind(bind).await?;
        Ok(Self {
            listener: Arc::new(listener),
            peers: Arc::new(DashMap::new()),
            inbound: Arc::new(OnceLock::new()),
        })
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
                            register_connection(stream, peer, &peers, tx.clone(), cancel.clone());
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
        let stream = TcpStream::connect(peer).await?;
        let sender = register_connection(
            stream,
            peer,
            &self.peers,
            state.tx.clone(),
            state.cancel.clone(),
        );
        sender.send(bytes).await.map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "tcp writer channel closed")
        })
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }
}

/// Install writer + reader tasks for one TCP stream and register its
/// writer mpsc in the peer pool. Returns the writer handle so a caller
/// that just opened an outbound connection can immediately push bytes.
fn register_connection(
    stream: TcpStream,
    peer: SocketAddr,
    peers: &Arc<DashMap<SocketAddr, mpsc::Sender<Bytes>>>,
    tx: mpsc::Sender<Datagram>,
    cancel: CancellationToken,
) -> mpsc::Sender<Bytes> {
    let (write_tx, write_rx) = mpsc::channel::<Bytes>(WRITE_QUEUE_DEPTH);
    peers.insert(peer, write_tx.clone());

    let (read_half, write_half) = stream.into_split();
    spawn_writer(
        peer,
        write_half,
        write_rx,
        cancel.clone(),
        Arc::clone(peers),
    );
    spawn_framed_reader(peer, read_half, tx, cancel);

    write_tx
}

fn spawn_writer(
    peer: SocketAddr,
    mut write_half: tokio::net::tcp::OwnedWriteHalf,
    mut rx: mpsc::Receiver<Bytes>,
    cancel: CancellationToken,
    peers: Arc<DashMap<SocketAddr, mpsc::Sender<Bytes>>>,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                msg = rx.recv() => match msg {
                    Some(bytes) => {
                        if let Err(e) = write_half.write_all(&bytes).await {
                            warn!(%peer, ?e, "tcp write error");
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
        let _ = write_half.shutdown().await;
        peers.remove(&peer);
    });
}

fn spawn_framed_reader(
    peer: SocketAddr,
    mut read_half: tokio::net::tcp::OwnedReadHalf,
    tx: mpsc::Sender<Datagram>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        let mut buf = BytesMut::with_capacity(8192);
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                res = read_half.read_buf(&mut buf) => match res {
                    Ok(0) => {
                        debug!(%peer, "tcp peer closed");
                        break;
                    }
                    Ok(_) => {
                        loop {
                            match take_one_message(&mut buf) {
                                FrameOutcome::Complete(bytes) => {
                                    if tx.send(Datagram { bytes, peer }).await.is_err() {
                                        debug!("tcp reader: receiver dropped");
                                        return;
                                    }
                                }
                                FrameOutcome::Partial => break,
                                FrameOutcome::Overflow => {
                                    warn!(%peer, "tcp message exceeded cap; closing");
                                    return;
                                }
                                FrameOutcome::BadLength => {
                                    warn!(%peer, "tcp malformed Content-Length; closing");
                                    return;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!(%peer, ?e, "tcp read error");
                        break;
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

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

        cancel.cancel();
    }
}
