//! Minimal User-Agent Server.
//!
//! Scope today:
//! - `OPTIONS` → `200 OK`.
//! - `INVITE` with SDP body → `100 Trying`, then `200 OK` carrying an
//!   SDP answer with an engine-allocated UDP port. Creates an early
//!   dialog; `ACK` confirms; `BYE` tears it down with `200 OK`.
//! - `INVITE` with no common codec → `488 Not Acceptable Here`.
//! - **Rendezvous bridging**: two `INVITE`s with the same Request-URI
//!   user-part (e.g. both to `sip:room-1@engine`) are paired. The engine
//!   spins up a byte-transparent UDP bridge between their media sockets
//!   and tears it down on `BYE` from either side.
//! - Every other method → `405 Method Not Allowed`.
//! - UDP retransmission dedupe by `Via` branch: retransmits replay the
//!   cached final response byte-for-byte.
//! - INVITE 2xx retransmit lives as a per-dialog timer loop per
//!   RFC 3261 §13.3.1.4 — bytes parked on the [`DialogRecord`],
//!   driven by [`Self::spawn_invite_2xx_retransmit`], cancelled on
//!   ACK arrival in [`Self::handle_ack`].
//!
//! Non-scope (follow-up passes): full RFC 3261 transaction FSMs with
//! timers A–K, `CANCEL`, re-`INVITE`, `UPDATE`, N-party conferences,
//! TCP/TLS transports.

use std::fmt::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use dashmap::DashMap;
use smiths_core::metrics::{Metrics, SipCodeLabel, SipMethodLabel};
use smiths_core::{
    BridgeId, BridgeLeg, DialogKey, DialogRecord, DialogSessions, DialogState, EndpointId, Event,
    EventBus, MediaFabric, NegotiatedCodec, NegotiationOutcome, SdpNegotiator, SipEvent, SrtpKeys,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument, warn};

use crate::transport::{Datagram, Transport};
use crate::txn::{
    Role as TxnRole, ServerInviteTxn, ServerNonInviteTxn, TransactionDriver,
    TransactionKey as TxnKey,
};

/// RFC 3261 §17.1.1.2 / §13.3.1.4 base retransmit interval (500 ms).
const T1: Duration = Duration::from_millis(500);
/// RFC 3261 §17.1.1.2 upper bound on a single retransmit interval (4 s).
///
/// §13.3.1.4 instructs the TU to double the interval starting at T1
/// and **cap each interval at T2**; this is the cap.
const T2: Duration = Duration::from_secs(4);
/// RFC 3261 §13.3.1.4 total retransmit budget: 64 · T1 = 32 s. After
/// this much wall-clock has elapsed without ACK, the UAS should
/// terminate the dialog (via BYE) — we cancel the loop here and
/// leave dialog termination to a follow-on.
const INVITE_2XX_BUDGET: Duration = Duration::from_secs(32);

/// First leg of a pending rendezvous bridge, waiting for a matching
/// second `INVITE`. Holds only tokens — the socket lives in the
/// [`MediaFabric`].
#[derive(Clone, Debug)]
struct PendingLeg {
    dialog_key: DialogKey,
    endpoint: EndpointId,
    remote_media: SocketAddr,
    /// Negotiated SRTP keys for this leg. `None` for plain-RTP calls;
    /// `Some(_)` when the offerer advertised `RTP/SAVP` with a
    /// supported `a=crypto:` line. Threaded into the bridge when the
    /// matching second INVITE lands.
    srtp: Option<SrtpKeys>,
    /// Codec the negotiator chose for this leg (slice 5.6c).
    /// Carried through the rendezvous so the pairing step can
    /// detect a codec mismatch and route to the transcoded
    /// session path instead of the plain passthrough bridge.
    audio_codec: Option<NegotiatedCodec>,
}

/// Per-dialog CDR metadata captured at 200 OK INVITE. Consumed
/// on BYE to emit a `CallDetailRecord` via the configured
/// [`smiths_core::storage::CdrStore`].
#[derive(Clone, Debug)]
struct CdrInProgress {
    call_id: String,
    from_uri: String,
    to_uri: String,
    started_at_unix: i64,
}

/// Seam the UAS uses to build a transcoded media session when a
/// rendezvous pair's two legs speak different codecs (slice
/// 5.6c). Keeps `smiths-sip` free of a direct
/// `smiths-transcode` / `smiths-media` dep — the CLI wires a
/// concrete impl (typically `smiths_media::TranscodedSession` +
/// `smiths_transcode::CpuBudget`) at boot.
///
/// `try_orchestrate` is consulted only when the two legs'
/// [`NegotiatedCodec`] values differ. Implementations return:
///
/// - `Ok(Some(session))` on admission success — the UAS installs
///   the session via [`DialogSessions`] and skips the plain
///   passthrough bridge.
/// - `Ok(None)` when admission is refused (CPU budget
///   exhausted). Callers emit a `488 Not Acceptable Here` plus
///   `Warning: 370` — the UAS uses this as a signal to tear the
///   second leg down cleanly.
/// - `Err(_)` on fabric / codec construction failure; treated
///   the same as a passthrough bridge failure today (logged, no
///   bridge installed).
#[async_trait::async_trait]
pub trait TranscodeOrchestrator: Send + Sync {
    /// Attempt to build a transcoded session for the two legs.
    async fn try_orchestrate(
        &self,
        leg_a: BridgeLeg,
        codec_a: NegotiatedCodec,
        leg_b: BridgeLeg,
        codec_b: NegotiatedCodec,
    ) -> Result<Option<Arc<dyn smiths_core::media::MediaSession>>, smiths_core::MediaError>;
}

/// Seam the UAS uses to install a T.38 UDPTL session when a
/// re-INVITE flips a live audio call to FAX (slice 5.6d
/// scaffold). Parallel to [`TranscodeOrchestrator`]; the future
/// re-INVITE handler calls `try_orchestrate_fax` on detection
/// of `m=image udptl t38` in the offer, then
/// `DialogSessions::swap` to atomically replace the audio
/// session. Today's UAS doesn't yet parse re-INVITE bodies
/// (non-scope per `handle_invite`'s module doc) — the seam
/// exists so the future work slots in without another
/// `UasServer` surface change.
#[async_trait::async_trait]
pub trait FaxOrchestrator: Send + Sync {
    /// Build a UDPTL relay session bridging two legs that just
    /// re-INVITEd into T.38. `leg_a` + `leg_b` carry each side's
    /// local endpoint + peer UDPTL address from the paired
    /// re-INVITE answers. `None` = orchestrator declined
    /// (policy, resource exhaustion).
    async fn try_orchestrate_fax(
        &self,
        leg_a: BridgeLeg,
        leg_b: BridgeLeg,
    ) -> Result<Option<Arc<dyn smiths_core::media::MediaSession>>, smiths_core::MediaError>;
}

/// Seam the UAS uses to install a conference-participant
/// session when an MCP `join_conference` call fires against a
/// live dialog (slice 5.6e scaffold). Parallel to
/// [`TranscodeOrchestrator`]; the MCP wiring is a follow-on —
/// today's `join_conference` tool updates the
/// `ConferenceRegistry` but doesn't yet swap the 2-peer bridge
/// for a conference-participant session.
#[async_trait::async_trait]
pub trait ConferenceOrchestrator: Send + Sync {
    /// Install a conference-participant session on `dialog`.
    /// `conference_id` is the opaque id minted by
    /// `smiths_mixer::ConferenceRegistry::create`. `None` =
    /// orchestrator declined (conference closed, etc.).
    async fn try_orchestrate_conference(
        &self,
        dialog: DialogKey,
        conference_id: u64,
        leg: BridgeLeg,
    ) -> Result<Option<Arc<dyn smiths_core::media::MediaSession>>, smiths_core::MediaError>;

    /// Join `leg` into the conference named `room`, creating the
    /// conference on first use. This is the entry point the UAS uses
    /// for conference *rooms*: it doesn't know conference ids, only
    /// the Request-URI user-part. Each INVITE to the same room adds
    /// one participant to the same mixer — no pairing, no parking.
    /// `None` = the orchestrator declined (caller falls back to the
    /// 2-peer rendezvous). Default declines so non-mixer
    /// implementations needn't implement room semantics.
    async fn orchestrate_room(
        &self,
        dialog: DialogKey,
        room: &str,
        leg: BridgeLeg,
    ) -> Result<Option<Arc<dyn smiths_core::media::MediaSession>>, smiths_core::MediaError> {
        let _ = (dialog, room, leg);
        Ok(None)
    }
}

/// Parsed request summary.
struct RequestSummary {
    method: String,
    branch: Option<String>,
    call_id: Option<String>,
    from_tag: Option<String>,
    to_tag: Option<String>,
    /// User-part of the Request-URI (everything between `sip:` and the
    /// `@` on the request line). Used as a rendezvous key.
    ruri_user: Option<String>,
    /// Full Request-URI from the request line, used by digest auth.
    request_uri: Option<String>,
    /// Raw `Authorization:` header value, if present.
    authorization: Option<String>,
    /// Normalized `Content-Type` header value, lowercased without
    /// trailing whitespace or parameters.
    content_type: Option<String>,
    /// Raw `Contact:` header value (everything after the colon).
    /// Parsed by the REGISTER path to extract the contact URI.
    /// `None` on requests that omit Contact entirely — common on
    /// OPTIONS + BYE where the header isn't mandatory.
    contact: Option<String>,
    /// URI-part of the `From:` header (`sip:bob@x`, no tag / params).
    /// Extracted by [`summarize_request`] so the CDR path doesn't
    /// re-parse the header.
    from_uri: Option<String>,
    /// URI-part of the `To:` header (`sip:alice@y`, no tag / params).
    to_uri: Option<String>,
    /// Parsed `Expires:` header (RFC 3261 §20.19). For REGISTER the
    /// expiration is also carryable on each `Contact:` param via
    /// `;expires=N`; we honour the top-level header as the default
    /// and let the registrar override per-contact.
    expires: Option<u32>,
    /// Message body as UTF-8 (SDP is ASCII).
    body: Option<String>,
    /// Raw request bytes; the response builder copies header lines
    /// from them verbatim.
    raw: Bytes,
    /// `X-Smiths-Webrtc-Tag:` (slice 5.10-sipjoin). Present only
    /// on INVITEs that want to join a pre-parked WebRTC leg
    /// sharing the same tag via the `[webrtc]` rendezvous map.
    /// Absent on every other request + on INVITEs from clients
    /// that don't care about WebRTC bridging.
    webrtc_tag: Option<String>,
}

/// UAS answering a subset of RFC 3261 requests.
///
/// Owns no sockets of its own beyond the SIP signaling transport. All
/// media-plane resources are borrowed from the injected
/// [`MediaFabric`]; SDP parsing/negotiation happens entirely through
/// the injected [`SdpNegotiator`]. That split is what lets this crate
/// depend on `smiths-core` only — no cross-sibling deps on
/// `smiths-media` or `smiths-sdp`.
pub struct UasServer<T: Transport> {
    transport: Arc<T>,
    bus: EventBus,
    /// Per-dialog 2xx INVITE retransmit handles. The cancel token is
    /// tripped by [`Self::handle_ack`] on Early → Confirmed and by
    /// dialog teardown; the spawned retransmit task exits either way.
    /// Keyed by dialog rather than branch because the 2xx ACK is
    /// end-to-end (its own transaction) per RFC 3261 §17.1.1.3 — the
    /// INVITE branch is no help once the FSM has 2xx-bypassed to
    /// Terminated.
    invite_2xx_retransmits: Arc<DashMap<DialogKey, CancellationToken>>,
    /// Active + early dialog records keyed by
    /// `(Call-ID, local-tag, remote-tag)`. [`DialogRecord`] is
    /// serializable — this is the HA snapshot surface.
    dialogs: Arc<DashMap<DialogKey, DialogRecord>>,
    /// `Contact` header value used in responses that establish or
    /// target a dialog. Preformatted at startup from the local bind.
    contact: String,
    /// Media fabric: allocator + bridge factory. All RTP sockets live
    /// here; the UAS only ever holds [`EndpointId`] / [`BridgeId`].
    media_fabric: Arc<dyn MediaFabric>,
    /// SDP offer/answer engine. Opaque behind the trait.
    negotiator: Arc<dyn SdpNegotiator>,
    /// IP to bind RTP sockets on. Mirrors the signaling transport's
    /// local IP.
    media_bind_ip: IpAddr,
    /// When set, published in SDP answers instead of [`media_bind_ip`]
    /// or the routing-table guess — required for NAT/DMZ trunk setups.
    sdp_advertise_ip: Option<IpAddr>,
    /// First-come leg of a rendezvous bridge, keyed by Request-URI
    /// user-part. The second `INVITE` with the same key pairs with it.
    pending_bridges: Arc<DashMap<String, PendingLeg>>,
    /// Live bridges keyed by dialog. Both sides of a paired call point
    /// at the same [`BridgeId`]; the first `BYE` releases it from the
    /// fabric and clears both entries.
    bridges_by_dialog: Arc<DashMap<DialogKey, BridgeId>>,
    /// Non-passthrough media sessions (transcoding, later FAX /
    /// conference) keyed per-leg. Slice 5.6c uses this for the
    /// transcoded rendezvous path only; FAX / conference wirings
    /// are follow-on slices. Empty when no transcoded path has
    /// fired — the plain-bridge rendezvous stays untouched.
    dialog_sessions: DialogSessions,
    /// Optional transcoded-session builder (slice 5.6c). `None` =
    /// rendezvous mismatches fall back to the plain passthrough
    /// bridge (the pre-5.6c behaviour). The CLI wires a real
    /// orchestrator from `[media.transcode]` config.
    transcode_orchestrator: Option<Arc<dyn TranscodeOrchestrator>>,
    /// Optional FAX session builder (slice 5.6d scaffold).
    /// `None` = re-INVITE to T.38 doesn't install a session (the
    /// future re-INVITE handler logs + does nothing). CLI wires
    /// a real orchestrator from `[media.fax]` config.
    fax_orchestrator: Option<Arc<dyn FaxOrchestrator>>,
    /// Optional conference-participant session builder (slice
    /// 5.6e scaffold). `None` = `join_conference` MCP tool
    /// updates the registry but doesn't yet bridge RTP. CLI
    /// wires a real orchestrator from `[media.mixer]` config.
    conference_orchestrator: Option<Arc<dyn ConferenceOrchestrator>>,
    /// Request-URI user-part prefix that marks a *conference room*
    /// (slice 5.6e-runtime). When set and a conference orchestrator is
    /// wired, an INVITE whose room matches this prefix joins an N-party
    /// mixer (one participant per INVITE) instead of the 2-peer
    /// rendezvous. `None` = no conference routing — every room uses the
    /// classic bridge, so default behaviour is unchanged.
    conference_room_prefix: Option<String>,
    /// Registrar: digest-auths `REGISTER` against a [`CredentialStore`].
    /// `None` = auth disabled, registrar accepts any REGISTER blindly
    /// (dev convenience; never do that in prod).
    registrar: Option<crate::auth::digest::Registrar>,
    /// Contact-binding persistence for registered UAs (slice 2.1).
    /// `None` = in-memory REGISTER handling only (every successful
    /// REGISTER is 200 OK but the binding isn't persisted anywhere).
    /// Production deployments wire a `SqliteAuthStore` (or equivalent)
    /// here via [`Self::with_registration_store`] so `sip://
    /// registrations` has something to read.
    registration_store: Option<Arc<dyn crate::auth::RegistrationStore>>,
    /// Call-detail-record persistence (slice 2.3, P23). `None` =
    /// CDR recording is off; `handle_bye` emits no CDR rows. When
    /// wired, a row lands per dialog terminate with duration +
    /// From/To + result ("answered").
    cdr_store: Option<Arc<dyn smiths_core::storage::CdrStore>>,
    /// Per-dialog CDR metadata captured at 200 OK INVITE and
    /// consumed on `handle_bye`. Kept off `DialogRecord` so the
    /// serializable snapshot surface (HA) stays clean — a failover
    /// primary that resumes mid-call won't emit a CDR for the old
    /// dialog it inherits, which is the correct posture.
    cdr_pending: Arc<DashMap<DialogKey, CdrInProgress>>,
    /// Prometheus metrics. Defaults to [`Metrics::noop`] so tests and
    /// single-server setups can ignore observability entirely.
    metrics: Arc<Metrics>,
    /// Shared correlator for responses to locally-originated requests
    /// (the [`crate::UacClient`]). `None` = UAS-only deployment;
    /// responses are simply dropped (old behaviour).
    response_router: Option<Arc<crate::ResponseRouter>>,
    /// Shared graceful-drain flag. When set, new `INVITE`s are
    /// rejected with `503 Service Unavailable` so load balancers
    /// route traffic elsewhere while live dialogs finish naturally.
    /// `None` = drain disabled (tests, single-shot deployments).
    drain: Option<smiths_core::Drain>,
    /// Per-source-IP token bucket. Always present — when config
    /// disables rate limiting it's a cheap always-allow.
    rate_limit: crate::rate_limit::SipRateLimiter,
    /// Optional handle on the WebRTC rendezvous map (slice
    /// 5.10-sipjoin). When present + an `INVITE` carries
    /// `X-Smiths-Webrtc-Tag:`, the UAS asks the rendezvous to
    /// bridge this SIP dialog with a WebRTC leg sharing the
    /// same tag instead of running the normal SIP-side
    /// rendezvous on the Request-URI user-part. `None` = the
    /// header is silently ignored (safe fallback for
    /// deployments without the WebRTC adapter wired).
    webrtc_rendezvous: Option<Arc<dyn smiths_core::WebRtcRendezvous>>,
    /// HA replicator (slice 6.2). Standalone deployments use a no-op.
    replicator: Arc<dyn smiths_core::Replicator>,
    /// Async driver hosting every server-side transaction — both
    /// INVITE (`ServerInviteTxn` with G/H/I timers, ACK correlation,
    /// 2xx bypass) and non-INVITE (`ServerNonInviteTxn` with timer J).
    /// Retransmit replay is FSM-driven, freeing the legacy
    /// dedupe-DashMap path entirely. INVITE 2xx bypasses the FSM and
    /// is TU-owned (see [`Self::invite_2xx_retransmits`]).
    txn_driver: TransactionDriver<T>,
}

impl<T: Transport> UasServer<T> {
    /// Build a new UAS. Reads the transport's local address to compose
    /// the `Contact` header and derive the media bind IP.
    pub fn new(
        transport: Arc<T>,
        bus: EventBus,
        media_fabric: Arc<dyn MediaFabric>,
        negotiator: Arc<dyn SdpNegotiator>,
    ) -> Result<Self, crate::Error> {
        let local = transport.local_addr()?;
        // A bind of `0.0.0.0` or `[::]` would produce a non-routable
        // Contact — fine for localhost tests; the B2BUA work in later
        // phases will compute this per outbound peer.
        let contact = format!("<sip:smiths@{local}>");
        // Server-side txn driver. A fresh `ResponseRouter` is passed
        // in because the driver constructor requires one — server
        // FSMs never subscribe to it (only client FSMs do). When a
        // shared router is injected via [`Self::with_response_router`]
        // the UAC's driver uses it for response demux; the UAS's
        // server-side driver stays independent.
        let router = Arc::new(crate::ResponseRouter::new());
        let txn_driver = TransactionDriver::new(Arc::clone(&transport), router);
        Ok(Self {
            transport,
            bus,
            invite_2xx_retransmits: Arc::new(DashMap::new()),
            dialogs: Arc::new(DashMap::new()),
            replicator: Arc::new(smiths_core::NoopReplicator),
            contact,
            media_fabric,
            negotiator,
            media_bind_ip: local.ip(),
            sdp_advertise_ip: None,
            pending_bridges: Arc::new(DashMap::new()),
            bridges_by_dialog: Arc::new(DashMap::new()),
            dialog_sessions: DialogSessions::new(),
            transcode_orchestrator: None,
            fax_orchestrator: None,
            conference_orchestrator: None,
            conference_room_prefix: None,
            registrar: None,
            registration_store: None,
            cdr_store: None,
            cdr_pending: Arc::new(DashMap::new()),
            metrics: Metrics::noop(),
            response_router: None,
            drain: None,
            rate_limit: crate::rate_limit::SipRateLimiter::disabled(),
            webrtc_rendezvous: None,
            txn_driver,
        })
    }

    /// Attach a [`smiths_core::WebRtcRendezvous`] handle so
    /// `INVITE` requests carrying `X-Smiths-Webrtc-Tag:` can
    /// bridge with a pre-parked WebRTC leg (slice
    /// 5.10-sipjoin). `None` = the header is ignored.
    #[must_use]
    pub fn with_webrtc_rendezvous(
        mut self,
        rendezvous: Arc<dyn smiths_core::WebRtcRendezvous>,
    ) -> Self {
        self.webrtc_rendezvous = Some(rendezvous);
        self
    }

    /// Attach a digest registrar — `REGISTER` now requires valid auth.
    #[must_use]
    pub fn with_registrar(mut self, registrar: crate::auth::digest::Registrar) -> Self {
        self.registrar = Some(registrar);
        self
    }

    /// Attach a [`crate::auth::RegistrationStore`] so successful
    /// REGISTER requests persist their `Contact:` bindings. Without
    /// one, REGISTER still authenticates + returns `200 OK` but
    /// nothing survives past the response — in-memory deployments
    /// where no MCP caller needs to inspect bindings.
    #[must_use]
    pub fn with_registration_store(
        mut self,
        store: Arc<dyn crate::auth::RegistrationStore>,
    ) -> Self {
        self.registration_store = Some(store);
        self
    }

    /// Attach a [`smiths_core::storage::CdrStore`] so dialog
    /// terminates emit a call-detail row. Without one, the UAS
    /// still serves BYE correctly — it just produces no audit trail.
    #[must_use]
    pub fn with_cdr_store(mut self, store: Arc<dyn smiths_core::storage::CdrStore>) -> Self {
        self.cdr_store = Some(store);
        self
    }

    /// Attach a shared metrics handle. Without this, the UAS uses a
    /// throwaway registry — safe for tests, invisible to operators.
    /// Also rebuilds the transaction driver so its
    /// `sip_server_txns_active` gauge increments flow into the same
    /// registry that the `/metrics` endpoint serves.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        // Rebuild the driver with a shared metrics handle. The driver
        // is cheap (just a fresh Arc-backed inner + DashMap), so
        // swapping it before any inbound traffic has registered txns
        // is fine.
        let router = Arc::new(crate::ResponseRouter::new());
        self.txn_driver = TransactionDriver::new(Arc::clone(&self.transport), router)
            .with_metrics(Arc::clone(&metrics));
        self.metrics = metrics;
        self
    }

    /// Install a [`crate::ResponseRouter`] so responses arriving on
    /// the UAS's socket get forwarded to the UAC. Without this, the
    /// UAS drops responses (pre-UAC behaviour).
    #[must_use]
    pub fn with_response_router(mut self, router: Arc<crate::ResponseRouter>) -> Self {
        self.response_router = Some(router);
        self
    }

    /// Override the IP address embedded in SDP answers (NAT / DMZ).
    #[must_use]
    pub fn with_sdp_advertise_ip(mut self, ip: IpAddr) -> Self {
        self.sdp_advertise_ip = Some(ip);
        self
    }

    /// Override the host in the `Contact:` header (NAT / DMZ).
    ///
    /// [`Self::new`] composes `Contact` from the transport's local
    /// address, which for a `0.0.0.0` bind is not routable. A carrier
    /// that targets the dialog by `Contact` — Megafon Multifon does —
    /// then sends its ACK for our 200 OK to `0.0.0.0`, it never
    /// arrives, and the call is torn down once the 2xx retransmit
    /// budget is exhausted. Setting the public address here keeps the
    /// ACK on a routable path. The port is left as bound.
    #[must_use]
    pub fn with_contact_advertise_ip(mut self, ip: IpAddr) -> Self {
        if let Ok(local) = self.transport.local_addr() {
            self.contact = format!("<sip:smiths@{}>", SocketAddr::new(ip, local.port()));
        }
        self
    }

    /// Attach a shared [`smiths_core::Drain`] so the UAS can refuse
    /// new INVITEs during graceful shutdown. Without it, drain-aware
    /// shutdown is a no-op (new dialogs keep being admitted until the
    /// cancel token fires).
    #[must_use]
    pub fn with_drain(mut self, drain: smiths_core::Drain) -> Self {
        self.drain = Some(drain);
        self
    }

    /// Attach a per-source-IP rate limiter. Without this, the UAS
    /// runs with an always-allow limiter (zero overhead) — the CLI
    /// wires a real one from `config.sip.rate_limit`.
    #[must_use]
    pub fn with_rate_limit(mut self, rate_limit: crate::rate_limit::SipRateLimiter) -> Self {
        self.rate_limit = rate_limit;
        self
    }

    /// Attach a [`TranscodeOrchestrator`] so rendezvous pairs with
    /// different per-leg codecs route through a transcoded session
    /// instead of a plain passthrough bridge (slice 5.6c). Without
    /// this, a codec mismatch falls through to the passthrough
    /// path — which forwards bytes but won't be audible to the peer
    /// that expected a different codec.
    #[must_use]
    pub fn with_transcode_orchestrator(
        mut self,
        orchestrator: Arc<dyn TranscodeOrchestrator>,
    ) -> Self {
        self.transcode_orchestrator = Some(orchestrator);
        self
    }

    /// Attach a [`FaxOrchestrator`] (slice 5.6d scaffold). Today's
    /// UAS has no re-INVITE parser so this field is read-only
    /// until the future handler lands; the accessor is a
    /// forward-compat hook so a deployment that's ready with an
    /// orchestrator can wire it today and have it activate when
    /// the re-INVITE path does.
    #[must_use]
    pub fn with_fax_orchestrator(mut self, orchestrator: Arc<dyn FaxOrchestrator>) -> Self {
        self.fax_orchestrator = Some(orchestrator);
        self
    }

    /// Attach a [`ConferenceOrchestrator`] (slice 5.6e scaffold).
    /// Same forward-compat story as [`Self::with_fax_orchestrator`].
    #[must_use]
    pub fn with_conference_orchestrator(
        mut self,
        orchestrator: Arc<dyn ConferenceOrchestrator>,
    ) -> Self {
        self.conference_orchestrator = Some(orchestrator);
        self
    }

    /// Mark a Request-URI user-part prefix as conference rooms (slice
    /// 5.6e-runtime). With a [`ConferenceOrchestrator`] also wired,
    /// INVITEs to `sip:<prefix>…@engine` join an N-party mixer instead
    /// of the 2-peer rendezvous. Without this, all rooms bridge as
    /// before.
    #[must_use]
    pub fn with_conference_rooms(mut self, prefix: impl Into<String>) -> Self {
        self.conference_room_prefix = Some(prefix.into());
        self
    }

    /// Inject an HA replicator (slice 6.2).
    #[must_use]
    pub fn with_replicator(mut self, replicator: Arc<dyn smiths_core::Replicator>) -> Self {
        self.replicator = replicator;
        self
    }

    /// Inject an existing dialog table (slice 6.2).
    /// Useful for sharing the table across multiple listeners in HA setups.
    #[must_use]
    pub fn with_dialogs(
        mut self,
        dialogs: Arc<dashmap::DashMap<smiths_core::DialogKey, smiths_core::DialogRecord>>,
    ) -> Self {
        self.dialogs = dialogs;
        self
    }

    /// Snapshot handle on the runtime session table (slice 5.6c).
    /// Useful for tests asserting which transcoded / FAX /
    /// conference sessions have been installed.
    #[must_use]
    pub fn dialog_sessions(&self) -> &DialogSessions {
        &self.dialog_sessions
    }

    /// Shared handle on the dialog table (slice 6.1). Cloned out
    /// so the CLI can take a live-dialog snapshot on graceful
    /// shutdown — the UAS's `run()` consumes `self`, so without
    /// this accessor the snapshot path would need to live inside
    /// the UAS and duplicate the shutdown plumbing. The `Arc` +
    /// `DashMap` are cheap to share; concurrent read from the
    /// snapshot writer doesn't interfere with the live UAS
    /// modifying its own dialogs because `DashMap::iter` yields
    /// a consistent per-shard view.
    #[must_use]
    pub fn dialogs_handle(&self) -> Arc<DashMap<DialogKey, DialogRecord>> {
        Arc::clone(&self.dialogs)
    }

    /// Prime the dialog table with a set of pre-existing records
    /// (slice 6.1 — snapshot replay). Called by the CLI at boot
    /// before `run()` if a snapshot file was loaded. Each restored
    /// dialog gets its record slot re-populated; the UAS then
    /// processes subsequent in-dialog requests (ACK, BYE,
    /// re-INVITE) exactly as if the record had been built by a
    /// live INVITE.
    ///
    /// Returns the number of records restored so the caller can
    /// emit a log or increment its own counter.
    ///
    /// Does **not** re-bind media endpoints or rebuild bridges —
    /// a failover primary restarts the media plane from cold, which
    /// is correct: an in-flight RTP flow belonging to a pre-crash
    /// primary can't be resumed without the socket state, and a
    /// sane BYE (from either side) will tear the restored record
    /// down cleanly.
    pub fn restore_dialogs(&self, records: impl IntoIterator<Item = DialogRecord>) -> usize {
        let mut n = 0;
        for rec in records {
            let key = rec.key();
            self.dialogs.insert(key, rec);
            n += 1;
        }
        n
    }

    /// Run the UAS event loop. Exits when `cancel` fires or `rx` closes.
    #[instrument(skip_all)]
    pub async fn run(self, mut rx: mpsc::Receiver<Datagram>, cancel: CancellationToken) {
        info!("UAS started");
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    debug!("UAS cancelled");
                    break;
                }
                maybe = rx.recv() => {
                    let Some(dg) = maybe else {
                        debug!("UAS datagram channel closed");
                        break;
                    };
                    self.handle_datagram(dg).await;
                }
            }
        }
        info!("UAS stopped");
    }

    async fn handle_datagram(&self, dg: Datagram) {
        let peer = dg.peer;
        // Rate-limit *before* parsing: reject hostile bursts without
        // burning the rsip parser on them. Disabled limiter is a
        // single-atomic no-op.
        if !self.rate_limit.allow(peer.ip()) {
            debug!(%peer, "SIP datagram dropped by rate limiter");
            return;
        }
        let parse = match rsip::SipMessage::try_from(dg.bytes.as_ref()) {
            Ok(msg) => msg,
            Err(e) => {
                let reason = e.to_string();
                warn!(%peer, %reason, "malformed SIP message dropped");
                self.metrics.sip_parse_errors.inc();
                let _ = self
                    .bus
                    .publish(Event::Sip(SipEvent::ParseError { peer, reason }));
                return;
            }
        };

        match parse {
            rsip::SipMessage::Request(_) => {
                let summary = summarize_request(&dg.bytes);
                self.handle_request(summary, peer).await;
            }
            rsip::SipMessage::Response(_) => {
                if let Some(router) = self.response_router.as_ref() {
                    if let Some(branch) = extract_via_branch(&dg.bytes) {
                        let delivered = router.deliver(&branch, dg.bytes.clone());
                        if !delivered {
                            debug!(%peer, branch, "response with unknown branch; dropped");
                        }
                    } else {
                        debug!(%peer, "response without Via branch; dropped");
                    }
                } else {
                    debug!(%peer, "ignoring response (no UAC attached)");
                }
            }
        }
    }

    async fn handle_request(&self, req: RequestSummary, peer: SocketAddr) {
        info!(
            %peer,
            method = %req.method,
            call_id = req.call_id.as_deref().unwrap_or("-"),
            "SIP request received"
        );
        self.metrics
            .sip_requests
            .get_or_create(&SipMethodLabel {
                method: req.method.clone(),
            })
            .inc();
        let _ = self.bus.publish(Event::Sip(SipEvent::RequestReceived {
            peer,
            method: req.method.clone(),
            call_id: req.call_id.clone(),
        }));

        // Server-side transaction routing. Every method the UAS
        // responds to — INVITE / OPTIONS / BYE / REGISTER / CANCEL /
        // unknown-405 — lives as a `ServerInviteTxn` or
        // `ServerNonInviteTxn` in the driver. Retransmits feed into
        // `deliver_request` so the FSM replays its cached final
        // response; new requests register a fresh FSM entry before
        // the handler runs, so the subsequent [`Self::respond`] call
        // routes through `send_response`.
        //
        // ACK is the exception on both sides: it never gets its own
        // transaction (RFC 3261 §17.1.1.3 makes ACK for non-2xx part
        // of the INVITE transaction; ACK for 2xx is end-to-end per
        // §13.3.1.4). [`Self::handle_ack`] reaches into the INVITE
        // FSM directly for the non-2xx → Confirmed transition.
        if let Some(branch) = req.branch.as_deref()
            && req.method != "ACK"
        {
            {
                let key = server_txn_key(branch, &req.method);
                if self.txn_driver.is_alive(&key) {
                    // Retransmit: FSM replays its cached response.
                    debug!(%peer, branch, method = %req.method, "retransmit → server FSM");
                    self.txn_driver
                        .deliver_request(&key, req.method.clone(), req.raw.clone());
                    return;
                }
                // INVITE retransmits arriving after the FSM has
                // 2xx-bypassed to Terminated are dropped — the
                // per-dialog retransmit loop owns replay cadence
                // (RFC 3261 §13.3.1.4). Answering a peer retry here
                // would inject an off-schedule 2xx and break the
                // T1-doubling contract.
                if req.method == "INVITE" && self.dialog_for_invite(&req).is_some() {
                    debug!(
                        %peer, branch,
                        "INVITE retransmit for dialog with live 2xx loop; dropped (TU drives replay)"
                    );
                    return;
                }
                // Fresh transaction — register before the handler
                // runs so `respond` / `send_provisional` route through
                // `driver.send_response`.
                if req.method == "INVITE" {
                    let txn = ServerInviteTxn::new(branch.to_string());
                    let _tu_rx = self.txn_driver.start_server(Box::new(txn), peer);
                } else {
                    let txn = ServerNonInviteTxn::new(branch.to_string(), req.method.clone());
                    let _tu_rx = self.txn_driver.start_server(Box::new(txn), peer);
                }
            }
        }

        match req.method.as_str() {
            "OPTIONS" => self.handle_options(&req, peer).await,
            "INVITE" => self.handle_invite(&req, peer).await,
            "ACK" => self.handle_ack(&req, peer),
            "BYE" => self.handle_bye(&req, peer).await,
            "REGISTER" => self.handle_register(&req, peer).await,
            _ => {
                self.respond(
                    &req,
                    405,
                    "Method Not Allowed",
                    Some(&next_tag()),
                    &[],
                    b"",
                    peer,
                )
                .await;
            }
        }
    }

    async fn handle_options(&self, req: &RequestSummary, peer: SocketAddr) {
        self.respond(req, 200, "OK", Some(&next_tag()), &[], b"", peer)
            .await;
    }

    /// `REGISTER` with digest auth. No registrar attached → blindly
    /// `200 OK` (dev mode). Registrar attached → full challenge-response.
    #[instrument(skip_all, fields(%peer, call_id = %req.call_id.as_deref().unwrap_or("-")))]
    async fn handle_register(&self, req: &RequestSummary, peer: SocketAddr) {
        let Some(reg) = self.registrar.as_ref() else {
            self.respond(req, 200, "OK", Some(&next_tag()), &[], b"", peer)
                .await;
            return;
        };
        let ruri = req.request_uri.as_deref().unwrap_or("");
        match req.authorization.as_deref() {
            None => {
                let challenge = reg.challenge(crate::auth::digest::Algorithm::Md5, false);
                let hdr: [(&str, &str); 1] = [("WWW-Authenticate", &challenge)];
                self.respond(req, 401, "Unauthorized", Some(&next_tag()), &hdr, b"", peer)
                    .await;
            }
            Some(auth) => match reg.authenticate("REGISTER", ruri, auth) {
                Ok(user) => {
                    info!(%user, %peer, "REGISTER authenticated");
                    self.persist_register_binding(req, reg.realm(), &user);
                    self.respond(req, 200, "OK", Some(&next_tag()), &[], b"", peer)
                        .await;
                }
                Err(e) => {
                    info!(?e, %peer, "REGISTER auth failed; re-challenging");
                    let stale = matches!(e, crate::auth::digest::AuthError::StaleNonce);
                    let challenge = reg.challenge(crate::auth::digest::Algorithm::Md5, stale);
                    let hdr: [(&str, &str); 1] = [("WWW-Authenticate", &challenge)];
                    self.respond(req, 401, "Unauthorized", Some(&next_tag()), &hdr, b"", peer)
                        .await;
                }
            },
        }
    }

    /// Persist the `Contact:` → expiry binding a successful REGISTER
    /// just established. Silent no-op when no
    /// [`crate::auth::RegistrationStore`] is attached; the auth flow
    /// has already decided the request is legitimate by this point.
    ///
    /// Slice 2.1 (v0.33.0): the parse is permissive — anything inside
    /// the first `<...>` on the Contact line is the URI; otherwise we
    /// take the first whitespace-delimited token. Full RFC 3261 §25.1
    /// multi-contact + `expires=` parameters are follow-on work; they
    /// matter for forking proxies more than for a registrar that only
    /// binds one AOR at a time.
    fn persist_register_binding(&self, req: &RequestSummary, realm: &str, username: &str) {
        let Some(store) = self.registration_store.as_ref() else {
            return;
        };
        let Some(contact_hdr) = req.contact.as_deref() else {
            debug!("REGISTER missing Contact header; skipping bind");
            return;
        };
        let Some(contact_uri) = first_contact_uri(contact_hdr) else {
            debug!(
                contact = contact_hdr,
                "could not parse Contact URI; skipping bind"
            );
            return;
        };
        let aor = format!("sip:{username}@{realm}");
        // Expires: 0 means "unregister THIS contact" per RFC 3261
        // §10.3.7. Unbind and bow out.
        let ttl = req.expires.unwrap_or(3600);
        if ttl == 0 {
            match store.unbind(&aor, &contact_uri) {
                Ok(()) => debug!(%aor, contact = %contact_uri, "REGISTER unbind"),
                Err(e) => debug!(%aor, ?e, "REGISTER unbind failed"),
            }
            return;
        }
        let expires_at_unix = current_unix_secs().saturating_add(i64::from(ttl));
        let binding = crate::auth::Binding {
            aor,
            contact: contact_uri,
            expires_at_unix,
        };
        match store.bind(&binding) {
            Ok(_) => debug!(aor = %binding.aor, "REGISTER binding persisted"),
            Err(e) => warn!(aor = %binding.aor, ?e, "REGISTER binding persist failed"),
        }
    }

    /// Digest-authenticate an incoming INVITE. Returns `true` when the
    /// request may proceed; emits the appropriate `401 Unauthorized` and
    /// returns `false` otherwise. No registrar attached → every INVITE
    /// is waved through (dev mode, matching `handle_register`).
    async fn invite_auth_ok(&self, req: &RequestSummary, peer: SocketAddr) -> bool {
        let Some(reg) = self.registrar.as_ref() else {
            return true;
        };
        let ruri = req.request_uri.as_deref().unwrap_or("");
        match req.authorization.as_deref() {
            None => {
                let challenge = reg.challenge(crate::auth::digest::Algorithm::Md5, false);
                let hdr: [(&str, &str); 1] = [("WWW-Authenticate", &challenge)];
                self.respond(req, 401, "Unauthorized", Some(&next_tag()), &hdr, b"", peer)
                    .await;
                false
            }
            Some(auth) => match reg.authenticate("INVITE", ruri, auth) {
                Ok(user) => {
                    info!(%user, %peer, "INVITE authenticated");
                    true
                }
                Err(e) => {
                    info!(?e, %peer, "INVITE auth failed; re-challenging");
                    let stale = matches!(e, crate::auth::digest::AuthError::StaleNonce);
                    let challenge = reg.challenge(crate::auth::digest::Algorithm::Md5, stale);
                    let hdr: [(&str, &str); 1] = [("WWW-Authenticate", &challenge)];
                    self.respond(req, 401, "Unauthorized", Some(&next_tag()), &hdr, b"", peer)
                        .await;
                    false
                }
            },
        }
    }

    #[allow(clippy::too_many_lines)] // negotiation + bridge wiring belong together
    #[instrument(skip_all, fields(%peer, call_id = %req.call_id.as_deref().unwrap_or("-")))]
    async fn handle_invite(&self, req: &RequestSummary, peer: SocketAddr) {
        // Graceful drain: refuse new INVITEs before touching auth /
        // media / dialog state. Existing dialogs keep flowing through
        // the BYE path unchanged because drain only gates fresh
        // INVITEs. `Retry-After: 0` tells compliant peers to retry
        // immediately against the next hop in their load-balancer set.
        if let Some(d) = &self.drain
            && d.is_draining()
        {
            debug!(%peer, "rejecting INVITE while draining");
            self.respond(
                req,
                503,
                "Service Unavailable",
                Some(&next_tag()),
                &[("Retry-After", "0")],
                &[],
                peer,
            )
            .await;
            return;
        }

        // When a registrar is attached, INVITE requires digest auth. We
        // challenge before emitting 100 Trying so the rejection path
        // stays tight — no media allocation, no dialog state, just the
        // 401 back to the caller. The ACK that closes the rejected
        // transaction is handled by the normal ACK dispatch below.
        if !self.invite_auth_ok(req, peer).await {
            return;
        }

        // 100 Trying short-circuits UDP INVITE retransmission.
        self.send_provisional(req, 100, "Trying", peer).await;

        let call_id = req.call_id.clone().unwrap_or_default();
        let remote_tag = req.from_tag.clone().unwrap_or_default();
        if call_id.is_empty() || remote_tag.is_empty() {
            warn!(%peer, "INVITE missing Call-ID or From-tag; rejecting 400");
            self.respond(req, 400, "Bad Request", Some(&next_tag()), &[], &[], peer)
                .await;
            return;
        }

        let has_offer =
            matches!(req.content_type.as_deref(), Some("application/sdp")) && req.body.is_some();

        // Allocate a media endpoint first (so we can include its port
        // in the answer), then negotiate. On any failure the endpoint
        // is released so the fabric's table doesn't grow unbounded.
        let (endpoint, sdp_answer_body, remote_media, srtp_keys, audio_codec, video_codec, ice) =
            if has_offer {
                let endpoint = match self.media_fabric.allocate(self.media_bind_ip).await {
                    Ok(ep) => ep,
                    Err(e) => {
                        warn!(?e, "failed to allocate media endpoint for INVITE");
                        self.respond(
                            req,
                            500,
                            "Server Internal Error",
                            Some(&next_tag()),
                            &[],
                            &[],
                            peer,
                        )
                        .await;
                        return;
                    }
                };
                // Safe unwrap: `has_offer` verified Some(body) above.
                let body = req.body.as_deref().unwrap_or_default();
                // When the signaling transport is bound to a wildcard
                // (0.0.0.0 / ::), `media_bind_ip` is unroutable. Ask the
                // kernel which local address it would use to reach `peer`
                // and publish *that* in SDP — otherwise the remote UA
                // tries to sendto(0.0.0.0) and fails.
                let effective_local_ip = self
                    .sdp_advertise_ip
                    .unwrap_or(resolve_local_ip_for(self.media_bind_ip, peer).await);
                // Slice 5.1 / P11: call the multi-stream path with
                // `video_port = None`. The negotiator preserves m-line
                // ordering when the offer carries `m=video` by emitting
                // an RFC 3264 port-0 decline — dual-bridge wiring that
                // actually relays video is a follow-on, but the
                // declining answer shape is right today.
                match self.negotiator.negotiate(
                    body,
                    effective_local_ip,
                    endpoint.local_addr().port(),
                    None,
                ) {
                    NegotiationOutcome::Accepted {
                        answer_body,
                        remote_media,
                        // Slice 5.1: video endpoint surfaces on the
                        // outcome; the UAS's dual-bridge wiring is a
                        // follow-on. Peers that offered video see a
                        // declining `m=video 0 ...` in the answer, so
                        // this binding isn't used yet but keeps the
                        // destructure exhaustive.
                        video_media: _video_media,
                        srtp,
                        // Slice 5.10-dtls: DTLS-SRTP parameters surface
                        // here when the SIP offer used the WebRTC
                        // transport profile. The SIP UAS proper
                        // doesn't drive the DTLS handshake today —
                        // the WebRTC-native adapter (5.10-bridge) is
                        // the consumer; SIP-side handshake wiring is
                        // a dedicated follow-on. Bound so the
                        // destructure stays exhaustive.
                        dtls: _dtls,
                        // Slice 5.6: per-leg codec goes into
                        // DialogRecord.per_leg_codec so the
                        // transcoding router (5.6b) can compare legs.
                        audio_codec,
                        video_codec,
                        ice,
                    } => (
                        Some(endpoint),
                        Some(answer_body),
                        remote_media,
                        srtp,
                        audio_codec,
                        video_codec,
                        ice,
                    ),
                    NegotiationOutcome::Mismatch => {
                        self.media_fabric.release_endpoint(endpoint.id()).await;
                        info!(%peer, "SDP offer had no acceptable codec; 488");
                        self.respond(
                            req,
                            488,
                            "Not Acceptable Here",
                            Some(&next_tag()),
                            &[],
                            &[],
                            peer,
                        )
                        .await;
                        return;
                    }
                    NegotiationOutcome::UnsupportedTransport { reason } => {
                        self.media_fabric.release_endpoint(endpoint.id()).await;
                        info!(%peer, %reason, "SDP offer used an unsupported transport; 488 + Warning");
                        // RFC 3261 §20.43: `Warning: <code> <host> "<text>"`.
                        // Code 399 is the "miscellaneous" catch-all; the
                        // quoted text carries the human-readable reason so
                        // the peer sees *why* we rejected.
                        let warning = format_warning(&reason);
                        let warning_hdr: [(&str, &str); 1] = [("Warning", warning.as_str())];
                        self.respond(
                            req,
                            488,
                            "Not Acceptable Here",
                            Some(&next_tag()),
                            &warning_hdr,
                            &[],
                            peer,
                        )
                        .await;
                        return;
                    }
                    NegotiationOutcome::Malformed(err) => {
                        self.media_fabric.release_endpoint(endpoint.id()).await;
                        warn!(%peer, %err, "malformed SDP offer");
                        self.respond(req, 400, "Bad Request", Some(&next_tag()), &[], &[], peer)
                            .await;
                        return;
                    }
                }
            } else {
                (None, None, None, None, None, None, None)
            };

        let local_tag = next_tag();
        let rendezvous = req.ruri_user.clone();
        let dialog_key: DialogKey = (call_id.clone(), local_tag.clone(), remote_tag.clone());

        // Slice 5.10-sipjoin: when an `X-Smiths-Webrtc-Tag:`
        // header is present + the WebRTC rendezvous is wired,
        // the SIP dialog joins the shared pending-legs map
        // instead of the local Request-URI-user-part one. A
        // match installs the bridge through the WebRTC handler;
        // a miss parks the SIP leg there until its WebRTC
        // partner arrives. Header without rendezvous wired =
        // silent ignore (honest fallback for deployments that
        // don't run WebRTC).
        let mut webrtc_bridged = false;
        if let (Some(tag), Some(rdv), Some(ep), Some(remote_rtp)) = (
            req.webrtc_tag.as_deref(),
            self.webrtc_rendezvous.as_ref(),
            endpoint.as_ref(),
            remote_media,
        ) {
            match rdv
                .pair_sip_leg(tag, ep.id(), remote_rtp, srtp_keys.clone())
                .await
            {
                Ok(Some(bid)) => {
                    self.bridges_by_dialog.insert(dialog_key.clone(), bid);
                    info!(
                        %tag,
                        ?bid,
                        "webrtc rendezvous: SIP dialog bridged to WebRTC partner"
                    );
                    webrtc_bridged = true;
                }
                Ok(None) => {
                    info!(%tag, "webrtc rendezvous: SIP leg parked awaiting WebRTC partner");
                    // The WebRTC handler holds the SIP leg;
                    // the SIP UAS stores the tag on the
                    // DialogRecord so BYE can tell the
                    // rendezvous to release.
                    webrtc_bridged = true; // skip the SIP-side rendezvous
                }
                Err(e) => {
                    warn!(%tag, ?e, "webrtc rendezvous: pair_sip_leg failed; falling through");
                }
            }
        }

        // Conference rooms (slice 5.6e-runtime): an INVITE whose room
        // matches the configured prefix joins an N-party mixer right
        // away — one participant per INVITE, no pairing or parking.
        // The session is filed under `dialog_sessions` so BYE stops it
        // alongside transcoded sessions. A decline / error falls
        // through to the classic 2-peer rendezvous below.
        let mut conference_joined = false;
        if !webrtc_bridged
            && let (Some(orch), Some(prefix), Some(key), Some(ep), Some(remote_rtp)) = (
                self.conference_orchestrator.as_ref(),
                self.conference_room_prefix.as_deref(),
                rendezvous.as_ref(),
                endpoint.as_ref(),
                remote_media,
            )
            && key.starts_with(prefix)
        {
            let leg = BridgeLeg {
                endpoint: ep.id(),
                peer: remote_rtp,
                srtp: srtp_keys.clone(),
            };
            match orch.orchestrate_room(dialog_key.clone(), key, leg).await {
                Ok(Some(session)) => {
                    use smiths_core::{LegId, MediaKindTag};
                    self.dialog_sessions.install(
                        dialog_key.clone(),
                        (LegId(0), MediaKindTag::Audio),
                        session,
                    );
                    info!(room = %key, "conference participant joined");
                    conference_joined = true;
                }
                Ok(None) => {
                    warn!(room = %key, "conference declined; falling back to rendezvous");
                }
                Err(e) => {
                    warn!(room = %key, ?e, "conference join failed; falling back to rendezvous");
                }
            }
        }

        // Rendezvous pairing: need a key, an endpoint, and the peer RTP
        // address from the offer.
        if !webrtc_bridged
            && !conference_joined
            && let (Some(key), Some(ep), Some(remote_rtp)) =
                (rendezvous.as_ref(), endpoint.as_ref(), remote_media)
        {
            if let Some((_, pending)) = self.pending_bridges.remove(key) {
                let leg_a = BridgeLeg {
                    endpoint: pending.endpoint,
                    peer: pending.remote_media,
                    srtp: pending.srtp.clone(),
                };
                let leg_b = BridgeLeg {
                    endpoint: ep.id(),
                    peer: remote_rtp,
                    srtp: srtp_keys.clone(),
                };
                // Slice 5.6c: detect codec mismatch at pair time.
                // When both codecs are known and differ AND a
                // `TranscodeOrchestrator` is wired, route through
                // the transcoded session path; otherwise fall
                // through to the plain passthrough bridge (pre-5.6c
                // behaviour).
                let codec_mismatch = match (pending.audio_codec.as_ref(), audio_codec.as_ref()) {
                    (Some(a), Some(b)) => a != b,
                    _ => false,
                };
                let orchestrated = if codec_mismatch {
                    self.try_orchestrate_transcoded(
                        key,
                        &pending.dialog_key,
                        &dialog_key,
                        leg_a.clone(),
                        pending.audio_codec.clone().unwrap_or(NegotiatedCodec::Pcmu),
                        leg_b.clone(),
                        audio_codec.clone().unwrap_or(NegotiatedCodec::Pcmu),
                    )
                    .await
                } else {
                    false
                };
                if !orchestrated {
                    match self.media_fabric.bridge(leg_a, leg_b).await {
                        Ok(bid) => {
                            self.bridges_by_dialog.insert(pending.dialog_key, bid);
                            self.bridges_by_dialog.insert(dialog_key.clone(), bid);
                            info!(rendezvous = %key, "rendezvous bridge established");
                        }
                        Err(e) => warn!(?e, rendezvous = %key, "rendezvous bridge failed"),
                    }
                }
            } else {
                self.pending_bridges.insert(
                    key.clone(),
                    PendingLeg {
                        dialog_key: dialog_key.clone(),
                        endpoint: ep.id(),
                        remote_media: remote_rtp,
                        srtp: srtp_keys.clone(),
                        audio_codec: audio_codec.clone(),
                    },
                );
                info!(rendezvous = %key, "rendezvous leg parked, awaiting peer");
            }
        }

        // Slice 5.6: populate per-leg codec from the negotiator's
        // output. `LegId(0)` is the answerer's own leg — that's the
        // leg whose codec the negotiator just chose. The far-end
        // leg's codec is learned at bridge time (today's 2-peer
        // bridge uses the same codec both sides; 5.6b will refine
        // this when transcoding wires through).
        let mut per_leg_codec = std::collections::BTreeMap::new();
        if let Some(c) = audio_codec.clone() {
            per_leg_codec.insert(smiths_core::LegId(0), c);
        }
        if let Some(c) = video_codec.clone() {
            // Video-leg codec under LegId(0) would collide with the
            // audio entry; use LegId(1) to keep both entries alive.
            // The LegId namespace is process-scoped, not
            // cross-dialog, so collision with a future remote leg
            // is impossible within this record.
            per_leg_codec.insert(smiths_core::LegId(1), c);
        }
        let record = DialogRecord {
            call_id: call_id.clone(),
            local_tag: local_tag.clone(),
            remote_tag,
            state: DialogState::Early,
            peer_signal: peer,
            rendezvous,
            media: endpoint.as_ref().map(|ep| ep.id()),
            remote_media,
            pending_2xx: None,
            per_leg_codec,
            ice,
        };
        self.dialogs.insert(dialog_key.clone(), record.clone());
        self.replicator
            .replicate(smiths_core::DialogDelta::Upsert(Box::new(record)));
        self.metrics.dialogs_active.inc();

        // CDR: remember the call's start + URIs so the `handle_bye`
        // path can emit a complete record. We capture even when no
        // `CdrStore` is wired — the side-table is cheap, and swapping
        // the store at runtime (tests) doesn't lose the start time.
        if self.cdr_store.is_some() {
            self.cdr_pending.insert(
                dialog_key.clone(),
                CdrInProgress {
                    call_id: call_id.clone(),
                    from_uri: req.from_uri.clone().unwrap_or_default(),
                    to_uri: req.to_uri.clone().unwrap_or_default(),
                    started_at_unix: smiths_core::storage::CallDetailRecord::now_unix(),
                },
            );
        }

        let mut extras: Vec<(&str, &str)> = vec![("Contact", self.contact.as_str())];
        if sdp_answer_body.is_some() {
            extras.push(("Content-Type", "application/sdp"));
        }
        let body_slice = sdp_answer_body.as_deref().unwrap_or("");
        self.respond(
            req,
            200,
            "OK",
            Some(&local_tag),
            &extras,
            body_slice.as_bytes(),
            peer,
        )
        .await;

        let _ = self.bus.publish(Event::Sip(SipEvent::DialogCreated {
            call_id,
            from_uri: req.from_uri.clone(),
            media_endpoint: endpoint.as_ref().map(|ep| ep.id()),
            remote_rtp: remote_media,
        }));
    }

    /// Slice 5.6c: try to route a codec-mismatched rendezvous pair
    /// through a transcoded session. Returns `true` if the
    /// orchestrator admitted the call and the session was installed
    /// into [`DialogSessions`] — in that case the caller skips the
    /// plain passthrough bridge. Returns `false` when no
    /// orchestrator is wired, admission was refused, or
    /// construction errored — the caller then falls through to the
    /// passthrough path (pre-5.6c behaviour).
    #[allow(clippy::too_many_arguments)]
    async fn try_orchestrate_transcoded(
        &self,
        rendezvous_key: &str,
        dialog_a: &DialogKey,
        dialog_b: &DialogKey,
        leg_a: BridgeLeg,
        codec_a: NegotiatedCodec,
        leg_b: BridgeLeg,
        codec_b: NegotiatedCodec,
    ) -> bool {
        let Some(orch) = self.transcode_orchestrator.as_ref() else {
            warn!(
                rendezvous = rendezvous_key,
                %codec_a,
                %codec_b,
                "codec mismatch at rendezvous but no TranscodeOrchestrator wired; \
                 falling through to passthrough bridge (audio will not be audible)",
            );
            return false;
        };
        match orch
            .try_orchestrate(leg_a, codec_a.clone(), leg_b, codec_b.clone())
            .await
        {
            Ok(Some(session)) => {
                // Install the same session handle under both legs'
                // keys. `(LegId(0), Audio)` for the first-in leg,
                // `(LegId(1), Audio)` for the second — matches
                // slice 5.6's `per_leg_codec` convention.
                use smiths_core::{LegId, MediaKindTag};
                let key_a = (LegId(0), MediaKindTag::Audio);
                let key_b = (LegId(1), MediaKindTag::Audio);
                self.dialog_sessions
                    .install(dialog_a.clone(), key_a, Arc::clone(&session));
                self.dialog_sessions
                    .install(dialog_b.clone(), key_b, session);
                info!(
                    rendezvous = rendezvous_key,
                    %codec_a,
                    %codec_b,
                    "rendezvous transcoded session installed",
                );
                true
            }
            Ok(None) => {
                warn!(
                    rendezvous = rendezvous_key,
                    %codec_a,
                    %codec_b,
                    "transcode admission refused (budget exhausted); \
                     passthrough fallback will not produce audible audio",
                );
                // NOTE: a future slice should respond 488 + `Warning:
                // 370` here instead of silently falling through, but
                // that path needs to unwind leg-A's already-200-OK'd
                // dialog too. Out of scope for 5.6c.
                false
            }
            Err(e) => {
                warn!(
                    rendezvous = rendezvous_key,
                    %codec_a,
                    %codec_b,
                    error = %e,
                    "transcoded session construction failed; \
                     falling back to passthrough",
                );
                false
            }
        }
    }

    fn handle_ack(&self, req: &RequestSummary, peer: SocketAddr) {
        // Transaction-layer: for a non-2xx final, ACK shares the
        // INVITE's branch (RFC 3261 §17.1.1.3) and transitions the
        // server INVITE FSM from Completed → Confirmed, cancelling
        // G/H and arming I. For 2xx the FSM bypassed to Terminated
        // on the 2xx send, so `is_alive` is already false here.
        if let Some(branch) = req.branch.as_deref() {
            let invite_key = server_txn_key(branch, "INVITE");
            if self.txn_driver.is_alive(&invite_key) {
                self.txn_driver
                    .deliver_request(&invite_key, "ACK".into(), req.raw.clone());
            }
        }

        // Dialog-layer: Early → Confirmed on the 2xx ACK. Cancels any
        // in-flight §13.3.1.4 retransmit loop and clears the parked
        // 2xx bytes off the record — both are scoped to the
        // pre-confirmation window.
        let Some(key) = in_dialog_key(req) else {
            debug!(%peer, "ACK missing dialog identifiers; dropping");
            return;
        };
        if let Some(mut entry) = self.dialogs.get_mut(&key) {
            if entry.state == DialogState::Early {
                entry.state = DialogState::Confirmed;
                entry.pending_2xx = None;
                info!(call_id = %entry.call_id, "dialog confirmed");
                self.replicator
                    .replicate(smiths_core::DialogDelta::Upsert(Box::new(entry.clone())));
            }
        } else {
            debug!(?key, "ACK for unknown dialog; ignoring");
        }
        self.cancel_invite_2xx_retransmit(&key);
    }

    #[instrument(skip_all, fields(%peer, call_id = %req.call_id.as_deref().unwrap_or("-")))]
    async fn handle_bye(&self, req: &RequestSummary, peer: SocketAddr) {
        let Some(key) = in_dialog_key(req) else {
            self.respond(req, 400, "Bad Request", Some(&next_tag()), &[], &[], peer)
                .await;
            return;
        };

        match self.dialogs.remove(&key) {
            Some((_, record)) => {
                self.metrics.dialogs_active.dec();
                self.replicator
                    .replicate(smiths_core::DialogDelta::Delete(key.clone()));
                // BYE before ACK is exotic but legal — cancel any
                // in-flight §13.3.1.4 retransmit so the loop doesn't
                // keep firing after the dialog is gone.
                self.cancel_invite_2xx_retransmit(&key);
                // Drop an unpaired pending leg if this was it.
                if let Some(rv) = record.rendezvous.as_ref()
                    && let Some(entry) = self.pending_bridges.get(rv)
                {
                    let same = entry.dialog_key == key;
                    drop(entry);
                    if same {
                        self.pending_bridges.remove(rv);
                    }
                }
                // Tear down the live bridge if this dialog is part of
                // one. Whichever side BYE-s first wins the race; the
                // second BYE finds no entry and the fabric release is
                // idempotent.
                if let Some((_, bid)) = self.bridges_by_dialog.remove(&key) {
                    // Find the *other* leg sharing this bridge before we
                    // drop its entry — releasing only the media bridge
                    // leaves the peer's SIP dialog up (it hears silence
                    // but the call never drops). Propagate the hang-up so
                    // a BYE from either leg ends the whole call.
                    let peer_key = self
                        .bridges_by_dialog
                        .iter()
                        .find(|e| *e.value() == bid)
                        .map(|e| e.key().clone());
                    self.bridges_by_dialog.retain(|_, other| *other != bid);
                    self.media_fabric.release_bridge(bid).await;
                    debug!(call_id = %record.call_id, "rendezvous bridge stopped");
                    if let Some(pk) = peer_key {
                        self.bye_peer_leg(&pk).await;
                    }
                }
                // Slice 5.6c: drain any non-passthrough sessions
                // (transcoded, and later FAX / conference) that
                // belong to this dialog. `remove_dialog` returns
                // every handle we owned; we stop each one so the
                // forwarder tasks exit and the admission lease
                // (if any) releases.
                for session in self.dialog_sessions.remove_dialog(&key) {
                    session.stop().await;
                }
                if let Some(ep) = record.media {
                    self.media_fabric.release_endpoint(ep).await;
                }
                self.respond(req, 200, "OK", Some(&record.local_tag), &[], &[], peer)
                    .await;
                // CDR: fire after the 200 lands so a failing
                // CdrStore::record never blocks the BYE response.
                self.emit_cdr_for(&key, "answered");
                let _ = self.bus.publish(Event::Sip(SipEvent::DialogTerminated {
                    call_id: record.call_id,
                }));
            }
            None => {
                self.respond(
                    req,
                    481,
                    "Call/Transaction Does Not Exist",
                    Some(&next_tag()),
                    &[],
                    &[],
                    peer,
                )
                .await;
            }
        }
    }

    /// End the *other* leg of a torn-down bridge by originating a BYE
    /// toward its peer, then release its dialog + media.
    ///
    /// When one bridged leg sends BYE the engine only releases the
    /// media bridge; the surviving leg's SIP dialog stays Confirmed and
    /// the call never actually drops (the remote just hears silence).
    /// This makes the bridge behave like a B2BUA: a hang-up on either
    /// side terminates both. We reconstruct an in-dialog BYE from the
    /// stored tags — the engine never originated a request in this
    /// dialog, so `CSeq` starts at 1; the remote matches on
    /// Call-ID + tags regardless of the Request-URI.
    async fn bye_peer_leg(&self, key: &DialogKey) {
        let Some((_, record)) = self.dialogs.remove(key) else {
            return;
        };
        self.metrics.dialogs_active.dec();
        self.replicator
            .replicate(smiths_core::DialogDelta::Delete(key.clone()));
        self.cancel_invite_2xx_retransmit(key);

        let via = self.transport.local_addr().unwrap_or(record.peer_signal);
        let branch = format!("z9hG4bK{}", next_tag());
        let peer_uri = format!("sip:{}", record.peer_signal);
        let bye = build_peer_bye(&PeerByeFields {
            request_uri: &peer_uri,
            via_sent_by: via,
            branch: &branch,
            from_uri: &format!("<sip:smiths@{via}>"),
            from_tag: &record.local_tag,
            to_uri: &format!("<{peer_uri}>"),
            to_tag: &record.remote_tag,
            call_id: &record.call_id,
        });
        // Fire-and-forget the BYE with a couple of UDP retransmits: the
        // dialog is already removed locally, so we don't process the
        // peer's 200, but a single lost datagram would otherwise leave
        // the far end ringing. Duplicate BYEs are harmless (200 then 481).
        let bye = Bytes::from(bye);
        let transport = Arc::clone(&self.transport);
        let dest = record.peer_signal;
        let call_id = record.call_id.clone();
        tokio::spawn(async move {
            for attempt in 0..3u8 {
                if let Err(e) = transport.send(bye.clone(), dest).await {
                    warn!(peer = %dest, ?e, "failed to BYE bridged peer leg");
                    break;
                }
                debug!(%call_id, peer = %dest, attempt, "BYE → bridged peer leg");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        });

        for session in self.dialog_sessions.remove_dialog(key) {
            session.stop().await;
        }
        if let Some(ep) = record.media {
            self.media_fabric.release_endpoint(ep).await;
        }
        self.emit_cdr_for(key, "answered");
        let _ = self.bus.publish(Event::Sip(SipEvent::DialogTerminated {
            call_id: record.call_id,
        }));
    }

    /// Pop the CDR-in-progress entry for `key`, compose a full
    /// `CallDetailRecord`, and fire-and-forget through the configured
    /// `CdrStore`. Silent no-op when no store is wired or no
    /// in-progress entry exists (BYE for an unknown dialog, or a
    /// failover-inherited call the side table didn't see).
    fn emit_cdr_for(&self, key: &DialogKey, result: &str) {
        let Some(store) = self.cdr_store.as_ref() else {
            self.cdr_pending.remove(key);
            return;
        };
        let Some((_, in_progress)) = self.cdr_pending.remove(key) else {
            debug!(?key, "BYE without cdr_pending — no row emitted");
            return;
        };
        let now = smiths_core::storage::CallDetailRecord::now_unix();
        let cdr = smiths_core::storage::CallDetailRecord {
            call_id: in_progress.call_id,
            from_uri: in_progress.from_uri,
            to_uri: in_progress.to_uri,
            started_at_unix: in_progress.started_at_unix,
            ended_at_unix: now,
            duration_secs: now.saturating_sub(in_progress.started_at_unix).max(0),
            result: result.to_owned(),
        };
        if let Err(e) = store.record(&cdr) {
            warn!(?e, call_id = %cdr.call_id, "CDR write failed");
        }
    }

    /// Send a provisional (1xx) response. Routed through the server
    /// INVITE FSM when one is registered — the FSM caches the
    /// provisional as `last_response` so an INVITE retransmit replays
    /// it instead of burning the request all the way to the handler.
    async fn send_provisional(
        &self,
        req: &RequestSummary,
        status: u16,
        reason: &str,
        peer: SocketAddr,
    ) {
        let bytes = Bytes::from(build_response(&req.raw, status, reason, None, &[], b""));
        if let Some(branch) = req.branch.as_deref() {
            let key = server_txn_key(branch, &req.method);
            if self.txn_driver.is_alive(&key) {
                self.txn_driver.send_response(&key, status, bytes);
                self.emit_response_metrics(req, peer, status);
                return;
            }
        }
        // Fallback — no branch (exceptional) or no FSM. Direct send.
        if let Err(e) = self.transport.send(bytes, peer).await {
            warn!(%peer, ?e, "failed to send provisional response");
            return;
        }
        self.emit_response_metrics(req, peer, status);
    }

    /// Send a final (>= 200) response through the server transaction
    /// FSM. The driver caches the bytes in `last_response` (replayed
    /// on request retransmits) and arms the method-appropriate
    /// timer — G/H for INVITE non-2xx, J for every non-INVITE final.
    ///
    /// INVITE 2xx bypasses the FSM straight to Terminated per §17.2.1;
    /// the TU (us) owns 2xx retransmit per §13.3.1.4. The bytes are
    /// parked on the [`DialogRecord`] and a per-dialog timer loop is
    /// armed via [`Self::spawn_invite_2xx_retransmit`].
    #[allow(clippy::too_many_arguments)] // a response is genuinely this many knobs
    async fn respond(
        &self,
        req: &RequestSummary,
        status: u16,
        reason: &str,
        local_tag: Option<&str>,
        extras: &[(&str, &str)],
        body: &[u8],
        peer: SocketAddr,
    ) {
        let bytes = Bytes::from(build_response(
            &req.raw, status, reason, local_tag, extras, body,
        ));

        // Park INVITE 2xx on the dialog record and spin up the
        // §13.3.1.4 retransmit loop. Done *before* the driver send
        // so a racing ACK on a fast loopback can still find the
        // retransmit handle when it cancels.
        if req.method == "INVITE"
            && (200..300).contains(&status)
            && let (Some(call_id), Some(remote_tag), Some(l_tag)) =
                (req.call_id.as_deref(), req.from_tag.as_deref(), local_tag)
        {
            let key: DialogKey = (call_id.to_owned(), l_tag.to_owned(), remote_tag.to_owned());
            if let Some(mut entry) = self.dialogs.get_mut(&key) {
                entry.pending_2xx = Some(bytes.to_vec());
                self.replicator
                    .replicate(smiths_core::DialogDelta::Upsert(Box::new(entry.clone())));
            }
            self.spawn_invite_2xx_retransmit(&key, bytes.clone(), peer);
        }

        if let Some(branch) = req.branch.as_deref() {
            let key = server_txn_key(branch, &req.method);
            if self.txn_driver.is_alive(&key) {
                self.txn_driver.send_response(&key, status, bytes);
                self.emit_response_metrics(req, peer, status);
                return;
            }
        }

        // Fallback — no branch header, exceptionally rare. Direct send.
        if let Err(e) = self.transport.send(bytes, peer).await {
            warn!(%peer, ?e, "failed to send response");
            return;
        }
        self.emit_response_metrics(req, peer, status);
    }

    /// Start the RFC 3261 §13.3.1.4 per-dialog 2xx retransmit loop.
    /// The first retransmit fires at T1 after this call (the initial
    /// send goes through the FSM's `SendToPeer` in [`Self::respond`]);
    /// subsequent intervals double up to T2 and the whole loop caps
    /// at 64·T1 total wall-clock. ACK (cancel token tripped in
    /// [`Self::handle_ack`]) or dialog teardown bow out early.
    ///
    /// Each retransmit bumps `sip_invite_2xx_retransmits`. A retransmit
    /// emitted after the first is the operator's signal that either
    /// (a) the 2xx was lost and we're doing RFC-compliant recovery,
    /// or (b) the peer stopped `ACK`ing and we're burning the budget.
    fn spawn_invite_2xx_retransmit(&self, key: &DialogKey, bytes: Bytes, peer: SocketAddr) {
        let cancel = CancellationToken::new();
        // Replace any pre-existing handle for this dialog (re-INVITE
        // scenarios; today we don't re-INVITE but the map should
        // degrade sanely). Abort-on-insert, then start the new loop.
        if let Some(old) = self
            .invite_2xx_retransmits
            .insert(key.clone(), cancel.clone())
        {
            old.cancel();
        }
        let transport = Arc::clone(&self.transport);
        let metrics = Arc::clone(&self.metrics);
        let retransmits = Arc::clone(&self.invite_2xx_retransmits);
        let task_key = key.clone();
        let task_cancel = cancel.clone();
        tokio::spawn(async move {
            let mut interval = T1;
            let mut elapsed = Duration::ZERO;
            loop {
                tokio::select! {
                    biased;
                    () = task_cancel.cancelled() => {
                        // Canceller (ACK, BYE, or a re-spawn that
                        // replaced us) already cleared / rewrote our
                        // map slot. Don't touch it on the way out.
                        return;
                    }
                    () = tokio::time::sleep(interval) => {}
                }
                elapsed = elapsed.saturating_add(interval);
                if elapsed > INVITE_2XX_BUDGET {
                    // §13.3.1.4: after 64·T1 without ACK the TU gives
                    // up. Dialog-level cleanup (sending BYE) is a
                    // follow-on; today we just exit the loop.
                    warn!(
                        ?task_key,
                        "INVITE 2xx retransmit budget exhausted without ACK"
                    );
                    break;
                }
                if let Err(e) = transport.send(bytes.clone(), peer).await {
                    warn!(%peer, ?e, "INVITE 2xx retransmit failed");
                    break;
                }
                metrics.sip_invite_2xx_retransmits.inc();
                debug!(?task_key, ?interval, "INVITE 2xx retransmit fired");
                // Double, capped at T2.
                interval = std::cmp::min(interval.saturating_mul(2), T2);
            }
            // Natural exit (budget or send-error): if our token is
            // still live, we own the map slot — take it out. If it's
            // been cancelled mid-iteration, a replacement has already
            // rewired the entry and we must not touch it.
            if !task_cancel.is_cancelled() {
                retransmits.remove(&task_key);
            }
        });
    }

    /// Trip the cancel token for `key`'s retransmit loop (if any) and
    /// remove the handle. Idempotent — safe to call from both ACK
    /// and BYE paths even when no loop is registered.
    fn cancel_invite_2xx_retransmit(&self, key: &DialogKey) {
        if let Some((_, token)) = self.invite_2xx_retransmits.remove(key) {
            token.cancel();
        }
    }

    /// Best-effort match from a retransmitted INVITE (no to-tag yet)
    /// back to the Early dialog we already answered. Returns `None`
    /// when no dialog is registered for `(call_id, from_tag)`, which
    /// means the INVITE is genuinely new.
    fn dialog_for_invite(&self, req: &RequestSummary) -> Option<DialogKey> {
        let call_id = req.call_id.as_deref()?;
        let remote_tag = req.from_tag.as_deref()?;
        self.dialogs.iter().find_map(|entry| {
            let k = entry.key();
            if k.0 == call_id && k.2 == remote_tag {
                Some(k.clone())
            } else {
                None
            }
        })
    }

    fn emit_response_metrics(&self, req: &RequestSummary, peer: SocketAddr, status: u16) {
        self.metrics
            .sip_responses
            .get_or_create(&SipCodeLabel {
                code: status.to_string(),
            })
            .inc();
        let _ = self.bus.publish(Event::Sip(SipEvent::ResponseSent {
            peer,
            status,
            call_id: req.call_id.clone(),
        }));
    }
}

/// Build the server-side [`TxnKey`] for a request's (branch, method)
/// tuple. The role is always [`TxnRole::Server`] — the UAS only
/// reaches into the driver for its own incoming-side FSMs.
fn server_txn_key(branch: &str, method: &str) -> TxnKey {
    TxnKey {
        branch: branch.to_owned(),
        method: method.to_owned(),
        role: TxnRole::Server,
    }
}

/// Format a SIP `Warning:` header value per RFC 3261 §20.43:
/// `<code> <warn-agent> "<text>"`. Code 399 is the miscellaneous
/// catch-all the RFC reserves for "just carrying a text reason";
/// `warn-agent` is our product token, and the text is quoted so
/// spaces inside it survive the wire.
///
/// The reason is sanitized — we strip any embedded `"` since the
/// `UnsupportedTransport` payload is internal-text but ends up on the
/// wire.
fn format_warning(reason: &str) -> String {
    let sanitized: String = reason.chars().filter(|c| *c != '"').collect();
    format!("399 smiths-net \"{sanitized}\"")
}

/// Key for an in-dialog request (ACK, BYE, re-INVITE).
///
/// Incoming request sees From as remote and To as local.
/// Extract the first `Via` header's `branch` parameter from any raw
/// SIP message (request or response). Returns `None` when the header
/// or parameter is missing. Used by the response-router forwarder.
/// Fuzz-only hooks. Exposed so `smiths-fuzz` can drive our hand-rolled
/// parsers directly without booting a full UAS. Not part of the
/// stable API — internal to the workspace.
#[doc(hidden)]
pub mod __fuzz {
    use bytes::Bytes;

    /// Run `summarize_request` on arbitrary bytes. Panics/UB/OOB are
    /// the bugs the fuzzer hunts for.
    pub fn summarize_request(raw: &[u8]) {
        let _ = super::summarize_request(&Bytes::copy_from_slice(raw));
    }

    /// Run `extract_via_branch` on arbitrary bytes.
    pub fn extract_via_branch(raw: &[u8]) {
        let _ = super::extract_via_branch(&Bytes::copy_from_slice(raw));
    }
}

fn extract_via_branch(raw: &Bytes) -> Option<String> {
    let text = std::str::from_utf8(raw).ok()?;
    for line in text.split("\r\n") {
        if line.is_empty() {
            break; // headers done
        }
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("via:") || lower.starts_with("v:") {
            let idx = lower.find(";branch=")?;
            let after = &line[idx + ";branch=".len()..];
            let end = after
                .find(|c: char| c == ';' || c == ',' || c.is_whitespace())
                .unwrap_or(after.len());
            return Some(after[..end].to_owned());
        }
    }
    None
}

fn in_dialog_key(req: &RequestSummary) -> Option<DialogKey> {
    let call_id = req.call_id.clone()?;
    let local_tag = req.to_tag.clone()?;
    let remote_tag = req.from_tag.clone()?;
    Some((call_id, local_tag, remote_tag))
}

/// Extract the minimum routing info we need from a request's raw bytes.
fn summarize_request(raw: &Bytes) -> RequestSummary {
    let text = String::from_utf8_lossy(raw);
    let (headers, body) = split_headers_body(&text);

    let mut lines = headers.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut tokens = request_line.split_whitespace();
    let method = tokens.next().unwrap_or("").to_ascii_uppercase();
    let ruri_raw = tokens.next();
    let ruri_user = ruri_raw.and_then(ruri_user_from);
    let request_uri = ruri_raw.map(str::to_owned);

    let mut branch = None;
    let mut call_id = None;
    let mut from_tag = None;
    let mut to_tag = None;
    let mut content_type: Option<String> = None;
    let mut authorization: Option<String> = None;
    let mut contact: Option<String> = None;
    let mut expires: Option<u32> = None;
    let mut from_uri: Option<String> = None;
    let mut to_uri: Option<String> = None;
    let mut webrtc_tag: Option<String> = None;

    for line in lines {
        if line.is_empty() {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if branch.is_none() && (lower.starts_with("via:") || lower.starts_with("v:")) {
            if let Some(idx) = lower.find(";branch=") {
                let rest = &line[idx + ";branch=".len()..];
                let end = rest
                    .find(|c: char| c == ';' || c.is_whitespace())
                    .unwrap_or(rest.len());
                branch = Some(rest[..end].to_owned());
            }
        } else if call_id.is_none() && (lower.starts_with("call-id:") || lower.starts_with("i:")) {
            let v = line.split_once(':').map_or("", |(_, v)| v).trim();
            if !v.is_empty() {
                call_id = Some(v.to_owned());
            }
        } else if from_tag.is_none() && (lower.starts_with("from:") || lower.starts_with("f:")) {
            let v = line.split_once(':').map_or("", |(_, v)| v);
            from_tag = extract_tag_param(v);
            from_uri = first_contact_uri(v);
        } else if to_tag.is_none() && (lower.starts_with("to:") || lower.starts_with("t:")) {
            let v = line.split_once(':').map_or("", |(_, v)| v);
            to_tag = extract_tag_param(v);
            to_uri = first_contact_uri(v);
        } else if content_type.is_none()
            && (lower.starts_with("content-type:") || lower.starts_with("c:"))
        {
            let v = line.split_once(':').map_or("", |(_, v)| v).trim();
            // Strip any `; charset=...` and normalize.
            let media_type = v.split(';').next().unwrap_or(v).trim().to_ascii_lowercase();
            if !media_type.is_empty() {
                content_type = Some(media_type);
            }
        } else if authorization.is_none() && lower.starts_with("authorization:") {
            let v = line.split_once(':').map_or("", |(_, v)| v).trim();
            if !v.is_empty() {
                authorization = Some(v.to_owned());
            }
        } else if contact.is_none() && (lower.starts_with("contact:") || lower.starts_with("m:")) {
            let v = line.split_once(':').map_or("", |(_, v)| v).trim();
            if !v.is_empty() {
                contact = Some(v.to_owned());
            }
        } else if expires.is_none() && lower.starts_with("expires:") {
            let v = line.split_once(':').map_or("", |(_, v)| v).trim();
            if let Ok(n) = v.parse::<u32>() {
                expires = Some(n);
            }
        } else if webrtc_tag.is_none() && lower.starts_with("x-smiths-webrtc-tag:") {
            // Slice 5.10-sipjoin: custom extension header asking
            // the UAS to bridge this dialog with a WebRTC leg
            // sharing the same tag. Case-insensitive prefix
            // match; value trimmed of surrounding whitespace.
            let v = line.split_once(':').map_or("", |(_, v)| v).trim();
            if !v.is_empty() {
                webrtc_tag = Some(v.to_owned());
            }
        }
    }

    RequestSummary {
        method,
        branch,
        call_id,
        from_tag,
        to_tag,
        ruri_user,
        request_uri,
        authorization,
        content_type,
        contact,
        expires,
        from_uri,
        to_uri,
        body: (!body.is_empty()).then(|| body.to_owned()),
        raw: raw.clone(),
        webrtc_tag,
    }
}

/// Current wall-clock seconds, clamped on overflow. Pure; pulled out
/// so tests can swap it for a deterministic source if ever needed.
fn current_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Extract the first contact URI from a `Contact:` header value.
///
/// Prefers the form `"display" <sip:...>` (the URI is the first
/// angle-bracketed token). Falls back to the first whitespace-/
/// comma-delimited token, stripping trailing parameters.
///
/// Deliberately permissive — slice 2.1 binds one URI per AOR; a
/// follow-on slice gets to parse multiple contacts + `;expires=N`
/// per-contact params per RFC 3261 §25.1.
fn first_contact_uri(header: &str) -> Option<String> {
    let trimmed = header.trim();
    if trimmed.is_empty() || trimmed == "*" {
        return None;
    }
    if let Some(start) = trimmed.find('<') {
        let after = &trimmed[start + 1..];
        if let Some(end) = after.find('>') {
            let inner = &after[..end];
            if !inner.is_empty() {
                return Some(inner.to_owned());
            }
        }
    }
    let first = trimmed
        .split([',', ' ', '\t'])
        .next()
        .unwrap_or(trimmed)
        .trim();
    // Strip any ;param=value trailer.
    let uri = first.split(';').next().unwrap_or(first).trim();
    (!uri.is_empty()).then(|| uri.to_owned())
}

#[cfg(test)]
mod contact_parse_tests {
    use super::first_contact_uri;

    #[test]
    fn angle_bracketed_form() {
        assert_eq!(
            first_contact_uri("\"Alice\" <sip:alice@pc.example>;expires=3600"),
            Some("sip:alice@pc.example".into())
        );
    }

    #[test]
    fn bare_uri_with_params() {
        assert_eq!(
            first_contact_uri("sip:bob@pc.example;expires=60"),
            Some("sip:bob@pc.example".into())
        );
    }

    #[test]
    fn multiple_contacts_take_first() {
        assert_eq!(
            first_contact_uri("<sip:a@x>, <sip:b@y>"),
            Some("sip:a@x".into())
        );
    }

    #[test]
    fn wildcard_is_none() {
        assert_eq!(first_contact_uri("*"), None);
    }

    #[test]
    fn empty_is_none() {
        assert_eq!(first_contact_uri("   "), None);
    }
}

/// Pull the user-part out of a Request-URI like `sip:user@host:port` or
/// `sips:user@host`. Returns `None` when the URI has no `@` segment
/// (e.g. `sip:host:port` — valid, just anonymous).
fn ruri_user_from(request_uri: &str) -> Option<String> {
    let rest = request_uri
        .strip_prefix("sip:")
        .or_else(|| request_uri.strip_prefix("sips:"))
        .or_else(|| request_uri.strip_prefix("<sip:"))
        .or_else(|| request_uri.strip_prefix("<sips:"))?;
    let (user, _) = rest.split_once('@')?;
    if user.is_empty() {
        None
    } else {
        Some(user.to_owned())
    }
}

/// Split a SIP message text into `(headers, body)`. RFC 3261 uses
/// `\r\n\r\n` as the delimiter; we also tolerate `\n\n` since some
/// tools normalize newlines.
fn split_headers_body(text: &str) -> (&str, &str) {
    if let Some(idx) = text.find("\r\n\r\n") {
        (&text[..idx], &text[idx + 4..])
    } else if let Some(idx) = text.find("\n\n") {
        (&text[..idx], &text[idx + 2..])
    } else {
        (text, "")
    }
}

/// Extract the `;tag=VALUE` parameter from a `From` / `To` header value.
fn extract_tag_param(header_value: &str) -> Option<String> {
    let lower = header_value.to_ascii_lowercase();
    let idx = lower.find(";tag=")?;
    let rest = &header_value[idx + ";tag=".len()..];
    let end = rest
        .find(|c: char| c == ';' || c == ',' || c == '>' || c.is_whitespace())
        .unwrap_or(rest.len());
    if end == 0 {
        None
    } else {
        Some(rest[..end].to_owned())
    }
}

/// Build a response by copying the mandatory headers from `request`.
///
/// - Copies `Via`, `From`, `Call-ID`, `CSeq` verbatim.
/// - Rewrites `To`: preserves existing `;tag=` if set, otherwise appends
///   `local_tag` if provided.
/// - Appends any `extras` after the copied headers.
/// - Emits `Content-Length: N` derived from `body.len()` and appends
///   `body` after the blank line.
fn build_response(
    request: &Bytes,
    status: u16,
    reason: &str,
    local_tag: Option<&str>,
    extras: &[(&str, &str)],
    body: &[u8],
) -> Vec<u8> {
    let text = String::from_utf8_lossy(request);
    // Consider only the header region of the request; bodies can contain
    // lines that look like SIP headers (SDP never does, but other MIME
    // types could) and must not be echoed.
    let (headers, _) = split_headers_body(&text);
    let mut out = String::with_capacity(request.len() + body.len() + 64);
    // `write!` on String is infallible.
    let _ = write!(out, "SIP/2.0 {status} {reason}\r\n");

    let mut lines = headers.split("\r\n");
    let _ = lines.next(); // skip the request line

    for line in lines {
        if line.is_empty() {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("via:")
            || lower.starts_with("v:")
            || lower.starts_with("from:")
            || lower.starts_with("f:")
            || lower.starts_with("call-id:")
            || lower.starts_with("i:")
            || lower.starts_with("cseq:")
        {
            out.push_str(line);
            out.push_str("\r\n");
        } else if lower.starts_with("to:") || lower.starts_with("t:") {
            out.push_str(line);
            if let Some(tag) = local_tag
                && !lower.contains(";tag=")
            {
                out.push_str(";tag=");
                out.push_str(tag);
            }
            out.push_str("\r\n");
        }
    }

    for (name, value) in extras {
        out.push_str(name);
        out.push_str(": ");
        out.push_str(value);
        out.push_str("\r\n");
    }

    let _ = write!(out, "Content-Length: {}\r\n\r\n", body.len());
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

/// Pick the local IP to publish in outbound SDP for a given peer.
///
/// - If `bind_ip` is a concrete address, trust it.
/// - Otherwise (wildcard `0.0.0.0` / `::`) use the kernel's routing
///   table: bind an ephemeral UDP socket, `connect(peer)` to pick a
///   route (no packets sent), and read back the local address the
///   kernel chose. Fall back to loopback if anything fails.
async fn resolve_local_ip_for(bind_ip: IpAddr, peer: SocketAddr) -> IpAddr {
    if !bind_ip.is_unspecified() {
        return bind_ip;
    }
    let unspec: SocketAddr = match peer {
        SocketAddr::V4(_) => ([0, 0, 0, 0], 0).into(),
        SocketAddr::V6(_) => (std::net::Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    if let Ok(sock) = tokio::net::UdpSocket::bind(unspec).await
        && sock.connect(peer).await.is_ok()
        && let Ok(addr) = sock.local_addr()
        && !addr.ip().is_unspecified()
    {
        return addr.ip();
    }
    match peer {
        SocketAddr::V4(_) => IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        SocketAddr::V6(_) => IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
    }
}

/// Fields for an engine-originated in-dialog BYE on a bridged peer leg.
struct PeerByeFields<'a> {
    request_uri: &'a str,
    via_sent_by: SocketAddr,
    branch: &'a str,
    from_uri: &'a str,
    from_tag: &'a str,
    to_uri: &'a str,
    to_tag: &'a str,
    call_id: &'a str,
}

/// Build a minimal RFC 3261 in-dialog BYE the engine sends to drop a
/// bridged peer leg. `CSeq` is fixed at 1: the engine never originates a
/// request in these (inbound, UAS-accepted) dialogs, so 1 is always
/// fresh in its own sequence space.
fn build_peer_bye(f: &PeerByeFields<'_>) -> Vec<u8> {
    format!(
        "BYE {ruri} SIP/2.0\r\n\
         Via: SIP/2.0/UDP {via};branch={branch};rport\r\n\
         Max-Forwards: 70\r\n\
         From: {from};tag={ftag}\r\n\
         To: {to};tag={ttag}\r\n\
         Call-ID: {cid}\r\n\
         CSeq: 1 BYE\r\n\
         Content-Length: 0\r\n\r\n",
        ruri = f.request_uri,
        via = f.via_sent_by,
        branch = f.branch,
        from = f.from_uri,
        ftag = f.from_tag,
        to = f.to_uri,
        ttag = f.to_tag,
        cid = f.call_id,
    )
    .into_bytes()
}

/// Monotonic, process-unique tag for `From` / `To`.
fn next_tag() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    // Low 64 bits of wall-clock nanos are enough: tags only need
    // uniqueness within a process, not cryptographic entropy.
    #[allow(clippy::cast_possible_truncation)]
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let pid = u64::from(std::process::id());
    format!("smiths-{:016x}", n.wrapping_add(nanos ^ pid))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_OPTIONS: &str = concat!(
        "OPTIONS sip:alice@smiths.local SIP/2.0\r\n",
        "Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-abc123;rport\r\n",
        "From: Bob <sip:bob@smiths.local>;tag=314\r\n",
        "To: Alice <sip:alice@smiths.local>\r\n",
        "Call-ID: cid-xyz-42@10.0.0.1\r\n",
        "CSeq: 1 OPTIONS\r\n",
        "Max-Forwards: 70\r\n",
        "User-Agent: Smiths-Testkit/0.1\r\n",
        "Content-Length: 0\r\n\r\n",
    );

    const SAMPLE_BYE: &str = concat!(
        "BYE sip:alice@smiths.local SIP/2.0\r\n",
        "Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-bye-1\r\n",
        "From: Bob <sip:bob@smiths.local>;tag=bob-1\r\n",
        "To: Alice <sip:alice@smiths.local>;tag=smiths-xyz\r\n",
        "Call-ID: cid-xyz-42@10.0.0.1\r\n",
        "CSeq: 2 BYE\r\n",
        "Content-Length: 0\r\n\r\n",
    );

    #[test]
    fn summary_extracts_branch_call_id_and_tags() {
        let raw = Bytes::copy_from_slice(SAMPLE_BYE.as_bytes());
        let s = summarize_request(&raw);
        assert_eq!(s.method, "BYE");
        assert_eq!(s.branch.as_deref(), Some("z9hG4bK-bye-1"));
        assert_eq!(s.call_id.as_deref(), Some("cid-xyz-42@10.0.0.1"));
        assert_eq!(s.from_tag.as_deref(), Some("bob-1"));
        assert_eq!(s.to_tag.as_deref(), Some("smiths-xyz"));
    }

    #[test]
    fn response_copies_required_headers_and_adds_to_tag() {
        let raw = Bytes::copy_from_slice(SAMPLE_OPTIONS.as_bytes());
        let resp = build_response(&raw, 200, "OK", Some("abc-tag"), &[], b"");
        let s = std::str::from_utf8(&resp).unwrap();
        assert!(s.starts_with("SIP/2.0 200 OK\r\n"));
        assert!(s.contains("Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-abc123;rport\r\n"));
        assert!(s.contains("From: Bob <sip:bob@smiths.local>;tag=314\r\n"));
        assert!(s.contains("Call-ID: cid-xyz-42@10.0.0.1\r\n"));
        assert!(s.contains("CSeq: 1 OPTIONS\r\n"));
        assert!(s.contains(";tag=abc-tag\r\n"));
        assert!(s.ends_with("Content-Length: 0\r\n\r\n"));
        assert!(!s.contains("User-Agent:"));
        assert!(!s.contains("Max-Forwards:"));
    }

    #[test]
    fn response_preserves_existing_to_tag() {
        let raw = Bytes::copy_from_slice(SAMPLE_BYE.as_bytes());
        let resp = build_response(&raw, 200, "OK", Some("replacement"), &[], b"");
        let s = std::str::from_utf8(&resp).unwrap();
        // BYE's To already has tag=smiths-xyz; must NOT be overwritten.
        assert!(s.contains("To: Alice <sip:alice@smiths.local>;tag=smiths-xyz\r\n"));
        assert!(!s.contains("replacement"));
    }

    #[test]
    fn response_appends_extra_headers() {
        let raw = Bytes::copy_from_slice(SAMPLE_OPTIONS.as_bytes());
        let resp = build_response(
            &raw,
            200,
            "OK",
            Some("t"),
            &[("Contact", "<sip:engine@10.0.0.9:5060>")],
            b"",
        );
        let s = std::str::from_utf8(&resp).unwrap();
        assert!(s.contains("Contact: <sip:engine@10.0.0.9:5060>\r\n"));
    }

    #[test]
    fn response_body_and_content_length() {
        let raw = Bytes::copy_from_slice(SAMPLE_OPTIONS.as_bytes());
        let body = b"v=0\r\n";
        let resp = build_response(
            &raw,
            200,
            "OK",
            Some("t"),
            &[("Content-Type", "application/sdp")],
            body,
        );
        let s = std::str::from_utf8(&resp).unwrap();
        assert!(s.contains("Content-Type: application/sdp\r\n"));
        assert!(s.contains("Content-Length: 5\r\n\r\n"));
        assert!(s.ends_with("v=0\r\n"));
    }

    #[test]
    fn tags_are_unique_across_calls() {
        let a = next_tag();
        let b = next_tag();
        assert_ne!(a, b);
    }

    #[test]
    fn extract_tag_param_handles_edge_cases() {
        assert_eq!(extract_tag_param("<sip:x>;tag=abc"), Some("abc".to_owned()));
        assert_eq!(
            extract_tag_param("<sip:x>;TAG=abc;foo=bar"),
            Some("abc".to_owned())
        );
        assert_eq!(extract_tag_param("<sip:x>"), None);
        assert_eq!(extract_tag_param("<sip:x>;tag="), None);
    }
}
