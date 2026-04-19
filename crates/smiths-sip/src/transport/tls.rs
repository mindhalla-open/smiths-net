//! SIP over TLS (RFC 5630 / RFC 3261 §26.2).
//!
//! Structurally parallels [`super::tcp`] — same framing, same
//! per-peer writer mpsc, same accept loop — with a `TlsAcceptor`
//! wrapping the raw TCP stream. MVP scope: inbound only (the engine
//! is typically the receiver). Outbound TLS gets its own session
//! when a real UAC lands.

use std::fs::File;
use std::io::BufReader;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use dashmap::DashMap;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls_pemfile::{certs, private_key};
use tokio::io::{AsyncReadExt, AsyncWriteExt, split};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_rustls::{TlsAcceptor, server::TlsStream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, instrument, warn};

use super::framing::{FrameOutcome, take_one_message};
use super::{Datagram, Transport};

/// Per-connection writer mpsc depth.
const WRITE_QUEUE_DEPTH: usize = 32;

/// SIP TLS transport. Cheap to clone via `Arc`. Inbound-only — SIP
/// clients open the TLS connection and the engine responds on it.
#[derive(Clone)]
pub struct TlsTransport {
    listener: Arc<TcpListener>,
    acceptor: TlsAcceptor,
    peers: Arc<DashMap<SocketAddr, mpsc::Sender<Bytes>>>,
}

impl TlsTransport {
    /// Load cert+key from disk, build a `rustls::ServerConfig`, and
    /// bind a TCP listener. `cert_path` is a PEM bundle (leaf + any
    /// chain); `key_path` is a PEM-encoded private key (PKCS#8 or
    /// RSA). `bind.port() == 0` lets the OS assign one.
    pub async fn bind(
        bind: SocketAddr,
        cert_path: &Path,
        key_path: &Path,
    ) -> std::io::Result<Self> {
        let config = build_server_config(cert_path, key_path)?;
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind(bind).await?;
        Ok(Self {
            listener: Arc::new(listener),
            acceptor,
            peers: Arc::new(DashMap::new()),
        })
    }

    /// Spawn the accept loop. Per-peer reader tasks forward framed
    /// messages to `tx`; the loop exits when `cancel` fires.
    #[instrument(skip_all, fields(local = ?self.local_addr().ok()))]
    pub fn spawn_reader(
        &self,
        tx: mpsc::Sender<Datagram>,
        cancel: CancellationToken,
    ) -> JoinHandle<()> {
        let listener = Arc::clone(&self.listener);
        let peers = Arc::clone(&self.peers);
        let acceptor = self.acceptor.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        debug!("tls accept loop cancelled");
                        break;
                    }
                    res = listener.accept() => match res {
                        Ok((tcp, peer)) => {
                            // Defer TLS handshake off the accept loop.
                            let acc = acceptor.clone();
                            let tx = tx.clone();
                            let peers = Arc::clone(&peers);
                            let cancel = cancel.clone();
                            tokio::spawn(async move {
                                match acc.accept(tcp).await {
                                    Ok(tls) => {
                                        register_connection(tls, peer, &peers, tx, cancel);
                                    }
                                    Err(e) => {
                                        warn!(%peer, ?e, "tls handshake failed");
                                    }
                                }
                            });
                        }
                        Err(e) => {
                            error!(?e, "tls accept error");
                        }
                    }
                }
            }
        })
    }
}

impl Transport for TlsTransport {
    async fn send(&self, bytes: Bytes, peer: SocketAddr) -> std::io::Result<()> {
        let Some(sender) = self.peers.get(&peer).map(|e| e.value().clone()) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                format!("tls transport: no live connection to {peer}"),
            ));
        };
        sender.send(bytes).await.map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "tls writer channel closed")
        })
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }
}

/// Load cert bundle + private key and assemble a TLS 1.2/1.3 server
/// config. Client authentication is disabled by default — mTLS lands
/// alongside Phase 6 SRTP.
fn build_server_config(cert_path: &Path, key_path: &Path) -> std::io::Result<rustls::ServerConfig> {
    let cert_chain = load_certs(cert_path)?;
    let key = load_private_key(key_path)?;
    rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)
        .map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("tls server config: {e}"),
            )
        })
}

fn load_certs(path: &Path) -> std::io::Result<Vec<CertificateDer<'static>>> {
    let mut reader = BufReader::new(File::open(path).map_err(|e| {
        std::io::Error::new(e.kind(), format!("tls cert `{}`: {e}", path.display()))
    })?);
    let out: Result<Vec<_>, _> = certs(&mut reader).collect();
    out
}

fn load_private_key(path: &Path) -> std::io::Result<PrivateKeyDer<'static>> {
    let mut reader = BufReader::new(File::open(path).map_err(|e| {
        std::io::Error::new(e.kind(), format!("tls key `{}`: {e}", path.display()))
    })?);
    private_key(&mut reader)?.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("no private key found in `{}`", path.display()),
        )
    })
}

/// Install writer + framed-reader tasks on one accepted TLS stream.
fn register_connection(
    stream: TlsStream<TcpStream>,
    peer: SocketAddr,
    peers: &Arc<DashMap<SocketAddr, mpsc::Sender<Bytes>>>,
    tx: mpsc::Sender<Datagram>,
    cancel: CancellationToken,
) {
    let (write_tx, write_rx) = mpsc::channel::<Bytes>(WRITE_QUEUE_DEPTH);
    peers.insert(peer, write_tx);

    let (read_half, write_half) = split(stream);
    spawn_writer(
        peer,
        write_half,
        write_rx,
        cancel.clone(),
        Arc::clone(peers),
    );
    spawn_framed_reader(peer, read_half, tx, cancel);
    info!(%peer, "tls connection registered");
}

fn spawn_writer(
    peer: SocketAddr,
    mut write_half: tokio::io::WriteHalf<TlsStream<TcpStream>>,
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
                            warn!(%peer, ?e, "tls write error");
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
    mut read_half: tokio::io::ReadHalf<TlsStream<TcpStream>>,
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
                        debug!(%peer, "tls peer closed");
                        break;
                    }
                    Ok(_) => {
                        loop {
                            match take_one_message(&mut buf) {
                                FrameOutcome::Complete(bytes) => {
                                    if tx.send(Datagram { bytes, peer }).await.is_err() {
                                        debug!("tls reader: receiver dropped");
                                        return;
                                    }
                                }
                                FrameOutcome::Partial => break,
                                FrameOutcome::Overflow => {
                                    warn!(%peer, "tls message exceeded cap; closing");
                                    return;
                                }
                                FrameOutcome::BadLength => {
                                    warn!(%peer, "tls malformed Content-Length; closing");
                                    return;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!(%peer, ?e, "tls read error");
                        break;
                    }
                }
            }
        }
    });
}

/// Collect both on-disk paths into one struct so CLI / config wiring
/// has somewhere coherent to carry them.
#[derive(Clone, Debug)]
pub struct TlsPaths {
    pub cert: PathBuf,
    pub key: PathBuf,
}
