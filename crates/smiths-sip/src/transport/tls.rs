//! SIP over TLS (RFC 5630 / RFC 3261 §26.2).
//!
//! Structurally parallels [`super::tcp`] — same framing, same
//! per-peer writer mpsc, same accept loop, same per-connection tasks
//! from [`super::stream`] — with a `TlsAcceptor` wrapping the raw TCP
//! stream. Inbound only: SIP clients open the TLS connection and the
//! engine responds on it.
//!
//! Inbound connections are capped ([`TlsTransport::with_max_connections`],
//! default 1024, checked before the handshake) and closed after a
//! period of inactivity ([`TlsTransport::with_idle_timeout`], default
//! 5 minutes).

use std::fs::File;
use std::io::BufReader;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use dashmap::DashMap;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls_pemfile::{certs, private_key};
use tokio::io::split;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_rustls::{TlsAcceptor, server::TlsStream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, instrument, warn};

use super::stream::{ConnLimits, PeerMap, StreamContext, register_stream};
use super::{Datagram, Transport, TransportKind};

/// SIP TLS transport. Cheap to clone via `Arc`. Inbound-only — SIP
/// clients open the TLS connection and the engine responds on it.
#[derive(Clone)]
pub struct TlsTransport {
    listener: Arc<TcpListener>,
    acceptor: TlsAcceptor,
    peers: PeerMap,
    limits: ConnLimits,
}

impl TlsTransport {
    /// Load cert+key from disk, build a `rustls::ServerConfig`, and
    /// bind a TCP listener. `cert_path` is a PEM bundle (leaf + any
    /// chain); `key_path` is a PEM-encoded private key (PKCS#8 or
    /// RSA). `bind.port == 0` lets the OS assign one.
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
            limits: ConnLimits::default(),
        })
    }

    /// Cap on simultaneously open connections admitted by the accept
    /// loop; excess connections are closed before the TLS handshake.
    /// `0` removes the cap. Default 1024.
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

    /// Number of connections currently in the pool.
    #[must_use]
    pub fn connections(&self) -> usize {
        self.peers.len()
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
        let limits = self.limits;
        let ctx = StreamContext {
            label: "tls",
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
                        debug!("tls accept loop cancelled");
                        break;
                    }
                    res = listener.accept() => match res {
                        Ok((tcp, peer)) => {
                            if !limits.admits(peers.len()) {
                                warn!(
                                    %peer,
                                    cap = limits.max_connections,
                                    "tls connection cap reached; refusing"
                                );
                                drop(tcp);
                                continue;
                            }
                            // Defer TLS handshake off the accept loop.
                            let acc = acceptor.clone();
                            let ctx = ctx.clone();
                            tokio::spawn(async move {
                                match acc.accept(tcp).await {
                                    Ok(tls) => register_connection(&ctx, tls, peer),
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

    fn kind(&self) -> TransportKind {
        TransportKind::Tls
    }
}

/// Load cert bundle + private key and assemble a TLS 1.2/1.3 server
/// config. Client authentication is disabled — mTLS is not offered.
fn build_server_config(cert_path: &Path, key_path: &Path) -> std::io::Result<rustls::ServerConfig> {
    install_default_crypto_provider();
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

/// Pick rustls' `ring` backend explicitly.
///
/// `ServerConfig::builder` panics when it cannot infer a single
/// process-wide provider, which is exactly what happens once another
/// dependency in the binary also pulls in `aws-lc-rs` — the engine
/// links `reqwest`, so that is the normal case, not a corner one.
/// Installing here means enabling `sip.transports = ["tls"]` cannot
/// take the process down at startup.
fn install_default_crypto_provider() {
    use std::sync::Once;
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        // Errors only when something already installed a default,
        // which is just as good — the process has one either way.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
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

/// Split one accepted TLS stream and hand both halves to the shared
/// per-connection tasks.
fn register_connection(ctx: &StreamContext, stream: TlsStream<TcpStream>, peer: SocketAddr) {
    let (read_half, write_half) = split(stream);
    register_stream(ctx, peer, read_half, write_half);
    info!(%peer, "tls connection registered");
}

/// Collect both on-disk paths into one struct so CLI / config wiring
/// has somewhere coherent to carry them.
#[derive(Clone, Debug)]
pub struct TlsPaths {
    pub cert: PathBuf,
    pub key: PathBuf,
}
