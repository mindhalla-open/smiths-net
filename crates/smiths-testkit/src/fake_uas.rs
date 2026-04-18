//! Tiny UAS for integration tests that exercise engine-initiated
//! outbound flows.
//!
//! Unlike `FakeUac`, this helper has no opinion about what requests
//! look like — it exposes raw receive + send primitives so each test
//! can script its own response behavior. That keeps the helper small
//! while staying generic across future flows (outbound INVITE, refer,
//! option checks, ...).

use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::time::timeout;

const RECV_TIMEOUT: Duration = Duration::from_secs(3);

/// One raw SIP request captured by the UAS, with the peer that sent it.
#[derive(Debug)]
pub struct CapturedRequest {
    /// Raw request bytes (as received on the wire).
    pub raw: Vec<u8>,
    /// Source peer.
    pub peer: SocketAddr,
}

impl CapturedRequest {
    /// Convenience: the raw bytes as a UTF-8 string (SIP is ASCII).
    #[must_use]
    pub fn as_str(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.raw)
    }
}

/// Minimal UAS: binds a UDP socket, captures requests, sends canned
/// responses.
pub struct FakeUas {
    socket: UdpSocket,
}

impl FakeUas {
    /// Bind on loopback (OS-chosen port).
    pub async fn bind() -> std::io::Result<Self> {
        let socket = UdpSocket::bind("127.0.0.1:0").await?;
        Ok(Self { socket })
    }

    /// Local bind address — pass this to the engine so it targets us.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Wait for one SIP request (`RECV_TIMEOUT` hard cap).
    pub async fn recv_request(&self) -> std::io::Result<CapturedRequest> {
        let mut buf = vec![0u8; 8192];
        let (n, peer) = timeout(RECV_TIMEOUT, self.socket.recv_from(&mut buf))
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "recv timeout"))??;
        buf.truncate(n);
        Ok(CapturedRequest { raw: buf, peer })
    }

    /// Send a raw SIP response back to `peer`. Caller composes every
    /// byte — headers, body, framing.
    pub async fn send_raw(&self, bytes: &[u8], peer: SocketAddr) -> std::io::Result<()> {
        let n = self.socket.send_to(bytes, peer).await?;
        if n != bytes.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "short UDP send",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn roundtrip_one_request_and_response() {
        let uas = FakeUas::bind().await.unwrap();
        let uas_addr = uas.local_addr().unwrap();

        // Client: send a hand-rolled OPTIONS to the UAS.
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client_addr = client.local_addr().unwrap();
        let req = b"OPTIONS sip:engine SIP/2.0\r\nCSeq: 1 OPTIONS\r\n\r\n";
        client.send_to(req, uas_addr).await.unwrap();

        let captured = uas.recv_request().await.unwrap();
        assert!(captured.as_str().starts_with("OPTIONS"));
        assert_eq!(captured.peer, client_addr);

        uas.send_raw(b"SIP/2.0 200 OK\r\n\r\n", captured.peer)
            .await
            .unwrap();

        let mut buf = vec![0u8; 4096];
        let (n, _) = timeout(RECV_TIMEOUT, client.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert!(
            std::str::from_utf8(&buf[..n])
                .unwrap()
                .starts_with("SIP/2.0 200")
        );
    }
}
