//! [`MediaFabric`] implementation on plain UDP.
//!
//! Owns all media sockets, one pair per endpoint (even RTP / odd
//! RTCP). Hands out [`MediaEndpoint`] trait objects to the signaling
//! layer, which never touches a socket directly. On
//! [`MediaFabric::bridge`], spins up the SSRC-rewriting forwarder
//! from [`crate::bridge`] and retains the [`Bridge`] so that a later
//! `release_bridge` call can await its shutdown.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use dashmap::DashMap;
use smiths_core::media::{
    BridgeId, BridgeLeg, Endpoint, EndpointId, MediaEndpoint, MediaError, MediaFabric,
};
use smiths_core::sdp::SrtpKeys;
use smiths_core::{Metrics, SrtpTransform};
use tokio::net::UdpSocket;
use tracing::{debug, instrument, warn};

use crate::bridge::{Bridge, BridgeConfig, Leg, LegSrtp, RtcpLeg};
use crate::dtls::{HandshakeOutcome, HandshakeResult, PeerBoundUdp, classify_error};
use crate::port_allocator::{DEFAULT_MAX_ATTEMPTS, allocate_rtp_rtcp_pair};
use crate::srtp::AesCmHmacSha1_80Transform;
use smiths_core::metrics::WebRtcDtlsOutcomeLabel;
use smiths_dtls::{DtlsHandshakeError, DtlsLeg, DtlsLegConfig};

/// Materialize [`LegSrtp`] transforms from negotiated [`SrtpKeys`].
/// `None` in → `None` out (plain RTP). Failures propagate as
/// [`MediaError::Io`] wrapping an `InvalidData` I/O error; the only
/// realistic cause is wrong key-material length for the suite, which
/// the negotiator path shouldn't produce.
fn build_leg_srtp(keys: Option<&SrtpKeys>) -> Result<Option<LegSrtp>, MediaError> {
    let Some(keys) = keys else {
        return Ok(None);
    };
    // `AesCmHmacSha1_80Transform::from_sdes` is the only suite wired
    // today; extending to another profile is a match on `keys.suite`.
    let peer_tx = AesCmHmacSha1_80Transform::from_sdes(&keys.peer_tx_key).map_err(|e| {
        warn!(?e, "SRTP peer-tx transform init failed");
        std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
    })?;
    let local_tx = AesCmHmacSha1_80Transform::from_sdes(&keys.local_tx_key).map_err(|e| {
        warn!(?e, "SRTP local-tx transform init failed");
        std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
    })?;
    Ok(Some(LegSrtp {
        peer_tx: Arc::new(peer_tx) as Arc<dyn SrtpTransform>,
        local_tx: Arc::new(local_tx) as Arc<dyn SrtpTransform>,
    }))
}

/// RTP + RTCP socket pair the fabric owns for one endpoint.
struct EndpointSockets {
    rtp: Arc<UdpSocket>,
    /// RTCP socket paired with `rtp` (port = `rtp_port` + 1). Bridges
    /// emit Sender Reports from it and consume the peer's RTCP on it,
    /// unless the peer negotiated `a=rtcp-mux`, in which case RTCP
    /// rides the RTP socket instead.
    rtcp: Arc<UdpSocket>,
}

/// Build the bridge-level [`Leg`] for one [`BridgeLeg`]: honors the
/// leg's explicit RTCP destination (`a=rtcp:` / `a=rtcp-mux`) and
/// clock rate, falling back to the RFC 3550 §11 port + 1 convention.
fn build_leg(req: &BridgeLeg, sockets: &EndpointSockets, srtp: Option<LegSrtp>) -> Leg {
    let rtcp_socket = if req.rtcp_muxed() {
        Arc::clone(&sockets.rtp)
    } else {
        Arc::clone(&sockets.rtcp)
    };
    Leg {
        socket: Arc::clone(&sockets.rtp),
        peer: req.peer,
        rtcp: Some(RtcpLeg {
            socket: rtcp_socket,
            peer: req.rtcp_destination(),
        }),
        srtp,
        clock_rate: req.clock_rate,
    }
}

/// Default UDP-backed [`MediaFabric`].
pub struct UdpMediaFabric {
    next_endpoint: AtomicU64,
    next_bridge: AtomicU64,
    endpoints: DashMap<EndpointId, EndpointSockets>,
    bridges: DashMap<BridgeId, Bridge>,
    /// Shared metrics handle. `None` on test fabrics; the CLI wires
    /// the engine-wide `Arc<Metrics>` via [`Self::with_metrics`].
    metrics: Option<Arc<Metrics>>,
    /// DTMF sink wired into every subsequently-spawned bridge. `None`
    /// = bridges don't detect DTMF. Set via [`Self::with_dtmf_sink`]
    /// — the CLI plumbs a `BusDtmfSink` here so RFC 4733 keypresses
    /// land on the event bus.
    dtmf_sink: Option<Arc<dyn smiths_core::dtmf::DtmfSink>>,
    /// `true` → spawned bridges also run the Goertzel inband
    /// detector on plaintext PCMU. Off by default.
    inband_dtmf: bool,
    /// Optional `(min, max)` UDP port window for RTP/RTCP allocation.
    /// `None` (default) = ephemeral OS-assigned ports. Set via
    /// [`Self::with_rtp_port_range`] so operators can firewall a fixed
    /// range.
    rtp_ports: Option<(u16, u16)>,
    /// RTCP Sender Report cadence for spawned bridges. `None` disables
    /// emission; the default is `BridgeConfig::default`'s 5 s.
    rtcp_interval: Option<std::time::Duration>,
}

impl Default for UdpMediaFabric {
    fn default() -> Self {
        Self {
            next_endpoint: AtomicU64::new(0),
            next_bridge: AtomicU64::new(0),
            endpoints: DashMap::new(),
            bridges: DashMap::new(),
            metrics: None,
            dtmf_sink: None,
            inband_dtmf: false,
            rtp_ports: None,
            rtcp_interval: BridgeConfig::default().rtcp_interval,
        }
    }
}

impl UdpMediaFabric {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the RTCP Sender Report cadence for every subsequently
    /// spawned bridge (`None` disables emission). Builder-style.
    #[must_use]
    pub fn with_rtcp_interval(mut self, interval: Option<std::time::Duration>) -> Self {
        self.rtcp_interval = interval;
        self
    }

    /// Attach a metrics handle. Builder-style so existing tests can
    /// keep using `new` without changes.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Pin RTP/RTCP allocation to the inclusive `[min, max]` UDP port
    /// window so the media plane fits one firewall rule. `None` reverts
    /// to ephemeral ports. Builder-style for the same reason as
    /// [`Self::with_metrics`].
    #[must_use]
    pub fn with_rtp_port_range(mut self, range: Option<(u16, u16)>) -> Self {
        self.rtp_ports = range;
        self
    }

    /// Attach a DTMF sink. Every bridge spawned after this call
    /// receives RFC 4733 keypresses through the sink; bridges
    /// spawned before keep their (empty) config. In practice the
    /// CLI calls this once at boot and never again.
    #[must_use]
    pub fn with_dtmf_sink(mut self, sink: Arc<dyn smiths_core::dtmf::DtmfSink>) -> Self {
        self.dtmf_sink = Some(sink);
        self
    }

    /// Opt every subsequently-spawned bridge into the Goertzel
    /// inband DTMF detector. No-op without a wired
    /// sink — detected presses need somewhere to go.
    #[must_use]
    pub fn with_inband_dtmf(mut self, enabled: bool) -> Self {
        self.inband_dtmf = enabled;
        self
    }

    fn fresh_endpoint_id(&self) -> EndpointId {
        EndpointId(self.next_endpoint.fetch_add(1, Ordering::Relaxed))
    }

    fn fresh_bridge_id(&self) -> BridgeId {
        BridgeId(self.next_bridge.fetch_add(1, Ordering::Relaxed))
    }

    /// Direct handle on the RTP socket allocated for `id`
    /// ( + 5.6e-runtime). Trait-level
    /// `MediaFabric::bridge` is purpose-built for the plain
    /// SSRC-rewriting case; non-passthrough sessions
    /// (`UdptlSession`, `ConferenceParticipantSession`,
    /// future SRTP-transforming flows) need raw socket access
    /// to spawn their own forwarder tasks. Returns `None` when
    /// the endpoint was never allocated or has been released.
    ///
    /// Kept out of the `MediaFabric` trait because it's a
    /// concrete-implementation escape hatch — if a future
    /// fabric variant doesn't use UDP sockets at all (a WASM
    /// host fabric, say), the trait shouldn't force the
    /// concept.
    #[must_use]
    pub fn endpoint_socket(&self, id: EndpointId) -> Option<Arc<UdpSocket>> {
        self.endpoints.get(&id).map(|e| Arc::clone(&e.rtp))
    }

    /// Per-direction RTP/RTCP statistics of a live bridge, or `None`
    /// once it has been released (or never existed).
    #[must_use]
    pub fn bridge_stats(&self, id: BridgeId) -> Option<crate::bridge::BridgeStats> {
        self.bridges.get(&id).map(|b| b.stats())
    }

    /// Drive the DTLS-SRTP handshake for a WebRTC leg (slice
    /// 5.10-dtls) against the fabric's UDP endpoint. Returns
    /// the SRTP keying material callers should thread into
    /// [`smiths_core::BridgeLeg::with_srtp`] when they eventually
    /// call [`MediaFabric::bridge`].
    ///
    /// **The socket is not `connect`ed.** A [`PeerBoundUdp`]
    /// adapter wraps it for the handshake and reads only the
    /// peer's datagrams; after this returns, the same
    /// `Arc<UdpSocket>` is still free for bridge forwarders to
    /// use via `send_to` / `recv_from` as before.
    ///
    /// The metrics handle (if configured) observes the
    /// handshake outcome on
    /// `smiths_webrtc_dtls_handshakes_total{outcome}` with the
    /// stable labels from [`HandshakeOutcome`]. Errors are also
    /// logged with the endpoint id + peer address so operators
    /// can correlate with the offer's SDP `o=` line in the
    /// calling handler's log.
    ///
    /// # Errors
    /// [`MediaError::UnknownEndpoint`] when `endpoint` was
    /// never allocated. [`MediaError::Io`] wrapping a
    /// `DtlsHandshakeError` message for every other failure
    /// mode (fingerprint mismatch, cert load, transport, etc.).
    #[instrument(skip(self, dtls), fields(?endpoint, %peer))]
    pub async fn run_dtls_handshake(
        &self,
        endpoint: EndpointId,
        peer: SocketAddr,
        dtls: DtlsLegConfig,
    ) -> Result<HandshakeResult, MediaError> {
        let sock = self
            .endpoint_socket(endpoint)
            .ok_or(MediaError::UnknownEndpoint(endpoint))?;
        let conn = Arc::new(PeerBoundUdp::new(sock, peer));
        let leg = DtlsLeg::new(dtls);
        let started = std::time::Instant::now();
        let handshake = leg.handshake(conn).await;
        let elapsed = started.elapsed();
        match handshake {
            Ok(srtp) => {
                self.bump_dtls_metric(HandshakeOutcome::Success);
                debug!(
                    ?endpoint, %peer, ?elapsed,
                    "DTLS-SRTP handshake completed"
                );
                Ok(HandshakeResult { srtp, elapsed })
            }
            Err(e) => {
                let outcome = classify_error(&e);
                self.bump_dtls_metric(outcome);
                warn!(
                    ?endpoint,
                    %peer,
                    ?elapsed,
                    outcome = outcome.as_str(),
                    err = %e,
                    "DTLS-SRTP handshake failed"
                );
                Err(dtls_err_to_media(&e))
            }
        }
    }

    fn bump_dtls_metric(&self, outcome: HandshakeOutcome) {
        if let Some(m) = &self.metrics {
            m.webrtc_dtls_handshakes
                .get_or_create(&WebRtcDtlsOutcomeLabel {
                    outcome: outcome.as_str().to_owned(),
                })
                .inc();
        }
    }
}

/// Map a [`DtlsHandshakeError`] into the fabric's error type.
/// Kept non-lossy: the full error string rides on the
/// `Io::other` wrapper so operators can grep logs without
/// losing detail, and callers can still classify via the
/// original via [`classify_error`].
fn dtls_err_to_media(e: &DtlsHandshakeError) -> MediaError {
    MediaError::Io(std::io::Error::other(e.to_string()))
}

#[async_trait]
impl MediaFabric for UdpMediaFabric {
    #[instrument(skip(self), fields(%bind_ip))]
    async fn allocate(&self, bind_ip: IpAddr) -> Result<Arc<dyn MediaEndpoint>, MediaError> {
        let pair = allocate_rtp_rtcp_pair(bind_ip, self.rtp_ports, DEFAULT_MAX_ATTEMPTS).await?;
        let id = self.fresh_endpoint_id();
        let rtp_addr = pair.rtp_addr;
        let rtcp_addr = pair.rtcp_addr;
        self.endpoints.insert(
            id,
            EndpointSockets {
                rtp: Arc::new(pair.rtp),
                rtcp: Arc::new(pair.rtcp),
            },
        );
        debug!(?id, %rtp_addr, %rtcp_addr, "media endpoint allocated");
        Ok(Arc::new(Endpoint {
            id,
            local_addr: rtp_addr,
            rtcp_addr: Some(rtcp_addr),
        }))
    }

    #[instrument(skip(self, a, b), fields(?a.endpoint, ?b.endpoint, peer_a = %a.peer, peer_b = %b.peer))]
    async fn bridge(&self, a: BridgeLeg, b: BridgeLeg) -> Result<BridgeId, MediaError> {
        let srtp_a = build_leg_srtp(a.srtp.as_ref())?;
        let srtp_b = build_leg_srtp(b.srtp.as_ref())?;
        let leg_a = {
            let entry = self
                .endpoints
                .get(&a.endpoint)
                .ok_or(MediaError::UnknownEndpoint(a.endpoint))?;
            build_leg(&a, &entry, srtp_a)
        };
        let leg_b = {
            let entry = self
                .endpoints
                .get(&b.endpoint)
                .ok_or(MediaError::UnknownEndpoint(b.endpoint))?;
            build_leg(&b, &entry, srtp_b)
        };

        let id = self.fresh_bridge_id();
        let cfg = BridgeConfig {
            rtcp_interval: self.rtcp_interval,
            metrics: self.metrics.clone(),
            dtmf_sink: self.dtmf_sink.clone(),
            inband_dtmf: self.inband_dtmf,
        };
        let bridge = Bridge::spawn_with(id, &leg_a, &leg_b, &cfg);
        self.bridges.insert(id, bridge);
        if let Some(m) = &self.metrics {
            m.bridges_active.inc();
        }
        Ok(id)
    }

    async fn release_bridge(&self, id: BridgeId) {
        if let Some((_, bridge)) = self.bridges.remove(&id) {
            bridge.shutdown().await;
            if let Some(m) = &self.metrics {
                m.bridges_active.dec();
            }
        }
    }

    async fn release_endpoint(&self, id: EndpointId) {
        // Dropping the `EndpointSockets` closes both UDP sockets unless
        // a forwarder task still holds a clone of `rtp`.
        self.endpoints.remove(&id);
    }

    async fn send_packet(
        &self,
        src: EndpointId,
        dest: SocketAddr,
        bytes: &[u8],
    ) -> Result<(), MediaError> {
        let sock = self
            .endpoints
            .get(&src)
            .ok_or(MediaError::UnknownEndpoint(src))?
            .rtp
            .clone();
        sock.send_to(bytes, dest).await.map_err(MediaError::Io)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::time::Duration;
    use tokio::time::timeout;

    #[tokio::test(flavor = "multi_thread")]
    async fn allocate_returns_bound_local_addr() {
        let fab = UdpMediaFabric::new();
        let ep = fab.allocate(IpAddr::V4(Ipv4Addr::LOCALHOST)).await.unwrap();
        assert_eq!(ep.local_addr().ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_ne!(ep.local_addr().port(), 0);
        assert_eq!(ep.local_addr().port() % 2, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bridge_forwards_between_allocated_endpoints() {
        let fab = UdpMediaFabric::new();
        let ep_a = fab.allocate(IpAddr::V4(Ipv4Addr::LOCALHOST)).await.unwrap();
        let ep_b = fab.allocate(IpAddr::V4(Ipv4Addr::LOCALHOST)).await.unwrap();

        // Stand-in for UA-A / UA-B.
        let ua_a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let ua_b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr_ua_a = ua_a.local_addr().unwrap();
        let addr_ua_b = ua_b.local_addr().unwrap();

        let bid = fab
            .bridge(
                BridgeLeg::plain(ep_a.id(), addr_ua_a),
                BridgeLeg::plain(ep_b.id(), addr_ua_b),
            )
            .await
            .unwrap();

        // Minimal valid RTP header (V=2, PT=0 PCMU, SEQ=1, TS=0, SSRC=0xDEAD_BEEF)
        // + 4 bytes of payload. Must be parseable by the SSRC router.
        let rtp_a_to_b: &[u8] = &[
            0x80, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0xDE, 0xAD, 0xBE, 0xEF, 1, 2, 3, 4,
        ];
        ua_a.send_to(rtp_a_to_b, ep_a.local_addr()).await.unwrap();
        let mut buf = [0u8; 256];
        let (n, _) = timeout(Duration::from_secs(1), ua_b.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        // Payload preserved.
        assert_eq!(&buf[12..n], &[1, 2, 3, 4]);

        fab.release_bridge(bid).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bridge_honors_explicit_rtcp_peer() {
        use crate::rtcp::parse_sr;

        let fab = UdpMediaFabric::new().with_rtcp_interval(Some(Duration::from_millis(100)));
        let ep_a = fab.allocate(IpAddr::V4(Ipv4Addr::LOCALHOST)).await.unwrap();
        let ep_b = fab.allocate(IpAddr::V4(Ipv4Addr::LOCALHOST)).await.unwrap();
        let ua_a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let ua_b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        // UA-A's RTCP lives on an unrelated port (what `a=rtcp:` says).
        let ua_a_rtcp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let ua_a_rtcp_addr = ua_a_rtcp.local_addr().unwrap();

        let bid = fab
            .bridge(
                BridgeLeg::plain(ep_a.id(), ua_a.local_addr().unwrap())
                    .with_rtcp_peer(ua_a_rtcp_addr),
                BridgeLeg::plain(ep_b.id(), ua_b.local_addr().unwrap()),
            )
            .await
            .unwrap();
        let mut buf = [0u8; 256];
        let (n, from) = timeout(Duration::from_secs(2), ua_a_rtcp.recv_from(&mut buf))
            .await
            .expect("SR must arrive at the explicit rtcp peer")
            .unwrap();
        assert!(parse_sr(&buf[..n]).is_some());
        assert_eq!(
            from.port(),
            ep_a.local_addr().port() + 1,
            "non-muxed RTCP is sent from the endpoint's RTCP socket"
        );
        fab.release_bridge(bid).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rtcp_mux_rides_the_rtp_socket_both_ways() {
        use crate::rtcp::{ReportBlock, build_rr, is_rtcp};

        let fab = UdpMediaFabric::new().with_rtcp_interval(Some(Duration::from_millis(100)));
        let ep_a = fab.allocate(IpAddr::V4(Ipv4Addr::LOCALHOST)).await.unwrap();
        let ep_b = fab.allocate(IpAddr::V4(Ipv4Addr::LOCALHOST)).await.unwrap();
        let ua_a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let ua_b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let ua_a_addr = ua_a.local_addr().unwrap();

        let bid = fab
            .bridge(
                // `a=rtcp-mux`: RTCP destination == RTP destination.
                BridgeLeg::plain(ep_a.id(), ua_a_addr).with_rtcp_peer(ua_a_addr),
                BridgeLeg::plain(ep_b.id(), ua_b.local_addr().unwrap()),
            )
            .await
            .unwrap();
        let stats = fab.bridge_stats(bid).expect("live bridge");

        // UA-A's RTCP arrives on the engine's RTP port and is absorbed.
        let rb = ReportBlock {
            ssrc: stats.ssrc_toward_a,
            fraction_lost: 9,
            cumulative_lost: 1,
            extended_highest_seq: 1,
            jitter: 0,
            last_sr: 0,
            delay_since_last_sr: 0,
        };
        ua_a.send_to(&build_rr(0xA, &rb), ep_a.local_addr())
            .await
            .unwrap();
        timeout(Duration::from_secs(2), async {
            loop {
                if fab
                    .bridge_stats(bid)
                    .is_some_and(|s| s.b_to_a.peer_reports == 1)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("muxed RR absorbed");
        assert_eq!(fab.bridge_stats(bid).unwrap().b_to_a.peer_fraction_lost, 9);

        // The engine's SR reaches UA-A's RTP socket, sent from the
        // engine's RTP port (not port + 1).
        let mut buf = [0u8; 256];
        let (n, from) = timeout(Duration::from_secs(2), ua_a.recv_from(&mut buf))
            .await
            .expect("muxed SR on the RTP socket")
            .unwrap();
        assert!(is_rtcp(&buf[..n]));
        assert_eq!(from, ep_a.local_addr());
        fab.release_bridge(bid).await;
        assert!(fab.bridge_stats(bid).is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bridge_with_unknown_endpoint_errors() {
        let fab = UdpMediaFabric::new();
        let ep = fab.allocate(IpAddr::V4(Ipv4Addr::LOCALHOST)).await.unwrap();
        let bogus = EndpointId(9999);
        let err = fab
            .bridge(
                BridgeLeg::plain(ep.id(), "127.0.0.1:1".parse().unwrap()),
                BridgeLeg::plain(bogus, "127.0.0.1:2".parse().unwrap()),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, MediaError::UnknownEndpoint(x) if x == bogus));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn release_bridge_is_idempotent() {
        let fab = UdpMediaFabric::new();
        let ep_a = fab.allocate(IpAddr::V4(Ipv4Addr::LOCALHOST)).await.unwrap();
        let ep_b = fab.allocate(IpAddr::V4(Ipv4Addr::LOCALHOST)).await.unwrap();
        let ua_a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let ua_b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let bid = fab
            .bridge(
                BridgeLeg::plain(ep_a.id(), ua_a.local_addr().unwrap()),
                BridgeLeg::plain(ep_b.id(), ua_b.local_addr().unwrap()),
            )
            .await
            .unwrap();
        fab.release_bridge(bid).await;
        // Second release must not panic or block.
        fab.release_bridge(bid).await;
    }
}
