//! DTLS-SRTP handshake wiring for the UDP media fabric (slice
//! 5.10-dtls).
//!
//! The heavy lifting — cert loading, `DTLSConn` orchestration,
//! fingerprint verification, RFC 5764 §4.2 key export — lives
//! in [`smiths_dtls::DtlsLeg`]. This module adds the fabric-side
//! pieces that couldn't live there without pulling the full
//! media dep graph into `smiths-dtls`:
//!
//! - [`PeerBoundUdp`] — a minimal `webrtc_util::conn::Conn`
//!   adapter over a shared `tokio::net::UdpSocket`. The fabric
//!   owns the socket long-term; this adapter lets the handshake
//!   drive it without taking ownership, and the bridge
//!   forwarders reclaim the same `Arc<UdpSocket>` once the
//!   handshake completes. Bound to a fixed peer address: reads
//!   discard datagrams from any other source, writes always
//!   target the bound peer.
//! - [`HandshakeOutcome`] + [`classify_error`] — map
//!   [`smiths_dtls::DtlsHandshakeError`] onto the stable
//!   [`smiths_core::metrics::WebRtcDtlsOutcomeLabel`] vocabulary
//!   so the `smiths_webrtc_dtls_handshakes_total{outcome}`
//!   counter has bounded cardinality regardless of which
//!   webrtc-dtls error variant fires.
//!
//! ## Why a fixed peer
//!
//! Full WebRTC demultiplexes DTLS / STUN / SRTP over one ICE
//! candidate pair — the first-byte value tells the kernel which
//! demuxer to run (RFC 5764 §5.1.2). That demux lives in slice
//! 5.10-ice / 5.11-turn; until it lands the fabric's DTLS
//! handshake runs on the bridged UDP endpoint while that socket
//! carries *only* DTLS traffic (RTP flows after the handshake
//! completes). Packets from anyone other than the named peer
//! get dropped — no cross-talk with concurrent dialogs.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use smiths_core::SrtpKeys;
use smiths_dtls::DtlsHandshakeError;
use tokio::net::UdpSocket;
use webrtc_util::Error as WebRtcUtilError;
use webrtc_util::conn::Conn;

/// `webrtc_util::conn::Conn` wrapper that pins a
/// [`tokio::net::UdpSocket`] to one peer for the duration of a
/// handshake.
///
/// - `recv(buf)` reads one datagram; datagrams whose source
///   address differs from [`Self::peer`] are silently dropped
///   and the read retried. This protects the handshake from
///   stray traffic — STUN keep-alives, port-scanners, re-orderings
///   from a previous dialog, etc.
/// - `send(buf)` is always directed at [`Self::peer`]; the
///   underlying socket is **never** `connect()`ed, so the
///   bridge can continue to use `send_to` after the handshake
///   completes without a re-association.
/// - `local_addr()` reports the socket's bound address, as
///   `DTLSConn` expects.
/// - `close()` is a no-op — the fabric owns the socket's lifetime
///   through its endpoint map, and closing it here would break
///   the bridge that's about to reuse it.
#[derive(Clone, Debug)]
pub struct PeerBoundUdp {
    /// Shared socket (the fabric's endpoint). Cloned; dropping
    /// this value does not close the socket.
    pub sock: Arc<UdpSocket>,
    /// Address the handshake talks to. Typically the peer's
    /// media endpoint from the SDP offer's `c=` / `m=` line.
    pub peer: SocketAddr,
}

impl PeerBoundUdp {
    /// Convenience constructor that mirrors the fabric's
    /// endpoint-lookup shape.
    #[must_use]
    pub fn new(sock: Arc<UdpSocket>, peer: SocketAddr) -> Self {
        Self { sock, peer }
    }
}

#[async_trait]
impl Conn for PeerBoundUdp {
    async fn connect(&self, _addr: SocketAddr) -> Result<(), WebRtcUtilError> {
        // No-op: the wrapper is already peer-bound. DTLSConn calls
        // this for some transports; for UDP we treat it as idempotent.
        Ok(())
    }

    async fn recv(&self, buf: &mut [u8]) -> Result<usize, WebRtcUtilError> {
        // Drop-and-retry loop: discard datagrams whose source
        // doesn't match the bound peer so a burst of unrelated
        // STUN/NAT traffic can't feed the DTLS state machine a
        // non-peer frame.
        loop {
            let (n, from) = self.sock.recv_from(buf).await.map_err(webrtc_util_io)?;
            if from == self.peer {
                return Ok(n);
            }
            tracing::debug!(
                %from, expected = %self.peer,
                "PeerBoundUdp dropped datagram from unexpected source"
            );
        }
    }

    async fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr), WebRtcUtilError> {
        let n = self.recv(buf).await?;
        Ok((n, self.peer))
    }

    async fn send(&self, buf: &[u8]) -> Result<usize, WebRtcUtilError> {
        self.sock
            .send_to(buf, self.peer)
            .await
            .map_err(webrtc_util_io)
    }

    async fn send_to(&self, buf: &[u8], _target: SocketAddr) -> Result<usize, WebRtcUtilError> {
        // Force every send through the bound peer — callers that
        // try to talk to anyone else during the handshake are
        // almost certainly confused.
        self.send(buf).await
    }

    fn local_addr(&self) -> Result<SocketAddr, WebRtcUtilError> {
        self.sock.local_addr().map_err(webrtc_util_io)
    }

    fn remote_addr(&self) -> Option<SocketAddr> {
        Some(self.peer)
    }

    async fn close(&self) -> Result<(), WebRtcUtilError> {
        // The fabric owns the socket — closing here would break
        // the subsequent bridge that reuses the same Arc.
        Ok(())
    }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }
}

fn webrtc_util_io(e: io::Error) -> WebRtcUtilError {
    // `WebRtcUtilError` has a `From<io::Error>` impl through its
    // `IoError(io::Error)` newtype — the `?` ergonomics mirror
    // what the webrtc-util test code does.
    WebRtcUtilError::from(e)
}

/// Canonical outcome label for
/// [`smiths_core::metrics::Metrics::webrtc_dtls_handshakes`].
/// Stable vocabulary so dashboards survive downstream library
/// renames.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum HandshakeOutcome {
    /// Handshake completed and keys were exported.
    Success,
    /// Peer's cert hashed to something other than the
    /// SDP-advertised fingerprint.
    FingerprintMismatch,
    /// Peer advertised a weak fingerprint algorithm (anything
    /// other than `sha-256` today).
    UnsupportedAlgorithm,
    /// Cert loading / PEM round-trip failed — engine-local
    /// configuration issue.
    CertLoad,
    /// Anything else: handshake timeout, cipher mismatch,
    /// transport error — labelled together under `"other"` so
    /// operators watching the ratio dashboard still get a
    /// signal.
    Other,
}

impl HandshakeOutcome {
    /// Wire-form token stored under
    /// `WebRtcDtlsOutcomeLabel::outcome`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::FingerprintMismatch => "fingerprint_mismatch",
            Self::UnsupportedAlgorithm => "unsupported_algorithm",
            Self::CertLoad => "cert_load",
            Self::Other => "other",
        }
    }
}

/// Classify a handshake error into the fabric's metric
/// vocabulary. Pure — tests can walk every variant without
/// live sockets.
#[must_use]
pub fn classify_error(err: &DtlsHandshakeError) -> HandshakeOutcome {
    match err {
        DtlsHandshakeError::FingerprintMismatch { .. } => HandshakeOutcome::FingerprintMismatch,
        DtlsHandshakeError::UnsupportedAlgorithm(_) => HandshakeOutcome::UnsupportedAlgorithm,
        DtlsHandshakeError::CertLoad(_) => HandshakeOutcome::CertLoad,
        DtlsHandshakeError::Dtls(_) | DtlsHandshakeError::KeyExport(_) => HandshakeOutcome::Other,
    }
}

/// Successful-handshake witness. Only exists for the
/// `Success` variant; callers use this to thread the derived
/// [`SrtpKeys`] into the existing `BridgeLeg::with_srtp` path.
#[derive(Clone, Debug)]
pub struct HandshakeResult {
    /// SRTP keying material extracted per RFC 5764 §4.2.
    pub srtp: SrtpKeys,
    /// Wall-clock duration of the handshake. Logged on success;
    /// surfaces as a latency histogram bucket in a follow-on.
    pub elapsed: std::time::Duration,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_outcome_tokens_are_stable() {
        // Dashboards rely on the exact wire-token string.
        assert_eq!(HandshakeOutcome::Success.as_str(), "success");
        assert_eq!(
            HandshakeOutcome::FingerprintMismatch.as_str(),
            "fingerprint_mismatch"
        );
        assert_eq!(
            HandshakeOutcome::UnsupportedAlgorithm.as_str(),
            "unsupported_algorithm"
        );
        assert_eq!(HandshakeOutcome::CertLoad.as_str(), "cert_load");
        assert_eq!(HandshakeOutcome::Other.as_str(), "other");
    }

    #[test]
    fn classify_error_maps_every_variant() {
        let cases = [
            (
                DtlsHandshakeError::FingerprintMismatch {
                    expected: "AA".into(),
                    observed: "BB".into(),
                },
                HandshakeOutcome::FingerprintMismatch,
            ),
            (
                DtlsHandshakeError::UnsupportedAlgorithm("sha-1".into()),
                HandshakeOutcome::UnsupportedAlgorithm,
            ),
            (
                DtlsHandshakeError::CertLoad("bad DER".into()),
                HandshakeOutcome::CertLoad,
            ),
            (
                DtlsHandshakeError::Dtls("handshake timeout".into()),
                HandshakeOutcome::Other,
            ),
            (
                DtlsHandshakeError::KeyExport("state=idle".into()),
                HandshakeOutcome::Other,
            ),
        ];
        for (err, expected) in &cases {
            assert_eq!(
                classify_error(err),
                *expected,
                "classify_error({err:?}) should be {expected:?}"
            );
        }
    }
}
