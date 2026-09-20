//! DTLS-SRTP loopback harness.
//!
//! Runs the engine's real DTLS-SRTP keying path between two media
//! endpoints on loopback and then exercises the derived keys with
//! SRTP protect / unprotect in both directions. It is the cheapest
//! end-to-end check that the pieces a WebRTC call depends on —
//! self-signed cert minting, fingerprint verification, RFC 5764 key
//! export, and the SRTP transform — agree with each other.
//!
//! What it covers: cert generation, `a=fingerprint` verification,
//! DTLS handshake over real UDP sockets, keying-material export, and
//! SRTP interop between the two derived key stacks.
//!
//! What it does not cover: ICE, SDP signaling, and any browser. A
//! browser-driven test needs a Chromium dependency this workspace
//! deliberately does not carry.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use smiths_core::media::SrtpTransform;
use smiths_core::{MediaFabric, SelfSignedCert};
use smiths_dtls::{DtlsLegConfig, DtlsRole};
use smiths_media::UdpMediaFabric;
use smiths_media::srtp::AesCmHmacSha1_80Transform;
use smiths_sdp::Fingerprint;

/// Configuration for a [`DtlsLoopbackHarness`] run.
#[derive(Clone, Debug)]
pub struct HarnessConfig {
    /// RTP packets to push through the derived keys in each
    /// direction.
    pub packets: u32,
    /// Upper bound on the DTLS handshake. Exceeding it fails the run
    /// rather than hanging the test suite.
    pub handshake_timeout: Duration,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            packets: 8,
            handshake_timeout: Duration::from_secs(10),
        }
    }
}

/// Drives one DTLS-SRTP loopback exchange.
#[derive(Debug)]
pub struct DtlsLoopbackHarness {
    /// Config the harness was instantiated with.
    pub config: HarnessConfig,
}

impl DtlsLoopbackHarness {
    /// Build a harness with the supplied config. Nothing binds until
    /// [`Self::run`] is called.
    #[must_use]
    pub fn new(config: HarnessConfig) -> Self {
        Self { config }
    }

    /// Bind two endpoints, run the handshake between them, and push
    /// `config.packets` SRTP packets through the derived keys in each
    /// direction.
    ///
    /// # Errors
    ///
    /// Fails when an endpoint cannot bind, the handshake errors or
    /// exceeds `handshake_timeout`, the two sides derive keys that do
    /// not mirror, or an SRTP round trip does not reproduce the
    /// original packet.
    pub async fn run(&self) -> Result<LoopbackStats, HarnessError> {
        let loopback = IpAddr::from([127, 0, 0, 1]);
        let client_fabric = Arc::new(UdpMediaFabric::new());
        let server_fabric = Arc::new(UdpMediaFabric::new());

        let client_endpoint = client_fabric
            .allocate(loopback)
            .await
            .map_err(|e| HarnessError::Media(e.to_string()))?;
        let server_endpoint = server_fabric
            .allocate(loopback)
            .await
            .map_err(|e| HarnessError::Media(e.to_string()))?;
        let client_addr = client_endpoint.local_addr();
        let server_addr = server_endpoint.local_addr();

        let client_cert = SelfSignedCert::generate("harness-client")
            .map_err(|e| HarnessError::Cert(e.to_string()))?;
        let server_cert = SelfSignedCert::generate("harness-server")
            .map_err(|e| HarnessError::Cert(e.to_string()))?;

        let client_cfg = DtlsLegConfig {
            local_cert: client_cert.clone(),
            role: DtlsRole::Client,
            peer_fingerprint: Fingerprint {
                algorithm: "sha-256".into(),
                value: server_cert.sha256_fingerprint.clone(),
            },
        };
        let server_cfg = DtlsLegConfig {
            local_cert: server_cert,
            role: DtlsRole::Server,
            peer_fingerprint: Fingerprint {
                algorithm: "sha-256".into(),
                value: client_cert.sha256_fingerprint,
            },
        };

        // A DTLS handshake is a synchronous back-and-forth: both legs
        // have to be in flight or the first ClientHello has nobody to
        // answer it.
        let client_id = client_endpoint.id();
        let server_id = server_endpoint.id();
        let client_task = Arc::clone(&client_fabric);
        let server_task = Arc::clone(&server_fabric);
        let client_handle = tokio::spawn(async move {
            client_task
                .run_dtls_handshake(client_id, server_addr, client_cfg)
                .await
        });
        let server_handle = tokio::spawn(async move {
            server_task
                .run_dtls_handshake(server_id, client_addr, server_cfg)
                .await
        });

        let both = async { tokio::try_join!(client_handle, server_handle) };
        let (client_res, server_res) = tokio::time::timeout(self.config.handshake_timeout, both)
            .await
            .map_err(|_| HarnessError::HandshakeTimeout(self.config.handshake_timeout))?
            .map_err(|e| HarnessError::Media(format!("handshake task panicked: {e}")))?;

        let client = client_res.map_err(|e| HarnessError::Handshake(e.to_string()))?;
        let server = server_res.map_err(|e| HarnessError::Handshake(e.to_string()))?;

        // RFC 5764 splits one export into a client stack and a server
        // stack. Each side transmits with its own and receives with
        // the peer's, so the two must mirror exactly.
        if client.srtp.local_tx_key != server.srtp.peer_tx_key
            || server.srtp.local_tx_key != client.srtp.peer_tx_key
        {
            return Err(HarnessError::KeyMismatch);
        }

        let client_tx = AesCmHmacSha1_80Transform::from_sdes(&client.srtp.local_tx_key)
            .map_err(|e| HarnessError::Srtp(e.to_string()))?;
        let server_rx = AesCmHmacSha1_80Transform::from_sdes(&client.srtp.local_tx_key)
            .map_err(|e| HarnessError::Srtp(e.to_string()))?;
        let server_tx = AesCmHmacSha1_80Transform::from_sdes(&server.srtp.local_tx_key)
            .map_err(|e| HarnessError::Srtp(e.to_string()))?;
        let client_rx = AesCmHmacSha1_80Transform::from_sdes(&server.srtp.local_tx_key)
            .map_err(|e| HarnessError::Srtp(e.to_string()))?;

        let forward_packets = pump(&client_tx, &server_rx, self.config.packets, 0x1234_5678)?;
        let reverse_packets = pump(&server_tx, &client_rx, self.config.packets, 0x8765_4321)?;

        Ok(LoopbackStats {
            forward_packets,
            reverse_packets,
            handshake: client.elapsed.max(server.elapsed),
        })
    }
}

/// Protect `count` synthetic RTP packets with `tx` and unprotect them
/// with `rx`, checking each one round-trips byte for byte.
fn pump(
    tx: &dyn SrtpTransform,
    rx: &dyn SrtpTransform,
    count: u32,
    ssrc: u32,
) -> Result<u64, HarnessError> {
    let mut delivered = 0;
    for seq in 0..count {
        #[allow(clippy::cast_possible_truncation)] // seq is bounded by `count`, a u32 packet count
        let packet = rtp_packet(seq as u16, ssrc);
        let protected = tx
            .protect_rtp(&packet)
            .map_err(|e| HarnessError::Srtp(e.to_string()))?;
        let recovered = rx
            .unprotect_rtp(&protected)
            .map_err(|e| HarnessError::Srtp(e.to_string()))?;
        if recovered != packet {
            return Err(HarnessError::PayloadMismatch { seq });
        }
        delivered += 1;
    }
    Ok(delivered)
}

/// A minimal PCMU packet: 12-byte header plus 160 samples of a
/// recognizable ramp, which is enough for the crypto round trip.
fn rtp_packet(seq: u16, ssrc: u32) -> Vec<u8> {
    let mut packet = Vec::with_capacity(172);
    packet.push(0x80); // version 2, no padding, no extension, no CSRC
    packet.push(0x00); // PCMU, marker clear
    packet.extend_from_slice(&seq.to_be_bytes());
    packet.extend_from_slice(&(u32::from(seq) * 160).to_be_bytes());
    packet.extend_from_slice(&ssrc.to_be_bytes());
    packet.extend((0..160u16).map(|i| u8::try_from(i % 256).expect("modulo 256 fits u8")));
    packet
}

/// Outcome of one harness run.
#[derive(Clone, Debug)]
pub struct LoopbackStats {
    /// Packets that survived client→server protect + unprotect.
    pub forward_packets: u64,
    /// Packets that survived server→client protect + unprotect.
    pub reverse_packets: u64,
    /// How long the slower of the two handshake legs took.
    pub handshake: Duration,
}

/// Why a harness run did not complete.
#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    /// An endpoint could not be allocated from the media fabric.
    #[error("media fabric: {0}")]
    Media(String),
    /// Minting a self-signed cert failed.
    #[error("certificate: {0}")]
    Cert(String),
    /// The DTLS handshake itself failed on one of the legs.
    #[error("dtls handshake: {0}")]
    Handshake(String),
    /// The handshake did not finish inside the configured budget.
    #[error("dtls handshake did not complete within {0:?}")]
    HandshakeTimeout(Duration),
    /// The two legs exported keying material that does not mirror,
    /// so each side would decrypt the other's traffic as garbage.
    #[error("exported SRTP keys do not mirror between the two legs")]
    KeyMismatch,
    /// An SRTP protect or unprotect call failed.
    #[error("srtp: {0}")]
    Srtp(String),
    /// A packet survived unprotect but came back different.
    #[error("packet {seq} did not survive the SRTP round trip intact")]
    PayloadMismatch {
        /// Sequence number of the packet that did not match.
        seq: u32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_pumps_eight_packets() {
        let cfg = HarnessConfig::default();
        assert_eq!(cfg.packets, 8);
        assert_eq!(cfg.handshake_timeout, Duration::from_secs(10));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn loopback_run_keys_mirror_and_srtp_round_trips() {
        let harness = DtlsLoopbackHarness::new(HarnessConfig {
            packets: 4,
            ..HarnessConfig::default()
        });
        let stats = harness.run().await.expect("harness run");
        assert_eq!(stats.forward_packets, 4);
        assert_eq!(stats.reverse_packets, 4);
    }
}
