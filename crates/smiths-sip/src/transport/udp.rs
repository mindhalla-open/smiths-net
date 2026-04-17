//! SIP over UDP.

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, instrument, warn};

use super::{Datagram, Transport};

/// SIP UDP transport.
///
/// Cheap to clone via `Arc`. UDP framing maps one datagram to one SIP
/// message per RFC 3261 §18.
#[derive(Clone)]
pub struct UdpTransport {
    socket: Arc<UdpSocket>,
}

impl UdpTransport {
    /// Bind a UDP socket. `bind.port() == 0` lets the OS assign one.
    pub async fn bind(bind: SocketAddr) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(bind).await?;
        Ok(Self {
            socket: Arc::new(socket),
        })
    }

    /// Spawn a reader task that forwards incoming datagrams to `tx`.
    /// The task exits when `cancel` fires or `tx` is dropped.
    #[instrument(skip_all, fields(local = ?self.local_addr().ok()))]
    pub fn spawn_reader(
        &self,
        tx: mpsc::Sender<Datagram>,
        cancel: CancellationToken,
    ) -> JoinHandle<()> {
        let socket = Arc::clone(&self.socket);
        tokio::spawn(async move {
            // Max UDP payload is 65 507 bytes; give ourselves room.
            let mut buf = vec![0u8; 65_535];
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        debug!("udp reader cancelled");
                        break;
                    }
                    res = socket.recv_from(&mut buf) => {
                        match res {
                            Ok((n, peer)) => {
                                let bytes = Bytes::copy_from_slice(&buf[..n]);
                                if tx.send(Datagram { bytes, peer }).await.is_err() {
                                    debug!("udp reader: receiver dropped");
                                    break;
                                }
                            }
                            Err(e) => {
                                // A single recv failure shouldn't kill the loop.
                                error!(?e, "udp recv_from error");
                            }
                        }
                    }
                }
            }
        })
    }
}

impl Transport for UdpTransport {
    async fn send(&self, bytes: Bytes, peer: SocketAddr) -> std::io::Result<()> {
        let n = self.socket.send_to(&bytes, peer).await?;
        if n != bytes.len() {
            warn!(
                sent = n,
                expected = bytes.len(),
                %peer,
                "short UDP send"
            );
        }
        Ok(())
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}
