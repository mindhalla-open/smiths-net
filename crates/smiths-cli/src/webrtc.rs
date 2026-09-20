//! WebRTC-native signaling runtime.
//!
//! Hosts the axum WebSocket route + the concrete
//! [`WebRtcSessionHandler`] impl that routes offers through the
//! shared `SdpNegotiator`. Lives in `smiths-cli` so the CLI owns
//! the axum dep and `smiths-sip` stays free of a `smiths-sdp`
//! dep (same shape as the UAS ⇄ negotiator split on the SIP side).
//!
//! ## What ships today
//!
//! - [`CliWebRtcHandler`] — wraps the shared SDP negotiator +
//!   media fabric + tag-keyed rendezvous map. `handle_offer_tagged`
//!   parses the SDP, negotiates an answer (DTLS-SRTP accepted
//!   when a cert is configured), allocates a media endpoint,
//!   runs the DTLS handshake when appropriate, and either parks
//!   the leg in [`PendingLegs`] awaiting its partner or installs
//!   the bridge against the already-parked partner.
//! - [`serve_webrtc`] — axum router with `GET /smiths/webrtc`
//!   upgrading to a WebSocket, piping binary frames through the
//!   listener's `handle_frame`.
//!
//! ## Rendezvous semantics
//!
//! A session's optional `tag` (set on the client's
//! `session-init` frame) is the rendezvous key. The first leg
//! with tag `X` parks its endpoint + DTLS context in the
//! handler's map; the second leg with tag `X` pulls the
//! partner's endpoint and calls `MediaFabric::bridge` — the
//! answer the engine sends back is the same single-leg answer
//! as before, and audio starts flowing immediately.
//!
//! Unpaired legs are evicted after
//! [`DEFAULT_RENDEZVOUS_DEADLINE`] (30 s). Eviction releases
//! the endpoint and bumps
//! `smiths_webrtc_sessions_paired_total{partner="none"}` so
//! dashboards alert on a rising slope of orphaned legs.
//!
//! ## Boundaries
//!
//! - **ICE.** Answers carry host candidates from the negotiator
//!   and, when `[webrtc.ice] stun_servers` is set, server-reflexive
//!   candidates gathered with one STUN Binding per server against
//!   the leg's own media socket. Connectivity checks are ICE-lite:
//!   the DTLS handshake and RTP go to the peer address the offer
//!   advertised.
//! - **SIP ↔ WebRTC.** A SIP INVITE carrying
//!   `X-Smiths-Webrtc-Tag` pairs with a parked WebRTC leg through
//!   [`smiths_core::WebRtcRendezvous`]; two WebRTC legs sharing a
//!   tag pair with each other.
//! - **TLS termination.** `serve_webrtc` binds plain HTTP /
//!   WebSocket; production deployments front the engine with a
//!   TLS terminator for `wss://` (`docs/deployment/webrtc.md`).
//!   `Config::validate` refuses `[webrtc] tls_cert` / `tls_key`.
//! - **Media ports.** The adapter's fabric honors
//!   `[media.rtp_ports]` like the SIP fabric does, so one firewall
//!   window covers both.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use dashmap::DashMap;
use smiths_core::metrics::{IceCandidateTypeLabel, PrivacyRejectReasonLabel, WebRtcPartnerLabel};
use smiths_core::{
    BridgeId, BridgeLeg, Config, EndpointId, MediaFabric, Metrics, NegotiationOutcome,
    SdpNegotiator, SelfSignedCert, Shutdown, SrtpKeys, WebRtcPrivacyConfig, WebRtcPrivacyMode,
};
use smiths_dtls::{DtlsLegConfig, DtlsRole as DtlsLegRole};
use smiths_media::UdpMediaFabric;
use smiths_sdp::SessionDescription;
use smiths_sdp::privacy::{OfferPrivacyVerdict, redact_ip, reject_direct_candidates};
use smiths_sdp::types::IceCandidate;
use smiths_sip::webrtc::{
    WebRtcHandlerError, WebRtcSession, WebRtcSessionHandler, WebSocketSignalingListener,
};
use smiths_sip::webtransport::WebTransportSessionId;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// How long connectivity checks may run before the offer is
/// rejected. Browsers give up around 30 s; failing sooner frees the
/// media endpoint while the caller is still waiting.
const DEFAULT_ICE_DEADLINE: Duration = Duration::from_secs(10);

/// Default deadline for an unpaired WebRTC leg (slice
/// 5.10-bridge). An operator-facing TOML knob lands with
/// `[webrtc] rendezvous_deadline_s` when a real use case asks
/// for a shorter value; the default matches the WebRTC
/// `iceGatheringTimeout` ballpark most browsers use.
pub(crate) const DEFAULT_RENDEZVOUS_DEADLINE: Duration = Duration::from_secs(30);

/// Parked-leg state stored in the rendezvous map until a
/// partner arrives or the deadline fires. Kept compact — the
/// heavy data (UDP socket, SRTP transform) lives in the
/// `UdpMediaFabric` under `endpoint`.
#[derive(Debug)]
struct PendingLeg {
    /// Handler session this leg belongs to. Used for teardown
    /// logs + possible future leg-to-WebSocket reverse lookup.
    session: WebTransportSessionId,
    /// Fabric endpoint allocated for this leg. The bridge
    /// installer pulls its socket from the fabric by this id.
    endpoint: EndpointId,
    /// Remote RTP address from the offer's `c=` / `m=` lines.
    peer: SocketAddr,
    /// Post-handshake SRTP material. `None` when the leg
    /// negotiated plain `RTP/AVP` or `RTP/SAVP` (SDES surfaces
    /// the keys directly on `NegotiationOutcome::Accepted`);
    /// `Some` only on the DTLS-SRTP path.
    srtp: Option<SrtpKeys>,
    /// Deadline evictor handle. Aborting this handle before
    /// the partner arrives prevents the evictor from running.
    evictor: tokio::task::JoinHandle<()>,
}

/// Concrete [`WebRtcSessionHandler`] used by the CLI. Owns the
/// shared SDP negotiator, the media fabric for endpoint
/// allocation + DTLS handshake + bridge install, and the
/// rendezvous map keyed by the session `tag`.
pub(crate) struct CliWebRtcHandler {
    negotiator: Arc<dyn SdpNegotiator>,
    local_ip: IpAddr,
    /// STUN servers queried for server-reflexive candidates on
    /// every accepted offer. Empty = host candidates only.
    stun_servers: Vec<SocketAddr>,
    /// Handle on the DTLS-SRTP identity — None = DTLS offers
    /// land on `UnsupportedTransport`; Some = every
    /// `UDP/TLS/RTP/SAVP[F]` offer gets a real answer.
    dtls_cert: Option<Arc<SelfSignedCert>>,
    /// Media fabric used to allocate endpoints, drive the DTLS
    /// handshake, and install the bridge on rendezvous.
    fabric: Arc<UdpMediaFabric>,
    /// Tag → parked leg. First leg parks; second leg pulls and
    /// installs the bridge.
    pending: Arc<DashMap<String, PendingLeg>>,
    /// Session-id → live bridge installed for that leg. Used
    /// on `bye` to release the bridge idempotently.
    active: Arc<DashMap<WebTransportSessionId, BridgeId>>,
    /// Shared metrics handle for the pairing counter.
    metrics: Option<Arc<Metrics>>,
    /// Rendezvous deadline (default
    /// [`DEFAULT_RENDEZVOUS_DEADLINE`]). Overridable via
    /// `with_rendezvous_deadline` for tests that need tighter
    /// eviction.
    rendezvous_deadline: Duration,
    /// Privacy posture. Reads of
    /// `[webrtc.privacy]`; the 5.8-b read-through adapter
    /// live-updates the inner `Mutex<WebRtcPrivacyConfig>` when
    /// the operator rotates the `redaction_key` or flips `mode`.
    privacy: Arc<std::sync::Mutex<WebRtcPrivacyConfig>>,
    /// Session-id → live ICE agent, so a trickled candidate that
    /// arrives mid-check reaches the agent still running it.
    ice_agents: Arc<DashMap<WebTransportSessionId, crate::ice_driver::SharedAgent>>,
    /// Session-id → cancel handle for that leg's keepalive task, so
    /// `bye` stops it instead of leaving it pinging a dead peer.
    ice_keepalives: Arc<DashMap<WebTransportSessionId, tokio_util::sync::CancellationToken>>,
    /// How long a leg may spend on connectivity checks before the
    /// offer is rejected and its endpoint released.
    ice_deadline: Duration,
    /// Whether `[webrtc.ice] enabled` selected the agent. When off,
    /// media goes to the address in the offer's `c=` / `m=` lines,
    /// which is the pre-ICE behaviour.
    ice_enabled: bool,
}

impl CliWebRtcHandler {
    /// Build a handler bound to the given negotiator, media fabric
    /// and local IP (the address the engine publishes in the answer's
    /// `c=` line). Media ports come from the fabric's allocator,
    /// which honors `[media.rtp_ports]`.
    #[must_use]
    pub(crate) fn new(
        negotiator: Arc<dyn SdpNegotiator>,
        fabric: Arc<UdpMediaFabric>,
        local_ip: IpAddr,
    ) -> Self {
        Self {
            negotiator,
            local_ip,
            stun_servers: Vec::new(),
            dtls_cert: None,
            fabric,
            pending: Arc::new(DashMap::new()),
            active: Arc::new(DashMap::new()),
            metrics: None,
            rendezvous_deadline: DEFAULT_RENDEZVOUS_DEADLINE,
            privacy: Arc::new(std::sync::Mutex::new(WebRtcPrivacyConfig::default())),
            ice_agents: Arc::new(DashMap::new()),
            ice_keepalives: Arc::new(DashMap::new()),
            ice_deadline: DEFAULT_ICE_DEADLINE,
            ice_enabled: false,
        }
    }

    /// STUN servers to query for server-reflexive candidates
    /// (`[webrtc.ice] stun_servers`). Setting them also turns the ICE
    /// agent on, since `[webrtc.ice] enabled` is what populates them.
    #[must_use]
    pub(crate) fn with_stun_servers(mut self, servers: Vec<SocketAddr>) -> Self {
        self.stun_servers = servers;
        self
    }

    /// Run connectivity checks before DTLS (`[webrtc.ice] enabled`).
    #[must_use]
    pub(crate) fn with_ice_enabled(mut self, enabled: bool) -> Self {
        self.ice_enabled = enabled;
        self
    }

    /// Override the connectivity-check deadline. Tests use a tight
    /// one so a peer that never answers does not hold the suite up.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_ice_deadline(mut self, deadline: Duration) -> Self {
        self.ice_deadline = deadline;
        self
    }

    /// Attach the `[webrtc.privacy]` config. Holds an
    /// `Arc<Mutex<_>>` internally so the CLI's config
    /// read-through adapter can swap `mode` / `redaction_key`
    /// live without rebuilding the handler.
    #[must_use]
    pub(crate) fn with_privacy(self, cfg: WebRtcPrivacyConfig) -> Self {
        {
            let mut guard = self
                .privacy
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = cfg;
        }
        self
    }

    /// Handle onto the mutable privacy config. Shared by the
    /// config read-through adapter in `main.rs`: flipping
    /// `mode` or rotating `redaction_key` mutates through
    /// this.
    pub(crate) fn privacy_handle(&self) -> Arc<std::sync::Mutex<WebRtcPrivacyConfig>> {
        Arc::clone(&self.privacy)
    }

    fn snapshot_privacy(&self) -> WebRtcPrivacyConfig {
        self.privacy
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn bump_reject(&self, reason: &'static str) {
        if let Some(m) = &self.metrics {
            m.webrtc_candidates_rejected
                .get_or_create(&PrivacyRejectReasonLabel {
                    reason: reason.into(),
                })
                .inc();
        }
    }

    fn bump_redaction(&self) {
        if let Some(m) = &self.metrics {
            m.webrtc_privacy_redactions.inc();
        }
    }

    /// Render a peer socket address for log emission, honoring
    /// the current privacy mode. In `Strict` the IP half goes
    /// through `redact_ip`; in other modes the address renders
    /// verbatim. The port is kept in both modes — operators
    /// triangulating NAT issues need it, and a port alone
    /// leaks nothing about the peer's identity.
    fn render_peer(&self, peer: SocketAddr) -> String {
        let snap = self.snapshot_privacy();
        if snap.mode == WebRtcPrivacyMode::Strict && !snap.redaction_key.is_empty() {
            self.bump_redaction();
            format!(
                "ip=<redacted:{}>:{}",
                redact_ip(peer.ip(), snap.redaction_key.as_bytes()),
                peer.port()
            )
        } else {
            peer.to_string()
        }
    }

    /// Attach the engine's DTLS-SRTP identity. When `Some`, the
    /// negotiator accepts `UDP/TLS/RTP/SAVP[F]` offers and the
    /// handler drives the handshake before parking / bridging.
    /// When `None`, those offers reject with a clear reason.
    #[must_use]
    pub(crate) fn with_dtls_cert(mut self, cert: Arc<SelfSignedCert>) -> Self {
        self.dtls_cert = Some(cert);
        self
    }

    /// Attach the metrics handle — the pairing counter bumps
    /// through it.
    #[must_use]
    pub(crate) fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Override the rendezvous deadline (default 30 s). Tests
    /// that want to observe eviction without waiting the
    /// production default dial this down to milliseconds.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn with_rendezvous_deadline(mut self, d: Duration) -> Self {
        self.rendezvous_deadline = d;
        self
    }

    /// Run ICE for one leg, if the negotiation asked for it.
    ///
    /// Returns the validated remote address, or `None` when ICE is
    /// off or the offer carried no ICE parameters — in which case the
    /// caller keeps the address from the offer's `c=` / `m=` lines.
    /// The agent is registered under `session` first so a trickled
    /// candidate arriving mid-check reaches it.
    async fn run_ice(
        &self,
        session: WebTransportSessionId,
        endpoint: EndpointId,
        ice: Option<&smiths_core::sdp::IceParams>,
        sdp_offer: &str,
    ) -> Result<Option<SocketAddr>, crate::ice_driver::IceError> {
        let (Some(params), true) = (ice, self.ice_enabled) else {
            return Ok(None);
        };
        let Some(socket) = self.fabric.endpoint_socket(endpoint) else {
            // The endpoint was allocated a few lines ago, so this can
            // only mean it was released underneath us.
            return Ok(None);
        };
        let base = socket.local_addr()?;

        // Local candidates: the media socket itself, plus any
        // server-reflexive address the answer already advertised, so
        // the agent checks exactly what the peer was told about.
        let mut local = smiths_ice::candidate::gather_host_candidates(&[base], 1);
        if !self.stun_servers.is_empty() {
            let observed = smiths_ice::stun::gather_srflx_candidates(
                &socket,
                &self.stun_servers,
                Duration::from_secs(1),
            )
            .await;
            for (idx, addr) in observed.into_iter().enumerate() {
                local.push(srflx_candidate(addr, base, idx));
            }
        }

        let remote: Vec<IceCandidate> = sdp_offer
            .lines()
            .filter_map(|l| l.strip_prefix("a=candidate:"))
            .filter_map(|rest| {
                smiths_sdp::parse::parse_candidate_line(&format!("candidate:{rest}")).ok()
            })
            .collect();
        debug!(
            ?session,
            local = local.len(),
            remote = remote.len(),
            "starting ICE connectivity checks"
        );
        let agent = crate::ice_driver::build_agent(
            params.clone(),
            &local,
            &remote,
            &socket,
            self.metrics.clone(),
        )?;
        self.ice_agents.insert(session, Arc::clone(&agent));

        let selected =
            crate::ice_driver::run_to_completion(&agent, &socket, self.ice_deadline).await?;
        // Consent freshness for the nominated pair. Send-only, so it
        // never competes with DTLS or the bridge for inbound packets.
        let keepalive_cancel = tokio_util::sync::CancellationToken::new();
        self.ice_keepalives
            .insert(session, keepalive_cancel.clone());
        crate::ice_driver::spawn_keepalives(agent, keepalive_cancel);
        Ok(Some(selected))
    }

    /// Gather server-reflexive candidates for the leg's socket and
    /// append them to the answer's first media section. STUN
    /// failures leave the answer as it was — host-only.
    async fn add_srflx_candidates(&self, endpoint: EndpointId, answer: &str) -> String {
        let Some(socket) = self.fabric.endpoint_socket(endpoint) else {
            return answer.to_owned();
        };
        let Ok(local) = socket.local_addr() else {
            return answer.to_owned();
        };
        let observed = smiths_ice::stun::gather_srflx_candidates(
            &socket,
            &self.stun_servers,
            Duration::from_secs(1),
        )
        .await;
        if observed.is_empty() {
            debug!(?endpoint, "no srflx candidate gathered");
            return answer.to_owned();
        }
        let Ok(mut sdp) = SessionDescription::parse(answer) else {
            return answer.to_owned();
        };
        let Some(m) = sdp.media.first_mut() else {
            return answer.to_owned();
        };
        for (idx, addr) in observed.into_iter().enumerate() {
            m.candidates.push(srflx_candidate(addr, local, idx));
            if let Some(metrics) = &self.metrics {
                metrics
                    .ice_candidates_gathered
                    .get_or_create(&IceCandidateTypeLabel { ty: "srflx".into() })
                    .inc();
            }
            debug!(?endpoint, %addr, "srflx candidate added to answer");
        }
        sdp.to_string()
    }

    fn bump_pair(&self, partner: &'static str) {
        if let Some(m) = &self.metrics {
            m.webrtc_sessions_paired
                .get_or_create(&WebRtcPartnerLabel {
                    partner: partner.into(),
                })
                .inc();
        }
    }

    /// Core of the tag-keyed rendezvous. Given the just-
    /// negotiated leg's endpoint / peer / srtp, either:
    ///
    /// - park under `tag` (no partner yet) + arm the deadline
    ///   evictor; or
    /// - pull the existing partner out of the map + call
    ///   `MediaFabric::bridge` + retain the `BridgeId` under
    ///   the session so `bye` can release it.
    ///
    /// Returns `Ok()` whether this leg paired or parked —
    /// the caller sends the answer either way. The only
    /// `Err` path is a fabric bridge install failure, which
    /// surfaces as `WebRtcHandlerError::Resource` so the
    /// client gets a diagnostic frame.
    async fn rendezvous(
        &self,
        session: WebTransportSessionId,
        tag: &str,
        endpoint: EndpointId,
        peer: SocketAddr,
        srtp: Option<SrtpKeys>,
    ) -> Result<(), WebRtcHandlerError> {
        // Remove atomically: if the partner's there, take
        // ownership (so a concurrent third leg with the same
        // tag can start parking behind us without racing).
        if let Some((_, partner)) = self.pending.remove(tag) {
            // Partner was parked — abort its deadline evictor,
            // install the bridge, retain the BridgeId.
            partner.evictor.abort();
            let leg_self = match &srtp {
                Some(keys) => BridgeLeg::with_srtp(endpoint, peer, keys.clone()),
                None => BridgeLeg::plain(endpoint, peer),
            };
            let leg_partner = match &partner.srtp {
                Some(keys) => BridgeLeg::with_srtp(partner.endpoint, partner.peer, keys.clone()),
                None => BridgeLeg::plain(partner.endpoint, partner.peer),
            };
            let bridge_id = self
                .fabric
                .bridge(leg_self, leg_partner)
                .await
                .map_err(|e| WebRtcHandlerError::Resource(format!("bridge install: {e}")))?;
            self.active.insert(session, bridge_id);
            self.active.insert(partner.session, bridge_id);
            self.bump_pair("webrtc");
            info!(
                ?session,
                partner = ?partner.session,
                %tag,
                ?bridge_id,
                "webrtc rendezvous paired; bridge installed"
            );
            Ok(())
        } else {
            // No partner yet — park + arm the deadline evictor.
            let evictor = self.spawn_evictor(tag.to_owned(), session);
            self.pending.insert(
                tag.to_owned(),
                PendingLeg {
                    session,
                    endpoint,
                    peer,
                    srtp,
                    evictor,
                },
            );
            debug!(
                ?session,
                %tag,
                deadline_secs = self.rendezvous_deadline.as_secs(),
                "webrtc rendezvous: leg parked awaiting partner"
            );
            Ok(())
        }
    }

    fn spawn_evictor(
        &self,
        tag: String,
        session: WebTransportSessionId,
    ) -> tokio::task::JoinHandle<()> {
        let pending = Arc::clone(&self.pending);
        let fabric = Arc::clone(&self.fabric);
        let metrics = self.metrics.clone();
        let deadline = self.rendezvous_deadline;
        tokio::spawn(async move {
            tokio::time::sleep(deadline).await;
            // Only evict if the entry is still there + still
            // belongs to the session that parked it. A partner
            // that won the rendezvous aborts this handle before
            // we get here.
            if let Some((_, parked)) = pending.remove_if(&tag, |_, p| p.session == session) {
                warn!(
                    ?session,
                    %tag,
                    deadline_secs = deadline.as_secs(),
                    "webrtc rendezvous leg evicted; no partner arrived before deadline"
                );
                fabric.release_endpoint(parked.endpoint).await;
                if let Some(m) = metrics {
                    m.webrtc_sessions_paired
                        .get_or_create(&WebRtcPartnerLabel {
                            partner: "none".into(),
                        })
                        .inc();
                }
            }
        })
    }
}

#[async_trait]
impl smiths_core::WebRtcRendezvous for CliWebRtcHandler {
    async fn pair_sip_leg(
        &self,
        tag: &str,
        endpoint: EndpointId,
        peer: SocketAddr,
        srtp: Option<SrtpKeys>,
    ) -> Result<Option<BridgeId>, String> {
        // If a WebRTC leg is already parked under `tag`,
        // install the bridge now.
        if let Some((_, partner)) = self.pending.remove(tag) {
            partner.evictor.abort();
            let leg_sip = match &srtp {
                Some(k) => BridgeLeg::with_srtp(endpoint, peer, k.clone()),
                None => BridgeLeg::plain(endpoint, peer),
            };
            let leg_webrtc = match &partner.srtp {
                Some(k) => BridgeLeg::with_srtp(partner.endpoint, partner.peer, k.clone()),
                None => BridgeLeg::plain(partner.endpoint, partner.peer),
            };
            let bid = self
                .fabric
                .bridge(leg_sip, leg_webrtc)
                .await
                .map_err(|e| format!("bridge install: {e}"))?;
            // The WebRTC partner's session maps to this
            // bridge id for teardown-on-bye; the SIP side
            // stores the id in its own dialog record.
            self.active.insert(partner.session, bid);
            self.bump_pair("sip");
            info!(
                partner = ?partner.session,
                %tag,
                ?bid,
                "sip→webrtc rendezvous paired; bridge installed"
            );
            Ok(Some(bid))
        } else {
            // No WebRTC partner yet — park this SIP leg in
            // the same map the WebRTC handler consults when
            // its own leg arrives.
            let synthetic_session =
                WebTransportSessionId(u64::from(u32::MAX ^ next_sip_pending_suffix()));
            let evictor = self.spawn_evictor(tag.to_owned(), synthetic_session);
            self.pending.insert(
                tag.to_owned(),
                PendingLeg {
                    session: synthetic_session,
                    endpoint,
                    peer,
                    srtp,
                    evictor,
                },
            );
            debug!(
                %tag,
                ?synthetic_session,
                "sip leg parked awaiting webrtc partner"
            );
            Ok(None)
        }
    }

    async fn release_sip_leg(&self, tag: &str) {
        if let Some((_, parked)) = self.pending.remove(tag) {
            parked.evictor.abort();
            self.fabric.release_endpoint(parked.endpoint).await;
            debug!(%tag, "sip-parked leg released");
        }
    }
}

/// Give SIP-side parked legs a synthetic `WebTransportSessionId`
/// (the top bit of `u32::MAX` is the marker + a monotonically
/// increasing suffix) so they can't collide with real WebRTC
/// session IDs minted by the listener. Per-process counter is
/// fine — we just need uniqueness, not cryptographic
/// unguessability.
fn next_sip_pending_suffix() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[async_trait]
impl WebRtcSessionHandler for CliWebRtcHandler {
    async fn handle_offer(
        &self,
        session: WebTransportSessionId,
        sdp_offer: &str,
    ) -> Result<String, WebRtcHandlerError> {
        // Plain path with no tag — single-leg answer, no
        // rendezvous. Used by clients that omit `tag` on
        // session-init (echo / health-probe flows).
        self.handle_offer_tagged(session, None, sdp_offer).await
    }

    // Single-sitting offer dispatch: parse → privacy filter →
    // allocate → negotiate → optional DTLS handshake → rendezvous.
    // Splitting into per-phase helpers sacrificed readability for
    // a pedantic line-count target; the state threads through too
    // many locals to cleanly factor.
    #[allow(clippy::too_many_lines)]
    async fn handle_offer_tagged(
        &self,
        session: WebTransportSessionId,
        tag: Option<&str>,
        sdp_offer: &str,
    ) -> Result<String, WebRtcHandlerError> {
        debug!(?session, ?tag, "webrtc: negotiating offer");

        // : pre-negotiation candidate filter.
        // In `relay_only` / `strict` mode, reject offers that
        // advertise `host` / `srflx` candidates — the engine
        // is contractually "relay only" and accepting such a
        // candidate would defeat the whole point. Parse the
        // offer cheaply (the negotiator parses it again on
        // `negotiate_audio`; one extra parse per offer is
        // below the noise floor on a SIP-scale engine).
        let privacy = self.snapshot_privacy();
        if privacy.mode != WebRtcPrivacyMode::Open
            && let Ok(parsed_offer) = SessionDescription::parse(sdp_offer)
            && reject_direct_candidates(&parsed_offer) == OfferPrivacyVerdict::Rejected
        {
            for m in &parsed_offer.media {
                for c in &m.candidates {
                    if c.candidate_type == "host" {
                        self.bump_reject("host");
                    } else if c.candidate_type == "srflx" {
                        self.bump_reject("srflx");
                    }
                }
            }
            warn!(
                ?session,
                mode = ?privacy.mode,
                "privacy: rejecting offer with host/srflx candidates"
            );
            return Err(WebRtcHandlerError::OfferRejected(
                "privacy policy forbids host/srflx candidates — \
                 set iceTransportPolicy=\"relay\" on the client"
                    .into(),
            ));
        }

        // Allocate a fabric endpoint before negotiating so the
        // answer we build publishes its port.
        let endpoint = self
            .fabric
            .allocate(self.local_ip)
            .await
            .map_err(|e| WebRtcHandlerError::Resource(format!("endpoint allocate: {e}")))?;
        let endpoint_id = endpoint.id();
        let port = endpoint.local_addr().port();

        let outcome = self
            .negotiator
            .negotiate_audio(sdp_offer, self.local_ip, port);
        match outcome {
            NegotiationOutcome::Accepted {
                answer_body,
                remote_media,
                srtp: sdes_keys,
                dtls,
                ice,
                ..
            } => {
                let Some(peer) = remote_media else {
                    self.fabric.release_endpoint(endpoint_id).await;
                    return Err(WebRtcHandlerError::OfferRejected(
                        "offer had no usable audio endpoint (port 0 / no c= line)".into(),
                    ));
                };
                // Connectivity checks run before DTLS and decide where
                // media actually goes. Without ICE the peer stays
                // whatever the offer's `c=` / `m=` lines claimed.
                let peer = match self
                    .run_ice(session, endpoint_id, ice.as_ref(), sdp_offer)
                    .await
                {
                    Ok(Some(selected)) => selected,
                    Ok(None) => peer,
                    Err(e) => {
                        warn!(?session, %e, "ICE connectivity checks failed");
                        self.ice_agents.remove(&session);
                        self.fabric.release_endpoint(endpoint_id).await;
                        return Err(WebRtcHandlerError::OfferRejected(format!(
                            "ICE connectivity checks failed: {e}"
                        )));
                    }
                };
                // If the offer used DTLS-SRTP, run the
                // handshake right now against the allocated
                // socket. The dtls params carry the role +
                // peer fingerprint; the engine cert comes from
                // `self.dtls_cert` (guaranteed `Some` on this
                // path since `NegotiationOutcome::Accepted` with
                // `Some(dtls)` only happens when the negotiator
                // had a cert).
                let srtp_keys = if let Some(params) = dtls {
                    let Some(cert) = self.dtls_cert.as_ref() else {
                        self.fabric.release_endpoint(endpoint_id).await;
                        return Err(WebRtcHandlerError::Resource(
                            "DTLS-SRTP accepted but handler has no cert — engine init bug".into(),
                        ));
                    };
                    let role = match params.local_role {
                        smiths_core::DtlsRole::Client => DtlsLegRole::Client,
                        smiths_core::DtlsRole::Server => DtlsLegRole::Server,
                    };
                    let leg_cfg = DtlsLegConfig {
                        local_cert: (**cert).clone(),
                        role,
                        peer_fingerprint: smiths_sdp::Fingerprint {
                            algorithm: params.peer_fingerprint_algorithm.clone(),
                            value: params.peer_fingerprint_value.clone(),
                        },
                    };
                    match self
                        .fabric
                        .run_dtls_handshake(endpoint_id, peer, leg_cfg)
                        .await
                    {
                        Ok(r) => Some(r.srtp),
                        Err(e) => {
                            // Log the offer's `o=` line so the
                            // operator can correlate with the
                            // peer's SDP in logs (
                            // Small 2 requirement).
                            let origin = sdp_offer
                                .lines()
                                .find(|l| l.starts_with("o="))
                                .unwrap_or("o=<missing>");
                            warn!(?session, %origin, ?e, "DTLS-SRTP handshake failed");
                            self.fabric.release_endpoint(endpoint_id).await;
                            return Err(WebRtcHandlerError::OfferRejected(format!(
                                "DTLS-SRTP handshake failed: {e}"
                            )));
                        }
                    }
                } else {
                    sdes_keys
                };

                // If the session carries a tag, drive the
                // rendezvous; otherwise we've allocated an
                // endpoint that's inert until a SIP INVITE or
                // follow-on slice installs the bridge. Keep it
                // for the session lifetime; `handle_bye` cleans
                // up.
                if let Some(tag) = tag {
                    self.rendezvous(session, tag, endpoint_id, peer, srtp_keys)
                        .await?;
                }
                // : strip `host` candidates
                // from the answer in `relay_only` / `strict`.
                // Today the negotiator doesn't emit candidates
                // in answers (ICE emission lands with
                // 5.10-ice); this call is a no-op in that
                // steady state and becomes meaningful the
                // moment the negotiator starts advertising host
                // candidates.
                let final_answer = if matches!(
                    privacy.mode,
                    WebRtcPrivacyMode::RelayOnly | WebRtcPrivacyMode::Strict
                ) {
                    strip_answer_host_candidates(&answer_body)
                } else {
                    answer_body
                };
                // Server-reflexive candidates only make sense in
                // `open` mode: relay-only deployments would strip
                // them again on the client side.
                let final_answer =
                    if self.stun_servers.is_empty() || privacy.mode != WebRtcPrivacyMode::Open {
                        final_answer
                    } else {
                        self.add_srflx_candidates(endpoint_id, &final_answer).await
                    };
                let peer_rendered = self.render_peer(peer);
                debug!(
                    ?session,
                    port,
                    answer_len = final_answer.len(),
                    peer = %peer_rendered,
                    "webrtc: offer accepted"
                );
                Ok(final_answer)
            }
            NegotiationOutcome::Mismatch => {
                self.fabric.release_endpoint(endpoint_id).await;
                Err(WebRtcHandlerError::OfferRejected("no common codec".into()))
            }
            NegotiationOutcome::UnsupportedTransport { reason } => {
                self.fabric.release_endpoint(endpoint_id).await;
                Err(WebRtcHandlerError::OfferRejected(reason))
            }
            NegotiationOutcome::Malformed(e) => {
                self.fabric.release_endpoint(endpoint_id).await;
                Err(WebRtcHandlerError::OfferRejected(format!(
                    "malformed SDP: {e}"
                )))
            }
        }
    }

    async fn handle_bye(&self, session: WebTransportSessionId) {
        let peer_note = self
            .active
            .get(&session)
            .map(|e| format!(" bridge={:?}", *e.value()))
            .unwrap_or_default();
        debug!(?session, note = %peer_note, "webrtc: session bye");
        // Stop consent keepalives and forget the agent: nothing
        // should keep pinging a peer that has hung up.
        if let Some((_, cancel)) = self.ice_keepalives.remove(&session) {
            cancel.cancel();
        }
        self.ice_agents.remove(&session);
        // Release a live bridge if one is installed for this
        // session. The fabric's `release_bridge` is idempotent
        // — a concurrent bye from the partner side races harmlessly.
        if let Some((_, bridge_id)) = self.active.remove(&session) {
            self.fabric.release_bridge(bridge_id).await;
            debug!(?session, ?bridge_id, "webrtc: bridge released");
        }
        // If this session still had a leg parked (partner
        // never arrived, bye came before the deadline), pull
        // it out + abort its evictor so we don't race the
        // deadline cleanup.
        let maybe_tag = self
            .pending
            .iter()
            .find(|e| e.value().session == session)
            .map(|e| e.key().clone());
        if let Some(tag) = maybe_tag
            && let Some((_, parked)) = self.pending.remove(&tag)
        {
            parked.evictor.abort();
            self.fabric.release_endpoint(parked.endpoint).await;
            debug!(?session, %tag, "webrtc: parked leg released on bye");
        }
    }

    async fn handle_ice_candidate(
        &self,
        session: WebTransportSessionId,
        candidate: &str,
        sdp_m_line_index: u16,
    ) -> Result<(), WebRtcHandlerError> {
        use smiths_sdp::parse::parse_candidate_line;
        match parse_candidate_line(candidate) {
            Ok(c) => {
                debug!(
                    ?session,
                    sdp_m_line_index,
                    ty = %c.candidate_type,
                    addr = %c.address,
                    port = c.port,
                    "webrtc: trickle ICE candidate"
                );
                // Hand it to the agent still running checks for this
                // leg. A candidate that arrives after ICE finished
                // has no agent to reach, which is expected rather
                // than an error.
                if let Some(agent) = self.ice_agents.get(&session).map(|e| Arc::clone(e.value())) {
                    crate::ice_driver::add_remote_candidate(&agent, &c).await;
                } else {
                    debug!(
                        ?session,
                        "no live ICE agent for this session; candidate noted only"
                    );
                }
            }
            Err(e) => {
                warn!(?session, error = %e, raw = %candidate, "webrtc: malformed ICE candidate");
            }
        }
        Ok(())
    }
}

/// RFC 8445 §5.1.2 priority for a server-reflexive candidate
/// (type preference 100) on component 1.
fn srflx_candidate(observed: SocketAddr, base: SocketAddr, idx: usize) -> IceCandidate {
    let local_pref: u32 = if observed.is_ipv6() { 65_535 } else { 65_534 };
    IceCandidate {
        foundation: format!("srflx{idx}"),
        component: 1,
        transport: "UDP".into(),
        priority: (100u32 << 24) + (local_pref << 8) + 255,
        address: observed.ip(),
        port: observed.port(),
        candidate_type: "srflx".into(),
        related_address: Some(base.ip()),
        related_port: Some(base.port()),
        raw_params: Vec::new(),
    }
}

/// Build the signaling handler from `[webrtc]` without starting
/// its servers. The handler doubles as the SIP UAS's
/// [`smiths_core::WebRtcRendezvous`] so SIP INVITEs carrying
/// `X-Smiths-Webrtc-Tag:` can bridge against a parked WebRTC
/// partner. It shares the engine's media fabric, so WebRTC and SIP
/// endpoints draw from one `[media.rtp_ports]` allocator and a
/// bridge can span both.
pub(crate) fn build_handler(
    config: &Config,
    metrics: &Arc<Metrics>,
    fabric: Arc<UdpMediaFabric>,
) -> Arc<CliWebRtcHandler> {
    let bind = config.webrtc.ws_bind;
    let dtls_cert = match SelfSignedCert::generate("smiths-net-webrtc") {
        Ok(c) => Some(Arc::new(c)),
        Err(e) => {
            warn!(
                ?e,
                "minting WebRTC DTLS cert failed; DTLS-SRTP offers will be rejected"
            );
            None
        }
    };
    let mut negotiator_builder = smiths_sdp::Negotiator::with_default_codecs(bind.ip());
    if let Some(cert) = dtls_cert.as_ref() {
        negotiator_builder = negotiator_builder.with_dtls_cert(Arc::clone(cert));
    }
    negotiator_builder = negotiator_builder.with_ice_enabled(config.webrtc.ice.enabled);
    let negotiator: Arc<dyn SdpNegotiator> = Arc::new(negotiator_builder);
    let mut handler = CliWebRtcHandler::new(negotiator, fabric, bind.ip())
        .with_metrics(Arc::clone(metrics))
        .with_privacy(config.webrtc.privacy.clone());
    handler = handler.with_ice_enabled(config.webrtc.ice.enabled);
    if config.webrtc.ice.enabled {
        handler = handler.with_stun_servers(config.webrtc.ice.stun_servers.clone());
    }
    if let Some(cert) = dtls_cert {
        handler = handler.with_dtls_cert(cert);
    }
    Arc::new(handler)
}

/// Start the WebSocket signaling server and, when `[webrtc.turn]`
/// enables it without an `external_url`, the embedded TURN server.
pub(crate) fn spawn_servers(
    config: &Config,
    metrics: &Arc<Metrics>,
    handler: Arc<CliWebRtcHandler>,
    shutdown: &Shutdown,
) -> Vec<JoinHandle<()>> {
    let mut tasks = Vec::new();
    let bind = config.webrtc.ws_bind;
    let cancel = shutdown.token();
    tasks.push(tokio::spawn(async move {
        if let Err(e) = serve_webrtc(bind, handler, cancel).await {
            warn!(%bind, ?e, "WebRTC WebSocket server error");
        }
    }));

    let turn = &config.webrtc.turn;
    if turn.enabled && !turn.external_url.is_empty() {
        info!(url = %turn.external_url, "webrtc.turn.external_url set; embedded TURN server skipped");
        return tasks;
    }
    if !turn.enabled {
        return tasks;
    }
    let realm = if turn.realm.is_empty() {
        "smiths-turn"
    } else {
        turn.realm.as_str()
    };
    let turn_cfg = smiths_ice::TurnServerConfig {
        bind: turn.bind,
        realm: realm.to_owned(),
        relay_ip: turn.relay_ip.unwrap_or_else(|| turn.bind.ip()),
        allocation_lifetime: Duration::from_secs(u64::from(turn.allocation_lifetime_s)),
        credentials: turn
            .credentials
            .iter()
            .map(|c| smiths_ice::LongTermCredential::new(&c.username, realm, &c.password))
            .collect(),
    };
    if turn_cfg.credentials.is_empty() {
        warn!(
            bind = %turn_cfg.bind,
            "webrtc.turn.enabled but credentials list is empty; every Allocate will 401. \
             Add `[[webrtc.turn.credentials]]` entries or disable the server."
        );
    }
    let server = Arc::new(smiths_ice::TurnServer::new(turn_cfg).with_metrics(Arc::clone(metrics)));
    let cancel = shutdown.token();
    tasks.push(tokio::spawn(async move {
        if let Err(e) = server.run(cancel).await {
            warn!(?e, "TURN server exited with error");
        }
    }));
    tasks
}

/// Axum app state: the WebSocket route handler needs the
/// shared [`WebSocketSignalingListener`] to parse frames.
#[derive(Clone)]
struct WsState {
    listener: Arc<WebSocketSignalingListener>,
}

/// Serve the WebRTC signaling WebSocket on `bind` until
/// `cancel` fires. Returns when the axum server exits — either
/// because the cancellation token tripped or a bind error
/// surfaced.
///
/// # Errors
/// Returns an error when the TCP listener can't bind or the
/// axum server encounters a fatal I/O error.
pub(crate) async fn serve_webrtc(
    bind: SocketAddr,
    handler: Arc<CliWebRtcHandler>,
    cancel: CancellationToken,
) -> Result<()> {
    let listener_sip = Arc::new(WebSocketSignalingListener::new(handler));
    let state = WsState {
        listener: Arc::clone(&listener_sip),
    };
    let app: Router = Router::new()
        .route("/smiths/webrtc", get(ws_upgrade))
        .with_state(state);
    let tcp = TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding WebRTC WebSocket on {bind}"))?;
    info!(%bind, "WebRTC WebSocket adapter ready at /smiths/webrtc");
    axum::serve(tcp, app)
        .with_graceful_shutdown(async move { cancel.cancelled().await })
        .await
        .context("WebRTC WebSocket server exited with error")?;
    Ok(())
}

async fn ws_upgrade(State(state): State<WsState>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

/// Drive a single WebSocket connection through the listener's
/// `handle_frame`. The listener already knows the `WtSignal` JSON
/// shape; this wrapper just converts between axum's `Message`
/// and raw bytes, and closes the socket on terminal frames.
/// Reparse `answer_body`, drop every `host`-typed
/// `a=candidate:` line, re-emit. Wrapper around
/// `smiths_sdp::privacy::strip_host_candidates` that works on
/// the string shape the handler returns — round-trip through
/// `SessionDescription::parse` + `Display` preserves every
/// other attribute. When the answer can't be re-parsed (never
/// happens with an answer we just emitted), returns the
/// original verbatim.
fn strip_answer_host_candidates(answer_body: &str) -> String {
    match SessionDescription::parse(answer_body) {
        Ok(mut sdp) => {
            smiths_sdp::privacy::strip_host_candidates(&mut sdp);
            sdp.to_string()
        }
        Err(_) => answer_body.to_owned(),
    }
}

async fn handle_socket(mut socket: WebSocket, state: WsState) {
    let mut session: Option<WebRtcSession> = None;
    loop {
        let msg = match socket.recv().await {
            Some(Ok(m)) => m,
            Some(Err(e)) => {
                debug!(error = %e, "webrtc ws: recv error");
                break;
            }
            None => break,
        };
        let payload: Vec<u8> = match msg {
            Message::Text(t) => t.as_bytes().to_vec(),
            Message::Binary(b) => b.to_vec(),
            Message::Ping(p) => {
                if let Err(e) = socket.send(Message::Pong(p)).await {
                    debug!(error = %e, "webrtc ws: pong send failed");
                    break;
                }
                continue;
            }
            Message::Pong(_) => continue,
            Message::Close(_) => break,
        };
        let reply = state.listener.handle_frame(&mut session, &payload).await;
        let Some(frame) = reply else {
            // Non-terminal silent ack (ice-candidate / ice-end) — keep
            // reading. Terminal `bye` also returns None, but the client
            // is expected to close its side; if it doesn't, we'll see
            // either more frames or EOF on the next recv.
            continue;
        };
        let encoded = match frame.encode() {
            Ok(b) => b,
            Err(e) => {
                warn!(error = %e, "webrtc ws: encode reply failed");
                break;
            }
        };
        if let Err(e) = socket.send(Message::Binary(encoded.into())).await {
            debug!(error = %e, "webrtc ws: send failed; closing");
            break;
        }
    }
    debug!("webrtc ws: session closed");
}

#[cfg(test)]
mod tests {
    use super::*;
    use smiths_sdp::Negotiator;
    use std::net::Ipv4Addr;

    fn fabric() -> Arc<UdpMediaFabric> {
        Arc::new(UdpMediaFabric::new())
    }

    fn handler() -> CliWebRtcHandler {
        let neg: Arc<dyn SdpNegotiator> = Arc::new(Negotiator::with_default_codecs(IpAddr::V4(
            Ipv4Addr::LOCALHOST,
        )));
        CliWebRtcHandler::new(neg, fabric(), IpAddr::V4(Ipv4Addr::LOCALHOST))
    }

    /// A handler whose negotiator also emits ICE parameters, which is
    /// what `build_handler` does when `[webrtc.ice] enabled` is set.
    fn ice_handler() -> CliWebRtcHandler {
        // ICE rides on DTLS-SRTP offers, which is the only shape a
        // browser sends, so the negotiator needs a cert too.
        let cert = Arc::new(SelfSignedCert::generate("test-webrtc").expect("cert"));
        let neg: Arc<dyn SdpNegotiator> = Arc::new(
            Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST))
                .with_ice_enabled(true)
                .with_dtls_cert(Arc::clone(&cert)),
        );
        CliWebRtcHandler::new(neg, fabric(), IpAddr::V4(Ipv4Addr::LOCALHOST))
            .with_ice_enabled(true)
            .with_dtls_cert(cert)
    }

    /// An SDP offer that asks for ICE against `peer`, with a plain
    /// RTP profile so the test exercises ICE without needing DTLS.
    fn ice_offer(peer: SocketAddr, ufrag: &str, pwd: &str) -> String {
        format!(
            "v=0\r\n\
             o=- 1 1 IN IP4 {ip}\r\n\
             s=-\r\n\
             c=IN IP4 {ip}\r\n\
             t=0 0\r\n\
             m=audio {port} UDP/TLS/RTP/SAVP 0\r\n\
             a=rtpmap:0 PCMU/8000\r\n\
             a=setup:actpass\r\n\
             a=fingerprint:sha-256 AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99\r\n\
             a=ice-ufrag:{ufrag}\r\n\
             a=ice-pwd:{pwd}\r\n\
             a=candidate:1 1 UDP 2130706431 {ip} {port} typ host\r\n",
            ip = peer.ip(),
            port = peer.port(),
        )
    }

    /// With ICE enabled but a peer that never answers a connectivity
    /// check, the offer is rejected rather than bridged to an address
    /// nobody validated — and the media endpoint is released.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ice_enabled_rejects_a_peer_that_never_answers_checks() {
        let dead = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let peer = dead.local_addr().expect("addr");
        // Bound but never read: checks go out, nothing comes back.
        let h = ice_handler().with_ice_deadline(Duration::from_millis(400));
        let err = h
            .handle_offer_tagged(
                WebTransportSessionId(7),
                None,
                &ice_offer(peer, "remoteUfrag", "remotePasswordxxxx"),
            )
            .await
            .expect_err("an unvalidated peer must not be accepted");
        let msg = err.to_string();
        assert!(msg.contains("ICE"), "{msg}");
        assert!(
            h.ice_agents.is_empty(),
            "a failed leg must not leave its agent registered"
        );
    }

    /// The same offer with ICE off keeps the pre-ICE behaviour: the
    /// address from the offer is used and the leg is accepted.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ice_disabled_uses_the_offer_address_unchanged() {
        let dead = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let peer = dead.local_addr().expect("addr");
        // Same ICE attributes, plain profile: with the agent off the
        // engine answers immediately instead of checking anything.
        let offer = format!(
            "v=0\r\n\
             o=- 1 1 IN IP4 {ip}\r\n\
             s=-\r\n\
             c=IN IP4 {ip}\r\n\
             t=0 0\r\n\
             m=audio {port} RTP/AVP 0\r\n\
             a=rtpmap:0 PCMU/8000\r\n\
             a=ice-ufrag:remoteUfrag\r\n\
             a=ice-pwd:remotePasswordxxxx\r\n",
            ip = peer.ip(),
            port = peer.port(),
        );
        let h = handler();
        let answer = h
            .handle_offer_tagged(WebTransportSessionId(8), None, &offer)
            .await
            .expect("ICE off must not gate the offer");
        assert!(answer.contains("m=audio"), "{answer}");
        assert!(h.ice_agents.is_empty(), "no agent when ICE is disabled");
    }

    #[tokio::test]
    async fn dtls_srtp_offer_rejects_without_cert() {
        // Without a configured cert the negotiator surfaces
        // `UnsupportedTransport`; the handler forwards the
        // reason verbatim so the browser sees a diagnostic
        // rather than a silent drop.
        let h = handler();
        let offer = "v=0\r\n\
                     o=- 1 1 IN IP4 192.0.2.1\r\n\
                     s=-\r\n\
                     c=IN IP4 192.0.2.1\r\n\
                     t=0 0\r\n\
                     m=audio 49170 UDP/TLS/RTP/SAVP 0\r\n\
                     a=rtpmap:0 PCMU/8000\r\n\
                     a=setup:actpass\r\n\
                     a=fingerprint:sha-256 AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99\r\n";
        let err = h
            .handle_offer(WebTransportSessionId(1), offer)
            .await
            .unwrap_err();
        match err {
            WebRtcHandlerError::OfferRejected(reason) => {
                assert!(
                    reason.to_ascii_lowercase().contains("cert"),
                    "reason should mention the missing cert, got: {reason}"
                );
            }
            other => panic!("expected OfferRejected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn plain_rtp_avp_offer_is_accepted() {
        // The SIP-style `RTP/AVP` offer goes through fine — the
        // handler isn't WebRTC-exclusive, it reuses the same
        // negotiator smiths-sip uses for UDP SIP. Useful for
        // SIP-over-WebSocket callers that ride this adapter
        // without DTLS.
        let h = handler();
        let offer = "v=0\r\n\
                     o=- 1 1 IN IP4 192.0.2.1\r\n\
                     s=-\r\n\
                     c=IN IP4 192.0.2.1\r\n\
                     t=0 0\r\n\
                     m=audio 49170 RTP/AVP 0\r\n\
                     a=rtpmap:0 PCMU/8000\r\n";
        let answer = h
            .handle_offer(WebTransportSessionId(2), offer)
            .await
            .expect("RTP/AVP offer must negotiate");
        assert!(answer.contains("m=audio"));
        assert!(answer.contains("PCMU"));
    }

    #[tokio::test]
    async fn malformed_offer_surfaces_as_offer_rejected() {
        let h = handler();
        let err = h
            .handle_offer(WebTransportSessionId(3), "not an sdp body")
            .await
            .unwrap_err();
        assert!(matches!(err, WebRtcHandlerError::OfferRejected(_)));
    }

    /// Minimal STUN server: answers every Binding request with the
    /// sender's own address in `XOR-MAPPED-ADDRESS`.
    async fn spawn_stun_responder() -> SocketAddr {
        use smiths_ice::StunMessage;
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 1500];
            while let Ok((n, from)) = sock.recv_from(&mut buf).await {
                if let Ok(req) = StunMessage::decode(&buf[..n])
                    && let Ok(bytes) = StunMessage::new_binding_response(&req, from).encode()
                {
                    let _ = sock.send_to(&bytes, from).await;
                }
            }
        });
        addr
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stun_servers_add_srflx_candidates_to_the_answer() {
        let stun = spawn_stun_responder().await;
        let neg: Arc<dyn SdpNegotiator> = Arc::new(
            Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST)).with_ice_enabled(true),
        );
        let h = CliWebRtcHandler::new(neg, fabric(), IpAddr::V4(Ipv4Addr::LOCALHOST))
            .with_stun_servers(vec![stun]);
        let offer = "v=0\r\n\
                     o=- 1 1 IN IP4 127.0.0.1\r\n\
                     s=-\r\n\
                     c=IN IP4 127.0.0.1\r\n\
                     t=0 0\r\n\
                     m=audio 49170 RTP/AVP 0\r\n\
                     a=rtpmap:0 PCMU/8000\r\n\
                     a=ice-ufrag:abcd\r\n\
                     a=ice-pwd:0123456789abcdef0123456789\r\n";
        let answer = h
            .handle_offer(WebTransportSessionId(7), offer)
            .await
            .expect("offer must negotiate");
        let srflx: Vec<&str> = answer
            .lines()
            .filter(|l| l.starts_with("a=candidate:") && l.contains(" typ srflx"))
            .collect();
        assert_eq!(srflx.len(), 1, "answer:\n{answer}");
        assert!(srflx[0].contains("raddr 127.0.0.1"), "{}", srflx[0]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unreachable_stun_server_keeps_host_only_answer() {
        // Nothing listens here; the 1 s gather times out and the
        // answer is returned unchanged.
        let dead: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let neg: Arc<dyn SdpNegotiator> = Arc::new(
            Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST)).with_ice_enabled(true),
        );
        let h = CliWebRtcHandler::new(neg, fabric(), IpAddr::V4(Ipv4Addr::LOCALHOST))
            .with_stun_servers(vec![dead]);
        let offer = "v=0\r\n\
                     o=- 1 1 IN IP4 127.0.0.1\r\n\
                     s=-\r\n\
                     c=IN IP4 127.0.0.1\r\n\
                     t=0 0\r\n\
                     m=audio 49170 RTP/AVP 0\r\n\
                     a=rtpmap:0 PCMU/8000\r\n";
        let answer = h
            .handle_offer(WebTransportSessionId(8), offer)
            .await
            .expect("offer must negotiate");
        assert!(!answer.contains("typ srflx"), "{answer}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_webrtc_legs_with_same_tag_pair_and_bridge() {
        //  Small 1 — two WebRTC legs dialling
        // tag "X" end up sharing a bridge. `RTP/AVP` path so
        // the test doesn't need a DTLS handshake between two
        // in-process fabrics; the pairing logic under test is
        // transport-agnostic.
        let mut scratch = prometheus_client::registry::Registry::default();
        let metrics = Metrics::register(&mut scratch);
        let neg: Arc<dyn SdpNegotiator> = Arc::new(Negotiator::with_default_codecs(IpAddr::V4(
            Ipv4Addr::LOCALHOST,
        )));
        let h = CliWebRtcHandler::new(neg, fabric(), IpAddr::V4(Ipv4Addr::LOCALHOST))
            .with_metrics(Arc::clone(&metrics));

        let offer_a = "v=0\r\n\
                       o=- 1 1 IN IP4 127.0.0.1\r\n\
                       s=-\r\n\
                       c=IN IP4 127.0.0.1\r\n\
                       t=0 0\r\n\
                       m=audio 49170 RTP/AVP 0\r\n\
                       a=rtpmap:0 PCMU/8000\r\n";
        let offer_b = "v=0\r\n\
                       o=- 2 2 IN IP4 127.0.0.1\r\n\
                       s=-\r\n\
                       c=IN IP4 127.0.0.1\r\n\
                       t=0 0\r\n\
                       m=audio 49172 RTP/AVP 0\r\n\
                       a=rtpmap:0 PCMU/8000\r\n";

        // First leg parks under tag "X".
        let ans_a = h
            .handle_offer_tagged(WebTransportSessionId(10), Some("X"), offer_a)
            .await
            .expect("first leg answer");
        assert!(ans_a.contains("m=audio"));
        assert_eq!(h.pending.len(), 1);
        assert_eq!(h.active.len(), 0);

        // Second leg with the same tag pulls + installs the bridge.
        let ans_b = h
            .handle_offer_tagged(WebTransportSessionId(11), Some("X"), offer_b)
            .await
            .expect("second leg answer");
        assert!(ans_b.contains("m=audio"));
        assert_eq!(h.pending.len(), 0, "partner removed from pending map");
        // Both sessions point at the same bridge id.
        let bridge_a = *h.active.get(&WebTransportSessionId(10)).unwrap().value();
        let bridge_b = *h.active.get(&WebTransportSessionId(11)).unwrap().value();
        assert_eq!(bridge_a, bridge_b);

        // Metric credited to `partner="webrtc"`.
        let paired = metrics
            .webrtc_sessions_paired
            .get_or_create(&WebRtcPartnerLabel {
                partner: "webrtc".into(),
            })
            .get();
        assert_eq!(paired, 1);

        // Bye from one side releases the bridge.
        h.handle_bye(WebTransportSessionId(10)).await;
        assert_eq!(h.active.len(), 1, "only the sending side was released");
        h.handle_bye(WebTransportSessionId(11)).await;
        assert_eq!(h.active.len(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unpaired_leg_is_evicted_after_deadline() {
        let mut scratch = prometheus_client::registry::Registry::default();
        let metrics = Metrics::register(&mut scratch);
        let neg: Arc<dyn SdpNegotiator> = Arc::new(Negotiator::with_default_codecs(IpAddr::V4(
            Ipv4Addr::LOCALHOST,
        )));
        let h = CliWebRtcHandler::new(neg, fabric(), IpAddr::V4(Ipv4Addr::LOCALHOST))
            .with_metrics(Arc::clone(&metrics))
            .with_rendezvous_deadline(Duration::from_millis(80));

        let offer = "v=0\r\n\
                     o=- 1 1 IN IP4 127.0.0.1\r\n\
                     s=-\r\n\
                     c=IN IP4 127.0.0.1\r\n\
                     t=0 0\r\n\
                     m=audio 49170 RTP/AVP 0\r\n\
                     a=rtpmap:0 PCMU/8000\r\n";
        h.handle_offer_tagged(WebTransportSessionId(99), Some("orphan"), offer)
            .await
            .expect("parked");
        assert_eq!(h.pending.len(), 1);

        // Sleep past the deadline — the evictor fires.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(h.pending.len(), 0, "evictor should remove the parked leg");
        let orphaned = metrics
            .webrtc_sessions_paired
            .get_or_create(&WebRtcPartnerLabel {
                partner: "none".into(),
            })
            .get();
        assert_eq!(orphaned, 1);
    }

    #[tokio::test]
    async fn relay_only_rejects_offer_with_host_candidate() {
        //  — in `relay_only`, an offer
        // advertising a `host` candidate is rejected before
        // the negotiator even runs; the reject-reason counter
        // bumps against the `host` bucket.
        let mut scratch = prometheus_client::registry::Registry::default();
        let metrics = Metrics::register(&mut scratch);
        let neg: Arc<dyn SdpNegotiator> = Arc::new(Negotiator::with_default_codecs(IpAddr::V4(
            Ipv4Addr::LOCALHOST,
        )));
        let h = CliWebRtcHandler::new(neg, fabric(), IpAddr::V4(Ipv4Addr::LOCALHOST))
            .with_metrics(Arc::clone(&metrics))
            .with_privacy(WebRtcPrivacyConfig {
                mode: WebRtcPrivacyMode::RelayOnly,
                redaction_key: String::new(),
            });

        let offer = "v=0\r\n\
                     o=- 1 1 IN IP4 192.0.2.1\r\n\
                     s=-\r\n\
                     c=IN IP4 192.0.2.1\r\n\
                     t=0 0\r\n\
                     m=audio 49170 RTP/AVP 0\r\n\
                     a=rtpmap:0 PCMU/8000\r\n\
                     a=candidate:1 1 UDP 2130706431 192.168.1.5 49170 typ host\r\n";
        let err = h
            .handle_offer(WebTransportSessionId(77), offer)
            .await
            .unwrap_err();
        match err {
            WebRtcHandlerError::OfferRejected(reason) => {
                assert!(
                    reason.contains("privacy"),
                    "reason should mention privacy, got: {reason}"
                );
            }
            other => panic!("expected OfferRejected, got {other:?}"),
        }
        let rejected = metrics
            .webrtc_candidates_rejected
            .get_or_create(&PrivacyRejectReasonLabel {
                reason: "host".into(),
            })
            .get();
        assert_eq!(rejected, 1);
    }

    #[tokio::test]
    async fn strict_mode_redacts_peer_ip_in_render() {
        // `render_peer` is what the handler's debug/warn logs
        // call on every peer address. In strict mode with a
        // redaction key set, the IP half is hashed; port stays
        // for triage. The counter bumps exactly once per
        // render.
        let mut scratch = prometheus_client::registry::Registry::default();
        let metrics = Metrics::register(&mut scratch);
        let neg: Arc<dyn SdpNegotiator> = Arc::new(Negotiator::with_default_codecs(IpAddr::V4(
            Ipv4Addr::LOCALHOST,
        )));
        let h = CliWebRtcHandler::new(neg, fabric(), IpAddr::V4(Ipv4Addr::LOCALHOST))
            .with_metrics(Arc::clone(&metrics))
            .with_privacy(WebRtcPrivacyConfig {
                mode: WebRtcPrivacyMode::Strict,
                redaction_key: "rotation-key-2026-04".into(),
            });

        let rendered = h.render_peer("203.0.113.45:55123".parse().unwrap());
        assert!(
            !rendered.contains("203.0.113.45"),
            "strict mode must never render the literal IP"
        );
        assert!(
            rendered.contains(":55123"),
            "port should remain visible for triage; got {rendered}"
        );
        let redactions = metrics.webrtc_privacy_redactions.get();
        assert_eq!(redactions, 1);
    }

    #[tokio::test]
    async fn open_mode_leaves_host_candidates_alone() {
        // Sanity: `Open` preserves the pre-5.11 behaviour —
        // no offer rejection, no redactions.
        let mut scratch = prometheus_client::registry::Registry::default();
        let metrics = Metrics::register(&mut scratch);
        let h = CliWebRtcHandler::new(
            Arc::new(Negotiator::with_default_codecs(IpAddr::V4(
                Ipv4Addr::LOCALHOST,
            ))),
            fabric(),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
        )
        .with_metrics(Arc::clone(&metrics));

        let offer = "v=0\r\n\
                     o=- 1 1 IN IP4 192.0.2.1\r\n\
                     s=-\r\n\
                     c=IN IP4 192.0.2.1\r\n\
                     t=0 0\r\n\
                     m=audio 49170 RTP/AVP 0\r\n\
                     a=rtpmap:0 PCMU/8000\r\n\
                     a=candidate:1 1 UDP 2130706431 192.168.1.5 49170 typ host\r\n";
        let answer = h
            .handle_offer(WebTransportSessionId(78), offer)
            .await
            .expect("open mode must accept");
        assert!(answer.contains("m=audio"));
        assert_eq!(
            metrics
                .webrtc_candidates_rejected
                .get_or_create(&PrivacyRejectReasonLabel {
                    reason: "host".into(),
                })
                .get(),
            0,
            "open mode must not bump reject counter"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sip_leg_pairs_with_parked_webrtc_leg() {
        //  — a WebRTC leg parks under tag
        // "room-1"; a SIP leg arrives via `pair_sip_leg(...)`
        // with the same tag and the bridge is installed
        // through the WebRTC handler's rendezvous map.
        use smiths_core::WebRtcRendezvous;

        let mut scratch = prometheus_client::registry::Registry::default();
        let metrics = Metrics::register(&mut scratch);
        let h = CliWebRtcHandler::new(
            Arc::new(Negotiator::with_default_codecs(IpAddr::V4(
                Ipv4Addr::LOCALHOST,
            ))),
            fabric(),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
        )
        .with_metrics(Arc::clone(&metrics));

        // Park a WebRTC leg by invoking the handler directly —
        // in production this is the first `handle_offer_tagged`.
        let offer = "v=0\r\n\
                     o=- 1 1 IN IP4 127.0.0.1\r\n\
                     s=-\r\n\
                     c=IN IP4 127.0.0.1\r\n\
                     t=0 0\r\n\
                     m=audio 49170 RTP/AVP 0\r\n\
                     a=rtpmap:0 PCMU/8000\r\n";
        h.handle_offer_tagged(WebTransportSessionId(1_001), Some("room-1"), offer)
            .await
            .expect("first leg parks");
        assert_eq!(h.pending.len(), 1);
        assert_eq!(
            metrics
                .webrtc_sessions_paired
                .get_or_create(&WebRtcPartnerLabel {
                    partner: "sip".into(),
                })
                .get(),
            0
        );

        // SIP leg arrives through the rendezvous trait.
        // Allocate a real endpoint on the handler's fabric so
        // the bridge install finds a matching socket.
        let sip_endpoint = h
            .fabric
            .allocate(IpAddr::V4(Ipv4Addr::LOCALHOST))
            .await
            .expect("sip endpoint");
        let sip_peer: SocketAddr = "127.0.0.1:60001".parse().unwrap();
        let bid = h
            .pair_sip_leg(
                "room-1",
                sip_endpoint.id(),
                sip_peer,
                None, // plain RTP — DTLS handshake not exercised in this test
            )
            .await
            .expect("pair_sip_leg succeeds");
        assert!(
            bid.is_some(),
            "partner was parked; should have installed bridge"
        );
        assert_eq!(h.pending.len(), 0, "parked leg pulled");
        // `partner="sip"` credits the bump.
        let sip_paired = metrics
            .webrtc_sessions_paired
            .get_or_create(&WebRtcPartnerLabel {
                partner: "sip".into(),
            })
            .get();
        assert_eq!(sip_paired, 1);

        // No-partner case: sip arrives first, parks.
        let sip_endpoint_2 = h
            .fabric
            .allocate(IpAddr::V4(Ipv4Addr::LOCALHOST))
            .await
            .unwrap();
        let parked = h
            .pair_sip_leg("solo", sip_endpoint_2.id(), sip_peer, None)
            .await
            .expect("park ok");
        assert!(parked.is_none(), "no partner → park, no bridge");
        assert!(
            h.pending.contains_key("solo"),
            "sip leg should be parked in the map"
        );

        // release_sip_leg is idempotent + evicts cleanly.
        h.release_sip_leg("solo").await;
        assert!(!h.pending.contains_key("solo"));
        h.release_sip_leg("solo").await; // no-op second call
    }

    #[tokio::test]
    async fn hot_reload_flips_mode_live() {
        //  Small 1 — mutating the handler's
        // privacy handle flips the enforcement without
        // rebuilding. First offer (open) accepts; reload to
        // relay_only; second offer rejects.
        let mut scratch = prometheus_client::registry::Registry::default();
        let metrics = Metrics::register(&mut scratch);
        let h = CliWebRtcHandler::new(
            Arc::new(Negotiator::with_default_codecs(IpAddr::V4(
                Ipv4Addr::LOCALHOST,
            ))),
            fabric(),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
        )
        .with_metrics(Arc::clone(&metrics));

        let offer = "v=0\r\n\
                     o=- 1 1 IN IP4 192.0.2.1\r\n\
                     s=-\r\n\
                     c=IN IP4 192.0.2.1\r\n\
                     t=0 0\r\n\
                     m=audio 49170 RTP/AVP 0\r\n\
                     a=rtpmap:0 PCMU/8000\r\n\
                     a=candidate:1 1 UDP 2130706431 192.168.1.5 49170 typ host\r\n";
        h.handle_offer(WebTransportSessionId(81), offer)
            .await
            .expect("open mode accepts");

        // Flip the mode via the handle the config adapter uses.
        *h.privacy_handle().lock().unwrap() = WebRtcPrivacyConfig {
            mode: WebRtcPrivacyMode::RelayOnly,
            redaction_key: String::new(),
        };

        let err = h
            .handle_offer(WebTransportSessionId(82), offer)
            .await
            .unwrap_err();
        assert!(matches!(err, WebRtcHandlerError::OfferRejected(_)));
    }
}
